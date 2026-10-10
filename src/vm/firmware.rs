//! UEFI firmware discovery (QEMU firmware descriptors, then well-known
//! paths), per-VM NVRAM stores and Secure Boot key enrolment.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, PipeReader, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::LazyLock;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use nix::errno::Errno;
use nix::fcntl::{fcntl, FcntlArg, OFlag};
use serde::Deserialize;

use super::config::{firmware_vars_path, VmConfig};

/// A UEFI firmware usable with QEMU's pflash devices: a read-only code image
/// shared by all VMs and a pristine NVRAM template each VM gets its own
/// writable copy of.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Firmware {
    pub description: String,
    /// Executable image.
    pub code: String,
    /// `raw` or `qcow2`.
    pub code_format: String,
    /// NVRAM template, copied per VM.
    pub vars_template: String,
    pub vars_format: String,
    /// The image can enforce Secure Boot.
    pub secure_boot: bool,
    /// The template already carries PK, KEK and db.
    pub enrolled_keys: bool,
    /// Needs `-machine smm=on` and the pflash `secure` property.
    pub requires_smm: bool,
}

impl Firmware {
    /// Whether both image files are present on the host.
    fn exists(&self) -> bool {
        Path::new(&self.code).exists() && Path::new(&self.vars_template).exists()
    }
}

/// The subset of QEMU's firmware descriptor schema
/// (docs/interop/firmware.json) that selecting a firmware needs. Keys the
/// schema has and this struct lacks are ignored; missing keys are empty.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FirmwareDescriptor {
    description: String,
    #[serde(rename = "interface-types")]
    interface_types: Vec<String>,
    mapping: FirmwareMapping,
    targets: Vec<FirmwareTarget>,
    features: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FirmwareMapping {
    device: String,
    /// `split` (default), `combined` or `stateless`.
    mode: String,
    executable: FirmwareFile,
    #[serde(rename = "nvram-template")]
    nvram_template: FirmwareFile,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FirmwareTarget {
    architecture: String,
    /// Globs over canonical machine names.
    machines: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FirmwareFile {
    filename: String,
    format: String,
}

impl FirmwareDescriptor {
    /// Whether the descriptor is a split-pflash UEFI firmware for the
    /// architecture and one of the machine names (see [`machine_names`]).
    fn supports(&self, arch: &str, machines: &[&str]) -> bool {
        if !self.interface_types.iter().any(|t| t == "uefi") || self.mapping.device != "flash" {
            return false;
        }
        if !self.mapping.mode.is_empty() && self.mapping.mode != "split" {
            return false;
        }
        if self.mapping.executable.filename.is_empty()
            || self.mapping.nvram_template.filename.is_empty()
        {
            return false;
        }
        self.targets
            .iter()
            .filter(|t| t.architecture == arch)
            .flat_map(|t| &t.machines)
            // A malformed glob matches nothing, as with Go's path.Match.
            .filter_map(|g| glob::Pattern::new(g).ok())
            .any(|pattern| machines.iter().any(|m| pattern.matches(m)))
    }

    fn into_firmware(self) -> Firmware {
        let has = |feature: &str| self.features.iter().any(|f| f == feature);
        let secure_boot = has("secure-boot");
        let enrolled_keys = has("enrolled-keys");
        let requires_smm = has("requires-smm");
        let or_raw = |f: String| if f.is_empty() { "raw".to_string() } else { f };
        Firmware {
            description: self.description,
            code: self.mapping.executable.filename,
            code_format: or_raw(self.mapping.executable.format),
            vars_template: self.mapping.nvram_template.filename,
            vars_format: or_raw(self.mapping.nvram_template.format),
            secure_boot,
            enrolled_keys,
            requires_smm,
        }
    }
}

/// The names a descriptor glob may be written against: the type as given
/// plus the canonical prefix its alias stands for, so that `pc-q35-*`
/// matches `q35` without asking QEMU to expand the alias (`*` matches the
/// empty string).
fn machine_names(machine: &str) -> Vec<&str> {
    match machine {
        "q35" => vec![machine, "pc-q35-"],
        "pc" => vec![machine, "pc-i440fx-"],
        "virt" => vec![machine, "virt-"],
        _ => vec![machine],
    }
}

/// One row of the well-known-path table; the formats and the description
/// are filled in when an entry is picked.
struct KnownEntry {
    code: &'static str,
    vars: &'static str,
    secure_boot: bool,
    enrolled_keys: bool,
    requires_smm: bool,
}

/// The template for a plain image row: `KnownEntry { code, vars, ..PLAIN }`.
const PLAIN: KnownEntry = KnownEntry {
    code: "",
    vars: "",
    secure_boot: false,
    enrolled_keys: false,
    requires_smm: false,
};

impl KnownEntry {
    fn firmware(&self) -> Firmware {
        Firmware {
            code: self.code.to_string(),
            vars_template: self.vars.to_string(),
            secure_boot: self.secure_boot,
            enrolled_keys: self.enrolled_keys,
            requires_smm: self.requires_smm,
            ..Firmware::default()
        }
    }
}

/// The fallback for hosts without descriptors, in order of preference.
const KNOWN_X86_64: &[KnownEntry] = &[
    // Fedora / RHEL
    KnownEntry {
        code: "/usr/share/edk2/ovmf/OVMF_CODE.secboot.fd",
        vars: "/usr/share/edk2/ovmf/OVMF_VARS.secboot.fd",
        secure_boot: true,
        enrolled_keys: true,
        requires_smm: true,
    },
    KnownEntry {
        code: "/usr/share/edk2/ovmf/OVMF_CODE.fd",
        vars: "/usr/share/edk2/ovmf/OVMF_VARS.fd",
        ..PLAIN
    },
    // Debian / Ubuntu
    KnownEntry {
        code: "/usr/share/OVMF/OVMF_CODE_4M.secboot.fd",
        vars: "/usr/share/OVMF/OVMF_VARS_4M.ms.fd",
        secure_boot: true,
        enrolled_keys: true,
        requires_smm: true,
    },
    KnownEntry {
        code: "/usr/share/OVMF/OVMF_CODE_4M.fd",
        vars: "/usr/share/OVMF/OVMF_VARS_4M.fd",
        ..PLAIN
    },
    // Arch
    KnownEntry {
        code: "/usr/share/edk2/x64/OVMF_CODE.secboot.4m.fd",
        vars: "/usr/share/edk2/x64/OVMF_VARS.4m.fd",
        secure_boot: true,
        requires_smm: true,
        ..PLAIN
    },
    KnownEntry {
        code: "/usr/share/edk2/x64/OVMF_CODE.4m.fd",
        vars: "/usr/share/edk2/x64/OVMF_VARS.4m.fd",
        ..PLAIN
    },
    // Images bundled with QEMU itself
    KnownEntry {
        code: "/usr/share/qemu/edk2-x86_64-secure-code.fd",
        vars: "/usr/share/qemu/edk2-i386-vars.fd",
        secure_boot: true,
        requires_smm: true,
        ..PLAIN
    },
    KnownEntry {
        code: "/usr/share/qemu/edk2-x86_64-code.fd",
        vars: "/usr/share/qemu/edk2-i386-vars.fd",
        ..PLAIN
    },
    KnownEntry {
        code: "/opt/homebrew/share/qemu/edk2-x86_64-secure-code.fd",
        vars: "/opt/homebrew/share/qemu/edk2-i386-vars.fd",
        secure_boot: true,
        requires_smm: true,
        ..PLAIN
    },
    KnownEntry {
        code: "/opt/homebrew/share/qemu/edk2-x86_64-code.fd",
        vars: "/opt/homebrew/share/qemu/edk2-i386-vars.fd",
        ..PLAIN
    },
];

const KNOWN_AARCH64: &[KnownEntry] = &[
    KnownEntry {
        code: "/usr/share/AAVMF/AAVMF_CODE.ms.fd",
        vars: "/usr/share/AAVMF/AAVMF_VARS.ms.fd",
        secure_boot: true,
        enrolled_keys: true,
        ..PLAIN
    },
    KnownEntry {
        code: "/usr/share/AAVMF/AAVMF_CODE.fd",
        vars: "/usr/share/AAVMF/AAVMF_VARS.fd",
        ..PLAIN
    },
    KnownEntry {
        code: "/usr/share/edk2/aarch64/QEMU_EFI-pflash.raw",
        vars: "/usr/share/edk2/aarch64/vars-template-pflash.raw",
        ..PLAIN
    },
    KnownEntry {
        code: "/usr/share/edk2/aarch64/QEMU_EFI.fd",
        vars: "/usr/share/edk2/aarch64/QEMU_VARS.fd",
        ..PLAIN
    },
    KnownEntry {
        code: "/usr/share/qemu/edk2-aarch64-code.fd",
        vars: "/usr/share/qemu/edk2-arm-vars.fd",
        ..PLAIN
    },
    KnownEntry {
        code: "/opt/homebrew/share/qemu/edk2-aarch64-code.fd",
        vars: "/opt/homebrew/share/qemu/edk2-arm-vars.fd",
        ..PLAIN
    },
];

/// The well-known-path candidates for an architecture, in order of
/// preference; empty for an architecture the table does not know.
pub(crate) fn known_firmware(arch: &str) -> &'static [Firmware] {
    static X86_64: LazyLock<Vec<Firmware>> =
        LazyLock::new(|| KNOWN_X86_64.iter().map(KnownEntry::firmware).collect());
    static AARCH64: LazyLock<Vec<Firmware>> =
        LazyLock::new(|| KNOWN_AARCH64.iter().map(KnownEntry::firmware).collect());
    match arch {
        "x86_64" => &X86_64,
        "aarch64" => &AARCH64,
        _ => &[],
    }
}

/// Picks the UEFI firmware for an architecture and machine type, the way
/// libvirt does: QEMU firmware descriptors first, then well-known paths.
/// With `secure_boot` it only returns Secure Boot capable images and prefers
/// a template with the keys already enrolled; without it, it prefers a plain
/// image. Error when nothing is found:
/// `no UEFI firmware found for <arch>/<machine> — install OVMF:\n  sudo pacman -S edk2-ovmf    # Arch\n  sudo apt install ovmf       # Debian/Ubuntu\n  sudo dnf install edk2-ovmf  # Fedora`
/// (`Secure Boot capable UEFI firmware` with `secure_boot`).
pub fn find_firmware(arch: &str, machine: &str, secure_boot: bool) -> Result<Firmware> {
    if let Some(found) = fixture_lookup(arch, machine, secure_boot) {
        return found;
    }
    find_firmware_in(&default_firmware_dirs(), arch, machine, secure_boot)
}

/// The lookup against a test's [`fixture`] host, when one is installed.
#[cfg(test)]
fn fixture_lookup(arch: &str, machine: &str, secure_boot: bool) -> Option<Result<Firmware>> {
    let host = fixture::snapshot()?;
    Some(find_firmware_with(
        &host.dirs,
        host.known_for(arch),
        arch,
        machine,
        secure_boot,
    ))
}

#[cfg(not(test))]
fn fixture_lookup(_arch: &str, _machine: &str, _secure_boot: bool) -> Option<Result<Firmware>> {
    None
}

/// [`find_firmware`] reading descriptors from explicit directories, highest
/// priority first (tests point it at fixtures; the well-known-path fallback
/// still applies).
pub(crate) fn find_firmware_in(
    dirs: &[PathBuf],
    arch: &str,
    machine: &str,
    secure_boot: bool,
) -> Result<Firmware> {
    find_firmware_with(dirs, known_firmware(arch), arch, machine, secure_boot)
}

/// [`find_firmware_in`] with an explicit well-known-path table for the
/// architecture (`known`, in order of preference; empty disables the
/// fallback), so a lookup can be exercised without the host's packages.
pub(crate) fn find_firmware_with(
    dirs: &[PathBuf],
    known: &[Firmware],
    arch: &str,
    machine: &str,
    secure_boot: bool,
) -> Result<Firmware> {
    let machines = machine_names(machine);
    let mut fallback = None;
    for d in load_firmware_descriptors(dirs) {
        if !d.supports(arch, &machines) {
            continue;
        }
        let fw = d.into_firmware();
        if !fw.exists() {
            continue;
        }
        if secure_boot {
            if !fw.secure_boot {
                continue;
            }
            if fw.enrolled_keys {
                return Ok(fw);
            }
        } else if !fw.secure_boot {
            return Ok(fw);
        }
        if fallback.is_none() {
            fallback = Some(fw);
        }
    }
    if let Some(fw) = fallback {
        return Ok(fw);
    }

    for fw in known {
        if secure_boot && !fw.secure_boot {
            continue;
        }
        if fw.exists() {
            let mut fw = fw.clone();
            fw.code_format = "raw".to_string();
            fw.vars_format = "raw".to_string();
            fw.description = Path::new(&fw.code)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| fw.code.clone());
            return Ok(fw);
        }
    }

    let what = if secure_boot {
        "Secure Boot capable UEFI firmware"
    } else {
        "UEFI firmware"
    };
    bail!(
        concat!(
            "no {} found for {}/{} — install OVMF:\n",
            "  sudo pacman -S edk2-ovmf    # Arch\n",
            "  sudo apt install ovmf       # Debian/Ubuntu\n",
            "  sudo dnf install edk2-ovmf  # Fedora"
        ),
        what,
        arch,
        machine
    )
}

/// The descriptor directories, highest priority first:
/// `$XDG_CONFIG_HOME/qemu/firmware` (or `~/.config/qemu/firmware`),
/// `/etc/qemu/firmware`, `/usr/share/qemu/firmware`,
/// `/opt/homebrew/share/qemu/firmware`, `/usr/local/share/qemu/firmware`.
pub(crate) fn default_firmware_dirs() -> Vec<PathBuf> {
    let cfg_home = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        // Only $HOME counts, as with Go's os.UserHomeDir; without it the
        // directory is the relative `.config/qemu/firmware`.
        _ => std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join(".config"),
    };
    vec![
        cfg_home.join("qemu").join("firmware"),
        PathBuf::from("/etc/qemu/firmware"),
        PathBuf::from("/usr/share/qemu/firmware"),
        // Homebrew on Apple silicon
        PathBuf::from("/opt/homebrew/share/qemu/firmware"),
        // Homebrew on Intel, source installs
        PathBuf::from("/usr/local/share/qemu/firmware"),
    ]
}

/// Reads every descriptor following QEMU's rules: a file in a
/// higher-priority directory hides one of the same name below it, an empty
/// file hides without replacing, and the result is ordered by file name.
fn load_firmware_descriptors(dirs: &[PathBuf]) -> Vec<FirmwareDescriptor> {
    // Byte-wise ordering by file name, as QEMU sorts them.
    let mut by_name: BTreeMap<OsString, PathBuf> = BTreeMap::new();
    for dir in dirs.iter().rev() {
        // Lowest priority first, so a later directory wins.
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            if is_dir || !name.as_encoded_bytes().ends_with(b".json") {
                continue;
            }
            let path = dir.join(&name);
            by_name.insert(name, path);
        }
    }

    by_name
        .into_values()
        .filter_map(|path| {
            // An unreadable or empty file masks a lower descriptor and
            // contributes nothing; so does malformed JSON.
            let data = fs::read(path).ok().filter(|d| !d.is_empty())?;
            serde_json::from_slice(&data).ok()
        })
        .collect()
}

/// Enrolls Secure Boot keys into a vars template that lacks them; from the
/// virt-firmware package.
pub(crate) const ENROLL_TOOL: &str = "virt-fw-vars";

/// Gives a UEFI VM its private NVRAM store if it does not have one yet (a
/// copy of the firmware's template, run through `virt-fw-vars` for Secure
/// Boot when the template has no keys). BIOS VMs need nothing.
pub fn ensure_firmware_vars(storage: &Path, cfg: &VmConfig) -> Result<()> {
    if !cfg.uefi() {
        return Ok(());
    }
    let dst = firmware_vars_path(storage, &cfg.name);
    if dst.exists() {
        return Ok(());
    }
    let fw = find_firmware(arch_of(cfg), machine_of(cfg), cfg.secure_boot)?;
    create_firmware_vars(&fw, &dst, cfg.secure_boot)
}

/// Removes a file when dropped, whatever the outcome of the work in between.
struct RemoveOnDrop<'a>(&'a Path);

impl Drop for RemoveOnDrop<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.0);
    }
}

/// Writes a fresh NVRAM store at `dst` from the firmware's template (via
/// `<dst>.tmp`), enrolling Secure Boot keys when asked for and not already
/// present.
pub(crate) fn create_firmware_vars(fw: &Firmware, dst: &Path, secure_boot: bool) -> Result<()> {
    let tmp = {
        let mut p = dst.as_os_str().to_owned();
        p.push(".tmp");
        PathBuf::from(p)
    };
    let _cleanup = RemoveOnDrop(&tmp);

    if !secure_boot || fw.enrolled_keys {
        copy_file(Path::new(&fw.vars_template), &tmp).context("copy NVRAM template")?;
        return rename(&tmp, dst);
    }

    let Some(tool) = lookup_tool(ENROLL_TOOL) else {
        bail!(
            concat!(
                "{} not found — it enrolls the Secure Boot keys into the firmware's NVRAM. Install it:\n",
                "  sudo pacman -S virt-firmware     # Arch\n",
                "  sudo dnf install virt-firmware   # Fedora\n",
                "  pip install virt-firmware        # elsewhere\n",
                "or pick UEFI without Secure Boot"
            ),
            ENROLL_TOOL
        );
    };
    // A generated, throw-away platform key plus every Microsoft KEK and db
    // certificate (2011 and 2023 generations, UEFI CA and option ROM CA), so
    // both Windows and shim-based Linux boot. --secure-boot turns enforcement
    // on; without it OVMF would stay in setup mode.
    let mut cmd = Command::new(tool);
    cmd.arg("--input")
        .arg(&fw.vars_template)
        .arg("--output")
        .arg(&tmp)
        .args(["--enroll-generate", "ostrich"])
        .args(["--microsoft-kek", "all", "--microsoft-db", "all"])
        .arg("--secure-boot");
    let (status, out) = combined_output(cmd, None)
        .with_context(|| format!("enroll Secure Boot keys with {ENROLL_TOOL}"))?;
    if !status.success() {
        bail!(
            "enroll Secure Boot keys with {ENROLL_TOOL}: {}\n{}",
            exit_status_text(status),
            String::from_utf8_lossy(&out)
        );
    }
    rename(&tmp, dst)
}

fn rename(from: &Path, to: &Path) -> Result<()> {
    fs::rename(from, to).with_context(|| format!("rename {} to {}", from.display(), to.display()))
}

/// Copies a file, truncating `dst`, mode 0644 for a new file. The copy is
/// flushed to disk before it counts as made: a write error that a network
/// or FUSE filesystem only reports at that point (Go saw it from `Close`)
/// fails the copy as `close <dst>: <error>` instead of leaving a short file.
pub(crate) fn copy_file(src: &Path, dst: &Path) -> io::Result<()> {
    let mut input = File::open(src).map_err(|err| at(err, "open", src))?;
    let mut output = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .open(dst)
        .map_err(|err| at(err, "open", dst))?;
    io::copy(&mut input, &mut output).map_err(|err| at(err, "copy to", dst))?;
    output.sync_all().map_err(|err| at(err, "close", dst))
}

/// Puts the path into an I/O error the way Go's `*PathError` does:
/// `open /x/y: No such file or directory (os error 2)`.
fn at(err: io::Error, op: &str, path: &Path) -> io::Error {
    io::Error::new(err.kind(), format!("{op} {}: {err}", path.display()))
}

/// The QEMU target architecture of a config: `arch`, or `x86_64` when empty.
pub fn arch_of(cfg: &VmConfig) -> &str {
    if cfg.arch.is_empty() {
        "x86_64"
    } else {
        &cfg.arch
    }
}

/// The machine type: `virt` for `aarch64`/`arm64`, `q35` otherwise.
pub fn machine_of(cfg: &VmConfig) -> &'static str {
    match arch_of(cfg) {
        "aarch64" | "arm64" => "virt",
        _ => "q35",
    }
}

// --- host tools ---

/// Resolves a host tool the way Go's `exec.LookPath` does: the first
/// executable of that name on `$PATH`, or `None`. Tests may redirect it
/// through the `fixture` module.
pub(crate) fn lookup_tool(name: &str) -> Option<PathBuf> {
    match fixture_tool_dir() {
        Some(dir) => {
            which::which_in(name, Some(dir), std::env::current_dir().unwrap_or_default()).ok()
        }
        None => which::which(name).ok(),
    }
}

#[cfg(test)]
fn fixture_tool_dir() -> Option<PathBuf> {
    fixture::tool_dir()
}

#[cfg(not(test))]
fn fixture_tool_dir() -> Option<PathBuf> {
    None
}

/// The pause between checks on a running command.
const COMMAND_POLL: Duration = Duration::from_millis(10);
/// How many times a spawn that failed with `ETXTBSY` is tried again.
const SPAWN_BUSY_RETRIES: u32 = 10;
/// The pause before each of those tries.
const SPAWN_BUSY_PAUSE: Duration = Duration::from_millis(10);

/// `cmd.spawn()`, tried again up to [`SPAWN_BUSY_RETRIES`] times, 10 ms
/// apart, while it fails with `ETXTBSY` (`Text file busy`). An executable
/// that was written a moment ago fails that way when another thread forks
/// meanwhile: the forked child holds a copy of the writable descriptor until
/// it execs. That is how the stand-in tools of the tests are made; for a
/// real tool the retry never triggers.
pub(crate) fn spawn_retrying(cmd: &mut Command) -> io::Result<Child> {
    let mut retries = 0;
    loop {
        match cmd.spawn() {
            Err(err)
                if err.raw_os_error() == Some(Errno::ETXTBSY as i32)
                    && retries < SPAWN_BUSY_RETRIES =>
            {
                retries += 1;
                thread::sleep(SPAWN_BUSY_PAUSE);
            }
            result => return result,
        }
    }
}

/// Runs a command with stdin from `/dev/null` and stdout and stderr merged
/// into one captured stream, like Go's `cmd.CombinedOutput()`, and returns
/// the exit status with the output. A child that daemonises may leave the
/// pipe open in a grandchild; `pipe_grace` bounds how long after the child's
/// exit the pipe is still read (Go's `WaitDelay`), `None` reads to EOF.
pub(crate) fn combined_output(
    mut cmd: Command,
    pipe_grace: Option<Duration>,
) -> io::Result<(ExitStatus, Vec<u8>)> {
    let (mut reader, writer) = io::pipe()?;
    let stderr = writer.try_clone()?;
    let mut child = spawn_retrying(cmd.stdin(Stdio::null()).stdout(writer).stderr(stderr))?;
    // The command holds our ends of the pipe; drop them or EOF never comes.
    drop(cmd);
    let flags = OFlag::from_bits_retain(fcntl(&reader, FcntlArg::F_GETFL)?);
    fcntl(&reader, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;

    let mut out = Vec::new();
    let status = loop {
        drain(&mut reader, &mut out)?;
        if let Some(status) = child.try_wait()? {
            break status;
        }
        thread::sleep(COMMAND_POLL);
    };
    let deadline = pipe_grace.map(|grace| Instant::now() + grace);
    while !drain(&mut reader, &mut out)? {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
        thread::sleep(COMMAND_POLL);
    }
    Ok((status, out))
}

/// Reads whatever the non-blocking pipe has; true at end of stream.
fn drain(reader: &mut PipeReader, out: &mut Vec<u8>) -> io::Result<bool> {
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => return Ok(true),
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
}

/// How a process ended, in the words of Go's `os.ProcessState.String`,
/// which is what an `exec.ExitError` prints: `exit status 1`,
/// `signal: killed`, `signal: segmentation fault (core dumped)`.
pub(crate) fn exit_status_text(status: ExitStatus) -> String {
    let mut text = if let Some(code) = status.code() {
        format!("exit status {code}")
    } else if let Some(sig) = status.signal() {
        format!("signal: {}", signal_name(sig))
    } else if let Some(sig) = status.stopped_signal() {
        format!("stop signal: {}", signal_name(sig))
    } else if status.continued() {
        "continued".to_string()
    } else {
        status.to_string()
    };
    if status.core_dumped() {
        text.push_str(" (core dumped)");
    }
    text
}

/// A signal's name as Go's `syscall.Signal.String` gives it, from Go's
/// signal table for the platform; `signal <n>` for one it does not name.
fn signal_name(sig: i32) -> String {
    #[cfg(target_os = "macos")]
    const NAMES: [&str; 32] = [
        "",
        "hangup",
        "interrupt",
        "quit",
        "illegal instruction",
        "trace/BPT trap",
        "abort trap",
        "EMT trap",
        "floating point exception",
        "killed",
        "bus error",
        "segmentation fault",
        "bad system call",
        "broken pipe",
        "alarm clock",
        "terminated",
        "urgent I/O condition",
        "suspended (signal)",
        "suspended",
        "continued",
        "child exited",
        "stopped (tty input)",
        "stopped (tty output)",
        "I/O possible",
        "cputime limit exceeded",
        "filesize limit exceeded",
        "virtual timer expired",
        "profiling timer expired",
        "window size changes",
        "information request",
        "user defined signal 1",
        "user defined signal 2",
    ];
    #[cfg(not(target_os = "macos"))]
    const NAMES: [&str; 32] = [
        "",
        "hangup",
        "interrupt",
        "quit",
        "illegal instruction",
        "trace/breakpoint trap",
        "aborted",
        "bus error",
        "floating point exception",
        "killed",
        "user defined signal 1",
        "segmentation fault",
        "user defined signal 2",
        "broken pipe",
        "alarm clock",
        "terminated",
        "stack fault",
        "child exited",
        "continued",
        "stopped (signal)",
        "stopped",
        "stopped (tty input)",
        "stopped (tty output)",
        "urgent I/O condition",
        "CPU time limit exceeded",
        "file size limit exceeded",
        "virtual timer expired",
        "profiling timer expired",
        "window changed",
        "I/O possible",
        "power failure",
        "bad system call",
    ];
    match usize::try_from(sig).ok().and_then(|i| NAMES.get(i)) {
        Some(name) if !name.is_empty() => (*name).to_string(),
        _ => format!("signal {sig}"),
    }
}

/// A thread-local stand-in for the host, so tests can point the lookups at
/// fixtures the way the Go tests swapped `firmwareDirs`, `knownFirmware` and
/// `PATH`. The public entry points ([`find_firmware`],
/// [`ensure_firmware_vars`], [`lookup_tool`] and so `check_tpm`/`start_tpm`
/// and everything built on them) honour it, so other modules' tests can use
/// it too. Every test runs on its own thread, so one test's fixture never
/// leaks into another's; the [`fixture::Guard`] removes it at the end.
#[cfg(test)]
pub(crate) mod fixture {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    use super::Firmware;

    /// What the fixture replaces.
    #[derive(Debug, Clone, Default)]
    pub(crate) struct Host {
        /// Descriptor directories, highest priority first.
        pub(crate) dirs: Vec<PathBuf>,
        /// The well-known-path table, per architecture; empty disables it.
        pub(crate) known: HashMap<String, Vec<Firmware>>,
        /// The one directory host tools are looked up in instead of `$PATH`
        /// (Go: `t.Setenv("PATH", dir)`); `None` keeps the real `$PATH`.
        pub(crate) tool_dir: Option<PathBuf>,
    }

    impl Host {
        pub(crate) fn known_for(&self, arch: &str) -> &[Firmware] {
            self.known.get(arch).map_or(&[], Vec::as_slice)
        }
    }

    thread_local! {
        static HOST: RefCell<Option<Host>> = const { RefCell::new(None) };
    }

    /// Removes the fixture when dropped.
    #[must_use = "the fixture is removed as soon as the guard is dropped"]
    pub(crate) struct Guard(());

    impl Drop for Guard {
        fn drop(&mut self) {
            HOST.with(|h| h.borrow_mut().take());
        }
    }

    /// Installs a fixture with the given descriptor directories (highest
    /// priority first), no well-known paths and the real `$PATH`.
    pub(crate) fn install(dirs: Vec<PathBuf>) -> Guard {
        HOST.with(|h| {
            *h.borrow_mut() = Some(Host {
                dirs,
                ..Host::default()
            })
        });
        Guard(())
    }

    /// Changes the installed fixture.
    pub(crate) fn update(f: impl FnOnce(&mut Host)) {
        HOST.with(|h| f(h.borrow_mut().as_mut().expect("no host fixture installed")));
    }

    /// A copy of the installed fixture, if any.
    pub(crate) fn snapshot() -> Option<Host> {
        HOST.with(|h| h.borrow().clone())
    }

    /// The fixture's tool directory, if a fixture with one is installed.
    pub(crate) fn tool_dir() -> Option<PathBuf> {
        HOST.with(|h| h.borrow().as_ref().and_then(|h| h.tool_dir.clone()))
    }

    /// Polls a freshly started VM's monitor every 200 ms for up to 15 s and
    /// returns its `info qtree` output (Go: `waitForMonitor`).
    pub(crate) fn wait_for_monitor(storage: &Path, name: &str) -> String {
        use std::thread;
        use std::time::{Duration, Instant};

        use crate::vm::monitor::monitor_command;

        let deadline = Instant::now() + Duration::from_secs(15);
        let mut last = String::new();
        while Instant::now() < deadline {
            match monitor_command(storage, name, "info qtree") {
                Ok(qtree) => return qtree,
                Err(err) => last = err.to_string(),
            }
            thread::sleep(Duration::from_millis(200));
        }
        panic!("QEMU monitor of {name} did not come up: {last}");
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use tempfile::TempDir;

    use super::super::config::{vm_dir, FirmwareType};
    use super::*;

    /// Creates `n` empty descriptor directories, highest priority first.
    fn descriptor_dirs(n: usize) -> Vec<TempDir> {
        (0..n).map(|_| tempfile::tempdir().unwrap()).collect()
    }

    fn paths(dirs: &[TempDir]) -> Vec<PathBuf> {
        dirs.iter().map(|d| d.path().to_path_buf()).collect()
    }

    /// Points the lookups at `n` fresh descriptor directories and disables
    /// the well-known-path fallback (Go: `fakeFirmwareDirs`).
    fn fake_firmware_dirs(n: usize) -> (Vec<TempDir>, fixture::Guard) {
        let dirs = descriptor_dirs(n);
        let guard = fixture::install(paths(&dirs));
        (dirs, guard)
    }

    /// Writes a descriptor plus the (tiny) firmware files it names and
    /// returns their paths (Go: `writeDescriptor`).
    fn write_descriptor(
        dir: &Path,
        name: &str,
        arch: &str,
        machine_glob: &str,
        features: &[&str],
        with_files: bool,
    ) -> (String, String) {
        let code = dir.join(format!("{name}-code.fd")).display().to_string();
        let vars = dir.join(format!("{name}-vars.fd")).display().to_string();
        if with_files {
            fs::write(&code, "code").unwrap();
            fs::write(&vars, "vars-template").unwrap();
        }
        let json = serde_json::json!({
            "description": name,
            "interface-types": ["uefi"],
            "mapping": {
                "device": "flash",
                "executable": {"filename": code, "format": "raw"},
                "nvram-template": {"filename": vars, "format": "raw"},
            },
            "targets": [{"architecture": arch, "machines": [machine_glob]}],
            "features": features,
        });
        fs::write(dir.join(format!("{name}.json")), json.to_string()).unwrap();
        (code, vars)
    }

    /// An executable shell script at `dir/<name>`.
    fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn vm(name: &str) -> VmConfig {
        VmConfig {
            name: name.to_string(),
            ..VmConfig::default()
        }
    }

    #[test]
    fn find_firmware_from_descriptors() {
        let dirs = descriptor_dirs(1);
        let dir = dirs[0].path();
        let (sb_code, _) = write_descriptor(
            dir,
            "50-secure",
            "x86_64",
            "pc-q35-*",
            &["secure-boot", "requires-smm"],
            true,
        );
        let (plain_code, _) = write_descriptor(dir, "60-plain", "x86_64", "pc-q35-*", &[], true);
        write_descriptor(dir, "60-i440fx", "x86_64", "pc-i440fx-*", &[], true);
        write_descriptor(dir, "60-arm", "aarch64", "virt-*", &[], true);
        let dirs = paths(&dirs);

        let fw = find_firmware_with(&dirs, &[], "x86_64", "q35", false).unwrap();
        assert!(
            fw.code == plain_code && !fw.secure_boot && !fw.requires_smm,
            "plain UEFI should pick the non-Secure-Boot image, got {fw:?}"
        );

        let fw = find_firmware_with(&dirs, &[], "x86_64", "q35", true).unwrap();
        assert!(
            fw.code == sb_code && fw.secure_boot && fw.requires_smm && !fw.enrolled_keys,
            "Secure Boot should pick the secboot image, got {fw:?}"
        );
        assert_eq!(
            (fw.code_format.as_str(), fw.vars_format.as_str()),
            ("raw", "raw")
        );
        assert_eq!(fw.description, "50-secure");

        assert!(
            find_firmware_with(&dirs, &[], "x86_64", "pc", true).is_err(),
            "i440fx has no Secure Boot descriptor, expected an error"
        );
        let fw = find_firmware_with(&dirs, &[], "aarch64", "virt", false).unwrap();
        assert!(fw.code.contains("60-arm"), "aarch64 lookup: {fw:?}");
        let err = find_firmware_with(&dirs, &[], "riscv64", "virt", false).unwrap_err();
        assert_eq!(
            err.to_string(),
            "no UEFI firmware found for riscv64/virt — install OVMF:\n  sudo pacman -S edk2-ovmf    # Arch\n  sudo apt install ovmf       # Debian/Ubuntu\n  sudo dnf install edk2-ovmf  # Fedora"
        );
        let err = find_firmware_with(&dirs, &[], "x86_64", "pc", true).unwrap_err();
        assert!(
            err.to_string().starts_with(
                "no Secure Boot capable UEFI firmware found for x86_64/pc — install OVMF:\n"
            ),
            "{err}"
        );
    }

    #[test]
    fn find_firmware_prefers_enrolled_keys_and_skips_missing_files() {
        let dirs = descriptor_dirs(1);
        let dir = dirs[0].path();
        write_descriptor(
            dir,
            "40-secure-gone",
            "x86_64",
            "pc-q35-*",
            &["secure-boot", "enrolled-keys"],
            false,
        );
        write_descriptor(
            dir,
            "50-secure",
            "x86_64",
            "pc-q35-*",
            &["secure-boot", "requires-smm"],
            true,
        );
        let (enrolled_code, _) = write_descriptor(
            dir,
            "55-secure-enrolled",
            "x86_64",
            "pc-q35-*",
            &["secure-boot", "requires-smm", "enrolled-keys"],
            true,
        );
        let dirs = paths(&dirs);

        let fw = find_firmware_with(&dirs, &[], "x86_64", "q35", true).unwrap();
        assert!(
            fw.code == enrolled_code && fw.enrolled_keys,
            "expected the enrolled-keys firmware, got {fw:?}"
        );
        // With only Secure Boot images around, plain UEFI falls back to one
        // of them: the first.
        let fw = find_firmware_with(&dirs, &[], "x86_64", "q35", false).unwrap();
        assert!(fw.secure_boot, "plain UEFI fallback: {fw:?}");
        assert_eq!(fw.description, "50-secure");
    }

    #[test]
    fn firmware_descriptor_priority_and_masking() {
        let dirs = descriptor_dirs(2); // dirs[0] overrides dirs[1]
        let (low_code, _) =
            write_descriptor(dirs[1].path(), "60-ovmf", "x86_64", "pc-q35-*", &[], true);
        let (high_code, _) =
            write_descriptor(dirs[0].path(), "60-ovmf", "x86_64", "pc-q35-*", &[], true);
        let fw = find_firmware_with(&paths(&dirs), &[], "x86_64", "q35", false).unwrap();
        assert!(
            fw.code == high_code && fw.code != low_code,
            "higher-priority directory should win, got {}",
            fw.code
        );

        // An empty file in the top directory masks the descriptor below.
        fs::write(dirs[0].path().join("60-ovmf.json"), "").unwrap();
        assert!(
            find_firmware_with(&paths(&dirs), &[], "x86_64", "q35", false).is_err(),
            "masked descriptor still found"
        );
    }

    #[test]
    fn find_firmware_known_paths() {
        let empty = descriptor_dirs(1);
        let dir = tempfile::tempdir().unwrap();
        let code = dir
            .path()
            .join("OVMF_CODE.secboot.fd")
            .display()
            .to_string();
        let vars = dir.path().join("OVMF_VARS.fd").display().to_string();
        fs::write(&code, "c").unwrap();
        fs::write(&vars, "v").unwrap();
        let known = vec![
            Firmware {
                code: dir.path().join("missing.fd").display().to_string(),
                vars_template: vars.clone(),
                secure_boot: true,
                enrolled_keys: true,
                ..Firmware::default()
            },
            Firmware {
                code: code.clone(),
                vars_template: vars.clone(),
                secure_boot: true,
                requires_smm: true,
                ..Firmware::default()
            },
        ];
        let fw = find_firmware_with(&paths(&empty), &known, "x86_64", "q35", true).unwrap();
        assert!(
            fw.code == code && fw.requires_smm && fw.code_format == "raw",
            "got {fw:?}"
        );
        assert_eq!(fw.vars_format, "raw");
        assert_eq!(fw.description, "OVMF_CODE.secboot.fd");

        // The known-path pass does not prefer plain images: the first
        // existing entry wins even for plain UEFI.
        let fw = find_firmware_with(&paths(&empty), &known, "x86_64", "q35", false).unwrap();
        assert!(fw.secure_boot && fw.code == code);
        // An empty table means no fallback at all.
        assert!(find_firmware_with(&paths(&empty), &[], "x86_64", "q35", true).is_err());
    }

    #[test]
    fn known_firmware_table_and_default_dirs() {
        let x86 = known_firmware("x86_64");
        assert_eq!(x86.len(), 10);
        assert_eq!(x86[0].code, "/usr/share/edk2/ovmf/OVMF_CODE.secboot.fd");
        assert!(x86[0].secure_boot && x86[0].enrolled_keys && x86[0].requires_smm);
        assert_eq!(x86[4].code, "/usr/share/edk2/x64/OVMF_CODE.secboot.4m.fd");
        assert!(x86[4].secure_boot && !x86[4].enrolled_keys && x86[4].requires_smm);
        assert_eq!(
            x86[9].vars_template,
            "/opt/homebrew/share/qemu/edk2-i386-vars.fd"
        );
        assert!(x86
            .iter()
            .all(|fw| fw.code_format.is_empty() && fw.description.is_empty()));
        let arm = known_firmware("aarch64");
        assert_eq!(arm.len(), 6);
        assert!(arm[0].secure_boot && arm[0].enrolled_keys && !arm[0].requires_smm);
        assert_eq!(arm[5].code, "/opt/homebrew/share/qemu/edk2-aarch64-code.fd");
        assert!(known_firmware("arm64").is_empty());
        assert!(known_firmware("riscv64").is_empty());

        let dirs = default_firmware_dirs();
        assert_eq!(dirs.len(), 5);
        assert!(dirs[0].ends_with("qemu/firmware"), "{:?}", dirs[0]);
        assert_eq!(
            &dirs[1..],
            &[
                PathBuf::from("/etc/qemu/firmware"),
                PathBuf::from("/usr/share/qemu/firmware"),
                PathBuf::from("/opt/homebrew/share/qemu/firmware"),
                PathBuf::from("/usr/local/share/qemu/firmware"),
            ]
        );
    }

    #[test]
    fn descriptor_rules() {
        let dirs = descriptor_dirs(1);
        let dir = dirs[0].path();
        let code = dir.join("code.fd").display().to_string();
        let vars = dir.join("vars.fd").display().to_string();
        fs::write(&code, "c").unwrap();
        fs::write(&vars, "v").unwrap();
        let descriptor = |name: &str, patch: serde_json::Value| {
            let mut d = serde_json::json!({
                "description": name,
                "interface-types": ["uefi"],
                "mapping": {
                    "device": "flash",
                    "executable": {"filename": code},
                    "nvram-template": {"filename": vars},
                },
                "targets": [{"architecture": "x86_64", "machines": ["pc-q35-*"]}],
            });
            for (k, v) in patch.as_object().unwrap() {
                if k.contains('.') {
                    let (outer, inner) = k.split_once('.').unwrap();
                    d[outer][inner] = v.clone();
                } else {
                    d[k] = v.clone();
                }
            }
            fs::write(dir.join(format!("{name}.json")), d.to_string()).unwrap();
        };
        let found = |arch: &str, machine: &str| {
            find_firmware_with(&paths(&dirs), &[], arch, machine, false)
                .map(|fw| fw.description)
                .ok()
        };

        // Formats default to raw; the files sort by name, so "10-ok" wins.
        descriptor("10-ok", serde_json::json!({}));
        let fw = find_firmware_with(&paths(&dirs), &[], "x86_64", "q35", false).unwrap();
        assert_eq!(
            (fw.code_format.as_str(), fw.vars_format.as_str()),
            ("raw", "raw")
        );
        assert!(!fw.secure_boot && !fw.enrolled_keys && !fw.requires_smm);
        fs::remove_file(dir.join("10-ok.json")).unwrap();

        descriptor(
            "20-combined",
            serde_json::json!({"mapping.mode": "combined"}),
        );
        descriptor(
            "21-stateless",
            serde_json::json!({"mapping.mode": "stateless"}),
        );
        descriptor("22-bios", serde_json::json!({"interface-types": ["bios"]}));
        descriptor("23-memory", serde_json::json!({"mapping.device": "memory"}));
        descriptor(
            "24-no-vars",
            serde_json::json!({"mapping.nvram-template": {"filename": ""}}),
        );
        descriptor(
            "25-other-arch",
            serde_json::json!({"targets": [{"architecture": "i386", "machines": ["pc-q35-*"]}]}),
        );
        descriptor(
            "26-bad-glob",
            serde_json::json!({"targets": [{"architecture": "x86_64", "machines": ["pc-q35-["]}]}),
        );
        fs::write(dir.join("27-garbage.json"), "{not json").unwrap();
        fs::write(dir.join("28-array.json"), "[]").unwrap();
        fs::write(dir.join("29-not-descriptor.txt"), "{}").unwrap();
        fs::create_dir(dir.join("30-dir.json")).unwrap();
        assert_eq!(found("x86_64", "q35"), None);

        // Accepted spellings: an explicit split mode, the literal alias, the
        // canonical prefix, and the arm and i440fx names.
        descriptor("40-split", serde_json::json!({"mapping.mode": "split"}));
        assert_eq!(found("x86_64", "q35").as_deref(), Some("40-split"));
        assert_eq!(found("x86_64", "pc-q35-9.0"), Some("40-split".to_string()));
        assert_eq!(found("x86_64", "pc"), None);
        descriptor(
            "41-literal",
            serde_json::json!({"targets": [{"architecture": "x86_64", "machines": ["q35", "pc"]}]}),
        );
        assert_eq!(found("x86_64", "pc").as_deref(), Some("41-literal"));
        descriptor(
            "42-i440fx",
            serde_json::json!({"targets": [{"architecture": "x86_64", "machines": ["pc-i440fx-*"]}]}),
        );
        descriptor(
            "43-virt",
            serde_json::json!({"targets": [{"architecture": "aarch64", "machines": ["virt-*"]}]}),
        );
        assert_eq!(found("aarch64", "virt").as_deref(), Some("43-virt"));
        assert_eq!(found("aarch64", "virt-9.0").as_deref(), Some("43-virt"));
        assert_eq!(found("aarch64", "raspi3b"), None);
        fs::remove_file(dir.join("41-literal.json")).unwrap();
        assert_eq!(found("x86_64", "pc").as_deref(), Some("42-i440fx"));

        assert_eq!(machine_names("q35"), ["q35", "pc-q35-"]);
        assert_eq!(machine_names("pc"), ["pc", "pc-i440fx-"]);
        assert_eq!(machine_names("virt"), ["virt", "virt-"]);
        assert_eq!(machine_names("microvm"), ["microvm"]);
    }

    #[test]
    fn fixture_redirects_the_public_lookup() {
        let (dirs, _guard) = fake_firmware_dirs(1);
        let (code, vars) =
            write_descriptor(dirs[0].path(), "60-plain", "x86_64", "pc-q35-*", &[], true);
        assert_eq!(find_firmware("x86_64", "q35", false).unwrap().code, code);
        assert!(find_firmware("x86_64", "q35", true).is_err());

        fixture::update(|h| {
            h.known.insert(
                "x86_64".to_string(),
                vec![Firmware {
                    code: code.clone(),
                    vars_template: vars,
                    secure_boot: true,
                    ..Firmware::default()
                }],
            );
        });
        let fw = find_firmware("x86_64", "q35", true).unwrap();
        assert!(
            fw.secure_boot && fw.description == "60-plain-code.fd",
            "{fw:?}"
        );
        assert!(find_firmware("aarch64", "virt", false).is_err());
    }

    #[test]
    fn ensure_firmware_vars_copies_enrolls_and_keeps() {
        let (dirs, _guard) = fake_firmware_dirs(1);
        let (_, tmpl) = write_descriptor(
            dirs[0].path(),
            "50-secure",
            "x86_64",
            "pc-q35-*",
            &["secure-boot", "requires-smm"],
            true,
        );
        let storage = tempfile::tempdir().unwrap();
        let storage = storage.path();
        fs::create_dir_all(vm_dir(storage, "vm")).unwrap();
        let vars = firmware_vars_path(storage, "vm");

        let bios = vm("vm");
        ensure_firmware_vars(storage, &bios).unwrap();
        assert!(!vars.exists(), "BIOS VM should not get an NVRAM store");

        let uefi = VmConfig {
            firmware: FirmwareType::Uefi,
            ..vm("vm")
        };
        ensure_firmware_vars(storage, &uefi).unwrap();
        assert_eq!(fs::read_to_string(&vars).unwrap(), "vars-template");
        // An existing store is left alone.
        fs::write(&vars, "guest-state").unwrap();
        ensure_firmware_vars(storage, &uefi).unwrap();
        assert_eq!(fs::read_to_string(&vars).unwrap(), "guest-state");
        fs::remove_file(&vars).unwrap();

        // Secure Boot without enrolled keys needs virt-fw-vars: fail clearly
        // without it...
        let bin = tempfile::tempdir().unwrap();
        fixture::update(|h| h.tool_dir = Some(bin.path().to_path_buf()));
        let sb = VmConfig {
            secure_boot: true,
            ..vm("vm")
        };
        let err = ensure_firmware_vars(storage, &sb).unwrap_err();
        assert_eq!(
            err.to_string(),
            "virt-fw-vars not found — it enrolls the Secure Boot keys into the firmware's NVRAM. Install it:\n  sudo pacman -S virt-firmware     # Arch\n  sudo dnf install virt-firmware   # Fedora\n  pip install virt-firmware        # elsewhere\nor pick UEFI without Secure Boot"
        );
        assert!(!vars.exists());
        let tmp = PathBuf::from(format!("{}.tmp", vars.display()));
        assert!(!tmp.exists(), "temp file left behind");

        // ...and call it with the template and the enrolment flags when
        // present.
        let args = bin.path().join("args");
        write_script(
            bin.path(),
            ENROLL_TOOL,
            &format!(
                "echo \"$@\" > {}\nwhile [ $# -gt 0 ]; do [ \"$1\" = --output ] && printf enrolled > \"$2\"; shift; done\n",
                args.display()
            ),
        );
        ensure_firmware_vars(storage, &sb).unwrap();
        assert_eq!(fs::read_to_string(&vars).unwrap(), "enrolled");
        assert!(!tmp.exists());
        let got = fs::read_to_string(&args).unwrap();
        assert_eq!(
            got.trim_end(),
            format!(
                "--input {tmpl} --output {} --enroll-generate ostrich --microsoft-kek all --microsoft-db all --secure-boot",
                tmp.display()
            )
        );

        // A template that already has the keys is simply copied.
        fs::remove_file(&vars).unwrap();
        fs::remove_file(&args).unwrap();
        write_descriptor(
            dirs[0].path(),
            "40-enrolled",
            "x86_64",
            "pc-q35-*",
            &["secure-boot", "enrolled-keys"],
            true,
        );
        ensure_firmware_vars(storage, &sb).unwrap();
        assert!(
            !args.exists(),
            "enrolled template should not go through the enroll tool"
        );
        assert_eq!(fs::read_to_string(&vars).unwrap(), "vars-template");
    }

    #[test]
    fn ensure_firmware_vars_needs_the_vm_directory() {
        let (dirs, _guard) = fake_firmware_dirs(1);
        write_descriptor(dirs[0].path(), "60-plain", "x86_64", "pc-q35-*", &[], true);
        let storage = tempfile::tempdir().unwrap();
        let uefi = VmConfig {
            firmware: FirmwareType::Uefi,
            ..vm("nodir")
        };
        let err = ensure_firmware_vars(storage.path(), &uefi).unwrap_err();
        let text = format!("{err:#}");
        let tmp = format!(
            "{}.tmp",
            firmware_vars_path(storage.path(), "nodir").display()
        );
        assert!(
            text.starts_with(&format!("copy NVRAM template: open {tmp}: ")),
            "{text}"
        );
        assert!(!firmware_vars_path(storage.path(), "nodir").exists());
    }

    #[test]
    fn create_firmware_vars_reports_the_tool_failure() {
        let bin = tempfile::tempdir().unwrap();
        let _guard = fixture::install(Vec::new());
        fixture::update(|h| h.tool_dir = Some(bin.path().to_path_buf()));
        write_script(
            bin.path(),
            ENROLL_TOOL,
            "echo nope\necho really >&2\nexit 3\n",
        );
        let dir = tempfile::tempdir().unwrap();
        let tmpl = dir.path().join("vars.fd");
        fs::write(&tmpl, "vars-template").unwrap();
        let fw = Firmware {
            vars_template: tmpl.display().to_string(),
            secure_boot: true,
            ..Firmware::default()
        };
        let dst = dir.path().join("efivars.fd");
        let err = create_firmware_vars(&fw, &dst, true).unwrap_err();
        assert_eq!(
            err.to_string(),
            "enroll Secure Boot keys with virt-fw-vars: exit status 3\nnope\nreally\n"
        );
        assert!(!dst.exists());
        assert!(
            !dir.path().join("efivars.fd.tmp").exists(),
            "temp file left behind"
        );

        // Plain copies do not need the tool, even with Secure Boot, when the
        // template already carries the keys.
        let enrolled = Firmware {
            enrolled_keys: true,
            ..fw.clone()
        };
        create_firmware_vars(&enrolled, &dst, true).unwrap();
        assert_eq!(fs::read_to_string(&dst).unwrap(), "vars-template");
    }

    #[test]
    fn copy_file_overwrites_and_names_paths() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let dst = dir.path().join("dst");
        fs::write(&src, "new").unwrap();
        fs::write(&dst, "old and longer").unwrap();
        copy_file(&src, &dst).unwrap();
        assert_eq!(fs::read_to_string(&dst).unwrap(), "new");

        let missing = dir.path().join("missing");
        let err = copy_file(&missing, &dst).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(
            err.to_string()
                .starts_with(&format!("open {}: ", missing.display())),
            "{err}"
        );
        let nodir = dir.path().join("no/such/dir");
        let err = copy_file(&src, &nodir).unwrap_err();
        assert!(
            err.to_string()
                .starts_with(&format!("open {}: ", nodir.display())),
            "{err}"
        );
    }

    /// A copy that cannot be flushed to its destination is an error, as Go's
    /// `Close` error was, not a silent short file. `/dev/null` takes every
    /// write but refuses fsync (EINVAL), which stands in for a network
    /// filesystem that reports a failed write-back only then.
    #[test]
    #[cfg(target_os = "linux")]
    fn copy_file_reports_a_failed_flush() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::write(&src, "nvram").unwrap();
        let err = copy_file(&src, Path::new("/dev/null")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().starts_with("close /dev/null: "), "{err}");
    }

    #[test]
    fn arch_and_machine_of() {
        let cfg = vm("a");
        assert_eq!((arch_of(&cfg), machine_of(&cfg)), ("x86_64", "q35"));
        for (arch, machine) in [
            ("x86_64", "q35"),
            ("aarch64", "virt"),
            ("arm64", "virt"),
            ("riscv64", "q35"),
            ("i386", "q35"),
        ] {
            let cfg = VmConfig {
                arch: arch.to_string(),
                ..vm("a")
            };
            assert_eq!((arch_of(&cfg), machine_of(&cfg)), (arch, machine));
        }
    }

    /// Go's `ProcessState.String` wording, signal names from Go's table.
    #[test]
    fn exit_status_texts() {
        let ok = Command::new("true").status().unwrap();
        assert_eq!(exit_status_text(ok), "exit status 0");
        let failed = Command::new("sh").args(["-c", "exit 7"]).status().unwrap();
        assert_eq!(exit_status_text(failed), "exit status 7");
        let killed = Command::new("sh")
            .args(["-c", "kill -9 $$"])
            .status()
            .unwrap();
        assert_eq!(exit_status_text(killed), "signal: killed");

        // Raw wait statuses: the exit code in the second byte, a fatal
        // signal in the low seven bits with 0x80 for a core dump, 0x7f
        // under the signal for a stop, 0xffff for a continue.
        #[cfg(not(target_os = "macos"))]
        let cases = [
            (3 << 8, "exit status 3"),
            (255 << 8, "exit status 255"),
            (1, "signal: hangup"),
            (2, "signal: interrupt"),
            (6, "signal: aborted"),
            (9, "signal: killed"),
            (11, "signal: segmentation fault"),
            (11 | 0x80, "signal: segmentation fault (core dumped)"),
            (6 | 0x80, "signal: aborted (core dumped)"),
            (13, "signal: broken pipe"),
            (15, "signal: terminated"),
            (31, "signal: bad system call"),
            (34, "signal: signal 34"),
            (19 << 8 | 0x7f, "stop signal: stopped (signal)"),
            (0xffff, "continued"),
        ];
        #[cfg(target_os = "macos")]
        let cases = [
            (3 << 8, "exit status 3"),
            (6, "signal: abort trap"),
            (9, "signal: killed"),
            (10, "signal: bus error"),
            (11 | 0x80, "signal: segmentation fault (core dumped)"),
            (15, "signal: terminated"),
        ];
        for (raw, want) in cases {
            assert_eq!(
                exit_status_text(ExitStatus::from_raw(raw)),
                want,
                "wait status {raw:#x}"
            );
        }
    }

    /// A spawn that hits ETXTBSY, a freshly written executable whose
    /// writable descriptor a concurrently forked child still holds, is
    /// tried again; any other spawn error is reported at once.
    #[test]
    fn spawn_retrying_waits_out_a_busy_executable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool");
        // Hold the file open for writing, as the forked child would, and
        // let go of it after a while.
        let writer = File::create(&path).unwrap();
        fs::write(&path, "#!/bin/sh\nexit 4\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let err = Command::new(&path).spawn().unwrap_err();
        assert_eq!(err.raw_os_error(), Some(Errno::ETXTBSY as i32), "{err}");
        let release = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            drop(writer);
        });
        let status = spawn_retrying(&mut Command::new(&path))
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(status.code(), Some(4));
        release.join().unwrap();

        // Busy for longer than the retries last: the error comes back.
        let _writer = File::options().write(true).open(&path).unwrap();
        let started = Instant::now();
        let err = spawn_retrying(&mut Command::new(&path)).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(Errno::ETXTBSY as i32), "{err}");
        assert!(
            started.elapsed() >= SPAWN_BUSY_PAUSE * SPAWN_BUSY_RETRIES,
            "gave up after {:?}",
            started.elapsed()
        );
        let err = spawn_retrying(&mut Command::new(dir.path().join("none"))).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn combined_output_merges_streams_and_bounds_the_pipe_wait() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo out; echo err >&2; printf tail; exit 5"]);
        let (status, out) = combined_output(cmd, None).unwrap();
        assert_eq!(status.code(), Some(5));
        assert_eq!(out, b"out\nerr\ntail");

        // A grandchild keeps the pipe open past the child's exit; the grace
        // period bounds how long that is waited for.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo started; sleep 3 & exit 0"]);
        let started = Instant::now();
        let (status, out) = combined_output(cmd, Some(Duration::from_millis(300))).unwrap();
        let elapsed = started.elapsed();
        assert!(status.success());
        assert_eq!(out, b"started\n");
        assert!(
            elapsed >= Duration::from_millis(250) && elapsed < Duration::from_secs(2),
            "{elapsed:?}"
        );
    }

    #[test]
    fn firmware_label() {
        for (cfg, want) in [
            (vm("a"), "BIOS"),
            (
                VmConfig {
                    firmware: FirmwareType::Bios,
                    tpm: true,
                    ..vm("a")
                },
                "BIOS, TPM 2.0",
            ),
            (
                VmConfig {
                    firmware: FirmwareType::Uefi,
                    ..vm("a")
                },
                "UEFI",
            ),
            (
                VmConfig {
                    firmware: FirmwareType::Uefi,
                    secure_boot: true,
                    ..vm("a")
                },
                "UEFI + Secure Boot",
            ),
            (
                VmConfig {
                    secure_boot: true,
                    tpm: true,
                    ..vm("a")
                },
                "UEFI + Secure Boot, TPM 2.0",
            ),
        ] {
            assert_eq!(cfg.firmware_label(), want, "{cfg:?}");
            if cfg.secure_boot {
                assert!(cfg.uefi(), "{cfg:?}: Secure Boot should imply UEFI");
            }
        }
    }

    /// Enrols the keys into this host's real OVMF template with the real
    /// virt-fw-vars; the store must stay the template's size for pflash.
    #[test]
    fn secure_boot_enrolment_with_host_firmware() {
        if which::which(ENROLL_TOOL).is_err() {
            eprintln!("skipping: {ENROLL_TOOL} not installed");
            return;
        }
        let fw = match find_firmware("x86_64", "q35", true) {
            Ok(fw) => fw,
            Err(err) => {
                eprintln!("skipping: {err}");
                return;
            }
        };
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("efivars.fd");
        create_firmware_vars(&fw, &dst, true).unwrap();
        assert!(!dir.path().join("efivars.fd.tmp").exists());
        let store = fs::metadata(&dst).unwrap().len();
        let tmpl = fs::metadata(&fw.vars_template).unwrap().len();
        if fw.vars_format == "raw" {
            assert_eq!(
                store, tmpl,
                "NVRAM store and template differ in size — pflash needs them equal"
            );
        }
        if !fw.enrolled_keys {
            let print = Command::new(ENROLL_TOOL)
                .arg("--input")
                .arg(&dst)
                .arg("--print")
                .output()
                .unwrap();
            let text = String::from_utf8_lossy(&print.stdout);
            assert!(print.status.success(), "{text}");
            for var in ["PK", "KEK", "db", "SecureBootEnable"] {
                assert!(
                    text.lines().any(|l| l.starts_with(var)),
                    "{var} missing:\n{text}"
                );
            }
        }
    }

    #[test]
    fn build_qemu_args_firmware_and_tpm() {
        use super::super::config::tpm_sock_path;
        use super::super::process::build_qemu_args;

        let (dirs, _guard) = fake_firmware_dirs(1);
        let (sb_code, _) = write_descriptor(
            dirs[0].path(),
            "50-secure",
            "x86_64",
            "pc-q35-*",
            &["secure-boot", "requires-smm"],
            true,
        );
        let (plain_code, _) =
            write_descriptor(dirs[0].path(), "60-plain", "x86_64", "pc-q35-*", &[], true);
        let storage = tempfile::tempdir().unwrap();
        let storage = storage.path();
        let base = VmConfig {
            cpu: 1,
            ram: 128,
            ..vm("t")
        };

        let (_, args) = build_qemu_args(&base, storage).unwrap();
        let joined = args.join(" ");
        assert!(
            !joined.contains("pflash") && !joined.contains("smm=on") && !joined.contains("tpm"),
            "BIOS VM got firmware/TPM args: {joined}"
        );

        let uefi = VmConfig {
            firmware: FirmwareType::Uefi,
            ..base.clone()
        };
        let (_, args) = build_qemu_args(&uefi, storage).unwrap();
        let joined = args.join(" ");
        assert!(
            joined.contains("-machine q35 ") && !joined.contains("smm=on"),
            "plain UEFI should not enable SMM: {joined}"
        );
        assert!(
            joined.contains(&format!(
                "-drive if=pflash,format=raw,unit=0,readonly=on,file={plain_code}"
            )) && joined.contains(&format!(
                "-drive if=pflash,format=raw,unit=1,file={}",
                firmware_vars_path(storage, "t").display()
            )),
            "pflash drives missing: {joined}"
        );

        let sb = VmConfig {
            secure_boot: true,
            tpm: true,
            ..base.clone()
        };
        let (_, args) = build_qemu_args(&sb, storage).unwrap();
        let joined = args.join(" ");
        for want in [
            "-machine q35,smm=on".to_string(),
            "-global driver=cfi.pflash01,property=secure,value=on".to_string(),
            format!("unit=0,readonly=on,file={sb_code}"),
            format!(
                "-chardev socket,id=chrtpm,path={}",
                tpm_sock_path(storage, "t").display()
            ),
            "-tpmdev emulator,id=tpm0,chardev=chrtpm".to_string(),
            "-device tpm-tis,tpmdev=tpm0".to_string(),
        ] {
            assert!(joined.contains(&want), "missing {want:?}: {joined}");
        }

        fs::remove_file(&sb_code).unwrap();
        let sb = VmConfig {
            secure_boot: true,
            ..base
        };
        assert!(
            build_qemu_args(&sb, storage).is_err(),
            "missing firmware should fail"
        );
    }

    #[test]
    fn manager_update_firmware() {
        use super::super::config::{load_config, NetworkConfig, NetworkType};
        use super::super::manager::Manager;
        use super::super::tpm::TPM_BIN;

        let (dirs, _guard) = fake_firmware_dirs(1);
        write_descriptor(
            dirs[0].path(),
            "50-secure",
            "x86_64",
            "pc-q35-*",
            &["secure-boot", "requires-smm", "enrolled-keys"],
            true,
        );
        write_descriptor(dirs[0].path(), "60-plain", "x86_64", "pc-q35-*", &[], true);
        if which::which("qemu-img").is_err() {
            eprintln!("skipping: qemu-img not installed");
            return;
        }
        let storage = tempfile::tempdir().unwrap();
        let storage = storage.path();
        let m = Manager::new(storage);
        let mut cfg = VmConfig {
            cpu: 1,
            ram: 128,
            disk_size: 1,
            network: NetworkConfig {
                kind: NetworkType::None,
                ..NetworkConfig::default()
            },
            ..vm("fw")
        };
        m.create(&mut cfg).unwrap();
        let vars = firmware_vars_path(storage, "fw");
        assert!(!vars.exists(), "BIOS VM should have no NVRAM");

        // BIOS → UEFI creates the store.
        let mut edited = cfg.clone();
        edited.firmware = FirmwareType::Uefi;
        m.update("fw", &mut edited).unwrap();
        assert!(vars.exists(), "UEFI VM should have an NVRAM store");
        fs::write(&vars, "guest-state").unwrap();

        // UEFI → UEFI + Secure Boot rebuilds it with keys.
        edited.secure_boot = true;
        m.update("fw", &mut edited).unwrap();
        assert_eq!(
            fs::read_to_string(&vars).unwrap(),
            "vars-template",
            "enabling Secure Boot should rebuild NVRAM"
        );
        fs::write(&vars, "guest-state").unwrap();

        // Turning Secure Boot off keeps it, and so does going back to BIOS.
        edited.secure_boot = false;
        m.update("fw", &mut edited).unwrap();
        edited.firmware = FirmwareType::Bios;
        m.update("fw", &mut edited).unwrap();
        assert_eq!(
            fs::read_to_string(&vars).unwrap(),
            "guest-state",
            "NVRAM should survive disabling Secure Boot/UEFI"
        );
        let saved = load_config(storage, "fw").unwrap();
        assert!(
            saved.firmware == FirmwareType::Bios && !saved.secure_boot,
            "{saved:?}"
        );

        // TPM needs swtpm on the host.
        let empty = tempfile::tempdir().unwrap();
        fixture::update(|h| h.tool_dir = Some(empty.path().to_path_buf()));
        edited.tpm = true;
        let err = m.update("fw", &mut edited).unwrap_err();
        assert!(
            err.to_string().contains(TPM_BIN),
            "expected a {TPM_BIN} hint, got {err}"
        );
    }

    /// Starts VMs on the host's real OVMF and checks QEMU got the firmware
    /// image and the VM's own NVRAM copy on its pflash units. The Secure
    /// Boot case runs when this host can enroll the keys (a template that
    /// has them, or virt-fw-vars). Skipped without QEMU or UEFI firmware.
    #[test]
    fn uefi_boot_with_qemu() {
        use super::super::config::{NetworkConfig, NetworkType};
        use super::super::manager::Manager;
        use super::super::monitor::monitor_command;
        use super::super::process::{start, stop};

        for bin in ["qemu-system-x86_64", "qemu-img"] {
            if which::which(bin).is_err() {
                eprintln!("skipping: {bin} not installed");
                return;
            }
        }
        if let Err(err) = find_firmware("x86_64", "q35", false) {
            eprintln!("skipping: {err}");
            return;
        }

        /// Stops the VM when the test ends, however it ends.
        struct StopOnDrop<'a>(&'a Path, &'a str);
        impl Drop for StopOnDrop<'_> {
            fn drop(&mut self) {
                let _ = stop(self.0, self.1);
            }
        }

        for secure_boot in [false, true] {
            let name = if secure_boot { "secureboot" } else { "uefi" };
            let fw = match find_firmware("x86_64", "q35", secure_boot) {
                Ok(fw) => fw,
                Err(err) => {
                    eprintln!("skipping {name}: {err}");
                    continue;
                }
            };
            if secure_boot && !fw.enrolled_keys && which::which(ENROLL_TOOL).is_err() {
                eprintln!("skipping {name}: {ENROLL_TOOL} not installed");
                continue;
            }

            let storage = tempfile::tempdir().unwrap();
            let storage = storage.path();
            let mut cfg = VmConfig {
                cpu: 1,
                ram: 512,
                disk_size: 1,
                firmware: FirmwareType::Uefi,
                secure_boot,
                network: NetworkConfig {
                    kind: NetworkType::None,
                    ..NetworkConfig::default()
                },
                ..vm(name)
            };
            Manager::new(storage).create(&mut cfg).unwrap();
            let vars = firmware_vars_path(storage, name);
            let store = fs::metadata(&vars).expect("NVRAM store not created").len();
            if let Ok(tmpl) = fs::metadata(&fw.vars_template) {
                if fw.vars_format == "raw" {
                    assert_eq!(
                        store,
                        tmpl.len(),
                        "NVRAM store is {store} bytes, template {} — pflash needs them equal",
                        tmpl.len()
                    );
                }
            }

            start(storage, &cfg).unwrap();
            let _stop = StopOnDrop(storage, name);
            fixture::wait_for_monitor(storage, name);

            let block = monitor_command(storage, name, "info block").unwrap_or_default();
            for want in [fw.code.as_str(), &vars.display().to_string()] {
                assert!(
                    block.contains(want),
                    "pflash image {want} not attached:\n{block}"
                );
            }
            stop(storage, name).unwrap();
        }
    }
}
