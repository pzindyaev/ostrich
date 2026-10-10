//! Additional virtio disks next to the main one: config, images, QEMU
//! arguments and hot-plug.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::Path;
use std::process::Command;

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};

use super::config::{extra_disk_path, null_as_default, VmConfig};
use super::firmware::{combined_output, exit_status_text};
use super::monitor::{hmp_quote, monitor_command, monitor_must_succeed};
use super::process::qemu_opt_escape;
use super::usbimage::{check_image, image_state_of, ImageState};

/// An additional virtio disk of a VM (`vm.yaml` schema). Its image is
/// `<vm-dir>/<name>.qcow2`; the name also serves as the virtio serial, so
/// the guest finds the disk as `/dev/disk/by-id/virtio-<name>`.
///
/// A key that is missing from `vm.yaml` or null reads as `""` or 0, as
/// with Go's YAML decoder, so the VM still loads and [`Disk::validate`]
/// names what is wrong when it is started or edited.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Disk {
    #[serde(default, deserialize_with = "null_as_default")]
    pub name: String,
    /// GiB.
    #[serde(default, deserialize_with = "null_as_default")]
    pub size: u32,
}

/// How many additional disks a VM can have: one PCIe root port each, always
/// present so a disk can be hot-plugged into a running VM.
pub const MAX_EXTRA_DISKS: usize = 8;
/// The file stem of the main disk (`disk.qcow2`).
pub(crate) const PRIMARY_DISK_NAME: &str = "disk";
/// The pcie.0 slot of the first root port; the ports are pinned high so the
/// devices QEMU slots by itself (xHCI, NIC, the main disk) keep the addresses
/// they had before the ports existed: OVMF boot entries name the disk by its
/// PCI address.
pub(crate) const DISK_PORT_ADDR_BASE: u32 = 0x10;
/// The virtio-blk serial limit; QEMU truncates silently.
pub(crate) const DISK_NAME_MAX_LEN: usize = 20;
/// Creates, resizes and copies disk images.
pub(crate) const QEMU_IMG_BIN: &str = "qemu-img";

/// The name rule, `^[A-Za-z][A-Za-z0-9_-]{0,19}$`: ASCII only, so the byte
/// length is the character count.
fn valid_disk_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=DISK_NAME_MAX_LEN).contains(&bytes.len())
        && bytes[0].is_ascii_alphabetic()
        && bytes[1..]
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

impl Disk {
    /// Checks the name (`^[A-Za-z][A-Za-z0-9_-]{0,19}$`, not `disk` in any
    /// case) and the size (≥ 1). Errors, verbatim:
    /// `disk name "<n>": letters, digits, hyphens and underscores, starting with a letter, at most 20 characters`,
    /// `disk name "<n>" is taken by the main disk`, `disk "<n>": size must be at least 1 GiB`.
    pub fn validate(&self) -> Result<()> {
        if !valid_disk_name(&self.name) {
            bail!(
                "disk name {:?}: letters, digits, hyphens and underscores, starting with a letter, at most {DISK_NAME_MAX_LEN} characters",
                self.name
            );
        }
        if self.name.eq_ignore_ascii_case(PRIMARY_DISK_NAME) {
            bail!("disk name {:?} is taken by the main disk", self.name);
        }
        if self.size < 1 {
            bail!("disk {:?}: size must be at least 1 GiB", self.name);
        }
        Ok(())
    }
}

/// Checks every disk and that the names are unique ignoring case, so two
/// images cannot clash on a case-insensitive filesystem. Errors:
/// `at most 8 additional disks`, `duplicate disk name "<n>"`.
pub fn validate_disks(disks: &[Disk]) -> Result<()> {
    if disks.len() > MAX_EXTRA_DISKS {
        bail!("at most {MAX_EXTRA_DISKS} additional disks");
    }
    let mut seen = HashSet::with_capacity(disks.len());
    for d in disks {
        d.validate()?;
        if !seen.insert(d.name.to_ascii_lowercase()) {
            bail!("duplicate disk name {:?}", d.name);
        }
    }
    Ok(())
}

/// Parses a comma-separated list of `[name:]size` entries, sizes in GiB,
/// e.g. `data:50, 100`. An entry without a name gets the lowest free
/// `disk<n>`, n from 1, so `100` above becomes disk1.
pub fn parse_disks(s: &str) -> Result<Vec<Disk>> {
    let mut disks = Vec::new();
    let mut unnamed = Vec::new();
    for entry in s.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let (name, size_str, named) = match entry.split_once(':') {
            Some((name, size)) => (name.trim(), size.trim(), true),
            None => ("", entry, false),
        };
        let syntax_error =
            || anyhow!("invalid disk {entry:?} — expected [name:]size in GiB, e.g. data:50");
        // A signed parse, so that "-5" is reported as too small, not as
        // malformed.
        let size: i64 = size_str.parse().map_err(|_| syntax_error())?;
        if named && name.is_empty() {
            return Err(syntax_error());
        }
        if size < 1 {
            bail!("invalid disk {entry:?} — size must be at least 1 GiB");
        }
        let size = u32::try_from(size).map_err(|_| syntax_error())?;
        if !named {
            unnamed.push(disks.len());
        }
        disks.push(Disk {
            name: name.to_string(),
            size,
        });
    }

    let mut taken: HashSet<String> = disks.iter().map(|d| d.name.to_lowercase()).collect();
    taken.insert(PRIMARY_DISK_NAME.to_string());
    let mut n = 1;
    for &i in &unnamed {
        let name = loop {
            let candidate = format!("disk{n}");
            if !taken.contains(&candidate) {
                break candidate;
            }
            n += 1;
        };
        taken.insert(name.clone());
        disks[i].name = name;
    }
    validate_disks(&disks)?;
    Ok(disks)
}

/// The inverse of [`parse_disks`]: `data:50, disk1:100`.
pub fn format_disks(disks: &[Disk]) -> String {
    disks
        .iter()
        .map(|d| format!("{}:{}", d.name, d.size))
        .collect::<Vec<_>>()
        .join(", ")
}

/// What an edit does to a VM's additional disks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiskChange {
    pub added: Vec<Disk>,
    /// With the new size.
    pub grown: Vec<Disk>,
    pub removed: Vec<Disk>,
}

impl DiskChange {
    /// Whether the change touches any disk.
    pub fn any(&self) -> bool {
        !(self.added.is_empty() && self.grown.is_empty() && self.removed.is_empty())
    }
}

/// Compares the configured disks before and after an edit, matched by name.
/// A disk that got smaller is an error, since the image cannot shrink
/// without destroying data:
/// `disk "<n>" can only grow (currently <s> GiB) — shrinking would destroy data`.
pub fn diff_disks(old: &[Disk], cur: &[Disk]) -> Result<DiskChange> {
    let old_by_name: HashMap<&str, &Disk> = old.iter().map(|d| (d.name.as_str(), d)).collect();
    let mut ch = DiskChange::default();
    let mut kept = HashSet::with_capacity(cur.len());
    for d in cur {
        match old_by_name.get(d.name.as_str()) {
            None => ch.added.push(d.clone()),
            Some(o) if d.size < o.size => bail!(
                "disk {:?} can only grow (currently {} GiB) — shrinking would destroy data",
                d.name,
                o.size
            ),
            Some(o) if d.size > o.size => ch.grown.push(d.clone()),
            Some(_) => {}
        }
        kept.insert(d.name.as_str());
    }
    ch.removed.extend(
        old.iter()
            .filter(|d| !kept.contains(d.name.as_str()))
            .cloned(),
    );
    Ok(ch)
}

/// The disks' names, comma-separated (`a, b`).
pub fn disk_names(disks: &[Disk]) -> String {
    join_names(disks.iter())
}

/// [`disk_names`] over any sequence of disks.
pub(crate) fn join_names<'a>(disks: impl Iterator<Item = &'a Disk>) -> String {
    disks
        .map(|d| d.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

// --- images ---

/// Runs a `qemu-img <verb> ...` command, its stdout and stderr captured
/// together and read to the end, as Go's `CombinedOutput` did; failure is
/// `qemu-img <verb>: <status>\n<output>`. The pipe is close-on-exec, so a
/// child that another thread spawns meanwhile does not keep it open and
/// hold up the read.
fn run_qemu_img(verb: &str, cmd: Command) -> Result<()> {
    match combined_output(cmd, None) {
        Ok((status, _)) if status.success() => Ok(()),
        Ok((status, out)) => bail!(
            "qemu-img {verb}: {}\n{}",
            exit_status_text(status),
            String::from_utf8_lossy(&out)
        ),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            bail!("qemu-img {verb}: exec: {QEMU_IMG_BIN:?}: executable file not found in $PATH\n")
        }
        Err(e) => bail!("qemu-img {verb}: {e}\n"),
    }
}

/// Makes a new qcow2 image of the given virtual size with
/// `qemu-img create -f qcow2 <path> <n>G`. Refuses to touch an existing
/// file (`<path> already exists`): qemu-img create would silently replace
/// it, and a leftover image may hold data. A failing command is
/// `qemu-img create: <status>\n<output>`.
pub(crate) fn create_disk_image(path: &Path, size_gib: u32) -> Result<()> {
    if fs::metadata(path).is_ok() {
        bail!("{} already exists", path.display());
    }
    let mut cmd = Command::new(QEMU_IMG_BIN);
    cmd.args(["create", "-f", "qcow2"])
        .arg(path)
        .arg(format!("{size_gib}G"));
    run_qemu_img("create", cmd)
}

/// Grows an image with `qemu-img resize <path> <n>G`; failure is
/// `qemu-img resize: <status>\n<output>`. The guest still has to extend its
/// own partitions and filesystem.
pub(crate) fn resize_disk_image(path: &Path, size_gib: u32) -> Result<()> {
    let mut cmd = Command::new(QEMU_IMG_BIN);
    cmd.arg("resize").arg(path).arg(format!("{size_gib}G"));
    run_qemu_img("resize", cmd)
}

/// Validates the configured disks and names each one whose image is missing,
/// which would stop QEMU from starting (`disk <n>: <error>` lines, then
/// `Remove it in the edit form, or put the file back.`).
pub fn check_extra_disks(storage: &Path, cfg: &VmConfig) -> Result<()> {
    validate_disks(&cfg.disks)?;
    let mut msg = String::new();
    for d in &cfg.disks {
        let path = extra_disk_path(storage, &cfg.name, &d.name);
        if let Err(e) = check_image(&path.to_string_lossy()) {
            let _ = writeln!(msg, "disk {}: {e:#}", d.name);
        }
    }
    if msg.is_empty() {
        return Ok(());
    }
    bail!("{msg}Remove it in the edit form, or put the file back.")
}

/// Checks each additional disk's image on the host.
pub fn disk_states(storage: &Path, cfg: &VmConfig) -> Vec<ImageState> {
    cfg.disks
        .iter()
        .map(|d| image_state_of(&extra_disk_path(storage, &cfg.name, &d.name).to_string_lossy()))
        .collect()
}

// --- QEMU ---

/// `disk-rp<i+1>`: the PCIe root port that holds the disk at index `i`.
pub(crate) fn disk_port_id(i: usize) -> String {
    format!("disk-rp{}", i + 1)
}

/// `disk-<name>`: the virtio-blk device of a disk.
pub(crate) fn disk_device_id(name: &str) -> String {
    format!("disk-{name}")
}

/// `disk-<name>-drive`: the block backend behind a disk's device. There is
/// no hot-unplug for disks, so unlike USB images the ID needs no unique
/// suffix.
pub(crate) fn disk_drive_id(name: &str) -> String {
    format!("{}-drive", disk_device_id(name))
}

/// The root ports every VM gets, [`MAX_EXTRA_DISKS`] of them, pinned to the
/// same pcie.0 slots whether or not disks are configured:
/// `-device pcie-root-port,id=disk-rp<n>,bus=pcie.0,chassis=<n>,addr=0x<10+i>`.
/// The root bus itself does not take hot-plugged devices; a port does, and
/// holds one. Chassis numbers must differ between ports.
pub(crate) fn disk_port_args() -> Vec<String> {
    let mut args = Vec::with_capacity(2 * MAX_EXTRA_DISKS);
    for i in 0..MAX_EXTRA_DISKS {
        args.push("-device".to_string());
        args.push(format!(
            "pcie-root-port,id={},bus=pcie.0,chassis={},addr=0x{:x}",
            disk_port_id(i),
            i + 1,
            DISK_PORT_ADDR_BASE as usize + i
        ));
    }
    args
}

/// `if=none,id=<drive>,format=qcow2,file=<path>` (commas doubled), used
/// verbatim on the command line (`-drive`) and for hot-plug (`drive_add`).
pub(crate) fn disk_drive(path: &str, drive_id: &str) -> String {
    format!(
        "if=none,id={drive_id},format=qcow2,file={}",
        qemu_opt_escape(path)
    )
}

/// `virtio-blk-pci,id=disk-<name>,drive=disk-<name>-drive,bus=disk-rp<idx+1>,serial=<name>`,
/// used both on the command line (`-device`) and for hot-plug (`device_add`).
/// The serial lets the guest tell the disks apart by name.
pub(crate) fn disk_device(name: &str, idx: usize) -> String {
    format!(
        "virtio-blk-pci,id={},drive={},bus={},serial={name}",
        disk_device_id(name),
        disk_drive_id(name),
        disk_port_id(idx)
    )
}

/// The root ports followed by a `-drive`/`-device` pair per configured disk.
/// Disk i sits on port i, so a disk appended to the config while the VM runs
/// can be hot-plugged onto the port its index names.
pub(crate) fn extra_disk_args(cfg: &VmConfig, storage: &Path) -> Vec<String> {
    let mut args = disk_port_args();
    for (i, d) in cfg.disks.iter().enumerate() {
        let path = extra_disk_path(storage, &cfg.name, &d.name);
        args.push("-drive".to_string());
        args.push(disk_drive(&path.to_string_lossy(), &disk_drive_id(&d.name)));
        args.push("-device".to_string());
        args.push(disk_device(&d.name, i));
    }
    args
}

// --- Hot-plug ---

/// Attaches `cfg.disks[idx]` to the running VM through the monitor: the
/// drive first (`drive_add 0 "<opts>"`), then the virtio-blk device on root
/// port `idx`; on failure the orphan drive is deleted again. Disks are only
/// ever appended while a VM runs (removing one needs it stopped), so the
/// port is free unless the VM was started by an Ostrich without root ports,
/// which QEMU reports as the bus not being found. Errors:
/// `no disk at index <i>`, `at most 8 additional disks`, `QEMU: <output>`.
pub fn disk_hotplug(storage: &Path, cfg: &VmConfig, idx: usize) -> Result<()> {
    let Some(d) = cfg.disks.get(idx) else {
        bail!("no disk at index {idx}");
    };
    if idx >= MAX_EXTRA_DISKS {
        bail!("at most {MAX_EXTRA_DISKS} additional disks");
    }
    d.validate()?;
    let path = extra_disk_path(storage, &cfg.name, &d.name);
    let path = path.to_string_lossy();
    check_image(&path)?;
    let drive_id = disk_drive_id(&d.name);
    // drive_add answers "OK" on success; its first argument is a PCI address
    // that is ignored for if=none drives, and the options are one quoted
    // HMP argument in case the path has spaces.
    let resp = monitor_command(
        storage,
        &cfg.name,
        &format!("drive_add 0 {}", hmp_quote(&disk_drive(&path, &drive_id))),
    )?;
    if resp != "OK" {
        bail!("QEMU: {resp}");
    }
    if let Err(e) = monitor_must_succeed(
        storage,
        &cfg.name,
        &format!("device_add {}", disk_device(&d.name, idx)),
    ) {
        // Leave no orphan drive behind holding the file open.
        let _ = monitor_command(storage, &cfg.name, &format!("drive_del {drive_id}"));
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::process::Stdio;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::vm::config::{
        config_file_path, load_config, monitor_path, vm_dir, NetworkConfig, NetworkType,
    };
    use crate::vm::manager::Manager;
    use crate::vm::process::{build_qemu_args, start, status, stop, VmStatus};

    fn disk(name: &str, size: u32) -> Disk {
        Disk {
            name: name.into(),
            size,
        }
    }

    fn text(err: &anyhow::Error) -> String {
        format!("{err:#}")
    }

    #[test]
    fn parse_and_format_disks() {
        let cases: [(&str, Vec<Disk>, &str); 5] = [
            ("", vec![], ""),
            (
                "data:50, 100",
                vec![disk("data", 50), disk("disk1", 100)],
                "data:50, disk1:100",
            ),
            (" scratch : 10 ,", vec![disk("scratch", 10)], "scratch:10"),
            // Unnamed entries take the lowest free number, around the named ones.
            (
                "disk1:1, 5, disk3:2, 7",
                vec![
                    disk("disk1", 1),
                    disk("disk2", 5),
                    disk("disk3", 2),
                    disk("disk4", 7),
                ],
                "disk1:1, disk2:5, disk3:2, disk4:7",
            ),
            (
                "DISK1:1, 5",
                vec![disk("DISK1", 1), disk("disk2", 5)],
                "DISK1:1, disk2:5",
            ),
        ];
        for (input, want, format) in &cases {
            let got = parse_disks(input).unwrap_or_else(|e| panic!("parse_disks({input:?}): {e}"));
            assert_eq!(&got, want, "parse_disks({input:?})");
            assert_eq!(format_disks(&got), *format, "format_disks of {input:?}");
            assert_eq!(
                parse_disks(&format_disks(&got)).unwrap(),
                got,
                "round trip of {input:?}"
            );
        }
    }

    #[test]
    fn parse_disks_rejects_bad_entries() {
        let bad = [
            ("data", "expected [name:]size"),
            ("data:x", "expected [name:]size"),
            (":5", "expected [name:]size"),
            ("a:b:5", "expected [name:]size"),
            ("data:0", "at least 1 GiB"),
            ("data:-3", "at least 1 GiB"),
            ("disk:5", "taken by the main disk"),
            ("Disk:5", "taken by the main disk"),
            ("1data:5", "starting with a letter"),
            ("a b:5", "letters, digits"),
            ("averyveryverylongdiskname:5", "at most 20 characters"),
            ("Data:5, data:6", "duplicate disk name"),
            (
                "a:1,b:1,c:1,d:1,e:1,f:1,g:1,h:1,i:1",
                "at most 8 additional disks",
            ),
        ];
        for (input, want) in bad {
            let err = parse_disks(input).expect_err(input);
            assert!(
                text(&err).contains(want),
                "parse_disks({input:?}) = {err}, want {want:?}"
            );
        }
        assert_eq!(
            text(&parse_disks("data").unwrap_err()),
            "invalid disk \"data\" — expected [name:]size in GiB, e.g. data:50"
        );
        assert_eq!(
            text(&parse_disks("1data:5").unwrap_err()),
            "disk name \"1data\": letters, digits, hyphens and underscores, starting with a letter, at most 20 characters"
        );
        assert_eq!(
            text(&parse_disks("Data:5, data:6").unwrap_err()),
            "duplicate disk name \"data\""
        );
    }

    #[test]
    fn diff_disks_reports_added_grown_removed() {
        let old = vec![disk("data", 50), disk("scratch", 10), disk("logs", 5)];
        let cur = vec![disk("data", 80), disk("logs", 5), disk("new", 1)];
        let ch = diff_disks(&old, &cur).unwrap();
        assert_eq!(
            ch,
            DiskChange {
                added: vec![disk("new", 1)],
                grown: vec![disk("data", 80)],
                removed: vec![disk("scratch", 10)],
            }
        );
        assert!(ch.any());
        let same = diff_disks(&old, &old).unwrap();
        assert!(!same.any(), "no change: {same:?}");
        let err = diff_disks(&old, &[disk("data", 20)]).unwrap_err();
        assert_eq!(
            text(&err),
            "disk \"data\" can only grow (currently 50 GiB) — shrinking would destroy data"
        );
        assert_eq!(disk_names(&old), "data, scratch, logs");
    }

    #[test]
    fn root_ports_and_device_specs() {
        let ports = disk_port_args();
        assert_eq!(ports.len(), 2 * MAX_EXTRA_DISKS);
        for i in 0..MAX_EXTRA_DISKS {
            assert_eq!(ports[2 * i], "-device");
            assert_eq!(
                ports[2 * i + 1],
                format!(
                    "pcie-root-port,id=disk-rp{},bus=pcie.0,chassis={},addr=0x{:x}",
                    i + 1,
                    i + 1,
                    0x10 + i
                )
            );
        }
        assert_eq!(disk_port_id(0), "disk-rp1");
        assert_eq!(disk_device_id("data"), "disk-data");
        assert_eq!(disk_drive_id("data"), "disk-data-drive");
        assert_eq!(
            disk_device("scratch", 1),
            "virtio-blk-pci,id=disk-scratch,drive=disk-scratch-drive,bus=disk-rp2,serial=scratch"
        );
    }

    /// Keys missing from a `disks:` entry, or null, read as zero values, as
    /// with Go's YAML decoder: the VM still loads, and validation says what
    /// is wrong.
    #[test]
    fn disks_with_missing_keys_load_and_fail_validation() {
        let storage = tempfile::tempdir().unwrap();
        let storage = storage.path();
        fs::create_dir_all(vm_dir(storage, "v")).unwrap();
        fs::write(
            config_file_path(storage, "v"),
            "name: v\ncpu: 1\nram: 512\ndisk_size: 8\ndisks:\n  - name: data\n  - size: 5\n  - name: ~\n    size:\n  - {}\n",
        )
        .unwrap();
        let cfg = load_config(storage, "v").unwrap();
        assert_eq!(
            cfg.disks,
            [disk("data", 0), disk("", 5), disk("", 0), Disk::default()]
        );
        assert_eq!(format_disks(&cfg.disks), "data:0, :5, :0, :0");
        assert_eq!(
            text(&validate_disks(&cfg.disks).unwrap_err()),
            "disk \"data\": size must be at least 1 GiB"
        );
        assert_eq!(
            text(&validate_disks(&cfg.disks[1..]).unwrap_err()),
            "disk name \"\": letters, digits, hyphens and underscores, starting with a letter, at most 20 characters"
        );
        // The listing keeps the VM rather than dropping it as unreadable.
        let listed = Manager::new(storage).list().unwrap();
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert_eq!(listed[0].disks, cfg.disks);

        // A null item (a bare `-`, or `~`) is a zero disk; a null list is empty.
        fs::write(
            config_file_path(storage, "v"),
            "name: v\ncpu: 1\nram: 512\ndisk_size: 8\ndisks:\n  -\n  - ~\n  - name: data\n    size: 2\n",
        )
        .unwrap();
        let cfg = load_config(storage, "v").unwrap();
        assert_eq!(
            cfg.disks,
            [Disk::default(), Disk::default(), disk("data", 2)]
        );
        fs::write(
            config_file_path(storage, "v"),
            "name: v\ncpu: 1\nram: 512\ndisk_size: 8\ndisks: ~\n",
        )
        .unwrap();
        assert!(load_config(storage, "v").unwrap().disks.is_empty());
    }

    #[test]
    fn image_create_refuses_existing_file_and_quotes_qemu_img() {
        if which::which(QEMU_IMG_BIN).is_err() {
            eprintln!("skipping: {QEMU_IMG_BIN} not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.qcow2");
        create_disk_image(&path, 1).unwrap();
        let err = create_disk_image(&path, 1).unwrap_err();
        assert_eq!(text(&err), format!("{} already exists", path.display()));
        // A directory counts as existing too.
        let err = create_disk_image(dir.path(), 1).unwrap_err();
        assert!(text(&err).ends_with("already exists"), "{err}");
        // qemu-img's own complaint comes after the status, on its own line.
        let err = create_disk_image(&dir.path().join("huge.qcow2"), u32::MAX).unwrap_err();
        let msg = text(&err);
        assert!(msg.starts_with("qemu-img create: exit status 1\n"), "{msg}");
        assert!(msg.contains("qemu-img:"), "{msg}");
        resize_disk_image(&path, 2).unwrap();
        let err = resize_disk_image(&dir.path().join("none.qcow2"), 2).unwrap_err();
        assert!(
            text(&err).starts_with("qemu-img resize: exit status 1\n"),
            "{err}"
        );
    }

    /// qemu-img's output pipe is close-on-exec: a long-lived child that
    /// another thread spawns while qemu-img runs must not get a copy of it,
    /// or reading the output to its end waits for that child too.
    #[test]
    fn qemu_img_output_pipe_stays_out_of_other_children() {
        let stop = Arc::new(AtomicBool::new(false));
        let spawner = thread::spawn({
            let stop = Arc::clone(&stop);
            move || {
                let mut kids = Vec::new();
                while !stop.load(Ordering::Relaxed) && kids.len() < 300 {
                    let kid = Command::new("sleep")
                        .arg("5")
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn();
                    kids.extend(kid.ok());
                }
                kids
            }
        });
        let mut worst = Duration::ZERO;
        for _ in 0..100 {
            let started = Instant::now();
            let result = run_qemu_img("create", Command::new("true"));
            worst = worst.max(started.elapsed());
            if result.is_err() || worst > Duration::from_secs(2) {
                break;
            }
        }
        stop.store(true, Ordering::Relaxed);
        for mut kid in spawner.join().unwrap() {
            let _ = kid.kill();
            let _ = kid.wait();
        }
        assert!(
            worst < Duration::from_secs(2),
            "a qemu-img run took {worst:?}"
        );
    }

    #[test]
    fn build_qemu_args_extra_disks() {
        for arch in ["x86_64", "aarch64"] {
            let tmp = tempfile::tempdir().unwrap();
            // A comma: the option escaping must hold up.
            let storage = tmp.path().join("vms, here");
            let mut cfg = VmConfig {
                name: "t".into(),
                cpu: 1,
                ram: 128,
                arch: arch.into(),
                network: NetworkConfig {
                    kind: NetworkType::None,
                    ..Default::default()
                },
                ..Default::default()
            };
            let (_, args) = build_qemu_args(&cfg, &storage).unwrap();
            let joined = args.join(" ");
            // Every VM gets the root ports, pinned to the same slots, disks or not.
            for i in 0..MAX_EXTRA_DISKS {
                let want = format!(
                    "-device pcie-root-port,id=disk-rp{},bus=pcie.0,chassis={},addr=0x{:x}",
                    i + 1,
                    i + 1,
                    0x10 + i
                );
                assert!(
                    joined.contains(&want),
                    "{arch}: args lack {want:?}:\n{joined}"
                );
            }
            assert_eq!(
                joined.matches("pcie-root-port").count(),
                MAX_EXTRA_DISKS,
                "{arch}"
            );
            assert!(
                !joined.contains("virtio-blk-pci,id=disk-"),
                "{arch}: no extra disks configured, yet:\n{joined}"
            );

            cfg.disks = vec![disk("data", 50), disk("scratch", 10)];
            let (_, args) = build_qemu_args(&cfg, &storage).unwrap();
            let joined = args.join(" ");
            for (i, d) in cfg.disks.iter().enumerate() {
                let path = extra_disk_path(&storage, "t", &d.name)
                    .to_string_lossy()
                    .replace(',', ",,");
                let want = format!(
                    "-drive if=none,id=disk-{n}-drive,format=qcow2,file={path} -device virtio-blk-pci,id=disk-{n},drive=disk-{n}-drive,bus=disk-rp{p},serial={n}",
                    n = d.name,
                    p = i + 1
                );
                assert!(
                    joined.contains(&want),
                    "{arch}: args lack {want:?}:\n{joined}"
                );
            }
            // The main disk comes first (the guest numbers it vda), the ports
            // before the devices that sit on them.
            let at = |s: &str| {
                joined
                    .find(s)
                    .unwrap_or_else(|| panic!("{arch}: no {s:?} in {joined}"))
            };
            assert!(
                at("/t/disk.qcow2") < at("pcie-root-port")
                    && at("id=disk-rp8") < at("virtio-blk-pci,id=disk-"),
                "{arch}: order wrong:\n{joined}"
            );
        }

        let tmp = tempfile::tempdir().unwrap();
        let cfg = VmConfig {
            name: "t".into(),
            cpu: 1,
            ram: 128,
            network: NetworkConfig {
                kind: NetworkType::None,
                ..Default::default()
            },
            disks: (0..=MAX_EXTRA_DISKS)
                .map(|i| disk(&format!("d{i}"), 1))
                .collect(),
            ..Default::default()
        };
        let err = build_qemu_args(&cfg, tmp.path()).unwrap_err();
        assert!(text(&err).contains("at most 8"), "too many disks: {err}");
    }

    /// Start fails up front, with the path, rather than launching a QEMU
    /// that dies on its discarded stderr.
    #[test]
    fn start_refuses_missing_extra_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = tmp.path();
        let cfg = VmConfig {
            name: "t".into(),
            cpu: 1,
            ram: 128,
            network: NetworkConfig {
                kind: NetworkType::None,
                ..Default::default()
            },
            disks: vec![disk("data", 1)],
            ..Default::default()
        };
        fs::create_dir_all(vm_dir(storage, &cfg.name)).unwrap();
        let err = start(storage, &cfg).unwrap_err();
        let msg = text(&err);
        assert!(
            msg.contains("data.qcow2") && msg.contains("edit form"),
            "start error = {msg}"
        );
        let info = status(storage, &cfg.name).unwrap();
        assert_eq!(
            info.status,
            VmStatus::Stopped,
            "a VM must not be started with a missing disk: {info:?}"
        );
    }

    #[test]
    fn check_extra_disks_names_every_missing_image() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = tmp.path();
        let cfg = VmConfig {
            name: "t".into(),
            disks: vec![disk("data", 1), disk("scratch", 1), disk("logs", 1)],
            ..Default::default()
        };
        fs::create_dir_all(vm_dir(storage, "t")).unwrap();
        fs::write(extra_disk_path(storage, "t", "scratch"), [0u8; 16]).unwrap();
        let err = check_extra_disks(storage, &cfg).unwrap_err();
        assert_eq!(
            text(&err),
            format!(
                "disk data: image not found: {}\ndisk logs: image not found: {}\nRemove it in the edit form, or put the file back.",
                extra_disk_path(storage, "t", "data").display(),
                extra_disk_path(storage, "t", "logs").display()
            )
        );
        let states = disk_states(storage, &cfg);
        assert_eq!(states.len(), 3);
        assert!(
            !states[0].ok() && states[1].ok() && !states[2].ok(),
            "{states:?}"
        );
        assert_eq!(states[1].size, 16);

        let one = VmConfig {
            disks: vec![disk("scratch", 1)],
            ..cfg
        };
        check_extra_disks(storage, &one).unwrap();
    }

    /// Serves one HMP command per connection on `sock` the way QEMU's
    /// readline monitor does: banner and prompt, the readline-style echo of
    /// the command, the reply, the prompt again.
    fn fake_hmp_sessions(sock: &Path, reply: impl Fn(&str) -> String + Send + 'static) {
        let listener = UnixListener::bind(sock).unwrap();
        thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { return };
                let _ = conn
                    .write_all(b"QEMU 9.2.0 monitor - type 'help' for more information\r\n(qemu) ");
                let mut line = String::new();
                if BufReader::new(&conn).read_line(&mut line).is_err() {
                    continue;
                }
                let cmd = line.trim_end_matches('\n');
                let mut echo = String::new();
                for (i, (pos, c)) in cmd.char_indices().enumerate() {
                    echo.push_str(&"\x1b[D".repeat(i));
                    echo.push_str(&cmd[..pos + c.len_utf8()]);
                    echo.push_str("\x1b[K");
                }
                echo.push_str("\r\n");
                let _ = conn.write_all(format!("{echo}{}(qemu) ", reply(cmd)).as_bytes());
            }
        });
    }

    #[test]
    fn disk_hotplug_hmp() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = tmp.path();
        let cfg = VmConfig {
            name: "t".into(),
            disks: vec![disk("data", 1), disk("scratch", 2)],
            ..Default::default()
        };
        fs::create_dir_all(vm_dir(storage, "t")).unwrap();
        // Only has to exist.
        let scratch = vm_dir(storage, "t").join("scratch.qcow2");
        fs::write(&scratch, [0u8; 16]).unwrap();

        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let device_reply = Arc::new(Mutex::new(String::new()));
        {
            let (seen, device_reply) = (Arc::clone(&seen), Arc::clone(&device_reply));
            fake_hmp_sessions(&monitor_path(storage, "t"), move |cmd| {
                seen.lock().unwrap().push(cmd.to_string());
                if cmd.starts_with("drive_add ") {
                    "OK\r\n".to_string()
                } else if cmd.starts_with("device_add ") {
                    device_reply.lock().unwrap().clone()
                } else {
                    String::new()
                }
            });
        }
        let commands = || std::mem::take(&mut *seen.lock().unwrap());

        disk_hotplug(storage, &cfg, 1).unwrap();
        let want = vec![
            format!("drive_add 0 \"if=none,id=disk-scratch-drive,format=qcow2,file={}\"", scratch.display()),
            "device_add virtio-blk-pci,id=disk-scratch,drive=disk-scratch-drive,bus=disk-rp2,serial=scratch".to_string(),
        ];
        assert_eq!(commands(), want);

        // A missing image is caught before anything reaches the monitor.
        let err = disk_hotplug(storage, &cfg, 0).unwrap_err();
        assert!(text(&err).contains("not found"), "missing image: {err}");
        assert!(commands().is_empty(), "monitor reached for a missing image");

        // QEMU's own error surfaces, and the drive added first is rolled back.
        *device_reply.lock().unwrap() = "Error: Bus 'disk-rp2' not found\r\n".to_string();
        let err = disk_hotplug(storage, &cfg, 1).unwrap_err();
        assert!(
            text(&err).contains("Bus 'disk-rp2' not found"),
            "device_add failure: {err}"
        );
        let got = commands();
        assert!(
            got.len() == 3 && got[2] == "drive_del disk-scratch-drive",
            "no rollback after a failed device_add: {got:?}"
        );

        let err = disk_hotplug(storage, &cfg, 5).unwrap_err();
        assert_eq!(text(&err), "no disk at index 5");
    }

    /// Polls the monitor until it answers `info qtree`.
    fn wait_for_monitor(storage: &Path, name: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if let Ok(out) = monitor_command(storage, name, "info qtree") {
                if !out.is_empty() {
                    return out;
                }
            }
            thread::sleep(Duration::from_millis(200));
        }
        panic!(
            "QEMU monitor did not come up (status {:?})",
            status(storage, name)
        );
    }

    /// Polls `info block` until it does (or does not) mention `path`.
    fn wait_for_block(storage: &Path, name: &str, path: &str, present: bool) -> String {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut out = String::new();
        while Instant::now() < deadline {
            out = monitor_command(storage, name, "info block").unwrap_or_default();
            if out.contains(path) == present {
                return out;
            }
            thread::sleep(Duration::from_millis(250));
        }
        panic!("drive for {path} present={present} not reached; info block:\n{out}");
    }

    /// Boots a real VM with one extra disk, hot-plugs a second one onto its
    /// root port and checks QEMU's view of both.
    #[test]
    fn extra_disks_with_qemu() {
        for bin in ["qemu-system-x86_64", QEMU_IMG_BIN] {
            if which::which(bin).is_err() {
                eprintln!("skipping: {bin} not installed");
                return;
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let storage = tmp.path();
        let mut cfg = VmConfig {
            name: "disktest".into(),
            cpu: 1,
            ram: 128,
            disk_size: 1,
            network: NetworkConfig {
                kind: NetworkType::None,
                ..Default::default()
            },
            disks: vec![disk("data", 1)],
            ..Default::default()
        };
        Manager::new(storage).create(&mut cfg).unwrap();
        start(storage, &cfg).unwrap();
        struct StopOnDrop<'a>(&'a Path, String);
        impl Drop for StopOnDrop<'_> {
            fn drop(&mut self) {
                let _ = stop(self.0, &self.1);
            }
        }
        let _guard = StopOnDrop(storage, cfg.name.clone());

        wait_for_monitor(storage, &cfg.name);
        let data = extra_disk_path(storage, &cfg.name, "data");
        let block = wait_for_block(storage, &cfg.name, &data.to_string_lossy(), true);
        assert!(
            block.contains("disk-data-drive") && block.contains("/disk-data/"),
            "boot-time disk not attached under its IDs:\n{block}"
        );

        // Hot-plug a second disk, the way an edit of a running VM does: the
        // image is made first, then the device goes onto the next root port.
        cfg.disks.push(disk("scratch", 1));
        let scratch = extra_disk_path(storage, &cfg.name, "scratch");
        create_disk_image(&scratch, 1).unwrap();
        disk_hotplug(storage, &cfg, 1).unwrap();
        let scratch_str = scratch.to_string_lossy();
        let block = wait_for_block(storage, &cfg.name, &scratch_str, true);
        assert!(
            block.contains("/disk-scratch/"),
            "hot-plugged disk not attached to its device:\n{block}"
        );
        let qtree = monitor_command(storage, &cfg.name, "info qtree").unwrap_or_default();
        let mut port = &qtree[qtree.find("bus: disk-rp2").expect("disk-rp2 in qtree")..];
        if let Some(end) = port[1..].find("dev: pcie-root-port") {
            port = &port[..end];
        }
        assert!(
            port.contains("id \"disk-scratch\"") && port.contains("serial = \"scratch\""),
            "hot-plugged disk should sit on disk-rp2 with its serial:\n{port}"
        );

        // Plugging the same disk twice fails with QEMU's message, and the
        // failed attempt leaves no second drive behind.
        let err = disk_hotplug(storage, &cfg, 1).unwrap_err();
        assert!(
            text(&err).contains("disk-scratch"),
            "duplicate hot-plug should fail naming the disk, got: {err}"
        );
        let block = monitor_command(storage, &cfg.name, "info block").unwrap_or_default();
        assert_eq!(
            block.matches(&*scratch_str).count(),
            1,
            "failed hot-plug left a drive behind:\n{block}"
        );

        stop(storage, &cfg.name).unwrap();
    }
}
