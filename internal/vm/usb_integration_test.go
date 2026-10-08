package vm

import (
	"os"
	"os/exec"
	"strings"
	"testing"
	"time"
)

// TestUSBPassthroughWithQEMU boots a real (diskless-content, display-less) VM
// and checks the USB controller, boot-time usb-host devices and monitor
// hot-plug against the actual QEMU. Skipped when QEMU is not installed.
func TestUSBPassthroughWithQEMU(t *testing.T) {
	for _, bin := range []string{"qemu-system-x86_64", "qemu-img"} {
		if _, err := exec.LookPath(bin); err != nil {
			t.Skipf("%s not installed", bin)
		}
	}

	storage := t.TempDir()
	cfg := &VMConfig{
		Name: "usbtest", CPU: 1, RAM: 128, DiskSize: 1,
		Network: NetworkConfig{Type: NetworkNone},
		// Not a real device: QEMU accepts it and waits for it to be plugged in.
		USBDevices: []USBDevice{{VendorID: "1234", ProductID: "5678", Name: "Phantom"}},
	}
	if err := NewManager(storage).Create(cfg); err != nil {
		t.Fatal(err)
	}
	if err := Start(storage, cfg); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = Stop(storage, cfg.Name) })

	qtree := waitForMonitor(t, storage, cfg.Name)
	for _, want := range []string{`dev: qemu-xhci, id "xhci"`, `dev: usb-host, id "usb-1234-5678"`} {
		if !strings.Contains(qtree, want) {
			t.Errorf("boot-time device tree lacks %q:\n%s", want, qtree)
		}
	}
	if st := USBStates(cfg.USBDevices); st[0].Host != nil {
		t.Errorf("phantom device should be reported as not connected, got %+v", st[0].Host)
	}

	// Hot-plug a second device, then unplug it again.
	cfg.USBDevices = append(cfg.USBDevices, USBDevice{VendorID: "abcd", ProductID: "ef01", Port: "9-1.2"})
	if err := USBHotplug(storage, cfg, 1); err != nil {
		t.Fatalf("hot-plug: %v", err)
	}
	if qtree, _ = MonitorCommand(storage, cfg.Name, "info qtree"); !strings.Contains(qtree, `id "usb-abcd-ef01-9-1.2"`) {
		t.Errorf("hot-plugged device missing from device tree:\n%s", qtree)
	}
	if err := USBHotunplug(storage, cfg, 1); err != nil {
		t.Fatalf("hot-unplug: %v", err)
	}
	if qtree, _ = MonitorCommand(storage, cfg.Name, "info qtree"); strings.Contains(qtree, `usb-abcd-ef01`) {
		t.Errorf("hot-unplugged device still in device tree:\n%s", qtree)
	}

	// QEMU's errors must surface instead of being swallowed.
	err := USBHotplug(storage, cfg, 0) // same ID as the boot-time device
	if err == nil || !strings.Contains(err.Error(), "usb-1234-5678") {
		t.Errorf("duplicate ID should fail with QEMU's message, got: %v", err)
	}
	err = monitorMustSucceed(storage, cfg.Name, "device_add usb-host,id=x,bus=nope.0,vendorid=0x1,productid=0x1")
	if err == nil || !strings.Contains(err.Error(), "nope.0") {
		t.Errorf("bad bus should fail with QEMU's message, got: %v", err)
	}

	hosts, _ := MonitorCommand(storage, cfg.Name, "info usbhost")
	t.Logf("info usbhost:\n%s", hosts)

	if err := Stop(storage, cfg.Name); err != nil {
		t.Fatal(err)
	}
	if info, _ := Status(storage, cfg.Name); info.Status != StatusStopped {
		t.Errorf("VM still running after Stop: %+v", info)
	}
}

// waitForMonitor polls until the VM's monitor answers, returning "info qtree".
func waitForMonitor(t *testing.T, storage, name string) string {
	t.Helper()
	deadline := time.Now().Add(15 * time.Second)
	for time.Now().Before(deadline) {
		if out, err := MonitorCommand(storage, name, "info qtree"); err == nil && out != "" {
			return out
		}
		time.Sleep(200 * time.Millisecond)
	}
	info, _ := Status(storage, name)
	t.Fatalf("QEMU monitor did not come up (status %+v)", info)
	return ""
}

// TestUSBRealDeviceWithQEMU passes a physical host device through to a VM and
// checks QEMU actually claims it. The host loses the device for the duration,
// so it only runs for the device named in OSTRICH_USB_TEST_DEVICE (vendor:product),
// e.g. a webcam — never pick the keyboard or mouse you are typing on.
func TestUSBRealDeviceWithQEMU(t *testing.T) {
	id := os.Getenv("OSTRICH_USB_TEST_DEVICE")
	if id == "" {
		t.Skip("set OSTRICH_USB_TEST_DEVICE=vvvv:pppp to pass a real device through")
	}
	for _, bin := range []string{"qemu-system-x86_64", "qemu-img"} {
		if _, err := exec.LookPath(bin); err != nil {
			t.Skipf("%s not installed", bin)
		}
	}
	dev, err := ParseUSBID(id)
	if err != nil {
		t.Fatal(err)
	}
	st := USBStates([]USBDevice{dev})
	if st[0].Host == nil {
		t.Fatalf("%s is not connected to the host", id)
	}
	if !st[0].Host.Writable {
		t.Fatalf("%s is not writable by this user:\n%s", id, UdevRuleHint(dev))
	}
	dev.Name = st[0].Host.Label()

	storage := t.TempDir()
	cfg := &VMConfig{
		Name: "usbreal", CPU: 1, RAM: 128, DiskSize: 1,
		Network: NetworkConfig{Type: NetworkNone}, USBDevices: []USBDevice{dev},
	}
	if err := NewManager(storage).Create(cfg); err != nil {
		t.Fatal(err)
	}
	if err := Start(storage, cfg); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = Stop(storage, cfg.Name) })
	waitForMonitor(t, storage, cfg.Name)

	// "info usb" only lists a usb-host device once QEMU has opened the host
	// device and attached it to a port, so this proves the claim succeeded.
	qemuID := USBDeviceIDs(cfg.USBDevices)[0]
	attached := waitForUSB(t, storage, cfg.Name, qemuID, true)
	t.Logf("attached at boot:\n%s", attached)

	if err := USBHotunplug(storage, cfg, 0); err != nil {
		t.Fatalf("hot-unplug: %v", err)
	}
	waitForUSB(t, storage, cfg.Name, qemuID, false)
	if err := USBHotplug(storage, cfg, 0); err != nil {
		t.Fatalf("hot-plug: %v", err)
	}
	t.Logf("re-attached by hot-plug:\n%s", waitForUSB(t, storage, cfg.Name, qemuID, true))

	if err := Stop(storage, cfg.Name); err != nil {
		t.Fatal(err)
	}
	// The host should have the device back once QEMU is gone.
	deadline := time.Now().Add(10 * time.Second)
	for time.Now().Before(deadline) {
		if st := USBStates([]USBDevice{dev}); st[0].Host != nil {
			t.Logf("host sees the device again at %s", st[0].Host.DevNode)
			return
		}
		time.Sleep(200 * time.Millisecond)
	}
	t.Error("host did not get the device back after the VM stopped")
}

// waitForUSB polls "info usb" until the device ID is (or is no longer) listed.
func waitForUSB(t *testing.T, storage, name, qemuID string, present bool) string {
	t.Helper()
	deadline := time.Now().Add(15 * time.Second)
	var out string
	for time.Now().Before(deadline) {
		out, _ = MonitorCommand(storage, name, "info usb")
		if strings.Contains(out, "ID: "+qemuID) == present {
			return out
		}
		time.Sleep(250 * time.Millisecond)
	}
	t.Fatalf("device %s present=%v not reached; info usb:\n%s", qemuID, present, out)
	return ""
}
