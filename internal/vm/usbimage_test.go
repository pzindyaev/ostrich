package vm

import (
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

// writeImage creates a small fake image file and returns its path.
func writeImage(t *testing.T, dir, name string, size int) string {
	t.Helper()
	p := filepath.Join(dir, name)
	if err := os.WriteFile(p, make([]byte, size), 0o644); err != nil {
		t.Fatal(err)
	}
	return p
}

func TestUSBImageValidate(t *testing.T) {
	dir := t.TempDir()
	ok := USBImage{Path: writeImage(t, dir, "ok.iso", 16)}
	if err := ok.Validate(); err != nil {
		t.Fatal(err)
	}
	cases := map[string]USBImage{
		"empty":     {},
		"relative":  {Path: "ok.iso"},
		"missing":   {Path: filepath.Join(dir, "nope.iso")},
		"directory": {Path: dir},
	}
	for name, img := range cases {
		err := img.Validate()
		if err == nil {
			t.Errorf("%s: %+v accepted", name, img)
			continue
		}
		if name == "missing" && !strings.Contains(err.Error(), "not found") {
			t.Errorf("missing file error = %v", err)
		}
	}
	if os.Getuid() != 0 {
		unreadable := USBImage{Path: writeImage(t, dir, "secret.iso", 16)}
		if err := os.Chmod(unreadable.Path, 0o000); err != nil {
			t.Fatal(err)
		}
		if err := unreadable.Validate(); err == nil || !strings.Contains(err.Error(), "no read access") {
			t.Errorf("unreadable file error = %v", err)
		}
	}

	states := USBImageStates([]USBImage{ok, {Path: filepath.Join(dir, "nope.iso")}})
	if states[0].Err != nil || states[0].Size != 16 || states[1].Err == nil {
		t.Errorf("states = %+v", states)
	}
	err := CheckUSBImages([]USBImage{ok, {Path: filepath.Join(dir, "nope.iso")}})
	if err == nil || !strings.Contains(err.Error(), "nope.iso") || strings.Contains(err.Error(), "ok.iso") {
		t.Errorf("CheckUSBImages error = %v", err)
	}
	if err := CheckUSBImages(nil); err != nil {
		t.Errorf("no images must pass: %v", err)
	}
}

func TestUSBImageIDsAndSpec(t *testing.T) {
	imgs := []USBImage{
		{Path: "/isos/virtio-win.iso"},
		{Path: "/isos/My Stuff (2024), v2.iso"}, // spaces, parens and a comma
		{Path: "/other/virtio-win.iso"},         // same file name elsewhere
	}
	ids := USBImageIDs(imgs)
	want := []string{"usbimg-virtio-win.iso", "usbimg-My-Stuff--2024---v2.iso", "usbimg-virtio-win.iso-2"}
	for i := range want {
		if ids[i] != want[i] {
			t.Errorf("id[%d] = %q, want %q", i, ids[i], want[i])
		}
	}

	drive := usbImageDriveID(ids[0])
	if got := usbImageDrive(imgs[0], drive); got != "if=none,id=usbimg-virtio-win.iso-drive,format=raw,readonly=on,file=/isos/virtio-win.iso" {
		t.Errorf("drive = %q", got)
	}
	// A comma in the path has to be doubled for QEMU's option parser.
	if got := usbImageDrive(imgs[1], usbImageDriveID(ids[1])); !strings.HasSuffix(got, ",file=/isos/My Stuff (2024),, v2.iso") {
		t.Errorf("drive with comma = %q", got)
	}
	if got := usbImageDevice(ids[0], drive); got != "usb-storage,id=usbimg-virtio-win.iso,bus=xhci.0,drive=usbimg-virtio-win.iso-drive,removable=on" {
		t.Errorf("device = %q", got)
	}
}

func TestBuildQEMUArgsUSBImages(t *testing.T) {
	cfg := &VMConfig{Name: "t", CPU: 1, RAM: 128, Network: NetworkConfig{Type: NetworkNone},
		USBImages: []USBImage{{Path: "/isos/a.iso"}, {Path: "/isos/b.iso"}}}
	_, args, _ := BuildQEMUArgs(cfg, t.TempDir())
	joined := strings.Join(args, " ")
	for _, want := range []string{
		"-drive if=none,id=usbimg-a.iso-drive,format=raw,readonly=on,file=/isos/a.iso -device usb-storage,id=usbimg-a.iso,bus=xhci.0,drive=usbimg-a.iso-drive,removable=on",
		"-drive if=none,id=usbimg-b.iso-drive,format=raw,readonly=on,file=/isos/b.iso -device usb-storage,id=usbimg-b.iso,bus=xhci.0,drive=usbimg-b.iso-drive,removable=on",
	} {
		if !strings.Contains(joined, want) {
			t.Errorf("args lack %q:\n%s", want, joined)
		}
	}
	// The controller the devices attach to comes first.
	if strings.Index(joined, "qemu-xhci") > strings.Index(joined, "usb-storage") {
		t.Errorf("xhci controller must precede usb-storage devices:\n%s", joined)
	}
}

// TestStartRefusesMissingImage checks Start fails up front, with the path,
// rather than launching a QEMU that dies on its discarded stderr.
func TestStartRefusesMissingImage(t *testing.T) {
	storage := t.TempDir()
	cfg := &VMConfig{Name: "t", CPU: 1, RAM: 128, Network: NetworkConfig{Type: NetworkNone},
		USBImages: []USBImage{{Path: filepath.Join(storage, "gone.iso")}}}
	if err := os.MkdirAll(VMDir(storage, cfg.Name), 0o755); err != nil {
		t.Fatal(err)
	}
	err := Start(storage, cfg)
	if err == nil || !strings.Contains(err.Error(), "gone.iso") || !strings.Contains(err.Error(), "ISO hot-plug") {
		t.Errorf("Start error = %v", err)
	}
	if info, _ := Status(storage, cfg.Name); info.Status != StatusStopped {
		t.Errorf("a VM must not be started with a missing image: %+v", info)
	}
}

// TestUSBImagesWithQEMU boots a real VM with one image attached at boot, then
// hot-plugs a second one, checks both show up as USB mass storage backed by
// the right (read-only) files, and unplugs them again. Skipped without QEMU.
func TestUSBImagesWithQEMU(t *testing.T) {
	for _, bin := range []string{"qemu-system-x86_64", "qemu-img"} {
		if _, err := exec.LookPath(bin); err != nil {
			t.Skipf("%s not installed", bin)
		}
	}
	isos := t.TempDir()
	boot := writeImage(t, isos, "boot.iso", 1<<20)
	plug := writeImage(t, isos, "plug, me.iso", 2<<20) // comma: the option escaping must hold up

	storage := t.TempDir()
	cfg := &VMConfig{
		Name: "isotest", CPU: 1, RAM: 128, DiskSize: 1,
		Network:   NetworkConfig{Type: NetworkNone},
		USBImages: []USBImage{{Path: boot}},
	}
	if err := NewManager(storage).Create(cfg); err != nil {
		t.Fatal(err)
	}
	if err := Start(storage, cfg); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = Stop(storage, cfg.Name) })

	waitForMonitor(t, storage, cfg.Name)
	usb := waitForUSB(t, storage, cfg.Name, "usbimg-boot.iso", true)
	if !strings.Contains(usb, "QEMU USB MSD") {
		t.Errorf("boot-time image is not a mass-storage device:\n%s", usb)
	}
	block, _ := MonitorCommand(storage, cfg.Name, "info block usbimg-boot.iso-drive")
	if !strings.Contains(block, boot) || !strings.Contains(block, "read-only") {
		t.Errorf("boot-time drive should be %s read-only:\n%s", boot, block)
	}

	// Hot-plug a second image, then unplug it: device and drive must both go.
	cfg.USBImages = append(cfg.USBImages, USBImage{Path: plug})
	if err := USBImageHotplug(storage, cfg, 1); err != nil {
		t.Fatalf("hot-plug: %v", err)
	}
	id := USBImageIDs(cfg.USBImages)[1]
	waitForUSB(t, storage, cfg.Name, id, true)
	block = waitForBlock(t, storage, cfg.Name, plug, true)
	if !strings.Contains(block, "read-only") {
		t.Errorf("hot-plugged drive should be read-only:\n%s", block)
	}
	if err := USBImageHotunplug(storage, cfg, 1); err != nil {
		t.Fatalf("hot-unplug: %v", err)
	}
	waitForUSB(t, storage, cfg.Name, id, false)
	waitForBlock(t, storage, cfg.Name, plug, false) // QEMU reaps the drive with the device

	// Plugging the same image again right away must work.
	if err := USBImageHotplug(storage, cfg, 1); err != nil {
		t.Fatalf("second hot-plug: %v", err)
	}
	waitForUSB(t, storage, cfg.Name, id, true)
	waitForBlock(t, storage, cfg.Name, plug, true)

	// QEMU's own errors surface instead of being swallowed: a duplicate ID.
	// The drive added for the failed attempt is rolled back.
	err := USBImageHotplug(storage, cfg, 1)
	if err == nil || !strings.Contains(err.Error(), id) {
		t.Errorf("duplicate hot-plug should fail with QEMU's message, got: %v", err)
	}
	if block, _ = MonitorCommand(storage, cfg.Name, "info block"); strings.Count(block, plug) != 1 {
		t.Errorf("failed hot-plug left a drive behind:\n%s", block)
	}
	// And a missing file is caught before anything reaches the monitor.
	cfg.USBImages = append(cfg.USBImages, USBImage{Path: filepath.Join(isos, "gone.iso")})
	if err := USBImageHotplug(storage, cfg, 2); err == nil || !strings.Contains(err.Error(), "not found") {
		t.Errorf("missing image should fail, got: %v", err)
	}

	if err := Stop(storage, cfg.Name); err != nil {
		t.Fatal(err)
	}
}

// waitForBlock polls "info block" until a drive backed by path is (or is no
// longer) listed, and returns the output.
func waitForBlock(t *testing.T, storage, name, path string, present bool) string {
	t.Helper()
	deadline := time.Now().Add(15 * time.Second)
	var out string
	for time.Now().Before(deadline) {
		out, _ = MonitorCommand(storage, name, "info block")
		if strings.Contains(out, path) == present {
			return out
		}
		time.Sleep(250 * time.Millisecond)
	}
	t.Fatalf("drive for %s present=%v not reached; info block:\n%s", path, present, out)
	return ""
}
