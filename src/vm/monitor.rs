//! The HMP monitor client (`-monitor unix:...`).
//!
//! The monitor talks a human-oriented protocol: a banner and a `(qemu) `
//! prompt on connect, then for each command the readline-style echo of what
//! was typed, the command's output, and the prompt again.

use std::io::{self, Read, Write};
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use nix::errno::Errno;
use nix::libc;
use regex::Regex;

use super::config::monitor_path;

/// The monitor's prompt, which ends every response.
pub const HMP_PROMPT: &str = "(qemu) ";
/// How long one command may take, connect included.
pub const HMP_TIMEOUT: Duration = Duration::from_secs(5);
/// The pause between connection attempts while the monitor's backlog is
/// full.
const CONNECT_RETRY: Duration = Duration::from_millis(10);

/// CSI escape sequences, which readline's echo is made of.
fn ansi_escape() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\x1b\[[0-9;]*[A-Za-z]").expect("valid regex"))
}

/// Sends one HMP command to the VM's monitor socket and returns its output,
/// with the echo and prompt removed. Commands such as `device_add` return
/// `""` on success and an `Error: ...` line on failure.
pub fn monitor_command(storage: &Path, name: &str, command: &str) -> Result<String> {
    hmp_command(&monitor_path(storage, name), command, HMP_TIMEOUT)
}

/// [`monitor_command`] against an explicit socket with an explicit timeout.
/// Errors: `connect to QEMU monitor: <path>: <io error>` (`i/o timeout`
/// when the monitor does not take the connection in time),
/// `QEMU monitor: <error>`.
///
/// One fresh connection per command: QEMU's HMP socket serves a single
/// client, so a persistent one would block everyone else.
pub(crate) fn hmp_command(sock: &Path, command: &str, timeout: Duration) -> Result<String> {
    // One absolute deadline covers the whole exchange: connect, banner,
    // write, reply.
    let deadline = Instant::now() + timeout;
    let conn = connect_by(sock, deadline)
        .map_err(|e| anyhow!("connect to QEMU monitor: {}: {e}", sock.display()))?;
    let mut conn = Deadline { conn, at: deadline };

    read_until_prompt(&mut conn).map_err(|e| anyhow!("QEMU monitor: {e:#}"))?;
    conn.write_all(format!("{command}\n").as_bytes())
        .map_err(|e| anyhow!("QEMU monitor: {e}"))?;
    let raw = read_until_prompt(&mut conn).map_err(|e| anyhow!("QEMU monitor: {e:#}"))?;
    Ok(clean_hmp_response(&raw, command))
}

/// Connects to the Unix socket at `path` by `deadline`, as Go's
/// `net.DialTimeout` bounds its dial. A plain blocking connect waits for as
/// long as the listener's backlog is full, which QEMU's monitor (a backlog
/// of one, and no accepting while a client is attached) makes indefinitely;
/// here the connect does not block, a full backlog is tried again every
/// [`CONNECT_RETRY`], and past the deadline the error is `i/o timeout`.
/// Every other failure is the system's, as `UnixStream::connect` reports it.
fn connect_by(path: &Path, deadline: Instant) -> io::Result<UnixStream> {
    let (addr, addr_len) = sockaddr_un(path)?;
    let mut sock = nonblocking_socket()?;
    loop {
        // SAFETY: `addr` is an initialised sockaddr_un that outlives the
        // call, and `addr_len` does not exceed its size.
        let rc = unsafe {
            libc::connect(
                sock.as_raw_fd(),
                (&raw const addr).cast::<libc::sockaddr>(),
                addr_len,
            )
        };
        match if rc == 0 { None } else { Some(Errno::last()) } {
            None | Some(Errno::EISCONN) => break,
            Some(Errno::EINTR) => continue,
            // Under way: asked again after the pause, connect answers
            // EALREADY, EISCONN or the reason it failed.
            Some(Errno::EINPROGRESS | Errno::EALREADY) => {}
            // The backlog is full: a fresh socket tries again after the
            // pause, as the state of one whose connect failed is undefined.
            Some(Errno::EAGAIN) => sock = nonblocking_socket()?,
            Some(err) => return Err(err.into()),
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(timed_out());
        }
        thread::sleep(left.min(CONNECT_RETRY));
    }
    let conn = UnixStream::from(sock);
    conn.set_nonblocking(false)?;
    Ok(conn)
}

/// A Unix stream socket, close-on-exec (a child that inherited it would
/// keep the single-client monitor busy) and non-blocking.
fn nonblocking_socket() -> io::Result<OwnedFd> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let kind = libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let kind = libc::SOCK_STREAM;
    // SAFETY: socket(2) takes no pointers; the descriptor it returns is
    // owned by nothing else.
    let fd = unsafe { libc::socket(libc::AF_UNIX, kind, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh, valid descriptor that nothing else owns.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        use nix::fcntl::{fcntl, FcntlArg, FdFlag, OFlag};
        fcntl(&fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
        let flags = OFlag::from_bits_retain(fcntl(&fd, FcntlArg::F_GETFL)?);
        fcntl(&fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;
    }
    Ok(fd)
}

/// The socket address of the filesystem path `path`, and its length, with
/// the errors `UnixStream::connect` gives for a path that cannot be one.
fn sockaddr_un(path: &Path) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    // SAFETY: a sockaddr_un of zero bytes is a valid value.
    let mut addr: libc::sockaddr_un = unsafe { mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_os_str().as_bytes();
    if bytes.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "paths must not contain interior null bytes",
        ));
    }
    // The path needs a terminating NUL after it.
    if bytes.len() >= addr.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path must be shorter than SUN_LEN",
        ));
    }
    for (dst, &src) in addr.sun_path.iter_mut().zip(bytes) {
        *dst = src as libc::c_char;
    }
    let mut len = mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len();
    if !bytes.is_empty() {
        len += 1;
    }
    // At most the size of sockaddr_un, so this cannot truncate.
    Ok((addr, len as libc::socklen_t))
}

/// A socket whose reads and writes all count against one absolute
/// deadline, like Go's `SetDeadline`, rather than each restarting the clock.
struct Deadline {
    conn: UnixStream,
    at: Instant,
}

impl Deadline {
    /// What is left of the deadline, or the timeout error once it has passed.
    fn remaining(&self) -> io::Result<Duration> {
        let left = self.at.saturating_duration_since(Instant::now());
        if left.is_zero() {
            Err(timed_out())
        } else {
            Ok(left)
        }
    }
}

fn timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "i/o timeout")
}

/// A socket timeout surfaces as `WouldBlock` or `TimedOut` depending on the
/// platform; both mean the deadline passed.
fn map_timeout(e: io::Error) -> io::Error {
    match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => timed_out(),
        _ => e,
    }
}

impl Read for Deadline {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.conn.set_read_timeout(Some(self.remaining()?))?;
        self.conn.read(buf).map_err(map_timeout)
    }
}

impl Write for Deadline {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.conn.set_write_timeout(Some(self.remaining()?))?;
        self.conn.write(buf).map_err(map_timeout)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.conn.flush()
    }
}

/// Wraps `s` as one HMP string argument. The monitor splits arguments on
/// whitespace unless they are double-quoted, inside which backslash, the
/// quote itself and line breaks are escaped.
pub(crate) fn hmp_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Accumulates monitor output until the prompt arrives and returns
/// everything before it. On EOF before the prompt the error reads
/// `connection closed (is another client attached to the monitor?)`.
pub(crate) fn read_until_prompt(r: &mut dyn Read) -> Result<String> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let n = match r.read(&mut tmp) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        buf.extend_from_slice(&tmp[..n]);
        if buf.ends_with(HMP_PROMPT.as_bytes()) {
            buf.truncate(buf.len() - HMP_PROMPT.len());
            return Ok(match String::from_utf8(buf) {
                Ok(s) => s,
                Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
            });
        }
        if n == 0 {
            bail!("connection closed (is another client attached to the monitor?)");
        }
    }
}

/// Strips terminal escapes, CRs and the echoed command line. Readline redraws
/// the whole input after every byte, so the echo line is a jumble of
/// prefixes that ends with the full command.
pub(crate) fn clean_hmp_response(raw: &str, command: &str) -> String {
    let cleaned = ansi_escape().replace_all(raw, "").replace('\r', "");
    let body = match cleaned.split_once('\n') {
        Some((first, rest)) if first.ends_with(command) => rest,
        _ => cleaned.as_str(),
    };
    body.trim().to_string()
}

/// Runs an HMP command that prints nothing on success and turns any output
/// (QEMU's `Error: ...` lines) into the error `QEMU: <output>`.
pub(crate) fn monitor_must_succeed(storage: &Path, name: &str, command: &str) -> Result<()> {
    let resp = monitor_command(storage, name, command)?;
    if !resp.is_empty() {
        bail!("QEMU: {resp}");
    }
    Ok(())
}

/// Whether the HMP monitor behind `sock` accepts a connection and prompts
/// within `timeout`. QEMU creates the listening socket while still parsing
/// its command line, so a connection alone proves little; the prompt arrives
/// once the main loop runs, that is, once setup is through.
pub(crate) fn monitor_answers(sock: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let Ok(conn) = connect_by(sock, deadline) else {
        return false;
    };
    let mut conn = Deadline { conn, at: deadline };
    read_until_prompt(&mut conn).is_ok()
}

/// A fake HMP monitor on a Unix socket, for the tests of everything that
/// talks to QEMU through the monitor.
#[cfg(test)]
pub(crate) mod testutil {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread::{self, JoinHandle};

    /// What QEMU prints on connect.
    pub(crate) const BANNER: &str =
        "QEMU 9.2.0 monitor - type 'help' for more information\r\n(qemu) ";

    /// Serves one monitor session the way QEMU's readline HMP does: banner
    /// and prompt, a per-byte redraw echo of the command, then reply and
    /// prompt. The thread ends with that session, so a client that
    /// reconnects per command, as [`super::monitor_command`] does, needs
    /// [`fake_hmp_sessions`].
    pub(crate) fn fake_hmp(
        sock: &Path,
        reply: impl Fn(&str) -> String + Send + 'static,
    ) -> JoinHandle<()> {
        let listener = UnixListener::bind(sock).expect("bind fake monitor");
        thread::spawn(move || {
            if let Ok((conn, _)) = listener.accept() {
                serve_hmp_session(conn, &reply);
            }
        })
    }

    /// A fake monitor that serves one session after another until dropped.
    pub(crate) struct FakeHmp {
        sock: PathBuf,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl Drop for FakeHmp {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            // Wake the accept loop so it sees the flag. Should the socket be
            // gone already, the thread stays parked in accept until the test
            // process exits, which is harmless.
            if UnixStream::connect(&self.sock).is_ok() {
                if let Some(thread) = self.thread.take() {
                    let _ = thread.join();
                }
            }
        }
    }

    /// [`fake_hmp`] for a client that reconnects per command: it serves one
    /// session after another until the returned guard is dropped.
    pub(crate) fn fake_hmp_sessions(
        sock: &Path,
        reply: impl Fn(&str) -> String + Send + 'static,
    ) -> FakeHmp {
        let listener = UnixListener::bind(sock).expect("bind fake monitor");
        let stop = Arc::new(AtomicBool::new(false));
        let thread = thread::spawn({
            let stop = Arc::clone(&stop);
            move || {
                for conn in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    match conn {
                        Ok(conn) => serve_hmp_session(conn, &reply),
                        Err(_) => break,
                    }
                }
            }
        });
        FakeHmp {
            sock: sock.to_path_buf(),
            stop,
            thread: Some(thread),
        }
    }

    /// Answers one command on `conn` the way QEMU's readline HMP does, then
    /// closes it.
    pub(crate) fn serve_hmp_session(mut conn: UnixStream, reply: &dyn Fn(&str) -> String) {
        if conn.write_all(BANNER.as_bytes()).is_err() {
            return;
        }
        let Ok(mut reader) = conn.try_clone().map(BufReader::new) else {
            return;
        };
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() || !line.ends_with('\n') {
            return;
        }
        let cmd = line.trim_end_matches('\n');
        let mut echo = String::new();
        for (i, c) in cmd.char_indices() {
            echo.push_str(&"\x1b[D".repeat(i));
            echo.push_str(&cmd[..i + c.len_utf8()]);
            echo.push_str("\x1b[K");
        }
        echo.push_str("\r\n");
        let _ = conn.write_all(format!("{echo}{}(qemu) ", reply(cmd)).as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{Cursor, Read, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::{Arc, Mutex};

    use super::testutil::*;
    use super::*;
    use crate::vm::config::vm_dir;

    #[test]
    fn command_success_yields_empty_output() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("m.sock");
        let seen = Arc::new(Mutex::new(String::new()));
        let server = fake_hmp(&sock, {
            let seen = Arc::clone(&seen);
            move |cmd| {
                *seen.lock().unwrap() = cmd.to_string();
                String::new()
            }
        });
        let out = hmp_command(
            &sock,
            "device_add usb-host,id=usb-046d-085c,bus=xhci.0,vendorid=0x046d,productid=0x085c",
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(out, "", "success must yield empty output");
        server.join().unwrap();
        let seen = seen.lock().unwrap();
        assert!(
            seen.starts_with("device_add usb-host,id=usb-046d-085c"),
            "monitor received {seen:?}"
        );
    }

    #[test]
    fn command_returns_qemu_error_lines() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("m.sock");
        let server = fake_hmp(&sock, |_| "Error: Bus 'xhci.0' not found\r\n".to_string());
        let out = hmp_command(
            &sock,
            "device_add usb-host,id=x,bus=xhci.0",
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(out, "Error: Bus 'xhci.0' not found");
        server.join().unwrap();
    }

    #[test]
    fn command_without_socket_is_a_connect_error() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("missing.sock");
        let err = hmp_command(&sock, "info usb", Duration::from_secs(1)).unwrap_err();
        let text = err.to_string();
        assert!(
            text.starts_with(&format!("connect to QEMU monitor: {}: ", sock.display())),
            "{text}"
        );
    }

    #[test]
    fn command_on_a_closed_connection_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("m.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        // Accept and hang up without ever prompting, as QEMU does for a
        // second client while the first one is attached.
        let server = thread::spawn(move || drop(listener.accept()));
        let err = hmp_command(&sock, "info usb", Duration::from_secs(1)).unwrap_err();
        assert_eq!(
            err.to_string(),
            "QEMU monitor: connection closed (is another client attached to the monitor?)"
        );
        server.join().unwrap();
    }

    #[test]
    fn command_gives_up_at_the_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("m.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        // Prompt once, then keep the connection open and never answer.
        let server = thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            conn.write_all(BANNER.as_bytes()).unwrap();
            let mut sink = [0u8; 256];
            while matches!(conn.read(&mut sink), Ok(n) if n > 0) {}
        });
        let started = Instant::now();
        let err = hmp_command(&sock, "info usb", Duration::from_millis(200)).unwrap_err();
        let took = started.elapsed();
        assert_eq!(err.to_string(), "QEMU monitor: i/o timeout");
        assert!(took >= Duration::from_millis(200), "gave up after {took:?}");
        assert!(took < Duration::from_secs(5), "took {took:?}");
        server.join().unwrap();
    }

    /// A listener that never accepts and whose backlog is full, as QEMU's
    /// monitor is while another client is attached, makes the connect time
    /// out at the deadline instead of hanging (Go: `net.DialTimeout`).
    #[test]
    fn command_gives_up_on_a_monitor_that_does_not_accept() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("m.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        // A backlog of none: one connection waits, the next one cannot.
        // SAFETY: listen(2) on a socket this test owns.
        assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
        let _waiting = UnixStream::connect(&sock).unwrap();

        let (done_tx, done) = std::sync::mpsc::channel();
        let probe = sock.clone();
        thread::spawn(move || {
            let started = Instant::now();
            let result = hmp_command(&probe, "info usb", Duration::from_millis(300));
            let _ = done_tx.send((result, started.elapsed()));
        });
        let (result, took) = done
            .recv_timeout(Duration::from_secs(5))
            .expect("the connect hangs on a full backlog");
        assert_eq!(
            result.unwrap_err().to_string(),
            format!("connect to QEMU monitor: {}: i/o timeout", sock.display())
        );
        assert!(took >= Duration::from_millis(300), "gave up after {took:?}");

        let started = Instant::now();
        assert!(!monitor_answers(&sock, Duration::from_millis(200)));
        assert!(started.elapsed() < Duration::from_secs(2));

        // Once the listener takes the waiting connection there is room, and
        // the timeout covers the connect and the exchange together.
        drop(listener.accept().unwrap());
        let started = Instant::now();
        let err = hmp_command(&sock, "info usb", Duration::from_millis(300)).unwrap_err();
        assert_eq!(err.to_string(), "QEMU monitor: i/o timeout");
        let took = started.elapsed();
        assert!(
            took >= Duration::from_millis(300) && took < Duration::from_secs(2),
            "{took:?}"
        );
    }

    #[test]
    fn connect_errors_read_like_the_standard_library() {
        let dir = tempfile::tempdir().unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        let missing = dir.path().join("missing.sock");
        let want = UnixStream::connect(&missing).unwrap_err();
        let got = connect_by(&missing, deadline).unwrap_err();
        assert_eq!(
            (got.kind(), got.to_string()),
            (want.kind(), want.to_string())
        );

        // A socket file nobody listens on any more is refused.
        let stale = dir.path().join("stale.sock");
        drop(UnixListener::bind(&stale).unwrap());
        let want = UnixStream::connect(&stale).unwrap_err();
        let got = connect_by(&stale, deadline).unwrap_err();
        assert_eq!(
            (got.kind(), got.to_string()),
            (want.kind(), want.to_string())
        );

        let long = dir.path().join("x".repeat(200));
        let got = connect_by(&long, deadline).unwrap_err();
        assert_eq!(got.kind(), io::ErrorKind::InvalidInput);
        let got = connect_by(Path::new("bad\0path"), deadline).unwrap_err();
        assert_eq!(got.kind(), io::ErrorKind::InvalidInput);

        // A connection that is made is an ordinary blocking stream.
        let ok = dir.path().join("ok.sock");
        let listener = UnixListener::bind(&ok).unwrap();
        let mut conn = connect_by(&ok, deadline).unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        peer.write_all(b"hi").unwrap();
        let mut buf = [0u8; 2];
        conn.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"hi");
    }

    #[test]
    fn clean_response_without_echo_keeps_the_first_line() {
        // A monitor without readline echoes nothing; the first line is real output.
        let got = clean_hmp_response("  Device 0.1, Port 1, Speed 480 Mb/s\r\n", "info usb");
        assert!(got.starts_with("Device 0.1"), "got {got:?}");
    }

    #[test]
    fn clean_response_drops_escapes_and_the_echo() {
        let raw = "i\x1b[K\x1b[Din\x1b[K\x1b[D\x1b[Dinf\x1b[K\x1b[D\x1b[D\x1b[Dinfo\x1b[K\r\n\
                   Device 0.1, Port 1, Speed 480 Mb/s\r\n";
        assert_eq!(
            clean_hmp_response(raw, "info"),
            "Device 0.1, Port 1, Speed 480 Mb/s"
        );
        assert_eq!(
            clean_hmp_response("drive_add 0 x\r\nOK\r\n", "drive_add 0 x"),
            "OK"
        );
        assert_eq!(clean_hmp_response("device_add x\r\n", "device_add x"), "");
        assert_eq!(clean_hmp_response("", "info usb"), "");
        // Only a first line that ends with the command is an echo.
        assert_eq!(
            clean_hmp_response("info usbhost\r\nx\r\n", "info usb"),
            "info usbhost\nx"
        );
    }

    #[test]
    fn quote_escapes_backslash_quote_and_line_breaks() {
        let cases = [
            (r"file=/isos/a.iso", r#""file=/isos/a.iso""#),
            (r"file=/isos/my disk.iso", r#""file=/isos/my disk.iso""#),
            (
                r#"file=/isos/say "hi".iso"#,
                r#""file=/isos/say \"hi\".iso""#,
            ),
            (r"file=C:\isos\a.iso", r#""file=C:\\isos\\a.iso""#),
            (
                "file=/isos/line\nbreak.iso",
                r#""file=/isos/line\nbreak.iso""#,
            ),
            ("a\rb", r#""a\rb""#),
            ("", r#""""#),
        ];
        for (input, want) in cases {
            assert_eq!(hmp_quote(input), want, "hmp_quote({input:?})");
        }
    }

    #[test]
    fn read_until_prompt_stops_at_the_prompt() {
        let mut r = Cursor::new(b"QEMU 9.2.0 monitor\r\n(qemu) ".to_vec());
        assert_eq!(read_until_prompt(&mut r).unwrap(), "QEMU 9.2.0 monitor\r\n");
        let mut r = Cursor::new(b"(qemu) ".to_vec());
        assert_eq!(read_until_prompt(&mut r).unwrap(), "");
        // Only a prompt at the very end counts: a banner that arrives after
        // it in the same chunk hides it, as it does for QEMU's real client.
        let mut r = Cursor::new(b"(qemu) rest".to_vec());
        assert!(read_until_prompt(&mut r).is_err());
        // EOF before the prompt, even with output, is the "closed" error.
        let mut r = Cursor::new(b"partial (qemu)".to_vec());
        assert_eq!(
            read_until_prompt(&mut r).unwrap_err().to_string(),
            "connection closed (is another client attached to the monitor?)"
        );
        let mut r = Cursor::new(Vec::new());
        assert!(read_until_prompt(&mut r).is_err());
    }

    #[test]
    fn answers_only_once_the_prompt_arrives() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!monitor_answers(
            &dir.path().join("missing.sock"),
            Duration::from_millis(250)
        ));

        let sock = dir.path().join("m.sock");
        let monitor = fake_hmp_sessions(&sock, |_| String::new());
        assert!(monitor_answers(&sock, Duration::from_secs(1)));
        assert!(
            monitor_answers(&sock, Duration::from_secs(1)),
            "every probe gets a session"
        );
        drop(monitor);

        // A socket that accepts but never prompts, as QEMU's does while it
        // is still setting up, is not an answer.
        let silent = dir.path().join("silent.sock");
        let listener = UnixListener::bind(&silent).unwrap();
        let server = thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut sink = [0u8; 16];
            while matches!(conn.read(&mut sink), Ok(n) if n > 0) {}
        });
        assert!(!monitor_answers(&silent, Duration::from_millis(250)));
        server.join().unwrap();
    }

    #[test]
    fn must_succeed_turns_output_into_an_error() {
        let storage = tempfile::tempdir().unwrap();
        fs::create_dir_all(vm_dir(storage.path(), "t")).unwrap();
        let _monitor = fake_hmp_sessions(&monitor_path(storage.path(), "t"), |cmd| {
            if cmd == "device_del gone" {
                "Error: Device 'gone' not found\r\n".to_string()
            } else {
                String::new()
            }
        });
        monitor_must_succeed(storage.path(), "t", "device_del ok").unwrap();
        let err = monitor_must_succeed(storage.path(), "t", "device_del gone").unwrap_err();
        assert_eq!(err.to_string(), "QEMU: Error: Device 'gone' not found");
        assert_eq!(
            monitor_command(storage.path(), "t", "device_del ok").unwrap(),
            ""
        );
    }
}
