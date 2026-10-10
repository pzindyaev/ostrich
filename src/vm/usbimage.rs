//! Disk images attached to the guest as read-only USB mass-storage drives
//! (the USB drives of the ISO dialog), and the image checks shared by
//! the boot ISO and the extra disks.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};

use super::config::{null_as_default, VmConfig};
use super::monitor::{hmp_quote, monitor_command, monitor_must_succeed};
use super::process::qemu_opt_escape;
use super::usb::USB_BUS_NAME;

/// A disk image on the host — an ISO as a rule — attached to the guest as a
/// read-only USB mass-storage drive (`vm.yaml` schema). The guest sees a USB
/// stick holding the image byte for byte: a Linux guest mounts the ISO9660
/// filesystem straight off it, and a fresh VM boots a hybrid ISO from it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbImage {
    /// Absolute path on the host. A missing key or a null loads as `""` (as
    /// yaml.v3 did), which [`UsbImage::validate`] reports as `no image path`
    /// on start, and the entry can still be detached.
    #[serde(default, deserialize_with = "null_as_default")]
    pub path: String,
}

impl UsbImage {
    /// The file name.
    pub fn label(&self) -> String {
        base_name(&self.path).to_string()
    }

    /// Checks that the image is an absolute path to a readable file.
    /// Absolute, because QEMU reads a prefix such as `nbd:` in a file name
    /// as a protocol, and a running VM resolves relative paths against its
    /// own cwd. Error for a relative path: `image path must be absolute: <path>`;
    /// otherwise what `check_image` says.
    pub fn validate(&self) -> Result<()> {
        if !self.path.is_empty() && !Path::new(&self.path).is_absolute() {
            bail!("image path must be absolute: {}", self.path);
        }
        check_image(&self.path)
    }
}

/// The last element of `path` the way Go's `filepath.Base` sees it, which
/// is what the device IDs were derived from: trailing slashes are dropped
/// first, `""` is `.` and `/` stays `/`.
fn base_name(path: &str) -> &str {
    if path.is_empty() {
        return ".";
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/";
    }
    match trimmed.rfind('/') {
        Some(i) => &trimmed[i + 1..],
        None => trimmed,
    }
}

/// Checks that `path` names a readable file. QEMU refuses to start without
/// one, and that only shows on its discarded stderr. Errors, verbatim:
/// `no image path`, `image not found: <path>`, `image <path>: <io error>`,
/// `image is a directory: <path>`, `no read access to image <path>`.
pub(crate) fn check_image(path: &str) -> Result<()> {
    if path.is_empty() {
        bail!("no image path");
    }
    let meta = match fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => bail!("image not found: {path}"),
        Err(e) => bail!("image {path}: {e}"),
    };
    if meta.is_dir() {
        bail!("image is a directory: {path}");
    }
    if File::open(path).is_err() {
        bail!("no read access to image {path}");
    }
    Ok(())
}

/// What QEMU does not allow in an object ID after the first character
/// (letters, digits, `.`, `_` and `-` are fine).
fn usb_id_unsafe(c: char) -> bool {
    !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// The QEMU device IDs for the configured images, derived from the file
/// name (`usbimg-<name with unsafe chars as '-'>`) so an image attached at
/// boot can later be named for hot-unplug. Two images with the same file
/// name get a numeric suffix (`-2`, `-3`, …).
pub fn usb_image_ids(imgs: &[UsbImage]) -> Vec<String> {
    let mut ids = Vec::with_capacity(imgs.len());
    let mut seen: HashMap<String, usize> = HashMap::with_capacity(imgs.len());
    for img in imgs {
        let mut id = String::from("usbimg-");
        id.extend(
            base_name(&img.path)
                .chars()
                .map(|c| if usb_id_unsafe(c) { '-' } else { c }),
        );
        let n = seen.entry(id.clone()).or_insert(0);
        *n += 1;
        ids.push(if *n > 1 { format!("{id}-{n}") } else { id });
    }
    ids
}

/// The block backend behind the usb-storage device attached at boot:
/// `<id>-drive`. Hot-plug appends a unique suffix: QEMU deletes the drive of
/// an unplugged device only once it finalizes the device object, which it
/// defers, so a re-plug of the same image must not reuse the ID meanwhile.
pub(crate) fn usb_image_drive_id(id: &str) -> String {
    format!("{id}-drive")
}

/// `if=none,id=<drive>,format=raw,readonly=on,file=<path>` (commas in the
/// path doubled), used verbatim for `-drive` and `drive_add`. The image is
/// opened read-only, so the file is never modified and several VMs can
/// share it.
pub(crate) fn usb_image_drive(img: &UsbImage, drive_id: &str) -> String {
    format!(
        "if=none,id={drive_id},format=raw,readonly=on,file={}",
        qemu_opt_escape(&img.path)
    )
}

/// `usb-storage,id=<id>,bus=xhci.0,drive=<drive>,removable=on`, used for
/// `-device` and `device_add`. The guest sees a removable drive, like a real
/// stick, on the always-present xHCI bus.
pub(crate) fn usb_image_device(id: &str, drive_id: &str) -> String {
    format!("usb-storage,id={id},bus={USB_BUS_NAME},drive={drive_id},removable=on")
}

/// Validates every configured image and names each one that would stop
/// QEMU from starting: lines `USB drive <label>: <error>` followed by
/// `Detach it in the ISO dialog (i), or put the file back.`
pub fn check_usb_images(imgs: &[UsbImage]) -> Result<()> {
    let mut msg = String::new();
    for img in imgs {
        if let Err(e) = img.validate() {
            msg.push_str(&format!("USB drive {}: {e:#}\n", img.label()));
        }
    }
    if msg.is_empty() {
        return Ok(());
    }
    bail!("{msg}Detach it in the ISO dialog (i), or put the file back.")
}

/// An image path paired with what the host shows for it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageState {
    pub path: String,
    /// Bytes, when the file is there.
    pub size: u64,
    /// `None` when the image is present and readable; the error text otherwise.
    pub err: Option<String>,
}

impl ImageState {
    /// Whether the image is present and readable.
    pub fn ok(&self) -> bool {
        self.err.is_none()
    }
}

/// Checks one image file on the host.
pub fn image_state_of(path: &str) -> ImageState {
    let mut state = ImageState {
        path: path.to_string(),
        ..Default::default()
    };
    if let Err(e) = check_image(path) {
        state.err = Some(format!("{e:#}"));
        return state;
    }
    if let Ok(meta) = fs::metadata(path) {
        state.size = meta.len();
    }
    state
}

/// Checks each configured image on the host (validate() errors win over the
/// plain file check).
pub fn usb_image_states(imgs: &[UsbImage]) -> Vec<ImageState> {
    imgs.iter()
        .map(|img| {
            let mut state = image_state_of(&img.path);
            if let Err(e) = img.validate() {
                state.err = Some(format!("{e:#}"));
            }
            state
        })
        .collect()
}

// --- Hot-plug ---

/// Nanoseconds since the Unix epoch; zero should the clock predate it.
fn unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// `n` in base 36 with lower-case digits, like Go's `strconv.FormatInt(n, 36)`.
fn base36(mut n: u128) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".to_string();
    }
    let mut digits = Vec::with_capacity(16);
    while n > 0 {
        digits.push(char::from(DIGITS[(n % 36) as usize]));
        n /= 36;
    }
    digits.iter().rev().collect()
}

/// Attaches `cfg.usb_images[idx]` to the running VM through the monitor: the
/// drive first (`drive_add 0 "<opts>"`, which answers `OK`; the drive ID gets
/// a unique base-36 timestamp suffix), then the usb-storage device on top of
/// it; on failure the orphan drive is deleted again.
pub fn usb_image_hotplug(storage: &Path, cfg: &VmConfig, idx: usize) -> Result<()> {
    let img = cfg
        .usb_images
        .get(idx)
        .ok_or_else(|| anyhow!("no USB image at index {idx}"))?;
    img.validate()?;
    let id = usb_image_ids(&cfg.usb_images).swap_remove(idx);
    let drive_id = format!("{}-{}", usb_image_drive_id(&id), base36(unix_nanos()));
    // Unlike device_add, drive_add answers "OK" on success. Its first argument
    // is a PCI address that is ignored for if=none drives; the options are one
    // HMP string argument, so they are quoted in case the path has spaces.
    let resp = monitor_command(
        storage,
        &cfg.name,
        &format!(
            "drive_add 0 {}",
            hmp_quote(&usb_image_drive(img, &drive_id))
        ),
    )?;
    if resp != "OK" {
        bail!("QEMU: {resp}");
    }
    let device = format!("device_add {}", usb_image_device(&id, &drive_id));
    if let Err(e) = monitor_must_succeed(storage, &cfg.name, &device) {
        // Leave no orphan drive behind holding the file open.
        let _ = monitor_command(storage, &cfg.name, &format!("drive_del {drive_id}"));
        return Err(e);
    }
    Ok(())
}

/// Detaches `cfg.usb_images[idx]` from the running VM (`device_del <id>`);
/// `cfg` is the config as it was before the entry was removed, so the
/// device ID matches. QEMU drops the drive together with the device,
/// shortly after.
pub fn usb_image_hotunplug(storage: &Path, cfg: &VmConfig, idx: usize) -> Result<()> {
    let ids = usb_image_ids(&cfg.usb_images);
    let id = ids
        .get(idx)
        .ok_or_else(|| anyhow!("no USB image at index {idx}"))?;
    monitor_must_succeed(storage, &cfg.name, &format!("device_del {id}"))
}

/// Helpers shared by the tests that put image files in front of a VM.
#[cfg(test)]
pub(crate) mod testutil {
    use std::fs;
    use std::path::Path;
    use std::thread;
    use std::time::{Duration, Instant};

    use crate::vm::monitor::monitor_command;
    use crate::vm::process::{status, stop};

    /// Creates a small zero-filled image file and returns its path.
    pub(crate) fn write_image(dir: &Path, name: &str, size: usize) -> String {
        let path = dir.join(name);
        fs::write(&path, vec![0u8; size]).unwrap();
        path.to_string_lossy().into_owned()
    }

    /// Stops the VM when dropped, so a failing test leaves no QEMU behind.
    pub(crate) struct StopOnDrop<'a> {
        pub storage: &'a Path,
        pub name: &'a str,
    }

    impl Drop for StopOnDrop<'_> {
        fn drop(&mut self) {
            let _ = stop(self.storage, self.name);
        }
    }

    /// Polls the monitor until it answers `info qtree`, and returns the
    /// output.
    pub(crate) fn wait_for_monitor(storage: &Path, name: &str) -> String {
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

    /// Polls `info usb` until the device with the QEMU ID is (or is no
    /// longer) listed, and returns the output.
    pub(crate) fn wait_for_usb(storage: &Path, name: &str, qemu_id: &str, present: bool) -> String {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut out = String::new();
        while Instant::now() < deadline {
            out = monitor_command(storage, name, "info usb").unwrap_or_default();
            if out.contains(&format!("ID: {qemu_id}")) == present {
                return out;
            }
            thread::sleep(Duration::from_millis(250));
        }
        panic!("device {qemu_id} present={present} not reached; info usb:\n{out}");
    }

    /// Polls `info block` until a drive backed by `path` is (or is no
    /// longer) listed, and returns the output.
    pub(crate) fn wait_for_block(storage: &Path, name: &str, path: &str, present: bool) -> String {
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
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use super::testutil::*;
    use super::*;
    use crate::vm::config::{monitor_path, vm_dir, NetworkConfig, NetworkType};
    use crate::vm::manager::Manager;
    use crate::vm::monitor::testutil::fake_hmp_sessions;
    use crate::vm::process::{build_qemu_args, start, status, stop, VmStatus};

    fn image(path: &str) -> UsbImage {
        UsbImage {
            path: path.to_string(),
        }
    }

    fn path_string(p: &Path) -> String {
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn label_is_the_file_name() {
        assert_eq!(image("/isos/a.iso").label(), "a.iso");
        assert_eq!(
            image("/isos/My Stuff (2024), v2.iso").label(),
            "My Stuff (2024), v2.iso"
        );
        assert_eq!(image("a.iso").label(), "a.iso");
        assert_eq!(image("/isos/dir/").label(), "dir");
        assert_eq!(image("").label(), ".");
        assert_eq!(image("/").label(), "/");
    }

    #[test]
    fn validate_checks_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let ok = image(&write_image(dir.path(), "ok.iso", 16));
        ok.validate().unwrap();

        let nope = path_string(&dir.path().join("nope.iso"));
        let cases = [
            ("empty", image(""), "no image path".to_string()),
            (
                "relative",
                image("ok.iso"),
                "image path must be absolute: ok.iso".to_string(),
            ),
            ("missing", image(&nope), format!("image not found: {nope}")),
            (
                "directory",
                image(&path_string(dir.path())),
                format!("image is a directory: {}", dir.path().display()),
            ),
        ];
        for (name, img, want) in cases {
            let Err(err) = img.validate() else {
                panic!("{name}: {img:?} accepted");
            };
            assert_eq!(err.to_string(), want, "{name}");
            if name == "missing" {
                assert!(
                    err.to_string().contains("not found"),
                    "missing file error = {err}"
                );
            }
        }

        // A file this user may not open (root may open anything).
        let secret = write_image(dir.path(), "secret.iso", 16);
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o000)).unwrap();
        if File::open(&secret).is_err() {
            let err = image(&secret).validate().unwrap_err();
            assert_eq!(err.to_string(), format!("no read access to image {secret}"));
        } else {
            eprintln!("skipping: running as root, every file is readable");
        }

        // A stat failure other than "not found" names the path and the reason.
        let locked = dir.path().join("locked");
        fs::create_dir(&locked).unwrap();
        let inside = write_image(&locked, "x.iso", 16);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::metadata(&inside).is_err() {
            let err = image(&inside).validate().unwrap_err().to_string();
            assert!(err.starts_with(&format!("image {inside}: ")), "{err}");
            assert!(!err.contains("not found"), "{err}");
        }
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

        let states = usb_image_states(&[ok.clone(), image(&nope)]);
        assert!(
            states[0].err.is_none() && states[0].size == 16 && states[1].err.is_some(),
            "states = {states:?}"
        );
        assert!(states[0].ok() && !states[1].ok());

        let err = check_usb_images(&[ok, image(&nope)])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("nope.iso") && !err.contains("ok.iso"),
            "check_usb_images error = {err}"
        );
        assert_eq!(
            err,
            format!("USB drive nope.iso: image not found: {nope}\nDetach it in the ISO dialog (i), or put the file back.")
        );
        check_usb_images(&[]).expect("no images must pass");
    }

    #[test]
    fn image_states_carry_size_or_error() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            image_state_of(""),
            ImageState {
                path: String::new(),
                size: 0,
                err: Some("no image path".to_string())
            }
        );
        let ok = write_image(dir.path(), "ok.iso", 1024);
        assert_eq!(
            image_state_of(&ok),
            ImageState {
                path: ok.clone(),
                size: 1024,
                err: None
            }
        );
        let nope = path_string(&dir.path().join("nope.iso"));
        let state = image_state_of(&nope);
        assert_eq!(
            state.err.as_deref(),
            Some(format!("image not found: {nope}").as_str())
        );
        assert_eq!(state.size, 0);

        // usb_image_states lets validate() have the last word.
        let states = usb_image_states(&[image("rel.iso"), image(&ok)]);
        assert_eq!(
            states[0].err.as_deref(),
            Some("image path must be absolute: rel.iso")
        );
        assert_eq!(
            states[1],
            ImageState {
                path: ok,
                size: 1024,
                err: None
            }
        );
    }

    #[test]
    fn ids_and_device_spec() {
        let imgs = [
            image("/isos/virtio-win.iso"),
            image("/isos/My Stuff (2024), v2.iso"), // spaces, parens and a comma
            image("/other/virtio-win.iso"),         // same file name elsewhere
        ];
        let ids = usb_image_ids(&imgs);
        assert_eq!(
            ids,
            [
                "usbimg-virtio-win.iso",
                "usbimg-My-Stuff--2024---v2.iso",
                "usbimg-virtio-win.iso-2"
            ]
        );
        // A third copy counts on.
        let more = usb_image_ids(&[imgs[0].clone(), imgs[2].clone(), image("/x/virtio-win.iso")]);
        assert_eq!(more[2], "usbimg-virtio-win.iso-3");
        assert!(usb_image_ids(&[]).is_empty());
        // Non-ASCII is unsafe for an ID too, one dash per character.
        assert_eq!(
            usb_image_ids(&[image("/isos/Grüße.iso")])[0],
            "usbimg-Gr--e.iso"
        );

        let drive = usb_image_drive_id(&ids[0]);
        assert_eq!(drive, "usbimg-virtio-win.iso-drive");
        assert_eq!(
            usb_image_device(&ids[0], &drive),
            "usb-storage,id=usbimg-virtio-win.iso,bus=xhci.0,drive=usbimg-virtio-win.iso-drive,removable=on"
        );
    }

    #[test]
    fn drive_spec_doubles_commas() {
        let imgs = [
            image("/isos/virtio-win.iso"),
            image("/isos/My Stuff (2024), v2.iso"),
        ];
        let ids = usb_image_ids(&imgs);
        assert_eq!(
            usb_image_drive(&imgs[0], &usb_image_drive_id(&ids[0])),
            "if=none,id=usbimg-virtio-win.iso-drive,format=raw,readonly=on,file=/isos/virtio-win.iso"
        );
        // A comma in the path has to be doubled for QEMU's option parser.
        let got = usb_image_drive(&imgs[1], &usb_image_drive_id(&ids[1]));
        assert!(
            got.ends_with(",file=/isos/My Stuff (2024),, v2.iso"),
            "drive with comma = {got:?}"
        );
    }

    #[test]
    fn base36_matches_go() {
        assert_eq!(base36(0), "0");
        assert_eq!(base36(35), "z");
        assert_eq!(base36(36), "10");
        assert_eq!(base36(1_700_000_000_000_000_000), "cwyvpelgpse8");
        assert_eq!(base36(i64::MAX as u128), "1y2p0ij32e8e7");
        assert!(unix_nanos() > 1_600_000_000_000_000_000);
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
    fn build_qemu_args_attaches_images() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = config("t");
        cfg.usb_images = vec![image("/isos/a.iso"), image("/isos/b.iso")];
        let (_, args) = build_qemu_args(&cfg, dir.path()).unwrap();
        let joined = args.join(" ");
        for want in [
            "-drive if=none,id=usbimg-a.iso-drive,format=raw,readonly=on,file=/isos/a.iso -device usb-storage,id=usbimg-a.iso,bus=xhci.0,drive=usbimg-a.iso-drive,removable=on",
            "-drive if=none,id=usbimg-b.iso-drive,format=raw,readonly=on,file=/isos/b.iso -device usb-storage,id=usbimg-b.iso,bus=xhci.0,drive=usbimg-b.iso-drive,removable=on",
        ] {
            assert!(joined.contains(want), "args lack {want:?}:\n{joined}");
        }
        // The controller the devices attach to comes first.
        assert!(
            joined.find("qemu-xhci") < joined.find("usb-storage"),
            "xhci controller must precede usb-storage devices:\n{joined}"
        );
    }

    /// A hand-edited entry without a usable `path:` loads with an empty path,
    /// as yaml.v3 did, so the VM stays listed and the entry can be detached;
    /// start then names it with Go's wording.
    #[test]
    fn entry_without_a_path_loads_and_fails_on_start() {
        let storage = tempfile::tempdir().unwrap();
        fs::create_dir_all(vm_dir(storage.path(), "u")).unwrap();
        fs::write(
            crate::vm::config::config_file_path(storage.path(), "u"),
            "name: u\ncpu: 1\nram: 512\ndisk_size: 5\nnetwork:\n  type: none\nusb_images:\n  - {}\n  - path: null\n  - pth: /isos/a.iso\n  -\n",
        )
        .unwrap();
        let cfg = crate::vm::config::load_config(storage.path(), "u").unwrap();
        assert_eq!(cfg.usb_images, vec![image(""); 4]);
        let names: Vec<String> = Manager::new(storage.path())
            .list()
            .unwrap()
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, ["u"]);

        let one = &cfg.usb_images[..1];
        assert_eq!(
            check_usb_images(one).unwrap_err().to_string(),
            "USB drive .: no image path\nDetach it in the ISO dialog (i), or put the file back."
        );
        let err = format!("{:#}", start(storage.path(), &cfg).unwrap_err());
        assert!(err.contains("USB drive .: no image path\n"), "{err}");
        assert_eq!(
            status(storage.path(), "u").unwrap().status,
            VmStatus::Stopped
        );
    }

    /// `start` fails up front, with the path, rather than launching a QEMU
    /// that dies on its discarded stderr.
    #[test]
    fn start_refuses_a_missing_image() {
        let storage = tempfile::tempdir().unwrap();
        let mut cfg = config("t");
        let gone = path_string(&storage.path().join("gone.iso"));
        cfg.usb_images = vec![image(&gone)];
        fs::create_dir_all(vm_dir(storage.path(), &cfg.name)).unwrap();
        let err = format!("{:#}", start(storage.path(), &cfg).unwrap_err());
        assert_eq!(
            err,
            format!("USB drive gone.iso: image not found: {gone}\nDetach it in the ISO dialog (i), or put the file back.")
        );
        let info = status(storage.path(), &cfg.name).unwrap();
        assert_eq!(
            info.status,
            VmStatus::Stopped,
            "a VM must not be started with a missing image: {info:?}"
        );
    }

    /// The drive ID of a `drive_add 0 "if=none,id=<drive>,..."` command.
    fn drive_id_of(drive_add: &str) -> &str {
        drive_add
            .strip_prefix("drive_add 0 \"if=none,id=")
            .and_then(|s| s.split(',').next())
            .unwrap_or_else(|| panic!("not a drive_add: {drive_add:?}"))
    }

    #[test]
    fn hotplug_drives_the_monitor() {
        let storage = tempfile::tempdir().unwrap();
        let isos = tempfile::tempdir().unwrap();
        fs::create_dir_all(vm_dir(storage.path(), "t")).unwrap();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let fail_drive = Arc::new(AtomicBool::new(false));
        let fail_device = Arc::new(AtomicBool::new(false));
        let _monitor = fake_hmp_sessions(&monitor_path(storage.path(), "t"), {
            let commands = Arc::clone(&commands);
            let (fail_drive, fail_device) = (Arc::clone(&fail_drive), Arc::clone(&fail_device));
            move |cmd| {
                commands.lock().unwrap().push(cmd.to_string());
                if cmd.starts_with("drive_add ") {
                    if fail_drive.load(Ordering::SeqCst) {
                        "Error: Duplicate ID\r\n".to_string()
                    } else {
                        "OK\r\n".to_string()
                    }
                } else if cmd.starts_with("device_add ") && fail_device.load(Ordering::SeqCst) {
                    "Error: Duplicate device ID 'usbimg-plug--me.iso'\r\n".to_string()
                } else {
                    String::new()
                }
            }
        });
        let sent = || std::mem::take(&mut *commands.lock().unwrap());

        // Comma: the option escaping must hold up inside the HMP quotes.
        let plug = write_image(isos.path(), "plug, me.iso", 16);
        let mut cfg = config("t");
        cfg.usb_images = vec![image(&plug)];

        usb_image_hotplug(storage.path(), &cfg, 0).unwrap();
        let cmds = sent();
        assert_eq!(cmds.len(), 2, "{cmds:?}");
        let drive_id = drive_id_of(&cmds[0]).to_string();
        let suffix = drive_id
            .strip_prefix("usbimg-plug--me.iso-drive-")
            .unwrap_or_else(|| panic!("{drive_id}"));
        assert!(
            !suffix.is_empty()
                && suffix
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
            "unique base-36 suffix, got {drive_id:?}"
        );
        assert_eq!(
            cmds[0],
            format!(
                "drive_add 0 \"if=none,id={drive_id},format=raw,readonly=on,file={}\"",
                plug.replace(',', ",,")
            )
        );
        assert_eq!(
            cmds[1],
            format!("device_add usb-storage,id=usbimg-plug--me.iso,bus=xhci.0,drive={drive_id},removable=on")
        );

        // A re-plug gets a fresh drive ID.
        usb_image_hotplug(storage.path(), &cfg, 0).unwrap();
        let again = sent();
        assert_ne!(drive_id_of(&again[0]), drive_id, "{again:?}");

        // drive_add answering anything but OK is QEMU's error, and no device
        // is added on top.
        fail_drive.store(true, Ordering::SeqCst);
        let err = usb_image_hotplug(storage.path(), &cfg, 0).unwrap_err();
        assert_eq!(err.to_string(), "QEMU: Error: Duplicate ID");
        assert_eq!(sent().len(), 1);
        fail_drive.store(false, Ordering::SeqCst);

        // A failed device_add rolls the drive back.
        fail_device.store(true, Ordering::SeqCst);
        let err = usb_image_hotplug(storage.path(), &cfg, 0).unwrap_err();
        assert_eq!(
            err.to_string(),
            "QEMU: Error: Duplicate device ID 'usbimg-plug--me.iso'"
        );
        let cmds = sent();
        assert_eq!(cmds.len(), 3, "{cmds:?}");
        assert_eq!(cmds[2], format!("drive_del {}", drive_id_of(&cmds[0])));
        fail_device.store(false, Ordering::SeqCst);

        // A missing file is caught before anything reaches the monitor.
        cfg.usb_images
            .push(image(&path_string(&isos.path().join("gone.iso"))));
        let err = usb_image_hotplug(storage.path(), &cfg, 1).unwrap_err();
        assert!(
            err.to_string().contains("not found"),
            "missing image should fail, got: {err}"
        );
        assert!(sent().is_empty());

        let err = usb_image_hotplug(storage.path(), &cfg, 2).unwrap_err();
        assert_eq!(err.to_string(), "no USB image at index 2");
        assert!(sent().is_empty());
    }

    #[test]
    fn hotunplug_names_the_device() {
        let storage = tempfile::tempdir().unwrap();
        fs::create_dir_all(vm_dir(storage.path(), "t")).unwrap();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let _monitor = fake_hmp_sessions(&monitor_path(storage.path(), "t"), {
            let commands = Arc::clone(&commands);
            move |cmd| {
                commands.lock().unwrap().push(cmd.to_string());
                if cmd == "device_del usbimg-a.iso" {
                    "Error: Device 'usbimg-a.iso' not found\r\n".to_string()
                } else {
                    String::new()
                }
            }
        });
        let mut cfg = config("t");
        // The pre-removal config: the second copy keeps its -2 suffix.
        cfg.usb_images = vec![image("/isos/a.iso"), image("/other/a.iso")];

        usb_image_hotunplug(storage.path(), &cfg, 1).unwrap();
        assert_eq!(*commands.lock().unwrap(), ["device_del usbimg-a.iso-2"]);

        let err = usb_image_hotunplug(storage.path(), &cfg, 0).unwrap_err();
        assert_eq!(
            err.to_string(),
            "QEMU: Error: Device 'usbimg-a.iso' not found"
        );

        let err = usb_image_hotunplug(storage.path(), &cfg, 2).unwrap_err();
        assert_eq!(err.to_string(), "no USB image at index 2");
        assert_eq!(commands.lock().unwrap().len(), 2);
    }

    /// Boots a real VM with one image attached at boot, then hot-plugs a
    /// second one, checks both show up as USB mass storage backed by the
    /// right (read-only) files, and unplugs them again.
    #[test]
    fn images_with_qemu() {
        for bin in ["qemu-system-x86_64", "qemu-img"] {
            if which::which(bin).is_err() {
                eprintln!("skipping: {bin} not installed");
                return;
            }
        }
        let isos = tempfile::tempdir().unwrap();
        let boot = write_image(isos.path(), "boot.iso", 1 << 20);
        // Comma: the option escaping must hold up.
        let plug = write_image(isos.path(), "plug, me.iso", 2 << 20);

        let storage = tempfile::tempdir().unwrap();
        let mut cfg = config("isotest");
        cfg.disk_size = 1;
        cfg.usb_images = vec![image(&boot)];
        Manager::new(storage.path()).create(&mut cfg).unwrap();
        start(storage.path(), &cfg).unwrap();
        let _stop = StopOnDrop {
            storage: storage.path(),
            name: &cfg.name,
        };

        wait_for_monitor(storage.path(), &cfg.name);
        let usb = wait_for_usb(storage.path(), &cfg.name, "usbimg-boot.iso", true);
        assert!(
            usb.contains("QEMU USB MSD"),
            "boot-time image is not a mass-storage device:\n{usb}"
        );
        let block = monitor_command(
            storage.path(),
            &cfg.name,
            "info block usbimg-boot.iso-drive",
        )
        .unwrap_or_default();
        assert!(
            block.contains(&boot) && block.contains("read-only"),
            "boot-time drive should be {boot} read-only:\n{block}"
        );

        // Hot-plug a second image, then unplug it: device and drive must both go.
        cfg.usb_images.push(image(&plug));
        usb_image_hotplug(storage.path(), &cfg, 1).expect("hot-plug");
        let id = usb_image_ids(&cfg.usb_images).swap_remove(1);
        wait_for_usb(storage.path(), &cfg.name, &id, true);
        let block = wait_for_block(storage.path(), &cfg.name, &plug, true);
        assert!(
            block.contains("read-only"),
            "hot-plugged drive should be read-only:\n{block}"
        );
        usb_image_hotunplug(storage.path(), &cfg, 1).expect("hot-unplug");
        wait_for_usb(storage.path(), &cfg.name, &id, false);
        wait_for_block(storage.path(), &cfg.name, &plug, false); // QEMU reaps the drive with the device

        // Plugging the same image again right away must work.
        usb_image_hotplug(storage.path(), &cfg, 1).expect("second hot-plug");
        wait_for_usb(storage.path(), &cfg.name, &id, true);
        wait_for_block(storage.path(), &cfg.name, &plug, true);

        // QEMU's own errors surface instead of being swallowed: a duplicate
        // ID. The drive added for the failed attempt is rolled back.
        let err = usb_image_hotplug(storage.path(), &cfg, 1).unwrap_err();
        assert!(
            err.to_string().contains(&id),
            "duplicate hot-plug should fail with QEMU's message, got: {err}"
        );
        let block = monitor_command(storage.path(), &cfg.name, "info block").unwrap_or_default();
        assert_eq!(
            block.matches(&plug).count(),
            1,
            "failed hot-plug left a drive behind:\n{block}"
        );
        // And a missing file is caught before anything reaches the monitor.
        cfg.usb_images
            .push(image(&path_string(&isos.path().join("gone.iso"))));
        let err = usb_image_hotplug(storage.path(), &cfg, 2).unwrap_err();
        assert!(
            err.to_string().contains("not found"),
            "missing image should fail, got: {err}"
        );

        stop(storage.path(), &cfg.name).unwrap();
    }
}
