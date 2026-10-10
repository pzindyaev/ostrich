//! The guest's IPv4 address: fixed for user networking, looked up by MAC in
//! dnsmasq leases and the ARP table for tap networking.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;

use super::config::{normalize_mac, parse_mac, NetworkType, VmConfig};
use super::process::USER_NET_GUEST_IP;

/// Where a dnsmasq serving the bridge may keep its leases.
pub(crate) const DNSMASQ_LEASE_GLOBS: [&str; 4] = [
    "/var/lib/misc/dnsmasq.leases",
    "/var/lib/dnsmasq/*.leases",
    "/var/lib/libvirt/dnsmasq/*.leases",
    // root-only by default
    "/var/lib/NetworkManager/dnsmasq-*.leases",
];

/// How the lease globs are expanded: like Go's `filepath.Glob`, a `*` does
/// not cross a `/`, and a leading dot is nothing special.
const GLOB_OPTIONS: glob::MatchOptions = glob::MatchOptions {
    case_sensitive: true,
    require_literal_separator: true,
    require_literal_leading_dot: false,
};

/// The guest's IPv4 address, or `None` if it is not (yet) known.
///
/// User networking always yields [`USER_NET_GUEST_IP`]. For tap networking
/// the address is assigned by whatever DHCP server sits on the bridge, so it
/// is looked up by the VM's MAC: first in local dnsmasq lease files, then in
/// the host's ARP table (which only has an entry once the host and guest
/// have exchanged traffic). Whether the VM runs is the caller's business.
pub fn guest_ip(cfg: &VmConfig) -> Option<String> {
    match cfg.network.kind {
        NetworkType::User => Some(USER_NET_GUEST_IP.to_string()),
        NetworkType::Tap => {
            let mac = normalize_mac(&cfg.network.mac)?;
            lease_ip(&mac).or_else(|| arp_ip(&mac))
        }
        NetworkType::None => None,
    }
}

/// Searches the dnsmasq lease files (`expiry mac ip hostname client-id`).
pub(crate) fn lease_ip(mac: &str) -> Option<String> {
    DNSMASQ_LEASE_GLOBS
        .iter()
        .filter_map(|pattern| glob::glob_with(pattern, GLOB_OPTIONS).ok())
        .flatten()
        .filter_map(Result::ok)
        .find_map(|path| scan_for_mac(&path, mac, 1, 2))
}

/// Searches `/proc/net/arp` (`IP HWtype Flags HWaddress Mask Device`).
pub(crate) fn arp_ip(mac: &str) -> Option<String> {
    scan_for_mac(Path::new("/proc/net/arp"), mac, 3, 0)
}

/// The `ip_col` field of the first line of `path` whose `mac_col` field is
/// the MAC (compared case-insensitively, in canonical form), when it parses
/// as an IPv4 address. A file that cannot be opened or read, such as a
/// root-only lease file, yields `None` like one without the MAC.
pub(crate) fn scan_for_mac(
    path: &Path,
    mac: &str,
    mac_col: usize,
    ip_col: usize,
) -> Option<String> {
    let want = parse_mac(mac)?;
    let file = File::open(path).ok()?;
    for line in BufReader::new(file).split(b'\n') {
        let line = line.ok()?;
        let line = String::from_utf8_lossy(&line);
        let Some((hw, ip)) = fields(&line, mac_col, ip_col) else {
            continue;
        };
        if parse_mac(hw) != Some(want) {
            continue;
        }
        if let Some(ip) = parse_ipv4(ip) {
            return Some(ip.to_string());
        }
    }
    None
}

/// The `a`-th and `b`-th whitespace-separated fields of `line`, when it has
/// both.
fn fields(line: &str, a: usize, b: usize) -> Option<(&str, &str)> {
    let (mut fa, mut fb) = (None, None);
    for (i, field) in line.split_whitespace().enumerate() {
        if i == a {
            fa = Some(field);
        }
        if i == b {
            fb = Some(field);
        }
        if fa.is_some() && fb.is_some() {
            break;
        }
    }
    Some((fa?, fb?))
}

/// `s` as an IPv4 address, taking an IPv4-mapped IPv6 address too, the way
/// Go's `net.ParseIP(s).To4()` does; any other IPv6 address is not one.
fn parse_ipv4(s: &str) -> Option<Ipv4Addr> {
    match s.parse::<IpAddr>().ok()? {
        IpAddr::V4(v4) => Some(v4),
        IpAddr::V6(v6) => v6.to_ipv4_mapped(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn scan_for_mac_reads_dnsmasq_leases() {
        let dir = tempfile::tempdir().unwrap();
        let leases = dir.path().join("dnsmasq.leases");
        fs::write(
            &leases,
            "1760000000 52:54:00:12:34:56 192.168.76.10 debian 01:52:54:00:12:34:56\n\
             1760000001 52:54:00:AB:CD:EF 192.168.76.11 * *\n\
             1760000002 52:54:00:00:00:03 fd00::3 v6only *\n\
             1760000003 52:54:00:00:00:04 ::ffff:192.168.76.14 mapped *\n\
             1760000004 52:54:00:12:34:56 192.168.76.99 stale *\n\
             duid 00:01:00:01:2b:aa:bb:cc:52:54:00:00:00:01\n",
        )
        .unwrap();
        // The first line with the MAC wins.
        assert_eq!(
            scan_for_mac(&leases, "52:54:00:12:34:56", 1, 2).as_deref(),
            Some("192.168.76.10")
        );
        // Case and notation do not matter, on either side.
        assert_eq!(
            scan_for_mac(&leases, "52-54-00-ab-cd-ef", 1, 2).as_deref(),
            Some("192.168.76.11")
        );
        assert_eq!(
            scan_for_mac(&leases, "5254.00AB.CDEF", 1, 2).as_deref(),
            Some("192.168.76.11")
        );
        // An IPv6 lease is no IPv4 address, but an IPv4-mapped one is.
        assert_eq!(scan_for_mac(&leases, "52:54:00:00:00:03", 1, 2), None);
        assert_eq!(
            scan_for_mac(&leases, "52:54:00:00:00:04", 1, 2).as_deref(),
            Some("192.168.76.14")
        );
        // Lines too short for the columns are skipped, not tripped over.
        assert_eq!(scan_for_mac(&leases, "00:01:00:01:2b:aa", 1, 2), None);
        assert_eq!(scan_for_mac(&leases, "52:54:00:99:99:99", 1, 2), None);
    }

    #[test]
    fn scan_for_mac_reads_the_arp_table() {
        let dir = tempfile::tempdir().unwrap();
        let arp = dir.path().join("arp");
        fs::write(
            &arp,
            "IP address       HW type     Flags       HW address            Mask     Device\n\
             192.168.76.10    0x1         0x2         52:54:00:12:34:56     *        br0\n\
             192.168.76.11    0x1         0x0         00:00:00:00:00:00     *        br0\n\
             10.0.0.1         0x1         0x2         AA:BB:CC:DD:EE:FF     *        eth0\n",
        )
        .unwrap();
        assert_eq!(
            scan_for_mac(&arp, "52:54:00:12:34:56", 3, 0).as_deref(),
            Some("192.168.76.10")
        );
        assert_eq!(
            scan_for_mac(&arp, "aa:bb:cc:dd:ee:ff", 3, 0).as_deref(),
            Some("10.0.0.1")
        );
        // The header's fourth field is "type", which is no MAC.
        assert_eq!(scan_for_mac(&arp, "52:54:00:00:00:01", 3, 0), None);
    }

    #[test]
    fn scan_for_mac_tolerates_what_it_cannot_use() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("leases");
        fs::write(&path, "1 52:54:00:12:34:56 192.168.76.10 x *\n").unwrap();
        // Columns past the end of every line.
        assert_eq!(scan_for_mac(&path, "52:54:00:12:34:56", 1, 7), None);
        assert_eq!(scan_for_mac(&path, "52:54:00:12:34:56", 9, 2), None);
        // A MAC that is not one.
        assert_eq!(scan_for_mac(&path, "nope", 1, 2), None);
        // A file that is not there, as a root-only lease file looks to a user.
        assert_eq!(
            scan_for_mac(&dir.path().join("missing"), "52:54:00:12:34:56", 1, 2),
            None
        );
        // Bytes that are not UTF-8 do not stop the scan.
        let mut data = b"1 52:54:00:00:00:09 192.168.76.9 h\xff\xfe *\n".to_vec();
        data.extend_from_slice(b"2 52:54:00:12:34:56 192.168.76.10 x *\n");
        fs::write(&path, data).unwrap();
        assert_eq!(
            scan_for_mac(&path, "52:54:00:12:34:56", 1, 2).as_deref(),
            Some("192.168.76.10")
        );
        assert_eq!(
            scan_for_mac(&path, "52:54:00:00:00:09", 1, 2).as_deref(),
            Some("192.168.76.9")
        );
    }

    #[test]
    fn guest_ip_by_network_type() {
        let mut cfg = VmConfig::default();
        cfg.network.kind = NetworkType::User;
        assert_eq!(guest_ip(&cfg).as_deref(), Some(USER_NET_GUEST_IP));
        cfg.network.kind = NetworkType::None;
        assert_eq!(guest_ip(&cfg), None);
        // A tap VM whose MAC is not one cannot be looked up.
        cfg.network.kind = NetworkType::Tap;
        cfg.network.mac = "nope".into();
        assert_eq!(guest_ip(&cfg), None);
    }

    #[test]
    fn lease_globs_are_the_go_ones_in_order() {
        assert_eq!(
            DNSMASQ_LEASE_GLOBS,
            [
                "/var/lib/misc/dnsmasq.leases",
                "/var/lib/dnsmasq/*.leases",
                "/var/lib/libvirt/dnsmasq/*.leases",
                "/var/lib/NetworkManager/dnsmasq-*.leases",
            ]
        );
    }
}
