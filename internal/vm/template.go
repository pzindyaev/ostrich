package vm

import (
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"time"

	"gopkg.in/yaml.v3"
)

// templatesDirName is the directory under the storage path that holds the
// templates. It starts with a dot so it can never clash with a VM: VM names
// may only contain letters, digits, hyphens and underscores.
const templatesDirName = ".templates"

// Template is the YAML schema stored in <storage>/.templates/<name>/template.yaml.
//
// A template is a stopped VM's disk, firmware NVRAM and TPM state frozen as a
// starting point for new VMs, plus the machine definition those files were
// made with. It deliberately carries only what defines the machine: the
// resource defaults a new VM starts from and the firmware the installed OS
// expects. Anything bound to one VM or to the host — MAC address, port
// forwards, VNC display, boot ISO, USB devices and images — is left out, as
// clones would clash over it or boot the installer again; so are additional
// disks, which hold one VM's data rather than the installed system.
type Template struct {
	Name        string       `yaml:"name"`
	Description string       `yaml:"description,omitempty"`
	SourceVM    string       `yaml:"source_vm"` // the VM it was made from (informational)
	CPU         int          `yaml:"cpu"`       // defaults for a new VM, changeable on creation
	RAM         int          `yaml:"ram"`       // MiB
	DiskSize    int          `yaml:"disk_size"` // GiB; the disk image is copied as is
	Arch        string       `yaml:"arch"`
	Firmware    FirmwareType `yaml:"firmware,omitempty"`
	SecureBoot  bool         `yaml:"secure_boot,omitempty"`
	TPM         bool         `yaml:"tpm,omitempty"`
	Network     NetworkType  `yaml:"network"`       // default for a new VM, changeable on creation
	VNC         bool         `yaml:"vnc,omitempty"` // the source had a VNC display; a new VM gets a free one
	CreatedAt   time.Time    `yaml:"created_at"`
}

// FirmwareLabel describes the template's boot platform, like VMConfig.FirmwareLabel.
func (t *Template) FirmwareLabel() string {
	return t.vmConfig().FirmwareLabel()
}

// UEFI reports whether VMs from the template boot UEFI firmware.
func (t *Template) UEFI() bool {
	return t.Firmware == FirmwareUEFI || t.SecureBoot
}

// vmConfig returns the machine definition as a VMConfig with no name.
func (t *Template) vmConfig() *VMConfig {
	return &VMConfig{
		CPU:        t.CPU,
		RAM:        t.RAM,
		DiskSize:   t.DiskSize,
		Arch:       t.Arch,
		Firmware:   t.Firmware,
		SecureBoot: t.SecureBoot,
		TPM:        t.TPM,
		Network:    NetworkConfig{Type: t.Network},
	}
}

// NewVMConfig returns the config a VM created from the template starts from:
// the template's machine definition and its resource and network defaults.
// The caller sets Name and may change CPU, RAM, Network and VNCPort before
// passing it to Manager.CreateFromTemplate.
func (t *Template) NewVMConfig() *VMConfig {
	return t.vmConfig()
}

// --- Path helpers ---

// TemplatesDir returns the directory holding all templates.
func TemplatesDir(storagePath string) string {
	return filepath.Join(storagePath, templatesDirName)
}

// TemplateDir returns the directory that holds all files for a named template.
func TemplateDir(storagePath, name string) string {
	return filepath.Join(TemplatesDir(storagePath), name)
}

// TemplateFilePath returns the template.yaml path.
func TemplateFilePath(storagePath, name string) string {
	return filepath.Join(TemplateDir(storagePath, name), "template.yaml")
}

// TemplateDiskPath returns the template's qcow2 disk image path.
func TemplateDiskPath(storagePath, name string) string {
	return filepath.Join(TemplateDir(storagePath, name), "disk.qcow2")
}

// TemplateFirmwareVarsPath returns the template's copy of the UEFI NVRAM.
func TemplateFirmwareVarsPath(storagePath, name string) string {
	return filepath.Join(TemplateDir(storagePath, name), "efivars.fd")
}

// TemplateTPMDir returns the template's copy of the emulated TPM's state.
func TemplateTPMDir(storagePath, name string) string {
	return filepath.Join(TemplateDir(storagePath, name), "tpm")
}

// --- YAML I/O ---

// LoadTemplate reads and parses template.yaml for the named template.
func LoadTemplate(storagePath, name string) (*Template, error) {
	data, err := os.ReadFile(TemplateFilePath(storagePath, name))
	if err != nil {
		return nil, err
	}
	var t Template
	if err := yaml.Unmarshal(data, &t); err != nil {
		return nil, err
	}
	return &t, nil
}

// SaveTemplate marshals and writes template.yaml.
func SaveTemplate(storagePath string, t *Template) error {
	data, err := yaml.Marshal(t)
	if err != nil {
		return err
	}
	return os.WriteFile(TemplateFilePath(storagePath, t.Name), data, 0644)
}

// TemplateDiskUsage returns the size of the template's disk image file on the
// host, or 0 when it cannot be read.
func TemplateDiskUsage(storagePath, name string) int64 {
	st, err := os.Stat(TemplateDiskPath(storagePath, name))
	if err != nil {
		return 0
	}
	return st.Size()
}

// --- Manager operations ---

// ListTemplates returns all valid templates, sorted by name.
func (m *Manager) ListTemplates() ([]*Template, error) {
	entries, err := os.ReadDir(TemplatesDir(m.StoragePath))
	if err != nil {
		if os.IsNotExist(err) {
			return nil, nil
		}
		return nil, err
	}
	var tpls []*Template
	for _, e := range entries {
		if !e.IsDir() {
			continue
		}
		t, err := LoadTemplate(m.StoragePath, e.Name())
		if err != nil {
			continue // skip malformed entries silently, like List does
		}
		tpls = append(tpls, t)
	}
	sort.Slice(tpls, func(i, j int) bool { return tpls[i].Name < tpls[j].Name })
	return tpls, nil
}

// TemplateExists reports whether a template directory and its file exist.
func (m *Manager) TemplateExists(name string) bool {
	_, err := os.Stat(TemplateFilePath(m.StoragePath, name))
	return err == nil
}

// CreateTemplate freezes the stopped VM vmName as the template tplName. The
// disk image, UEFI NVRAM and TPM state are copied, so the template is
// independent of the VM and the VM can go on being used or be deleted.
//
// The VM must be stopped: a disk copied under a running guest would be as
// inconsistent as one pulled from a machine mid-write. It is up to the user to
// shut the guest down from inside first; killing QEMU would leave the
// filesystem dirty just the same.
func (m *Manager) CreateTemplate(vmName, tplName, description string) error {
	cfg, err := LoadConfig(m.StoragePath, vmName)
	if err != nil {
		return fmt.Errorf("load VM config: %w", err)
	}
	if info, err := Status(m.StoragePath, vmName); err == nil && info.Status == StatusRunning {
		return fmt.Errorf("VM %q is running — shut it down from inside the guest first, so the disk is in a consistent state", vmName)
	}
	if m.TemplateExists(tplName) {
		return fmt.Errorf("a template named %q already exists", tplName)
	}
	if _, err := exec.LookPath(qemuImgBin); err != nil {
		return fmt.Errorf("%q not found in PATH — is QEMU installed?", qemuImgBin)
	}

	dir := TemplateDir(m.StoragePath, tplName)
	if err := os.MkdirAll(dir, 0755); err != nil {
		return fmt.Errorf("create template directory: %w", err)
	}
	// Nothing half-made is left behind: a failed copy takes the directory
	// with it, so the template list never shows a template with no disk.
	ok := false
	defer func() {
		if !ok {
			_ = os.RemoveAll(dir)
		}
	}()

	if err := copyDisk(DiskPath(m.StoragePath, vmName), TemplateDiskPath(m.StoragePath, tplName)); err != nil {
		return err
	}
	if cfg.UEFI() {
		if err := copyIfExists(FirmwareVarsPath(m.StoragePath, vmName), TemplateFirmwareVarsPath(m.StoragePath, tplName)); err != nil {
			return fmt.Errorf("copy UEFI NVRAM: %w", err)
		}
	}
	if cfg.TPM {
		if err := copyDirIfExists(TPMDir(m.StoragePath, vmName), TemplateTPMDir(m.StoragePath, tplName)); err != nil {
			return fmt.Errorf("copy TPM state: %w", err)
		}
	}

	t := &Template{
		Name:        tplName,
		Description: description,
		SourceVM:    vmName,
		CPU:         cfg.CPU,
		RAM:         cfg.RAM,
		DiskSize:    cfg.DiskSize,
		Arch:        archOf(cfg),
		Firmware:    cfg.Firmware,
		SecureBoot:  cfg.SecureBoot,
		TPM:         cfg.TPM,
		Network:     cfg.Network.Type,
		VNC:         cfg.VNCPort > 0,
		CreatedAt:   time.Now(),
	}
	if t.Firmware == "" {
		t.Firmware = FirmwareBIOS
	}
	if t.Network == "" {
		t.Network = NetworkUser
	}
	if err := SaveTemplate(m.StoragePath, t); err != nil {
		return fmt.Errorf("save template: %w", err)
	}
	ok = true
	return nil
}

// CreateFromTemplate makes a new VM from the template tplName. cfg carries the
// user's choices — Name, CPU, RAM, Network and VNCPort — and the template
// decides the rest: disk, architecture, firmware and TPM, which the installed
// OS depends on. The disk image is a copy, so the new VM is independent of
// the template. The MAC address is new unless cfg sets one.
func (m *Manager) CreateFromTemplate(tplName string, cfg *VMConfig) error {
	t, err := LoadTemplate(m.StoragePath, tplName)
	if err != nil {
		return fmt.Errorf("load template: %w", err)
	}
	if m.Exists(cfg.Name) {
		return fmt.Errorf("a VM named %q already exists", cfg.Name)
	}
	if _, err := exec.LookPath(qemuImgBin); err != nil {
		return fmt.Errorf("%q not found in PATH — is QEMU installed?", qemuImgBin)
	}

	cfg.DiskSize = t.DiskSize
	cfg.Disks = nil // the template carries the main disk only
	cfg.Arch = t.Arch
	cfg.Firmware = t.Firmware
	cfg.SecureBoot = t.SecureBoot
	cfg.TPM = t.TPM
	cfg.CreatedAt = time.Now()
	if cfg.Arch == "" {
		cfg.Arch = "x86_64"
	}
	if cfg.Firmware == "" {
		cfg.Firmware = FirmwareBIOS
	}
	if cfg.Network.Type == "" {
		cfg.Network.Type = t.Network
	}
	if cfg.Network.Type == "" {
		cfg.Network.Type = NetworkUser
	}
	if cfg.Network.MAC == "" {
		cfg.Network.MAC = randomMAC()
	}
	if cfg.TPM {
		if err := CheckTPM(); err != nil {
			return err
		}
	}

	vmDir := VMDir(m.StoragePath, cfg.Name)
	if err := os.MkdirAll(vmDir, 0755); err != nil {
		return fmt.Errorf("create VM directory: %w", err)
	}
	ok := false
	defer func() {
		if !ok {
			_ = os.RemoveAll(vmDir)
		}
	}()

	if err := copyDisk(TemplateDiskPath(m.StoragePath, tplName), DiskPath(m.StoragePath, cfg.Name)); err != nil {
		return err
	}
	// The NVRAM holds the boot entries the installed OS was registered with,
	// so the clone boots the way the source did. A template without one (made
	// by hand) gets a fresh store below.
	if cfg.UEFI() {
		if err := copyIfExists(TemplateFirmwareVarsPath(m.StoragePath, tplName), FirmwareVarsPath(m.StoragePath, cfg.Name)); err != nil {
			return fmt.Errorf("copy UEFI NVRAM: %w", err)
		}
		if err := EnsureFirmwareVars(m.StoragePath, cfg); err != nil {
			return err
		}
	}
	// The TPM state goes with the disk: a guest that sealed secrets to the
	// TPM (BitLocker, Windows Hello) finds them where it left them.
	if cfg.TPM {
		if err := copyDirIfExists(TemplateTPMDir(m.StoragePath, tplName), TPMDir(m.StoragePath, cfg.Name)); err != nil {
			return fmt.Errorf("copy TPM state: %w", err)
		}
	}

	if err := SaveConfig(m.StoragePath, cfg); err != nil {
		return fmt.Errorf("save VM config: %w", err)
	}
	ok = true
	return nil
}

// DeleteTemplate removes the template and its files. VMs made from it are
// full copies and are not affected.
func (m *Manager) DeleteTemplate(name string) error {
	if !m.TemplateExists(name) {
		return fmt.Errorf("no template named %q", name)
	}
	return os.RemoveAll(TemplateDir(m.StoragePath, name))
}

// FreeVNCDisplay returns the lowest VNC display number (1–99) no VM uses, or
// 0 when all are taken.
func (m *Manager) FreeVNCDisplay() int {
	cfgs, _ := m.List()
	used := map[int]bool{}
	for _, c := range cfgs {
		used[c.VNCPort] = true
	}
	for n := 1; n <= 99; n++ {
		if !used[n] {
			return n
		}
	}
	return 0
}

// --- file copying ---

// qemuImgBin creates, resizes and copies disk images.
const qemuImgBin = "qemu-img"

// copyDisk copies a qcow2 image by converting it to a fresh qcow2 file, the
// way qemu-img clones images: only allocated clusters are read and written,
// clusters that have been freed or zeroed are left out, and the result is
// a tidy image regardless of how fragmented the source had become. The
// virtual disk size is unchanged. The source must not be in use.
func copyDisk(src, dst string) error {
	if _, err := os.Stat(src); err != nil {
		return fmt.Errorf("disk image: %w", err)
	}
	out, err := exec.Command(qemuImgBin, "convert", "-f", "qcow2", "-O", "qcow2", src, dst).CombinedOutput()
	if err != nil {
		return fmt.Errorf("copy disk image: %w\n%s", err, out)
	}
	return nil
}

// copyIfExists copies src to dst when src is there; a missing src is not an error.
func copyIfExists(src, dst string) error {
	if _, err := os.Stat(src); err != nil {
		if os.IsNotExist(err) {
			return nil
		}
		return err
	}
	return copyFile(src, dst)
}

// copyDirIfExists copies the regular files under src into dst, recursively,
// creating dst. A missing src is not an error.
func copyDirIfExists(src, dst string) error {
	info, err := os.Stat(src)
	if err != nil {
		if os.IsNotExist(err) {
			return nil
		}
		return err
	}
	if !info.IsDir() {
		return errors.New(src + " is not a directory")
	}
	return filepath.Walk(src, func(path string, fi os.FileInfo, err error) error {
		if err != nil {
			return err
		}
		rel, err := filepath.Rel(src, path)
		if err != nil {
			return err
		}
		target := filepath.Join(dst, rel)
		if fi.IsDir() {
			return os.MkdirAll(target, fi.Mode().Perm()|0700)
		}
		if !fi.Mode().IsRegular() {
			return nil // sockets, pipes: runtime leftovers, never state
		}
		return copyFile(path, target)
	})
}
