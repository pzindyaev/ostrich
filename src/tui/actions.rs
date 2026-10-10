//! The dashboard's background work and host-side helpers: loading the lists
//! and details, starting and stopping VMs, the serial console command, the
//! VNC viewer and the clipboard.

use std::ffi::{OsStr, OsString};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;

use anyhow::{anyhow, bail, Context as _, Result};

use super::console::{sanitize_console_line, CONSOLE_TAIL_LINES};
use super::events::{err_text, Details, TaskResult, TemplateEntry, VmEntry};
use crate::vm::{self, Manager, VmConfig};

/// The error for every action that needs a running VM.
pub const NOT_RUNNING: &str = "VM is not running";

/// What `c` reports when socat is not installed.
pub const SOCAT_MISSING: &str = "socat not found — install it to connect interactively\n  sudo apt install socat   # Debian/Ubuntu\n  sudo dnf install socat   # Fedora\n  brew install socat       # macOS";

/// What `v` reports when no known viewer is installed.
pub const NO_VNC_VIEWER: &str = "no VNC viewer found — install one, e.g.:\n  sudo apt install tigervnc-viewer   # Debian/Ubuntu\n  sudo dnf install tigervnc          # Fedora\n  brew install --cask tigervnc-viewer # macOS";

/// What the udev-command copy reports without a clipboard tool.
pub const NO_CLIPBOARD_TOOL: &str =
    "no clipboard tool found — install wl-clipboard (Wayland) or xclip (X11), or select the command with the mouse";

/// The host address a VNC viewer connects to.
const VNC_HOST: &str = "127.0.0.1";
/// VNC display `n` listens on TCP port `5900 + n`.
const VNC_BASE_PORT: u32 = 5900;

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Lists the VMs with their status and the templates with their disk usage.
/// Each listing's error goes into its own field (`vm_err`, `tpl_err`), so a
/// templates directory that cannot be read does not hide the VMs.
pub fn load_all(mgr: &Manager) -> TaskResult {
    let storage = mgr.storage();
    let (vms, vm_err) = match mgr.list() {
        Ok(cfgs) => (
            cfgs.into_iter()
                .map(|cfg| {
                    // A status error (an unreadable PID file) counts as stopped.
                    let status = vm::status(storage, &cfg.name).unwrap_or_default();
                    VmEntry { cfg, status }
                })
                .collect(),
            None,
        ),
        Err(e) => (Vec::new(), Some(err_text(&e))),
    };
    let (templates, tpl_err) = match mgr.list_templates() {
        Ok(tpls) => (
            tpls.into_iter()
                .map(|tpl| {
                    let disk_usage = vm::template_disk_usage(storage, &tpl.name);
                    TemplateEntry { tpl, disk_usage }
                })
                .collect(),
            None,
        ),
        Err(e) => (Vec::new(), Some(err_text(&e))),
    };
    TaskResult::Loaded {
        vms,
        templates,
        vm_err,
        tpl_err,
    }
}

/// Gathers everything the details pane shows for one VM: status, guest IP
/// (when running), USB states, boot ISO state, USB image states, extra disk
/// states and the sanitised console tail (last 200 lines).
pub fn load_details(storage: &Path, cfg: &VmConfig) -> TaskResult {
    // A missing or unreadable log is simply no output.
    let console: Vec<String> = vm::read_console_tail(storage, &cfg.name, CONSOLE_TAIL_LINES)
        .unwrap_or_default()
        .iter()
        .map(|l| sanitize_console_line(l))
        .collect();
    let status = vm::status(storage, &cfg.name).unwrap_or_default();
    let guest_ip = if status.running() {
        vm::guest_ip(cfg)
    } else {
        None
    };
    TaskResult::Details(Box::new(Details {
        name: cfg.name.clone(),
        status,
        guest_ip,
        usb: vm::usb_states(&cfg.usb_devices),
        cdrom: vm::image_state_of(&cfg.cdrom_path),
        images: vm::usb_image_states(&cfg.usb_images),
        disks: vm::disk_states(storage, cfg),
        console,
    }))
}

// ---------------------------------------------------------------------------
// VM actions
// ---------------------------------------------------------------------------

/// `Done { "<verb>ed <name>" }`, or `Failed` with the bare error (an empty
/// `what`): Go's status bar showed `✗ <err>` for these.
fn outcome(done: String, result: Result<()>) -> TaskResult {
    match result {
        Ok(()) => TaskResult::Done { what: done },
        Err(e) => failed(err_text(&e)),
    }
}

/// A [`TaskResult::Failed`] the status bar shows as the bare `err`.
fn failed(err: String) -> TaskResult {
    TaskResult::Failed {
        what: String::new(),
        err,
    }
}

/// `Done { "started <name>" }` or the bare start error.
pub fn start_vm(storage: &Path, cfg: &VmConfig) -> TaskResult {
    outcome(format!("started {}", cfg.name), vm::start(storage, cfg))
}

/// `Done { "stopped <name>" }` or the bare stop error.
pub fn stop_vm(storage: &Path, name: &str) -> TaskResult {
    outcome(format!("stopped {name}"), vm::stop(storage, name))
}

/// `Done { "deleted <name>" }` or the bare delete error.
pub fn delete_vm(mgr: &Manager, name: &str) -> TaskResult {
    outcome(format!("deleted {name}"), mgr.delete(name))
}

/// `Done { "deleted template <name>" }` or the bare delete error.
pub fn delete_template(mgr: &Manager, name: &str) -> TaskResult {
    outcome(
        format!("deleted template {name}"),
        mgr.delete_template(name),
    )
}

// ---------------------------------------------------------------------------
// Host tools
// ---------------------------------------------------------------------------

/// Where `name` is on the search path. `path` replaces `$PATH` when given
/// (tests).
fn on_path(name: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    match path {
        Some(p) => which::which_in(name, Some(p), "/"),
        None => which::which(name),
    }
    .ok()
}

/// The first of `names` found on the search path, with its index in
/// `names`. `path` replaces `$PATH` when given (tests).
fn first_on_path(names: &[&str], path: Option<&OsStr>) -> Option<(usize, PathBuf)> {
    names
        .iter()
        .enumerate()
        .find_map(|(i, name)| on_path(name, path).map(|p| (i, p)))
}

/// socat's options for our end of the serial console: the terminal in raw
/// mode without echo, so Ctrl-C, Ctrl-Z and the other control keys reach the
/// guest instead of signalling socat (and Ostrich, which shares its
/// terminal); Ctrl-] (0x1d) is the end of input that disconnects.
const SOCAT_TTY_OPTS: &str = "-,raw,echo=0,escape=0x1d";

/// The command that connects to the VM's serial socket interactively:
/// `socat -,raw,echo=0,escape=0x1d UNIX-CONNECT:<serial.sock>`. Ctrl-]
/// exits. Errors, as text: `VM is not running`, and when socat is missing
/// `socat not found — install it to connect interactively\n  sudo apt install socat   # Debian/Ubuntu\n  sudo dnf install socat   # Fedora\n  brew install socat       # macOS`.
pub fn serial_console_command(
    storage: &Path,
    name: &str,
    running: bool,
) -> std::result::Result<Command, String> {
    serial_console_command_in(storage, name, running, None)
}

/// [`serial_console_command`] with `path` in place of `$PATH` (tests).
fn serial_console_command_in(
    storage: &Path,
    name: &str,
    running: bool,
    path: Option<&OsStr>,
) -> std::result::Result<Command, String> {
    if !running {
        return Err(NOT_RUNNING.to_string());
    }
    let socat = on_path("socat", path).ok_or_else(|| SOCAT_MISSING.to_string())?;
    let mut target = OsString::from("UNIX-CONNECT:");
    target.push(vm::serial_sock_path(storage, name).as_os_str());
    let mut cmd = Command::new(socat);
    cmd.arg(SOCAT_TTY_OPTS).arg(target);
    Ok(cmd)
}

/// How a viewer wants the display named on its command line. Each viewer
/// has a different CLI convention for specifying host+port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VncArgStyle {
    /// TigerVNC / TightVNC: `host::port` (double-colon = explicit TCP port).
    HostPort,
    /// Remmina: `-c vnc://host:port`.
    RemminaUri,
    /// KRDC / Vinagre: `vnc://host:port` URI.
    Uri,
}

/// The supported viewers in preference order.
const KNOWN_VNC_VIEWERS: [(&str, VncArgStyle); 6] = [
    ("vncviewer", VncArgStyle::HostPort),
    ("tigervnc", VncArgStyle::HostPort),
    ("xtightvncviewer", VncArgStyle::HostPort),
    ("remmina", VncArgStyle::RemminaUri),
    ("krdc", VncArgStyle::Uri),
    ("vinagre", VncArgStyle::Uri),
];

/// The arguments that point a viewer of the given style at `host:port`.
fn vnc_viewer_args(style: VncArgStyle, host: &str, port: u32) -> Vec<String> {
    match style {
        VncArgStyle::HostPort => vec![format!("{host}::{port}")],
        VncArgStyle::RemminaUri => vec!["-c".to_string(), format!("vnc://{host}:{port}")],
        VncArgStyle::Uri => vec![format!("vnc://{host}:{port}")],
    }
}

/// The first available viewer: its path and argument style.
fn find_vnc_viewer(path: Option<&OsStr>) -> Option<(PathBuf, VncArgStyle)> {
    let names: Vec<&str> = KNOWN_VNC_VIEWERS.iter().map(|(n, _)| *n).collect();
    first_on_path(&names, path).map(|(i, p)| (p, KNOWN_VNC_VIEWERS[i].1))
}

/// Waits for a detached child on its own thread, so a viewer the user has
/// closed does not linger as a zombie for as long as Ostrich runs. When no
/// thread can be had (a pids limit), the viewer keeps running and is left
/// unreaped rather than taking the task down.
fn reap_in_background(mut child: Child) {
    let _ = thread::Builder::new()
        .name("ostrich-reaper".into())
        .spawn(move || {
            let _ = child.wait();
        });
}

/// Launches the first VNC viewer found (vncviewer, tigervnc, xtightvncviewer
/// with `host::port`; remmina with `-c vnc://host:port`; krdc and vinagre
/// with `vnc://host:port`) detached, against 127.0.0.1:5900+n.
/// `Done { "launched <viewer> → port <p>" }`, or a bare `Failed` with the Go
/// texts `VNC is not enabled for this VM (vnc_port: 0)`, `VM is not
/// running`, the no-viewer text, or `launch VNC viewer: <viewer path>:
/// <error>`.
pub fn launch_vnc_viewer(cfg: &VmConfig, running: bool) -> TaskResult {
    launch_vnc_viewer_in(cfg, running, None)
}

/// [`launch_vnc_viewer`] with `path` in place of `$PATH` (tests).
fn launch_vnc_viewer_in(cfg: &VmConfig, running: bool, path: Option<&OsStr>) -> TaskResult {
    if cfg.vnc_port == 0 {
        return failed("VNC is not enabled for this VM (vnc_port: 0)".to_string());
    }
    if !running {
        return failed(NOT_RUNNING.to_string());
    }
    let port = VNC_BASE_PORT + u32::from(cfg.vnc_port);
    let Some((viewer_path, style)) = find_vnc_viewer(path) else {
        return failed(NO_VNC_VIEWER.to_string());
    };
    let viewer = viewer_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // A GUI app: nothing to say to our terminal, and not ours to wait for.
    let spawned = Command::new(&viewer_path)
        .args(vnc_viewer_args(style, VNC_HOST, port))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match spawned {
        Ok(child) => {
            reap_in_background(child);
            TaskResult::Done {
                what: format!("launched {viewer} → port {port}"),
            }
        }
        // Go said `launch VNC viewer: fork/exec <path>: …`; Rust's exec
        // error does not name the binary, so the path goes in front of it.
        Err(e) => failed(format!("launch VNC viewer: {}: {e}", viewer_path.display())),
    }
}

/// A clipboard tool the way atotto/clipboard (what the Go program used)
/// picks it.
struct ClipboardTool {
    /// The program the text is piped to.
    copy: &'static str,
    args: &'static [&'static str],
    /// The paste half, which must be on the search path too.
    paste: Option<&'static str>,
    /// Considered only when `WAYLAND_DISPLAY` is set and not empty.
    wayland_only: bool,
}

/// atotto/clipboard v0.1.4's order on Linux and the BSDs: wl-copy (with
/// wl-paste, only under Wayland), `xclip -in -selection clipboard`,
/// `xsel --input --clipboard`, termux-clipboard-set (with
/// termux-clipboard-get), clip.exe (with powershell.exe, under WSL).
#[cfg(not(target_os = "macos"))]
const CLIPBOARD_TOOLS: &[ClipboardTool] = &[
    ClipboardTool {
        copy: "wl-copy",
        args: &[],
        paste: Some("wl-paste"),
        wayland_only: true,
    },
    ClipboardTool {
        copy: "xclip",
        args: &["-in", "-selection", "clipboard"],
        paste: None,
        wayland_only: false,
    },
    ClipboardTool {
        copy: "xsel",
        args: &["--input", "--clipboard"],
        paste: None,
        wayland_only: false,
    },
    ClipboardTool {
        copy: "termux-clipboard-set",
        args: &[],
        paste: Some("termux-clipboard-get"),
        wayland_only: false,
    },
    ClipboardTool {
        copy: "clip.exe",
        args: &[],
        paste: Some("powershell.exe"),
        wayland_only: false,
    },
];

/// On macOS atotto/clipboard always uses pbcopy.
#[cfg(target_os = "macos")]
const CLIPBOARD_TOOLS: &[ClipboardTool] = &[ClipboardTool {
    copy: "pbcopy",
    args: &[],
    paste: None,
    wayland_only: false,
}];

/// Whether this is a Wayland session: `WAYLAND_DISPLAY` set and not empty.
fn wayland_session() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty())
}

/// The first usable clipboard tool: its path and arguments. `wayland` says
/// whether wl-copy may be picked; `path` replaces `$PATH` when given (tests).
fn find_clipboard_tool(
    wayland: bool,
    path: Option<&OsStr>,
) -> Option<(PathBuf, &'static [&'static str])> {
    CLIPBOARD_TOOLS
        .iter()
        .filter(|t| wayland || !t.wayland_only)
        .find_map(|t| {
            let copy = on_path(t.copy, path)?;
            if let Some(paste) = t.paste {
                on_path(paste, path)?;
            }
            Some((copy, t.args))
        })
}

/// Runs `cmd` with `text` on its stdin and waits for it to succeed.
fn pipe_text(cmd: &mut Command, text: &str) -> Result<()> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let mut child = cmd
        .stdin(Stdio::piped())
        .spawn()
        .with_context(|| format!("run {program}"))?;
    // A tool that exits before reading everything breaks the pipe; its exit
    // status then says more than the broken pipe does, so that is reported
    // first, and the write error only when the tool claims success.
    let mut write_err = None;
    if let Some(mut stdin) = child.stdin.take() {
        if let Err(e) = stdin.write_all(text.as_bytes()) {
            if e.kind() != io::ErrorKind::BrokenPipe {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e).with_context(|| format!("write to {program}"));
            }
            write_err = Some(e);
        }
        // Dropping the pipe is the EOF the tool waits for.
    }
    let status = child
        .wait()
        .with_context(|| format!("wait for {program}"))?;
    if !status.success() {
        bail!("{program}: {status}");
    }
    if let Some(e) = write_err {
        return Err(e).with_context(|| format!("write to {program}"));
    }
    Ok(())
}

/// Copies `text` to the clipboard through the tool atotto/clipboard would
/// pick (see `CLIPBOARD_TOOLS`): `wl-copy` only in a Wayland session, then
/// `xclip -in -selection clipboard`, `xsel --input --clipboard`, …; `pbcopy`
/// on macOS. With none: `no clipboard tool found — install wl-clipboard
/// (Wayland) or xclip (X11), or select the command with the mouse`; a
/// failing tool: `copy to clipboard: <error>`.
pub fn copy_to_clipboard(text: &str) -> Result<()> {
    let (tool, args) =
        find_clipboard_tool(wayland_session(), None).ok_or_else(|| anyhow!(NO_CLIPBOARD_TOOL))?;
    let mut cmd = Command::new(tool);
    cmd.args(args).stdout(Stdio::null()).stderr(Stdio::null());
    pipe_text(&mut cmd, text).context("copy to clipboard")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::tui::events::failure_text;
    use crate::tui::testutil::*;
    use crate::vm::{FirmwareType, NetworkConfig, NetworkType, Template};

    /// Writes a `vm.yaml` for `cfg` without going through `create`.
    fn write_vm(storage: &Path, cfg: &VmConfig) {
        fs::create_dir_all(vm::vm_dir(storage, &cfg.name)).unwrap();
        vm::save_config(storage, cfg).unwrap();
    }

    /// A stopped BIOS VM with user networking.
    fn vm_cfg(name: &str) -> VmConfig {
        VmConfig {
            name: name.into(),
            cpu: 1,
            ram: 512,
            disk_size: 5,
            arch: "x86_64".into(),
            network: NetworkConfig {
                kind: NetworkType::User,
                mac: "52:54:00:00:00:01".into(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// Writes a template with a disk image file of `disk_bytes` bytes.
    fn write_template(storage: &Path, name: &str, disk_bytes: usize) {
        fs::create_dir_all(vm::template_dir(storage, name)).unwrap();
        let tpl = Template {
            name: name.into(),
            description: "base image".into(),
            source_vm: "src".into(),
            cpu: 2,
            ram: 1024,
            disk_size: 10,
            arch: "x86_64".into(),
            firmware: FirmwareType::Uefi,
            secure_boot: false,
            tpm: false,
            network: NetworkType::User,
            vnc: false,
            created_at: Default::default(),
        };
        vm::save_template(storage, &tpl).unwrap();
        fs::write(vm::template_disk_path(storage, name), vec![0u8; disk_bytes]).unwrap();
    }

    /// A directory of fake executables named `names`, for PATH lookups.
    #[cfg(unix)]
    fn fake_bin_dir(h: &Harness, names: &[&str]) -> PathBuf {
        fake_bin_dir_with(h, "bin", names, "#!/bin/sh\nexit 0\n")
    }

    /// Like [`fake_bin_dir`], every executable with the given contents; `tag`
    /// keeps the directory apart from the plain ones.
    #[cfg(unix)]
    fn fake_bin_dir_with(h: &Harness, tag: &str, names: &[&str], script: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = h.dir.path().join(format!("{tag}-{}", names.join("-")));
        fs::create_dir_all(&dir).unwrap();
        for n in names {
            let p = dir.join(n);
            fs::write(&p, script).unwrap();
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        }
        dir
    }

    #[test]
    fn load_all_lists_vms_and_templates() {
        let h = Harness::new();
        let storage = h.mgr.storage();
        write_vm(storage, &vm_cfg("b-vm"));
        write_vm(storage, &vm_cfg("a-vm"));
        write_template(storage, "base", 4096);
        match load_all(&h.mgr) {
            TaskResult::Loaded {
                vms,
                templates,
                vm_err,
                tpl_err,
            } => {
                assert_eq!(vm_err, None);
                assert_eq!(tpl_err, None);
                let names: Vec<&str> = vms.iter().map(|v| v.cfg.name.as_str()).collect();
                assert_eq!(names, ["a-vm", "b-vm"], "sorted by name");
                assert!(
                    vms.iter().all(|v| !v.status.running()),
                    "nothing runs: {vms:?}"
                );
                assert_eq!(templates.len(), 1);
                assert_eq!(templates[0].tpl.name, "base");
                assert_eq!(templates[0].tpl.description, "base image");
                assert_eq!(templates[0].disk_usage, 4096);
            }
            other => panic!("expected Loaded, got {other:?}"),
        }
    }

    #[test]
    fn load_all_reports_a_listing_error() {
        let h = Harness::new();
        // A storage path that is a file: read_dir fails, templates are
        // simply absent.
        let file = h.dir.path().join("not-a-dir");
        fs::write(&file, "x").unwrap();
        match load_all(&Manager::new(&file)) {
            TaskResult::Loaded {
                vms,
                templates,
                vm_err,
                ..
            } => {
                assert!(vms.is_empty());
                assert!(templates.is_empty());
                let err = vm_err.expect("a listing error");
                assert!(
                    err.starts_with(&format!("open {}", file.display())),
                    "{err}"
                );
            }
            other => panic!("expected Loaded, got {other:?}"),
        }
        // No storage directory at all is an empty dashboard, not an error.
        match load_all(&Manager::new(h.dir.path().join("missing"))) {
            TaskResult::Loaded {
                vms,
                templates,
                vm_err,
                tpl_err,
            } => {
                assert!(vms.is_empty() && templates.is_empty());
                assert_eq!((vm_err, tpl_err), (None, None));
            }
            other => panic!("expected Loaded, got {other:?}"),
        }
    }

    #[test]
    fn load_all_keeps_the_vms_when_the_templates_cannot_be_listed() {
        use std::os::unix::fs::PermissionsExt;
        let h = Harness::new();
        let storage = h.mgr.storage();
        write_vm(storage, &vm_cfg("a-vm"));
        let tpl_dir = vm::templates_dir(storage);
        fs::create_dir_all(&tpl_dir).unwrap();
        fs::set_permissions(&tpl_dir, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&tpl_dir).is_ok() {
            // Running as root: permissions do not stop the listing.
            fs::set_permissions(&tpl_dir, fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }
        let result = load_all(&h.mgr);
        fs::set_permissions(&tpl_dir, fs::Permissions::from_mode(0o755)).unwrap();
        match result {
            TaskResult::Loaded {
                vms,
                templates,
                vm_err,
                tpl_err,
            } => {
                assert_eq!(vm_err, None);
                assert_eq!(vms.len(), 1, "the VM list survives");
                assert!(templates.is_empty());
                let err = tpl_err.expect("a templates listing error");
                assert!(err.contains(".templates"), "{err}");
            }
            other => panic!("expected Loaded, got {other:?}"),
        }
    }

    #[test]
    fn load_details_reads_console_and_states() {
        let h = Harness::new();
        let storage = h.mgr.storage();
        let iso = h.dir.path().join("missing.iso");
        let mut cfg = vm_cfg("raw");
        cfg.cdrom_path = iso.to_string_lossy().into_owned();
        cfg.disks = vec![vm::Disk {
            name: "data".into(),
            size: 1,
        }];
        write_vm(storage, &cfg);
        let log = [
            "\x1b[2J\x1b[H\x1b[?25lGNU GRUB  version 2.12",
            "\x1b]0;qemu\x07\x1b(B\x1b[0;1;32m[  OK  ]\x1b[0m Started \x1b[0;1;39mNetwork Service\x1b[0m.",
            "[    0.000000] Linux version 6.8.0\tx86_64\t#1 SMP",
            "Downloading 10%\rDownloading 55%\rDownloading 100%\x1b[K",
            "",
            "login: \x1b[?25h",
        ]
        .join("\r\n");
        fs::write(vm::console_path(storage, "raw"), log).unwrap();

        let TaskResult::Details(d) = load_details(storage, &cfg) else {
            panic!("expected Details");
        };
        assert_eq!(d.name, "raw");
        assert!(!d.status.running());
        assert_eq!(d.guest_ip, None, "no IP for a stopped VM");
        assert_eq!(d.console[0], "GNU GRUB  version 2.12");
        assert_eq!(d.console[1], "[  OK  ] Started Network Service.");
        assert_eq!(d.console[3], "Downloading 100%");
        assert_eq!(d.console.last().map(String::as_str), Some("login:"));
        assert!(
            d.console.iter().all(|l| !l.contains('\x1b')),
            "{:?}",
            d.console
        );
        assert_eq!(d.cdrom.path, cfg.cdrom_path);
        assert!(
            d.cdrom
                .err
                .as_deref()
                .is_some_and(|e| e.contains("not found")),
            "{:?}",
            d.cdrom
        );
        assert_eq!(d.disks.len(), 1);
        assert!(
            d.disks[0].err.is_some(),
            "no image for the extra disk: {:?}",
            d.disks
        );
        assert!(d.usb.is_empty() && d.images.is_empty());

        // No console.log is simply no output.
        let quiet = vm_cfg("quiet");
        write_vm(storage, &quiet);
        let TaskResult::Details(d) = load_details(storage, &quiet) else {
            panic!("expected Details");
        };
        assert!(d.console.is_empty());
    }

    #[test]
    fn start_vm_fails_on_a_missing_iso() {
        let h = Harness::new();
        let storage = h.mgr.storage();
        let iso = h.dir.path().join("gone.iso");
        let mut cfg = vm_cfg("noiso");
        cfg.cdrom_path = iso.to_string_lossy().into_owned();
        write_vm(storage, &cfg);
        match start_vm(storage, &cfg) {
            TaskResult::Failed { what, err } => {
                assert_eq!(what, "", "shown bare, as in Go");
                assert!(err.contains("gone.iso"), "the error names the ISO: {err}");
                assert!(err.contains("not found"), "{err}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(
            !vm::pid_path(storage, "noiso").exists(),
            "nothing was launched"
        );
    }

    #[test]
    fn stop_and_delete_texts() {
        let h = Harness::new();
        let storage = h.mgr.storage();
        write_vm(storage, &vm_cfg("idle"));
        // Stopping a stopped VM is fine, like in Go.
        match stop_vm(storage, "idle") {
            TaskResult::Done { what } => assert_eq!(what, "stopped idle"),
            other => panic!("expected Done, got {other:?}"),
        }
        match delete_vm(&h.mgr, "idle") {
            TaskResult::Done { what } => assert_eq!(what, "deleted idle"),
            other => panic!("expected Done, got {other:?}"),
        }
        assert!(!vm::vm_dir(storage, "idle").exists());
    }

    #[test]
    fn delete_template_texts() {
        let h = Harness::new();
        let storage = h.mgr.storage();
        write_template(storage, "base", 16);
        match delete_template(&h.mgr, "base") {
            TaskResult::Done { what } => assert_eq!(what, "deleted template base"),
            other => panic!("expected Done, got {other:?}"),
        }
        assert!(!vm::template_dir(storage, "base").exists());
        match delete_template(&h.mgr, "base") {
            TaskResult::Failed { what, err } => {
                assert_eq!(what, "");
                assert_eq!(err, "no template named \"base\"");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn serial_console_command_checks_and_args() {
        let h = Harness::new();
        let storage = h.mgr.storage();
        // Go's bare texts.
        assert_eq!(
            serial_console_command(storage, "x", false).unwrap_err(),
            "VM is not running"
        );
        assert!(
            SOCAT_MISSING.starts_with("socat not found — install it to connect interactively\n")
        );
        #[cfg(unix)]
        {
            let empty = fake_bin_dir(&h, &[]);
            assert_eq!(
                serial_console_command_in(storage, "x", true, Some(empty.as_os_str())).unwrap_err(),
                SOCAT_MISSING
            );
            // Raw mode without echo, so Ctrl-C and friends go to the guest;
            // Ctrl-] still disconnects.
            let bin = fake_bin_dir(&h, &["socat"]);
            let cmd = serial_console_command_in(storage, "x", true, Some(bin.as_os_str()))
                .expect("a socat command");
            assert_eq!(Path::new(cmd.get_program()), bin.join("socat"));
            let args: Vec<String> = cmd
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            assert_eq!(
                args,
                [
                    "-,raw,echo=0,escape=0x1d".to_string(),
                    format!(
                        "UNIX-CONNECT:{}",
                        vm::serial_sock_path(storage, "x").display()
                    )
                ]
            );
        }
        if which::which("socat").is_ok() {
            let cmd = serial_console_command(storage, "x", true).expect("a socat command");
            assert_eq!(Path::new(cmd.get_program()).file_name().unwrap(), "socat");
            assert_eq!(cmd.get_args().next().unwrap(), "-,raw,echo=0,escape=0x1d");
        }
    }

    /// The status-bar text of a failed task, the way the dashboard shows it.
    fn failed_notice(r: TaskResult) -> String {
        match r {
            TaskResult::Failed { what, err } => failure_text(&what, &err),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn vnc_viewer_checks_and_args() {
        let mut cfg = vm_cfg("v");
        // Go's bare texts.
        assert_eq!(
            failed_notice(launch_vnc_viewer(&cfg, true)),
            "VNC is not enabled for this VM (vnc_port: 0)"
        );
        cfg.vnc_port = 2;
        assert_eq!(
            failed_notice(launch_vnc_viewer(&cfg, false)),
            "VM is not running"
        );
        assert_eq!(
            vnc_viewer_args(VncArgStyle::HostPort, "127.0.0.1", 5902),
            ["127.0.0.1::5902"]
        );
        assert_eq!(
            vnc_viewer_args(VncArgStyle::RemminaUri, "127.0.0.1", 5902),
            ["-c", "vnc://127.0.0.1:5902"]
        );
        assert_eq!(
            vnc_viewer_args(VncArgStyle::Uri, "127.0.0.1", 5902),
            ["vnc://127.0.0.1:5902"]
        );
        let names: Vec<&str> = KNOWN_VNC_VIEWERS.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            [
                "vncviewer",
                "tigervnc",
                "xtightvncviewer",
                "remmina",
                "krdc",
                "vinagre"
            ]
        );
        assert!(NO_VNC_VIEWER.starts_with("no VNC viewer found — install one, e.g.:\n"));
    }

    #[cfg(unix)]
    #[test]
    fn vnc_viewer_launch_outcomes() {
        let h = Harness::new();
        let mut cfg = vm_cfg("v");
        cfg.vnc_port = 2;
        let empty = fake_bin_dir(&h, &[]);
        assert_eq!(
            failed_notice(launch_vnc_viewer_in(&cfg, true, Some(empty.as_os_str()))),
            NO_VNC_VIEWER
        );
        // A viewer whose interpreter is gone cannot be spawned.
        let broken =
            fake_bin_dir_with(&h, "broken", &["vncviewer"], "#!/nonexistent/interpreter\n");
        let notice = failed_notice(launch_vnc_viewer_in(&cfg, true, Some(broken.as_os_str())));
        let want = format!(
            "launch VNC viewer: {}: ",
            broken.join("vncviewer").display()
        );
        assert!(notice.starts_with(&want), "{notice}");
        let ok = fake_bin_dir(&h, &["vncviewer"]);
        match launch_vnc_viewer_in(&cfg, true, Some(ok.as_os_str())) {
            TaskResult::Done { what } => assert_eq!(what, "launched vncviewer → port 5902"),
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn vnc_viewer_search_order() {
        let h = Harness::new();
        let empty = fake_bin_dir(&h, &[]);
        assert!(find_vnc_viewer(Some(empty.as_os_str())).is_none());
        let later = fake_bin_dir(&h, &["vinagre", "remmina"]);
        let (p, style) = find_vnc_viewer(Some(later.as_os_str())).expect("a viewer");
        assert_eq!(
            p.file_name().unwrap(),
            "remmina",
            "remmina comes before vinagre"
        );
        assert_eq!(style, VncArgStyle::RemminaUri);
        let first = fake_bin_dir(&h, &["krdc", "vncviewer"]);
        let (p, style) = find_vnc_viewer(Some(first.as_os_str())).expect("a viewer");
        assert_eq!(p.file_name().unwrap(), "vncviewer");
        assert_eq!(style, VncArgStyle::HostPort);
    }

    /// The name and arguments of the clipboard tool picked from `dir`.
    #[cfg(unix)]
    fn clipboard_pick(wayland: bool, dir: &Path) -> Option<(String, Vec<&'static str>)> {
        find_clipboard_tool(wayland, Some(dir.as_os_str())).map(|(p, args)| {
            (
                p.file_name().unwrap().to_string_lossy().into_owned(),
                args.to_vec(),
            )
        })
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn clipboard_tool_order_follows_atotto() {
        let h = Harness::new();
        let pick =
            |wayland: bool, names: &[&str]| clipboard_pick(wayland, &fake_bin_dir(&h, names));
        assert_eq!(pick(true, &[]), None);
        let xclip = Some(("xclip".to_string(), vec!["-in", "-selection", "clipboard"]));
        let wl = Some(("wl-copy".to_string(), vec![]));
        assert_eq!(
            pick(false, &["xsel", "xclip"]),
            xclip,
            "xclip comes before xsel"
        );
        assert_eq!(
            pick(false, &["xsel"]),
            Some(("xsel".to_string(), vec!["--input", "--clipboard"]))
        );
        // wl-copy only in a Wayland session: an X11 or SSH shell with
        // wl-clipboard installed still copies through xclip.
        let both = ["xclip", "wl-copy", "wl-paste"];
        assert_eq!(pick(false, &both), xclip);
        assert_eq!(pick(true, &both), wl);
        assert_eq!(pick(false, &["wl-copy", "wl-paste"]), None);
        // ... and only with wl-paste next to it.
        assert_eq!(pick(true, &["xclip", "wl-copy"]), xclip);
        assert_eq!(pick(true, &["wl-copy"]), None);
        // Termux and WSL, each with its paste half.
        assert_eq!(pick(false, &["termux-clipboard-set"]), None);
        assert_eq!(
            pick(false, &["termux-clipboard-get", "termux-clipboard-set"]),
            Some(("termux-clipboard-set".to_string(), vec![]))
        );
        assert_eq!(pick(false, &["clip.exe"]), None);
        assert_eq!(
            pick(false, &["clip.exe", "powershell.exe"]),
            Some(("clip.exe".to_string(), vec![]))
        );
        // pbcopy is macOS only.
        assert_eq!(pick(true, &["pbcopy"]), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn clipboard_tool_is_pbcopy_on_macos() {
        let h = Harness::new();
        let dir = fake_bin_dir(&h, &["xclip", "wl-copy", "wl-paste", "pbcopy"]);
        assert_eq!(
            clipboard_pick(true, &dir),
            Some(("pbcopy".to_string(), vec![]))
        );
    }

    #[cfg(unix)]
    #[test]
    fn clipboard_piping() {
        let h = Harness::new();
        // The text reaches the tool's stdin whole, and a failing tool is an error.
        let out = h.dir.path().join("clip.txt");
        let mut cat = Command::new("sh");
        cat.args(["-c", "cat > \"$0\"", out.to_str().unwrap()]);
        pipe_text(&mut cat, "echo 'rule' | sudo tee\n").unwrap();
        assert_eq!(
            fs::read_to_string(&out).unwrap(),
            "echo 'rule' | sudo tee\n"
        );
        let mut failing = Command::new("sh");
        failing.args(["-c", "exit 3"]);
        let err = format!("{:#}", pipe_text(&mut failing, "x").unwrap_err());
        assert!(err.starts_with("sh: exit status: 3"), "{err}");
        // A tool that exits without reading breaks the pipe: its exit status
        // is still what is reported, and a broken pipe only when it exits 0.
        let big = "x".repeat(1 << 20);
        let mut early = Command::new("sh");
        early.args(["-c", "exec 0<&-; exit 3"]);
        let err = format!("{:#}", pipe_text(&mut early, &big).unwrap_err());
        assert!(err.starts_with("sh: exit status: 3"), "{err}");
        let mut early_ok = Command::new("sh");
        early_ok.args(["-c", "exec 0<&-; exit 0"]);
        let err = format!("{:#}", pipe_text(&mut early_ok, &big).unwrap_err());
        assert!(err.starts_with("write to sh: "), "{err}");
        let mut missing = Command::new(h.dir.path().join("no-such-tool"));
        let err = format!(
            "{:#}",
            pipe_text(&mut missing, "x")
                .context("copy to clipboard")
                .unwrap_err()
        );
        assert!(err.starts_with("copy to clipboard: run "), "{err}");
    }
}
