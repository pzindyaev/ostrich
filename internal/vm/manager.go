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

// Update applies an edited config to the existing VM named oldName. It grows
// the disk image and renames the VM directory as needed, then rewrites vm.yaml.
// Renaming and disk resizing require the VM to be stopped; other changes to a
// running VM take effect the next time it is started.
func (m *Manager) Update(oldName string, cfg *VMConfig) error {
	old, err := LoadConfig(m.StoragePath, oldName)
	if err != nil {
		return fmt.Errorf("load VM config: %w", err)
	}

	renamed := cfg.Name != oldName
	resized := cfg.DiskSize != old.DiskSize

	if cfg.DiskSize < old.DiskSize {
		return fmt.Errorf("disk can only grow (currently %d GiB) — shrinking would destroy data", old.DiskSize)
	}
	if renamed && m.Exists(cfg.Name) {
		return fmt.Errorf("a VM named %q already exists", cfg.Name)
	}
	if renamed || resized {
		if info, err := Status(m.StoragePath, oldName); err == nil && info.Status == StatusRunning {
			return fmt.Errorf("stop the VM before changing its name or disk size")
		}
	}
	if cfg.Network.MAC == "" {
		cfg.Network.MAC = randomMAC()
	}

	if resized {
		diskPath := DiskPath(m.StoragePath, oldName)
		out, err := exec.Command("qemu-img", "resize", diskPath, fmt.Sprintf("%dG", cfg.DiskSize)).CombinedOutput()
		if err != nil {
			return fmt.Errorf("resize disk image: %w\n%s", err, out)
		}
	}

	if renamed {
		if err := os.Rename(VMDir(m.StoragePath, oldName), VMDir(m.StoragePath, cfg.Name)); err != nil {
			// Keep vm.yaml truthful about the disk we may have just grown.
			old.DiskSize = cfg.DiskSize
			_ = SaveConfig(m.StoragePath, old)
			return fmt.Errorf("rename VM directory: %w", err)
		}
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
