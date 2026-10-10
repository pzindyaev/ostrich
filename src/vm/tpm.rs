//! The emulated TPM 2.0: swtpm started next to QEMU.

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;

use super::config::{tpm_dir, tpm_log_path, tpm_pid_path, tpm_sock_path};
use super::firmware::{combined_output, exit_status_text, lookup_tool};
use super::process::pid_runs;

/// Emulates a TPM 2.0 in software; QEMU talks to it over a Unix socket.
pub(crate) const TPM_BIN: &str = "swtpm";

/// How long swtpm's parent may keep the output pipe open after it has
/// exited (Go's `WaitDelay`): the daemon inherits the pipe.
const PIPE_GRACE: Duration = Duration::from_secs(2);
/// How long the control socket is waited for after swtpm started.
const SOCKET_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a terminated swtpm gets to exit before it is killed.
const STOP_TIMEOUT: Duration = Duration::from_secs(2);
/// The pause between checks on the socket or the process.
const POLL: Duration = Duration::from_millis(50);

/// Whether the TPM emulator is installed. Error:
/// `swtpm not found — it provides the emulated TPM 2.0. Install it:\n  sudo pacman -S swtpm      # Arch\n  sudo apt install swtpm    # Debian/Ubuntu\n  sudo dnf install swtpm    # Fedora\n  brew install swtpm        # macOS\nor disable the TPM for this VM`.
pub fn check_tpm() -> Result<()> {
    lookup_tool(TPM_BIN).map(|_| ()).ok_or_else(tpm_not_found)
}

fn tpm_not_found() -> anyhow::Error {
    anyhow!(
        concat!(
            "{} not found — it provides the emulated TPM 2.0. Install it:\n",
            "  sudo pacman -S swtpm      # Arch\n",
            "  sudo apt install swtpm    # Debian/Ubuntu\n",
            "  sudo dnf install swtpm    # Fedora\n",
            "  brew install swtpm        # macOS\n",
            "or disable the TPM for this VM"
        ),
        TPM_BIN
    )
}

/// `<prefix><path>`, with the path's bytes untouched.
fn prefixed(prefix: &str, path: &Path) -> OsString {
    let mut s = OsString::from(prefix);
    s.push(path);
    s
}

/// Launches swtpm for the VM as a daemon
/// (`swtpm socket --tpm2 --tpmstate dir=<tpm dir> --ctrl type=unixio,path=<sock> --pid file=<pid> --log file=<log> --terminate --daemon`)
/// and waits up to 5 s for its control socket.
///
/// The TPM's state lives in the VM directory, so the guest sees the same TPM
/// across restarts. `--terminate` makes swtpm exit on its own once QEMU
/// disconnects; [`stop_tpm`] is the belt to those braces.
pub(crate) fn start_tpm(storage: &Path, name: &str) -> Result<()> {
    let bin = lookup_tool(TPM_BIN).ok_or_else(tpm_not_found)?;
    stop_tpm(storage, name); // a stale daemon would hold the socket
    let state_dir = tpm_dir(storage, name);
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&state_dir)
        .with_context(|| format!("create TPM state directory {}", state_dir.display()))?;
    let sock = tpm_sock_path(storage, name);

    let mut cmd = Command::new(bin);
    cmd.args(["socket", "--tpm2"])
        .arg("--tpmstate")
        .arg(prefixed("dir=", &state_dir))
        .arg("--ctrl")
        .arg(prefixed("type=unixio,path=", &sock))
        .arg("--pid")
        .arg(prefixed("file=", &tpm_pid_path(storage, name)))
        .arg("--log")
        .arg(prefixed("file=", &tpm_log_path(storage, name)))
        .args(["--terminate", "--daemon"]);
    // The parent returns once the daemon is set up; should the daemon hang on
    // to our stdio pipe, give up waiting for it rather than block forever.
    let (status, out) =
        combined_output(cmd, Some(PIPE_GRACE)).with_context(|| format!("start {TPM_BIN}"))?;
    if !status.success() {
        bail!(
            "start {TPM_BIN}: {}\n{}",
            exit_status_text(status),
            String::from_utf8_lossy(&out).trim()
        );
    }

    let deadline = Instant::now() + SOCKET_TIMEOUT;
    while Instant::now() < deadline {
        if sock.exists() {
            return Ok(());
        }
        thread::sleep(POLL);
    }
    stop_tpm(storage, name);
    bail!("{TPM_BIN} did not create its socket {}", sock.display())
}

/// Terminates the VM's swtpm if it is still around (SIGTERM, up to 2 s,
/// then SIGKILL) and removes its socket and PID file. Safe to call when
/// nothing is running, and when the PID in the file has gone to another
/// program since (after a host reboot, say): that process is left alone.
pub(crate) fn stop_tpm(storage: &Path, name: &str) {
    let pid_path = tpm_pid_path(storage, name);
    if let Some(pid) = fs::read_to_string(&pid_path)
        .ok()
        .and_then(|data| data.trim().parse::<i32>().ok())
        .filter(|pid| *pid > 0 && pid_runs(*pid, TPM_BIN))
    {
        let pid = Pid::from_raw(pid);
        // A PID that is gone or not ours is skipped without a word.
        if kill(pid, Signal::SIGTERM).is_ok() {
            let deadline = Instant::now() + STOP_TIMEOUT;
            while Instant::now() < deadline && kill(pid, None).is_ok() {
                thread::sleep(POLL);
            }
            if kill(pid, None).is_ok() {
                let _ = kill(pid, Signal::SIGKILL);
            }
        }
    }
    let _ = fs::remove_file(&pid_path);
    let _ = fs::remove_file(tpm_sock_path(storage, name));
}

/// The guest-facing TPM interface for the machine type: `tpm-tis` on x86,
/// `tpm-tis-device` on the arm `virt` board (the sysbus variant).
pub(crate) fn tpm_device(machine: &str) -> &'static str {
    if machine == "virt" {
        "tpm-tis-device"
    } else {
        "tpm-tis"
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::ExitStatusExt;
    use std::path::PathBuf;

    use tempfile::TempDir;

    use super::super::config::vm_dir;
    use super::super::firmware::{fixture, spawn_retrying};
    use super::*;

    const NOT_FOUND: &str = "swtpm not found — it provides the emulated TPM 2.0. Install it:\n  sudo pacman -S swtpm      # Arch\n  sudo apt install swtpm    # Debian/Ubuntu\n  sudo dnf install swtpm    # Fedora\n  brew install swtpm        # macOS\nor disable the TPM for this VM";

    /// A scratch storage directory whose socket paths fit `sun_path`.
    fn storage() -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        if dir.path().as_os_str().len() < 60 {
            dir
        } else {
            tempfile::tempdir_in("/tmp").unwrap()
        }
    }

    /// A fake `swtpm` on the fixture's tool directory.
    fn fake_swtpm(body: &str) -> (TempDir, fixture::Guard) {
        let bin = tempfile::tempdir().unwrap();
        let path = bin.path().join(TPM_BIN);
        fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let guard = fixture::install(Vec::new());
        fixture::update(|h| h.tool_dir = Some(bin.path().to_path_buf()));
        (bin, guard)
    }

    /// A stand-in program's body that idles until SIGTERM and stays the
    /// shell running its script, so `/proc` names the script.
    const IDLE_UNTIL_TERM: &str = "trap 'kill $! 2>/dev/null; exit 0' TERM\nsleep 60 &\nwait";

    /// Waits until the process just spawned as `pid` shows its command line
    /// (it has none until its exec is through), or gives up after 5 s.
    fn wait_for_exec(pid: u32) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| c.is_empty())
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn alive(pid: i32) -> bool {
        kill(Pid::from_raw(pid), None).is_ok()
    }

    /// Stops the VM's swtpm when the test ends, however it ends.
    struct StopOnDrop<'a>(&'a Path, &'a str);

    impl Drop for StopOnDrop<'_> {
        fn drop(&mut self) {
            stop_tpm(self.0, self.1);
        }
    }

    #[test]
    fn tpm_device_names() {
        assert_eq!(tpm_device("virt"), "tpm-tis-device");
        assert_eq!(tpm_device("q35"), "tpm-tis");
        assert_eq!(tpm_device("pc"), "tpm-tis");
        assert_eq!(tpm_device(""), "tpm-tis");
    }

    #[test]
    fn check_tpm_looks_up_swtpm() {
        assert_eq!(tpm_not_found().to_string(), NOT_FOUND);
        if which::which(TPM_BIN).is_ok() {
            check_tpm().unwrap();
        }

        // With nothing on the path both the check and a start say so.
        let empty = tempfile::tempdir().unwrap();
        let _guard = fixture::install(Vec::new());
        fixture::update(|h| h.tool_dir = Some(empty.path().to_path_buf()));
        assert_eq!(check_tpm().unwrap_err().to_string(), NOT_FOUND);
        let storage = storage();
        assert_eq!(
            start_tpm(storage.path(), "vm").unwrap_err().to_string(),
            NOT_FOUND
        );
        assert!(
            !tpm_dir(storage.path(), "vm").exists(),
            "nothing should be set up"
        );
    }

    #[test]
    fn stop_tpm_without_a_daemon_is_a_no_op() {
        let storage = storage();
        let storage = storage.path();
        fs::create_dir_all(vm_dir(storage, "vm")).unwrap();
        stop_tpm(storage, "vm");

        // Garbage, zero and negative PIDs are ignored; a PID beyond
        // pid_max is dead; the socket and PID file go either way, the log
        // and the state stay.
        for pid in ["garbage", "", "0", "-5", " 2147483647 \n"] {
            let pid_path = tpm_pid_path(storage, "vm");
            fs::write(&pid_path, pid).unwrap();
            fs::write(tpm_sock_path(storage, "vm"), "").unwrap();
            fs::write(tpm_log_path(storage, "vm"), "log").unwrap();
            fs::create_dir_all(tpm_dir(storage, "vm")).unwrap();
            stop_tpm(storage, "vm");
            assert!(!pid_path.exists(), "pid file kept for {pid:?}");
            assert!(
                !tpm_sock_path(storage, "vm").exists(),
                "socket kept for {pid:?}"
            );
            assert!(tpm_log_path(storage, "vm").exists());
            assert!(tpm_dir(storage, "vm").exists());
        }
    }

    /// A swtpm.pid whose PID another program holds now is not signalled;
    /// a script standing in for swtpm is.
    #[test]
    fn stop_tpm_leaves_other_programs_alone() {
        if !Path::new("/proc/self/cmdline").exists() {
            eprintln!("skipping: telling programs apart reads /proc");
            return;
        }
        let storage = storage();
        let storage = storage.path();
        fs::create_dir_all(vm_dir(storage, "vm")).unwrap();
        let pid_path = tpm_pid_path(storage, "vm");

        let mut stranger = Command::new("sleep").arg("60").spawn().unwrap();
        wait_for_exec(stranger.id());
        fs::write(&pid_path, format!("{}\n", stranger.id())).unwrap();
        fs::write(tpm_sock_path(storage, "vm"), "").unwrap();
        let started = Instant::now();
        stop_tpm(storage, "vm");
        let elapsed = started.elapsed();
        let untouched = stranger.try_wait().unwrap();
        let _ = stranger.kill();
        let _ = stranger.wait();
        assert_eq!(
            untouched, None,
            "stop_tpm signalled a process that is no swtpm"
        );
        assert!(elapsed < STOP_TIMEOUT, "{elapsed:?}");
        assert!(!pid_path.exists() && !tpm_sock_path(storage, "vm").exists());

        let bin = tempfile::tempdir().unwrap();
        let exe = bin.path().join(TPM_BIN);
        fs::write(&exe, format!("#!/bin/sh\n{IDLE_UNTIL_TERM}\n")).unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
        let fake = spawn_retrying(&mut Command::new(&exe)).unwrap();
        wait_for_exec(fake.id());
        let pid = fake.id() as i32;
        // Reap it as soon as it exits, as init would reap a real daemon.
        let reaper = thread::spawn(move || {
            let mut fake = fake;
            fake.wait().unwrap()
        });
        fs::write(&pid_path, pid.to_string()).unwrap();
        stop_tpm(storage, "vm");
        // It exits through its trap, or dies of the signal when that came
        // before the trap was set.
        let status = reaper.join().unwrap();
        assert!(
            status.success() || status.signal() == Some(Signal::SIGTERM as i32),
            "the stand-in did not get SIGTERM: {status:?}"
        );
        assert!(!alive(pid));
        assert!(!pid_path.exists());
    }

    #[test]
    fn start_and_stop_a_real_swtpm() {
        if which::which(TPM_BIN).is_err() {
            eprintln!("skipping: {TPM_BIN} not installed");
            return;
        }
        let storage = storage();
        let storage = storage.path();
        let name = "tpm";
        fs::create_dir_all(vm_dir(storage, name)).unwrap();
        let _stop = StopOnDrop(storage, name);

        start_tpm(storage, name).unwrap();
        let sock = tpm_sock_path(storage, name);
        let pid_path = tpm_pid_path(storage, name);
        assert!(sock.exists(), "control socket missing");
        let pid: i32 = fs::read_to_string(&pid_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(alive(pid), "swtpm {pid} is not running");
        let state = fs::metadata(tpm_dir(storage, name)).unwrap();
        assert!(state.is_dir());
        assert_eq!(state.permissions().mode() & 0o777, 0o700);
        assert!(
            tpm_log_path(storage, name).exists(),
            "swtpm should log to its file"
        );

        // Starting again replaces the running daemon rather than failing on
        // the socket it holds.
        start_tpm(storage, name).unwrap();
        let second: i32 = fs::read_to_string(&pid_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_ne!(pid, second);
        assert!(!alive(pid), "stale swtpm {pid} survived the restart");
        assert!(alive(second));

        stop_tpm(storage, name);
        assert!(!alive(second), "swtpm {second} survived stop");
        assert!(
            !pid_path.exists() && !sock.exists(),
            "pid file or socket left behind"
        );
        assert!(tpm_log_path(storage, name).exists());
        assert!(
            tpm_dir(storage, name).exists(),
            "the TPM state must survive a stop"
        );
    }

    #[test]
    fn start_tpm_tolerates_a_daemon_holding_the_pipe() {
        // The fake parent creates the "socket", leaves a child holding our
        // pipe for far longer than the grace period and exits.
        let (_bin, _guard) = fake_swtpm(
            "while [ $# -gt 0 ]; do case \"$1\" in --ctrl) sock=\"${2#type=unixio,path=}\";; esac; shift; done\n: > \"$sock\"\nsleep 10 &\nexit 0\n",
        );
        let storage = storage();
        let storage = storage.path();
        fs::create_dir_all(vm_dir(storage, "vm")).unwrap();
        let started = Instant::now();
        start_tpm(storage, "vm").unwrap();
        let elapsed = started.elapsed();
        assert!(
            elapsed >= PIPE_GRACE && elapsed < PIPE_GRACE + Duration::from_secs(3),
            "{elapsed:?}"
        );
        assert!(tpm_sock_path(storage, "vm").exists());
        stop_tpm(storage, "vm");
        assert!(!tpm_sock_path(storage, "vm").exists());
    }

    #[test]
    fn start_tpm_reports_a_failing_swtpm() {
        let (_bin, _guard) = fake_swtpm("echo boom\necho more >&2\nexit 1\n");
        let storage = storage();
        let storage = storage.path();
        fs::create_dir_all(vm_dir(storage, "vm")).unwrap();
        let err = start_tpm(storage, "vm").unwrap_err();
        assert_eq!(err.to_string(), "start swtpm: exit status 1\nboom\nmore");
        assert!(
            tpm_dir(storage, "vm").is_dir(),
            "the state directory is created first"
        );
    }

    #[test]
    fn start_tpm_reports_a_missing_socket() {
        let (_bin, _guard) = fake_swtpm("exit 0\n");
        let storage = storage();
        let storage = storage.path();
        fs::create_dir_all(vm_dir(storage, "vm")).unwrap();
        let started = Instant::now();
        let err = start_tpm(storage, "vm").unwrap_err();
        let elapsed = started.elapsed();
        assert_eq!(
            err.to_string(),
            format!(
                "swtpm did not create its socket {}",
                tpm_sock_path(storage, "vm").display()
            )
        );
        assert!(
            elapsed >= SOCKET_TIMEOUT && elapsed < SOCKET_TIMEOUT + Duration::from_secs(2),
            "{elapsed:?}"
        );
    }

    /// Checks swtpm is started with the VM, the guest gets a TPM device and
    /// the daemon is gone after Stop. Skipped without QEMU or swtpm.
    #[test]
    fn tpm_with_qemu() {
        use super::super::config::{NetworkConfig, NetworkType, VmConfig};
        use super::super::manager::Manager;
        use super::super::process::{start, stop};

        for bin in ["qemu-system-x86_64", "qemu-img", TPM_BIN] {
            if which::which(bin).is_err() {
                eprintln!("skipping: {bin} not installed");
                return;
            }
        }

        /// Stops the VM when the test ends, however it ends.
        struct StopVm<'a>(&'a Path, &'a str);
        impl Drop for StopVm<'_> {
            fn drop(&mut self) {
                let _ = stop(self.0, self.1);
            }
        }

        let storage = storage();
        let storage = storage.path();
        let mut cfg = VmConfig {
            name: "tpm".to_string(),
            cpu: 1,
            ram: 128,
            disk_size: 1,
            tpm: true,
            network: NetworkConfig {
                kind: NetworkType::None,
                ..NetworkConfig::default()
            },
            ..VmConfig::default()
        };
        Manager::new(storage).create(&mut cfg).unwrap();
        start(storage, &cfg).unwrap();
        let _stop = StopVm(storage, &cfg.name);

        let qtree = fixture::wait_for_monitor(storage, &cfg.name);
        assert!(
            qtree.contains("dev: tpm-tis"),
            "guest has no TPM device:\n{qtree}"
        );
        assert!(
            tpm_pid_path(storage, &cfg.name).exists(),
            "swtpm PID file missing"
        );
        let state: Vec<PathBuf> = fs::read_dir(tpm_dir(storage, &cfg.name))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert!(!state.is_empty(), "swtpm wrote no state");

        stop(storage, &cfg.name).unwrap();
        for p in [
            tpm_pid_path(storage, &cfg.name),
            tpm_sock_path(storage, &cfg.name),
        ] {
            assert!(!p.exists(), "{} still present after stop", p.display());
        }
    }
}
