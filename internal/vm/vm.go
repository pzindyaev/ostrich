package vm

import (
	"fmt"
	"net"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"time"

	"gopkg.in/yaml.v3"
)

// NetworkType identifies the networking backend for a VM.
type NetworkType string

const (
	NetworkUser NetworkType = "user" // NAT/SLIRP
	NetworkTap  NetworkType = "tap"  // bridged tap
	NetworkNone NetworkType = "none"
)

// PortForward describes a single host→guest port mapping (user networking only).
type PortForward struct {
	Host  int    `yaml:"host"`
	Guest int    `yaml:"guest"`
	Proto string `yaml:"proto"` // "tcp" or "udp"
}

// ParsePortForwards parses a comma-separated list of "[proto:]host:guest"
// entries, e.g. "2222:22, udp:5353:53". Proto defaults to tcp.
func ParsePortForwards(s string) ([]PortForward, error) {
	var fwds []PortForward
	for _, entry := range strings.Split(s, ",") {
		entry = strings.TrimSpace(entry)
		if entry == "" {
			continue
		}
		parts := strings.Split(entry, ":")
		proto := "tcp"
		if len(parts) == 3 {
			proto = strings.ToLower(parts[0])
			parts = parts[1:]
		}
		if len(parts) != 2 || (proto != "tcp" && proto != "udp") {
			return nil, fmt.Errorf("invalid port forward %q — expected [tcp|udp:]host:guest", entry)
		}
		host, err1 := strconv.Atoi(parts[0])
		guest, err2 := strconv.Atoi(parts[1])
		if err1 != nil || err2 != nil || host < 1 || host > 65535 || guest < 1 || guest > 65535 {
			return nil, fmt.Errorf("invalid port forward %q — ports must be 1–65535", entry)
		}
		fwds = append(fwds, PortForward{Host: host, Guest: guest, Proto: proto})
	}
	return fwds, nil
}

// FormatPortForwards is the inverse of ParsePortForwards.
func FormatPortForwards(fwds []PortForward) string {
	var parts []string
	for _, pf := range fwds {
		proto := pf.Proto
		if proto == "" {
			proto = "tcp"
		}
		parts = append(parts, fmt.Sprintf("%s:%d:%d", proto, pf.Host, pf.Guest))
	}
	return strings.Join(parts, ", ")
}

// ValidateMAC checks that s is a 48-bit MAC address.
func ValidateMAC(s string) error {
	hw, err := net.ParseMAC(s)
	if err != nil || len(hw) != 6 {
		return fmt.Errorf("invalid MAC address %q — expected e.g. 52:54:00:12:34:56", s)
	}
	return nil
}

// NetworkConfig holds networking parameters for a VM.
type NetworkConfig struct {
	Type         NetworkType   `yaml:"type"`
	MAC          string        `yaml:"mac,omitempty"`
	PortForwards []PortForward `yaml:"port_forwards,omitempty"`
}

// VMConfig is the YAML schema stored in <vm-dir>/vm.yaml.
type VMConfig struct {
	Name      string        `yaml:"name"`
	CPU       int           `yaml:"cpu"`
	RAM       int           `yaml:"ram"`       // MiB
	DiskSize  int           `yaml:"disk_size"` // GiB
	Arch      string        `yaml:"arch"`      // e.g. "x86_64", "aarch64"
	CDROMPath string        `yaml:"cdrom_path,omitempty"`
	Network   NetworkConfig `yaml:"network"`
	VNCPort   int           `yaml:"vnc_port,omitempty"` // VNC display number (TCP port = 5900+n); 0 = disabled
	CreatedAt time.Time     `yaml:"created_at"`
}

// --- Path helpers ---

// VMDir returns the directory that holds all files for a named VM.
func VMDir(storagePath, name string) string {
	return filepath.Join(storagePath, name)
}

// DiskPath returns the qcow2 disk image path.
func DiskPath(storagePath, name string) string {
	return filepath.Join(VMDir(storagePath, name), "disk.qcow2")
}

// ConfigFilePath returns the vm.yaml path.
func ConfigFilePath(storagePath, name string) string {
	return filepath.Join(VMDir(storagePath, name), "vm.yaml")
}

// ConsolePath returns the serial console log path.
func ConsolePath(storagePath, name string) string {
	return filepath.Join(VMDir(storagePath, name), "console.log")
}

// PIDPath returns the QEMU PID file path.
func PIDPath(storagePath, name string) string {
	return filepath.Join(VMDir(storagePath, name), "qemu.pid")
}

// MonitorPath returns the QEMU monitor Unix socket path.
func MonitorPath(storagePath, name string) string {
	return filepath.Join(VMDir(storagePath, name), "qemu-monitor.sock")
}

// SerialSockPath returns the Unix socket path for the serial console.
func SerialSockPath(storagePath, name string) string {
	return filepath.Join(VMDir(storagePath, name), "serial.sock")
}

// VNCSockPath returns the Unix socket path used for VNC (unused when VNCPort > 0).
func VNCSockPath(storagePath, name string) string {
	return filepath.Join(VMDir(storagePath, name), "vnc.sock")
}

// --- YAML I/O ---

// LoadConfig reads and parses vm.yaml for the named VM.
func LoadConfig(storagePath, name string) (*VMConfig, error) {
	data, err := os.ReadFile(ConfigFilePath(storagePath, name))
	if err != nil {
		return nil, err
	}
	var cfg VMConfig
	if err := yaml.Unmarshal(data, &cfg); err != nil {
		return nil, err
	}
	return &cfg, nil
}

// SaveConfig marshals and writes vm.yaml.
func SaveConfig(storagePath string, cfg *VMConfig) error {
	data, err := yaml.Marshal(cfg)
	if err != nil {
		return err
	}
	return os.WriteFile(ConfigFilePath(storagePath, cfg.Name), data, 0644)
}
