package vm

import (
	"bufio"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
)

// BridgeName is the host bridge that tap networking attaches VMs to.
const BridgeName = "br0"

// bridgeSubnet is the private network the setup hint gives the bridge; the
// host takes .1 and NetworkManager's DHCP hands out the rest.
const bridgeSubnet = "192.168.76"

// bridgeHost is where the pieces tap networking needs are looked for. Tests
// point it at a scratch directory.
type bridgeHost struct {
	sysClassNet string   // Linux network interfaces; a bridge has a bridge/ directory
	aclPaths    []string // qemu-bridge-helper's ACL, first one found counts
	helperPaths []string // qemu-bridge-helper itself
	ufwConf     string   // ufw's config; ENABLED=yes means the guests need rules
	lookPath    func(string) (string, error)
}

var defaultBridgeHost = bridgeHost{
	sysClassNet: "/sys/class/net",
	aclPaths: []string{
		"/etc/qemu/bridge.conf",     // Arch, Debian, Ubuntu, Fedora
		"/etc/qemu-kvm/bridge.conf", // RHEL and derivatives
		"/usr/local/etc/qemu/bridge.conf",
		"/opt/homebrew/etc/qemu/bridge.conf",
	},
	helperPaths: []string{
		"/usr/lib/qemu/qemu-bridge-helper",
		"/usr/libexec/qemu-bridge-helper",
		"/usr/lib64/qemu/qemu-bridge-helper",
		"/usr/local/libexec/qemu-bridge-helper",
	},
	ufwConf:  "/etc/ufw/ufw.conf",
	lookPath: exec.LookPath,
}

// bridgeStatus is what the host has, and lacks, for a VM to attach to the
// bridge: the bridge itself, the helper that creates the tap, and the ACL
// that lets the helper use the bridge.
type bridgeStatus struct {
	name       string
	exists     bool // the bridge device is there
	notBridge  bool // an interface of that name is there but is no bridge
	unknown    bool // no sysfs: nothing can be told about the device
	aclPath    string
	aclMissing bool // no ACL file at all: the helper denies everything
	aclUnread  bool // there is one but it cannot be read: assume it is fine
	allowed    bool
	helper     string // "" when not found
	setuid     bool
	nmcli      bool // NetworkManager's CLI is there: the hint uses it
	ufw        bool // ufw is enabled: the hint opens it for the guests
}

// ok reports whether nothing stands in the way, as far as can be told.
func (s bridgeStatus) ok() bool {
	return (s.exists || s.unknown) && !s.notBridge && (s.allowed || s.aclUnread) && s.helper != "" && s.setuid
}

// inspectBridge looks the bridge up on the host.
func inspectBridge(h bridgeHost, name string) bridgeStatus {
	s := bridgeStatus{name: name}

	dev := filepath.Join(h.sysClassNet, name)
	switch _, err := os.Stat(h.sysClassNet); {
	case err != nil:
		s.unknown = true
	default:
		if _, err := os.Stat(filepath.Join(dev, "bridge")); err == nil {
			s.exists = true
		} else if _, err := os.Stat(dev); err == nil {
			s.notBridge = true
		}
	}

	s.aclPath = h.aclPaths[0]
	s.aclMissing = true
	for _, p := range h.aclPaths {
		if _, err := os.Stat(p); err != nil {
			continue
		}
		s.aclPath, s.aclMissing = p, false
		allowed, err := bridgeAllowed(p, name)
		if err != nil {
			s.aclUnread = true
		}
		s.allowed = allowed
		break
	}

	for _, p := range h.helperPaths {
		st, err := os.Stat(p)
		if err != nil {
			continue
		}
		s.helper = p
		s.setuid = st.Mode()&os.ModeSetuid != 0
		break
	}

	if h.lookPath != nil {
		_, err := h.lookPath("nmcli")
		s.nmcli = err == nil
	}
	if data, err := os.ReadFile(h.ufwConf); err == nil {
		for _, line := range strings.Split(string(data), "\n") {
			if strings.TrimSpace(line) == "ENABLED=yes" {
				s.ufw = true
			}
		}
	}
	return s
}

// bridgeAllowed reads qemu-bridge-helper's ACL file and reports whether it
// lets the helper attach to the bridge, the way the helper itself decides:
// some "allow <bridge>" or "allow all" has to be there, and no "deny <bridge>"
// or "deny all". "include <file>" pulls in another file.
func bridgeAllowed(path, bridge string) (bool, error) {
	var allowed, denied bool
	if err := walkBridgeACL(path, bridge, &allowed, &denied, 0); err != nil {
		return false, err
	}
	return allowed && !denied, nil
}

func walkBridgeACL(path, bridge string, allowed, denied *bool, depth int) error {
	if depth > 8 {
		return nil
	}
	f, err := os.Open(path)
	if err != nil {
		return err
	}
	defer f.Close()
	sc := bufio.NewScanner(f)
	for sc.Scan() {
		line := sc.Text()
		if i := strings.IndexByte(line, '#'); i >= 0 {
			line = line[:i]
		}
		line = strings.TrimSpace(line)
		verb, arg, _ := strings.Cut(line, " ")
		arg = strings.TrimSpace(arg)
		switch verb {
		case "allow":
			if arg == "all" || arg == bridge {
				*allowed = true
			}
		case "deny":
			if arg == "all" || arg == bridge {
				*denied = true
			}
		case "include":
			_ = walkBridgeACL(arg, bridge, allowed, denied, depth+1) // the helper ignores errors here too
		}
	}
	return sc.Err()
}

// hint explains what is missing and gives the shell commands that put it in
// place, or "" when nothing is. The commands are meant to be copied as they
// are: short lines, continuation backslashes rather than long ones.
func (s bridgeStatus) hint() string {
	if s.ok() {
		return ""
	}
	var problems, cmds, notes []string

	switch {
	case s.notBridge:
		problems = append(problems, fmt.Sprintf("%s is not a bridge", s.name))
	case !s.exists && !s.unknown:
		problems = append(problems, fmt.Sprintf("the host has no bridge %s", s.name))
		if s.nmcli {
			cmds = append(cmds,
				fmt.Sprintf("sudo nmcli con add type bridge ifname %s con-name %s \\", s.name, s.name),
				fmt.Sprintf("    ipv4.method shared ipv4.addresses %s.1/24 \\", bridgeSubnet),
				"    ipv6.method disabled bridge.stp no connection.autoconnect yes",
				fmt.Sprintf("sudo nmcli con up %s", s.name),
			)
			notes = append(notes, fmt.Sprintf("This makes a NAT'd bridge that stays across reboots: guests get %s.10–254 by DHCP and reach the internet through the host.", bridgeSubnet))
		} else {
			cmds = append(cmds,
				fmt.Sprintf("sudo ip link add %s type bridge", s.name),
				fmt.Sprintf("sudo ip addr add %s.1/24 dev %s", bridgeSubnet, s.name),
				fmt.Sprintf("sudo ip link set %s up", s.name),
			)
			notes = append(notes, fmt.Sprintf("This bridge is gone after a reboot and has no DHCP or NAT: give guests static addresses in %s.0/24, or set it up in your network manager.", bridgeSubnet))
		}
		if s.ufw {
			cmds = append(cmds,
				fmt.Sprintf("sudo ufw allow in on %s to any port 67 proto udp", s.name),
				fmt.Sprintf("sudo ufw allow in on %s to any port 53", s.name),
				fmt.Sprintf("sudo ufw route allow in on %s", s.name),
			)
			notes = append(notes, "The ufw rules let the guests use the host's DHCP and DNS and route out.")
		}
	}

	if !s.allowed && !s.aclUnread {
		if s.aclMissing {
			problems = append(problems, fmt.Sprintf("there is no %s allowing it", s.aclPath))
			cmds = append(cmds, fmt.Sprintf("sudo install -d %s", filepath.Dir(s.aclPath)))
		} else {
			problems = append(problems, fmt.Sprintf("%s does not allow it", s.aclPath))
		}
		cmds = append(cmds, fmt.Sprintf("echo 'allow %s' | sudo tee -a %s", s.name, s.aclPath))
	}

	switch {
	case s.helper == "":
		problems = append(problems, "qemu-bridge-helper is not installed (it ships with QEMU: qemu-common on Arch and Fedora, qemu-system-common on Debian and Ubuntu)")
	case !s.setuid:
		problems = append(problems, s.helper+" is not setuid root")
		cmds = append(cmds, "sudo chmod u+s "+s.helper)
	}

	var b strings.Builder
	fmt.Fprintf(&b, "tap networking is not set up on this host: %s.", strings.Join(problems, "; "))
	if len(cmds) > 0 {
		b.WriteString("\nRun this, then start the VM:\n")
		for _, c := range cmds {
			b.WriteString("\n  " + c)
		}
	}
	if len(notes) > 0 {
		b.WriteString("\n\n" + strings.Join(notes, " "))
	}
	b.WriteString("\nFor a bridge onto a wired NIC, and to undo, see the README section \"Setting up br0\".")
	return b.String()
}

// BridgeHint reports what keeps VMs from attaching to the bridge, with the
// shell commands that fix it, or "" when the bridge is ready as far as can
// be told. It is what the forms show when tap networking is picked.
func BridgeHint(name string) string {
	return inspectBridge(defaultBridgeHost, name).hint()
}

// CheckBridge is BridgeHint as an error, for the start of a tap VM: QEMU
// would die with "bridge helper failed", which says less.
func CheckBridge(name string) error {
	if hint := BridgeHint(name); hint != "" {
		return fmt.Errorf("%s", hint)
	}
	return nil
}
