package vm

import (
	"errors"
	"fmt"
	"os"
	"os/exec"
	"regexp"
	"strconv"
	"strings"
)

// Disk is an additional virtio disk of a VM (vm.yaml schema), next to the
// main disk that disk_size describes. Its image is <vm-dir>/<name>.qcow2; the
// name also serves as the virtio serial, so the guest finds the disk as
// /dev/disk/by-id/virtio-<name> no matter how it numbers its block devices.
type Disk struct {
	Name string `yaml:"name"`
	Size int    `yaml:"size"` // GiB
}

const (
	// MaxExtraDisks is how many additional disks a VM can have: one PCIe root
	// port each, always present so a disk can be hot-plugged into a running VM.
	MaxExtraDisks = 8
	// primaryDiskName is the file stem of the main disk (disk.qcow2).
	primaryDiskName = "disk"
	// diskPortAddrBase is the pcie.0 slot of the first root port. The ports are
	// pinned high so the devices QEMU slots by itself (xHCI, NIC, the main
	// disk) keep the addresses they had before the ports existed: OVMF boot
	// entries name the disk by its PCI address.
	diskPortAddrBase = 0x10
	// diskNameMaxLen is the virtio-blk serial limit; QEMU truncates silently.
	diskNameMaxLen = 20
)

var diskNameRe = regexp.MustCompile(`^[A-Za-z][A-Za-z0-9_-]{0,19}$`)

// Validate checks the disk's name and size.
func (d Disk) Validate() error {
	if !diskNameRe.MatchString(d.Name) {
		return fmt.Errorf("disk name %q: letters, digits, hyphens and underscores, starting with a letter, at most %d characters", d.Name, diskNameMaxLen)
	}
	if strings.EqualFold(d.Name, primaryDiskName) {
		return fmt.Errorf("disk name %q is taken by the main disk", d.Name)
	}
	if d.Size < 1 {
		return fmt.Errorf("disk %q: size must be at least 1 GiB", d.Name)
	}
	return nil
}

// ValidateDisks checks every disk and that the names are unique, ignoring
// case so two images cannot clash on a case-insensitive filesystem.
func ValidateDisks(disks []Disk) error {
	if len(disks) > MaxExtraDisks {
		return fmt.Errorf("at most %d additional disks", MaxExtraDisks)
	}
	seen := map[string]bool{}
	for _, d := range disks {
		if err := d.Validate(); err != nil {
			return err
		}
		key := strings.ToLower(d.Name)
		if seen[key] {
			return fmt.Errorf("duplicate disk name %q", d.Name)
		}
		seen[key] = true
	}
	return nil
}

// ParseDisks parses a comma-separated list of "[name:]size" entries, sizes in
// GiB, e.g. "data:50, 100". An entry without a name gets the lowest free
// "disk<n>", n from 1, so "100" above becomes disk1.
func ParseDisks(s string) ([]Disk, error) {
	var disks []Disk
	var unnamed []int
	for _, entry := range strings.Split(s, ",") {
		entry = strings.TrimSpace(entry)
		if entry == "" {
			continue
		}
		name, sizeStr, named := strings.Cut(entry, ":")
		if !named {
			name, sizeStr = "", name
		}
		name, sizeStr = strings.TrimSpace(name), strings.TrimSpace(sizeStr)
		size, err := strconv.Atoi(sizeStr)
		if err != nil || (named && name == "") {
			return nil, fmt.Errorf("invalid disk %q — expected [name:]size in GiB, e.g. data:50", entry)
		}
		if size < 1 {
			return nil, fmt.Errorf("invalid disk %q — size must be at least 1 GiB", entry)
		}
		if !named {
			unnamed = append(unnamed, len(disks))
		}
		disks = append(disks, Disk{Name: name, Size: size})
	}

	taken := map[string]bool{primaryDiskName: true}
	for _, d := range disks {
		taken[strings.ToLower(d.Name)] = true
	}
	n := 1
	for _, i := range unnamed {
		for taken["disk"+strconv.Itoa(n)] {
			n++
		}
		disks[i].Name = "disk" + strconv.Itoa(n)
		taken[disks[i].Name] = true
	}
	if err := ValidateDisks(disks); err != nil {
		return nil, err
	}
	return disks, nil
}

// FormatDisks is the inverse of ParseDisks: "data:50, disk1:100".
func FormatDisks(disks []Disk) string {
	parts := make([]string, len(disks))
	for i, d := range disks {
		parts[i] = fmt.Sprintf("%s:%d", d.Name, d.Size)
	}
	return strings.Join(parts, ", ")
}

// DiskChange is what an edit does to a VM's additional disks.
type DiskChange struct {
	Added   []Disk
	Grown   []Disk // with the new size
	Removed []Disk
}

// Any reports whether the change touches any disk.
func (c DiskChange) Any() bool {
	return len(c.Added)+len(c.Grown)+len(c.Removed) > 0
}

// DiffDisks compares the configured disks before and after an edit, matched
// by name. A disk that got smaller is an error: the image cannot shrink
// without destroying data.
func DiffDisks(old, cur []Disk) (DiskChange, error) {
	var ch DiskChange
	oldByName := make(map[string]Disk, len(old))
	for _, d := range old {
		oldByName[d.Name] = d
	}
	kept := map[string]bool{}
	for _, d := range cur {
		o, ok := oldByName[d.Name]
		switch {
		case !ok:
			ch.Added = append(ch.Added, d)
		case d.Size < o.Size:
			return DiskChange{}, fmt.Errorf("disk %q can only grow (currently %d GiB) — shrinking would destroy data", d.Name, o.Size)
		case d.Size > o.Size:
			ch.Grown = append(ch.Grown, d)
		}
		kept[d.Name] = true
	}
	for _, d := range old {
		if !kept[d.Name] {
			ch.Removed = append(ch.Removed, d)
		}
	}
	return ch, nil
}

// DiskNames lists the disks' names, comma-separated.
func DiskNames(disks []Disk) string {
	names := make([]string, len(disks))
	for i, d := range disks {
		names[i] = d.Name
	}
	return strings.Join(names, ", ")
}

// --- images ---

// createDiskImage makes a new qcow2 image of the given virtual size. It
// refuses to touch an existing file: qemu-img create would silently replace
// it, and a leftover image may hold data.
func createDiskImage(path string, sizeGiB int) error {
	if _, err := os.Stat(path); err == nil {
		return fmt.Errorf("%s already exists", path)
	}
	out, err := exec.Command(qemuImgBin, "create", "-f", "qcow2", path, fmt.Sprintf("%dG", sizeGiB)).CombinedOutput()
	if err != nil {
		return fmt.Errorf("qemu-img create: %w\n%s", err, out)
	}
	return nil
}

// resizeDiskImage grows an image to the given virtual size. The guest still
// has to extend its own partitions and filesystem.
func resizeDiskImage(path string, sizeGiB int) error {
	out, err := exec.Command(qemuImgBin, "resize", path, fmt.Sprintf("%dG", sizeGiB)).CombinedOutput()
	if err != nil {
		return fmt.Errorf("qemu-img resize: %w\n%s", err, out)
	}
	return nil
}

// CheckExtraDisks validates the configured disks and names each one whose
// image is missing, which would stop QEMU from starting.
func CheckExtraDisks(storagePath string, cfg *VMConfig) error {
	if err := ValidateDisks(cfg.Disks); err != nil {
		return err
	}
	var msg string
	for _, d := range cfg.Disks {
		if err := checkImage(ExtraDiskPath(storagePath, cfg.Name, d.Name)); err != nil {
			msg += fmt.Sprintf("disk %s: %v\n", d.Name, err)
		}
	}
	if msg == "" {
		return nil
	}
	return errors.New(msg + "Remove it in the edit form, or put the file back.")
}

// DiskStates checks each additional disk's image on the host.
func DiskStates(storagePath string, cfg *VMConfig) []ImageState {
	states := make([]ImageState, len(cfg.Disks))
	for i, d := range cfg.Disks {
		states[i] = ImageStateOf(ExtraDiskPath(storagePath, cfg.Name, d.Name))
	}
	return states
}

// --- QEMU ---

// diskPortID names the PCIe root port that holds the disk at index i.
func diskPortID(i int) string {
	return fmt.Sprintf("disk-rp%d", i+1)
}

// diskDeviceID names the virtio-blk device of a disk.
func diskDeviceID(name string) string {
	return "disk-" + name
}

// diskDriveID names the block backend behind a disk's device. There is no
// hot-unplug for disks, so unlike USB images the ID needs no unique suffix.
func diskDriveID(name string) string {
	return diskDeviceID(name) + "-drive"
}

// diskPortArgs returns the root ports every VM gets, MaxExtraDisks of them,
// pinned to the same pcie.0 slots whether or not disks are configured. The
// root bus itself does not take hot-plugged devices; a port does, and holds
// one. Chassis numbers must differ between ports.
func diskPortArgs() []string {
	args := make([]string, 0, 2*MaxExtraDisks)
	for i := 0; i < MaxExtraDisks; i++ {
		args = append(args, "-device",
			fmt.Sprintf("pcie-root-port,id=%s,bus=pcie.0,chassis=%d,addr=0x%x", diskPortID(i), i+1, diskPortAddrBase+i))
	}
	return args
}

// diskDrive returns the drive options for a disk image, used verbatim on the
// command line (-drive) and for hot-plug (drive_add).
func diskDrive(path, driveID string) string {
	return fmt.Sprintf("if=none,id=%s,format=qcow2,file=%s", driveID, qemuOptEscape(path))
}

// diskDevice returns the "virtio-blk-pci,..." device spec for the disk at
// index idx, used both on the command line (-device) and for hot-plug
// (device_add). The serial lets the guest tell the disks apart by name.
func diskDevice(name string, idx int) string {
	return fmt.Sprintf("virtio-blk-pci,id=%s,drive=%s,bus=%s,serial=%s", diskDeviceID(name), diskDriveID(name), diskPortID(idx), name)
}

// extraDiskArgs returns the root ports followed by a drive and device per
// configured disk. Disk i sits on port i, so a disk appended to the config
// while the VM runs can be hot-plugged onto the port its index names.
func extraDiskArgs(cfg *VMConfig, storagePath string) []string {
	args := diskPortArgs()
	for i, d := range cfg.Disks {
		args = append(args,
			"-drive", diskDrive(ExtraDiskPath(storagePath, cfg.Name, d.Name), diskDriveID(d.Name)),
			"-device", diskDevice(d.Name, i),
		)
	}
	return args
}

// --- Hot-plug ---

// DiskHotplug attaches cfg.Disks[idx] to the running VM through the QEMU
// monitor: the drive first, then the virtio-blk device on root port idx.
// Disks are only ever appended while a VM runs (removing one needs it
// stopped), so the port is free unless the VM was started by an Ostrich
// without root ports, which QEMU reports as the bus not being found.
func DiskHotplug(storagePath string, cfg *VMConfig, idx int) error {
	if idx < 0 || idx >= len(cfg.Disks) {
		return fmt.Errorf("no disk at index %d", idx)
	}
	if idx >= MaxExtraDisks {
		return fmt.Errorf("at most %d additional disks", MaxExtraDisks)
	}
	d := cfg.Disks[idx]
	if err := d.Validate(); err != nil {
		return err
	}
	path := ExtraDiskPath(storagePath, cfg.Name, d.Name)
	if err := checkImage(path); err != nil {
		return err
	}
	driveID := diskDriveID(d.Name)
	// drive_add answers "OK" on success; its first argument is a PCI address
	// that is ignored for if=none drives, and the options are one quoted
	// HMP argument in case the path has spaces.
	resp, err := MonitorCommand(storagePath, cfg.Name, "drive_add 0 "+hmpQuote(diskDrive(path, driveID)))
	if err != nil {
		return err
	}
	if resp != "OK" {
		return fmt.Errorf("QEMU: %s", resp)
	}
	if err := monitorMustSucceed(storagePath, cfg.Name, "device_add "+diskDevice(d.Name, idx)); err != nil {
		// Leave no orphan drive behind holding the file open.
		_, _ = MonitorCommand(storagePath, cfg.Name, "drive_del "+driveID)
		return err
	}
	return nil
}
