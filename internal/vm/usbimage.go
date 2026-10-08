package vm

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"strconv"
	"time"
)

// USBImage is a disk image on the host — an ISO as a rule — attached to the
// guest as a read-only USB mass-storage drive (vm.yaml schema). The guest sees
// a USB stick holding the image byte for byte: a Linux guest mounts the ISO9660
// filesystem straight off it, and a fresh VM boots a hybrid ISO from it.
type USBImage struct {
	Path string `yaml:"path"` // absolute path on the host
}

// Label returns the file name.
func (img USBImage) Label() string {
	return filepath.Base(img.Path)
}

// Validate checks that the image is an absolute path to a readable file.
// Absolute, because QEMU reads a prefix such as "nbd:" in a file name as a
// protocol, and a running VM resolves relative paths against its own cwd.
func (img USBImage) Validate() error {
	if img.Path != "" && !filepath.IsAbs(img.Path) {
		return fmt.Errorf("image path must be absolute: %s", img.Path)
	}
	return checkImage(img.Path)
}

// checkImage checks that path names a readable file. QEMU refuses to start
// without one, and that only shows on its discarded stderr.
func checkImage(path string) error {
	if path == "" {
		return errors.New("no image path")
	}
	st, err := os.Stat(path)
	if err != nil {
		if os.IsNotExist(err) {
			return fmt.Errorf("image not found: %s", path)
		}
		return fmt.Errorf("image %s: %w", path, err)
	}
	if st.IsDir() {
		return fmt.Errorf("image is a directory: %s", path)
	}
	f, err := os.Open(path)
	if err != nil {
		return fmt.Errorf("no read access to image %s", path)
	}
	f.Close()
	return nil
}

// usbIDUnsafe matches what QEMU does not allow in an object ID after the
// first character (letters, digits, '.', '_' and '-' are fine).
var usbIDUnsafe = regexp.MustCompile(`[^A-Za-z0-9._-]`)

// USBImageIDs returns the QEMU device IDs for the configured images, derived
// from the file name so an image attached at boot can later be named for
// hot-unplug. Two images with the same file name get a numeric suffix.
func USBImageIDs(imgs []USBImage) []string {
	ids := make([]string, len(imgs))
	seen := map[string]int{}
	for i, img := range imgs {
		id := "usbimg-" + usbIDUnsafe.ReplaceAllString(img.Label(), "-")
		seen[id]++
		if n := seen[id]; n > 1 {
			id = fmt.Sprintf("%s-%d", id, n)
		}
		ids[i] = id
	}
	return ids
}

// usbImageDriveID names the block backend behind the usb-storage device
// attached at boot. Hot-plug appends a unique suffix: QEMU deletes the drive
// of an unplugged device only once it finalizes the device object, which it
// defers, so a re-plug of the same image must not reuse the ID meanwhile.
func usbImageDriveID(id string) string {
	return id + "-drive"
}

// usbImageDrive returns the drive options for an image, used verbatim on the
// command line (-drive) and for hot-plug (drive_add). The image is opened
// read-only, so the file is never modified and several VMs can share it.
func usbImageDrive(img USBImage, driveID string) string {
	return fmt.Sprintf("if=none,id=%s,format=raw,readonly=on,file=%s", driveID, qemuOptEscape(img.Path))
}

// usbImageDevice returns the "usb-storage,..." device spec for an image, used
// both on the command line (-device) and for hot-plug (device_add). The guest
// sees a removable drive, like a real stick, on the always-present xHCI bus.
func usbImageDevice(id, driveID string) string {
	return fmt.Sprintf("usb-storage,id=%s,bus=%s,drive=%s,removable=on", id, usbBusName, driveID)
}

// CheckUSBImages validates every configured image and names each one that
// would stop QEMU from starting.
func CheckUSBImages(imgs []USBImage) error {
	var msg string
	for _, img := range imgs {
		if err := img.Validate(); err != nil {
			msg += fmt.Sprintf("USB drive %s: %v\n", img.Label(), err)
		}
	}
	if msg == "" {
		return nil
	}
	return errors.New(msg + "Detach it in the ISO hot-plug screen, or put the file back.")
}

// ImageState pairs an image path with what the host shows for it.
type ImageState struct {
	Path string
	Size int64 // bytes, when the file is there
	Err  error // nil when the image is present and readable
}

// ImageStateOf checks one image file on the host.
func ImageStateOf(path string) ImageState {
	s := ImageState{Path: path}
	if s.Err = checkImage(path); s.Err != nil {
		return s
	}
	if st, err := os.Stat(path); err == nil {
		s.Size = st.Size()
	}
	return s
}

// USBImageStates checks each configured image on the host.
func USBImageStates(imgs []USBImage) []ImageState {
	states := make([]ImageState, len(imgs))
	for i, img := range imgs {
		states[i] = ImageStateOf(img.Path)
		if err := img.Validate(); err != nil {
			states[i].Err = err
		}
	}
	return states
}

// --- Hot-plug ---

// USBImageHotplug attaches cfg.USBImages[idx] to the running VM through the
// QEMU monitor: the drive first, then the usb-storage device on top of it.
func USBImageHotplug(storagePath string, cfg *VMConfig, idx int) error {
	img := cfg.USBImages[idx]
	if err := img.Validate(); err != nil {
		return err
	}
	id := USBImageIDs(cfg.USBImages)[idx]
	driveID := usbImageDriveID(id) + "-" + strconv.FormatInt(time.Now().UnixNano(), 36)
	// Unlike device_add, drive_add answers "OK" on success. Its first argument
	// is a PCI address that is ignored for if=none drives; the options are one
	// HMP string argument, so they are quoted in case the path has spaces.
	resp, err := MonitorCommand(storagePath, cfg.Name, "drive_add 0 "+hmpQuote(usbImageDrive(img, driveID)))
	if err != nil {
		return err
	}
	if resp != "OK" {
		return fmt.Errorf("QEMU: %s", resp)
	}
	if err := monitorMustSucceed(storagePath, cfg.Name, "device_add "+usbImageDevice(id, driveID)); err != nil {
		// Leave no orphan drive behind holding the file open.
		_, _ = MonitorCommand(storagePath, cfg.Name, "drive_del "+driveID)
		return err
	}
	return nil
}

// USBImageHotunplug detaches cfg.USBImages[idx] from the running VM; cfg is the
// config as it was before the entry was removed, so the device ID matches.
// QEMU drops the drive together with the device, shortly after.
func USBImageHotunplug(storagePath string, cfg *VMConfig, idx int) error {
	id := USBImageIDs(cfg.USBImages)[idx]
	return monitorMustSucceed(storagePath, cfg.Name, "device_del "+id)
}
