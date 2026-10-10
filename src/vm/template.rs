//! VM templates: a stopped VM's disk, UEFI NVRAM and TPM state frozen as a
//! starting point for new VMs, under `<storage>/.templates/<name>/`.

use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::config::{
    disk_path, firmware_label, firmware_vars_path, load_config, null_as_default, save_config,
    tpm_dir, vm_dir, write_file_atomic, yaml_bool, yaml_timestamp, FirmwareType, NetworkConfig,
    NetworkType, VmConfig,
};
use super::disk::QEMU_IMG_BIN;
use super::firmware::{arch_of, copy_file, ensure_firmware_vars, exit_status_text};
use super::manager::{random_mac, Manager};
use super::process::status;
use super::tpm::check_tpm;

/// The directory under the storage path that holds the templates. It starts
/// with a dot so it can never clash with a VM.
pub(crate) const TEMPLATES_DIR_NAME: &str = ".templates";

/// The YAML schema stored in `<storage>/.templates/<name>/template.yaml`.
///
/// A template is a stopped VM's disk, firmware NVRAM and TPM state frozen as
/// a starting point for new VMs, plus the machine definition those files
/// were made with. It deliberately carries only what defines the machine:
/// the resource defaults a new VM starts from and the firmware the installed
/// OS expects. Anything bound to one VM or to the host — MAC address, port
/// forwards, VNC display, boot ISO, USB devices and images — is left out, as
/// clones would clash over it or boot the installer again; so are additional
/// disks, which hold one VM's data rather than the installed system.
///
/// A hand-written file loads the way Go's yaml.v3 read it: a missing key —
/// `name` included — or an explicit null is the zero value, booleans may be
/// YAML 1.1 words such as `yes` or `on`, and `created_at` may be any
/// timestamp yaml.v3 knew, so the template stays listed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Template {
    #[serde(default, deserialize_with = "null_as_default")]
    pub name: String,
    #[serde(
        default,
        deserialize_with = "null_as_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub description: String,
    /// The VM it was made from (informational).
    #[serde(default, deserialize_with = "null_as_default")]
    pub source_vm: String,
    /// Defaults for a new VM, changeable on creation.
    #[serde(default, deserialize_with = "null_as_default")]
    pub cpu: u32,
    /// MiB.
    #[serde(default, deserialize_with = "null_as_default")]
    pub ram: u32,
    /// GiB; the disk image is copied as is.
    #[serde(default, deserialize_with = "null_as_default")]
    pub disk_size: u32,
    #[serde(default, deserialize_with = "null_as_default")]
    pub arch: String,
    #[serde(default)]
    pub firmware: FirmwareType,
    #[serde(
        default,
        deserialize_with = "yaml_bool",
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub secure_boot: bool,
    #[serde(
        default,
        deserialize_with = "yaml_bool",
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub tpm: bool,
    /// Default for a new VM, changeable on creation. A missing, null, empty
    /// or unknown one is user networking (see [`NetworkType`]).
    #[serde(default)]
    pub network: NetworkType,
    /// The source had a VNC display; a new VM gets a free one.
    #[serde(
        default,
        deserialize_with = "yaml_bool",
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub vnc: bool,
    #[serde(default, deserialize_with = "yaml_timestamp")]
    pub created_at: DateTime<Utc>,
}

impl Template {
    /// Describes the template's boot platform, like [`VmConfig::firmware_label`].
    pub fn firmware_label(&self) -> String {
        firmware_label(self.uefi(), self.secure_boot, self.tpm)
    }

    /// Whether VMs from the template boot UEFI firmware.
    pub fn uefi(&self) -> bool {
        self.firmware == FirmwareType::Uefi || self.secure_boot
    }

    /// The config a VM created from the template starts from: the machine
    /// definition and the resource and network defaults, no name.
    pub fn new_vm_config(&self) -> VmConfig {
        VmConfig {
            cpu: self.cpu,
            ram: self.ram,
            disk_size: self.disk_size,
            arch: self.arch.clone(),
            firmware: self.firmware,
            secure_boot: self.secure_boot,
            tpm: self.tpm,
            network: NetworkConfig {
                kind: self.network,
                ..NetworkConfig::default()
            },
            ..VmConfig::default()
        }
    }
}

// --- Path helpers ---

/// `<storage>/.templates`.
pub fn templates_dir(storage: &Path) -> PathBuf {
    storage.join(TEMPLATES_DIR_NAME)
}

/// The directory that holds all files for a named template.
pub fn template_dir(storage: &Path, name: &str) -> PathBuf {
    templates_dir(storage).join(name)
}

/// `template.yaml`.
pub fn template_file_path(storage: &Path, name: &str) -> PathBuf {
    template_dir(storage, name).join("template.yaml")
}

/// The template's qcow2 disk image.
pub fn template_disk_path(storage: &Path, name: &str) -> PathBuf {
    template_dir(storage, name).join("disk.qcow2")
}

/// The template's copy of the UEFI NVRAM.
pub fn template_firmware_vars_path(storage: &Path, name: &str) -> PathBuf {
    template_dir(storage, name).join("efivars.fd")
}

/// The template's copy of the emulated TPM's state.
pub fn template_tpm_dir(storage: &Path, name: &str) -> PathBuf {
    template_dir(storage, name).join("tpm")
}

// --- YAML I/O ---

/// Reads and parses `template.yaml` for the named template.
pub fn load_template(storage: &Path, name: &str) -> Result<Template> {
    let path = template_file_path(storage, name);
    let data = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let t: Template =
        serde_yaml_ng::from_str(&data).with_context(|| format!("parse {}", path.display()))?;
    Ok(t)
}

/// Serialises and writes `template.yaml`, atomically: a temporary file in
/// the template directory is renamed over it, so it is never left cut short.
pub fn save_template(storage: &Path, t: &Template) -> Result<()> {
    let path = template_file_path(storage, &t.name);
    let data = serde_yaml_ng::to_string(t)?;
    write_file_atomic(&path, data.as_bytes()).with_context(|| format!("write {}", path.display()))
}

/// The size of the template's disk image file on the host, or 0 when it
/// cannot be read.
pub fn template_disk_usage(storage: &Path, name: &str) -> u64 {
    fs::metadata(template_disk_path(storage, name)).map_or(0, |st| st.len())
}

// --- Manager operations ---

impl Manager {
    /// All valid templates, sorted by name; none when there is no templates
    /// directory, malformed entries skipped.
    pub fn list_templates(&self) -> Result<Vec<Template>> {
        let dir = templates_dir(&self.storage);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err).with_context(|| format!("read {}", dir.display())),
        };
        let mut tpls: Vec<Template> = entries
            .filter_map(|entry| {
                let entry = entry.ok()?;
                if !entry.file_type().ok()?.is_dir() {
                    return None;
                }
                let name = entry.file_name();
                // Skip malformed entries silently, like `list` does.
                load_template(&self.storage, name.to_str()?).ok()
            })
            .collect();
        tpls.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(tpls)
    }

    /// Whether a template directory and its file exist.
    pub fn template_exists(&self, name: &str) -> bool {
        template_file_path(&self.storage, name).exists()
    }

    /// Freezes the stopped VM `vm_name` as the template `tpl_name`: the disk
    /// image (`qemu-img convert`), UEFI NVRAM and TPM state are copied.
    /// Refuses a running VM; nothing half-made is left behind on failure.
    ///
    /// The copies make the template independent of the VM, so the VM can go
    /// on being used or be deleted. The VM must be stopped: a disk copied
    /// under a running guest would be as inconsistent as one pulled from a
    /// machine mid-write. It is up to the user to shut the guest down from
    /// inside first; killing QEMU would leave the filesystem dirty just the
    /// same.
    pub fn create_template(&self, vm_name: &str, tpl_name: &str, description: &str) -> Result<()> {
        let cfg = load_config(&self.storage, vm_name).context("load VM config")?;
        if matches!(status(&self.storage, vm_name), Ok(info) if info.running()) {
            bail!("VM {vm_name:?} is running — shut it down from inside the guest first, so the disk is in a consistent state");
        }
        if self.template_exists(tpl_name) {
            bail!("a template named {tpl_name:?} already exists");
        }
        if which::which(QEMU_IMG_BIN).is_err() {
            bail!("{QEMU_IMG_BIN:?} not found in PATH — is QEMU installed?");
        }

        let dir = template_dir(&self.storage, tpl_name);
        mkdir_all(&dir, 0o755)
            .with_context(|| format!("create template directory {}", dir.display()))?;
        // Nothing half-made is left behind: a failed copy takes the directory
        // with it, so the template list never shows a template with no disk.
        let mut guard = RemoveOnDrop::new(&dir);

        copy_disk(
            &disk_path(&self.storage, vm_name),
            &template_disk_path(&self.storage, tpl_name),
        )?;
        if cfg.uefi() {
            copy_if_exists(
                &firmware_vars_path(&self.storage, vm_name),
                &template_firmware_vars_path(&self.storage, tpl_name),
            )
            .context("copy UEFI NVRAM")?;
        }
        if cfg.tpm {
            copy_dir_if_exists(
                &tpm_dir(&self.storage, vm_name),
                &template_tpm_dir(&self.storage, tpl_name),
            )
            .context("copy TPM state")?;
        }

        let t = Template {
            name: tpl_name.to_string(),
            description: description.to_string(),
            source_vm: vm_name.to_string(),
            cpu: cfg.cpu,
            ram: cfg.ram,
            disk_size: cfg.disk_size,
            arch: arch_of(&cfg).to_string(),
            firmware: cfg.firmware,
            secure_boot: cfg.secure_boot,
            tpm: cfg.tpm,
            network: cfg.network.kind,
            vnc: cfg.vnc_port > 0,
            created_at: Utc::now(),
        };
        save_template(&self.storage, &t).context("save template")?;
        guard.disarm();
        Ok(())
    }

    /// Makes a new VM from the template. `cfg` carries the user's choices —
    /// name, cpu, ram, network and vnc_port — and the template decides the
    /// rest; the disk image is a copy; the MAC is new unless `cfg` sets one.
    ///
    /// The template decides what the installed OS depends on: disk,
    /// architecture, firmware and TPM. The disk image is a copy, so the new
    /// VM is independent of the template.
    pub fn create_from_template(&self, tpl_name: &str, cfg: &mut VmConfig) -> Result<()> {
        let t = load_template(&self.storage, tpl_name).context("load template")?;
        if self.exists(&cfg.name) {
            bail!("a VM named {:?} already exists", cfg.name);
        }
        if which::which(QEMU_IMG_BIN).is_err() {
            bail!("{QEMU_IMG_BIN:?} not found in PATH — is QEMU installed?");
        }

        cfg.disk_size = t.disk_size;
        cfg.disks.clear(); // the template carries the main disk only
        cfg.arch = t.arch;
        cfg.firmware = t.firmware;
        cfg.secure_boot = t.secure_boot;
        cfg.tpm = t.tpm;
        cfg.created_at = Utc::now();
        if cfg.arch.is_empty() {
            cfg.arch = "x86_64".to_string();
        }
        if cfg.network.mac.is_empty() {
            cfg.network.mac = random_mac();
        }
        if cfg.tpm {
            check_tpm()?;
        }

        let vm_dir = vm_dir(&self.storage, &cfg.name);
        mkdir_all(&vm_dir, 0o755)
            .with_context(|| format!("create VM directory {}", vm_dir.display()))?;
        let mut guard = RemoveOnDrop::new(&vm_dir);

        copy_disk(
            &template_disk_path(&self.storage, tpl_name),
            &disk_path(&self.storage, &cfg.name),
        )?;
        // The NVRAM holds the boot entries the installed OS was registered
        // with, so the clone boots the way the source did. A template without
        // one (made by hand) gets a fresh store below.
        if cfg.uefi() {
            copy_if_exists(
                &template_firmware_vars_path(&self.storage, tpl_name),
                &firmware_vars_path(&self.storage, &cfg.name),
            )
            .context("copy UEFI NVRAM")?;
            ensure_firmware_vars(&self.storage, cfg)?;
        }
        // The TPM state goes with the disk: a guest that sealed secrets to
        // the TPM (BitLocker, Windows Hello) finds them where it left them.
        if cfg.tpm {
            copy_dir_if_exists(
                &template_tpm_dir(&self.storage, tpl_name),
                &tpm_dir(&self.storage, &cfg.name),
            )
            .context("copy TPM state")?;
        }

        save_config(&self.storage, cfg).context("save VM config")?;
        guard.disarm();
        Ok(())
    }

    /// Removes the template and its files (`no template named "<n>"` when
    /// there is none). VMs made from it are not affected.
    pub fn delete_template(&self, name: &str) -> Result<()> {
        if !self.template_exists(name) {
            bail!("no template named {name:?}");
        }
        let dir = template_dir(&self.storage, name);
        fs::remove_dir_all(&dir).with_context(|| format!("remove {}", dir.display()))
    }

    /// The lowest VNC display number (1–99) no VM uses, or 0 when all are taken.
    pub fn free_vnc_display(&self) -> u16 {
        let cfgs = self.list().unwrap_or_default();
        (1..=99)
            .find(|n| cfgs.iter().all(|c| c.vnc_port != *n))
            .unwrap_or(0)
    }
}

/// Removes a directory when dropped unless disarmed: the Go `defer` that
/// takes a half-made template or VM directory away on any failure.
struct RemoveOnDrop<'a> {
    dir: &'a Path,
    armed: bool,
}

impl<'a> RemoveOnDrop<'a> {
    fn new(dir: &'a Path) -> Self {
        RemoveOnDrop { dir, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for RemoveOnDrop<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_dir_all(self.dir);
        }
    }
}

/// `mkdir -p` with an explicit mode for the directories it makes (subject
/// to the umask); an existing directory is left alone.
fn mkdir_all(dir: &Path, mode: u32) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(mode).create(dir)
}

/// An I/O error that names the path it happened on, the way Go's
/// `os.PathError` does, for the callers that wrap it with a context.
fn at(path: &Path, err: io::Error) -> io::Error {
    io::Error::new(err.kind(), format!("{}: {err}", path.display()))
}

// --- file copying ---

/// Copies a qcow2 image with `qemu-img convert -f qcow2 -O qcow2 <src> <dst>`.
/// Errors: `disk image: <stat error>`, `copy disk image: <status>\n<output>`
/// with the status in Go's words (`exit status 1`).
///
/// Converting is the way qemu-img clones images: only allocated clusters are
/// read and written, clusters that have been freed or zeroed are left out,
/// and the result is a tidy image regardless of how fragmented the source
/// had become. The virtual disk size is unchanged. The source must not be
/// in use.
pub(crate) fn copy_disk(src: &Path, dst: &Path) -> Result<()> {
    fs::metadata(src).with_context(|| format!("disk image {}", src.display()))?;
    let out = Command::new(QEMU_IMG_BIN)
        .args(["convert", "-f", "qcow2", "-O", "qcow2"])
        .arg(src)
        .arg(dst)
        .output()
        .with_context(|| format!("copy disk image: run {QEMU_IMG_BIN}"))?;
    if !out.status.success() {
        bail!(
            "copy disk image: {}\n{}{}",
            exit_status_text(out.status),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

/// Copies `src` to `dst` when `src` is there; a missing `src` is not an error.
pub(crate) fn copy_if_exists(src: &Path, dst: &Path) -> io::Result<()> {
    match fs::metadata(src) {
        Ok(_) => copy_file(src, dst),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(at(src, err)),
    }
}

/// Copies the regular files under `src` into `dst`, recursively, creating
/// `dst`. A missing `src` is not an error; sockets and pipes are skipped.
pub(crate) fn copy_dir_if_exists(src: &Path, dst: &Path) -> io::Result<()> {
    let info = match fs::metadata(src) {
        Ok(info) => info,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(at(src, err)),
    };
    if !info.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("{} is not a directory", src.display()),
        ));
    }
    copy_tree(src, dst)
}

/// One step of the walk behind [`copy_dir_if_exists`]: `path` is looked at
/// without following symlinks, directories are made at `target` with the
/// source's permissions plus owner rwx and descended into in name order,
/// regular files are copied, and anything else is left out.
fn copy_tree(path: &Path, target: &Path) -> io::Result<()> {
    let fi = fs::symlink_metadata(path).map_err(|err| at(path, err))?;
    if fi.is_dir() {
        mkdir_all(target, (fi.permissions().mode() & 0o777) | 0o700)
            .map_err(|err| at(target, err))?;
        let mut entries = fs::read_dir(path)
            .and_then(|it| it.collect::<io::Result<Vec<_>>>())
            .map_err(|err| at(path, err))?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            copy_tree(&entry.path(), &target.join(entry.file_name()))?;
        }
    } else if fi.is_file() {
        copy_file(path, target)?;
    }
    // Sockets, pipes, symlinks: runtime leftovers, never state.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::config::{console_path, PortForward};
    use crate::vm::usb::UsbDevice;
    use crate::vm::usbimage::UsbImage;

    /// Says so and returns false when qemu-img is not installed.
    fn have_qemu_img() -> bool {
        if which::which(QEMU_IMG_BIN).is_err() {
            eprintln!("skipping: {QEMU_IMG_BIN} not installed");
            return false;
        }
        true
    }

    /// Creates a VM directory with a small qcow2 disk and vm.yaml, the way
    /// `Manager::create` would, without touching firmware or TPM packages.
    fn new_test_vm(mgr: &Manager, cfg: &VmConfig) {
        fs::create_dir_all(vm_dir(&mgr.storage, &cfg.name)).unwrap();
        make_disk(&disk_path(&mgr.storage, &cfg.name));
        save_config(&mgr.storage, cfg).unwrap();
    }

    fn make_disk(path: &Path) {
        let out = Command::new(QEMU_IMG_BIN)
            .args(["create", "-f", "qcow2"])
            .arg(path)
            .arg("64M")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "qemu-img create: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// The image's virtual size in bytes, per `qemu-img info`. The top-level
    /// object is the image; newer qemu-img also nests its backing file's
    /// info, with a virtual-size of its own, so the JSON is parsed properly.
    fn virtual_size(path: &Path) -> u64 {
        let out = Command::new(QEMU_IMG_BIN)
            .args(["info", "--output=json"])
            .arg(path)
            .output()
            .unwrap();
        assert!(out.status.success(), "qemu-img info {}", path.display());
        let info: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        let size = info["virtual-size"].as_u64().unwrap_or(0);
        assert!(
            size > 0,
            "no virtual-size in {}",
            String::from_utf8_lossy(&out.stdout)
        );
        size
    }

    fn epoch() -> DateTime<Utc> {
        DateTime::<Utc>::default()
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    #[test]
    fn create_template_and_vm_from_it() {
        if !have_qemu_img() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mgr = Manager::new(dir.path());
        let src = VmConfig {
            name: "win11".into(),
            cpu: 4,
            ram: 8192,
            disk_size: 64,
            arch: "x86_64".into(),
            cdrom_path: "/isos/win11.iso".into(),
            firmware: FirmwareType::Uefi,
            secure_boot: true,
            tpm: true,
            network: NetworkConfig {
                kind: NetworkType::User,
                mac: "52:54:00:00:00:01".into(),
                port_forwards: vec![PortForward {
                    host: 3389,
                    guest: 3389,
                    proto: "tcp".into(),
                }],
            },
            vnc_port: 1,
            usb_devices: vec![UsbDevice {
                vendor_id: "046d".into(),
                product_id: "085c".into(),
                ..UsbDevice::default()
            }],
            usb_images: vec![UsbImage {
                path: "/isos/virtio-win.iso".into(),
            }],
            ..VmConfig::default()
        };
        new_test_vm(&mgr, &src);
        let s = mgr.storage();
        // Stand-ins for the UEFI NVRAM and TPM state the VM would have.
        fs::write(firmware_vars_path(s, "win11"), "nvram").unwrap();
        fs::create_dir_all(tpm_dir(s, "win11")).unwrap();
        fs::write(tpm_dir(s, "win11").join("tpm2-00.permall"), "tpm").unwrap();
        // Runtime leftovers that must not end up in the template.
        fs::write(console_path(s, "win11"), "boot log").unwrap();

        mgr.create_template("win11", "win11-base", "Windows 11 with updates")
            .unwrap();

        let tpl = load_template(s, "win11-base").unwrap();
        let want = Template {
            name: "win11-base".into(),
            description: "Windows 11 with updates".into(),
            source_vm: "win11".into(),
            cpu: 4,
            ram: 8192,
            disk_size: 64,
            arch: "x86_64".into(),
            firmware: FirmwareType::Uefi,
            secure_boot: true,
            tpm: true,
            network: NetworkType::User,
            vnc: true,
            created_at: epoch(),
        };
        let mut got = tpl.clone();
        got.created_at = want.created_at;
        assert_eq!(got, want);
        assert_ne!(tpl.created_at, epoch(), "created_at not set");
        assert_eq!(tpl.firmware_label(), "UEFI + Secure Boot, TPM 2.0");
        // Per-VM and host-bound settings are not written into template.yaml.
        let raw = read(&template_file_path(s, "win11-base"));
        for forbidden in [
            "mac",
            "port_forwards",
            "cdrom",
            "usb",
            "vnc_port",
            "3389",
            "52:54:00",
        ] {
            assert!(
                !raw.contains(forbidden),
                "template.yaml carries {forbidden:?}:\n{raw}"
            );
        }

        // The files that make up the machine are copied; runtime files are not.
        assert_eq!(
            virtual_size(&template_disk_path(s, "win11-base")),
            virtual_size(&disk_path(s, "win11"))
        );
        assert_ne!(template_disk_usage(s, "win11-base"), 0);
        assert_eq!(read(&template_firmware_vars_path(s, "win11-base")), "nvram");
        assert_eq!(
            read(&template_tpm_dir(s, "win11-base").join("tpm2-00.permall")),
            "tpm"
        );
        assert!(
            !template_dir(s, "win11-base").join("console.log").exists(),
            "console.log copied into the template"
        );

        // Templates are listed apart from VMs, and the VM list is not
        // confused by the templates directory.
        let tpls = mgr.list_templates().unwrap();
        assert_eq!(tpls.len(), 1);
        assert_eq!(tpls[0].name, "win11-base");
        let vms = mgr.list().unwrap();
        assert_eq!(vms.len(), 1);
        assert_eq!(vms[0].name, "win11");
        assert!(mgr.template_exists("win11-base"));
        assert!(!mgr.template_exists("nope"));
        assert!(!mgr.exists("win11-base"));
        let err = mgr
            .create_template("win11", "win11-base", "")
            .unwrap_err()
            .to_string();
        assert!(err.contains("already exists"), "duplicate template: {err}");

        // A new VM from the template: the user's choices on top of the
        // template's machine. The source VM's display 1 is taken, so the
        // clone gets display 2.
        assert_eq!(mgr.free_vnc_display(), 2);
        let mut cfg = tpl.new_vm_config();
        cfg.name = "win11-test".into();
        cfg.cpu = 2;
        cfg.ram = 4096;
        cfg.network = NetworkConfig {
            kind: NetworkType::Tap,
            ..NetworkConfig::default()
        };
        cfg.vnc_port = mgr.free_vnc_display();
        mgr.create_from_template("win11-base", &mut cfg).unwrap();
        let clone = load_config(s, "win11-test").unwrap();
        assert_eq!(
            (clone.cpu, clone.ram, clone.network.kind, clone.vnc_port),
            (2, 4096, NetworkType::Tap, 2),
            "user's choices lost"
        );
        assert_eq!(
            (
                clone.disk_size,
                clone.arch.as_str(),
                clone.firmware,
                clone.secure_boot,
                clone.tpm
            ),
            (64, "x86_64", FirmwareType::Uefi, true, true),
            "template's machine lost"
        );
        assert!(
            !clone.network.mac.is_empty() && clone.network.mac != src.network.mac,
            "MAC = {:?}, want a new one",
            clone.network.mac
        );
        assert!(
            clone.cdrom_path.is_empty()
                && clone.network.port_forwards.is_empty()
                && clone.usb_devices.is_empty()
                && clone.usb_images.is_empty(),
            "per-VM settings leaked into the clone: {clone:?}"
        );
        assert_ne!(clone.created_at, epoch(), "created_at not set on the clone");
        assert_eq!(
            virtual_size(&disk_path(s, "win11-test")),
            virtual_size(&disk_path(s, "win11"))
        );
        assert_eq!(read(&firmware_vars_path(s, "win11-test")), "nvram");
        assert_eq!(
            read(&tpm_dir(s, "win11-test").join("tpm2-00.permall")),
            "tpm"
        );
        let err = mgr
            .create_from_template("win11-base", &mut cfg)
            .unwrap_err()
            .to_string();
        assert!(err.contains("already exists"), "duplicate VM: {err}");
        // The second clone gets the next free display.
        assert_eq!(mgr.free_vnc_display(), 3);

        // Deleting the template leaves the clone, a full copy, alone.
        mgr.delete_template("win11-base").unwrap();
        assert!(!mgr.template_exists("win11-base"));
        assert!(
            disk_path(s, "win11-test").exists(),
            "clone disk gone with the template"
        );
        assert!(mgr.delete_template("win11-base").is_err());
    }

    #[test]
    fn create_template_refuses_running_vm() {
        if !have_qemu_img() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mgr = Manager::new(dir.path());
        let cfg = VmConfig {
            name: "deb".into(),
            cpu: 1,
            ram: 512,
            disk_size: 8,
            ..VmConfig::default()
        };
        new_test_vm(&mgr, &cfg);
        let qemu = crate::vm::process::testutil::FakeQemu::spawn();
        qemu.run_as(mgr.storage(), "deb");
        let err = mgr
            .create_template("deb", "deb-base", "")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("running") && err.contains("shut it down"),
            "running VM accepted: {err}"
        );
        assert!(
            !template_dir(mgr.storage(), "deb-base").exists(),
            "a template directory was left behind"
        );
        assert!(mgr.list_templates().unwrap().is_empty());
    }

    #[test]
    fn create_template_leaves_nothing_on_failure() {
        if !have_qemu_img() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mgr = Manager::new(dir.path());
        let cfg = VmConfig {
            name: "nodisk".into(),
            cpu: 1,
            ram: 512,
            disk_size: 8,
            ..VmConfig::default()
        };
        fs::create_dir_all(vm_dir(mgr.storage(), "nodisk")).unwrap();
        save_config(mgr.storage(), &cfg).unwrap();
        let err = mgr
            .create_template("nodisk", "broken", "")
            .unwrap_err()
            .to_string();
        assert!(err.contains("disk image"), "missing disk accepted: {err}");
        assert!(
            !template_dir(mgr.storage(), "broken").exists(),
            "a template directory was left behind"
        );
    }

    #[test]
    fn template_defaults_and_bios() {
        if !have_qemu_img() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mgr = Manager::new(dir.path());
        let s = mgr.storage();
        // A hand-written vm.yaml with the optional fields left out.
        let cfg = VmConfig {
            name: "min".into(),
            cpu: 1,
            ram: 512,
            disk_size: 8,
            ..VmConfig::default()
        };
        new_test_vm(&mgr, &cfg);
        mgr.create_template("min", "min-base", "").unwrap();
        let tpl = load_template(s, "min-base").unwrap();
        assert_eq!(tpl.arch, "x86_64");
        assert_eq!(tpl.firmware, FirmwareType::Bios);
        assert_eq!(tpl.network, NetworkType::User);
        assert!(!tpl.vnc && !tpl.uefi(), "defaults not filled in: {tpl:?}");
        assert_eq!(tpl.firmware_label(), "BIOS");
        for p in [
            template_firmware_vars_path(s, "min-base"),
            template_tpm_dir(s, "min-base"),
        ] {
            assert!(
                !p.exists(),
                "{} made for a BIOS VM without TPM",
                p.display()
            );
        }

        let mut clone = tpl.new_vm_config();
        clone.name = "min-2".into();
        mgr.create_from_template("min-base", &mut clone).unwrap();
        let got = load_config(s, "min-2").unwrap();
        assert_eq!(
            (
                got.cpu,
                got.ram,
                got.network.kind,
                got.vnc_port,
                got.firmware
            ),
            (1, 512, NetworkType::User, 0, FirmwareType::Bios),
            "clone = {got:?}"
        );
        assert!(
            !firmware_vars_path(s, "min-2").exists(),
            "NVRAM made for a BIOS clone"
        );
    }

    #[test]
    fn copy_dir_if_exists_copies_regular_files() {
        let src_dir = tempfile::tempdir().unwrap();
        let dst_dir = tempfile::tempdir().unwrap();
        let (src, dst) = (src_dir.path(), dst_dir.path().join("copy"));
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::set_permissions(src.join("sub"), fs::Permissions::from_mode(0o700)).unwrap();
        let files = [("a", "A"), ("sub/b", "B"), (".lock", "")];
        for (name, content) in files {
            fs::write(src.join(name), content).unwrap();
            fs::set_permissions(src.join(name), fs::Permissions::from_mode(0o600)).unwrap();
        }
        copy_dir_if_exists(src, &dst).unwrap();
        for (name, content) in files {
            assert_eq!(read(&dst.join(name)), content, "{name}");
        }
        copy_dir_if_exists(&src.join("missing"), &dst).expect("missing source should be fine");
        assert!(
            copy_dir_if_exists(&src.join("a"), &dst).is_err(),
            "a file as source should fail"
        );
    }

    // --- Pieces that stand on their own, so they are checked before the
    // rest of the crate is in place. ---

    #[test]
    fn template_methods() {
        let mut t = Template {
            name: "t".into(),
            description: String::new(),
            source_vm: "v".into(),
            cpu: 2,
            ram: 2048,
            disk_size: 20,
            arch: "aarch64".into(),
            firmware: FirmwareType::Bios,
            secure_boot: false,
            tpm: false,
            network: NetworkType::Tap,
            vnc: true,
            created_at: Utc::now(),
        };
        assert!(!t.uefi());
        assert_eq!(t.firmware_label(), "BIOS");
        t.tpm = true;
        assert_eq!(t.firmware_label(), "BIOS, TPM 2.0");
        t.firmware = FirmwareType::Uefi;
        assert!(t.uefi());
        assert_eq!(t.firmware_label(), "UEFI, TPM 2.0");
        // Secure Boot implies UEFI even when firmware says bios.
        t.firmware = FirmwareType::Bios;
        t.secure_boot = true;
        assert!(t.uefi());
        assert_eq!(t.firmware_label(), "UEFI + Secure Boot, TPM 2.0");

        // The machine definition and defaults, nothing bound to one VM.
        let cfg = t.new_vm_config();
        assert_eq!(
            cfg,
            VmConfig {
                cpu: 2,
                ram: 2048,
                disk_size: 20,
                arch: "aarch64".into(),
                firmware: FirmwareType::Bios,
                secure_boot: true,
                tpm: true,
                network: NetworkConfig {
                    kind: NetworkType::Tap,
                    ..NetworkConfig::default()
                },
                ..VmConfig::default()
            }
        );
        assert!(cfg.name.is_empty() && cfg.network.mac.is_empty() && cfg.vnc_port == 0);
        assert_eq!(cfg.created_at, epoch());
    }

    #[test]
    fn paths() {
        let s = Path::new("/vms");
        assert_eq!(templates_dir(s), PathBuf::from("/vms/.templates"));
        assert_eq!(template_dir(s, "a"), PathBuf::from("/vms/.templates/a"));
        assert_eq!(
            template_file_path(s, "a"),
            PathBuf::from("/vms/.templates/a/template.yaml")
        );
        assert_eq!(
            template_disk_path(s, "a"),
            PathBuf::from("/vms/.templates/a/disk.qcow2")
        );
        assert_eq!(
            template_firmware_vars_path(s, "a"),
            PathBuf::from("/vms/.templates/a/efivars.fd")
        );
        assert_eq!(
            template_tpm_dir(s, "a"),
            PathBuf::from("/vms/.templates/a/tpm")
        );
    }

    #[test]
    fn yaml_written_by_the_go_version_loads() {
        // The README example, plus the nanosecond local-offset timestamp
        // yaml.v3 writes.
        let yaml = "name: debian-12-base
description: Debian 12 with docker and my dotfiles
source_vm: debian-12
cpu: 2
ram: 2048
disk_size: 20
arch: x86_64
firmware: uefi
secure_boot: false
tpm: true
network: user
vnc: true
created_at: 2026-10-09T13:33:00.123456789+02:00
";
        let dir = tempfile::tempdir().unwrap();
        let s = dir.path();
        fs::create_dir_all(template_dir(s, "debian-12-base")).unwrap();
        fs::write(template_file_path(s, "debian-12-base"), yaml).unwrap();
        let t = load_template(s, "debian-12-base").unwrap();
        assert_eq!(
            t,
            Template {
                name: "debian-12-base".into(),
                description: "Debian 12 with docker and my dotfiles".into(),
                source_vm: "debian-12".into(),
                cpu: 2,
                ram: 2048,
                disk_size: 20,
                arch: "x86_64".into(),
                firmware: FirmwareType::Uefi,
                secure_boot: false,
                tpm: true,
                network: NetworkType::User,
                vnc: true,
                created_at: "2026-10-09T11:33:00.123456789Z".parse().unwrap(),
            }
        );
        assert_eq!(t.firmware_label(), "UEFI, TPM 2.0");

        // And it round-trips through save, in the Go field order, with the
        // empty and false fields left out.
        save_template(s, &t).unwrap();
        assert_eq!(load_template(s, "debian-12-base").unwrap(), t);
        let text = read(&template_file_path(s, "debian-12-base"));
        assert_eq!(
            text,
            "name: debian-12-base
description: Debian 12 with docker and my dotfiles
source_vm: debian-12
cpu: 2
ram: 2048
disk_size: 20
arch: x86_64
firmware: uefi
tpm: true
network: user
vnc: true
created_at: 2026-10-09T11:33:00.123456789Z
"
        );
    }

    #[test]
    fn minimal_and_odd_yaml_is_tolerated() {
        let dir = tempfile::tempdir().unwrap();
        let s = dir.path();
        // A hand-written file: no firmware, network, description or
        // timestamp, an unknown key, and tap networking.
        fs::create_dir_all(template_dir(s, "x")).unwrap();
        fs::write(
            template_file_path(s, "x"),
            "name: x\nsource_vm: y\ncpu: 1\nram: 512\ndisk_size: 5\narch: x86_64\nnetwork: tap\ncomment: by hand\n",
        )
        .unwrap();
        let t = load_template(s, "x").unwrap();
        assert_eq!(t.firmware, FirmwareType::Bios);
        assert_eq!(t.network, NetworkType::Tap);
        assert!(!t.uefi() && !t.secure_boot && !t.tpm && !t.vnc);
        assert!(t.description.is_empty());
        assert_eq!(t.created_at, epoch());

        // The minimal template as written looks like the Go version's.
        let min = Template {
            name: "min-base".into(),
            description: String::new(),
            source_vm: "min".into(),
            cpu: 1,
            ram: 512,
            disk_size: 8,
            arch: "x86_64".into(),
            firmware: FirmwareType::Bios,
            secure_boot: false,
            tpm: false,
            network: NetworkType::User,
            vnc: false,
            created_at: "2026-10-09T13:33:00Z".parse().unwrap(),
        };
        fs::create_dir_all(template_dir(s, "min-base")).unwrap();
        save_template(s, &min).unwrap();
        assert_eq!(
            read(&template_file_path(s, "min-base")),
            "name: min-base\nsource_vm: min\ncpu: 1\nram: 512\ndisk_size: 8\narch: x86_64\nfirmware: bios\nnetwork: user\ncreated_at: 2026-10-09T13:33:00Z\n"
        );
    }

    #[test]
    fn hand_written_forms_load_like_yaml_v3() {
        // What yaml.v3 accepted in a hand-written template.yaml: YAML 1.1
        // booleans, its other timestamp spellings, no name, explicit nulls.
        let dir = tempfile::tempdir().unwrap();
        let s = dir.path();
        let load = |yaml: &str| {
            fs::create_dir_all(template_dir(s, "h")).unwrap();
            fs::write(template_file_path(s, "h"), yaml).unwrap();
            load_template(s, "h")
        };

        let t = load("name: h\ntpm: yes\nsecure_boot: on\nvnc: Y\n").unwrap();
        assert!(t.tpm && t.secure_boot && t.vnc, "{t:?}");
        let t = load("name: h\ntpm: No\nsecure_boot: OFF\nvnc: n\n").unwrap();
        assert!(!t.tpm && !t.secure_boot && !t.vnc, "{t:?}");
        let t = load("name: h\ntpm: True\nvnc: FALSE\nsecure_boot: \"yes\"\n").unwrap();
        assert!(t.tpm && !t.vnc && t.secure_boot, "{t:?}");
        // What yaml.v3 refused stays refused.
        assert!(load("name: h\ntpm: yEs\n").is_err());
        assert!(load("name: h\ntpm: \"true\"\n").is_err());
        assert!(load("name: h\ntpm: 1\n").is_err());

        for (stamp, want) in [
            ("2026-10-09", "2026-10-09T00:00:00Z"),
            ("2026-10-09 13:33:00", "2026-10-09T13:33:00Z"),
            ("2026-10-09 13:33:00.25", "2026-10-09T13:33:00.250Z"),
            ("2026-1-9t3:3:0+02:00", "2026-01-09T01:03:00Z"),
            ("2026-1-9T3:3:0Z", "2026-01-09T03:03:00Z"),
            ("2026-10-09T13:33:00.5+02:00", "2026-10-09T11:33:00.500Z"),
        ] {
            let t = load(&format!("name: h\ncreated_at: {stamp}\n"))
                .unwrap_or_else(|e| panic!("{stamp}: {e:#}"));
            assert_eq!(
                t.created_at
                    .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
                want,
                "{stamp}"
            );
        }
        assert!(load("name: h\ncreated_at: 2026-10-09T13:33:00\n").is_err());
        assert!(load("name: h\ncreated_at: yesterday\n").is_err());

        // No name and explicit nulls: Go's zero values, not "null" or "~".
        let t = load(
            "description: null\nsource_vm: ~\ncpu: null\nram:\narch: ~\ntpm: null\nvnc:\ncreated_at: null\n",
        )
        .unwrap();
        assert_eq!(t, blank(""));
        let t = load("name: h\ndescription: 'null'\n").unwrap();
        assert_eq!(t.description, "null", "a quoted null is a string");

        // A missing, null, empty or unknown network type is user networking,
        // as Go's forms took it; only tap and none are anything else.
        for (yaml, want) in [
            ("name: h\n", NetworkType::User),
            ("name: h\nnetwork:\n", NetworkType::User),
            ("name: h\nnetwork: ~\n", NetworkType::User),
            ("name: h\nnetwork: null\n", NetworkType::User),
            ("name: h\nnetwork: ''\n", NetworkType::User),
            ("name: h\nnetwork: user\n", NetworkType::User),
            ("name: h\nnetwork: bridge\n", NetworkType::User),
            ("name: h\nnetwork: tap\n", NetworkType::Tap),
            ("name: h\nnetwork: none\n", NetworkType::None),
        ] {
            let t = load(yaml).unwrap_or_else(|e| panic!("{yaml}: {e:#}"));
            assert_eq!(t.network, want, "{yaml}");
            assert_eq!(t.new_vm_config().network.kind, want, "{yaml}");
        }

        // And such a template is listed rather than dropped.
        let names: Vec<String> = Manager::new(s)
            .list_templates()
            .unwrap()
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(names, ["h"]);
    }

    #[test]
    fn load_template_errors_name_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let s = dir.path();
        let err = load_template(s, "nope").unwrap_err().to_string();
        assert!(
            err.starts_with(&format!("read {}", template_file_path(s, "nope").display())),
            "{err}"
        );
        fs::create_dir_all(template_dir(s, "bad")).unwrap();
        fs::write(template_file_path(s, "bad"), "name: [unclosed\n").unwrap();
        let err = load_template(s, "bad").unwrap_err().to_string();
        assert!(
            err.starts_with(&format!("parse {}", template_file_path(s, "bad").display())),
            "{err}"
        );
        // Saving into a template directory that does not exist fails the
        // same way; the caller makes the directory.
        let t = blank("nodir");
        let err = save_template(s, &t).unwrap_err().to_string();
        assert!(
            err.starts_with(&format!(
                "write {}",
                template_file_path(s, "nodir").display()
            )),
            "{err}"
        );
    }

    /// A template with only a name, for tests that do not care about the rest.
    fn blank(name: &str) -> Template {
        Template {
            name: name.into(),
            description: String::new(),
            source_vm: String::new(),
            cpu: 0,
            ram: 0,
            disk_size: 0,
            arch: String::new(),
            firmware: FirmwareType::Bios,
            secure_boot: false,
            tpm: false,
            network: NetworkType::User,
            vnc: false,
            created_at: epoch(),
        }
    }

    #[test]
    fn template_disk_usage_is_the_apparent_size() {
        let dir = tempfile::tempdir().unwrap();
        let s = dir.path();
        assert_eq!(template_disk_usage(s, "none"), 0);
        fs::create_dir_all(template_dir(s, "t")).unwrap();
        fs::write(template_disk_path(s, "t"), [0u8; 1234]).unwrap();
        assert_eq!(template_disk_usage(s, "t"), 1234);
    }

    #[test]
    fn list_templates_sorts_by_yaml_name_and_skips_junk() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = Manager::new(dir.path());
        let s = mgr.storage();
        // No templates directory at all: an empty list, not an error.
        assert!(mgr.list_templates().unwrap().is_empty());

        // The name inside the YAML decides the order, not the directory name.
        for (dir_name, yaml_name) in [("zeta", "alpha"), ("alpha", "zeta"), ("mid", "mid")] {
            fs::create_dir_all(template_dir(s, dir_name)).unwrap();
            fs::write(
                template_file_path(s, dir_name),
                format!("name: {yaml_name}\n"),
            )
            .unwrap();
        }
        // Junk that is skipped silently: a directory without template.yaml,
        // one with malformed YAML, and a plain file.
        fs::create_dir_all(template_dir(s, "empty")).unwrap();
        fs::create_dir_all(template_dir(s, "broken")).unwrap();
        fs::write(template_file_path(s, "broken"), "name: [\n").unwrap();
        fs::write(templates_dir(s).join("stray.yaml"), "name: stray\n").unwrap();

        let tpls = mgr.list_templates().unwrap();
        let names: Vec<&str> = tpls.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["alpha", "mid", "zeta"]);
    }

    #[test]
    fn template_exists_checks_the_yaml_file() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = Manager::new(dir.path());
        let s = mgr.storage();
        assert!(!mgr.template_exists("t"));
        fs::create_dir_all(template_dir(s, "t")).unwrap();
        assert!(
            !mgr.template_exists("t"),
            "a directory without template.yaml is no template"
        );
        fs::write(template_file_path(s, "t"), "name: t\n").unwrap();
        assert!(mgr.template_exists("t"));
    }

    #[test]
    fn delete_template_removes_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = Manager::new(dir.path());
        let s = mgr.storage();
        assert_eq!(
            mgr.delete_template("nope").unwrap_err().to_string(),
            "no template named \"nope\""
        );
        fs::create_dir_all(template_tpm_dir(s, "t")).unwrap();
        fs::write(template_file_path(s, "t"), "name: t\n").unwrap();
        fs::write(template_tpm_dir(s, "t").join("tpm2-00.permall"), "tpm").unwrap();
        mgr.delete_template("t").unwrap();
        assert!(!template_dir(s, "t").exists());
        assert!(templates_dir(s).exists(), "only the one template goes");
    }

    #[test]
    fn copy_disk_converts_and_reports_a_missing_source() {
        if !have_qemu_img() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("disk.qcow2");
        let dst = dir.path().join("copy.qcow2");
        make_disk(&src);
        copy_disk(&src, &dst).unwrap();
        assert_eq!(virtual_size(&dst), virtual_size(&src));
        assert!(fs::metadata(&dst).unwrap().len() > 0);

        let missing = dir.path().join("missing.qcow2");
        let err = copy_disk(&missing, &dst).unwrap_err().to_string();
        assert!(
            err.starts_with(&format!("disk image {}", missing.display())),
            "{err}"
        );

        // qemu-img's own complaint comes along, after the exit status.
        let junk = dir.path().join("junk.qcow2");
        fs::write(&junk, "not an image").unwrap();
        let err = copy_disk(&junk, &dst).unwrap_err().to_string();
        assert!(err.starts_with("copy disk image: exit status 1\n"), "{err}");
        assert!(err.lines().count() >= 2, "qemu-img output missing: {err}");
    }

    #[test]
    fn copies_tolerate_a_missing_source() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        let dst = dir.path().join("dst");
        copy_if_exists(&missing, &dst).unwrap();
        copy_dir_if_exists(&missing, &dst).unwrap();
        assert!(!dst.exists());

        let file = dir.path().join("file");
        fs::write(&file, "x").unwrap();
        let err = copy_dir_if_exists(&file, &dst).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotADirectory);
        assert_eq!(
            err.to_string(),
            format!("{} is not a directory", file.display())
        );
    }

    #[test]
    fn copy_dir_if_exists_makes_the_directories_and_skips_non_files() {
        // An empty tree and a socket exercise everything but copy_file.
        let src_dir = tempfile::tempdir().unwrap();
        let dst_dir = tempfile::tempdir().unwrap();
        let (src, dst) = (src_dir.path(), dst_dir.path().join("copy"));
        fs::create_dir_all(src.join("sub/deeper")).unwrap();
        fs::set_permissions(src.join("sub"), fs::Permissions::from_mode(0o500)).unwrap();
        let _sock = std::os::unix::net::UnixListener::bind(src.join("swtpm.sock")).unwrap();
        copy_dir_if_exists(src, &dst).unwrap();
        assert!(dst.join("sub/deeper").is_dir());
        assert!(
            !dst.join("swtpm.sock").exists(),
            "a socket is a runtime leftover"
        );
        // Source permissions plus owner rwx, so the copy can be written into.
        let mode = fs::metadata(dst.join("sub")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        // Restore so the tempdir can be cleaned up.
        fs::set_permissions(src.join("sub"), fs::Permissions::from_mode(0o700)).unwrap();
    }
}
