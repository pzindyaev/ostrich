package vm

import (
	"fmt"
	"math/rand"
	"os"
	"os/exec"
	"sort"
	"time"
)

// Manager handles VM lifecycle operations rooted at StoragePath.
type Manager struct {
	StoragePath string
}

// NewManager creates a Manager for the given storage directory.
func NewManager(storagePath string) *Manager {
	return &Manager{StoragePath: storagePath}
}

// List returns all valid VM configs found under StoragePath, sorted by name.
func (m *Manager) List() ([]*VMConfig, error) {
	entries, err := os.ReadDir(m.StoragePath)
	if err != nil {
		if os.IsNotExist(err) {
			return nil, nil
		}
		return nil, err
	}

	var cfgs []*VMConfig
	for _, e := range entries {
		if !e.IsDir() {
			continue
		}
		cfg, err := LoadConfig(m.StoragePath, e.Name())
		if err != nil {
			continue // skip malformed entries silently
		}
		cfgs = append(cfgs, cfg)
	}

	sort.Slice(cfgs, func(i, j int) bool {
		return cfgs[i].Name < cfgs[j].Name
	})
	return cfgs, nil
}

// Create sets up the VM directory, creates the qcow2 disk, and writes vm.yaml.
func (m *Manager) Create(cfg *VMConfig) error {
	vmDir := VMDir(m.StoragePath, cfg.Name)
	if err := os.MkdirAll(vmDir, 0755); err != nil {
		return fmt.Errorf("create VM directory: %w", err)
	}

	if cfg.Network.MAC == "" {
		cfg.Network.MAC = randomMAC()
	}
	cfg.CreatedAt = time.Now()
	if cfg.Arch == "" {
		cfg.Arch = "x86_64"
	}
	if cfg.Network.Type == "" {
		cfg.Network.Type = NetworkUser
	}

	diskPath := DiskPath(m.StoragePath, cfg.Name)
	diskSizeStr := fmt.Sprintf("%dG", cfg.DiskSize)
	out, err := exec.Command("qemu-img", "create", "-f", "qcow2", diskPath, diskSizeStr).CombinedOutput()
	if err != nil {
		return fmt.Errorf("create disk image: %w\n%s", err, out)
	}

	if err := SaveConfig(m.StoragePath, cfg); err != nil {
		return fmt.Errorf("save VM config: %w", err)
	}
	return nil
}

// Delete stops the VM (if running) then removes its directory.
func (m *Manager) Delete(name string) error {
	_ = Stop(m.StoragePath, name) // best-effort stop
	return os.RemoveAll(VMDir(m.StoragePath, name))
}

// Exists reports whether a VM directory and config file exist.
func (m *Manager) Exists(name string) bool {
	_, err := os.Stat(ConfigFilePath(m.StoragePath, name))
	return err == nil
}

func randomMAC() string {
	r := rand.New(rand.NewSource(time.Now().UnixNano()))
	return fmt.Sprintf("52:54:00:%02x:%02x:%02x", r.Intn(256), r.Intn(256), r.Intn(256))
}
