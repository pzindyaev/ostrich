//! The QEMU process model: building the command line, starting a detached
//! QEMU and watching it come up, stopping it, telling whether it runs, and
//! reading the serial console log.

use std::fmt::{self, Write as _};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use nix::errno::Errno;
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;

use super::bridge::{check_bridge, BRIDGE_NAME};
use super::cdrom::cdrom_args;
use super::config::{
    console_path, disk_path, firmware_vars_path, monitor_path, normalize_mac, pid_path,
    qemu_log_path, serial_sock_path, tpm_sock_path, NetworkType, VmConfig,
};
use super::disk::{check_extra_disks, extra_disk_args, validate_disks};
use super::firmware::{
    arch_of, ensure_firmware_vars, exit_status_text, find_firmware, lookup_tool, machine_of,
    spawn_retrying,
};
use super::monitor::monitor_answers;
use super::tpm::{start_tpm, stop_tpm, tpm_device};
use super::usb::{check_usb_access, usb_device_ids, usb_host_device, USB_CONTROLLER_ID};
use super::usbimage::{
    check_image, check_usb_images, usb_image_device, usb_image_drive, usb_image_drive_id,
    usb_image_ids,
};

/// The running state of a VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VmStatus {
    #[default]
    Stopped,
    Running,
}

impl fmt::Display for VmStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            VmStatus::Running => "running",
            VmStatus::Stopped => "stopped",
        })
    }
}

/// The result of a status check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProcessInfo {
    pub pid: i32,
    pub status: VmStatus,
}

impl ProcessInfo {
    /// Whether the VM is running.
    pub fn running(&self) -> bool {
        self.status == VmStatus::Running
    }
}

/// How long a freshly launched QEMU is watched before it is assumed up.
///
/// QEMU that rejects its command line or cannot open a file exits within
/// milliseconds; one that comes up has its monitor answering once its setup
/// is through and the main loop runs. Past the grace period it is assumed up.
pub const START_GRACE: Duration = Duration::from_secs(3);
/// How long one monitor probe may take.
pub const START_PROBE_TIMEOUT: Duration = Duration::from_millis(250);
/// The pause between monitor probes.
pub const START_PROBE_INTERVAL: Duration = Duration::from_millis(50);
/// How many lines of QEMU's output a start error quotes.
pub const START_ERR_LINES: usize = 8;

/// How long a terminated QEMU gets to exit before it is killed.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// The pause between liveness checks while a VM shuts down.
const STOP_POLL: Duration = Duration::from_millis(500);
/// How long a killed QEMU is given to go away.
const KILL_SETTLE: Duration = Duration::from_millis(100);

/// SLIRP (user networking) addressing. Each VM gets its own private SLIRP
/// network with a single NIC, so the built-in DHCP server always hands out
/// [`USER_NET_GUEST_IP`]. These are QEMU's defaults, passed explicitly so the
/// address shown in the UI is one we set rather than one we assume.
pub const USER_NET_CIDR: &str = "10.0.2.0/24";
pub const USER_NET_GUEST_IP: &str = "10.0.2.15";
pub const USER_NET_GATEWAY: &str = "10.0.2.2";

/// The root-bus PCI slots that a VM without a network keeps its main disk,
/// its xHCI controller and (on `virt`, where the CD-ROM hangs off one) its
/// SCSI controller in: the slots Go's command line put them in. See the
/// `NetworkType::None` arm of [`build_qemu_args`].
const NO_NIC_DISK_ADDR: &str = "0x4";
const NO_NIC_XHCI_ADDR: &str = "0x3";
const NO_NIC_SCSI_ADDR: &str = "0x2";

/// The drive ID of the main disk: the one QEMU gives the first
/// `if=virtio` drive, set explicitly where the disk is a drive plus a device.
const MAIN_DRIVE_ID: &str = "virtio0";

/// A path as a command-line word.
fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// `-flag value` as two argv words.
fn opt(flag: &str, value: impl Into<String>) -> [String; 2] {
    [flag.to_string(), value.into()]
}

/// The main disk at `disk`: an `if=virtio` drive, whose device QEMU puts in
/// the first free root-bus slot once every `-device` has its place, or, to
/// pin it in slot `addr`, a drive and a `virtio-blk-pci` device of its own
/// (QEMU no longer takes a slot on an `if=virtio` drive). The guest sees the
/// same device either way, and the drive ID is [`MAIN_DRIVE_ID`] in both.
fn main_disk_args(disk: &str, addr: Option<&str>) -> Vec<String> {
    let drive = format!("file={disk},format=qcow2");
    match addr {
        None => opt("-drive", format!("{drive},if=virtio")).to_vec(),
        Some(addr) => [
            opt("-drive", format!("{drive},if=none,id={MAIN_DRIVE_ID}")),
            opt(
                "-device",
                format!("virtio-blk-pci,drive={MAIN_DRIVE_ID},addr={addr}"),
            ),
        ]
        .concat(),
    }
}

/// The `-device` of the VM's NIC. QEMU takes a MAC in the colon form only,
/// so one in another spelling [`validate_mac`] accepts (`52-54-00-12-34-56`,
/// `5254.0012.3456`) is passed as `52:54:00:12:34:56`. An empty one is left
/// out and QEMU picks its default; anything else goes as it is, for QEMU to
/// refuse by name.
///
/// [`validate_mac`]: super::config::validate_mac
fn nic_device(mac: &str) -> String {
    let mut dev = "virtio-net-pci,netdev=net0".to_string();
    if !mac.is_empty() {
        let _ = write!(dev, ",mac={}", normalize_mac(mac).as_deref().unwrap_or(mac));
    }
    dev
}

/// Whether KVM can run a guest of `arch` here: `/dev/kvm` exists and the
/// guest is the host's architecture (or 32-bit x86 on an x86_64 host). Go
/// added `-enable-kvm` whenever `/dev/kvm` existed, which QEMU refuses for
/// an aarch64 guest on an x86_64 host.
fn kvm_runs(arch: &str) -> bool {
    kvm_arch_matches(std::env::consts::ARCH, arch) && Path::new("/dev/kvm").exists()
}

/// Whether a `host` CPU's KVM runs a `guest` of that architecture.
fn kvm_arch_matches(host: &str, guest: &str) -> bool {
    let guest = if guest == "arm64" { "aarch64" } else { guest };
    guest == host || (host == "x86_64" && guest == "i386")
}

/// The QEMU binary name (`qemu-system-<arch>`) and argument list for a VM.
/// Fails when the VM wants UEFI firmware and none is installed, or the
/// disks are invalid.
///
/// The only host lookups are a `stat` of `/dev/kvm` and the firmware search.
pub fn build_qemu_args(cfg: &VmConfig, storage: &Path) -> Result<(String, Vec<String>)> {
    let arch = arch_of(cfg);
    let bin = format!("qemu-system-{arch}");
    validate_disks(&cfg.disks)?;

    let name = cfg.name.as_str();
    let disk = path_str(&disk_path(storage, name));
    let console = path_str(&console_path(storage, name));
    let serial_sock = path_str(&serial_sock_path(storage, name));
    let monitor = path_str(&monitor_path(storage, name));
    let pidfile = path_str(&pid_path(storage, name));

    let machine = machine_of(cfg);
    let mut machine_opts = machine.to_string();
    // A VM without a network keeps its devices in the slots Go gave them;
    // see the `NetworkType::None` arm below.
    let no_nic = cfg.network.kind == NetworkType::None;

    // UEFI: the firmware code and the VM's own NVRAM copy sit on two pflash
    // units. Secure Boot builds keep the variable store behind SMM, so SMM
    // must be on and flash writes restricted to it.
    let mut firmware_args = Vec::new();
    if cfg.uefi() {
        let fw = find_firmware(arch, machine, cfg.secure_boot)?;
        if fw.requires_smm {
            machine_opts.push_str(",smm=on");
            firmware_args.extend(opt(
                "-global",
                "driver=cfi.pflash01,property=secure,value=on",
            ));
        }
        firmware_args.extend(opt(
            "-drive",
            format!(
                "if=pflash,format={},unit=0,readonly=on,file={}",
                fw.code_format, fw.code
            ),
        ));
        firmware_args.extend(opt(
            "-drive",
            format!(
                "if=pflash,format={},unit=1,file={}",
                fw.vars_format,
                path_str(&firmware_vars_path(storage, name))
            ),
        ));
    }

    // Serial console: Unix socket (for interactive access) + logfile (for
    // the passive log view). Connect interactively with:
    // socat -,raw,echo=0,escape=0x1d UNIX-CONNECT:<serial.sock>
    let serial_chardev =
        format!("socket,id=serial0,path={serial_sock},server=on,wait=off,logfile={console}");

    let mut args = Vec::new();
    args.extend(opt("-name", name));
    args.extend(opt("-m", format!("{}M", cfg.ram)));
    args.extend(opt("-smp", cfg.cpu.to_string()));
    args.extend(opt("-machine", machine_opts));
    args.extend(firmware_args);
    args.extend(main_disk_args(&disk, no_nic.then_some(NO_NIC_DISK_ADDR)));
    // Additional disks, each on its own PCIe root port; the ports are always
    // there so a disk can be hot-plugged into a running VM.
    args.extend(extra_disk_args(cfg, storage));
    args.extend(opt("-chardev", serial_chardev));
    args.extend(opt("-serial", "chardev:serial0"));
    args.extend(opt("-monitor", format!("unix:{monitor},server,nowait")));
    args.extend(opt("-pidfile", pidfile));
    args.extend(opt("-display", "none"));

    // KVM acceleration when available and the guest is the host's kind.
    if kvm_runs(arch) {
        args.push("-enable-kvm".to_string());
        args.extend(opt("-cpu", "host"));
    }

    // Boot media. The CD-ROM drive is always there, empty without an ISO, so
    // one can be put in while the VM runs. The boot order only steers
    // SeaBIOS; OVMF boots the disk once an OS is installed there and tries
    // the CD before that.
    args.extend(cdrom_args(
        machine,
        &cfg.cdrom_path,
        no_nic.then_some(NO_NIC_SCSI_ADDR),
    ));
    if !cfg.cdrom_path.is_empty() {
        args.extend(opt("-boot", "order=dc"));
    }

    // TPM 2.0, backed by the swtpm daemon started alongside QEMU.
    if cfg.tpm {
        args.extend(opt(
            "-chardev",
            format!(
                "socket,id=chrtpm,path={}",
                path_str(&tpm_sock_path(storage, name))
            ),
        ));
        args.extend(opt("-tpmdev", "emulator,id=tpm0,chardev=chrtpm"));
        args.extend(opt(
            "-device",
            format!("{},tpmdev=tpm0", tpm_device(machine)),
        ));
    }

    // VNC display (TCP, localhost-only).
    if cfg.vnc_port > 0 {
        args.extend(opt("-vnc", format!("127.0.0.1:{}", cfg.vnc_port)));
    }

    // USB: an xHCI controller is always present so host devices can be
    // hot-plugged into a running VM; configured devices are attached at boot.
    // A device that is not connected yet is picked up by QEMU when plugged in.
    let mut xhci = format!("qemu-xhci,id={USB_CONTROLLER_ID}");
    if no_nic {
        let _ = write!(xhci, ",addr={NO_NIC_XHCI_ADDR}");
    }
    args.extend(opt("-device", xhci));
    for (dev, id) in cfg.usb_devices.iter().zip(usb_device_ids(&cfg.usb_devices)) {
        args.extend(opt("-device", usb_host_device(dev, &id)));
    }
    // Disk images attached as USB sticks share that bus; each is a drive plus
    // a usb-storage device on top of it.
    for (img, id) in cfg.usb_images.iter().zip(usb_image_ids(&cfg.usb_images)) {
        let drive_id = usb_image_drive_id(&id);
        args.extend(opt("-drive", usb_image_drive(img, &drive_id)));
        args.extend(opt("-device", usb_image_device(&id, &drive_id)));
    }

    // Networking.
    match cfg.network.kind {
        NetworkType::User => {
            let mut netdev =
                format!("user,id=net0,net={USER_NET_CIDR},dhcpstart={USER_NET_GUEST_IP}");
            for pf in &cfg.network.port_forwards {
                let _ = write!(netdev, ",hostfwd={}::{}-:{}", pf.proto(), pf.host, pf.guest);
            }
            args.extend(opt("-netdev", netdev));
            args.extend(opt("-device", nic_device(&cfg.network.mac)));
        }
        NetworkType::Tap => {
            // The bridge backend creates the tap through the setuid
            // qemu-bridge-helper, so no root is needed (plain "tap" would open
            // /dev/net/tun itself).
            args.extend(opt("-netdev", format!("bridge,id=net0,br={BRIDGE_NAME}")));
            args.extend(opt("-device", nic_device(&cfg.network.mac)));
        }
        // No NIC: `-nic none`, or QEMU would add its default one. Go passed
        // no network arguments here, so that default NIC took the first free
        // root-bus slot (00:02.0 on q35, 00:01.0 on virt) and pushed every
        // automatically placed device after it one slot down: the xHCI to
        // 00:03.0, the main disk to 00:04.0 and, on virt, the CD-ROM's SCSI
        // controller to 00:02.0. A UEFI guest installed back then keeps boot
        // entries naming those paths (`Pci(0x4,0x0)` for the disk), so with
        // `no_nic` above those devices are pinned where they were. User and
        // tap VMs, whose NIC comes last, keep the layout they always had.
        NetworkType::None => args.extend(opt("-nic", "none")),
    }

    Ok((bin, args))
}

/// Makes `s` safe as a value in a QEMU option string, where a comma is the
/// separator and is written as `,,`.
pub(crate) fn qemu_opt_escape(s: &str) -> String {
    s.replace(',', ",,")
}

/// Launches QEMU for the VM, detached (its own session) so it survives the
/// TUI's exit, with its output in `qemu.log`, and waits for it to either die
/// or come up. It starts the `cfg` it is given; `vm.yaml` is not re-read.
///
/// Pre-flight checks, in order: the VM is not running already
/// (`VM "<name>" is already running (PID <pid>)`), every USB device is
/// valid and accessible, the boot ISO is there (`boot ISO: <error>` plus a
/// hint line), the USB images and additional disks are there, the bridge is
/// ready for a tap VM (the bridge hint verbatim), the UEFI NVRAM exists, the
/// command line builds, and the QEMU binary is on `$PATH`
/// (`"<bin>" not found in PATH — is QEMU installed?`). Then the TPM
/// emulator is started if the VM has one, and stopped again on every later
/// failure. A QEMU that dies during startup is reported by
/// `start_failure`.
pub fn start(storage: &Path, cfg: &VmConfig) -> Result<()> {
    start_with(storage, cfg, &lookup_tool)
}

/// [`start`] with the QEMU binary resolved by `find_qemu` instead of a
/// `$PATH` search (tests point it at a stand-in); `None` means not found.
pub(crate) fn start_with(
    storage: &Path,
    cfg: &VmConfig,
    find_qemu: &dyn Fn(&str) -> Option<PathBuf>,
) -> Result<()> {
    if let Ok(info) = status(storage, &cfg.name) {
        if info.running() {
            bail!("VM {:?} is already running (PID {})", cfg.name, info.pid);
        }
    }

    for d in &cfg.usb_devices {
        d.validate()?;
    }
    // QEMU's own failure to open a USB device is only a warning in its log
    // and the VM would run with the device missing, so check up front.
    check_usb_access(&cfg.usb_devices)?;
    // Likewise QEMU would not start with an image file missing.
    if !cfg.cdrom_path.is_empty() {
        if let Err(e) = check_image(&cfg.cdrom_path) {
            bail!("boot ISO: {e:#}\nEject it in the ISO dialog (i), clear it in the edit form (e), or put the file back.");
        }
    }
    check_usb_images(&cfg.usb_images)?;
    check_extra_disks(storage, cfg)?;
    // The bridge helper's "access denied by acl file" names neither the
    // bridge nor the fix; this does.
    if cfg.network.kind == NetworkType::Tap {
        check_bridge(BRIDGE_NAME)?;
    }

    // A VM whose vm.yaml was switched to UEFI by hand has no NVRAM yet.
    ensure_firmware_vars(storage, cfg)?;
    let (bin, args) = build_qemu_args(cfg, storage)?;
    let Some(exe) = find_qemu(&bin) else {
        bail!("{bin:?} not found in PATH — is QEMU installed?");
    };

    if cfg.tpm {
        start_tpm(storage, &cfg.name)?;
    }
    launch(storage, cfg, &exe, &bin, &args).inspect_err(|_| stop_tpm(storage, &cfg.name))
}

/// Spawns QEMU and watches it come up; the part of [`start_with`] after
/// which the TPM emulator has to be stopped again on failure.
fn launch(storage: &Path, cfg: &VmConfig, exe: &Path, bin: &str, args: &[String]) -> Result<()> {
    let dev_null = File::open("/dev/null").context("open /dev/null")?;
    // QEMU's own output goes to a file in the VM directory: it is what tells
    // why a start failed, and it has to outlive Ostrich (a pipe would not).
    let log_path = qemu_log_path(storage, &cfg.name);
    let log = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .open(&log_path)
        .map_err(|e| anyhow!("create QEMU log: open {}: {e}", log_path.display()))?;
    let log_err = log
        .try_clone()
        .map_err(|e| anyhow!("create QEMU log: dup {}: {e}", log_path.display()))?;

    let mut cmd = Command::new(exe);
    cmd.arg0(bin)
        .args(args)
        .stdin(Stdio::from(dev_null))
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    // Detach from the terminal session, so QEMU survives the TUI's exit and
    // never gets its SIGHUP or job-control signals.
    // SAFETY: setsid is async-signal-safe and touches no state of this
    // process; it only runs in the child between fork and exec.
    unsafe {
        cmd.pre_exec(|| nix::unistd::setsid().map(drop).map_err(io::Error::from));
    }
    let mut child = spawn_retrying(&mut cmd).map_err(|e| anyhow!("start QEMU: {e}"))?;

    // Write the PID immediately (QEMU also writes it via -pidfile after
    // forking, but we write it now as a fallback).
    let pid = child.id() as i32;
    let pid_file = pid_path(storage, &cfg.name);
    if let Err(e) = write_pid(&pid_file, pid) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(anyhow!("write PID file: open {}: {e}", pid_file.display()));
    }

    // QEMU runs on its own (its own session, no pipes to us), but it stays
    // our child: reap it when it exits, or it would linger as a zombie that
    // still answers signals and so would look like a running VM. Should
    // Ostrich exit first, init takes over the reaping.
    let exited = match reap_child(
        child,
        thread::Builder::new().name("ostrich-qemu-reaper".into()),
    ) {
        Ok(exited) => exited,
        Err(e) => {
            cleanup_pid(storage, &cfg.name);
            return Err(anyhow!("start QEMU: spawn reaper thread: {e}"));
        }
    };

    // A QEMU that will not start is gone within moments, so wait for it to
    // either die or come up rather than call the launch a success.
    if let Err(e) = await_startup(
        &exited,
        &monitor_path(storage, &cfg.name),
        &log_path,
        START_GRACE,
    ) {
        cleanup_pid(storage, &cfg.name);
        return Err(e);
    }
    Ok(())
}

/// Waits for `child` on a thread of `builder`'s and delivers its exit
/// status on the returned channel. Should the system refuse the thread (a
/// pids limit), the child is killed and reaped here instead, and the error
/// returned: a QEMU nothing waits for would turn into a zombie that still
/// looks like a running VM.
fn reap_child(
    mut child: std::process::Child,
    builder: thread::Builder,
) -> io::Result<Receiver<io::Result<ExitStatus>>> {
    let pid = Pid::from_raw(child.id() as i32);
    let (exited_tx, exited) = mpsc::channel();
    match builder.spawn(move || {
        let _ = exited_tx.send(child.wait());
    }) {
        Ok(_) => Ok(exited),
        Err(e) => {
            // The refused closure took the Child with it (dropping a Child
            // neither kills nor reaps it), so the PID is what is left; it
            // stays ours until it is reaped.
            let _ = kill(pid, Signal::SIGKILL);
            let _ = nix::sys::wait::waitpid(pid, None);
            Err(e)
        }
    }
}

/// Writes `qemu.pid`: the decimal PID, no newline, mode 0644.
fn write_pid(path: &Path, pid: i32) -> io::Result<()> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .open(path)?
        .write_all(pid.to_string().as_bytes())
}

/// Waits for a just-launched QEMU to show whether it made it: `exited`
/// delivers its exit status should it die, and its monitor answers once it
/// is up. After `grace` it is taken to be up; should it die later, its log
/// still has the reason.
///
/// The order per round is: exit check, deadline, monitor probe, a short
/// sleep. A probe may take up to [`START_PROBE_TIMEOUT`], so the return can
/// overshoot `grace` by that much.
pub(crate) fn await_startup(
    exited: &Receiver<io::Result<ExitStatus>>,
    monitor_sock: &Path,
    log_path: &Path,
    grace: Duration,
) -> Result<()> {
    let deadline = Instant::now() + grace;
    loop {
        match exited.try_recv() {
            Ok(result) => return Err(start_failure(result.ok(), log_path)),
            // A reaper that went away without a word says nothing about QEMU.
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => {}
        }
        if Instant::now() > deadline {
            return Ok(());
        }
        if monitor_answers(monitor_sock, START_PROBE_TIMEOUT) {
            return Ok(());
        }
        thread::sleep(START_PROBE_INTERVAL);
    }
}

/// Describes a QEMU that exited during startup, quoting the last
/// [`START_ERR_LINES`] lines of its log:
///
/// ```text
/// QEMU exited during startup (exit status 1):
///   access denied by acl file
///   qemu-system-x86_64: -netdev bridge,id=net0,br=br0: bridge helper failed
/// ```
///
/// A failed exit is named in Go's words (`exit status 1`, `signal: killed`);
/// a clean exit or an unknown status (`None`) gets no parenthesis. A longer
/// log is cut to its last lines and ends with `(full output in <log>)`; an
/// empty one gives `<how> without any output`. The TUI shows this text.
pub(crate) fn start_failure(exit: Option<ExitStatus>, log_path: &Path) -> anyhow::Error {
    let mut how = String::from("QEMU exited during startup");
    if let Some(status) = exit.filter(|s| !s.success()) {
        let _ = write!(how, " ({})", exit_status_text(status));
    }
    let mut lines = read_tail(log_path, START_ERR_LINES + 1).unwrap_or_default();
    if lines.is_empty() {
        return anyhow!("{how} without any output");
    }
    let mut msg = format!("{how}:");
    let truncated = lines.len() > START_ERR_LINES;
    if truncated {
        lines.remove(0);
    }
    for l in &lines {
        msg.push_str("\n  ");
        msg.push_str(l);
    }
    if truncated {
        let _ = write!(msg, "\n  (full output in {})", log_path.display());
    }
    anyhow!("{msg}")
}

/// Sends SIGTERM to the VM process and waits up to 5 s before SIGKILL. The
/// TPM emulator, if any, goes with it. Never fails on a VM that is not
/// running, and never fails at all: it does what it can and removes
/// `qemu.pid`.
pub fn stop(storage: &Path, name: &str) -> Result<()> {
    terminate(storage, name);
    // Whatever QEMU's state, the emulator and its socket go away too.
    stop_tpm(storage, name);
    Ok(())
}

/// The QEMU half of [`stop`].
fn terminate(storage: &Path, name: &str) {
    let Ok(info) = status(storage, name) else {
        return;
    };
    if !info.running() {
        return;
    }
    let pid = Pid::from_raw(info.pid);
    if kill(pid, Signal::SIGTERM).is_err() {
        cleanup_pid(storage, name);
        return;
    }
    let deadline = Instant::now() + STOP_TIMEOUT;
    while Instant::now() < deadline {
        if !process_alive(info.pid) {
            break; // process is gone
        }
        thread::sleep(STOP_POLL);
    }
    // Force-kill if still alive.
    if process_alive(info.pid) {
        let _ = kill(pid, Signal::SIGKILL);
        thread::sleep(KILL_SETTLE);
    }
    cleanup_pid(storage, name);
}

/// Whether `pid` names a live, non-zombie process. A zombie, one that has
/// exited but whose parent has not reaped it yet, still answers a null
/// signal, so on Linux the state in `/proc` is checked as well. A null
/// signal refused for lack of permission counts as not alive, as in Go.
///
/// Unlike the Go version this refuses `pid <= 0` outright: `kill(0, 0)`
/// would address our own process group and succeed, so a `qemu.pid` holding
/// `0` read as a running VM whose [`stop`] would have signalled Ostrich
/// itself.
pub(crate) fn process_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    kill(Pid::from_raw(pid), None).is_ok() && !is_zombie(pid)
}

/// Whether `/proc/<pid>/stat` says the process is a zombie; false without
/// procfs (macOS), where a process answering signals counts as alive.
pub(crate) fn is_zombie(pid: i32) -> bool {
    let Ok(data) = fs::read(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // "pid (comm) S ...": comm may hold spaces and parentheses, so the state
    // is the field after the last ')'.
    let Some(i) = data.iter().rposition(|&b| b == b')') else {
        return false;
    };
    i + 2 < data.len() && data[i + 2] == b'Z'
}

/// What the program of a QEMU process is called: `qemu-system-<arch>`.
const QEMU_PROGRAM: &str = "qemu";

/// Shells that run a script named as their first argument: a program that
/// is a script shows up in `/proc` as `<shell> <script> <args>`.
const SCRIPT_SHELLS: [&str; 8] = ["sh", "bash", "dash", "ash", "zsh", "ksh", "mksh", "busybox"];

/// Whether a command line in `/proc/<pid>/cmdline` form (the argv words,
/// each NUL-terminated) runs a program whose file name starts with
/// `program`: the base name of `argv[0]`, or of `argv[1]` when `argv[0]` is
/// a shell running a script.
fn cmdline_runs(cmdline: &[u8], program: &str) -> bool {
    fn base(arg: &[u8]) -> &[u8] {
        arg.rsplit(|&b| b == b'/').next().unwrap_or(arg)
    }
    let mut args = cmdline.split(|&b| b == 0);
    let Some(argv0) = args.next().map(base) else {
        return false;
    };
    argv0.starts_with(program.as_bytes())
        || (SCRIPT_SHELLS.iter().any(|sh| argv0 == sh.as_bytes())
            && args
                .next()
                .is_some_and(|script| base(script).starts_with(program.as_bytes())))
}

/// Whether the live process `pid` runs `program` (see [`cmdline_runs`]). A
/// PID file outlives a host crash or reboot, and its PID may since belong to
/// some unrelated process, which must neither count as the VM's QEMU or
/// swtpm nor be signalled as one. Without procfs (macOS) nothing can be
/// told and any process passes; one whose command line cannot be read, gone
/// in the meantime, does not.
///
/// An empty command line is a kernel thread, which does not pass, or a
/// process in the middle of an exec or of exiting, which does: until its
/// exec is through, a process just spawned (QEMU, right after the start)
/// shows no command line yet.
pub(crate) fn pid_runs(pid: i32, program: &str) -> bool {
    match fs::read(format!("/proc/{pid}/cmdline")) {
        Ok(cmdline) if cmdline.is_empty() => !is_kernel_thread(pid),
        Ok(cmdline) => cmdline_runs(&cmdline, program),
        Err(_) => !Path::new("/proc/self/cmdline").exists(),
    }
}

/// Whether `/proc/<pid>/stat` flags the process as a kernel thread
/// (`PF_KTHREAD`); false without procfs.
fn is_kernel_thread(pid: i32) -> bool {
    const PF_KTHREAD: u64 = 0x0020_0000;
    let Ok(data) = fs::read(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // "pid (comm) state ppid pgrp session tty_nr tpgid flags ...", with
    // anything in comm, so the fields are counted from the last ')'.
    let Some(i) = data.iter().rposition(|&b| b == b')') else {
        return false;
    };
    String::from_utf8_lossy(&data[i + 1..])
        .split_ascii_whitespace()
        .nth(6)
        .and_then(|flags| flags.parse::<u64>().ok())
        .is_some_and(|flags| flags & PF_KTHREAD != 0)
}

/// Reads `qemu.pid` and checks whether that process is alive and is a QEMU;
/// a stale or malformed PID file, or one whose PID another program has
/// taken over, is removed and counts as stopped. Only a PID file that
/// exists but cannot be read is an error.
pub fn status(storage: &Path, name: &str) -> Result<ProcessInfo> {
    let path = pid_path(storage, name);
    let data = match fs::read(&path) {
        Ok(data) => data,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(ProcessInfo::default()),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    // Ostrich writes "<pid>", QEMU's -pidfile rewrites it as "<pid>\n".
    let Ok(pid) = String::from_utf8_lossy(&data).trim().parse::<i32>() else {
        cleanup_pid(storage, name);
        return Ok(ProcessInfo::default());
    };
    if !process_alive(pid) || !pid_runs(pid, QEMU_PROGRAM) {
        cleanup_pid(storage, name);
        return Ok(ProcessInfo::default());
    }
    Ok(ProcessInfo {
        pid,
        status: VmStatus::Running,
    })
}

/// The last `max_lines` lines of the serial console log; none when there is
/// no log yet.
pub fn read_console_tail(storage: &Path, name: &str, max_lines: usize) -> Result<Vec<String>> {
    read_tail(&console_path(storage, name), max_lines)
}

/// The most of one line [`read_tail`] keeps, in bytes: the line limit of
/// Go's `bufio.Scanner`.
const TAIL_LINE_MAX: usize = 64 * 1024;
/// How much [`read_tail`] reads at a time, going backwards from the end.
const TAIL_BLOCK: usize = 64 * 1024;

/// The last `max_lines` lines of a log file; none when there is no such
/// file. Lines are split at `\n` with a `\r` before it dropped, like Go's
/// `bufio.Scanner`; a final newline does not make an empty last line, and
/// bytes that are not UTF-8 are replaced rather than passed through.
///
/// The file is read backwards from its end, a block at a time, only as far
/// as those lines reach, so a log of gigabytes costs no more than its tail.
/// A line longer than [`TAIL_LINE_MAX`] keeps its last that many bytes (Go's
/// scanner gave up at such a line, and its TUI kept only the lines before
/// it), and at most `max_lines` lines of [`TAIL_LINE_MAX`] bytes are read,
/// with their newlines and the one before the oldest: the line that reaches
/// back past that is cut there.
pub(crate) fn read_tail(path: &Path, max_lines: usize) -> Result<Vec<String>> {
    let context = || format!("read {}", path.display());
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(context),
    };
    let meta = file.metadata().with_context(context)?;
    if meta.is_dir() {
        return Err(io::Error::from(Errno::EISDIR)).with_context(context);
    }
    tail_lines(&file, meta.len(), max_lines).with_context(context)
}

/// [`read_tail`] on an open file of `len` bytes.
fn tail_lines(file: &File, len: u64, max_lines: usize) -> io::Result<Vec<String>> {
    let mut lines = Vec::new(); // the newest first
    if max_lines == 0 || len == 0 {
        return Ok(lines);
    }
    // Each line with its newline, and the newline that ends the line before
    // them, so lines within the cap come back whole.
    let budget = u64::try_from(max_lines)
        .unwrap_or(u64::MAX)
        .saturating_mul(TAIL_LINE_MAX as u64 + 1)
        .saturating_add(1);
    let stop = len.saturating_sub(budget);
    let mut line = LineTail::default();
    let mut buf = vec![0u8; TAIL_BLOCK];
    let mut pos = len;
    while pos > stop {
        let start = pos.saturating_sub(TAIL_BLOCK as u64).max(stop);
        // At most TAIL_BLOCK, so the cast cannot truncate.
        let block = &mut buf[..(pos - start) as usize];
        file.read_exact_at(block, start)?;
        let mut end = block.len();
        while let Some(i) = block[..end].iter().rposition(|&b| b == b'\n') {
            line.prepend(&block[i + 1..end]);
            // The newline that ends the file ends the last line rather than
            // starting an empty one after it.
            if start + i as u64 + 1 < len {
                lines.push(line.take());
                if lines.len() == max_lines {
                    lines.reverse();
                    return Ok(lines);
                }
            }
            end = i;
        }
        line.prepend(&block[..end]);
        pos = start;
    }
    // What is left began at the start of the file, or before the part read.
    if stop == 0 {
        lines.push(line.take());
    } else if !line.is_empty() {
        line.cut = true;
        lines.push(line.take());
    }
    lines.reverse();
    Ok(lines)
}

/// The end of a line that is read backwards: its pieces, the last first,
/// at most [`TAIL_LINE_MAX`] bytes in all.
#[derive(Default)]
struct LineTail {
    pieces: Vec<Vec<u8>>,
    len: usize,
    /// Whether the line has more in front of what is held.
    cut: bool,
}

impl LineTail {
    fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Puts `bytes`, which come right before what is held, in front of it,
    /// as far as the limit allows.
    fn prepend(&mut self, bytes: &[u8]) {
        let take = bytes.len().min(TAIL_LINE_MAX - self.len);
        if take > 0 {
            self.pieces.push(bytes[bytes.len() - take..].to_vec());
            self.len += take;
        }
        self.cut |= take < bytes.len();
    }

    /// The line without its final `\r`, as text; the holder starts over.
    fn take(&mut self) -> String {
        let mut line = Vec::with_capacity(self.len);
        for piece in self.pieces.drain(..).rev() {
            line.extend_from_slice(&piece);
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        // A cut line may start inside a UTF-8 sequence; its leftover
        // continuation bytes would only turn into replacement characters.
        let skip = if self.cut {
            line.iter()
                .take(3)
                .take_while(|&&b| b & 0xC0 == 0x80)
                .count()
        } else {
            0
        };
        self.len = 0;
        self.cut = false;
        String::from_utf8_lossy(&line[skip..]).into_owned()
    }
}

/// Removes `qemu.pid`.
pub(crate) fn cleanup_pid(storage: &Path, name: &str) {
    let _ = fs::remove_file(pid_path(storage, name));
}

/// A stand-in running QEMU for the tests that need a VM to look running.
#[cfg(test)]
pub(crate) mod testutil {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::process::{Child, Command};
    use std::thread;
    use std::time::{Duration, Instant};

    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    use tempfile::TempDir;

    use crate::vm::config::pid_path;
    use crate::vm::firmware::spawn_retrying;

    /// A script named `qemu-system-x86_64` that idles until SIGTERM, so
    /// `/proc` shows a QEMU (the test process itself is none); stopped and
    /// reaped when dropped.
    pub(crate) struct FakeQemu {
        child: Child,
        _dir: TempDir,
    }

    impl FakeQemu {
        pub(crate) fn spawn() -> FakeQemu {
            let dir = tempfile::tempdir().unwrap();
            let exe = dir.path().join("qemu-system-x86_64");
            fs::write(
                &exe,
                "#!/bin/sh\ntrap 'kill $! 2>/dev/null; exit 0' TERM\nsleep 60 &\nwait\n",
            )
            .unwrap();
            fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
            let child = spawn_retrying(&mut Command::new(&exe)).unwrap();
            // Until its exec is through it shows no command line.
            let deadline = Instant::now() + Duration::from_secs(5);
            let cmdline = format!("/proc/{}/cmdline", child.id());
            while fs::read(&cmdline).is_ok_and(|c| c.is_empty()) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(1));
            }
            FakeQemu { child, _dir: dir }
        }

        pub(crate) fn pid(&self) -> i32 {
            self.child.id() as i32
        }

        /// Writes its PID as the `qemu.pid` of VM `name`.
        pub(crate) fn run_as(&self, storage: &Path, name: &str) {
            fs::write(pid_path(storage, name), self.pid().to_string()).unwrap();
        }
    }

    impl Drop for FakeQemu {
        fn drop(&mut self) {
            let _ = kill(Pid::from_raw(self.pid()), Signal::SIGTERM);
            let _ = self.child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    use nix::errno::Errno;
    use tempfile::TempDir;

    use super::testutil::FakeQemu;
    use super::*;
    use crate::vm::config::{vm_dir, FirmwareType, NetworkConfig, PortForward};
    use crate::vm::disk::Disk;
    use crate::vm::manager::Manager;
    use crate::vm::monitor::monitor_command;
    use crate::vm::monitor::testutil::fake_hmp_sessions;
    use crate::vm::usb::UsbDevice;
    use crate::vm::usbimage::testutil::{wait_for_monitor, write_image, StopOnDrop};
    use crate::vm::usbimage::UsbImage;

    /// A scratch directory whose socket paths fit `sun_path`.
    fn storage() -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        if dir.path().as_os_str().len() < 60 {
            dir
        } else {
            tempfile::tempdir_in("/tmp").unwrap()
        }
    }

    /// A small VM without a network, the shape every integration test uses.
    fn vm(name: &str) -> VmConfig {
        VmConfig {
            name: name.to_string(),
            cpu: 1,
            ram: 128,
            disk_size: 1,
            network: NetworkConfig {
                kind: NetworkType::None,
                ..NetworkConfig::default()
            },
            ..VmConfig::default()
        }
    }

    /// Whether the real QEMU tests can run here; says so when they cannot.
    fn have_qemu() -> bool {
        for bin in ["qemu-system-x86_64", "qemu-img"] {
            if which::which(bin).is_err() {
                eprintln!("skipping: {bin} not installed");
                return false;
            }
        }
        true
    }

    /// The status of a child that exited with `code` (Go: `exitedState`).
    fn exited_status(code: i32) -> ExitStatus {
        let status = Command::new("sh")
            .arg("-c")
            .arg(format!("exit {code}"))
            .status()
            .unwrap();
        assert_eq!(
            status.code(),
            Some(code),
            "could not produce exit status {code}"
        );
        status
    }

    /// The body of a stand-in program that idles until SIGTERM. It stays the
    /// shell running its script, so `/proc` shows `sh <dir>/<program>` (an
    /// `exec sleep` would turn it into a `sleep`, which is no QEMU), and on
    /// SIGTERM it takes its sleeping child along.
    const IDLE_UNTIL_TERM: &str = "trap 'kill $! 2>/dev/null; exit 0' TERM\nsleep 60 &\nwait";

    /// Waits until the process just spawned as `pid` is through its exec
    /// and shows its command line, or gives up after 5 s.
    fn wait_for_exec(pid: i32) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| c.is_empty())
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// A stand-in `qemu-system-x86_64`: a shell script with `body`, in `dir`.
    fn fake_qemu(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("qemu-system-x86_64");
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// `start` with the stand-in at `exe` as the only QEMU in sight.
    fn start_fake(storage: &Path, cfg: &VmConfig, exe: &Path) -> Result<()> {
        let exe = exe.to_path_buf();
        start_with(storage, cfg, &move |bin| {
            assert_eq!(bin, "qemu-system-x86_64");
            Some(exe.clone())
        })
    }

    /// Polls until `pid` is gone for good (ESRCH), or gives up after 5 s.
    fn wait_reaped(pid: i32) -> nix::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let r = kill(Pid::from_raw(pid), None);
            if r == Err(Errno::ESRCH) || Instant::now() >= deadline {
                return r;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn vm_status_display() {
        assert_eq!(VmStatus::Running.to_string(), "running");
        assert_eq!(VmStatus::Stopped.to_string(), "stopped");
        assert_eq!(VmStatus::default(), VmStatus::Stopped);
        let info = ProcessInfo::default();
        assert!(!info.running() && info.pid == 0);
    }

    #[test]
    fn qemu_opt_escape_doubles_commas() {
        assert_eq!(qemu_opt_escape("/isos/a,b,,c.iso"), "/isos/a,,b,,,,c.iso");
        assert_eq!(qemu_opt_escape("plain"), "plain");
        assert_eq!(qemu_opt_escape(""), "");
    }

    /// A child that has exited but not been reaped still answers a null
    /// signal, and must not count as a running VM (Go: TestStatusIgnoresZombie).
    #[test]
    fn status_ignores_zombie() {
        if !Path::new("/proc/self/stat").exists() {
            eprintln!("skipping: zombie detection reads /proc");
            return;
        }
        let mut child = Command::new("true").spawn().unwrap();
        let pid = child.id() as i32;
        // It exits at once; without a wait it stays a zombie.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !is_zombie(pid) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(is_zombie(pid), "child did not become a zombie");
        assert!(
            kill(Pid::from_raw(pid), None).is_ok(),
            "a zombie should still answer kill -0"
        );
        assert!(!process_alive(pid), "process_alive(zombie) = true");

        let storage = tempfile::tempdir().unwrap();
        let storage = storage.path();
        fs::create_dir_all(vm_dir(storage, "z")).unwrap();
        fs::write(pid_path(storage, "z"), pid.to_string()).unwrap();
        let info = status(storage, "z").unwrap();
        assert_eq!(
            info.status,
            VmStatus::Stopped,
            "Status = {info:?}; want stopped"
        );
        assert!(
            !pid_path(storage, "z").exists(),
            "stale pid file not cleaned up"
        );

        // A live QEMU still counts as running.
        let qemu = FakeQemu::spawn();
        fs::write(pid_path(storage, "z"), format!("{}\n", qemu.pid())).unwrap();
        let info = status(storage, "z").unwrap();
        assert_eq!(
            info,
            ProcessInfo {
                pid: qemu.pid(),
                status: VmStatus::Running
            },
            "Status of a live process"
        );
        let _ = child.wait();
    }

    #[test]
    fn status_handles_missing_and_malformed_pid_files() {
        let storage = tempfile::tempdir().unwrap();
        let storage = storage.path();
        assert_eq!(status(storage, "nope").unwrap(), ProcessInfo::default());

        fs::create_dir_all(vm_dir(storage, "v")).unwrap();
        for garbage in ["", "abc", "12x", "0", "-1", "99999999999"] {
            fs::write(pid_path(storage, "v"), garbage).unwrap();
            assert_eq!(
                status(storage, "v").unwrap(),
                ProcessInfo::default(),
                "{garbage:?}"
            );
            assert!(
                !pid_path(storage, "v").exists(),
                "{garbage:?} not cleaned up"
            );
        }
        // A plus sign and surrounding whitespace are tolerated, as by Atoi + TrimSpace.
        let qemu = FakeQemu::spawn();
        fs::write(pid_path(storage, "v"), format!("  +{}\n", qemu.pid())).unwrap();
        assert_eq!(status(storage, "v").unwrap().pid, qemu.pid());

        // A PID file that is there but unreadable is an error, not "stopped".
        fs::remove_file(pid_path(storage, "v")).unwrap();
        fs::create_dir(pid_path(storage, "v")).unwrap();
        let err = status(storage, "v").unwrap_err().to_string();
        assert!(err.contains("qemu.pid"), "{err}");
    }

    #[test]
    fn cmdline_runs_names_the_program_or_the_script() {
        let qemu: [&[u8]; 7] = [
            b"qemu-system-x86_64\0-name\0demo\0",
            b"/usr/bin/qemu-system-aarch64\0-m\x00512M\0",
            b"qemu-kvm\0",
            b"qemu-system-x86_64",
            // A script, as the kernel runs it: the shell, then the script.
            b"/bin/sh\0/tmp/x/qemu-system-x86_64\0-name\0demo\0",
            b"bash\0./qemu-wrapper\0",
            b"/usr/bin/dash\0qemu-system-x86_64\0",
        ];
        for cmdline in qemu {
            assert!(
                cmdline_runs(cmdline, QEMU_PROGRAM),
                "{:?} should count as QEMU",
                String::from_utf8_lossy(cmdline)
            );
        }
        let other: [&[u8]; 9] = [
            b"",
            b"\0",
            b"sleep\x0060\0",
            b"/usr/lib/firefox/firefox\0-contentproc\0",
            b"/opt/qemu/bin/run\0",
            b"/bin/sh\0-c\0qemu-system-x86_64 -name demo\0",
            b"/bin/sh\0",
            b"/usr/bin/vim\0qemu-notes.txt\0",
            b"/usr/bin/swtpm\0socket\0--tpm2\0",
        ];
        for cmdline in other {
            assert!(
                !cmdline_runs(cmdline, QEMU_PROGRAM),
                "{:?} should not count as QEMU",
                String::from_utf8_lossy(cmdline)
            );
        }
        assert!(cmdline_runs(b"/usr/bin/swtpm\0socket\0--tpm2\0", "swtpm"));
        assert!(!cmdline_runs(b"qemu-system-x86_64\0", "swtpm"));
    }

    /// A PID file whose PID another program holds now, as after a host
    /// reboot, counts as stopped and goes, and `stop` leaves that process
    /// alone; a script standing in for QEMU counts as running.
    #[test]
    fn status_ignores_a_pid_another_program_took_over() {
        if !Path::new("/proc/self/cmdline").exists() {
            eprintln!("skipping: telling programs apart reads /proc");
            return;
        }
        let storage = tempfile::tempdir().unwrap();
        let storage = storage.path();
        fs::create_dir_all(vm_dir(storage, "v")).unwrap();

        let mut stranger = Command::new("sleep").arg("60").spawn().unwrap();
        let pid = stranger.id() as i32;
        wait_for_exec(pid);
        assert!(process_alive(pid) && !pid_runs(pid, QEMU_PROGRAM));
        fs::write(pid_path(storage, "v"), format!("{pid}\n")).unwrap();
        assert_eq!(status(storage, "v").unwrap(), ProcessInfo::default());
        assert!(
            !pid_path(storage, "v").exists(),
            "stale pid file not cleaned up"
        );
        fs::write(pid_path(storage, "v"), pid.to_string()).unwrap();
        let started = Instant::now();
        stop(storage, "v").unwrap();
        assert!(
            started.elapsed() < STOP_TIMEOUT,
            "stop waited for a process that is no QEMU"
        );
        let untouched = stranger.try_wait().unwrap();
        let _ = stranger.kill();
        let _ = stranger.wait();
        assert_eq!(untouched, None, "stop signalled a process that is no QEMU");
        assert!(!pid_path(storage, "v").exists());

        let bin = tempfile::tempdir().unwrap();
        let exe = fake_qemu(bin.path(), IDLE_UNTIL_TERM);
        let mut qemu = spawn_retrying(&mut Command::new(&exe)).unwrap();
        let pid = qemu.id() as i32;
        wait_for_exec(pid);
        fs::write(pid_path(storage, "v"), pid.to_string()).unwrap();
        let info = status(storage, "v");
        let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
        let _ = qemu.wait();
        assert_eq!(
            info.unwrap(),
            ProcessInfo {
                pid,
                status: VmStatus::Running
            },
            "a QEMU script run by the shell"
        );
    }

    /// A process caught in the middle of its exec has no command line yet
    /// and passes; a kernel thread, which never has one, does not.
    #[test]
    fn pid_runs_trusts_an_exec_but_not_a_kernel_thread() {
        if !Path::new("/proc/self/cmdline").exists() {
            eprintln!("skipping: telling programs apart reads /proc");
            return;
        }
        assert!(!is_kernel_thread(std::process::id() as i32));
        assert!(!is_kernel_thread(0));
        // kthreadd is PID 2 outside a PID namespace.
        let kthreadd = fs::read_to_string("/proc/2/stat").is_ok_and(|s| s.contains("(kthreadd)"));
        if kthreadd {
            assert!(is_kernel_thread(2));
            assert!(!pid_runs(2, QEMU_PROGRAM));
        }
        // Gone: no command line to read.
        let mut child = Command::new("true").spawn().unwrap();
        let pid = child.id() as i32;
        child.wait().unwrap();
        assert!(!pid_runs(pid, QEMU_PROGRAM));
    }

    #[test]
    fn process_alive_rejects_non_positive_pids() {
        assert!(!process_alive(0), "pid 0 is our own process group");
        assert!(!process_alive(-1), "negative pids address process groups");
        assert!(process_alive(std::process::id() as i32));
        assert!(!is_zombie(0));
        assert!(!is_zombie(std::process::id() as i32));
    }

    /// A QEMU that dies is reaped by the Ostrich that started it, so it
    /// neither lingers as a zombie nor counts as running (Go:
    /// TestStartReapsQEMU). Skipped without QEMU.
    #[test]
    fn start_reaps_qemu() {
        if !have_qemu() {
            return;
        }
        let storage = storage();
        let storage = storage.path();
        let mut cfg = vm("reap");
        Manager::new(storage).create(&mut cfg).unwrap();
        start(storage, &cfg).unwrap();
        let _stop = StopOnDrop {
            storage,
            name: &cfg.name,
        };
        wait_for_monitor(storage, &cfg.name);
        let info = status(storage, &cfg.name).unwrap();
        assert!(info.running(), "Status = {info:?} after Start");

        // Kill it the way a crash would, then it must be gone for good.
        kill(Pid::from_raw(info.pid), Signal::SIGKILL).unwrap();
        let gone = wait_reaped(info.pid);
        assert_eq!(
            gone,
            Err(Errno::ESRCH),
            "QEMU {} not reaped after it died (kill -0: {gone:?})",
            info.pid
        );
        let info = status(storage, &cfg.name).unwrap();
        assert_eq!(
            info.status,
            VmStatus::Stopped,
            "Status = {info:?} after the VM died"
        );
    }

    // The startup watch (Go: TestAwaitStartup) returns as soon as the monitor
    // answers, fails with QEMU's output when QEMU exits, and gives up waiting
    // after the grace period.

    #[test]
    fn reap_child_delivers_the_exit_status() {
        let child = Command::new("sh").args(["-c", "exit 7"]).spawn().unwrap();
        let exited = reap_child(child, thread::Builder::new()).unwrap();
        let status = exited
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        assert_eq!(status.code(), Some(7));
    }

    #[test]
    fn reap_child_kills_and_reaps_when_no_thread_can_be_had() {
        let child = Command::new("sleep").arg("60").spawn().unwrap();
        let pid = Pid::from_raw(child.id() as i32);
        // A stack larger than the address space: the thread is refused the
        // way it is under an exhausted pids limit.
        let err = reap_child(child, thread::Builder::new().stack_size(1 << 47)).unwrap_err();
        assert!(!err.to_string().is_empty());
        // Killed and reaped: the PID no longer exists, not even as a zombie.
        assert_eq!(kill(pid, None), Err(Errno::ESRCH));
    }

    #[test]
    fn await_startup_returns_when_the_monitor_answers() {
        let dir = storage();
        let sock = dir.path().join("up.sock");
        let log_path = dir.path().join("qemu.log");
        let _monitor = fake_hmp_sessions(&sock, |_| String::new());
        let (_tx, exited) = mpsc::channel();
        let started = Instant::now();
        await_startup(&exited, &sock, &log_path, Duration::from_secs(10)).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "did not return when the monitor answered"
        );
    }

    #[test]
    fn await_startup_reports_an_exit_with_output() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("qemu.log");
        fs::write(
            &log_path,
            "access denied by acl file\nqemu-system-x86_64: -netdev bridge,id=net0,br=br0: bridge helper failed\n",
        )
        .unwrap();
        let (tx, exited) = mpsc::channel();
        tx.send(Ok(exited_status(1))).unwrap();
        let err = await_startup(
            &exited,
            &dir.path().join("none.sock"),
            &log_path,
            Duration::from_secs(10),
        )
        .expect_err("await_startup = Ok for a QEMU that exited");
        assert_eq!(
            err.to_string(),
            "QEMU exited during startup (exit status 1):\n  access denied by acl file\n  qemu-system-x86_64: -netdev bridge,id=net0,br=br0: bridge helper failed"
        );
    }

    #[test]
    fn await_startup_reports_an_exit_without_output() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("qemu.log");
        fs::write(&log_path, "").unwrap();
        let (tx, exited) = mpsc::channel();
        tx.send(Ok(exited_status(1))).unwrap();
        let err = await_startup(
            &exited,
            &dir.path().join("none.sock"),
            &log_path,
            Duration::from_secs(10),
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "QEMU exited during startup (exit status 1) without any output"
        );

        // No log at all reads the same; a clean exit and an unknown status
        // get no parenthesis.
        fs::remove_file(&log_path).unwrap();
        let err = start_failure(Some(exited_status(0)), &log_path);
        assert_eq!(
            err.to_string(),
            "QEMU exited during startup without any output"
        );
        let err = start_failure(None, &log_path);
        assert_eq!(
            err.to_string(),
            "QEMU exited during startup without any output"
        );
    }

    #[test]
    fn await_startup_gives_up_after_the_grace_period() {
        let dir = tempfile::tempdir().unwrap();
        let (_tx, exited) = mpsc::channel();
        let started = Instant::now();
        await_startup(
            &exited,
            &dir.path().join("none.sock"),
            &dir.path().join("qemu.log"),
            Duration::from_millis(300),
        )
        .unwrap();
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(300) && elapsed <= Duration::from_secs(2),
            "returned after {elapsed:?}, want about the grace period"
        );
    }

    /// A listener that never answers, like QEMU's before its main loop runs,
    /// does not count as up.
    #[test]
    fn await_startup_ignores_a_silent_listener() {
        let dir = storage();
        let sock = dir.path().join("silent.sock");
        let _listener = UnixListener::bind(&sock).unwrap();
        let (_tx, exited) = mpsc::channel();
        let started = Instant::now();
        await_startup(
            &exited,
            &sock,
            &dir.path().join("qemu.log"),
            Duration::from_millis(300),
        )
        .unwrap();
        assert!(
            started.elapsed() >= Duration::from_millis(300),
            "a silent listener counted as a running monitor"
        );
    }

    /// A long QEMU output is cut to its last lines and points at the log file
    /// (Go: TestStartFailureTruncates).
    #[test]
    fn start_failure_truncates() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("qemu.log");
        let lines: Vec<String> = (1..=START_ERR_LINES + 5)
            .map(|i| format!("line {i}"))
            .collect();
        fs::write(&log_path, lines.join("\n") + "\n").unwrap();
        let msg = start_failure(None, &log_path).to_string();
        assert!(
            !msg.contains("line 5\n")
                && msg.contains("line 6\n")
                && msg.contains(&format!("line {}", START_ERR_LINES + 5)),
            "wrong lines kept:\n{msg}"
        );
        assert!(
            msg.contains(&log_path.display().to_string()),
            "truncated error does not name the log file:\n{msg}"
        );
        assert_eq!(
            msg,
            format!(
                "QEMU exited during startup:\n  line 6\n  line 7\n  line 8\n  line 9\n  line 10\n  line 11\n  line 12\n  line 13\n  (full output in {})",
                log_path.display()
            )
        );

        // Short output is quoted whole, without the pointer.
        fs::write(&log_path, "one\ntwo\n").unwrap();
        let msg = start_failure(None, &log_path).to_string();
        assert!(
            msg.contains("\n  one\n  two") && !msg.contains(&log_path.display().to_string()),
            "short output: {msg:?}"
        );
        assert_eq!(msg, "QEMU exited during startup:\n  one\n  two");

        // Exactly the limit is not truncated either.
        fs::write(&log_path, lines[..START_ERR_LINES].join("\n")).unwrap();
        let msg = start_failure(Some(exited_status(2)), &log_path).to_string();
        assert!(msg.starts_with("QEMU exited during startup (exit status 2):\n  line 1\n"));
        assert!(!msg.contains("full output"), "{msg}");
    }

    /// A QEMU that dies on startup makes Start fail with what QEMU printed,
    /// and leaves no VM counted as running (Go: TestStartReportsQEMUFailure).
    /// Skipped without QEMU.
    #[test]
    fn start_reports_qemu_failure() {
        if !have_qemu() {
            return;
        }
        let storage = storage();
        let storage = storage.path();
        let mut cfg = vm("broken");
        Manager::new(storage).create(&mut cfg).unwrap();
        // A disk that is not a qcow2 image: QEMU refuses it at once.
        fs::write(disk_path(storage, &cfg.name), "not an image").unwrap();
        let result = start(storage, &cfg);
        let _stop = StopOnDrop {
            storage,
            name: &cfg.name,
        };
        let err = result
            .expect_err("Start = Ok for a QEMU that cannot open its disk")
            .to_string();
        eprintln!("Start error:\n{err}");
        assert!(
            err.contains("QEMU exited during startup (exit status 1):")
                && err.contains("disk.qcow2"),
            "error does not quote QEMU: {err}"
        );
        let info = status(storage, &cfg.name).unwrap();
        assert_eq!(
            info.status,
            VmStatus::Stopped,
            "Status = {info:?} after a failed start"
        );
        assert!(
            !pid_path(storage, &cfg.name).exists(),
            "pid file left behind by a failed start"
        );
        let logged = fs::read_to_string(qemu_log_path(storage, &cfg.name)).unwrap();
        assert!(logged.contains("disk.qcow2"), "qemu.log = {logged:?}");
    }

    #[test]
    fn start_refuses_a_running_vm() {
        let storage = tempfile::tempdir().unwrap();
        let storage = storage.path();
        let cfg = vm("busy");
        fs::create_dir_all(vm_dir(storage, &cfg.name)).unwrap();
        let qemu = FakeQemu::spawn();
        qemu.run_as(storage, &cfg.name);
        let me = qemu.pid();
        let err = start_with(storage, &cfg, &|_| {
            panic!("QEMU looked up for a running VM")
        })
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("VM \"busy\" is already running (PID {me})")
        );
    }

    #[test]
    fn start_refuses_a_missing_qemu() {
        let storage = tempfile::tempdir().unwrap();
        let cfg = vm("nobin");
        let err = start_with(storage.path(), &cfg, &|_| None).unwrap_err();
        assert_eq!(
            err.to_string(),
            "\"qemu-system-x86_64\" not found in PATH — is QEMU installed?"
        );
        let mut arm = cfg.clone();
        arm.arch = "aarch64".into();
        let err = start_with(storage.path(), &arm, &|_| None).unwrap_err();
        assert_eq!(
            err.to_string(),
            "\"qemu-system-aarch64\" not found in PATH — is QEMU installed?"
        );
        assert!(
            !qemu_log_path(storage.path(), &cfg.name).exists(),
            "a QEMU log was made without a QEMU"
        );
    }

    /// A stand-in QEMU that prints and exits 1 is quoted in the error; its
    /// output lands in a fresh qemu.log and no PID file stays behind.
    #[test]
    fn start_quotes_a_qemu_that_dies() {
        let bin = tempfile::tempdir().unwrap();
        let exe = fake_qemu(
            bin.path(),
            "echo 'warning: no such thing as a free lunch'\necho \"$0: -drive file=/x/disk.qcow2: Could not open\" >&2\nexit 1",
        );
        let storage = tempfile::tempdir().unwrap();
        let storage = storage.path();
        let cfg = vm("dies");
        fs::create_dir_all(vm_dir(storage, &cfg.name)).unwrap();
        let log_path = qemu_log_path(storage, &cfg.name);
        fs::write(&log_path, "stale output of an earlier run\n".repeat(20)).unwrap();

        let err = start_fake(storage, &cfg, &exe).unwrap_err().to_string();
        assert_eq!(
            err,
            format!(
                "QEMU exited during startup (exit status 1):\n  warning: no such thing as a free lunch\n  {}: -drive file=/x/disk.qcow2: Could not open",
                exe.display()
            )
        );
        assert_eq!(
            fs::read_to_string(&log_path).unwrap(),
            format!(
                "warning: no such thing as a free lunch\n{}: -drive file=/x/disk.qcow2: Could not open\n",
                exe.display()
            ),
            "qemu.log must be truncated and hold both streams"
        );
        assert!(
            !pid_path(storage, &cfg.name).exists(),
            "pid file left behind"
        );
        assert_eq!(status(storage, &cfg.name).unwrap(), ProcessInfo::default());
    }

    /// A stand-in QEMU killed by a signal is reported in Go's words.
    #[test]
    fn start_names_the_signal_a_qemu_died_of() {
        let bin = tempfile::tempdir().unwrap();
        let exe = fake_qemu(bin.path(), "kill -9 $$");
        let storage = tempfile::tempdir().unwrap();
        let storage = storage.path();
        let cfg = vm("killed");
        fs::create_dir_all(vm_dir(storage, &cfg.name)).unwrap();
        let err = start_fake(storage, &cfg, &exe).unwrap_err().to_string();
        assert_eq!(
            err,
            "QEMU exited during startup (signal: killed) without any output"
        );
        assert_eq!(fs::read(qemu_log_path(storage, &cfg.name)).unwrap(), b"");
        assert!(
            !pid_path(storage, &cfg.name).exists(),
            "pid file left behind"
        );
    }

    /// A QEMU that neither dies nor answers within the grace period is taken
    /// to be up; it runs detached in its own session, is counted as running,
    /// and `stop` terminates and reaps it.
    #[test]
    fn start_assumes_a_silent_qemu_is_up_and_stop_ends_it() {
        let bin = tempfile::tempdir().unwrap();
        // Prints its session ID so the detachment can be checked, then
        // idles as a process that still counts as QEMU.
        let exe = fake_qemu(
            bin.path(),
            &format!("ps -o sid= -p $$\necho \"$*\" >&2\n{IDLE_UNTIL_TERM}"),
        );
        let storage = tempfile::tempdir().unwrap();
        let storage = storage.path();
        let cfg = vm("silent");
        fs::create_dir_all(vm_dir(storage, &cfg.name)).unwrap();

        let started = Instant::now();
        start_fake(storage, &cfg, &exe).unwrap();
        let elapsed = started.elapsed();
        assert!(
            elapsed >= START_GRACE && elapsed < START_GRACE + Duration::from_secs(2),
            "start returned after {elapsed:?}, want about the grace period"
        );
        let info = status(storage, &cfg.name).unwrap();
        assert!(info.running(), "Status = {info:?} after a quiet start");
        assert_eq!(
            fs::read_to_string(pid_path(storage, &cfg.name)).unwrap(),
            info.pid.to_string(),
            "qemu.pid is the decimal PID without a newline"
        );
        assert!(
            process_alive(info.pid) && !is_zombie(info.pid),
            "the stand-in should be alive"
        );

        // Its own session, and the argv we built.
        let log = fs::read_to_string(qemu_log_path(storage, &cfg.name)).unwrap();
        let (sid, argv) = log.split_once('\n').unwrap();
        assert_eq!(
            sid.trim(),
            info.pid.to_string(),
            "QEMU is not its own session leader:\n{log}"
        );
        let (_, want) = build_qemu_args(&cfg, storage).unwrap();
        assert_eq!(argv.trim_end(), want.join(" "));

        stop(storage, &cfg.name).unwrap();
        assert_eq!(
            wait_reaped(info.pid),
            Err(Errno::ESRCH),
            "stopped QEMU {} still around",
            info.pid
        );
        assert!(
            !pid_path(storage, &cfg.name).exists(),
            "pid file left after stop"
        );
        assert_eq!(status(storage, &cfg.name).unwrap(), ProcessInfo::default());
        // Stopping a stopped VM is fine.
        stop(storage, &cfg.name).unwrap();
        stop(storage, "never-existed").unwrap();
    }

    #[test]
    fn stop_kills_a_qemu_that_ignores_sigterm() {
        let bin = tempfile::tempdir().unwrap();
        let exe = fake_qemu(bin.path(), "trap '' TERM\nwhile :; do sleep 1; done");
        let storage = tempfile::tempdir().unwrap();
        let storage = storage.path();
        let cfg = vm("stubborn");
        fs::create_dir_all(vm_dir(storage, &cfg.name)).unwrap();
        start_fake(storage, &cfg, &exe).unwrap();
        let info = status(storage, &cfg.name).unwrap();
        assert!(info.running());

        let started = Instant::now();
        stop(storage, &cfg.name).unwrap();
        let elapsed = started.elapsed();
        assert!(
            elapsed >= STOP_TIMEOUT && elapsed < STOP_TIMEOUT + Duration::from_secs(3),
            "stop took {elapsed:?}, want the SIGTERM grace then SIGKILL"
        );
        assert_eq!(wait_reaped(info.pid), Err(Errno::ESRCH));
        assert_eq!(status(storage, &cfg.name).unwrap(), ProcessInfo::default());
    }

    #[test]
    fn read_tail_splits_lines_like_a_scanner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("console.log");
        assert!(read_tail(&path, 10).unwrap().is_empty(), "missing file");

        fs::write(&path, "").unwrap();
        assert!(read_tail(&path, 10).unwrap().is_empty(), "empty file");

        fs::write(&path, "one\r\ntwo\n\nfour\r").unwrap();
        assert_eq!(read_tail(&path, 10).unwrap(), ["one", "two", "", "four"]);
        assert_eq!(read_tail(&path, 2).unwrap(), ["", "four"]);
        assert_eq!(read_tail(&path, 1).unwrap(), ["four"]);
        assert!(read_tail(&path, 0).unwrap().is_empty());

        fs::write(&path, "last line ends the file\n").unwrap();
        assert_eq!(read_tail(&path, 10).unwrap(), ["last line ends the file"]);

        // Bytes that are not UTF-8 do not stop the read.
        fs::write(&path, b"ok\n\xff\xfe bad\n").unwrap();
        assert_eq!(
            read_tail(&path, 10).unwrap(),
            ["ok", "\u{FFFD}\u{FFFD} bad"]
        );

        // A directory is an error, naming the path.
        let err = read_tail(dir.path(), 10).unwrap_err().to_string();
        assert!(err.contains(&dir.path().display().to_string()), "{err}");
    }

    /// The split [`read_tail`] has to match: every line of the whole file,
    /// the last `max_lines` kept.
    fn whole_file_tail(data: &[u8], max_lines: usize) -> Vec<String> {
        let mut lines: Vec<String> = data
            .split_inclusive(|&b| b == b'\n')
            .map(|l| {
                let l = l.strip_suffix(b"\n").unwrap_or(l);
                let l = l.strip_suffix(b"\r").unwrap_or(l);
                String::from_utf8_lossy(l).into_owned()
            })
            .collect();
        lines.drain(..lines.len().saturating_sub(max_lines));
        lines
    }

    /// Reading backwards block by block gives what splitting the whole file
    /// gives, wherever lines and the block boundaries fall.
    #[test]
    fn read_tail_matches_a_whole_file_split() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("console.log");
        let mut rng = fastrand::Rng::with_seed(0x0057_1c4e);
        for round in 0..30 {
            // Short lines, middling ones and ones of a few KiB.
            let newline_odds = [3, 60, 3000][round % 3];
            let size = rng.usize(0..3 * TAIL_BLOCK + 10);
            let data: Vec<u8> = (0..size)
                .map(|_| match rng.u32(0..newline_odds + 4) {
                    0 => b'\n',
                    1 => b'\r',
                    2 => 0xc3,
                    3 => 0xa9,
                    _ => b'a' + rng.u8(0..26),
                })
                .collect();
            fs::write(&path, &data).unwrap();
            for max_lines in [0, 1, 2, START_ERR_LINES + 1, 200, usize::MAX] {
                assert_eq!(
                    read_tail(&path, max_lines).unwrap(),
                    whole_file_tail(&data, max_lines),
                    "round {round}: {size} bytes, {max_lines} lines"
                );
            }
        }

        // Many short lines: only the last ones come back.
        let data: String = (0..100_000).map(|i| format!("line {i}\n")).collect();
        fs::write(&path, &data).unwrap();
        let tail = read_tail(&path, 200).unwrap();
        assert_eq!(tail.len(), 200);
        assert_eq!(tail[0], "line 99800");
        assert_eq!(tail[199], "line 99999");
    }

    /// A line longer than Go's scanner limit keeps its end, cut at a
    /// character boundary; no more than the lines' worth of the file is read.
    #[test]
    fn read_tail_caps_long_lines_and_what_it_reads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("console.log");

        let long = format!("{}{}", "a".repeat(10), "x".repeat(TAIL_LINE_MAX + 5));
        fs::write(&path, format!("first\n{long}\nlast\n")).unwrap();
        assert_eq!(
            read_tail(&path, 10).unwrap(),
            [
                "first".to_string(),
                "x".repeat(TAIL_LINE_MAX),
                "last".to_string()
            ]
        );
        // The `\r` of a CRLF ending still goes; it counted towards the cap.
        fs::write(&path, format!("{long}\r\nlast")).unwrap();
        assert_eq!(
            read_tail(&path, 10).unwrap(),
            ["x".repeat(TAIL_LINE_MAX - 1), "last".to_string()]
        );

        // A cut inside a two-byte character drops the orphaned byte rather
        // than show a replacement character.
        let wide = format!("{}z", "é".repeat(TAIL_LINE_MAX / 2 + 5));
        fs::write(&path, format!("{wide}\n")).unwrap();
        assert_eq!(
            read_tail(&path, 10).unwrap(),
            [format!("{}z", "é".repeat(TAIL_LINE_MAX / 2 - 1))]
        );

        // A carriage-return progress bar without a newline is one line, cut.
        let bar: String = (0..20_000).map(|i| format!("\r{i:>5}%")).collect();
        fs::write(&path, &bar).unwrap();
        let tail = read_tail(&path, 200).unwrap();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].len(), TAIL_LINE_MAX);
        assert!(bar.ends_with(&tail[0]));

        // At most max_lines lines of the cap (and their newlines) are read:
        // what starts further back is left out, and the line that reaches
        // past it is cut there.
        fs::write(&path, format!("early\n{}", "y".repeat(3 * TAIL_LINE_MAX))).unwrap();
        assert_eq!(read_tail(&path, 2).unwrap(), ["y".repeat(TAIL_LINE_MAX)]);
        fs::write(
            &path,
            format!("early\n{}\n", "y".repeat(2 * TAIL_LINE_MAX + 1)),
        )
        .unwrap();
        assert_eq!(
            read_tail(&path, 2).unwrap(),
            ["y".repeat(TAIL_LINE_MAX)],
            "a newline right at the limit makes no empty line"
        );

        // A huge log costs only its tail: a sparse GiB of zeros, then text.
        let file = File::create(&path).unwrap();
        file.set_len(1 << 30).unwrap();
        file.write_all_at(b"\nsecond to last\nlast\n", 1 << 30)
            .unwrap();
        let started = Instant::now();
        let tail = read_tail(&path, 2).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "took {:?}",
            started.elapsed()
        );
        assert_eq!(tail, ["second to last", "last"]);
        let tail = read_tail(&path, 3).unwrap();
        assert_eq!(tail.len(), 3);
        assert_eq!(tail[0], "\0".repeat(TAIL_LINE_MAX));
    }

    /// Lines of exactly the cap are within it and come back whole, however
    /// many are asked for: the read takes in their newlines and the one
    /// before the oldest of them.
    #[test]
    fn read_tail_keeps_lines_at_the_cap_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("console.log");
        let mut data = b"start\n".to_vec();
        for i in 0..5 {
            data.extend(vec![b'a' + i; TAIL_LINE_MAX]);
            data.push(b'\n');
        }
        for ending in ["final newline", "no final newline"] {
            fs::write(&path, &data).unwrap();
            for max_lines in 1..=7 {
                let tail = read_tail(&path, max_lines).unwrap();
                assert_eq!(
                    tail,
                    whole_file_tail(&data, max_lines),
                    "{ending}, {max_lines} lines"
                );
                assert!(
                    tail.iter()
                        .all(|l| l == "start" || l.len() == TAIL_LINE_MAX),
                    "{ending}, {max_lines} lines: a line was cut"
                );
            }
            data.pop();
        }
    }

    #[test]
    fn read_console_tail_reads_the_console_log() {
        let storage = tempfile::tempdir().unwrap();
        let storage = storage.path();
        assert!(read_console_tail(storage, "v", 200).unwrap().is_empty());
        fs::create_dir_all(vm_dir(storage, "v")).unwrap();
        fs::write(console_path(storage, "v"), "boot\nlogin: ").unwrap();
        assert_eq!(
            read_console_tail(storage, "v", 200).unwrap(),
            ["boot", "login: "]
        );
        assert_eq!(read_console_tail(storage, "v", 1).unwrap(), ["login: "]);
    }

    /// The whole argv of a VM that uses every feature, in the Go order.
    #[test]
    #[rustfmt::skip] // one flag/value pair per line reads like the command line
    fn build_qemu_args_full_argv() {
        let storage = Path::new("/vms");
        let cfg = VmConfig {
            name: "demo".into(),
            cpu: 2,
            ram: 2048,
            disk_size: 20,
            arch: "x86_64".into(),
            cdrom_path: "/isos/debian,12.iso".into(),
            tpm: true,
            network: NetworkConfig {
                kind: NetworkType::User,
                mac: "52:54:00:12:34:56".into(),
                port_forwards: vec![
                    PortForward {
                        host: 2222,
                        guest: 22,
                        proto: "tcp".into(),
                    },
                    PortForward {
                        host: 5353,
                        guest: 53,
                        proto: "udp".into(),
                    },
                    PortForward {
                        host: 8080,
                        guest: 80,
                        proto: String::new(),
                    },
                ],
            },
            vnc_port: 1,
            usb_devices: vec![
                UsbDevice {
                    vendor_id: "046D".into(),
                    product_id: "085c".into(),
                    name: "Webcam".into(),
                    port: String::new(),
                },
                UsbDevice {
                    vendor_id: "0781".into(),
                    product_id: "5583".into(),
                    name: String::new(),
                    port: "3-2.2.4".into(),
                },
            ],
            usb_images: vec![UsbImage {
                path: "/isos/virtio win,1.iso".into(),
            }],
            disks: vec![Disk {
                name: "data".into(),
                size: 50,
            }],
            ..VmConfig::default()
        };
        let (bin, args) = build_qemu_args(&cfg, storage).unwrap();
        assert_eq!(bin, "qemu-system-x86_64");

        let mut want: Vec<&str> = vec![
            "-name", "demo",
            "-m", "2048M",
            "-smp", "2",
            "-machine", "q35",
            "-drive", "file=/vms/demo/disk.qcow2,format=qcow2,if=virtio",
            "-device", "pcie-root-port,id=disk-rp1,bus=pcie.0,chassis=1,addr=0x10",
            "-device", "pcie-root-port,id=disk-rp2,bus=pcie.0,chassis=2,addr=0x11",
            "-device", "pcie-root-port,id=disk-rp3,bus=pcie.0,chassis=3,addr=0x12",
            "-device", "pcie-root-port,id=disk-rp4,bus=pcie.0,chassis=4,addr=0x13",
            "-device", "pcie-root-port,id=disk-rp5,bus=pcie.0,chassis=5,addr=0x14",
            "-device", "pcie-root-port,id=disk-rp6,bus=pcie.0,chassis=6,addr=0x15",
            "-device", "pcie-root-port,id=disk-rp7,bus=pcie.0,chassis=7,addr=0x16",
            "-device", "pcie-root-port,id=disk-rp8,bus=pcie.0,chassis=8,addr=0x17",
            "-drive", "if=none,id=disk-data-drive,format=qcow2,file=/vms/demo/data.qcow2",
            "-device", "virtio-blk-pci,id=disk-data,drive=disk-data-drive,bus=disk-rp1,serial=data",
            "-chardev", "socket,id=serial0,path=/vms/demo/serial.sock,server=on,wait=off,logfile=/vms/demo/console.log",
            "-serial", "chardev:serial0",
            "-monitor", "unix:/vms/demo/qemu-monitor.sock,server,nowait",
            "-pidfile", "/vms/demo/qemu.pid",
            "-display", "none",
        ];
        if kvm_runs("x86_64") {
            want.extend(["-enable-kvm", "-cpu", "host"]);
        }
        want.extend([
            "-drive", "if=ide,index=2,id=cdrom,media=cdrom,format=raw,file=/isos/debian,,12.iso",
            "-boot", "order=dc",
            "-chardev", "socket,id=chrtpm,path=/vms/demo/swtpm.sock",
            "-tpmdev", "emulator,id=tpm0,chardev=chrtpm",
            "-device", "tpm-tis,tpmdev=tpm0",
            "-vnc", "127.0.0.1:1",
            "-device", "qemu-xhci,id=xhci",
            "-device", "usb-host,id=usb-046d-085c,bus=xhci.0,vendorid=0x046d,productid=0x085c",
            "-device", "usb-host,id=usb-0781-5583-3-2.2.4,bus=xhci.0,vendorid=0x0781,productid=0x5583,hostbus=3,hostport=2.2.4",
            "-drive", "if=none,id=usbimg-virtio-win-1.iso-drive,format=raw,readonly=on,file=/isos/virtio win,,1.iso",
            "-device", "usb-storage,id=usbimg-virtio-win-1.iso,bus=xhci.0,drive=usbimg-virtio-win-1.iso-drive,removable=on",
            "-netdev", "user,id=net0,net=10.0.2.0/24,dhcpstart=10.0.2.15,hostfwd=tcp::2222-:22,hostfwd=udp::5353-:53,hostfwd=tcp::8080-:80",
            "-device", "virtio-net-pci,netdev=net0,mac=52:54:00:12:34:56",
        ]);
        assert_eq!(args, want);
    }

    #[test]
    fn kvm_only_for_the_hosts_architecture() {
        assert!(kvm_arch_matches("x86_64", "x86_64"));
        assert!(kvm_arch_matches("x86_64", "i386"));
        assert!(!kvm_arch_matches("x86_64", "aarch64"));
        assert!(kvm_arch_matches("aarch64", "aarch64"));
        assert!(kvm_arch_matches("aarch64", "arm64"));
        assert!(!kvm_arch_matches("aarch64", "x86_64"));
        assert!(!kvm_arch_matches("aarch64", "i386"));
    }

    /// The arm `virt` machine, tap networking and a TPM: no IDE, the sysbus
    /// TPM device, the bridge netdev; `none` networking gets `-nic none`
    /// and its devices pinned (see the next test).
    #[test]
    #[rustfmt::skip] // one flag/value pair per line reads like the command line
    fn build_qemu_args_virt_tap_and_none() {
        let storage = Path::new("/vms");
        let mut cfg = VmConfig {
            name: "pi".into(),
            cpu: 4,
            ram: 1024,
            arch: "aarch64".into(),
            tpm: true,
            network: NetworkConfig {
                kind: NetworkType::Tap,
                mac: "52:54:00:ab:cd:ef".into(),
                port_forwards: Vec::new(),
            },
            ..VmConfig::default()
        };
        let (bin, args) = build_qemu_args(&cfg, storage).unwrap();
        assert_eq!(bin, "qemu-system-aarch64");
        let mut want: Vec<String> = [
            "-name", "pi",
            "-m", "1024M",
            "-smp", "4",
            "-machine", "virt",
            "-drive", "file=/vms/pi/disk.qcow2,format=qcow2,if=virtio",
        ]
        .map(String::from)
        .to_vec();
        for i in 1..=8 {
            want.push("-device".to_string());
            want.push(format!(
                "pcie-root-port,id=disk-rp{i},bus=pcie.0,chassis={i},addr=0x{:x}",
                0x10 + i - 1
            ));
        }
        want.extend(
            [
                "-chardev", "socket,id=serial0,path=/vms/pi/serial.sock,server=on,wait=off,logfile=/vms/pi/console.log",
                "-serial", "chardev:serial0",
                "-monitor", "unix:/vms/pi/qemu-monitor.sock,server,nowait",
                "-pidfile", "/vms/pi/qemu.pid",
                "-display", "none",
            ]
            .map(String::from),
        );
        if kvm_runs("aarch64") {
            want.extend(["-enable-kvm", "-cpu", "host"].map(String::from));
        }
        want.extend(
            [
                "-drive", "if=none,id=cdrom,media=cdrom",
                "-device", "virtio-scsi-pci,id=scsi0",
                "-device", "scsi-cd,bus=scsi0.0,drive=cdrom",
                "-chardev", "socket,id=chrtpm,path=/vms/pi/swtpm.sock",
                "-tpmdev", "emulator,id=tpm0,chardev=chrtpm",
                "-device", "tpm-tis-device,tpmdev=tpm0",
                "-device", "qemu-xhci,id=xhci",
                "-netdev", "bridge,id=net0,br=br0",
                "-device", "virtio-net-pci,netdev=net0,mac=52:54:00:ab:cd:ef",
            ]
            .map(String::from),
        );
        assert_eq!(args, want);

        // `none` says so, or QEMU would add its default NIC.
        cfg.network.kind = NetworkType::None;
        let (_, args) = build_qemu_args(&cfg, storage).unwrap();
        assert_eq!(args[args.len() - 4..], ["-device", "qemu-xhci,id=xhci,addr=0x3", "-nic", "none"]);
        assert!(!args.iter().any(|a| a == "-netdev"));
    }

    /// A VM without a network gets Go's argv, which had no network
    /// arguments at all, plus `-nic none` against QEMU's default NIC, and the
    /// devices that NIC pushed down pinned where it left them: the main disk
    /// (a drive plus a device then) in slot 4, the xHCI in slot 3 and, on
    /// `virt`, the CD-ROM's SCSI controller in slot 2. User VMs keep Go's
    /// argv, whose network arguments come last.
    #[test]
    fn build_qemu_args_without_a_network_keeps_gos_slots() {
        let storage = Path::new("/vms");
        for arch in ["x86_64", "aarch64"] {
            let mut cfg = VmConfig {
                name: "n".into(),
                cpu: 1,
                ram: 512,
                arch: arch.into(),
                cdrom_path: "/isos/a.iso".into(),
                tpm: true,
                network: NetworkConfig {
                    kind: NetworkType::User,
                    mac: "52:54:00:12:34:56".into(),
                    port_forwards: Vec::new(),
                },
                vnc_port: 3,
                usb_images: vec![UsbImage {
                    path: "/isos/b.iso".into(),
                }],
                disks: vec![Disk {
                    name: "data".into(),
                    size: 1,
                }],
                ..VmConfig::default()
            };
            let (_, user) = build_qemu_args(&cfg, storage).unwrap();
            let (go, net) = user.split_at(user.len() - 4);
            assert_eq!(net[0], "-netdev", "{arch}: {net:?}");
            assert!(net[3].starts_with("virtio-net-pci,"), "{arch}: {net:?}");

            cfg.network.kind = NetworkType::None;
            let (_, none) = build_qemu_args(&cfg, storage).unwrap();
            let mut want: Vec<String> = Vec::new();
            for word in go {
                match word.as_str() {
                    "file=/vms/n/disk.qcow2,format=qcow2,if=virtio" => want.extend(
                        [
                            "file=/vms/n/disk.qcow2,format=qcow2,if=none,id=virtio0",
                            "-device",
                            "virtio-blk-pci,drive=virtio0,addr=0x4",
                        ]
                        .map(String::from),
                    ),
                    "qemu-xhci,id=xhci" => want.push("qemu-xhci,id=xhci,addr=0x3".into()),
                    "virtio-scsi-pci,id=scsi0" => {
                        want.push("virtio-scsi-pci,id=scsi0,addr=0x2".into())
                    }
                    _ => want.push(word.clone()),
                }
            }
            want.extend(["-nic", "none"].map(String::from));
            assert_eq!(none, want, "{arch}");
            assert_eq!(
                none.iter()
                    .any(|a| a == "virtio-scsi-pci,id=scsi0,addr=0x2"),
                arch == "aarch64",
                "{arch}: {none:?}"
            );
        }
    }

    /// QEMU takes a MAC in the colon form only: one in another spelling the
    /// forms accept goes in normalised, for user and tap alike. An empty one
    /// is left to QEMU's default; anything else is passed for QEMU to refuse.
    #[test]
    fn build_qemu_args_passes_the_mac_in_qemus_spelling() {
        let mut cfg = vm("mac");
        for kind in [NetworkType::User, NetworkType::Tap] {
            for (mac, want) in [
                (
                    "5254.0012.3456",
                    "virtio-net-pci,netdev=net0,mac=52:54:00:12:34:56",
                ),
                (
                    "52-54-00-AB-cd-EF",
                    "virtio-net-pci,netdev=net0,mac=52:54:00:ab:cd:ef",
                ),
                (
                    "52:54:00:12:34:56",
                    "virtio-net-pci,netdev=net0,mac=52:54:00:12:34:56",
                ),
                ("", "virtio-net-pci,netdev=net0"),
                ("nope", "virtio-net-pci,netdev=net0,mac=nope"),
            ] {
                cfg.network = NetworkConfig {
                    kind,
                    mac: mac.into(),
                    port_forwards: Vec::new(),
                };
                let (_, args) = build_qemu_args(&cfg, Path::new("/vms")).unwrap();
                assert_eq!(args[args.len() - 2..], ["-device", want], "{kind} {mac:?}");
            }
        }
    }

    #[test]
    fn build_qemu_args_rejects_bad_disks() {
        let mut cfg = vm("bad");
        cfg.disks = vec![
            Disk {
                name: "data".into(),
                size: 1,
            },
            Disk {
                name: "Data".into(),
                size: 1,
            },
        ];
        let err = build_qemu_args(&cfg, Path::new("/vms"))
            .unwrap_err()
            .to_string();
        assert_eq!(err, "duplicate disk name \"Data\"");
    }

    /// Sanity run against the real host, end to end: a tiny BIOS VM (1 CPU,
    /// 128 MiB, 1 GiB disk, no network, no ISO) is created through the
    /// manager, started, seen running and answering the monitor, stopped,
    /// seen stopped with neither a process nor a zombie left behind, and
    /// deleted. Ignored by default because it launches QEMU; run it with
    /// `cargo test -- --ignored vm::process::tests::real_qemu_lifecycle`.
    #[test]
    #[ignore = "launches a real QEMU"]
    fn real_qemu_lifecycle() {
        if !have_qemu() {
            return;
        }
        let storage = storage();
        let storage = storage.path();
        let mgr = Manager::new(storage);
        let mut cfg = vm("sanity");

        // Create: the directory, the disk image and vm.yaml, with the
        // defaults filled in.
        mgr.create(&mut cfg).unwrap();
        assert!(mgr.exists(&cfg.name));
        assert!(disk_path(storage, &cfg.name).is_file(), "no disk image");
        assert!(!cfg.network.mac.is_empty(), "create did not pick a MAC");
        assert_eq!(
            (cfg.firmware, cfg.arch.as_str()),
            (FirmwareType::Bios, "x86_64")
        );
        let listed = mgr.list().unwrap();
        assert_eq!(listed.len(), 1, "list = {listed:?}");
        assert_eq!(listed[0].name, cfg.name);
        assert_eq!(status(storage, &cfg.name).unwrap(), ProcessInfo::default());

        // Start: running, with a PID, and the monitor answers.
        start(storage, &cfg).unwrap();
        let _stop = StopOnDrop {
            storage,
            name: &cfg.name,
        };
        let info = status(storage, &cfg.name).unwrap();
        assert!(
            info.running() && info.pid > 0,
            "Status = {info:?} after start"
        );
        assert!(process_alive(info.pid) && !is_zombie(info.pid));
        wait_for_monitor(storage, &cfg.name);
        let out = monitor_command(storage, &cfg.name, "info status").unwrap();
        eprintln!("QEMU {} answered `info status` with {out:?}", info.pid);
        assert!(out.contains("VM status: running"), "info status = {out:?}");
        assert!(
            read_console_tail(storage, &cfg.name, 10).is_ok(),
            "console log unreadable"
        );

        // Stop: stopped, the process gone for good (reaped, not a zombie),
        // no pid file, no monitor.
        stop(storage, &cfg.name).unwrap();
        let gone = wait_reaped(info.pid);
        assert_eq!(
            gone,
            Err(Errno::ESRCH),
            "QEMU {} still around after stop (kill -0: {gone:?})",
            info.pid
        );
        assert!(!is_zombie(info.pid), "QEMU {} left as a zombie", info.pid);
        assert_eq!(status(storage, &cfg.name).unwrap(), ProcessInfo::default());
        assert!(
            !pid_path(storage, &cfg.name).exists(),
            "pid file left after stop"
        );
        assert!(
            monitor_command(storage, &cfg.name, "info status").is_err(),
            "monitor still answers after stop"
        );

        // Delete: nothing left of it.
        mgr.delete(&cfg.name).unwrap();
        assert!(!mgr.exists(&cfg.name));
        assert!(
            !vm_dir(storage, &cfg.name).exists(),
            "VM directory left after delete"
        );
        assert!(mgr.list().unwrap().is_empty());
    }

    /// The PCI devices in `info qtree` output, one
    /// `<bus> <slot>.<function> <device>, id "<id>"` line each, sorted.
    fn pci_placements(qtree: &str) -> Vec<String> {
        let mut buses: Vec<(usize, &str)> = Vec::new(); // (indent, name)
        let mut dev = None;
        let mut placed = Vec::new();
        for line in qtree.lines() {
            let text = line.trim_start();
            let indent = line.len() - text.len();
            if let Some(bus) = text.strip_prefix("bus: ") {
                buses.retain(|&(i, _)| i < indent);
                buses.push((indent, bus));
                dev = None;
            } else if let Some(name) = text.strip_prefix("dev: ") {
                buses.retain(|&(i, _)| i < indent);
                dev = buses.last().map(|&(_, bus)| (bus, name));
            } else if let Some(addr) = text.strip_prefix("addr = ") {
                if let Some((bus, name)) = dev.take() {
                    placed.push(format!("{bus} {addr} {name}"));
                }
            }
        }
        placed.sort();
        placed
    }

    /// Runs `bin` with the VM's `args` paused (`-S`), without KVM so any
    /// architecture runs, and returns its `info qtree`.
    fn paused_qtree(storage: &Path, name: &str, bin: &str, args: &[String]) -> String {
        let mut cmd = Command::new(bin);
        let mut words = args.iter();
        while let Some(word) = words.next() {
            match word.as_str() {
                "-enable-kvm" => {}
                "-cpu" => drop(words.next()),
                _ => drop(cmd.arg(word)),
            }
        }
        let log = File::create(qemu_log_path(storage, name)).unwrap();
        cmd.arg("-S")
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log);
        let mut qemu = spawn_retrying(&mut cmd).unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let qtree = loop {
            if let Ok(out) = monitor_command(storage, name, "info qtree") {
                if !out.is_empty() {
                    break Ok(out);
                }
            }
            if qemu.try_wait().unwrap().is_some() || Instant::now() >= deadline {
                break Err(fs::read_to_string(qemu_log_path(storage, name)).unwrap_or_default());
            }
            thread::sleep(Duration::from_millis(100));
        };
        let _ = qemu.kill();
        let _ = qemu.wait();
        qtree.unwrap_or_else(|log| panic!("{bin} did not come up:\n{log}"))
    }

    /// Checked against real QEMU, paused: without a network, every PCI
    /// device of the VM keeps the bus, slot and function it had under Go,
    /// whose command line had no network arguments and so QEMU's default
    /// NIC; only that NIC is gone. Done for each machine type, q35
    /// (`x86_64`) and `virt` (`aarch64`, when `qemu-system-aarch64` is
    /// installed), with a CD-ROM, an additional disk and a USB image. Ignored
    /// by default because it launches QEMU; run it with
    /// `cargo test -- --ignored vm::process::tests::real_qemu_keeps_gos_pci_slots_without_a_network`.
    #[test]
    #[ignore = "launches a real QEMU"]
    fn real_qemu_keeps_gos_pci_slots_without_a_network() {
        if !have_qemu() {
            return;
        }
        for (arch, default_nic) in [("x86_64", "e1000e"), ("aarch64", "virtio-net-pci")] {
            let bin = format!("qemu-system-{arch}");
            if which::which(&bin).is_err() {
                eprintln!("skipping {arch}: {bin} not installed");
                continue;
            }
            let storage = storage();
            let storage = storage.path();
            let mut cfg = vm("slots");
            cfg.arch = arch.into();
            cfg.cdrom_path = write_image(storage, "cd.iso", 0);
            cfg.usb_images = vec![UsbImage {
                path: write_image(storage, "stick.img", 1 << 20),
            }];
            cfg.disks = vec![Disk {
                name: "data".into(),
                size: 1,
            }];
            Manager::new(storage).create(&mut cfg).unwrap();

            // Go's argv: a user VM's without its network arguments, which
            // come last.
            cfg.network.kind = NetworkType::User;
            let (_, user) = build_qemu_args(&cfg, storage).unwrap();
            let go = paused_qtree(storage, &cfg.name, &bin, &user[..user.len() - 4]);
            cfg.network.kind = NetworkType::None;
            let (_, none) = build_qemu_args(&cfg, storage).unwrap();
            let ours = paused_qtree(storage, &cfg.name, &bin, &none);

            let go = pci_placements(&go);
            let ours = pci_placements(&ours);
            eprintln!("{arch} under Go:\n  {}", go.join("\n  "));
            let (nic, rest): (Vec<_>, Vec<_>) = go
                .into_iter()
                .partition(|d| d.contains(&format!(" {default_nic}, ")));
            assert_eq!(nic.len(), 1, "{arch}: Go's default NIC: {nic:?}");
            assert_eq!(ours, rest, "{arch}: devices moved");
            for dev in [
                "pcie.0 03.0 qemu-xhci, id \"xhci\"",
                "pcie.0 04.0 virtio-blk-pci, id \"\"",
                "disk-rp1 00.0 virtio-blk-pci, id \"disk-data\"",
            ] {
                assert!(ours.iter().any(|d| d == dev), "{arch}: no {dev}: {ours:?}");
            }
        }
    }
}
