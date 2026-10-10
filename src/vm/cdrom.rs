//! The CD-ROM drive holding the boot ISO, and swapping the disc in a running VM.

use std::path::Path;

use anyhow::Result;

use super::monitor::{hmp_quote, monitor_must_succeed};
use super::process::qemu_opt_escape;
use super::usbimage::check_image;

/// The QEMU drive ID of the VM's CD-ROM. The drive is always there, empty
/// when no ISO is configured, so a running VM can be given one.
pub(crate) const CDROM_DRIVE_ID: &str = "cdrom";

/// The QEMU arguments for the CD-ROM drive with `path` in it (`""` for an
/// empty drive). On x86 it sits where `-cdrom` would put it, IDE index 2
/// (the q35 machine's third SATA port); the `virt` machine has no IDE, so
/// there it hangs off a virtio-scsi controller, in PCI slot `scsi_addr`
/// (e.g. `0x2`) or, with `None`, the first free one.
pub(crate) fn cdrom_args(machine: &str, path: &str, scsi_addr: Option<&str>) -> Vec<String> {
    let mut opts = format!("id={CDROM_DRIVE_ID},media=cdrom");
    if !path.is_empty() {
        opts.push_str(",format=raw,file=");
        opts.push_str(&qemu_opt_escape(path));
    }
    if machine == "virt" {
        let mut scsi = "virtio-scsi-pci,id=scsi0".to_string();
        if let Some(addr) = scsi_addr {
            scsi.push_str(",addr=");
            scsi.push_str(addr);
        }
        return vec![
            "-drive".to_string(),
            format!("if=none,{opts}"),
            "-device".to_string(),
            scsi,
            "-device".to_string(),
            format!("scsi-cd,bus=scsi0.0,drive={CDROM_DRIVE_ID}"),
        ];
    }
    vec!["-drive".to_string(), format!("if=ide,index=2,{opts}")]
}

/// Puts a different ISO into the running VM's CD-ROM drive, or takes the
/// disc out when `path` is empty. The tray is forced open first
/// (`eject -f cdrom`), so a guest that has locked it (Linux does while the
/// disc is mounted) cannot hold up the swap; it sees the disc change the way
/// it would with a real drive. Then `change cdrom "<path>" raw`.
pub fn cdrom_change(storage: &Path, name: &str, path: &str) -> Result<()> {
    if !path.is_empty() {
        check_image(path)?;
    }
    monitor_must_succeed(storage, name, &format!("eject -f {CDROM_DRIVE_ID}"))?;
    if path.is_empty() {
        return Ok(());
    }
    monitor_must_succeed(
        storage,
        name,
        &format!("change {CDROM_DRIVE_ID} {} raw", hmp_quote(path)),
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::vm::config::{monitor_path, vm_dir, NetworkConfig, NetworkType, VmConfig};
    use crate::vm::manager::Manager;
    use crate::vm::monitor::monitor_command;
    use crate::vm::monitor::testutil::fake_hmp_sessions;
    use crate::vm::process::{build_qemu_args, start, status, stop, VmStatus};
    use crate::vm::usbimage::testutil::{wait_for_monitor, write_image, StopOnDrop};

    #[test]
    fn args_for_an_empty_drive() {
        assert_eq!(
            cdrom_args("q35", "", None).join(" "),
            "-drive if=ide,index=2,id=cdrom,media=cdrom"
        );
        assert_eq!(
            cdrom_args("virt", "", None).join(" "),
            "-drive if=none,id=cdrom,media=cdrom -device virtio-scsi-pci,id=scsi0 -device scsi-cd,bus=scsi0.0,drive=cdrom"
        );
    }

    /// A slot for the SCSI controller pins it on `virt`; q35's drive sits on
    /// the built-in SATA controller and has none to pin.
    #[test]
    fn args_with_a_pinned_controller() {
        assert_eq!(
            cdrom_args("virt", "/isos/a.iso", Some("0x2")).join(" "),
            "-drive if=none,id=cdrom,media=cdrom,format=raw,file=/isos/a.iso -device virtio-scsi-pci,id=scsi0,addr=0x2 -device scsi-cd,bus=scsi0.0,drive=cdrom"
        );
        assert_eq!(
            cdrom_args("q35", "/isos/a.iso", Some("0x2")),
            cdrom_args("q35", "/isos/a.iso", None)
        );
    }

    #[test]
    fn args_with_an_iso() {
        let cases = [
            (
                "q35",
                "/isos/a,b.iso",
                "-drive if=ide,index=2,id=cdrom,media=cdrom,format=raw,file=/isos/a,,b.iso",
            ),
            ("q35", "", "-drive if=ide,index=2,id=cdrom,media=cdrom"),
            (
                "virt",
                "/isos/a.iso",
                "-drive if=none,id=cdrom,media=cdrom,format=raw,file=/isos/a.iso -device virtio-scsi-pci,id=scsi0 -device scsi-cd,bus=scsi0.0,drive=cdrom",
            ),
            (
                "virt",
                "",
                "-drive if=none,id=cdrom,media=cdrom -device virtio-scsi-pci,id=scsi0 -device scsi-cd,bus=scsi0.0,drive=cdrom",
            ),
        ];
        for (machine, path, want) in cases {
            assert_eq!(
                cdrom_args(machine, path, None).join(" "),
                want,
                "cdrom_args({machine:?}, {path:?})"
            );
        }
    }

    fn config(name: &str) -> VmConfig {
        VmConfig {
            name: name.to_string(),
            cpu: 1,
            ram: 128,
            network: NetworkConfig {
                kind: NetworkType::None,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn build_qemu_args_places_the_drive() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = config("t");
        cfg.cdrom_path = "/isos/a.iso".to_string();
        let (_, args) = build_qemu_args(&cfg, dir.path()).unwrap();
        let joined = args.join(" ");
        assert!(
            joined.contains("-drive if=ide,index=2,id=cdrom,media=cdrom,format=raw,file=/isos/a.iso -boot order=dc"),
            "args with ISO: {joined}"
        );
        assert!(
            !joined.contains("-cdrom"),
            "-cdrom has no drive ID to swap by: {joined}"
        );

        // Without an ISO the drive is still there, empty, and nothing steers
        // the boot order towards it.
        cfg.cdrom_path.clear();
        let (_, args) = build_qemu_args(&cfg, dir.path()).unwrap();
        let joined = args.join(" ");
        assert!(
            joined.contains("-drive if=ide,index=2,id=cdrom,media=cdrom ")
                && !joined.contains("-boot"),
            "args without ISO: {joined}"
        );

        cfg.arch = "aarch64".to_string();
        let (_, args) = build_qemu_args(&cfg, dir.path()).unwrap();
        let joined = args.join(" ");
        assert!(
            joined.contains("scsi-cd,bus=scsi0.0,drive=cdrom") && !joined.contains("if=ide"),
            "virt machine must not get an IDE drive: {joined}"
        );
    }

    /// `start` fails up front, with the path, rather than launching a QEMU
    /// that dies on its discarded stderr.
    #[test]
    fn start_refuses_a_missing_boot_iso() {
        let storage = tempfile::tempdir().unwrap();
        let mut cfg = config("t");
        cfg.cdrom_path = storage
            .path()
            .join("gone.iso")
            .to_string_lossy()
            .into_owned();
        fs::create_dir_all(vm_dir(storage.path(), &cfg.name)).unwrap();
        let err = format!("{:#}", start(storage.path(), &cfg).unwrap_err());
        assert_eq!(
            err,
            format!(
                "boot ISO: image not found: {}\nEject it in the ISO dialog (i), clear it in the edit form (e), or put the file back.",
                cfg.cdrom_path
            )
        );
        let info = status(storage.path(), &cfg.name).unwrap();
        assert_eq!(
            info.status,
            VmStatus::Stopped,
            "a VM must not be started with a missing boot ISO: {info:?}"
        );
    }

    #[test]
    fn change_drives_the_monitor() {
        let storage = tempfile::tempdir().unwrap();
        let isos = tempfile::tempdir().unwrap();
        fs::create_dir_all(vm_dir(storage.path(), "t")).unwrap();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let _monitor = fake_hmp_sessions(&monitor_path(storage.path(), "t"), {
            let commands = Arc::clone(&commands);
            move |cmd| {
                commands.lock().unwrap().push(cmd.to_string());
                String::new()
            }
        });
        let sent = || std::mem::take(&mut *commands.lock().unwrap());

        // Space and comma: the path is HMP-quoted, and commas stay single.
        let iso = write_image(isos.path(), "second disc, v2.iso", 16);
        cdrom_change(storage.path(), "t", &iso).unwrap();
        assert_eq!(
            sent(),
            [
                "eject -f cdrom".to_string(),
                format!("change cdrom \"{iso}\" raw")
            ]
        );

        cdrom_change(storage.path(), "t", "").unwrap();
        assert_eq!(sent(), ["eject -f cdrom"]);

        // A missing file is caught before the drive is touched.
        let gone = isos.path().join("gone.iso").to_string_lossy().into_owned();
        let err = cdrom_change(storage.path(), "t", &gone).unwrap_err();
        assert_eq!(err.to_string(), format!("image not found: {gone}"));
        assert!(sent().is_empty(), "failed check must not touch the monitor");
    }

    #[test]
    fn change_reports_what_qemu_says() {
        let storage = tempfile::tempdir().unwrap();
        fs::create_dir_all(vm_dir(storage.path(), "t")).unwrap();
        let _monitor = fake_hmp_sessions(&monitor_path(storage.path(), "t"), |cmd| {
            if cmd.starts_with("eject ") {
                "Error: Device 'cdrom' not found\r\n".to_string()
            } else {
                String::new()
            }
        });
        let err = cdrom_change(storage.path(), "t", "").unwrap_err();
        assert_eq!(err.to_string(), "QEMU: Error: Device 'cdrom' not found");

        let stopped = tempfile::tempdir().unwrap();
        let err = cdrom_change(stopped.path(), "t", "").unwrap_err();
        assert!(
            err.to_string().starts_with("connect to QEMU monitor: "),
            "{err}"
        );
    }

    /// Boots a real VM with an ISO in the drive, swaps it for another, ejects
    /// it and puts one back, checking the drive each time.
    #[test]
    fn swap_with_qemu() {
        for bin in ["qemu-system-x86_64", "qemu-img"] {
            if which::which(bin).is_err() {
                eprintln!("skipping: {bin} not installed");
                return;
            }
        }
        let isos = tempfile::tempdir().unwrap();
        let first = write_image(isos.path(), "first.iso", 1 << 20);
        // Space and comma: quoting and escaping.
        let second = write_image(isos.path(), "second disc, v2.iso", 2 << 20);

        let storage = tempfile::tempdir().unwrap();
        let mut cfg = config("cdtest");
        cfg.disk_size = 1;
        cfg.cdrom_path = first.clone();
        Manager::new(storage.path()).create(&mut cfg).unwrap();
        start(storage.path(), &cfg).unwrap();
        let _stop = StopOnDrop {
            storage: storage.path(),
            name: &cfg.name,
        };
        wait_for_monitor(storage.path(), &cfg.name);

        let drive = || {
            monitor_command(
                storage.path(),
                &cfg.name,
                &format!("info block {CDROM_DRIVE_ID}"),
            )
            .unwrap()
        };
        let out = drive();
        assert!(
            out.contains(&first) && out.contains("read-only"),
            "boot-time ISO should be in the drive, read-only:\n{out}"
        );

        cdrom_change(storage.path(), &cfg.name, &second).expect("swap");
        let out = drive();
        assert!(
            out.contains(&second) && !out.contains(&first) && out.contains("tray closed"),
            "after swap:\n{out}"
        );

        cdrom_change(storage.path(), &cfg.name, "").expect("eject");
        let out = drive();
        assert!(out.contains("[not inserted]"), "after eject:\n{out}");
        cdrom_change(storage.path(), &cfg.name, "").expect("ejecting an empty drive must be fine");

        cdrom_change(storage.path(), &cfg.name, &first).expect("insert into empty drive");
        let out = drive();
        assert!(
            out.contains(&first) && out.contains("tray closed"),
            "after insert:\n{out}"
        );

        // A missing file is caught before the drive is touched.
        let gone = isos.path().join("gone.iso").to_string_lossy().into_owned();
        let err = cdrom_change(storage.path(), &cfg.name, &gone).unwrap_err();
        assert!(
            err.to_string().contains("not found"),
            "missing ISO should fail, got: {err}"
        );
        let out = drive();
        assert!(
            out.contains(&first),
            "failed swap must leave the disc alone:\n{out}"
        );

        stop(storage.path(), &cfg.name).unwrap();
    }
}
