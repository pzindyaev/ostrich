//! The `vm.yaml` schema and the paths of everything in a VM's directory.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use nix::errno::Errno;
use serde::de::{self, Deserializer, Unexpected, Visitor};
use serde::{Deserialize, Serialize};

use super::disk::{Disk, PRIMARY_DISK_NAME};
use super::usb::UsbDevice;
use super::usbimage::UsbImage;

/// The networking backend of a VM.
///
/// In `vm.yaml` and `template.yaml`, `tap` and `none` are those types and
/// anything else is [`NetworkType::User`]: a missing key, a null, `""`,
/// `user`, an unknown word or another scalar. Go gave a VM whose type it did
/// not know QEMU's default NIC, which is user networking, and its forms and
/// templates took such a type for user too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkType {
    /// SLIRP / NAT: works without host privileges.
    #[default]
    User,
    /// Bridged tap through the setuid `qemu-bridge-helper`.
    Tap,
    /// No network interface at all.
    None,
}

impl<'de> Deserialize<'de> for NetworkType {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct Kind;

        impl Visitor<'_> for Kind {
            type Value = NetworkType;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a network type")
            }

            fn visit_str<E: de::Error>(self, s: &str) -> std::result::Result<NetworkType, E> {
                Ok(match s {
                    "tap" => NetworkType::Tap,
                    "none" => NetworkType::None,
                    _ => NetworkType::User,
                })
            }

            fn visit_bool<E: de::Error>(self, _: bool) -> std::result::Result<NetworkType, E> {
                Ok(NetworkType::User)
            }

            fn visit_i64<E: de::Error>(self, _: i64) -> std::result::Result<NetworkType, E> {
                Ok(NetworkType::User)
            }

            fn visit_u64<E: de::Error>(self, _: u64) -> std::result::Result<NetworkType, E> {
                Ok(NetworkType::User)
            }

            fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<NetworkType, E> {
                Ok(NetworkType::User)
            }

            fn visit_unit<E: de::Error>(self) -> std::result::Result<NetworkType, E> {
                Ok(NetworkType::User)
            }

            fn visit_none<E: de::Error>(self) -> std::result::Result<NetworkType, E> {
                Ok(NetworkType::User)
            }
        }

        d.deserialize_any(Kind)
    }
}

impl NetworkType {
    /// The YAML spelling: `user`, `tap` or `none`.
    pub fn as_str(self) -> &'static str {
        match self {
            NetworkType::User => "user",
            NetworkType::Tap => "tap",
            NetworkType::None => "none",
        }
    }

    /// All types, in the order the forms offer them.
    pub const ALL: [NetworkType; 3] = [NetworkType::User, NetworkType::Tap, NetworkType::None];
}

impl fmt::Display for NetworkType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The guest firmware.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FirmwareType {
    /// edk2 / OVMF booted from pflash.
    Uefi,
    /// SeaBIOS, QEMU's default. Also what a missing or unknown value means.
    #[default]
    #[serde(other)]
    Bios,
}

impl FirmwareType {
    /// The YAML spelling: `bios` or `uefi`.
    pub fn as_str(self) -> &'static str {
        match self {
            FirmwareType::Bios => "bios",
            FirmwareType::Uefi => "uefi",
        }
    }
}

impl fmt::Display for FirmwareType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One host→guest port mapping (user networking only).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortForward {
    #[serde(default, deserialize_with = "null_as_default")]
    pub host: u16,
    #[serde(default, deserialize_with = "null_as_default")]
    pub guest: u16,
    /// `tcp` or `udp`; an empty string means `tcp`.
    #[serde(default, deserialize_with = "null_as_default")]
    pub proto: String,
}

impl PortForward {
    /// The protocol with the default applied.
    pub fn proto(&self) -> &str {
        if self.proto.is_empty() {
            "tcp"
        } else {
            &self.proto
        }
    }
}

/// Parses a comma-separated list of `[proto:]host:guest` entries, e.g.
/// `2222:22, udp:5353:53`. Proto defaults to tcp. Blank entries are skipped.
pub fn parse_port_forwards(s: &str) -> Result<Vec<PortForward>> {
    let mut fwds = Vec::new();
    for raw in s.split(',') {
        let entry = raw.trim();
        if entry.is_empty() {
            continue;
        }
        let mut parts: Vec<&str> = entry.split(':').collect();
        let mut proto = "tcp".to_string();
        if parts.len() == 3 {
            proto = parts[0].to_lowercase();
            parts.remove(0);
        }
        if parts.len() != 2 || (proto != "tcp" && proto != "udp") {
            bail!("invalid port forward {entry:?} — expected [tcp|udp:]host:guest");
        }
        let port = |p: &str| -> Option<u16> {
            let n: i64 = p.parse().ok()?;
            if (1..=65535).contains(&n) {
                Some(n as u16)
            } else {
                None
            }
        };
        match (port(parts[0]), port(parts[1])) {
            (Some(host), Some(guest)) => fwds.push(PortForward { host, guest, proto }),
            _ => bail!("invalid port forward {entry:?} — ports must be 1–65535"),
        }
    }
    Ok(fwds)
}

/// The inverse of [`parse_port_forwards`]: `tcp:2222:22, udp:5353:53`.
pub fn format_port_forwards(fwds: &[PortForward]) -> String {
    fwds.iter()
        .map(|pf| format!("{}:{}:{}", pf.proto(), pf.host, pf.guest))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Checks that `s` is a 48-bit MAC address: six hex octets separated by
/// colons or hyphens, or three groups of four hex digits separated by dots.
pub fn validate_mac(s: &str) -> Result<()> {
    if parse_mac(s).is_some() {
        Ok(())
    } else {
        bail!("invalid MAC address {s:?} — expected e.g. 52:54:00:12:34:56")
    }
}

/// Parses a MAC address in any of the forms [`validate_mac`] accepts into its
/// six octets, the way Go's `net.ParseMAC` reads a 48-bit one: every group
/// is ASCII hex digits only — two per group between colons or hyphens, four
/// between dots — so anything else, a sign or a multi-byte character
/// included, is simply not a MAC.
pub fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let (sep, width) = if s.contains(':') {
        (':', 2)
    } else if s.contains('-') {
        ('-', 2)
    } else if s.contains('.') {
        ('.', 4)
    } else {
        return None;
    };
    let groups: Vec<&[u8]> = s.split(sep).map(str::as_bytes).collect();
    if groups.len() * width != 12 {
        return None;
    }
    let nibble = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let mut bytes = [0u8; 6];
    let mut octets = bytes.iter_mut();
    for g in groups {
        if g.len() != width {
            return None;
        }
        for pair in g.chunks(2) {
            *octets.next()? = nibble(pair[0])? << 4 | nibble(pair[1])?;
        }
    }
    Some(bytes)
}

/// A MAC in the canonical `aa:bb:cc:dd:ee:ff` spelling, or `None` when `s`
/// is not one.
pub fn normalize_mac(s: &str) -> Option<String> {
    parse_mac(s).map(|b| {
        format!(
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            b[0], b[1], b[2], b[3], b[4], b[5]
        )
    })
}

/// Networking parameters of a VM.
///
/// [`NetworkConfig::default`] is user networking, what the forms start
/// from, and so is a `vm.yaml` that leaves out `network:` or its `type:`,
/// as with Go's empty type (see [`NetworkType`]).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct NetworkConfig {
    #[serde(rename = "type", default)]
    pub kind: NetworkType,
    #[serde(
        default,
        deserialize_with = "null_as_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub mac: String,
    #[serde(
        default,
        deserialize_with = "null_items_as_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub port_forwards: Vec<PortForward>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_zero(n: &u16) -> bool {
    *n == 0
}

/// The YAML schema stored in `<vm-dir>/vm.yaml`.
///
/// A hand-edited file loads the way Go's yaml.v3 read it: a missing key or
/// an explicit null (`null`, `~`, nothing) is the zero value, booleans may
/// be YAML 1.1 words such as `yes` or `on`, and `created_at` may be any
/// timestamp yaml.v3 knew. What is wrong with the values is then reported
/// where they are used — by validation on start or save — rather than the
/// VM silently dropping out of the list. `name` alone is required.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmConfig {
    pub name: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub cpu: u32,
    /// MiB.
    #[serde(default, deserialize_with = "null_as_default")]
    pub ram: u32,
    /// GiB; informational, the actual size lives in `disk.qcow2`.
    #[serde(default, deserialize_with = "null_as_default")]
    pub disk_size: u32,
    /// e.g. `x86_64`, `aarch64`; empty means `x86_64`.
    #[serde(default, deserialize_with = "null_as_default")]
    pub arch: String,
    /// The boot ISO in the CD-ROM drive; empty for an empty drive.
    #[serde(
        default,
        deserialize_with = "null_as_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub cdrom_path: String,
    #[serde(default)]
    pub firmware: FirmwareType,
    /// UEFI Secure Boot with Microsoft's keys enrolled; implies UEFI.
    #[serde(
        default,
        deserialize_with = "yaml_bool",
        skip_serializing_if = "is_false"
    )]
    pub secure_boot: bool,
    /// Emulated TPM 2.0 (swtpm).
    #[serde(
        default,
        deserialize_with = "yaml_bool",
        skip_serializing_if = "is_false"
    )]
    pub tpm: bool,
    #[serde(default, deserialize_with = "null_as_default")]
    pub network: NetworkConfig,
    /// VNC display number (TCP port = 5900 + n); 0 = disabled.
    #[serde(
        default,
        deserialize_with = "null_as_default",
        skip_serializing_if = "is_zero"
    )]
    pub vnc_port: u16,
    /// Host USB devices passed through to the guest.
    #[serde(
        default,
        deserialize_with = "null_items_as_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub usb_devices: Vec<UsbDevice>,
    /// Disk images attached as read-only USB drives (ISO hot-plug).
    #[serde(
        default,
        deserialize_with = "null_items_as_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub usb_images: Vec<UsbImage>,
    /// Additional virtio disks next to the main one.
    #[serde(
        default,
        deserialize_with = "null_items_as_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub disks: Vec<Disk>,
    #[serde(default, deserialize_with = "yaml_timestamp")]
    pub created_at: DateTime<Utc>,
}

impl Default for VmConfig {
    fn default() -> Self {
        VmConfig {
            name: String::new(),
            cpu: 0,
            ram: 0,
            disk_size: 0,
            arch: String::new(),
            cdrom_path: String::new(),
            firmware: FirmwareType::Bios,
            secure_boot: false,
            tpm: false,
            network: NetworkConfig::default(),
            vnc_port: 0,
            usb_devices: Vec::new(),
            usb_images: Vec::new(),
            disks: Vec::new(),
            created_at: DateTime::<Utc>::default(),
        }
    }
}

impl VmConfig {
    /// Whether the VM boots UEFI firmware. Secure Boot needs UEFI, so it
    /// implies it even if `firmware` says otherwise.
    pub fn uefi(&self) -> bool {
        self.firmware == FirmwareType::Uefi || self.secure_boot
    }

    /// Describes the boot platform, e.g. `UEFI + Secure Boot, TPM 2.0`.
    pub fn firmware_label(&self) -> String {
        firmware_label(self.uefi(), self.secure_boot, self.tpm)
    }
}

/// The shared wording of [`VmConfig::firmware_label`] and the template's.
pub fn firmware_label(uefi: bool, secure_boot: bool, tpm: bool) -> String {
    let mut label = if secure_boot {
        "UEFI + Secure Boot".to_string()
    } else if uefi {
        "UEFI".to_string()
    } else {
        "BIOS".to_string()
    };
    if tpm {
        label.push_str(", TPM 2.0");
    }
    label
}

// --- Path helpers ---

/// The directory that holds all files for a named VM.
pub fn vm_dir(storage: &Path, name: &str) -> PathBuf {
    storage.join(name)
}

/// The main qcow2 disk image.
pub fn disk_path(storage: &Path, name: &str) -> PathBuf {
    vm_dir(storage, name).join(format!("{PRIMARY_DISK_NAME}.qcow2"))
}

/// The qcow2 image of one of the VM's additional disks, named after it.
pub fn extra_disk_path(storage: &Path, vm_name: &str, disk_name: &str) -> PathBuf {
    vm_dir(storage, vm_name).join(format!("{disk_name}.qcow2"))
}

/// `vm.yaml`.
pub fn config_file_path(storage: &Path, name: &str) -> PathBuf {
    vm_dir(storage, name).join("vm.yaml")
}

/// The serial console log.
pub fn console_path(storage: &Path, name: &str) -> PathBuf {
    vm_dir(storage, name).join("console.log")
}

/// The QEMU PID file.
pub fn pid_path(storage: &Path, name: &str) -> PathBuf {
    vm_dir(storage, name).join("qemu.pid")
}

/// Where QEMU's own stdout and stderr go: its warnings, and the reason when
/// it refuses to start. Rewritten on each start.
pub fn qemu_log_path(storage: &Path, name: &str) -> PathBuf {
    vm_dir(storage, name).join("qemu.log")
}

/// The QEMU monitor Unix socket.
pub fn monitor_path(storage: &Path, name: &str) -> PathBuf {
    vm_dir(storage, name).join("qemu-monitor.sock")
}

/// The Unix socket of the serial console.
pub fn serial_sock_path(storage: &Path, name: &str) -> PathBuf {
    vm_dir(storage, name).join("serial.sock")
}

/// The VM's private UEFI NVRAM store (pflash unit 1).
pub fn firmware_vars_path(storage: &Path, name: &str) -> PathBuf {
    vm_dir(storage, name).join("efivars.fd")
}

/// The directory holding the emulated TPM's persistent state.
pub fn tpm_dir(storage: &Path, name: &str) -> PathBuf {
    vm_dir(storage, name).join("tpm")
}

/// The swtpm control socket QEMU connects to.
pub fn tpm_sock_path(storage: &Path, name: &str) -> PathBuf {
    vm_dir(storage, name).join("swtpm.sock")
}

/// The PID file written by swtpm.
pub fn tpm_pid_path(storage: &Path, name: &str) -> PathBuf {
    vm_dir(storage, name).join("swtpm.pid")
}

/// swtpm's log file.
pub fn tpm_log_path(storage: &Path, name: &str) -> PathBuf {
    vm_dir(storage, name).join("swtpm.log")
}

// --- Tolerant decoding ---
//
// Go's decoders never failed on a missing key or an explicit null; they left
// the field at its zero value. These helpers give the serde structs of
// `vm.yaml`, `template.yaml` and `config.json` the same leniency, so a
// hand-edited file still loads and its problems are reported by the
// validation that uses the values, with Go's wording.

/// A field whose explicit null — YAML `null`, `~` or no value at all, JSON
/// `null` — means the type's default, the zero value yaml.v3 and
/// encoding/json left there. Pair with `#[serde(default)]` for a missing key.
/// A quoted `"null"` stays a string, as it did in Go.
pub(crate) fn null_as_default<'de, D, T>(d: D) -> std::result::Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// A list where both the list and any of its items may be null: a null list
/// is empty, a null item the item's default (Go decoded one into the zero
/// value of the element).
pub(crate) fn null_items_as_default<'de, D, T>(d: D) -> std::result::Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<Vec<Option<T>>>::deserialize(d)?
        .unwrap_or_default()
        .into_iter()
        .map(Option::unwrap_or_default)
        .collect())
}

/// A boolean the way yaml.v3 decoded one into a Go `bool`: `true`/`false`
/// in their YAML spellings, plus the YAML 1.1 words it still accepted for a
/// typed bool — `y`, `yes`, `on` and `n`, `no`, `off` (lower case, capitalised
/// or upper case). A null is `false`.
pub(crate) fn yaml_bool<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<bool, D::Error> {
    struct YamlBool;

    impl Visitor<'_> for YamlBool {
        type Value = bool;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a boolean")
        }

        fn visit_bool<E: de::Error>(self, b: bool) -> std::result::Result<bool, E> {
            Ok(b)
        }

        fn visit_str<E: de::Error>(self, s: &str) -> std::result::Result<bool, E> {
            match s {
                "y" | "Y" | "yes" | "Yes" | "YES" | "on" | "On" | "ON" => Ok(true),
                "n" | "N" | "no" | "No" | "NO" | "off" | "Off" | "OFF" => Ok(false),
                _ => Err(E::invalid_value(Unexpected::Str(s), &self)),
            }
        }

        fn visit_unit<E: de::Error>(self) -> std::result::Result<bool, E> {
            Ok(false)
        }

        fn visit_none<E: de::Error>(self) -> std::result::Result<bool, E> {
            Ok(false)
        }
    }

    d.deserialize_any(YamlBool)
}

/// A timestamp in any spelling yaml.v3 read into a `time.Time`: RFC 3339
/// (what Go and this program write), the same with one-digit date and time
/// fields or a lower-case `t`, `<date> <time>` without a zone, or a date
/// alone; the last two are UTC. A null is the default timestamp.
pub(crate) fn yaml_timestamp<'de, D: Deserializer<'de>>(
    d: D,
) -> std::result::Result<DateTime<Utc>, D::Error> {
    match Option::<String>::deserialize(d)? {
        None => Ok(DateTime::<Utc>::default()),
        Some(s) => parse_yaml_timestamp(&s)
            .ok_or_else(|| de::Error::invalid_value(Unexpected::Str(&s), &"an RFC 3339 timestamp")),
    }
}

/// The parsing behind [`yaml_timestamp`].
fn parse_yaml_timestamp(s: &str) -> Option<DateTime<Utc>> {
    if let Ok(t) = s.parse::<DateTime<Utc>>() {
        return Some(t);
    }
    // yaml.v3's allowedTimestampFormats; Go's `Z07:00` is `Z` or `±hh:mm`.
    let zoned = match s.strip_suffix('Z') {
        Some(rest) => format!("{rest}+00:00"),
        None => s.to_string(),
    };
    for format in ["%Y-%m-%dT%H:%M:%S%.f%:z", "%Y-%m-%dt%H:%M:%S%.f%:z"] {
        if let Ok(t) = DateTime::parse_from_str(&zoned, format) {
            return Some(t.with_timezone(&Utc));
        }
    }
    if let Ok(t) = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f") {
        return Some(t.and_utc());
    }
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|t| t.and_utc())
}

// --- File writing ---

/// Writes `data` to `path` without ever leaving a truncated file behind: it
/// goes into a temporary file in the same directory, is flushed to disk and
/// then renamed over `path`, so a crash, a power cut or an exit part-way
/// leaves either the old file or the new one. Like Go's
/// `os.WriteFile(path, data, 0644)`, a new file gets mode 0644 (less the
/// umask) and a file that was there keeps its mode, and a symlink at `path`
/// (or a chain of them) is written through rather than replaced: the file
/// it points at is replaced, or made if it is not there yet.
pub(crate) fn write_file_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    write_file_atomic_with(path, |f| f.write_all(data))
}

/// [`write_file_atomic`] with the writing step supplied, so a test can make
/// it fail part-way.
fn write_file_atomic_with(
    path: &Path,
    write: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    let target = symlink_target(path)?;
    let dir = match target.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    let base = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // A name of its own per attempt, so two saves of the same file at once
    // do not write into each other's temporary file.
    let mut attempts = 0;
    let (tmp, mut file) = loop {
        let tmp = dir.join(format!(".{base}.{:08x}.tmp", fastrand::u32(..)));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&tmp)
        {
            Ok(file) => break (tmp, file),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists && attempts < 100 => {
                attempts += 1;
            }
            Err(err) => return Err(err),
        }
    };
    // The mode of the file being replaced carries over (best effort: the
    // content matters more than the mode).
    if let Ok(meta) = fs::metadata(&target) {
        let _ = file.set_permissions(meta.permissions());
    }
    let result = write(&mut file)
        .and_then(|()| file.sync_all())
        .and_then(|()| {
            drop(file);
            fs::rename(&tmp, &target)
        });
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
        return result;
    }
    // The rename itself is durable once the directory is flushed too; a
    // directory that cannot be opened for that changes nothing about the
    // file being whole.
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// How many symlinks [`symlink_target`] follows: Linux's `MAXSYMLINKS`.
const MAX_SYMLINKS: usize = 40;

/// Where a write to `path` lands: `path` itself, or, when it is a symlink,
/// the file at the end of the chain of links, whether that file exists or
/// not (`open` with `O_CREAT` makes it). A relative link is taken from the
/// directory the link is in. More than [`MAX_SYMLINKS`] links fail with
/// `ELOOP`, as `open` does.
fn symlink_target(path: &Path) -> io::Result<PathBuf> {
    let mut target = path.to_path_buf();
    for _ in 0..=MAX_SYMLINKS {
        match fs::symlink_metadata(&target) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let link = fs::read_link(&target)?;
                // An absolute link replaces the directory in the join.
                target = match target.parent() {
                    Some(dir) => dir.join(link),
                    None => link,
                };
            }
            _ => return Ok(target),
        }
    }
    Err(Errno::ELOOP.into())
}

// --- YAML I/O ---

/// Reads and parses `vm.yaml` for the named VM.
pub fn load_config(storage: &Path, name: &str) -> Result<VmConfig> {
    let path = config_file_path(storage, name);
    let data = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let cfg: VmConfig =
        serde_yaml_ng::from_str(&data).with_context(|| format!("parse {}", path.display()))?;
    Ok(cfg)
}

/// Serialises and writes `vm.yaml`, atomically: a temporary file in the VM
/// directory is renamed over it, so it is never left cut short.
pub fn save_config(storage: &Path, cfg: &VmConfig) -> Result<()> {
    let path = config_file_path(storage, &cfg.name);
    let data = serde_yaml_ng::to_string(cfg)?;
    write_file_atomic(&path, data.as_bytes()).with_context(|| format!("write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn port_forwards_parse_and_format() {
        let fwds = parse_port_forwards("2222:22, udp:5353:53,,  TCP:8080:80 ").unwrap();
        assert_eq!(
            fwds,
            vec![
                PortForward {
                    host: 2222,
                    guest: 22,
                    proto: "tcp".into()
                },
                PortForward {
                    host: 5353,
                    guest: 53,
                    proto: "udp".into()
                },
                PortForward {
                    host: 8080,
                    guest: 80,
                    proto: "tcp".into()
                },
            ]
        );
        assert_eq!(
            format_port_forwards(&fwds),
            "tcp:2222:22, udp:5353:53, tcp:8080:80"
        );
        assert!(parse_port_forwards("").unwrap().is_empty());
        assert!(parse_port_forwards("  ,  ").unwrap().is_empty());
        // An empty proto formats as tcp.
        let f = PortForward {
            host: 1,
            guest: 2,
            proto: String::new(),
        };
        assert_eq!(format_port_forwards(&[f]), "tcp:1:2");
    }

    #[test]
    fn port_forward_errors_name_the_entry() {
        let err = parse_port_forwards("2222").unwrap_err().to_string();
        assert_eq!(
            err,
            "invalid port forward \"2222\" — expected [tcp|udp:]host:guest"
        );
        let err = parse_port_forwards("sctp:1:2").unwrap_err().to_string();
        assert_eq!(
            err,
            "invalid port forward \"sctp:1:2\" — expected [tcp|udp:]host:guest"
        );
        let err = parse_port_forwards("0:22").unwrap_err().to_string();
        assert_eq!(err, "invalid port forward \"0:22\" — ports must be 1–65535");
        let err = parse_port_forwards("2222:70000").unwrap_err().to_string();
        assert_eq!(
            err,
            "invalid port forward \"2222:70000\" — ports must be 1–65535"
        );
        assert!(parse_port_forwards("a:b").is_err());
        assert!(parse_port_forwards("1:2:3:4").is_err());
    }

    #[test]
    fn mac_validation() {
        assert!(validate_mac("52:54:00:12:34:56").is_ok());
        assert!(validate_mac("52-54-00-12-34-56").is_ok());
        assert!(validate_mac("5254.0012.3456").is_ok());
        assert!(validate_mac("52:54:00:12:34").is_err());
        assert!(
            validate_mac("52:54:00:12:34:56:78:9a").is_err(),
            "EUI-64 is not 48 bits"
        );
        assert!(validate_mac("zz:54:00:12:34:56").is_err());
        assert!(validate_mac("").is_err());
        assert_eq!(
            validate_mac("nope").unwrap_err().to_string(),
            "invalid MAC address \"nope\" — expected e.g. 52:54:00:12:34:56"
        );
        assert_eq!(
            normalize_mac("52:54:00:AB:cd:EF").unwrap(),
            "52:54:00:ab:cd:ef"
        );
        assert_eq!(
            normalize_mac("5254.00ab.cdef").unwrap(),
            "52:54:00:ab:cd:ef"
        );
    }

    #[test]
    fn mac_with_non_ascii_or_odd_groups_is_rejected_without_panicking() {
        // A multi-byte character across the middle of a dotted group used to
        // be sliced through and panic; like Go's net.ParseMAC, anything but
        // ASCII hex digits in a group is simply not a MAC.
        for bad in [
            "a€.a€.a€",
            "aé5.0000.0000",
            "5254.00é.3456",
            "52:54:00:12:34:é",
            "52-54-00-12-34-€",
            // from_str_radix would take a sign.
            "+2:54:00:12:34:56",
            "+254.0012.3456",
            // Group width goes with the separator, as in Go.
            "52.54.00.12.34.56",
            "5254:0012:3456",
            "5254-0012-3456",
            "52:54:00-12:34:56",
        ] {
            assert_eq!(parse_mac(bad), None, "{bad}");
            assert_eq!(normalize_mac(bad), None, "{bad}");
            assert_eq!(
                validate_mac(bad).unwrap_err().to_string(),
                format!("invalid MAC address {bad:?} — expected e.g. 52:54:00:12:34:56")
            );
        }
        assert_eq!(
            parse_mac("52-54-00-AB-cd-EF"),
            Some([0x52, 0x54, 0x00, 0xab, 0xcd, 0xef])
        );
        // A tap VM whose hand-edited vm.yaml holds such a MAC gets no IP
        // rather than a panic on the refresh thread.
        let cfg = VmConfig {
            name: "t".into(),
            network: NetworkConfig {
                kind: NetworkType::Tap,
                mac: "aé5.0000.0000".into(),
                ..NetworkConfig::default()
            },
            ..VmConfig::default()
        };
        assert_eq!(crate::vm::guest_ip(&cfg), None);
    }

    #[test]
    fn firmware_labels() {
        let mut cfg = VmConfig::default();
        assert_eq!(cfg.firmware_label(), "BIOS");
        assert!(!cfg.uefi());
        cfg.tpm = true;
        assert_eq!(cfg.firmware_label(), "BIOS, TPM 2.0");
        cfg.firmware = FirmwareType::Uefi;
        assert_eq!(cfg.firmware_label(), "UEFI, TPM 2.0");
        assert!(cfg.uefi());
        cfg.firmware = FirmwareType::Bios;
        cfg.secure_boot = true;
        assert!(cfg.uefi(), "secure boot implies UEFI");
        assert_eq!(cfg.firmware_label(), "UEFI + Secure Boot, TPM 2.0");
    }

    #[test]
    fn paths() {
        let s = Path::new("/vms");
        assert_eq!(vm_dir(s, "a"), PathBuf::from("/vms/a"));
        assert_eq!(disk_path(s, "a"), PathBuf::from("/vms/a/disk.qcow2"));
        assert_eq!(
            extra_disk_path(s, "a", "data"),
            PathBuf::from("/vms/a/data.qcow2")
        );
        assert_eq!(config_file_path(s, "a"), PathBuf::from("/vms/a/vm.yaml"));
        assert_eq!(console_path(s, "a"), PathBuf::from("/vms/a/console.log"));
        assert_eq!(pid_path(s, "a"), PathBuf::from("/vms/a/qemu.pid"));
        assert_eq!(qemu_log_path(s, "a"), PathBuf::from("/vms/a/qemu.log"));
        assert_eq!(
            monitor_path(s, "a"),
            PathBuf::from("/vms/a/qemu-monitor.sock")
        );
        assert_eq!(
            serial_sock_path(s, "a"),
            PathBuf::from("/vms/a/serial.sock")
        );
        assert_eq!(
            firmware_vars_path(s, "a"),
            PathBuf::from("/vms/a/efivars.fd")
        );
        assert_eq!(tpm_dir(s, "a"), PathBuf::from("/vms/a/tpm"));
        assert_eq!(tpm_sock_path(s, "a"), PathBuf::from("/vms/a/swtpm.sock"));
        assert_eq!(tpm_pid_path(s, "a"), PathBuf::from("/vms/a/swtpm.pid"));
        assert_eq!(tpm_log_path(s, "a"), PathBuf::from("/vms/a/swtpm.log"));
    }

    #[test]
    fn yaml_written_by_the_go_version_loads() {
        let yaml = r#"name: debian-12
cpu: 2
ram: 2048
disk_size: 20
disks:
  - name: data
    size: 50
arch: x86_64
cdrom_path: /home/user/iso/debian-12.iso
firmware: uefi
secure_boot: true
tpm: true
network:
  type: user
  mac: 52:54:00:ab:cd:ef
  port_forwards:
    - host: 2222
      guest: 22
      proto: tcp
    - host: 8080
      guest: 80
      proto: tcp
vnc_port: 1
usb_devices:
  - vendor_id: "046d"
    product_id: "085c"
    name: C922 Pro Stream Webcam
  - vendor_id: "0781"
    product_id: "5583"
    port: 3-2.2.4
usb_images:
  - path: /home/user/iso/virtio-win.iso
created_at: 2026-03-17T09:00:00Z
"#;
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("debian-12")).unwrap();
        fs::write(dir.path().join("debian-12/vm.yaml"), yaml).unwrap();
        let cfg = load_config(dir.path(), "debian-12").unwrap();
        assert_eq!(cfg.name, "debian-12");
        assert_eq!((cfg.cpu, cfg.ram, cfg.disk_size), (2, 2048, 20));
        assert_eq!(
            cfg.disks,
            vec![Disk {
                name: "data".into(),
                size: 50
            }]
        );
        assert_eq!(cfg.cdrom_path, "/home/user/iso/debian-12.iso");
        assert_eq!(cfg.firmware, FirmwareType::Uefi);
        assert!(cfg.secure_boot && cfg.tpm);
        assert_eq!(cfg.network.kind, NetworkType::User);
        assert_eq!(cfg.network.mac, "52:54:00:ab:cd:ef");
        assert_eq!(cfg.network.port_forwards.len(), 2);
        assert_eq!(cfg.vnc_port, 1);
        assert_eq!(cfg.usb_devices.len(), 2);
        assert_eq!(cfg.usb_devices[0].vendor_id, "046d");
        assert_eq!(cfg.usb_devices[0].name, "C922 Pro Stream Webcam");
        assert_eq!(cfg.usb_devices[1].product_id, "5583");
        assert_eq!(cfg.usb_devices[1].port, "3-2.2.4");
        assert_eq!(cfg.usb_images[0].path, "/home/user/iso/virtio-win.iso");
        assert_eq!(cfg.created_at.to_rfc3339(), "2026-03-17T09:00:00+00:00");

        // And it round-trips through save.
        save_config(dir.path(), &cfg).unwrap();
        let again = load_config(dir.path(), "debian-12").unwrap();
        assert_eq!(again, cfg);
        let text = fs::read_to_string(dir.path().join("debian-12/vm.yaml")).unwrap();
        assert!(text.starts_with("name: debian-12\n"), "{text}");
        assert!(text.contains("created_at: 2026-03-17T09:00:00Z"), "{text}");
    }

    #[test]
    fn minimal_and_odd_yaml_is_tolerated() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("x")).unwrap();
        // A hand-written file with only the essentials, an unknown firmware
        // word, an unknown network type and a timestamp with a zone offset.
        fs::write(
            dir.path().join("x/vm.yaml"),
            "name: x\ncpu: 1\nram: 512\ndisk_size: 5\narch: x86_64\nfirmware: efi\nnetwork:\n  type: bridge\ncreated_at: 2026-10-09T13:33:00.5+02:00\n",
        )
        .unwrap();
        let cfg = load_config(dir.path(), "x").unwrap();
        assert_eq!(cfg.firmware, FirmwareType::Bios);
        assert_eq!(cfg.network.kind, NetworkType::User);
        assert_eq!(cfg.vnc_port, 0);
        assert!(cfg.disks.is_empty());
        assert_eq!(cfg.created_at.to_rfc3339(), "2026-10-09T11:33:00.500+00:00");

        // Empty fields are left out when written, like Go's omitempty.
        save_config(dir.path(), &cfg).unwrap();
        let text = fs::read_to_string(dir.path().join("x/vm.yaml")).unwrap();
        for absent in [
            "cdrom_path",
            "secure_boot",
            "tpm",
            "vnc_port",
            "usb_devices",
            "usb_images",
            "disks",
            "mac",
            "port_forwards",
        ] {
            assert!(
                !text.contains(absent),
                "{absent} should be omitted:\n{text}"
            );
        }
        assert!(text.contains("firmware: bios\n"));
        assert!(text.contains("type: user\n"));
    }

    #[test]
    fn usb_entry_missing_key_loads_and_fails_validation() {
        // A hand-edited entry without vendor_id or product_id loads with ""
        // (as yaml.v3 did) so the VM stays listed; start reports it through
        // validate() instead of the file failing to parse.
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("u")).unwrap();
        fs::write(
            dir.path().join("u/vm.yaml"),
            "name: u\ncpu: 1\nram: 512\ndisk_size: 5\nnetwork:\n  type: none\ncreated_at: 2026-10-09T13:33:00Z\nusb_devices:\n  - vendor_id: 046d\n  - {}\n",
        )
        .unwrap();
        let cfg = load_config(dir.path(), "u").unwrap();
        assert_eq!(cfg.usb_devices.len(), 2);
        let dev = &cfg.usb_devices[0];
        assert_eq!(
            (dev.vendor_id.as_str(), dev.product_id.as_str()),
            ("046d", "")
        );
        assert_eq!(
            dev.validate().unwrap_err().to_string(),
            "invalid USB device \"046d\":\"\" — vendor_id and product_id must be 4 hex digits"
        );
        assert_eq!(
            cfg.usb_devices[1].validate().unwrap_err().to_string(),
            "invalid USB device \"\":\"\" — vendor_id and product_id must be 4 hex digits"
        );
        let names: Vec<String> = crate::vm::manager::Manager::new(dir.path())
            .list()
            .unwrap()
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, ["u"]);
    }

    #[test]
    fn missing_or_unknown_network_type_is_user_like_gos_default_nic() {
        // Go gave a VM with an empty or unknown type QEMU's default NIC, which
        // is user networking, and its forms took such a type for user; only
        // tap and none are anything else.
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("n")).unwrap();
        for (yaml, want) in [
            ("name: n\n", NetworkType::User),
            ("name: n\nnetwork:\n", NetworkType::User),
            ("name: n\nnetwork: null\n", NetworkType::User),
            ("name: n\nnetwork: ~\n", NetworkType::User),
            (
                "name: n\nnetwork:\n  mac: 52:54:00:12:34:56\n",
                NetworkType::User,
            ),
            ("name: n\nnetwork:\n  type: ~\n", NetworkType::User),
            ("name: n\nnetwork:\n  type:\n", NetworkType::User),
            ("name: n\nnetwork:\n  type: ''\n", NetworkType::User),
            ("name: n\nnetwork:\n  type: user\n", NetworkType::User),
            ("name: n\nnetwork:\n  type: bridge\n", NetworkType::User),
            ("name: n\nnetwork:\n  type: Tap\n", NetworkType::User),
            ("name: n\nnetwork:\n  type: 1\n", NetworkType::User),
            ("name: n\nnetwork:\n  type: tap\n", NetworkType::Tap),
            ("name: n\nnetwork:\n  type: \"none\"\n", NetworkType::None),
        ] {
            fs::write(dir.path().join("n/vm.yaml"), yaml).unwrap();
            let cfg = load_config(dir.path(), "n").unwrap();
            assert_eq!(cfg.network.kind, want, "{yaml}");
            let (_, args) = crate::vm::build_qemu_args(&cfg, dir.path()).unwrap();
            let netdev = args
                .iter()
                .position(|a| a == "-netdev")
                .map(|i| args[i + 1].as_str());
            match want {
                NetworkType::User => assert!(
                    netdev.is_some_and(|n| n.starts_with("user,")),
                    "{yaml}: {args:?}"
                ),
                NetworkType::Tap => assert!(
                    netdev.is_some_and(|n| n.starts_with("bridge,")),
                    "{yaml}: {args:?}"
                ),
                NetworkType::None => assert_eq!(netdev, None, "{yaml}: {args:?}"),
            }
        }
        // A type that is no scalar is an error, as it was for yaml.v3.
        fs::write(
            dir.path().join("n/vm.yaml"),
            "name: n\nnetwork:\n  type: [tap]\n",
        )
        .unwrap();
        assert!(load_config(dir.path(), "n").is_err());
        assert_eq!(NetworkType::default(), NetworkType::User);
        assert_eq!(NetworkConfig::default().kind, NetworkType::User);
        assert_eq!(VmConfig::default().network.kind, NetworkType::User);
    }

    #[test]
    fn nulls_and_yaml_1_1_words_load_like_yaml_v3() {
        // Explicit nulls are zero values (not the strings "null" or "~"), and
        // booleans and timestamps take the spellings yaml.v3 accepted.
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("z")).unwrap();
        fs::write(
            dir.path().join("z/vm.yaml"),
            "name: z\ncpu: null\nram: ~\ndisk_size:\narch: null\ncdrom_path: ~\nsecure_boot: on\ntpm: yes\nnetwork:\n  type: user\n  mac: null\n  port_forwards:\n    - host: 2222\n      guest: 22\n      proto: null\n    - {}\nvnc_port: null\nusb_devices: ~\nusb_images: null\ndisks: null\ncreated_at: 2026-10-09 13:33:00\n",
        )
        .unwrap();
        let cfg = load_config(dir.path(), "z").unwrap();
        assert_eq!(
            cfg,
            VmConfig {
                name: "z".into(),
                secure_boot: true,
                tpm: true,
                network: NetworkConfig {
                    kind: NetworkType::User,
                    mac: String::new(),
                    port_forwards: vec![
                        PortForward {
                            host: 2222,
                            guest: 22,
                            proto: String::new(),
                        },
                        PortForward::default(),
                    ],
                },
                created_at: "2026-10-09T13:33:00Z".parse().unwrap(),
                ..VmConfig::default()
            }
        );
        // yaml.v3 refused these too.
        for bad in [
            "name: z\ntpm: \"true\"\n",
            "name: z\ntpm: yEs\n",
            "name: z\ncreated_at: 2026-10-09T13:33:00\n",
            "cpu: 1\n",
        ] {
            fs::write(dir.path().join("z/vm.yaml"), bad).unwrap();
            assert!(load_config(dir.path(), "z").is_err(), "{bad}");
        }
    }

    #[test]
    fn save_is_atomic_and_a_failed_write_keeps_the_old_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("a")).unwrap();
        let path = config_file_path(dir.path(), "a");
        let mut cfg = VmConfig {
            name: "a".into(),
            cpu: 2,
            ..VmConfig::default()
        };
        save_config(dir.path(), &cfg).unwrap();
        let old = fs::read_to_string(&path).unwrap();

        // A write that dies part-way — a full disk, say — leaves vm.yaml as
        // it was and no temporary file behind.
        let err = write_file_atomic_with(&path, |f| {
            f.write_all(b"name: a\ncp")?;
            Err(io::Error::other("disk full"))
        })
        .unwrap_err();
        assert_eq!(err.to_string(), "disk full");
        assert_eq!(fs::read_to_string(&path).unwrap(), old);
        assert_eq!(load_config(dir.path(), "a").unwrap(), cfg);
        let names: Vec<_> = fs::read_dir(dir.path().join("a"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["vm.yaml"]);

        // A successful save replaces it whole; the file, made by the first
        // save, is mode 0644 less the umask.
        cfg.cpu = 4;
        save_config(dir.path(), &cfg).unwrap();
        assert_eq!(load_config(dir.path(), "a").unwrap().cpu, 4);
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        let umask = fs::read_to_string("/proc/self/status").ok().and_then(|st| {
            st.lines()
                .find_map(|l| l.strip_prefix("Umask:"))
                .and_then(|m| u32::from_str_radix(m.trim(), 8).ok())
        });
        if let Some(umask) = umask {
            assert_eq!(mode, 0o644 & !umask, "mode {mode:o}");
        }
        // A mode the user gave the file stays, as os.WriteFile left it.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        save_config(dir.path(), &cfg).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "mode {mode:o}");
        let names: Vec<_> = fs::read_dir(dir.path().join("a"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["vm.yaml"]);

        // A VM directory that is gone fails with the path, as before.
        let gone = VmConfig {
            name: "gone".into(),
            ..VmConfig::default()
        };
        let err = save_config(dir.path(), &gone).unwrap_err().to_string();
        assert!(
            err.starts_with(&format!(
                "write {}",
                config_file_path(dir.path(), "gone").display()
            )),
            "{err}"
        );
    }

    /// A symlink is written through even when the file it points at is not
    /// there yet, which is then made, as Go's `os.WriteFile` did; so are
    /// relative links, taken from the link's directory, and chains of links.
    /// A loop fails rather than replace a link.
    #[test]
    fn write_goes_through_dangling_relative_and_chained_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let entries = |sub: &str| -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(dir.path().join(sub))
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        };
        let is_link = |p: &Path| fs::symlink_metadata(p).unwrap().file_type().is_symlink();
        fs::create_dir_all(dir.path().join("links")).unwrap();
        fs::create_dir_all(dir.path().join("dotfiles")).unwrap();

        // Dangling, absolute.
        let real = dir.path().join("dotfiles/config.json");
        let link = dir.path().join("links/config.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        write_file_atomic(&link, b"one").unwrap();
        assert!(is_link(&link), "the link was replaced");
        assert_eq!(fs::read(&real).unwrap(), b"one");

        // Dangling, relative to the link's directory (not the current one),
        // through a chain of two links.
        let real = dir.path().join("dotfiles/vm.yaml");
        let first = dir.path().join("links/vm.yaml");
        let second = dir.path().join("links/vm-link.yaml");
        std::os::unix::fs::symlink("vm-link.yaml", &first).unwrap();
        std::os::unix::fs::symlink("../dotfiles/vm.yaml", &second).unwrap();
        write_file_atomic(&first, b"two").unwrap();
        assert!(is_link(&first) && is_link(&second), "a link was replaced");
        assert_eq!(fs::read(&real).unwrap(), b"two");
        // Once the file is there, it is replaced the same way.
        write_file_atomic(&first, b"three").unwrap();
        assert_eq!(fs::read(&real).unwrap(), b"three");
        assert_eq!(entries("dotfiles"), ["config.json", "vm.yaml"]);
        assert_eq!(entries("links"), ["config.json", "vm-link.yaml", "vm.yaml"]);

        // A loop is an error and leaves the links alone.
        let a = dir.path().join("links/a");
        let b = dir.path().join("links/b");
        std::os::unix::fs::symlink("b", &a).unwrap();
        std::os::unix::fs::symlink("a", &b).unwrap();
        let err = write_file_atomic(&a, b"four").unwrap_err();
        assert_eq!(err.raw_os_error(), Some(Errno::ELOOP as i32), "{err}");
        assert!(is_link(&a) && is_link(&b), "a link was replaced");
        assert_eq!(
            entries("links"),
            ["a", "b", "config.json", "vm-link.yaml", "vm.yaml"]
        );
    }

    #[test]
    fn missing_vm_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_config(dir.path(), "nope").is_err());
    }
}
