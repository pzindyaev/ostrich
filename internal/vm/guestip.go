package vm

import (
	"bufio"
	"net"
	"os"
	"path/filepath"
	"strings"
)

// dnsmasqLeaseGlobs lists where a dnsmasq serving the bridge may keep its leases.
var dnsmasqLeaseGlobs = []string{
	"/var/lib/misc/dnsmasq.leases",
	"/var/lib/dnsmasq/*.leases",
	"/var/lib/libvirt/dnsmasq/*.leases",
	"/var/lib/NetworkManager/dnsmasq-*.leases", // root-only by default
}

// GuestIP returns the guest's IPv4 address, or "" if it is not (yet) known.
//
// User networking always yields UserNetGuestIP. For tap networking the address
// is assigned by whatever DHCP server sits on the bridge, so it is looked up by
// the VM's MAC: first in local dnsmasq lease files, then in the host's ARP
// table (which only has an entry once the host and guest have exchanged traffic).
func GuestIP(cfg *VMConfig) string {
	switch cfg.Network.Type {
	case NetworkUser:
		return UserNetGuestIP
	case NetworkTap:
		mac, err := net.ParseMAC(cfg.Network.MAC)
		if err != nil {
			return ""
		}
		if ip := leaseIP(mac); ip != "" {
			return ip
		}
		return arpIP(mac)
	}
	return ""
}

// leaseIP searches dnsmasq lease files ("expiry mac ip hostname client-id").
func leaseIP(mac net.HardwareAddr) string {
	for _, glob := range dnsmasqLeaseGlobs {
		paths, _ := filepath.Glob(glob)
		for _, p := range paths {
			if ip := scanForMAC(p, mac, 1, 2); ip != "" {
				return ip
			}
		}
	}
	return ""
}

// arpIP searches /proc/net/arp ("IP HWtype Flags HWaddress Mask Device").
func arpIP(mac net.HardwareAddr) string {
	return scanForMAC("/proc/net/arp", mac, 3, 0)
}

// scanForMAC returns the ipCol field of the first line whose macCol field equals mac.
func scanForMAC(path string, mac net.HardwareAddr, macCol, ipCol int) string {
	f, err := os.Open(path)
	if err != nil {
		return ""
	}
	defer f.Close()

	scanner := bufio.NewScanner(f)
	for scanner.Scan() {
		fields := strings.Fields(scanner.Text())
		if len(fields) <= macCol || len(fields) <= ipCol {
			continue
		}
		hw, err := net.ParseMAC(fields[macCol])
		if err != nil || hw.String() != mac.String() {
			continue
		}
		if ip := net.ParseIP(fields[ipCol]); ip != nil && ip.To4() != nil {
			return ip.String()
		}
	}
	return ""
}
