//! Host USB passthrough: the `vm.yaml` entries, host enumeration through
//! sysfs, matching, access checks with the udev fix, and hot-plug.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{anyhow, bail, Context, Result};
use nix::unistd::AccessFlags;
use regex::Regex;
use serde::{Deserialize, Serialize};

use super::config::{null_as_default, VmConfig};
use super::monitor::monitor_must_succeed;

/// A host USB device passed through to the guest (`vm.yaml` schema).
///
/// A device is matched by vendor/product ID, the way lsusb shows it. `port`
/// optionally pins the entry to one physical port — the sysfs device name,
/// e.g. `3-2.2.2` (bus 3, port path 2.2.2) — which is what tells apart two
/// identical devices plugged in at the same time.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbDevice {
    /// 4 hex digits, e.g. `046d`. A missing key or a null loads as `""` (as
    /// yaml.v3 did) and is reported by [`UsbDevice::validate`], not at load
    /// time.
    #[serde(default, deserialize_with = "null_as_default")]
    pub vendor_id: String,
    /// 4 hex digits, e.g. `085c`.
    #[serde(default, deserialize_with = "null_as_default")]
    pub product_id: String,
    /// Informational, captured when attached.
    #[serde(
        default,
        deserialize_with = "null_as_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub name: String,
    /// `<bus>-<port path>`, e.g. `3-2.2.2`.
    #[serde(
        default,
        deserialize_with = "null_as_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub port: String,
}

/// A vendor or product ID: exactly 4 hex digits.
static USB_HEX_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9a-fA-F]{4}$").expect("valid regex"));
/// A sysfs device name `<bus>-<port[.port…]>`; also what tells devices apart
/// from root hubs (`usbN`) and interfaces (`<bus>-<port>:<config>.<iface>`).
static USB_PORT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9]+-[0-9]+(\.[0-9]+)*$").expect("valid regex"));

/// Parses `vvvv:pppp` — hex vendor and product IDs as printed by lsusb —
/// into lower-case IDs. Error:
/// `invalid USB ID "<s>" — expected vendor:product as 4 hex digits each, e.g. 046d:085c`.
pub fn parse_usb_id(s: &str) -> Result<UsbDevice> {
    let parts: Vec<&str> = s.trim().split(':').collect();
    let [vendor, product] = parts[..] else {
        bail!(
            "invalid USB ID {s:?} — expected vendor:product as 4 hex digits each, e.g. 046d:085c"
        );
    };
    if !USB_HEX_ID.is_match(vendor) || !USB_HEX_ID.is_match(product) {
        bail!(
            "invalid USB ID {s:?} — expected vendor:product as 4 hex digits each, e.g. 046d:085c"
        );
    }
    Ok(UsbDevice {
        vendor_id: vendor.to_lowercase(),
        product_id: product.to_lowercase(),
        ..UsbDevice::default()
    })
}

impl UsbDevice {
    /// Checks the IDs and the optional port pin. Errors:
    /// `invalid USB device "<v>":"<p>" — vendor_id and product_id must be 4 hex digits`,
    /// `invalid USB port "<port>" for <id> — expected <bus>-<port path>, e.g. 3-2.2.2`.
    pub fn validate(&self) -> Result<()> {
        if !USB_HEX_ID.is_match(&self.vendor_id) || !USB_HEX_ID.is_match(&self.product_id) {
            bail!(
                "invalid USB device {:?}:{:?} — vendor_id and product_id must be 4 hex digits",
                self.vendor_id,
                self.product_id
            );
        }
        if !self.port.is_empty() && !USB_PORT_RE.is_match(&self.port) {
            bail!(
                "invalid USB port {:?} for {} — expected <bus>-<port path>, e.g. 3-2.2.2",
                self.port,
                self.id()
            );
        }
        Ok(())
    }

    /// `vvvv:pppp`, lower-case.
    pub fn id(&self) -> String {
        format!("{}:{}", self.vendor_id, self.product_id).to_lowercase()
    }

    /// The stored name, falling back to the ID.
    pub fn label(&self) -> String {
        if self.name.is_empty() {
            self.id()
        } else {
            self.name.clone()
        }
    }

    /// Whether the connected host device satisfies this entry (IDs compared
    /// ignoring case; the port only when pinned).
    pub fn matches(&self, h: &HostUsbDevice) -> bool {
        self.vendor_id.eq_ignore_ascii_case(&h.vendor_id)
            && self.product_id.eq_ignore_ascii_case(&h.product_id)
            && (self.port.is_empty() || self.port == h.port)
    }
}

/// QEMU object names. The controller is always present so devices can be
/// hot-plugged into a running VM; usb-host devices attach to its bus.
pub(crate) const USB_CONTROLLER_ID: &str = "xhci";
pub(crate) const USB_BUS_NAME: &str = "xhci.0";
// The bus is the controller's first one: `<controller>.0`.
const _: () = {
    let (bus, ctl) = (USB_BUS_NAME.as_bytes(), USB_CONTROLLER_ID.as_bytes());
    assert!(bus.len() == ctl.len() + 2 && bus[ctl.len()] == b'.' && bus[ctl.len() + 1] == b'0');
    let mut i = 0;
    while i < ctl.len() {
        assert!(bus[i] == ctl[i]);
        i += 1;
    }
};

/// The QEMU device IDs for the configured devices:
/// `usb-<vendor>-<product>[-<port>]`, with a numeric suffix for duplicates.
///
/// They are derived from the config rather than the position, so a device
/// attached at boot can later be named for hot-unplug. Hand-edited
/// duplicates get a numeric suffix so QEMU does not refuse to start on a
/// duplicate ID.
pub fn usb_device_ids(devs: &[UsbDevice]) -> Vec<String> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    devs.iter()
        .map(|d| {
            let mut id = format!(
                "usb-{}-{}",
                d.vendor_id.to_lowercase(),
                d.product_id.to_lowercase()
            );
            if !d.port.is_empty() {
                id.push('-');
                id.push_str(&d.port);
            }
            match seen.get_mut(&id) {
                Some(n) => {
                    *n += 1;
                    format!("{id}-{n}")
                }
                None => {
                    seen.insert(id.clone(), 1);
                    id
                }
            }
        })
        .collect()
}

/// `usb-host,id=<id>,bus=xhci.0,vendorid=0x<v>,productid=0x<p>[,hostbus=<bus>,hostport=<path>]`.
///
/// Used verbatim both on the command line (`-device`) and for hot-plug
/// (`device_add`).
pub(crate) fn usb_host_device(d: &UsbDevice, id: &str) -> String {
    let mut spec = format!(
        "usb-host,id={id},bus={USB_BUS_NAME},vendorid=0x{},productid=0x{}",
        d.vendor_id.to_lowercase(),
        d.product_id.to_lowercase()
    );
    if !d.port.is_empty() {
        let (bus, path) = d.port.split_once('-').unwrap_or((&d.port, ""));
        let _ = write!(spec, ",hostbus={bus},hostport={path}");
    }
    spec
}

// --- Host enumeration ---

/// A USB device currently connected to the host.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostUsbDevice {
    pub vendor_id: String,
    pub product_id: String,
    pub manufacturer: String,
    pub product: String,
    pub bus: u32,
    pub dev: u32,
    /// sysfs name `<bus>-<port path>`; stable for a physical port.
    pub port: String,
    /// `/dev/bus/usb/BBB/DDD` — what QEMU (libusb) opens.
    pub dev_node: String,
    /// This user may open `dev_node` read-write.
    pub writable: bool,
}

impl HostUsbDevice {
    /// `vvvv:pppp`.
    pub fn id(&self) -> String {
        format!("{}:{}", self.vendor_id, self.product_id)
    }

    /// `Manufacturer Product`, avoiding a doubled manufacturer name, falling
    /// back to the ID when the device reports no strings.
    pub fn label(&self) -> String {
        let (man, prod) = (self.manufacturer.trim(), self.product.trim());
        match (man.is_empty(), prod.is_empty()) {
            (true, true) => self.id(),
            (true, false) => prod.to_string(),
            (false, true) => man.to_string(),
            (false, false) if prod.to_lowercase().starts_with(&man.to_lowercase()) => {
                prod.to_string()
            }
            (false, false) => format!("{man} {prod}"),
        }
    }
}

/// Where the host's USB devices are looked up; tests point it at fixtures.
#[derive(Debug, Clone)]
pub(crate) struct UsbHost {
    pub sysfs_dir: PathBuf,
    pub dev_dir: PathBuf,
}

impl Default for UsbHost {
    fn default() -> Self {
        UsbHost {
            sysfs_dir: PathBuf::from("/sys/bus/usb/devices"),
            dev_dir: PathBuf::from("/dev/bus/usb"),
        }
    }
}

/// Enumerates connected USB devices through sysfs (Linux), ordered by bus
/// and port. Root hubs and hubs are left out: they stay with the host kernel
/// and cannot be passed through. Error without sysfs:
/// `list host USB devices (needs Linux sysfs): <io error>`.
pub fn list_host_usb_devices() -> Result<Vec<HostUsbDevice>> {
    list_host_usb_devices_in(&UsbHost::default())
}

/// [`list_host_usb_devices`] against explicit sysfs and devfs roots.
pub(crate) fn list_host_usb_devices_in(host: &UsbHost) -> Result<Vec<HostUsbDevice>> {
    let context = || {
        format!(
            "list host USB devices (needs Linux sysfs): open {}",
            host.sysfs_dir.display()
        )
    };
    let mut devs = Vec::new();
    for entry in fs::read_dir(&host.sysfs_dir).with_context(context)? {
        let entry = entry.with_context(context)?;
        let name = entry.file_name();
        // Devices are "<bus>-<port path>"; skip root hubs ("usbN") and
        // interfaces ("<bus>-<port>:<config>.<iface>").
        let Some(name) = name.to_str().filter(|n| USB_PORT_RE.is_match(n)) else {
            continue;
        };
        let dir = host.sysfs_dir.join(name);
        let (vendor, product) = (sysfs_attr(&dir, "idVendor"), sysfs_attr(&dir, "idProduct"));
        if vendor.is_empty() || product.is_empty() {
            continue;
        }
        if sysfs_attr(&dir, "bDeviceClass") == "09" {
            continue; // hub
        }
        let bus: u32 = sysfs_attr(&dir, "busnum").parse().unwrap_or(0);
        let devnum: u32 = sysfs_attr(&dir, "devnum").parse().unwrap_or(0);
        let node = host
            .dev_dir
            .join(format!("{bus:03}"))
            .join(format!("{devnum:03}"));
        devs.push(HostUsbDevice {
            vendor_id: vendor.to_lowercase(),
            product_id: product.to_lowercase(),
            manufacturer: sysfs_attr(&dir, "manufacturer"),
            product: sysfs_attr(&dir, "product"),
            bus,
            dev: devnum,
            port: name.to_string(),
            writable: writable(&node),
            dev_node: node.to_string_lossy().into_owned(),
        });
    }
    devs.sort_by(|a, b| port_cmp(&a.port, &b.port));
    Ok(devs)
}

/// Reads one sysfs attribute, trimmed; `""` when unreadable.
pub(crate) fn sysfs_attr(dir: &Path, attr: &str) -> String {
    fs::read(dir.join(attr))
        .map(|b| String::from_utf8_lossy(&b).trim().to_string())
        .unwrap_or_default()
}

/// Whether this user may open the device node read-write. It asks the
/// kernel (`access(2)` with W_OK) so ACLs — how udev's uaccess tag grants
/// access — are honoured, and nothing is opened.
pub(crate) fn writable(node: &Path) -> bool {
    nix::unistd::access(node, AccessFlags::W_OK).is_ok()
}

/// Orders `<bus>-<a>.<b>...` names numerically, component by component; a
/// name that is a prefix of another sorts first.
pub(crate) fn port_cmp(a: &str, b: &str) -> Ordering {
    port_key(a).cmp(port_key(b))
}

/// The numeric components of a port name; `3-2.10` → `3, 2, 10`.
fn port_key(port: &str) -> impl Iterator<Item = u64> + '_ {
    port.split(['-', '.'])
        .filter(|f| !f.is_empty())
        .map(|f| f.parse().unwrap_or(0))
}

// --- Config ↔ host matching ---

/// A configured device paired with what the host currently shows for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbState {
    pub device: UsbDevice,
    /// `None` when the device is not connected.
    pub host: Option<HostUsbDevice>,
}

/// Resolves each configured device against the host. Where sysfs is
/// unavailable every device reports as not connected.
pub fn usb_states(devs: &[UsbDevice]) -> Vec<UsbState> {
    usb_states_in(devs, &UsbHost::default())
}

/// [`usb_states`] against explicit sysfs and devfs roots.
pub(crate) fn usb_states_in(devs: &[UsbDevice], host: &UsbHost) -> Vec<UsbState> {
    let connected = list_host_usb_devices_in(host).unwrap_or_default();
    match_usb(devs, &connected)
}

/// Pairs config entries with connected host devices. Each host device is
/// claimed by at most one entry — port-pinned entries first, so an unpinned
/// entry for the same ID cannot steal their device.
pub fn match_usb(devs: &[UsbDevice], host: &[HostUsbDevice]) -> Vec<UsbState> {
    let mut states: Vec<UsbState> = devs
        .iter()
        .map(|d| UsbState {
            device: d.clone(),
            host: None,
        })
        .collect();
    let mut claimed = vec![false; host.len()];
    for pinned_pass in [true, false] {
        for (state, d) in states.iter_mut().zip(devs) {
            if d.port.is_empty() == pinned_pass || state.host.is_some() {
                continue;
            }
            if let Some((j, h)) = host
                .iter()
                .enumerate()
                .find(|(j, h)| !claimed[*j] && d.matches(h))
            {
                claimed[j] = true;
                state.host = Some(h.clone());
            }
        }
    }
    states
}

/// Names every configured device that is connected but not openable by this
/// user (`no write access to USB device <id> (<label>) at <node>` lines)
/// followed by [`udev_rule_hint`]. `Ok` without sysfs.
///
/// Without this check QEMU would start fine and silently never attach the
/// device (its libusb errors only go to stderr).
pub fn check_usb_access(devs: &[UsbDevice]) -> Result<()> {
    check_usb_access_in(devs, &UsbHost::default())
}

/// [`check_usb_access`] against explicit sysfs and devfs roots.
pub(crate) fn check_usb_access_in(devs: &[UsbDevice], host: &UsbHost) -> Result<()> {
    let Ok(connected) = list_host_usb_devices_in(host) else {
        return Ok(()); // no sysfs — nothing to check
    };
    let mut denied = Vec::new();
    let mut msg = String::new();
    for s in match_usb(devs, &connected) {
        if let Some(h) = s.host.as_ref().filter(|h| !h.writable) {
            let _ = writeln!(
                msg,
                "no write access to USB device {} ({}) at {}",
                h.id(),
                h.label(),
                h.dev_node
            );
            denied.push(s.device);
        }
    }
    if denied.is_empty() {
        return Ok(());
    }
    msg.push_str(&udev_rule_hint(&denied));
    Err(anyhow!(msg))
}

/// Where the generated udev rules go. It sorts before `73-seat-late.rules`,
/// which is what turns the uaccess tag into an ACL.
pub const UDEV_RULES_FILE: &str = "/etc/udev/rules.d/70-ostrich-usb.rules";

/// `Grant access to your user by running (then rescan):\n  <command>`.
pub fn udev_rule_hint(devs: &[UsbDevice]) -> String {
    format!(
        "Grant access to your user by running (then rescan):\n  {}",
        udev_rule_command(devs)
    )
}

/// The one-line shell command that grants access: [`udev_rule_command_words`] joined by spaces.
///
/// It appends a udev rule per device to [`UDEV_RULES_FILE`], reloads udev
/// and re-applies the rules to connected devices.
pub fn udev_rule_command(devs: &[UsbDevice]) -> String {
    udev_rule_command_words(devs).join(" ")
}

/// The command split into shell words, every one a complete argument or
/// operator, so a caller may break lines between any two words (with a
/// backslash continuation) to fit a narrow screen and the pasted result
/// still runs as one command in bash, zsh or fish.
///
/// Each rule is written as four quoted words which echo/printf join with
/// spaces; with several devices printf reuses its format once per device.
pub fn udev_rule_command_words(devs: &[UsbDevice]) -> Vec<String> {
    let mut words: Vec<String> = Vec::with_capacity(2 + 4 * devs.len() + 14);
    if devs.len() == 1 {
        words.push("echo".to_string());
    } else {
        words.push("printf".to_string());
        words.push(r"'%s %s %s %s\n'".to_string());
    }
    for d in devs {
        words.push(r#"'SUBSYSTEM=="usb",'"#.to_string());
        words.push(format!(
            r#"'ATTR{{idVendor}}=="{}",'"#,
            d.vendor_id.to_lowercase()
        ));
        words.push(format!(
            r#"'ATTR{{idProduct}}=="{}",'"#,
            d.product_id.to_lowercase()
        ));
        words.push(r#"'TAG+="uaccess"'"#.to_string());
    }
    words.extend(
        [
            "|",
            "sudo",
            "tee",
            "-a",
            UDEV_RULES_FILE,
            "&&",
            "sudo",
            "udevadm",
            "control",
            "--reload",
            "&&",
            "sudo",
            "udevadm",
            "trigger",
        ]
        .into_iter()
        .map(str::to_string),
    );
    words
}

// --- Hot-plug ---

/// Attaches `cfg.usb_devices[idx]` to the running VM (`device_add usb-host,...`)
/// after validating it and checking access. A device that is not connected
/// yet is picked up when plugged in.
pub fn usb_hotplug(storage: &Path, cfg: &VmConfig, idx: usize) -> Result<()> {
    let dev = usb_device_at(cfg, idx)?;
    dev.validate()?;
    check_usb_access(std::slice::from_ref(dev))?;
    let ids = usb_device_ids(&cfg.usb_devices);
    monitor_must_succeed(
        storage,
        &cfg.name,
        &format!("device_add {}", usb_host_device(dev, &ids[idx])),
    )
}

/// Detaches `cfg.usb_devices[idx]` from the running VM (`device_del <id>`);
/// `cfg` is the config as it was before the entry was removed, so the
/// device ID matches.
pub fn usb_hotunplug(storage: &Path, cfg: &VmConfig, idx: usize) -> Result<()> {
    usb_device_at(cfg, idx)?;
    let ids = usb_device_ids(&cfg.usb_devices);
    monitor_must_succeed(storage, &cfg.name, &format!("device_del {}", ids[idx]))
}

/// The configured device at `idx`, or `no USB device at index <idx>`.
fn usb_device_at(cfg: &VmConfig, idx: usize) -> Result<&UsbDevice> {
    cfg.usb_devices
        .get(idx)
        .ok_or_else(|| anyhow!("no USB device at index {idx}"))
}

#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::time::{Duration, Instant};

    use tempfile::tempdir;

    use super::*;
    use crate::vm::config::{NetworkConfig, NetworkType};
    use crate::vm::manager::Manager;
    use crate::vm::monitor::monitor_command;
    use crate::vm::process::{build_qemu_args, start, status, stop, VmStatus};

    fn dev(vendor: &str, product: &str) -> UsbDevice {
        UsbDevice {
            vendor_id: vendor.into(),
            product_id: product.into(),
            ..UsbDevice::default()
        }
    }

    fn pinned(vendor: &str, product: &str, port: &str) -> UsbDevice {
        UsbDevice {
            port: port.into(),
            ..dev(vendor, product)
        }
    }

    #[test]
    fn parse_usb_id_accepts_lsusb_form() {
        let d = parse_usb_id(" 046D:085c ").unwrap();
        assert_eq!(d.vendor_id, "046d");
        assert_eq!(d.product_id, "085c");
        assert_eq!(d.id(), "046d:085c");
        assert_eq!(d, dev("046d", "085c"));
        for bad in [
            "",
            "046d",
            "046d:85c",
            "46d:085c",
            "xyz1:0001",
            "046d:085c:1",
        ] {
            assert!(parse_usb_id(bad).is_err(), "parse_usb_id({bad:?}) accepted");
        }
        assert_eq!(
            parse_usb_id("046d:85c").unwrap_err().to_string(),
            "invalid USB ID \"046d:85c\" — expected vendor:product as 4 hex digits each, e.g. 046d:085c"
        );
    }

    #[test]
    fn usb_device_validate() {
        pinned("046d", "085c", "3-2.2.2").validate().unwrap();
        pinned("046d", "085c", "1-1").validate().unwrap();
        for bad in [
            dev("46d", "085c"),
            pinned("046d", "085c", "2.2.2"),
            pinned("046d", "085c", "3-"),
        ] {
            assert!(bad.validate().is_err(), "{bad:?} accepted");
        }
        assert_eq!(
            dev("46d", "085c").validate().unwrap_err().to_string(),
            "invalid USB device \"46d\":\"085c\" — vendor_id and product_id must be 4 hex digits"
        );
        assert_eq!(
            pinned("046D", "085c", "2.2.2")
                .validate()
                .unwrap_err()
                .to_string(),
            "invalid USB port \"2.2.2\" for 046d:085c — expected <bus>-<port path>, e.g. 3-2.2.2"
        );
    }

    #[test]
    fn usb_device_label_falls_back_to_id() {
        assert_eq!(dev("046D", "085C").label(), "046d:085c");
        assert_eq!(
            UsbDevice {
                name: "Webcam".into(),
                ..dev("046d", "085c")
            }
            .label(),
            "Webcam"
        );
    }

    #[test]
    fn usb_device_ids_and_spec() {
        let devs = [
            dev("046D", "085c"),
            pinned("046d", "085c", "3-2.2.2"),
            dev("046d", "085c"), // hand-edited duplicate
        ];
        let ids = usb_device_ids(&devs);
        assert_eq!(
            ids,
            ["usb-046d-085c", "usb-046d-085c-3-2.2.2", "usb-046d-085c-2"]
        );

        let spec = usb_host_device(&devs[0], &ids[0]);
        assert_eq!(
            spec,
            "usb-host,id=usb-046d-085c,bus=xhci.0,vendorid=0x046d,productid=0x085c"
        );
        let spec = usb_host_device(&devs[1], &ids[1]);
        assert!(
            spec.ends_with(",hostbus=3,hostport=2.2.2"),
            "pinned spec = {spec:?}"
        );
        assert_eq!(USB_BUS_NAME, format!("{USB_CONTROLLER_ID}.0"));
    }

    #[test]
    fn build_qemu_args_usb() {
        let storage = tempdir().unwrap();
        let cfg = VmConfig {
            name: "t".into(),
            cpu: 1,
            ram: 128,
            network: NetworkConfig {
                kind: NetworkType::None,
                ..NetworkConfig::default()
            },
            usb_devices: vec![dev("046d", "085c")],
            ..VmConfig::default()
        };
        let (_, args) = build_qemu_args(&cfg, storage.path()).unwrap();
        let joined = args.join(" ");
        assert!(
            joined.contains("-device qemu-xhci,id=xhci"),
            "missing xhci controller: {joined}"
        );
        assert!(
            joined.contains(
                "-device usb-host,id=usb-046d-085c,bus=xhci.0,vendorid=0x046d,productid=0x085c"
            ),
            "missing usb-host device: {joined}"
        );
        // The controller is there even with no devices, so hot-plug always works.
        let cfg = VmConfig {
            name: "t".into(),
            cpu: 1,
            ram: 128,
            ..VmConfig::default()
        };
        let (_, args) = build_qemu_args(&cfg, storage.path()).unwrap();
        assert!(
            args.join(" ").contains("qemu-xhci"),
            "xhci controller should be unconditional"
        );
    }

    #[test]
    fn host_usb_device_label() {
        let cases = [
            ("Logitech", "USB Receiver", "Logitech USB Receiver"),
            ("Logitech", "Logitech G600", "Logitech G600"),
            ("", "C922 Pro Stream Webcam", "C922 Pro Stream Webcam"),
            ("FIIO", "", "FIIO"),
            ("", "", "1234:5678"),
        ];
        for (man, prod, want) in cases {
            let h = HostUsbDevice {
                vendor_id: "1234".into(),
                product_id: "5678".into(),
                manufacturer: man.into(),
                product: prod.into(),
                ..HostUsbDevice::default()
            };
            assert_eq!(h.label(), want, "label({man:?}, {prod:?})");
        }
    }

    #[test]
    fn port_order_is_numeric_per_component() {
        assert_eq!(port_cmp("3-2.2", "3-2.10"), Ordering::Less);
        assert_eq!(port_cmp("1-5", "3-2"), Ordering::Less);
        assert_eq!(port_cmp("3-2", "3-2.1"), Ordering::Less);
        assert_eq!(port_cmp("3-2.1", "3-2"), Ordering::Greater);
        assert_eq!(port_cmp("3-2.2.2", "3-2.2.2"), Ordering::Equal);
        assert_eq!(port_cmp("10-1", "9-1"), Ordering::Greater);
    }

    /// Creates a fake sysfs device directory; attributes end in a newline
    /// the way the kernel prints them.
    fn write_sysfs_device(root: &Path, name: &str, attrs: &[(&str, &str)]) {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        for (k, v) in attrs {
            fs::write(dir.join(k), format!("{v}\n")).unwrap();
        }
    }

    /// The fixture tree of `TestListHostUSBDevices`: a root hub, a hub, two
    /// devices behind it (one with an interface dir), and one on bus 1.
    fn fixture_host() -> (tempfile::TempDir, tempfile::TempDir, UsbHost) {
        let (sys, dev_dir) = (tempdir().unwrap(), tempdir().unwrap());
        let s = sys.path();
        write_sysfs_device(
            s,
            "usb3",
            &[
                ("idVendor", "1d6b"),
                ("idProduct", "0002"),
                ("busnum", "3"),
                ("devnum", "1"),
                ("bDeviceClass", "09"),
            ],
        );
        write_sysfs_device(
            s,
            "3-2",
            &[
                ("idVendor", "174c"),
                ("idProduct", "2074"),
                ("busnum", "3"),
                ("devnum", "2"),
                ("bDeviceClass", "09"),
                ("product", "ASM107x"),
            ],
        );
        write_sysfs_device(
            s,
            "3-2.10",
            &[
                ("idVendor", "046d"),
                ("idProduct", "c52b"),
                ("busnum", "3"),
                ("devnum", "17"),
                ("bDeviceClass", "00"),
                ("manufacturer", "Logitech"),
                ("product", "USB Receiver"),
            ],
        );
        write_sysfs_device(
            s,
            "3-2.2",
            &[
                ("idVendor", "046d"),
                ("idProduct", "085c"),
                ("busnum", "3"),
                ("devnum", "16"),
                ("bDeviceClass", "ef"),
                ("product", "C922 Pro Stream Webcam"),
            ],
        );
        write_sysfs_device(s, "3-2.2:1.0", &[("bInterfaceClass", "0e")]);
        write_sysfs_device(
            s,
            "1-5",
            &[
                ("idVendor", "0b05"),
                ("idProduct", "18f3"),
                ("busnum", "1"),
                ("devnum", "3"),
                ("bDeviceClass", "00"),
                ("manufacturer", "AsusTek Computer Inc."),
                ("product", "AURA LED Controller"),
            ],
        );
        // Only the webcam's device node exists and is writable.
        fs::create_dir_all(dev_dir.path().join("003")).unwrap();
        fs::write(dev_dir.path().join("003").join("016"), b"").unwrap();
        let host = UsbHost {
            sysfs_dir: s.to_path_buf(),
            dev_dir: dev_dir.path().to_path_buf(),
        };
        (sys, dev_dir, host)
    }

    #[test]
    fn list_host_usb_devices_from_sysfs() {
        let (_sys, dev_dir, host) = fixture_host();

        let devs = list_host_usb_devices_in(&host).unwrap();
        let ports: Vec<&str> = devs.iter().map(|d| d.port.as_str()).collect();
        // Hubs and interfaces skipped; ordered by bus then port numerically (2 before 10).
        assert_eq!(ports, ["1-5", "3-2.2", "3-2.10"]);
        let cam = &devs[1];
        assert!(cam.writable, "webcam = {cam:?}");
        assert_eq!(cam.label(), "C922 Pro Stream Webcam");
        assert_eq!(cam.id(), "046d:085c");
        assert_eq!(
            cam.dev_node,
            dev_dir.path().join("003").join("016").to_string_lossy()
        );
        assert_eq!((cam.bus, cam.dev), (3, 16));
        assert!(
            !devs[2].writable,
            "device without a node must not be writable"
        );
        assert_eq!(
            devs[2].dev_node,
            dev_dir.path().join("003").join("017").to_string_lossy()
        );

        let states = usb_states_in(&[dev("046d", "c52b"), dev("dead", "beef")], &host);
        assert_eq!(
            states[0].host.as_ref().map(|h| h.port.as_str()),
            Some("3-2.10")
        );
        assert!(states[1].host.is_none(), "states = {states:?}");

        check_usb_access_in(&[dev("046d", "085c")], &host).expect("writable device must pass");
        let err = check_usb_access_in(&[dev("046d", "c52b"), dev("dead", "beef")], &host)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(r#"'ATTR{idVendor}=="046d",' 'ATTR{idProduct}=="c52b",'"#),
            "{err}"
        );
        assert!(!err.contains("dead"), "{err}");
        assert_eq!(
            err,
            format!(
                "no write access to USB device 046d:c52b (Logitech USB Receiver) at {}\n\
                 Grant access to your user by running (then rescan):\n  {}",
                devs[2].dev_node,
                udev_rule_command(&[dev("046d", "c52b")])
            )
        );
    }

    #[test]
    fn list_host_usb_devices_without_sysfs() {
        let missing = tempdir().unwrap().path().join("gone");
        let host = UsbHost {
            sysfs_dir: missing.clone(),
            dev_dir: PathBuf::from("/dev/bus/usb"),
        };
        let err = list_host_usb_devices_in(&host).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "list host USB devices (needs Linux sysfs): open {}",
                missing.display()
            )
        );
        let chain = format!("{err:#}");
        assert!(
            chain.starts_with(&format!(
                "list host USB devices (needs Linux sysfs): open {}: ",
                missing.display()
            )),
            "{chain}"
        );
        assert!(chain.contains("No such file or directory"), "{chain}");
        // No sysfs: nothing is connected and there is nothing to check.
        let states = usb_states_in(&[dev("046d", "085c")], &host);
        assert_eq!(
            states,
            [UsbState {
                device: dev("046d", "085c"),
                host: None
            }]
        );
        check_usb_access_in(&[dev("046d", "085c")], &host).unwrap();
    }

    #[test]
    fn match_usb_claims_pinned_first() {
        let host_dev = |port: &str, v: &str, p: &str| HostUsbDevice {
            vendor_id: v.into(),
            product_id: p.into(),
            port: port.into(),
            ..HostUsbDevice::default()
        };
        let host = [
            host_dev("1-1", "0781", "5583"),
            host_dev("1-2", "0781", "5583"),
            host_dev("1-3", "046d", "085c"),
        ];
        let devs = [
            dev("0781", "5583"),           // unpinned: must not steal 1-2
            pinned("0781", "5583", "1-2"), // pinned
            pinned("046D", "085C", "1-9"), // pinned to an absent port
        ];
        let states = match_usb(&devs, &host);
        assert_eq!(states.len(), 3);
        assert_eq!(
            states[0].host.as_ref().map(|h| h.port.as_str()),
            Some("1-1"),
            "unpinned entry"
        );
        assert_eq!(
            states[1].host.as_ref().map(|h| h.port.as_str()),
            Some("1-2"),
            "pinned entry"
        );
        assert!(
            states[2].host.is_none(),
            "entry pinned to absent port matched {:?}",
            states[2].host
        );
        assert_eq!(states[0].host.as_ref(), Some(&host[0]));
        assert_eq!(states[0].device, devs[0]);
        // Case-insensitive IDs still match when the port is present.
        let states = match_usb(&[pinned("046D", "085C", "1-3")], &host);
        assert_eq!(states[0].host.as_ref(), Some(&host[2]));
        // Without host devices nothing is connected.
        assert!(match_usb(&devs, &[]).iter().all(|s| s.host.is_none()));
    }

    #[test]
    fn udev_rule_command_text() {
        let one = udev_rule_command(&[dev("046D", "085c")]);
        let want = concat!(
            r#"echo 'SUBSYSTEM=="usb",' 'ATTR{idVendor}=="046d",' 'ATTR{idProduct}=="085c",' 'TAG+="uaccess"' "#,
            "| sudo tee -a /etc/udev/rules.d/70-ostrich-usb.rules && sudo udevadm control --reload && sudo udevadm trigger"
        );
        assert_eq!(one, want);
        assert_eq!(
            udev_rule_hint(&[dev("046D", "085c")]),
            format!("Grant access to your user by running (then rescan):\n  {want}")
        );
        let two = udev_rule_command(&[dev("046d", "085c"), dev("0781", "5583")]);
        assert!(
            two.starts_with(
                r#"printf '%s %s %s %s\n' 'SUBSYSTEM=="usb",' 'ATTR{idVendor}=="046d",'"#
            ),
            "two devices: {two}"
        );
        assert!(
            two.contains(r#"'TAG+="uaccess"' 'SUBSYSTEM=="usb",' 'ATTR{idVendor}=="0781",'"#),
            "two devices: {two}"
        );
        assert_eq!(
            udev_rule_command_words(&[dev("046d", "085c")]).len(),
            1 + 4 + 14
        );
        assert_eq!(
            udev_rule_command_words(&[dev("046d", "085c"), dev("0781", "5583")]).len(),
            2 + 8 + 14
        );
    }

    /// Runs the generated pipeline (minus sudo) in a real shell and checks
    /// the file ends up with one well-formed rule per device.
    #[test]
    fn udev_rule_command_produces_rules() {
        for shell in ["sh", "bash", "fish"] {
            let Ok(sh) = which::which(shell) else {
                eprintln!("skipping: {shell} not installed");
                continue;
            };
            let cases: [Vec<UsbDevice>; 2] = [
                vec![dev("046d", "085c")],
                vec![dev("046d", "085c"), dev("0781", "5583")],
            ];
            for (n, devs) in cases.iter().enumerate() {
                let tmp = tempdir().unwrap();
                let rules = tmp.path().join("rules");
                let words = udev_rule_command_words(devs);
                // Keep only the part that writes the file: "<producer> | sudo tee -a FILE".
                let end = words.iter().position(|w| w == "&&").unwrap_or(words.len());
                let cmd = words[..end].join(" ").replace(
                    &format!("sudo tee -a {UDEV_RULES_FILE}"),
                    &format!("tee -a {}", rules.display()),
                );
                // Break the line between words too, as the TUI does.
                let cmd = cmd.replacen(" 'ATTR{idProduct}", " \\\n  'ATTR{idProduct}", 1);
                let out = Command::new(&sh).arg("-c").arg(&cmd).output().unwrap();
                assert!(
                    out.status.success(),
                    "{shell} ({} devices): {}\n{}{}",
                    devs.len(),
                    out.status,
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                );
                let got = fs::read_to_string(&rules).unwrap_or_default();
                let want: String = devs
                    .iter()
                    .map(|d| {
                        format!(
                            "SUBSYSTEM==\"usb\", ATTR{{idVendor}}==\"{}\", ATTR{{idProduct}}==\"{}\", TAG+=\"uaccess\"\n",
                            d.vendor_id, d.product_id
                        )
                    })
                    .collect();
                assert_eq!(got, want, "{shell} (case {n}) wrote:\n{got}\nwant:\n{want}");
            }
        }
    }

    #[test]
    fn usb_device_yaml_quotes_numeric_looking_ids() {
        // yaml.v3 double-quoted ids that would otherwise parse as numbers
        // and accepted them unquoted on the way back in; both halves hold.
        let devs = vec![
            dev("046d", "085c"),
            pinned("0781", "5583", "3-2.2.4"),
            dev("1e10", "0001"),
        ];
        let text = serde_yaml_ng::to_string(&devs).unwrap();
        assert!(text.contains("vendor_id: 046d\n"), "{text}");
        assert!(
            text.contains("vendor_id: '0781'\n") || text.contains("vendor_id: \"0781\"\n"),
            "{text}"
        );
        assert!(
            text.contains("product_id: '0001'\n") || text.contains("product_id: \"0001\"\n"),
            "{text}"
        );
        assert!(
            text.contains("vendor_id: '1e10'\n") || text.contains("vendor_id: \"1e10\"\n"),
            "{text}"
        );
        assert!(
            !text.contains("name:"),
            "empty name must be omitted: {text}"
        );
        let back: Vec<UsbDevice> = serde_yaml_ng::from_str(&text).unwrap();
        assert_eq!(back, devs);
        let unquoted: Vec<UsbDevice> = serde_yaml_ng::from_str(
            "- vendor_id: 0781\n  product_id: 5583\n- vendor_id: 1e10\n  product_id: 0001\n  port: 3-2\n",
        )
        .unwrap();
        assert_eq!(
            unquoted,
            [dev("0781", "5583"), pinned("1e10", "0001", "3-2")]
        );
    }

    #[test]
    fn usb_device_null_keys_load_as_empty_like_yaml_v3() {
        // An explicit null (`null`, `~` or no value) is the zero value, as in
        // Go, not the string "null"; validate() then reports the entry with
        // Go's wording, and a null entry is an empty device.
        let devs: Vec<UsbDevice> = serde_yaml_ng::from_str(
            "- vendor_id: null\n  product_id: ~\n  name:\n  port: null\n- \n- vendor_id: 'null'\n  product_id: 085c\n",
        )
        .unwrap();
        assert_eq!(
            devs,
            [
                UsbDevice::default(),
                UsbDevice::default(),
                dev("null", "085c")
            ]
        );
        assert_eq!(
            devs[0].validate().unwrap_err().to_string(),
            "invalid USB device \"\":\"\" — vendor_id and product_id must be 4 hex digits"
        );
        assert_eq!(
            devs[2].validate().unwrap_err().to_string(),
            "invalid USB device \"null\":\"085c\" — vendor_id and product_id must be 4 hex digits"
        );
    }

    #[test]
    fn hotplug_rejects_bad_index() {
        let storage = tempdir().unwrap();
        let cfg = VmConfig {
            name: "t".into(),
            usb_devices: vec![dev("046d", "085c")],
            ..VmConfig::default()
        };
        let err = usb_hotplug(storage.path(), &cfg, 1)
            .unwrap_err()
            .to_string();
        assert_eq!(err, "no USB device at index 1");
        let err = usb_hotunplug(storage.path(), &cfg, 3)
            .unwrap_err()
            .to_string();
        assert_eq!(err, "no USB device at index 3");
        // An invalid entry is refused before the monitor is touched.
        let cfg = VmConfig {
            name: "t".into(),
            usb_devices: vec![dev("46d", "085c")],
            ..VmConfig::default()
        };
        let err = usb_hotplug(storage.path(), &cfg, 0)
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("invalid USB device"), "{err}");
    }

    // --- Integration tests against a real QEMU ---

    /// Whether every named binary is on PATH; prints why not otherwise.
    fn have_tools(tools: &[&str]) -> bool {
        for tool in tools {
            if which::which(tool).is_err() {
                eprintln!("skipping: {tool} not installed");
                return false;
            }
        }
        true
    }

    /// Polls until the VM's monitor answers, returning `info qtree`.
    fn wait_for_monitor(storage: &Path, name: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if let Ok(out) = monitor_command(storage, name, "info qtree") {
                if !out.is_empty() {
                    return out;
                }
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        panic!(
            "QEMU monitor did not come up (status {:?})",
            status(storage, name)
        );
    }

    /// Polls `info usb` until the device ID is (or is no longer) listed.
    fn wait_for_usb(storage: &Path, name: &str, qemu_id: &str, present: bool) -> String {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut out = String::new();
        while Instant::now() < deadline {
            out = monitor_command(storage, name, "info usb").unwrap_or_default();
            if out.contains(&format!("ID: {qemu_id}")) == present {
                return out;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        panic!("device {qemu_id} present={present} not reached; info usb:\n{out}");
    }

    /// Boots a real (diskless-content, display-less) VM and checks the USB
    /// controller, boot-time usb-host devices and monitor hot-plug against
    /// the actual QEMU.
    #[test]
    fn usb_passthrough_with_qemu() {
        if !have_tools(&["qemu-system-x86_64", "qemu-img"]) {
            return;
        }
        let storage = tempdir().unwrap();
        let storage = storage.path();
        let mut cfg = VmConfig {
            name: "usbtest".into(),
            cpu: 1,
            ram: 128,
            disk_size: 1,
            network: NetworkConfig {
                kind: NetworkType::None,
                ..NetworkConfig::default()
            },
            // Not a real device: QEMU accepts it and waits for it to be plugged in.
            usb_devices: vec![UsbDevice {
                name: "Phantom".into(),
                ..dev("1234", "5678")
            }],
            ..VmConfig::default()
        };
        Manager::new(storage).create(&mut cfg).unwrap();
        start(storage, &cfg).unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let qtree = wait_for_monitor(storage, &cfg.name);
            for want in [
                r#"dev: qemu-xhci, id "xhci""#,
                r#"dev: usb-host, id "usb-1234-5678""#,
            ] {
                assert!(
                    qtree.contains(want),
                    "boot-time device tree lacks {want:?}:\n{qtree}"
                );
            }
            let st = usb_states(&cfg.usb_devices);
            assert!(
                st[0].host.is_none(),
                "phantom device should be reported as not connected, got {:?}",
                st[0].host
            );

            // Hot-plug a second device, then unplug it again.
            cfg.usb_devices.push(pinned("abcd", "ef01", "9-1.2"));
            usb_hotplug(storage, &cfg, 1).expect("hot-plug");
            let qtree = monitor_command(storage, &cfg.name, "info qtree").unwrap_or_default();
            assert!(
                qtree.contains(r#"id "usb-abcd-ef01-9-1.2""#),
                "hot-plugged device missing from device tree:\n{qtree}"
            );
            usb_hotunplug(storage, &cfg, 1).expect("hot-unplug");
            let qtree = monitor_command(storage, &cfg.name, "info qtree").unwrap_or_default();
            assert!(
                !qtree.contains("usb-abcd-ef01"),
                "hot-unplugged device still in device tree:\n{qtree}"
            );

            // QEMU's errors must surface instead of being swallowed.
            let err = usb_hotplug(storage, &cfg, 0).unwrap_err().to_string(); // same ID as the boot-time device
            assert!(
                err.contains("usb-1234-5678"),
                "duplicate ID should fail with QEMU's message, got: {err}"
            );
            let err = monitor_must_succeed(
                storage,
                &cfg.name,
                "device_add usb-host,id=x,bus=nope.0,vendorid=0x1,productid=0x1",
            )
            .unwrap_err()
            .to_string();
            assert!(
                err.contains("nope.0"),
                "bad bus should fail with QEMU's message, got: {err}"
            );

            let hosts = monitor_command(storage, &cfg.name, "info usbhost").unwrap_or_default();
            eprintln!("info usbhost:\n{hosts}");

            stop(storage, &cfg.name).unwrap();
            let info = status(storage, &cfg.name).unwrap();
            assert_eq!(
                info.status,
                VmStatus::Stopped,
                "VM still running after stop: {info:?}"
            );
        }));
        let _ = stop(storage, &cfg.name);
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }

    /// Passes a physical host device through to a VM and checks QEMU
    /// actually claims it. The host loses the device for the duration, so it
    /// only runs for the device named in `OSTRICH_USB_TEST_DEVICE`
    /// (vendor:product), e.g. a webcam — never pick the keyboard or mouse
    /// you are typing on.
    #[test]
    fn usb_real_device_with_qemu() {
        let Ok(id) = std::env::var("OSTRICH_USB_TEST_DEVICE") else {
            eprintln!(
                "skipping: set OSTRICH_USB_TEST_DEVICE=vvvv:pppp to pass a real device through"
            );
            return;
        };
        if id.is_empty() {
            eprintln!(
                "skipping: set OSTRICH_USB_TEST_DEVICE=vvvv:pppp to pass a real device through"
            );
            return;
        }
        if !have_tools(&["qemu-system-x86_64", "qemu-img"]) {
            return;
        }
        let mut device = parse_usb_id(&id).unwrap();
        let st = usb_states(std::slice::from_ref(&device));
        let host = st[0]
            .host
            .as_ref()
            .unwrap_or_else(|| panic!("{id} is not connected to the host"));
        assert!(
            host.writable,
            "{id} is not writable by this user:\n{}",
            udev_rule_hint(std::slice::from_ref(&device))
        );
        device.name = host.label();

        let storage = tempdir().unwrap();
        let storage = storage.path();
        let mut cfg = VmConfig {
            name: "usbreal".into(),
            cpu: 1,
            ram: 128,
            disk_size: 1,
            network: NetworkConfig {
                kind: NetworkType::None,
                ..NetworkConfig::default()
            },
            usb_devices: vec![device.clone()],
            ..VmConfig::default()
        };
        Manager::new(storage).create(&mut cfg).unwrap();
        start(storage, &cfg).unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            wait_for_monitor(storage, &cfg.name);

            // "info usb" only lists a usb-host device once QEMU has opened the
            // host device and attached it to a port, so this proves the claim
            // succeeded.
            let qemu_id = usb_device_ids(&cfg.usb_devices).swap_remove(0);
            let attached = wait_for_usb(storage, &cfg.name, &qemu_id, true);
            eprintln!("attached at boot:\n{attached}");

            usb_hotunplug(storage, &cfg, 0).expect("hot-unplug");
            wait_for_usb(storage, &cfg.name, &qemu_id, false);
            usb_hotplug(storage, &cfg, 0).expect("hot-plug");
            eprintln!(
                "re-attached by hot-plug:\n{}",
                wait_for_usb(storage, &cfg.name, &qemu_id, true)
            );

            stop(storage, &cfg.name).unwrap();
            // The host should have the device back once QEMU is gone.
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                if let Some(h) = &usb_states(std::slice::from_ref(&device))[0].host {
                    eprintln!("host sees the device again at {}", h.dev_node);
                    return;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            panic!("host did not get the device back after the VM stopped");
        }));
        let _ = stop(storage, &cfg.name);
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }
}
