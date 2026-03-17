package vm

import (
	"os"
	"path/filepath"
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
