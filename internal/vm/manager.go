package vm

import (
	"fmt"
	"math/rand"
	"os"
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
		if !e.IsDir() || e.Name() == templatesDirName {
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

// Create sets up the VM directory, creates the qcow2 disk images, and writes
// vm.yaml. If anything fails, a directory made here is removed again; one
// that was already there is left alone.
func (m *Manager) Create(cfg *VMConfig) error {
	if err := ValidateDisks(cfg.Disks); err != nil {
		return err
	}
	vmDir := VMDir(m.StoragePath, cfg.Name)
	_, statErr := os.Stat(vmDir)
	madeDir := os.IsNotExist(statErr)
	if err := os.MkdirAll(vmDir, 0755); err != nil {
		return fmt.Errorf("create VM directory: %w", err)
	}
	ok := false
	defer func() {
		if !ok && madeDir {
			_ = os.RemoveAll(vmDir)
		}
	}()

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
	if cfg.Firmware == "" {
		cfg.Firmware = FirmwareBIOS
	}

	// Firmware and TPM need host packages; find out now rather than at start.
	if err := EnsureFirmwareVars(m.StoragePath, cfg); err != nil {
		return err
	}
	if cfg.TPM {
		if err := CheckTPM(); err != nil {
			return err
		}
	}

	if err := createDiskImage(DiskPath(m.StoragePath, cfg.Name), cfg.DiskSize); err != nil {
		return fmt.Errorf("create disk image: %w", err)
	}
	for _, d := range cfg.Disks {
		if err := createDiskImage(ExtraDiskPath(m.StoragePath, cfg.Name, d.Name), d.Size); err != nil {
			return fmt.Errorf("create disk %q: %w", d.Name, err)
		}
	}

	if err := SaveConfig(m.StoragePath, cfg); err != nil {
		return fmt.Errorf("save VM config: %w", err)
	}
	ok = true
	return nil
}

// Update applies an edited config to the existing VM named oldName. It grows
// the disk images, creates and removes additional disks, renames the VM
// directory and sets up UEFI NVRAM as needed, then rewrites vm.yaml.
// Renaming, resizing or removing disks and firmware changes require the VM
// to be stopped; a disk can be added to a running VM (the caller hot-plugs
// it), and other changes take effect the next time it is started.
func (m *Manager) Update(oldName string, cfg *VMConfig) error {
	old, err := LoadConfig(m.StoragePath, oldName)
	if err != nil {
		return fmt.Errorf("load VM config: %w", err)
	}
	if err := ValidateDisks(cfg.Disks); err != nil {
		return err
	}
	disks, err := DiffDisks(old.Disks, cfg.Disks)
	if err != nil {
		return err
	}

	renamed := cfg.Name != oldName
	resized := cfg.DiskSize != old.DiskSize
	firmwareChanged := cfg.UEFI() != old.UEFI() || cfg.SecureBoot != old.SecureBoot
	disksLocked := len(disks.Grown) > 0 || len(disks.Removed) > 0

	if cfg.DiskSize < old.DiskSize {
		return fmt.Errorf("disk can only grow (currently %d GiB) — shrinking would destroy data", old.DiskSize)
	}
	if renamed && m.Exists(cfg.Name) {
		return fmt.Errorf("a VM named %q already exists", cfg.Name)
	}
	if renamed || resized || firmwareChanged || disksLocked {
		if info, err := Status(m.StoragePath, oldName); err == nil && info.Status == StatusRunning {
			if renamed || resized || firmwareChanged {
				return fmt.Errorf("stop the VM before changing its name, disk size or firmware")
			}
			return fmt.Errorf("stop the VM before resizing or removing disks (%s)", DiskNames(append(disks.Grown, disks.Removed...)))
		}
	}
	if cfg.Network.MAC == "" {
		cfg.Network.MAC = randomMAC()
	}
	if cfg.Firmware == "" {
		cfg.Firmware = FirmwareBIOS
	}
	if cfg.TPM && !old.TPM {
		if err := CheckTPM(); err != nil {
			return err
		}
	}

	// Disk images change first, under the old name. A new image that is
	// left behind by a later failure would block the next attempt, so the
	// ones made here go again on failure; nothing is on them yet.
	if resized {
		if err := resizeDiskImage(DiskPath(m.StoragePath, oldName), cfg.DiskSize); err != nil {
			return fmt.Errorf("resize disk image: %w", err)
		}
	}
	var created []string
	undo := func() {
		for _, p := range created {
			_ = os.Remove(p)
		}
	}
	for _, d := range disks.Added {
		path := ExtraDiskPath(m.StoragePath, oldName, d.Name)
		if err := createDiskImage(path, d.Size); err != nil {
			undo()
			return fmt.Errorf("create disk %q: %w", d.Name, err)
		}
		created = append(created, path)
	}
	for _, d := range disks.Grown {
		if err := resizeDiskImage(ExtraDiskPath(m.StoragePath, oldName, d.Name), d.Size); err != nil {
			undo()
			return fmt.Errorf("resize disk %q: %w", d.Name, err)
		}
	}
	for _, d := range disks.Removed {
		if err := os.Remove(ExtraDiskPath(m.StoragePath, oldName, d.Name)); err != nil && !os.IsNotExist(err) {
			undo()
			return fmt.Errorf("remove disk %q: %w", d.Name, err)
		}
	}
	if resized || disks.Any() {
		// Keep vm.yaml truthful about the images, whatever fails from here on.
		old.DiskSize = cfg.DiskSize
		old.Disks = cfg.Disks
		if err := SaveConfig(m.StoragePath, old); err != nil {
			return fmt.Errorf("save VM config: %w", err)
		}
	}

	if renamed {
		if err := os.Rename(VMDir(m.StoragePath, oldName), VMDir(m.StoragePath, cfg.Name)); err != nil {
			return fmt.Errorf("rename VM directory: %w", err)
		}
	}

	// Turning Secure Boot on needs a store with the keys enrolled, so the
	// NVRAM is rebuilt (boot entries are re-created by the firmware). Turning
	// it off or switching to BIOS keeps the store so the VM can switch back.
	if cfg.SecureBoot && !old.SecureBoot {
		if err := os.Remove(FirmwareVarsPath(m.StoragePath, cfg.Name)); err != nil && !os.IsNotExist(err) {
			return fmt.Errorf("reset NVRAM: %w", err)
		}
	}
	if err := EnsureFirmwareVars(m.StoragePath, cfg); err != nil {
		return err
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
