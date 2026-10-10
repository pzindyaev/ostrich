//! What the host needs for tap networking, and the hint that says how to
//! set it up.

use std::fs;
use std::io::{self, BufRead, BufReader};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::Mutex;

use anyhow::Result;

/// The host bridge that tap networking attaches VMs to.
pub const BRIDGE_NAME: &str = "br0";

/// The private network the setup hint gives the bridge; the host takes .1
/// and NetworkManager's DHCP hands out the rest.
pub(crate) const BRIDGE_SUBNET: &str = "192.168.76";

/// Where the pieces tap networking needs are looked for. Tests point it at
/// a scratch directory.
#[derive(Debug, Clone)]
pub(crate) struct BridgeHost {
    /// Linux network interfaces; a bridge has a `bridge/` directory.
    pub sys_class_net: PathBuf,
    /// qemu-bridge-helper's ACL; the first one found counts.
    pub acl_paths: Vec<PathBuf>,
    /// qemu-bridge-helper itself.
    pub helper_paths: Vec<PathBuf>,
    /// ufw's config; `ENABLED=yes` means the guests need rules.
    pub ufw_conf: PathBuf,
    /// Whether a program is on PATH (`which`); tests replace it.
    pub look_path: fn(&str) -> bool,
}

impl Default for BridgeHost {
    fn default() -> Self {
        BridgeHost {
            sys_class_net: PathBuf::from("/sys/class/net"),
            acl_paths: vec![
                // Arch, Debian, Ubuntu, Fedora
                PathBuf::from("/etc/qemu/bridge.conf"),
                // RHEL and derivatives
                PathBuf::from("/etc/qemu-kvm/bridge.conf"),
                PathBuf::from("/usr/local/etc/qemu/bridge.conf"),
                PathBuf::from("/opt/homebrew/etc/qemu/bridge.conf"),
            ],
            helper_paths: vec![
                PathBuf::from("/usr/lib/qemu/qemu-bridge-helper"),
                PathBuf::from("/usr/libexec/qemu-bridge-helper"),
                PathBuf::from("/usr/lib64/qemu/qemu-bridge-helper"),
                PathBuf::from("/usr/local/libexec/qemu-bridge-helper"),
            ],
            ufw_conf: PathBuf::from("/etc/ufw/ufw.conf"),
            look_path: |name| which::which(name).is_ok(),
        }
    }
}

/// The host [`bridge_hint`] inspects: tests swap it out, the way the Go
/// tests swapped `defaultBridgeHost`.
#[cfg(test)]
pub(crate) static TEST_HOST: Mutex<Option<BridgeHost>> = Mutex::new(None);

/// The host [`bridge_hint`] inspects: the real one, unless a test swapped
/// it out.
fn default_host() -> BridgeHost {
    #[cfg(test)]
    {
        if let Some(h) = TEST_HOST.lock().ok().and_then(|g| g.clone()) {
            return h;
        }
    }
    BridgeHost::default()
}

/// What the host has, and lacks, for a VM to attach to the bridge: the
/// bridge itself, the helper that creates the tap, and the ACL that lets
/// the helper use the bridge.
#[derive(Debug, Clone, Default)]
pub(crate) struct BridgeStatus {
    pub name: String,
    /// The bridge device is there.
    pub exists: bool,
    /// An interface of that name is there but is no bridge.
    pub not_bridge: bool,
    /// No sysfs: nothing can be told about the device.
    pub unknown: bool,
    pub acl_path: PathBuf,
    /// No ACL file at all: the helper denies everything.
    pub acl_missing: bool,
    /// There is one but it cannot be read: assume it is fine.
    pub acl_unread: bool,
    pub allowed: bool,
    /// Empty when not found.
    pub helper: String,
    pub setuid: bool,
    /// NetworkManager's CLI is there: the hint uses it.
    pub nmcli: bool,
    /// ufw is enabled: the hint opens it for the guests.
    pub ufw: bool,
}

impl BridgeStatus {
    /// Whether nothing stands in the way, as far as can be told.
    pub fn ok(&self) -> bool {
        (self.exists || self.unknown)
            && !self.not_bridge
            && (self.allowed || self.acl_unread)
            && !self.helper.is_empty()
            && self.setuid
    }

    /// What is missing and the shell commands that put it in place, or `""`
    /// when nothing is. The commands are meant to be copied as they are:
    /// short lines, continuation backslashes rather than long ones.
    pub fn hint(&self) -> String {
        if self.ok() {
            return String::new();
        }
        let name = &self.name;
        let mut problems: Vec<String> = Vec::new();
        let mut cmds: Vec<String> = Vec::new();
        let mut notes: Vec<String> = Vec::new();

        if self.not_bridge {
            problems.push(format!("{name} is not a bridge"));
        } else if !self.exists && !self.unknown {
            problems.push(format!("the host has no bridge {name}"));
            if self.nmcli {
                cmds.extend([
                    format!("sudo nmcli con add type bridge ifname {name} con-name {name} \\"),
                    format!("    ipv4.method shared ipv4.addresses {BRIDGE_SUBNET}.1/24 \\"),
                    "    ipv6.method disabled bridge.stp no connection.autoconnect yes".to_string(),
                    format!("sudo nmcli con up {name}"),
                ]);
                notes.push(format!(
                    "This makes a NAT'd bridge that stays across reboots: guests get {BRIDGE_SUBNET}.10–254 by DHCP and reach the internet through the host."
                ));
            } else {
                cmds.extend([
                    format!("sudo ip link add {name} type bridge"),
                    format!("sudo ip addr add {BRIDGE_SUBNET}.1/24 dev {name}"),
                    format!("sudo ip link set {name} up"),
                ]);
                notes.push(format!(
                    "This bridge is gone after a reboot and has no DHCP or NAT: give guests static addresses in {BRIDGE_SUBNET}.0/24, or set it up in your network manager."
                ));
            }
            if self.ufw {
                cmds.extend([
                    format!("sudo ufw allow in on {name} to any port 67 proto udp"),
                    format!("sudo ufw allow in on {name} to any port 53"),
                    format!("sudo ufw route allow in on {name}"),
                ]);
                notes.push(
                    "The ufw rules let the guests use the host's DHCP and DNS and route out."
                        .to_string(),
                );
            }
        }

        if !self.allowed && !self.acl_unread {
            let acl = self.acl_path.display();
            if self.acl_missing {
                problems.push(format!("there is no {acl} allowing it"));
                cmds.push(format!("sudo install -d {}", dir_of(&self.acl_path)));
            } else {
                problems.push(format!("{acl} does not allow it"));
            }
            cmds.push(format!("echo 'allow {name}' | sudo tee -a {acl}"));
        }

        if self.helper.is_empty() {
            problems.push(
                "qemu-bridge-helper is not installed (it ships with QEMU: qemu-common on Arch and Fedora, qemu-system-common on Debian and Ubuntu)"
                    .to_string(),
            );
        } else if !self.setuid {
            problems.push(format!("{} is not setuid root", self.helper));
            cmds.push(format!("sudo chmod u+s {}", self.helper));
        }

        let mut b = format!(
            "tap networking is not set up on this host: {}.",
            problems.join("; ")
        );
        if !cmds.is_empty() {
            b.push_str("\nRun this, then start the VM:\n");
            for c in &cmds {
                b.push_str("\n  ");
                b.push_str(c);
            }
        }
        if !notes.is_empty() {
            b.push_str("\n\n");
            b.push_str(&notes.join(" "));
        }
        b.push_str(
            "\nFor a bridge onto a wired NIC, and to undo, see the README section \"Setting up br0\".",
        );
        b
    }
}

/// The directory part of `path`, the way Go's `filepath.Dir` gives it: `.`
/// for a bare file name, `/` at the root.
fn dir_of(path: &Path) -> String {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.display().to_string(),
        _ if path.has_root() => "/".to_string(),
        _ => ".".to_string(),
    }
}

/// Looks the bridge up on the host. Nothing is executed; everything is a
/// plain stat or read, apart from asking `look_path` about `nmcli`.
pub(crate) fn inspect_bridge(h: &BridgeHost, name: &str) -> BridgeStatus {
    let mut s = BridgeStatus {
        name: name.to_string(),
        acl_path: h.acl_paths.first().cloned().unwrap_or_default(),
        acl_missing: true,
        ..Default::default()
    };

    let dev = h.sys_class_net.join(name);
    if !h.sys_class_net.exists() {
        s.unknown = true;
    } else if dev.join("bridge").exists() {
        s.exists = true;
    } else if dev.exists() {
        s.not_bridge = true;
    }

    // Only the first ACL file that exists counts, even when it cannot be
    // read: that is how the helper picks it.
    if let Some(p) = h.acl_paths.iter().find(|p| p.exists()) {
        s.acl_path.clone_from(p);
        s.acl_missing = false;
        match bridge_allowed(p, name) {
            Ok(allowed) => s.allowed = allowed,
            Err(_) => s.acl_unread = true,
        }
    }

    if let Some((p, st)) = h
        .helper_paths
        .iter()
        .find_map(|p| fs::metadata(p).ok().map(|st| (p, st)))
    {
        s.helper = p.to_string_lossy().into_owned();
        s.setuid = st.mode() & 0o4000 != 0;
    }

    s.nmcli = (h.look_path)("nmcli");
    if let Ok(data) = fs::read(&h.ufw_conf) {
        s.ufw = String::from_utf8_lossy(&data)
            .split('\n')
            .any(|line| line.trim() == "ENABLED=yes");
    }
    s
}

/// Reads qemu-bridge-helper's ACL file and reports whether it lets the
/// helper attach to the bridge, the way the helper itself decides: some
/// `allow <bridge>` or `allow all` has to be there, and no `deny <bridge>`
/// or `deny all`. `include <file>` pulls in another file.
pub(crate) fn bridge_allowed(path: &Path, bridge: &str) -> std::io::Result<bool> {
    let (mut allowed, mut denied) = (false, false);
    walk_bridge_acl(path, bridge, &mut allowed, &mut denied, 0)?;
    Ok(allowed && !denied)
}

fn walk_bridge_acl(
    path: &Path,
    bridge: &str,
    allowed: &mut bool,
    denied: &mut bool,
    depth: u32,
) -> io::Result<()> {
    if depth > 8 {
        return Ok(());
    }
    let file = fs::File::open(path)?;
    for line in BufReader::new(file).split(b'\n') {
        let line = line?;
        // A `#` starts a comment; it is cut before anything is trimmed.
        let line = match line.iter().position(|&b| b == b'#') {
            Some(i) => &line[..i],
            None => &line[..],
        };
        let line = String::from_utf8_lossy(line);
        let line = line.trim();
        // The helper splits verb and argument at the first space only.
        let (verb, arg) = line.split_once(' ').unwrap_or((line, ""));
        let arg = arg.trim();
        match verb {
            "allow" if arg == "all" || arg == bridge => *allowed = true,
            "deny" if arg == "all" || arg == bridge => *denied = true,
            // The helper ignores errors here too.
            "include" => {
                let _ = walk_bridge_acl(Path::new(arg), bridge, allowed, denied, depth + 1);
            }
            _ => {}
        }
    }
    Ok(())
}

/// What keeps VMs from attaching to the bridge, with the shell commands that
/// fix it, or `""` when the bridge is ready as far as can be told. It is
/// what the forms show when tap networking is picked.
pub fn bridge_hint(name: &str) -> String {
    inspect_bridge(&default_host(), name).hint()
}

/// [`bridge_hint`] as an error, for the start of a tap VM: QEMU would die
/// with "bridge helper failed", which says less.
pub fn check_bridge(name: &str) -> Result<()> {
    let hint = bridge_hint(name);
    if hint.is_empty() {
        Ok(())
    } else {
        Err(anyhow::anyhow!("{hint}"))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::vm::config::{qemu_log_path, vm_dir, NetworkConfig, NetworkType, VmConfig};

    const SETUID: u32 = 0o4000;

    /// Writes `content` to `path`, making its directories, and then sets
    /// `mode` separately: writing as a non-root user strips the setuid bit.
    fn write_file(path: &Path, content: &str, mode: u32) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    /// Swaps the host [`bridge_hint`] inspects and puts the real one back
    /// when dropped.
    struct TestHost;

    impl TestHost {
        fn set(h: BridgeHost) -> Self {
            *TEST_HOST.lock().unwrap() = Some(h);
            TestHost
        }
    }

    impl Drop for TestHost {
        fn drop(&mut self) {
            *TEST_HOST.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }

    fn nmcli_installed(name: &str) -> bool {
        name == "nmcli"
    }

    fn nothing_installed(_name: &str) -> bool {
        false
    }

    /// Lays out a host under `dir`: a bridge when `bridge` is set, an ACL
    /// file with `acl` (none when empty), a helper with the given mode
    /// (none when 0), ufw on or off, nmcli there or not.
    fn fake_bridge_host(
        dir: &Path,
        bridge: &str,
        acl: &str,
        helper_mode: u32,
        ufw: bool,
        nmcli: bool,
    ) -> BridgeHost {
        let sys = dir.join("sys");
        fs::create_dir_all(sys.join("lo")).unwrap();
        if !bridge.is_empty() {
            fs::create_dir_all(sys.join(bridge).join("bridge")).unwrap();
        }
        let acl_path = dir.join("etc/qemu/bridge.conf");
        if !acl.is_empty() {
            write_file(&acl_path, acl, 0o644);
        }
        let helper = dir.join("lib/qemu-bridge-helper");
        if helper_mode != 0 {
            write_file(&helper, "#!/bin/sh\n", helper_mode);
        }
        let ufw_conf = dir.join("etc/ufw/ufw.conf");
        if ufw {
            write_file(&ufw_conf, "LOGLEVEL=low\nENABLED=yes\n", 0o644);
        }
        let look_path: fn(&str) -> bool = if nmcli {
            nmcli_installed
        } else {
            nothing_installed
        };
        BridgeHost {
            sys_class_net: sys,
            acl_paths: vec![acl_path],
            helper_paths: vec![helper],
            ufw_conf,
            look_path,
        }
    }

    fn assert_has(hint: &str, wants: &[&str]) {
        for want in wants {
            assert!(hint.contains(want), "hint lacks {want:?}:\n{hint}");
        }
    }

    fn assert_lacks(hint: &str, unwanted: &[&str]) {
        for s in unwanted {
            assert!(
                !hint.contains(s),
                "hint has {s:?}, which the host does not need:\n{hint}"
            );
        }
    }

    /// The ACL is read the way qemu-bridge-helper reads it.
    #[test]
    fn acl_is_read_like_the_helper() {
        let dir = tempfile::tempdir().unwrap();
        let extra = dir.path().join("extra.conf");
        write_file(&extra, "allow br1\n", 0o644);
        let extra = extra.display().to_string();
        let missing = dir.path().join("missing.conf").display().to_string();
        let cases = [
            ("allow br0\n".to_string(), true),
            ("allow all\n".to_string(), true),
            ("allow virbr0\n".to_string(), false),
            (String::new(), false),
            ("# allow br0\n".to_string(), false),
            ("allow br0 # ours\n".to_string(), true),
            ("allow br0\ndeny br0\n".to_string(), false),
            ("allow all\ndeny br0\n".to_string(), false),
            ("allow br0\ndeny all\n".to_string(), false),
            ("  allow   br0  \n".to_string(), true),
            (format!("include {extra}\n"), false),
            (format!("include {extra}\nallow br0\n"), true),
            (format!("include {missing}\nallow br0\n"), true),
        ];
        let path = dir.path().join("bridge.conf");
        for (conf, want) in &cases {
            write_file(&path, conf, 0o644);
            let got = bridge_allowed(&path, "br0").ok();
            assert_eq!(got, Some(*want), "bridge_allowed({conf:?})");
        }
        assert!(
            bridge_allowed(&dir.path().join("nope.conf"), "br0").is_err(),
            "a missing ACL file read without error"
        );
    }

    /// A ready host gets no hint.
    #[test]
    fn hint_ready() {
        let dir = tempfile::tempdir().unwrap();
        let h = fake_bridge_host(dir.path(), "br0", "allow br0\n", 0o755 | SETUID, true, true);
        let s = inspect_bridge(&h, "br0");
        assert!(
            s.ok() && s.hint().is_empty(),
            "ready host: ok={} hint={:?} ({s:?})",
            s.ok(),
            s.hint()
        );
    }

    /// A bare host gets the problems and the commands that fix each,
    /// tailored to what the host has.
    #[test]
    fn hint_nothing_there_network_manager_and_ufw() {
        let dir = tempfile::tempdir().unwrap();
        let h = fake_bridge_host(dir.path(), "", "allow virbr0\n", 0o755 | SETUID, true, true);
        let hint = inspect_bridge(&h, "br0").hint();
        let acl = h.acl_paths[0].display();
        assert_has(
            &hint,
            &[
                "the host has no bridge br0",
                "bridge.conf does not allow it",
                "sudo nmcli con add type bridge ifname br0 con-name br0 \\",
                "ipv4.method shared ipv4.addresses 192.168.76.1/24",
                "sudo nmcli con up br0",
                "sudo ufw allow in on br0 to any port 67 proto udp",
                "sudo ufw route allow in on br0",
                &format!("echo 'allow br0' | sudo tee -a {acl}"),
            ],
        );
        assert_lacks(&hint, &["ip link add", "setuid", "install -d"]);
    }

    #[test]
    fn hint_no_network_manager_no_ufw_no_acl_file() {
        let dir = tempfile::tempdir().unwrap();
        let h = fake_bridge_host(dir.path(), "", "", 0o755 | SETUID, false, false);
        let hint = inspect_bridge(&h, "br0").hint();
        let acl = h.acl_paths[0].display();
        assert_has(
            &hint,
            &[
                "sudo ip link add br0 type bridge",
                "sudo ip link set br0 up",
                &format!("there is no {acl} allowing it"),
                &format!(
                    "sudo install -d {}",
                    h.acl_paths[0].parent().unwrap().display()
                ),
                &format!("echo 'allow br0' | sudo tee -a {acl}"),
            ],
        );
        // (the temp paths may carry the test's name, so look for the commands)
        assert_lacks(&hint, &["sudo nmcli", "sudo ufw"]);
    }

    /// The ufw rules only accompany the creation of the bridge.
    #[test]
    fn hint_bridge_there_not_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let h = fake_bridge_host(
            dir.path(),
            "br0",
            "allow virbr0\n",
            0o755 | SETUID,
            true,
            true,
        );
        let hint = inspect_bridge(&h, "br0").hint();
        assert_lacks(&hint, &["no bridge", "sudo nmcli", "sudo ufw"]);
        assert_has(&hint, &["echo 'allow br0' | sudo tee -a"]);
    }

    #[test]
    fn hint_helper() {
        let dir = tempfile::tempdir().unwrap();
        let h = fake_bridge_host(dir.path(), "br0", "allow br0\n", 0o755, true, true);
        let hint = inspect_bridge(&h, "br0").hint();
        assert_has(
            &hint,
            &[
                "is not setuid root",
                &format!("sudo chmod u+s {}", h.helper_paths[0].display()),
            ],
        );

        let dir = tempfile::tempdir().unwrap();
        let h = fake_bridge_host(dir.path(), "br0", "allow br0\n", 0, true, true);
        let hint = inspect_bridge(&h, "br0").hint();
        assert_has(&hint, &["qemu-bridge-helper is not installed"]);
    }

    #[test]
    fn hint_interface_that_is_no_bridge() {
        let dir = tempfile::tempdir().unwrap();
        let h = fake_bridge_host(dir.path(), "", "allow br0\n", 0o755 | SETUID, false, true);
        fs::create_dir_all(h.sys_class_net.join("br0")).unwrap();
        let hint = inspect_bridge(&h, "br0").hint();
        assert_has(&hint, &["br0 is not a bridge"]);
        assert_lacks(&hint, &["sudo nmcli"]);
    }

    /// Without sysfs (macOS) and with an ACL only root may read, nothing
    /// can be told, and nothing is claimed.
    #[test]
    fn hint_unknowable() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = fake_bridge_host(
            dir.path(),
            "",
            "allow virbr0\n",
            0o755 | SETUID,
            false,
            true,
        );
        h.sys_class_net = dir.path().join("no-sysfs");
        let acl = &h.acl_paths[0];
        fs::set_permissions(acl, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::File::open(acl).is_ok() {
            eprintln!("skipping: root reads everything");
            return;
        }
        let s = inspect_bridge(&h, "br0");
        assert!(
            s.unknown && s.acl_unread && s.hint().is_empty(),
            "unknowable host: {s:?}\n{}",
            s.hint()
        );
        fs::set_permissions(acl, fs::Permissions::from_mode(0o644)).unwrap();
    }

    /// The hint is byte for byte what the Go version printed, for each
    /// shape of host it is tailored to.
    #[test]
    fn hint_text_matches_the_go_version() {
        let base = BridgeStatus {
            name: "br0".into(),
            acl_path: PathBuf::from("/etc/qemu/bridge.conf"),
            helper: "/usr/lib/qemu/qemu-bridge-helper".into(),
            setuid: true,
            ..Default::default()
        };

        // No bridge; nmcli and ufw there; an ACL that allows something else.
        let s = BridgeStatus {
            nmcli: true,
            ufw: true,
            ..base.clone()
        };
        assert_eq!(
            s.hint(),
            "tap networking is not set up on this host: the host has no bridge br0; /etc/qemu/bridge.conf does not allow it.\n\
             Run this, then start the VM:\n\
             \n\
             \x20 sudo nmcli con add type bridge ifname br0 con-name br0 \\\n\
             \x20     ipv4.method shared ipv4.addresses 192.168.76.1/24 \\\n\
             \x20     ipv6.method disabled bridge.stp no connection.autoconnect yes\n\
             \x20 sudo nmcli con up br0\n\
             \x20 sudo ufw allow in on br0 to any port 67 proto udp\n\
             \x20 sudo ufw allow in on br0 to any port 53\n\
             \x20 sudo ufw route allow in on br0\n\
             \x20 echo 'allow br0' | sudo tee -a /etc/qemu/bridge.conf\n\
             \n\
             This makes a NAT'd bridge that stays across reboots: guests get 192.168.76.10–254 by DHCP and reach the internet through the host. The ufw rules let the guests use the host's DHCP and DNS and route out.\n\
             For a bridge onto a wired NIC, and to undo, see the README section \"Setting up br0\"."
        );

        // No bridge; neither nmcli nor ufw; no ACL file at all.
        let s = BridgeStatus {
            acl_missing: true,
            ..base.clone()
        };
        assert_eq!(
            s.hint(),
            "tap networking is not set up on this host: the host has no bridge br0; there is no /etc/qemu/bridge.conf allowing it.\n\
             Run this, then start the VM:\n\
             \n\
             \x20 sudo ip link add br0 type bridge\n\
             \x20 sudo ip addr add 192.168.76.1/24 dev br0\n\
             \x20 sudo ip link set br0 up\n\
             \x20 sudo install -d /etc/qemu\n\
             \x20 echo 'allow br0' | sudo tee -a /etc/qemu/bridge.conf\n\
             \n\
             This bridge is gone after a reboot and has no DHCP or NAT: give guests static addresses in 192.168.76.0/24, or set it up in your network manager.\n\
             For a bridge onto a wired NIC, and to undo, see the README section \"Setting up br0\"."
        );

        // Only the helper is not setuid: commands, but no notes.
        let s = BridgeStatus {
            exists: true,
            allowed: true,
            setuid: false,
            ..base.clone()
        };
        assert_eq!(
            s.hint(),
            "tap networking is not set up on this host: /usr/lib/qemu/qemu-bridge-helper is not setuid root.\n\
             Run this, then start the VM:\n\
             \n\
             \x20 sudo chmod u+s /usr/lib/qemu/qemu-bridge-helper\n\
             For a bridge onto a wired NIC, and to undo, see the README section \"Setting up br0\"."
        );

        // Only the helper is missing: no commands at all.
        let s = BridgeStatus {
            exists: true,
            allowed: true,
            helper: String::new(),
            ..base
        };
        assert_eq!(
            s.hint(),
            "tap networking is not set up on this host: qemu-bridge-helper is not installed (it ships with QEMU: qemu-common on Arch and Fedora, qemu-system-common on Debian and Ubuntu).\n\
             For a bridge onto a wired NIC, and to undo, see the README section \"Setting up br0\"."
        );
    }

    /// The default host looks in the places the Go version did.
    #[test]
    fn default_host_paths() {
        let h = BridgeHost::default();
        assert_eq!(h.sys_class_net, PathBuf::from("/sys/class/net"));
        assert_eq!(
            h.acl_paths,
            [
                "/etc/qemu/bridge.conf",
                "/etc/qemu-kvm/bridge.conf",
                "/usr/local/etc/qemu/bridge.conf",
                "/opt/homebrew/etc/qemu/bridge.conf",
            ]
            .map(PathBuf::from)
        );
        assert_eq!(
            h.helper_paths,
            [
                "/usr/lib/qemu/qemu-bridge-helper",
                "/usr/libexec/qemu-bridge-helper",
                "/usr/lib64/qemu/qemu-bridge-helper",
                "/usr/local/libexec/qemu-bridge-helper",
            ]
            .map(PathBuf::from)
        );
        assert_eq!(h.ufw_conf, PathBuf::from("/etc/ufw/ufw.conf"));
        assert!((h.look_path)("sh"));
        assert!(!(h.look_path)("no-such-program-ostrich"));
    }

    /// A tap VM on a host without the bridge is refused before QEMU is
    /// launched, with the setup hint.
    #[test]
    fn start_checks_bridge() {
        let host_dir = tempfile::tempdir().unwrap();
        let _host = TestHost::set(fake_bridge_host(
            host_dir.path(),
            "",
            "allow virbr0\n",
            0o755 | SETUID,
            false,
            true,
        ));

        let storage = tempfile::tempdir().unwrap();
        let cfg = VmConfig {
            name: "tapvm".into(),
            cpu: 1,
            ram: 128,
            disk_size: 1,
            network: NetworkConfig {
                kind: NetworkType::Tap,
                mac: "52:54:00:00:00:02".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        fs::create_dir_all(vm_dir(storage.path(), &cfg.name)).unwrap();
        let err = match crate::vm::process::start(storage.path(), &cfg) {
            Ok(()) => {
                let _ = crate::vm::process::stop(storage.path(), &cfg.name);
                panic!("start = Ok without a bridge");
            }
            Err(err) => err.to_string(),
        };
        assert!(
            err.contains("the host has no bridge br0") && err.contains("sudo nmcli con up br0"),
            "start error lacks the hint: {err}"
        );
        assert!(
            !qemu_log_path(storage.path(), &cfg.name).exists(),
            "QEMU was launched despite the missing bridge"
        );
    }
}
