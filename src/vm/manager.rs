//! VM lifecycle operations rooted at the storage directory: list, create,
//! update, delete.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::Utc;

use super::config::{
    config_file_path, disk_path, extra_disk_path, firmware_vars_path, load_config, save_config,
    vm_dir, VmConfig,
};
use super::disk::{create_disk_image, diff_disks, join_names, resize_disk_image, validate_disks};
use super::firmware::ensure_firmware_vars;
use super::process::{status, stop};
use super::template::TEMPLATES_DIR_NAME;
use super::tpm::check_tpm;

/// Handles VM lifecycle operations rooted at `storage`.
#[derive(Debug, Clone)]
pub struct Manager {
    pub storage: PathBuf,
}

impl Manager {
    /// A manager for the given storage directory.
    pub fn new(storage: impl Into<PathBuf>) -> Self {
        Manager {
            storage: storage.into(),
        }
    }

    /// The storage directory.
    pub fn storage(&self) -> &Path {
        &self.storage
    }

    /// All valid VM configs under the storage directory, sorted by name.
    /// Malformed entries and the `.templates` directory are skipped; a
    /// missing storage directory yields an empty list.
    pub fn list(&self) -> Result<Vec<VmConfig>> {
        let entries = match fs::read_dir(&self.storage) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("open {}", self.storage.display())),
        };
        let mut cfgs = Vec::new();
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name == TEMPLATES_DIR_NAME {
                continue;
            }
            // Malformed entries are skipped silently.
            if let Ok(cfg) = load_config(&self.storage, name) {
                cfgs.push(cfg);
            }
        }
        cfgs.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(cfgs)
    }

    /// Sets up the VM directory, creates the qcow2 disk images and writes
    /// `vm.yaml`. Fills in a random MAC, `created_at`, and the defaults for
    /// `arch`, network type and firmware. Checks firmware and TPM packages
    /// first. If anything fails, a directory made here is removed again;
    /// one that was already there is left alone.
    pub fn create(&self, cfg: &mut VmConfig) -> Result<()> {
        validate_disks(&cfg.disks)?;
        let dir = vm_dir(&self.storage, &cfg.name);
        let made_dir = matches!(fs::metadata(&dir), Err(e) if e.kind() == io::ErrorKind::NotFound);
        fs::create_dir_all(&dir)
            .with_context(|| format!("mkdir {}", dir.display()))
            .context("create VM directory")?;
        let result = self.populate(cfg);
        if result.is_err() && made_dir {
            let _ = fs::remove_dir_all(&dir);
        }
        result
    }

    /// The part of [`Manager::create`] that runs once the VM directory is there.
    fn populate(&self, cfg: &mut VmConfig) -> Result<()> {
        if cfg.network.mac.is_empty() {
            cfg.network.mac = random_mac();
        }
        cfg.created_at = Utc::now();
        if cfg.arch.is_empty() {
            cfg.arch = "x86_64".to_string();
        }

        // Firmware and TPM need host packages; find out now rather than at start.
        ensure_firmware_vars(&self.storage, cfg)?;
        if cfg.tpm {
            check_tpm()?;
        }

        create_disk_image(&disk_path(&self.storage, &cfg.name), cfg.disk_size)
            .context("create disk image")?;
        for d in &cfg.disks {
            create_disk_image(&extra_disk_path(&self.storage, &cfg.name, &d.name), d.size)
                .with_context(|| format!("create disk {:?}", d.name))?;
        }

        save_config(&self.storage, cfg).context("save VM config")
    }

    /// Applies an edited config to the existing VM named `old_name`: grows
    /// the disk images, creates and removes additional disks, renames the
    /// VM directory and sets up UEFI NVRAM as needed, then rewrites
    /// `vm.yaml`. Renaming, resizing or removing disks and firmware changes
    /// require the VM to be stopped; a disk can be added to a running VM
    /// (the caller hot-plugs it), and other changes take effect the next
    /// time it is started.
    pub fn update(&self, old_name: &str, cfg: &mut VmConfig) -> Result<()> {
        let mut old = load_config(&self.storage, old_name).context("load VM config")?;
        validate_disks(&cfg.disks)?;
        let disks = diff_disks(&old.disks, &cfg.disks)?;

        let renamed = cfg.name != old_name;
        let resized = cfg.disk_size != old.disk_size;
        let firmware_changed = cfg.uefi() != old.uefi() || cfg.secure_boot != old.secure_boot;
        let disks_locked = !disks.grown.is_empty() || !disks.removed.is_empty();

        if cfg.disk_size < old.disk_size {
            bail!(
                "disk can only grow (currently {} GiB) — shrinking would destroy data",
                old.disk_size
            );
        }
        if renamed && self.exists(&cfg.name) {
            bail!("a VM named {:?} already exists", cfg.name);
        }
        if renamed || resized || firmware_changed || disks_locked {
            if let Ok(info) = status(&self.storage, old_name) {
                if info.running() {
                    if renamed || resized || firmware_changed {
                        bail!("stop the VM before changing its name, disk size or firmware");
                    }
                    bail!(
                        "stop the VM before resizing or removing disks ({})",
                        join_names(disks.grown.iter().chain(&disks.removed))
                    );
                }
            }
        }
        if cfg.network.mac.is_empty() {
            cfg.network.mac = random_mac();
        }
        if cfg.tpm && !old.tpm {
            check_tpm()?;
        }

        // Disk images change first, under the old name. A new image that is
        // left behind by a later failure would block the next attempt, so the
        // ones made here go again on failure; nothing is on them yet.
        if resized {
            resize_disk_image(&disk_path(&self.storage, old_name), cfg.disk_size)
                .context("resize disk image")?;
        }
        let mut created: Vec<PathBuf> = Vec::with_capacity(disks.added.len());
        let undo = |created: &[PathBuf]| {
            for p in created {
                let _ = fs::remove_file(p);
            }
        };
        for d in &disks.added {
            let path = extra_disk_path(&self.storage, old_name, &d.name);
            if let Err(e) = create_disk_image(&path, d.size) {
                undo(&created);
                return Err(e).with_context(|| format!("create disk {:?}", d.name));
            }
            created.push(path);
        }
        for d in &disks.grown {
            if let Err(e) =
                resize_disk_image(&extra_disk_path(&self.storage, old_name, &d.name), d.size)
            {
                undo(&created);
                return Err(e).with_context(|| format!("resize disk {:?}", d.name));
            }
        }
        for d in &disks.removed {
            let path = extra_disk_path(&self.storage, old_name, &d.name);
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => {
                    undo(&created);
                    return Err(e)
                        .with_context(|| format!("remove {}", path.display()))
                        .with_context(|| format!("remove disk {:?}", d.name));
                }
            }
        }
        if resized || disks.any() {
            // Keep vm.yaml truthful about the images, whatever fails from here on.
            old.disk_size = cfg.disk_size;
            old.disks.clone_from(&cfg.disks);
            save_config(&self.storage, &old).context("save VM config")?;
        }

        if renamed {
            let (from, to) = (
                vm_dir(&self.storage, old_name),
                vm_dir(&self.storage, &cfg.name),
            );
            fs::rename(&from, &to)
                .with_context(|| format!("rename {} {}", from.display(), to.display()))
                .context("rename VM directory")?;
        }

        // Turning Secure Boot on needs a store with the keys enrolled, so the
        // NVRAM is rebuilt (boot entries are re-created by the firmware). Turning
        // it off or switching to BIOS keeps the store so the VM can switch back.
        if cfg.secure_boot && !old.secure_boot {
            let vars = firmware_vars_path(&self.storage, &cfg.name);
            match fs::remove_file(&vars) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("remove {}", vars.display()))
                        .context("reset NVRAM")
                }
            }
        }
        ensure_firmware_vars(&self.storage, cfg)?;

        save_config(&self.storage, cfg).context("save VM config")
    }

    /// Stops the VM (best effort) then removes its directory; a VM that is
    /// already gone is not an error.
    pub fn delete(&self, name: &str) -> Result<()> {
        let _ = stop(&self.storage, name);
        let dir = vm_dir(&self.storage, name);
        match fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("remove {}", dir.display())),
        }
    }

    /// Whether a VM directory with a `vm.yaml` exists.
    pub fn exists(&self, name: &str) -> bool {
        fs::metadata(config_file_path(&self.storage, name)).is_ok()
    }
}

/// A random locally administered MAC in QEMU's `52:54:00:xx:xx:xx` range.
pub(crate) fn random_mac() -> String {
    format!(
        "52:54:00:{:02x}:{:02x}:{:02x}",
        fastrand::u8(..),
        fastrand::u8(..),
        fastrand::u8(..)
    )
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;
    use crate::vm::config::{pid_path, NetworkConfig, NetworkType};
    use crate::vm::disk::{Disk, QEMU_IMG_BIN};

    fn disk(name: &str, size: u32) -> Disk {
        Disk {
            name: name.into(),
            size,
        }
    }

    fn text(err: &anyhow::Error) -> String {
        format!("{err:#}")
    }

    /// A BIOS VM with no network, the shape every test here starts from.
    fn bios_vm(name: &str, disk_size: u32, disks: Vec<Disk>) -> VmConfig {
        VmConfig {
            name: name.into(),
            cpu: 1,
            ram: 512,
            disk_size,
            network: NetworkConfig {
                kind: NetworkType::None,
                ..Default::default()
            },
            disks,
            ..Default::default()
        }
    }

    /// Writes a `vm.yaml` for `cfg` without going through `create`.
    fn write_vm(storage: &Path, cfg: &VmConfig) {
        fs::create_dir_all(vm_dir(storage, &cfg.name)).unwrap();
        save_config(storage, cfg).unwrap();
    }

    /// The virtual size qemu-img reports for an image, in bytes.
    fn virtual_size(path: &Path) -> u64 {
        let out = Command::new(QEMU_IMG_BIN)
            .args(["info", "--output=json"])
            .arg(path)
            .output()
            .unwrap_or_else(|e| panic!("qemu-img info {}: {e}", path.display()));
        let info: serde_json::Value = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("no JSON from qemu-img info: {e}"));
        info["virtual-size"]
            .as_u64()
            .filter(|&n| n > 0)
            .expect("no virtual-size")
    }

    const GIB: u64 = 1 << 30;

    #[test]
    fn list_sorts_by_yaml_name_and_skips_junk() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = tmp.path();
        let mgr = Manager::new(storage.join("missing"));
        assert!(
            mgr.list().unwrap().is_empty(),
            "a missing storage directory is empty"
        );

        let mgr = Manager::new(storage);
        // Sorted by the name inside vm.yaml, not the directory name.
        for (dir, name) in [("zeta", "b-vm"), ("alpha", "c-vm"), ("mid", "a-vm")] {
            let cfg = VmConfig {
                name: name.into(),
                ..bios_vm(dir, 1, vec![])
            };
            fs::create_dir_all(vm_dir(storage, dir)).unwrap();
            fs::write(
                config_file_path(storage, dir),
                serde_yaml_ng::to_string(&cfg).unwrap(),
            )
            .unwrap();
        }
        fs::create_dir_all(vm_dir(storage, "no-yaml")).unwrap();
        fs::create_dir_all(vm_dir(storage, "bad-yaml")).unwrap();
        fs::write(config_file_path(storage, "bad-yaml"), "name: [\n").unwrap();
        fs::create_dir_all(vm_dir(storage, TEMPLATES_DIR_NAME)).unwrap();
        fs::write(config_file_path(storage, TEMPLATES_DIR_NAME), "name: tpl\n").unwrap();
        fs::write(storage.join("stray.txt"), "x").unwrap();

        let names: Vec<String> = mgr.list().unwrap().into_iter().map(|c| c.name).collect();
        assert_eq!(names, ["a-vm", "b-vm", "c-vm"]);
    }

    #[test]
    fn exists_needs_a_vm_yaml() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = Manager::new(tmp.path());
        assert!(!mgr.exists("a"));
        fs::create_dir_all(vm_dir(tmp.path(), "a")).unwrap();
        assert!(!mgr.exists("a"), "a directory without vm.yaml is not a VM");
        write_vm(tmp.path(), &bios_vm("a", 1, vec![]));
        assert!(mgr.exists("a"));
        assert_eq!(mgr.storage(), tmp.path());
    }

    #[test]
    fn random_mac_is_in_the_qemu_range() {
        for _ in 0..32 {
            let mac = random_mac();
            assert_eq!(mac.len(), 17, "{mac}");
            assert!(mac.starts_with("52:54:00:"), "{mac}");
            assert!(crate::vm::config::validate_mac(&mac).is_ok(), "{mac}");
            assert_eq!(mac, mac.to_lowercase());
        }
    }

    /// Duplicate names are caught before anything is made.
    #[test]
    fn create_rejects_invalid_disks_before_making_anything() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = Manager::new(tmp.path());
        let mut dup = bios_vm("dup", 1, vec![disk("data", 1), disk("Data", 1)]);
        let err = mgr.create(&mut dup).unwrap_err();
        assert!(text(&err).contains("duplicate"), "duplicate disks: {err}");
        assert!(
            !vm_dir(tmp.path(), "dup").exists(),
            "directory made for an invalid config"
        );
        assert!(
            dup.network.mac.is_empty(),
            "defaults filled in for a rejected config"
        );
    }

    /// The checks that need nothing but the saved config come before any
    /// image or process is looked at.
    #[test]
    fn update_refuses_bad_edits_before_touching_anything() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = tmp.path();
        let mgr = Manager::new(storage);
        let saved = bios_vm("deb", 4, vec![disk("data", 2)]);
        write_vm(storage, &saved);
        write_vm(storage, &bios_vm("other", 1, vec![]));

        let err = mgr.update("nope", &mut saved.clone()).unwrap_err();
        assert!(text(&err).starts_with("load VM config: read "), "{err}");

        let mut shrunk = VmConfig {
            disk_size: 3,
            ..saved.clone()
        };
        let err = mgr.update("deb", &mut shrunk).unwrap_err();
        assert_eq!(
            text(&err),
            "disk can only grow (currently 4 GiB) — shrinking would destroy data"
        );

        let mut shrunk_extra = VmConfig {
            disks: vec![disk("data", 1)],
            ..saved.clone()
        };
        let err = mgr.update("deb", &mut shrunk_extra).unwrap_err();
        assert_eq!(
            text(&err),
            "disk \"data\" can only grow (currently 2 GiB) — shrinking would destroy data"
        );

        let mut invalid = VmConfig {
            disks: vec![disk("disk", 1)],
            ..saved.clone()
        };
        let err = mgr.update("deb", &mut invalid).unwrap_err();
        assert_eq!(text(&err), "disk name \"disk\" is taken by the main disk");

        let mut taken = VmConfig {
            name: "other".into(),
            ..saved.clone()
        };
        let err = mgr.update("deb", &mut taken).unwrap_err();
        assert_eq!(text(&err), "a VM named \"other\" already exists");

        assert_eq!(
            load_config(storage, "deb").unwrap(),
            saved,
            "a refused update changed vm.yaml"
        );
        assert!(!disk_path(storage, "deb").exists());
    }

    #[test]
    fn create_and_update_extra_disks() {
        if which::which(QEMU_IMG_BIN).is_err() {
            eprintln!("skipping: {QEMU_IMG_BIN} not installed");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let storage = tmp.path();
        let mgr = Manager::new(storage);

        let mut cfg = bios_vm("deb", 1, vec![disk("data", 1)]);
        mgr.create(&mut cfg).unwrap();
        assert!(
            !cfg.network.mac.is_empty() && cfg.arch == "x86_64",
            "defaults not filled in: {cfg:?}"
        );
        let data = extra_disk_path(storage, "deb", "data");
        assert_eq!(virtual_size(&data), GIB);
        assert_eq!(virtual_size(&disk_path(storage, "deb")), GIB);
        let loaded = load_config(storage, "deb").unwrap();
        assert_eq!(loaded.disks, cfg.disks);
        assert_eq!(loaded, cfg, "the saved config is the one handed back");

        // Grow one, add one.
        let mut cur = VmConfig {
            disks: vec![disk("data", 2), disk("scratch", 1)],
            ..loaded
        };
        mgr.update("deb", &mut cur).unwrap();
        let scratch = extra_disk_path(storage, "deb", "scratch");
        assert_eq!(virtual_size(&data), 2 * GIB, "data not grown");
        assert!(scratch.exists(), "scratch not created");
        assert_eq!(load_config(storage, "deb").unwrap().disks, cur.disks);

        // Shrinking is refused before anything is touched.
        let mut shrunk = VmConfig {
            disks: vec![disk("data", 1), disk("scratch", 1)],
            ..cur.clone()
        };
        let err = mgr.update("deb", &mut shrunk).unwrap_err();
        assert!(text(&err).contains("can only grow"), "shrink: {err}");

        // A new disk never replaces a file that is already there.
        let stray = extra_disk_path(storage, "deb", "stray");
        fs::write(&stray, "precious").unwrap();
        let mut over = cur.clone();
        over.disks.push(disk("stray", 1));
        let err = mgr.update("deb", &mut over).unwrap_err();
        assert!(
            text(&err).contains("already exists"),
            "create over existing file: {err}"
        );
        assert_eq!(
            fs::read_to_string(&stray).unwrap(),
            "precious",
            "stray file overwritten"
        );
        assert_eq!(
            load_config(storage, "deb").unwrap().disks,
            cur.disks,
            "failed update changed the saved disks"
        );
        fs::remove_file(&stray).unwrap();

        // While running: no growing or removing, but adding is fine.
        let qemu = crate::vm::process::testutil::FakeQemu::spawn();
        qemu.run_as(storage, "deb");
        let mut grow = VmConfig {
            disks: vec![disk("data", 3), disk("scratch", 1)],
            ..cur.clone()
        };
        let err = mgr.update("deb", &mut grow).unwrap_err();
        assert_eq!(
            text(&err),
            "stop the VM before resizing or removing disks (data)"
        );
        let mut remove = VmConfig {
            disks: vec![disk("data", 2)],
            ..cur.clone()
        };
        let err = mgr.update("deb", &mut remove).unwrap_err();
        assert_eq!(
            text(&err),
            "stop the VM before resizing or removing disks (scratch)"
        );
        assert!(scratch.exists(), "scratch removed despite the refusal");
        let mut add = VmConfig {
            disks: vec![disk("data", 2), disk("scratch", 1), disk("logs", 1)],
            ..cur.clone()
        };
        mgr.update("deb", &mut add).unwrap();
        let logs = extra_disk_path(storage, "deb", "logs");
        assert!(logs.exists(), "logs not created");
        let mut renamed_running = VmConfig {
            name: "deb2".into(),
            ..add.clone()
        };
        let err = mgr.update("deb", &mut renamed_running).unwrap_err();
        assert_eq!(
            text(&err),
            "stop the VM before changing its name, disk size or firmware"
        );
        fs::remove_file(pid_path(storage, "deb")).unwrap();

        // Removing deletes the images; the others stay.
        let mut remove = VmConfig {
            disks: vec![disk("data", 2)],
            ..add
        };
        mgr.update("deb", &mut remove).unwrap();
        for p in [&scratch, &logs] {
            assert!(!p.exists(), "{} still there after removal", p.display());
        }
        assert!(data.exists(), "data gone");

        // Renaming the VM takes the images along.
        let mut renamed = VmConfig {
            name: "deb2".into(),
            ..remove
        };
        mgr.update("deb", &mut renamed).unwrap();
        assert!(
            extra_disk_path(storage, "deb2", "data").exists(),
            "data not moved with the VM"
        );
        assert!(!vm_dir(storage, "deb").exists());
        let loaded = load_config(storage, "deb2").unwrap();
        assert_eq!(loaded.disks, vec![disk("data", 2)]);
        assert_eq!(loaded.name, "deb2");

        // Growing the main disk resizes its image and saves the new size.
        let mut bigger = VmConfig {
            disk_size: 2,
            ..loaded
        };
        mgr.update("deb2", &mut bigger).unwrap();
        assert_eq!(virtual_size(&disk_path(storage, "deb2")), 2 * GIB);
        assert_eq!(load_config(storage, "deb2").unwrap().disk_size, 2);
    }

    #[test]
    fn create_cleans_up_only_its_own_dir() {
        if which::which(QEMU_IMG_BIN).is_err() {
            eprintln!("skipping: {QEMU_IMG_BIN} not installed");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let storage = tmp.path();
        let mgr = Manager::new(storage);
        // A size qemu-img rejects (too large for qcow2), after the directory
        // has been made.
        let mut bad = bios_vm("bad", u32::MAX, vec![]);
        let err = mgr.create(&mut bad).unwrap_err();
        assert!(
            text(&err).starts_with("create disk image: qemu-img create: exit status 1\n"),
            "{err}"
        );
        assert!(
            !vm_dir(storage, "bad").exists(),
            "failed create left its directory behind"
        );

        // A directory that was already there, holding a file, is left alone.
        let kept = vm_dir(storage, "kept");
        fs::create_dir_all(&kept).unwrap();
        let note = kept.join("notes.txt");
        fs::write(&note, "mine").unwrap();
        bad.name = "kept".into();
        mgr.create(&mut bad).unwrap_err();
        assert_eq!(
            fs::read_to_string(&note).unwrap(),
            "mine",
            "pre-existing directory was removed"
        );

        // Creating over an existing disk image is refused, and the image kept.
        let mut twice = bios_vm("twice", 1, vec![]);
        mgr.create(&mut twice).unwrap();
        let before = fs::read(disk_path(storage, "twice")).unwrap();
        let err = mgr.create(&mut bios_vm("twice", 1, vec![])).unwrap_err();
        assert_eq!(
            text(&err),
            format!(
                "create disk image: {} already exists",
                disk_path(storage, "twice").display()
            )
        );
        assert_eq!(fs::read(disk_path(storage, "twice")).unwrap(), before);
        assert!(mgr.exists("twice"));

        // Duplicate names are caught before anything is made.
        let mut dup = bios_vm("dup", 1, vec![disk("data", 1), disk("Data", 1)]);
        let err = mgr.create(&mut dup).unwrap_err();
        assert!(text(&err).contains("duplicate"), "duplicate disks: {err}");
        assert!(
            !vm_dir(storage, "dup").exists(),
            "directory made for an invalid config"
        );
    }

    #[test]
    fn delete_removes_the_directory_and_tolerates_a_missing_vm() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = tmp.path();
        let mgr = Manager::new(storage);
        write_vm(storage, &bios_vm("gone", 1, vec![]));
        fs::write(disk_path(storage, "gone"), "x").unwrap();
        mgr.delete("gone").unwrap();
        assert!(!vm_dir(storage, "gone").exists());
        mgr.delete("gone").unwrap();
        mgr.delete("never-was").unwrap();
    }
}
