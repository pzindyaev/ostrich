package vm

import (
	"encoding/json"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path"
	"path/filepath"
	"sort"
)

// Firmware is a UEFI firmware usable with QEMU's pflash devices: a read-only
// code image shared by all VMs and a pristine NVRAM template that each VM
// gets its own writable copy of.
type Firmware struct {
	Description  string
	Code         string // executable image
	CodeFormat   string // "raw" or "qcow2"
	VarsTemplate string // NVRAM template, copied per VM
	VarsFormat   string
	SecureBoot   bool // the image can enforce Secure Boot
	EnrolledKeys bool // the template already carries PK, KEK and db
	RequiresSMM  bool // needs -machine smm=on and the pflash "secure" property
}

// firmwareDescriptor is the subset of QEMU's firmware descriptor schema
// (docs/interop/firmware.json) that selecting a firmware needs.
type firmwareDescriptor struct {
	Description    string   `json:"description"`
	InterfaceTypes []string `json:"interface-types"`
	Mapping        struct {
		Device        string       `json:"device"`
		Mode          string       `json:"mode"` // "split" (default), "combined" or "stateless"
		Executable    firmwareFile `json:"executable"`
		NVRAMTemplate firmwareFile `json:"nvram-template"`
	} `json:"mapping"`
	Targets []struct {
		Architecture string   `json:"architecture"`
		Machines     []string `json:"machines"` // globs over canonical machine names
	} `json:"targets"`
	Features []string `json:"features"`
}

type firmwareFile struct {
	Filename string `json:"filename"`
	Format   string `json:"format"`
}

// firmwareDirs lists the descriptor directories, highest priority first. It is
// a variable so tests can point it at fixtures.
var firmwareDirs = defaultFirmwareDirs

func defaultFirmwareDirs() []string {
	cfgHome := os.Getenv("XDG_CONFIG_HOME")
	if cfgHome == "" {
		home, _ := os.UserHomeDir()
		cfgHome = filepath.Join(home, ".config")
	}
	return []string{
		filepath.Join(cfgHome, "qemu", "firmware"),
		"/etc/qemu/firmware",
		"/usr/share/qemu/firmware",
		"/opt/homebrew/share/qemu/firmware", // Homebrew on Apple silicon
		"/usr/local/share/qemu/firmware",    // Homebrew on Intel, source installs
	}
}

// knownFirmware is the fallback for hosts without descriptors, in order of
// preference per architecture.
var knownFirmware = map[string][]Firmware{
	"x86_64": {
		// Fedora / RHEL
		{Code: "/usr/share/edk2/ovmf/OVMF_CODE.secboot.fd", VarsTemplate: "/usr/share/edk2/ovmf/OVMF_VARS.secboot.fd", SecureBoot: true, EnrolledKeys: true, RequiresSMM: true},
		{Code: "/usr/share/edk2/ovmf/OVMF_CODE.fd", VarsTemplate: "/usr/share/edk2/ovmf/OVMF_VARS.fd"},
		// Debian / Ubuntu
		{Code: "/usr/share/OVMF/OVMF_CODE_4M.secboot.fd", VarsTemplate: "/usr/share/OVMF/OVMF_VARS_4M.ms.fd", SecureBoot: true, EnrolledKeys: true, RequiresSMM: true},
		{Code: "/usr/share/OVMF/OVMF_CODE_4M.fd", VarsTemplate: "/usr/share/OVMF/OVMF_VARS_4M.fd"},
		// Arch
		{Code: "/usr/share/edk2/x64/OVMF_CODE.secboot.4m.fd", VarsTemplate: "/usr/share/edk2/x64/OVMF_VARS.4m.fd", SecureBoot: true, RequiresSMM: true},
		{Code: "/usr/share/edk2/x64/OVMF_CODE.4m.fd", VarsTemplate: "/usr/share/edk2/x64/OVMF_VARS.4m.fd"},
		// Images bundled with QEMU itself
		{Code: "/usr/share/qemu/edk2-x86_64-secure-code.fd", VarsTemplate: "/usr/share/qemu/edk2-i386-vars.fd", SecureBoot: true, RequiresSMM: true},
		{Code: "/usr/share/qemu/edk2-x86_64-code.fd", VarsTemplate: "/usr/share/qemu/edk2-i386-vars.fd"},
		{Code: "/opt/homebrew/share/qemu/edk2-x86_64-secure-code.fd", VarsTemplate: "/opt/homebrew/share/qemu/edk2-i386-vars.fd", SecureBoot: true, RequiresSMM: true},
		{Code: "/opt/homebrew/share/qemu/edk2-x86_64-code.fd", VarsTemplate: "/opt/homebrew/share/qemu/edk2-i386-vars.fd"},
	},
	"aarch64": {
		{Code: "/usr/share/AAVMF/AAVMF_CODE.ms.fd", VarsTemplate: "/usr/share/AAVMF/AAVMF_VARS.ms.fd", SecureBoot: true, EnrolledKeys: true},
		{Code: "/usr/share/AAVMF/AAVMF_CODE.fd", VarsTemplate: "/usr/share/AAVMF/AAVMF_VARS.fd"},
		{Code: "/usr/share/edk2/aarch64/QEMU_EFI-pflash.raw", VarsTemplate: "/usr/share/edk2/aarch64/vars-template-pflash.raw"},
		{Code: "/usr/share/edk2/aarch64/QEMU_EFI.fd", VarsTemplate: "/usr/share/edk2/aarch64/QEMU_VARS.fd"},
		{Code: "/usr/share/qemu/edk2-aarch64-code.fd", VarsTemplate: "/usr/share/qemu/edk2-arm-vars.fd"},
		{Code: "/opt/homebrew/share/qemu/edk2-aarch64-code.fd", VarsTemplate: "/opt/homebrew/share/qemu/edk2-arm-vars.fd"},
	},
}

// FindFirmware picks the UEFI firmware for an architecture and machine type,
// the way libvirt does: QEMU firmware descriptors first, then well-known
// paths. With secureBoot it only returns Secure Boot capable images and
// prefers a template with the keys already enrolled; without it, it prefers
// a plain image, which boots without SMM.
func FindFirmware(arch, machine string, secureBoot bool) (*Firmware, error) {
	var fallback *Firmware
	for _, d := range loadFirmwareDescriptors() {
		if !d.supports(arch, machine) {
			continue
		}
		fw := d.firmware()
		if !fw.exists() {
			continue
		}
		if secureBoot {
			if !fw.SecureBoot {
				continue
			}
			if fw.EnrolledKeys {
				return &fw, nil
			}
		} else if !fw.SecureBoot {
			return &fw, nil
		}
		if fallback == nil {
			fallback = &fw
		}
	}
	if fallback != nil {
		return fallback, nil
	}

	for _, fw := range knownFirmware[arch] {
		if secureBoot && !fw.SecureBoot {
			continue
		}
		if fw.exists() {
			fw.CodeFormat, fw.VarsFormat = "raw", "raw"
			fw.Description = filepath.Base(fw.Code)
			return &fw, nil
		}
	}

	what := "UEFI firmware"
	if secureBoot {
		what = "Secure Boot capable UEFI firmware"
	}
	return nil, fmt.Errorf("no %s found for %s/%s — install OVMF:\n"+
		"  sudo pacman -S edk2-ovmf    # Arch\n"+
		"  sudo apt install ovmf       # Debian/Ubuntu\n"+
		"  sudo dnf install edk2-ovmf  # Fedora", what, arch, machine)
}

// loadFirmwareDescriptors reads every descriptor following QEMU's rules: a file
// in a higher-priority directory hides one of the same name below it, an empty
// file hides without replacing, and the result is ordered by file name.
func loadFirmwareDescriptors() []firmwareDescriptor {
	byName := map[string]string{}
	dirs := firmwareDirs()
	for i := len(dirs) - 1; i >= 0; i-- { // lowest priority first, so later wins
		entries, err := os.ReadDir(dirs[i])
		if err != nil {
			continue
		}
		for _, e := range entries {
			if !e.IsDir() && filepath.Ext(e.Name()) == ".json" {
				byName[e.Name()] = filepath.Join(dirs[i], e.Name())
			}
		}
	}
	names := make([]string, 0, len(byName))
	for n := range byName {
		names = append(names, n)
	}
	sort.Strings(names)

	var descs []firmwareDescriptor
	for _, n := range names {
		data, err := os.ReadFile(byName[n])
		if err != nil || len(data) == 0 {
			continue
		}
		var d firmwareDescriptor
		if err := json.Unmarshal(data, &d); err != nil {
			continue
		}
		descs = append(descs, d)
	}
	return descs
}

// supports reports whether the descriptor is a split-pflash UEFI firmware for
// the architecture and machine type.
func (d firmwareDescriptor) supports(arch, machine string) bool {
	if !hasString(d.InterfaceTypes, "uefi") || d.Mapping.Device != "flash" {
		return false
	}
	if d.Mapping.Mode != "" && d.Mapping.Mode != "split" {
		return false
	}
	if d.Mapping.Executable.Filename == "" || d.Mapping.NVRAMTemplate.Filename == "" {
		return false
	}
	for _, t := range d.Targets {
		if t.Architecture != arch {
			continue
		}
		for _, glob := range t.Machines {
			for _, candidate := range machineNames(machine) {
				if ok, _ := path.Match(glob, candidate); ok {
					return true
				}
			}
		}
	}
	return false
}

// machineNames returns the names a descriptor glob may be written against:
// the type as given plus the canonical prefix its alias stands for, so that
// "pc-q35-*" matches "q35" without asking QEMU to expand the alias.
func machineNames(machine string) []string {
	switch machine {
	case "q35":
		return []string{machine, "pc-q35-"}
	case "pc":
		return []string{machine, "pc-i440fx-"}
	case "virt":
		return []string{machine, "virt-"}
	}
	return []string{machine}
}

func (d firmwareDescriptor) firmware() Firmware {
	fw := Firmware{
		Description:  d.Description,
		Code:         d.Mapping.Executable.Filename,
		CodeFormat:   d.Mapping.Executable.Format,
		VarsTemplate: d.Mapping.NVRAMTemplate.Filename,
		VarsFormat:   d.Mapping.NVRAMTemplate.Format,
		SecureBoot:   hasString(d.Features, "secure-boot"),
		EnrolledKeys: hasString(d.Features, "enrolled-keys"),
		RequiresSMM:  hasString(d.Features, "requires-smm"),
	}
	if fw.CodeFormat == "" {
		fw.CodeFormat = "raw"
	}
	if fw.VarsFormat == "" {
		fw.VarsFormat = "raw"
	}
	return fw
}

func (fw Firmware) exists() bool {
	_, errCode := os.Stat(fw.Code)
	_, errVars := os.Stat(fw.VarsTemplate)
	return errCode == nil && errVars == nil
}

func hasString(list []string, s string) bool {
	for _, x := range list {
		if x == s {
			return true
		}
	}
	return false
}

// --- per-VM NVRAM ---

// enrollTool enrolls Secure Boot keys into a vars template that lacks them.
// It comes with the virt-firmware package.
const enrollTool = "virt-fw-vars"

// EnsureFirmwareVars gives a UEFI VM its private NVRAM store if it does not
// have one yet. The store is a copy of the firmware's template; for Secure
// Boot it must carry Microsoft's keys, so a template without them is run
// through virt-fw-vars first. BIOS VMs need nothing.
func EnsureFirmwareVars(storagePath string, cfg *VMConfig) error {
	if !cfg.UEFI() {
		return nil
	}
	dst := FirmwareVarsPath(storagePath, cfg.Name)
	if _, err := os.Stat(dst); err == nil {
		return nil
	}
	fw, err := FindFirmware(archOf(cfg), machineOf(cfg), cfg.SecureBoot)
	if err != nil {
		return err
	}
	return createFirmwareVars(fw, dst, cfg.SecureBoot)
}

// createFirmwareVars writes a fresh NVRAM store at dst from the firmware's
// template, enrolling Secure Boot keys when asked for and not already present.
func createFirmwareVars(fw *Firmware, dst string, secureBoot bool) error {
	tmp := dst + ".tmp"
	defer os.Remove(tmp)

	if !secureBoot || fw.EnrolledKeys {
		if err := copyFile(fw.VarsTemplate, tmp); err != nil {
			return fmt.Errorf("copy NVRAM template: %w", err)
		}
		return os.Rename(tmp, dst)
	}

	tool, err := exec.LookPath(enrollTool)
	if err != nil {
		return fmt.Errorf("%s not found — it enrolls the Secure Boot keys into the firmware's NVRAM. Install it:\n"+
			"  sudo pacman -S virt-firmware     # Arch\n"+
			"  sudo dnf install virt-firmware   # Fedora\n"+
			"  pip install virt-firmware        # elsewhere\n"+
			"or pick UEFI without Secure Boot", enrollTool)
	}
	// A generated, throw-away platform key plus every Microsoft KEK and db
	// certificate (2011 and 2023 generations, UEFI CA and option ROM CA), so
	// both Windows and shim-based Linux boot. --secure-boot turns enforcement
	// on; without it OVMF would stay in setup mode.
	out, err := exec.Command(tool,
		"--input", fw.VarsTemplate, "--output", tmp,
		"--enroll-generate", "ostrich",
		"--microsoft-kek", "all", "--microsoft-db", "all",
		"--secure-boot",
	).CombinedOutput()
	if err != nil {
		return fmt.Errorf("enroll Secure Boot keys with %s: %w\n%s", enrollTool, err, out)
	}
	return os.Rename(tmp, dst)
}

func copyFile(src, dst string) error {
	in, err := os.Open(src)
	if err != nil {
		return err
	}
	defer in.Close()
	out, err := os.OpenFile(dst, os.O_WRONLY|os.O_CREATE|os.O_TRUNC, 0644)
	if err != nil {
		return err
	}
	if _, err := io.Copy(out, in); err != nil {
		out.Close()
		return err
	}
	return out.Close()
}

// archOf and machineOf resolve the QEMU target and machine type for a config.
func archOf(cfg *VMConfig) string {
	if cfg.Arch == "" {
		return "x86_64"
	}
	return cfg.Arch
}

func machineOf(cfg *VMConfig) string {
	if arch := archOf(cfg); arch == "aarch64" || arch == "arm64" {
		return "virt"
	}
	return "q35"
}
