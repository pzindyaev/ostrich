package vm

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"syscall"
)

// USBDevice is a host USB device passed through to the guest (vm.yaml schema).
//
// A device is matched by vendor/product ID, the way lsusb shows it. Port
// optionally pins the entry to one physical port — the sysfs device name, e.g.
// "3-2.2.2" (bus 3, port path 2.2.2) — which is what tells apart two identical
// devices plugged in at the same time.
type USBDevice struct {
	VendorID  string `yaml:"vendor_id"`      // 4 hex digits, e.g. "046d"
	ProductID string `yaml:"product_id"`     // 4 hex digits, e.g. "085c"
	Name      string `yaml:"name,omitempty"` // informational, captured when attached
	Port      string `yaml:"port,omitempty"` // "<bus>-<port path>", e.g. "3-2.2.2"
}

var (
	usbHexID  = regexp.MustCompile(`^[0-9a-fA-F]{4}$`)
	usbPortRe = regexp.MustCompile(`^[0-9]+-[0-9]+(\.[0-9]+)*$`)
)

// ParseUSBID parses "vvvv:pppp" — hex vendor and product IDs as printed by lsusb.
func ParseUSBID(s string) (USBDevice, error) {
	parts := strings.Split(strings.TrimSpace(s), ":")
	if len(parts) != 2 || !usbHexID.MatchString(parts[0]) || !usbHexID.MatchString(parts[1]) {
		return USBDevice{}, fmt.Errorf("invalid USB ID %q — expected vendor:product as 4 hex digits each, e.g. 046d:085c", s)
	}
	return USBDevice{VendorID: strings.ToLower(parts[0]), ProductID: strings.ToLower(parts[1])}, nil
}

// Validate checks the IDs and the optional port pin.
func (d USBDevice) Validate() error {
	if !usbHexID.MatchString(d.VendorID) || !usbHexID.MatchString(d.ProductID) {
		return fmt.Errorf("invalid USB device %q:%q — vendor_id and product_id must be 4 hex digits", d.VendorID, d.ProductID)
	}
	if d.Port != "" && !usbPortRe.MatchString(d.Port) {
		return fmt.Errorf("invalid USB port %q for %s — expected <bus>-<port path>, e.g. 3-2.2.2", d.Port, d.ID())
	}
	return nil
}

// ID returns "vvvv:pppp".
func (d USBDevice) ID() string {
	return strings.ToLower(d.VendorID + ":" + d.ProductID)
}

// Label returns the stored name, falling back to the ID.
func (d USBDevice) Label() string {
	if d.Name != "" {
		return d.Name
	}
	return d.ID()
}

// Matches reports whether the connected host device satisfies this entry.
func (d USBDevice) Matches(h HostUSBDevice) bool {
	return strings.EqualFold(d.VendorID, h.VendorID) &&
		strings.EqualFold(d.ProductID, h.ProductID) &&
		(d.Port == "" || d.Port == h.Port)
}

// QEMU object names. The controller is always present so devices can be
// hot-plugged into a running VM; usb-host devices attach to its bus.
const (
	usbControllerID = "xhci"
	usbBusName      = usbControllerID + ".0"
)

// USBDeviceIDs returns the QEMU device IDs for the configured devices. They are
// derived from the config rather than the position, so a device attached at
// boot can later be named for hot-unplug. Hand-edited duplicates get a numeric
// suffix so QEMU does not refuse to start on a duplicate ID.
func USBDeviceIDs(devs []USBDevice) []string {
	ids := make([]string, len(devs))
	seen := map[string]int{}
	for i, d := range devs {
		id := "usb-" + strings.ToLower(d.VendorID) + "-" + strings.ToLower(d.ProductID)
		if d.Port != "" {
			id += "-" + d.Port
		}
		seen[id]++
		if n := seen[id]; n > 1 {
			id = fmt.Sprintf("%s-%d", id, n)
		}
		ids[i] = id
	}
	return ids
}

// usbHostDevice returns the QEMU "usb-host,..." device spec, used verbatim both
// on the command line (-device) and for hot-plug (device_add).
func usbHostDevice(d USBDevice, id string) string {
	spec := fmt.Sprintf("usb-host,id=%s,bus=%s,vendorid=0x%s,productid=0x%s",
		id, usbBusName, strings.ToLower(d.VendorID), strings.ToLower(d.ProductID))
	if d.Port != "" {
		bus, path, _ := strings.Cut(d.Port, "-")
		spec += fmt.Sprintf(",hostbus=%s,hostport=%s", bus, path)
	}
	return spec
}

// --- Host enumeration ---

// HostUSBDevice is a USB device currently connected to the host.
type HostUSBDevice struct {
	VendorID     string
	ProductID    string
	Manufacturer string
	Product      string
	Bus          int
	Dev          int
	Port         string // sysfs name "<bus>-<port path>"; stable for a physical port
	DevNode      string // /dev/bus/usb/BBB/DDD — what QEMU (libusb) opens
	Writable     bool   // this user may open DevNode read-write
}

// ID returns "vvvv:pppp".
func (h HostUSBDevice) ID() string {
	return h.VendorID + ":" + h.ProductID
}

// Label returns "Manufacturer Product", avoiding a doubled manufacturer name,
// falling back to the ID when the device reports no strings.
func (h HostUSBDevice) Label() string {
	man, prod := strings.TrimSpace(h.Manufacturer), strings.TrimSpace(h.Product)
	switch {
	case man == "" && prod == "":
		return h.ID()
	case man == "":
		return prod
	case prod == "":
		return man
	case strings.HasPrefix(strings.ToLower(prod), strings.ToLower(man)):
		return prod
	}
	return man + " " + prod
}

// Sysfs and devfs roots. Variables so tests can point them at fixtures.
var (
	usbSysfsDir = "/sys/bus/usb/devices"
	usbDevDir   = "/dev/bus/usb"
)

// ListHostUSBDevices enumerates connected USB devices through sysfs (Linux),
// ordered by bus and port. Root hubs and hubs are left out: they stay with the
// host kernel and cannot be passed through.
func ListHostUSBDevices() ([]HostUSBDevice, error) {
	entries, err := os.ReadDir(usbSysfsDir)
	if err != nil {
		return nil, fmt.Errorf("list host USB devices (needs Linux sysfs): %w", err)
	}

	var devs []HostUSBDevice
	for _, e := range entries {
		name := e.Name()
		// Devices are "<bus>-<port path>"; skip root hubs ("usbN") and
		// interfaces ("<bus>-<port>:<config>.<iface>").
		if !usbPortRe.MatchString(name) {
			continue
		}
		dir := filepath.Join(usbSysfsDir, name)
		vendor, product := sysfsAttr(dir, "idVendor"), sysfsAttr(dir, "idProduct")
		if vendor == "" || product == "" {
			continue
		}
		if sysfsAttr(dir, "bDeviceClass") == "09" {
			continue // hub
		}
		bus, _ := strconv.Atoi(sysfsAttr(dir, "busnum"))
		devnum, _ := strconv.Atoi(sysfsAttr(dir, "devnum"))
		node := filepath.Join(usbDevDir, fmt.Sprintf("%03d", bus), fmt.Sprintf("%03d", devnum))
		devs = append(devs, HostUSBDevice{
			VendorID:     strings.ToLower(vendor),
			ProductID:    strings.ToLower(product),
			Manufacturer: sysfsAttr(dir, "manufacturer"),
			Product:      sysfsAttr(dir, "product"),
			Bus:          bus,
			Dev:          devnum,
			Port:         name,
			DevNode:      node,
			Writable:     writable(node),
		})
	}

	sort.Slice(devs, func(i, j int) bool { return portLess(devs[i].Port, devs[j].Port) })
	return devs, nil
}

func sysfsAttr(dir, attr string) string {
	b, err := os.ReadFile(filepath.Join(dir, attr))
	if err != nil {
		return ""
	}
	return strings.TrimSpace(string(b))
}

// writable reports whether this user may open the device node read-write. It
// asks the kernel (access(2)) so ACLs — how udev's uaccess tag grants access —
// are honoured, and nothing is opened.
func writable(node string) bool {
	const wOK = 0x2
	return syscall.Access(node, wOK) == nil
}

// portLess orders "<bus>-<a>.<b>..." names numerically, component by component.
func portLess(a, b string) bool {
	ka, kb := portKey(a), portKey(b)
	for i := 0; i < len(ka) && i < len(kb); i++ {
		if ka[i] != kb[i] {
			return ka[i] < kb[i]
		}
	}
	return len(ka) < len(kb)
}

func portKey(port string) []int {
	var key []int
	for _, f := range strings.FieldsFunc(port, func(r rune) bool { return r == '-' || r == '.' }) {
		n, _ := strconv.Atoi(f)
		key = append(key, n)
	}
	return key
}

// --- Config ↔ host matching ---

// USBState pairs a configured device with what the host currently shows for it.
type USBState struct {
	Device USBDevice
	Host   *HostUSBDevice // nil when the device is not connected
}

// USBStates resolves each configured device against the host. Where sysfs is
// unavailable every device reports as not connected.
func USBStates(devs []USBDevice) []USBState {
	host, _ := ListHostUSBDevices()
	return MatchUSB(devs, host)
}

// MatchUSB pairs config entries with connected host devices. Each host device
// is claimed by at most one entry — port-pinned entries first, so an unpinned
// entry for the same ID cannot steal their device. The returned Host pointers
// point into the host slice passed in.
func MatchUSB(devs []USBDevice, host []HostUSBDevice) []USBState {
	states := make([]USBState, len(devs))
	for i, d := range devs {
		states[i].Device = d
	}
	claimed := make([]bool, len(host))
	for _, pinnedPass := range []bool{true, false} {
		for i, d := range devs {
			if (d.Port != "") != pinnedPass || states[i].Host != nil {
				continue
			}
			for j := range host {
				if !claimed[j] && d.Matches(host[j]) {
					claimed[j] = true
					states[i].Host = &host[j]
					break
				}
			}
		}
	}
	return states
}

// CheckUSBAccess returns an error naming every configured device that is
// connected but not openable by this user, together with the command that
// fixes it. Without this check QEMU would start fine and silently never attach
// the device (its libusb errors only go to stderr).
func CheckUSBAccess(devs []USBDevice) error {
	host, err := ListHostUSBDevices()
	if err != nil {
		return nil // no sysfs — nothing to check
	}
	var denied []USBDevice
	var b strings.Builder
	for _, s := range MatchUSB(devs, host) {
		if s.Host != nil && !s.Host.Writable {
			denied = append(denied, s.Device)
			fmt.Fprintf(&b, "no write access to USB device %s (%s) at %s\n", s.Host.ID(), s.Host.Label(), s.Host.DevNode)
		}
	}
	if len(denied) == 0 {
		return nil
	}
	b.WriteString(UdevRuleHint(denied...))
	return errors.New(b.String())
}

// UdevRulesFile is where the generated udev rules go. It sorts before
// 73-seat-late.rules, which is what turns the uaccess tag into an ACL.
const UdevRulesFile = "/etc/udev/rules.d/70-ostrich-usb.rules"

// UdevRuleHint explains how to grant the logged-in user access to the devices.
func UdevRuleHint(devs ...USBDevice) string {
	return "Grant access to your user by running (then rescan):\n  " + UdevRuleCommand(devs...)
}

// UdevRuleCommand returns a one-line shell command that grants the logged-in
// user access to the devices: it appends a udev rule per device to
// UdevRulesFile, reloads udev and re-applies the rules to connected devices.
func UdevRuleCommand(devs ...USBDevice) string {
	return strings.Join(UdevRuleCommandWords(devs...), " ")
}

// UdevRuleCommandWords is UdevRuleCommand split into shell words, every one
// of them a complete argument or operator. A caller may break lines between
// any two words (with a backslash continuation) to fit a narrow screen and the
// pasted result still runs as one command in bash, zsh or fish.
//
// Each rule is written as four quoted words which echo/printf join with
// spaces; with several devices printf reuses its format once per device.
func UdevRuleCommandWords(devs ...USBDevice) []string {
	var words []string
	if len(devs) == 1 {
		words = []string{"echo"}
	} else {
		words = []string{"printf", `'%s %s %s %s\n'`}
	}
	for _, d := range devs {
		words = append(words,
			`'SUBSYSTEM=="usb",'`,
			fmt.Sprintf(`'ATTR{idVendor}=="%s",'`, strings.ToLower(d.VendorID)),
			fmt.Sprintf(`'ATTR{idProduct}=="%s",'`, strings.ToLower(d.ProductID)),
			`'TAG+="uaccess"'`,
		)
	}
	return append(words,
		"|", "sudo", "tee", "-a", UdevRulesFile,
		"&&", "sudo", "udevadm", "control", "--reload",
		"&&", "sudo", "udevadm", "trigger",
	)
}

// --- Hot-plug ---

// USBHotplug attaches cfg.USBDevices[idx] to the running VM through the QEMU
// monitor. A device that is not connected yet is picked up when plugged in.
func USBHotplug(storagePath string, cfg *VMConfig, idx int) error {
	dev := cfg.USBDevices[idx]
	if err := dev.Validate(); err != nil {
		return err
	}
	if err := CheckUSBAccess([]USBDevice{dev}); err != nil {
		return err
	}
	id := USBDeviceIDs(cfg.USBDevices)[idx]
	return monitorMustSucceed(storagePath, cfg.Name, "device_add "+usbHostDevice(dev, id))
}

// USBHotunplug detaches cfg.USBDevices[idx] from the running VM; cfg is the
// config as it was before the entry was removed, so the device ID matches.
func USBHotunplug(storagePath string, cfg *VMConfig, idx int) error {
	id := USBDeviceIDs(cfg.USBDevices)[idx]
	return monitorMustSucceed(storagePath, cfg.Name, "device_del "+id)
}

// monitorMustSucceed runs an HMP command that prints nothing on success and
// turns any output (QEMU's "Error: ..." lines) into an error.
func monitorMustSucceed(storagePath, name, command string) error {
	resp, err := MonitorCommand(storagePath, name, command)
	if err != nil {
		return err
	}
	if resp != "" {
		return fmt.Errorf("QEMU: %s", resp)
	}
	return nil
}
