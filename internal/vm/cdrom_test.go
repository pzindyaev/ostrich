package vm

import (
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
)

func TestCDROMArgs(t *testing.T) {
	cases := []struct {
		machine, path, want string
	}{
		{"q35", "/isos/a,b.iso", "-drive if=ide,index=2,id=cdrom,media=cdrom,format=raw,file=/isos/a,,b.iso"},
		{"q35", "", "-drive if=ide,index=2,id=cdrom,media=cdrom"},
		{"virt", "/isos/a.iso", "-drive if=none,id=cdrom,media=cdrom,format=raw,file=/isos/a.iso -device virtio-scsi-pci,id=scsi0 -device scsi-cd,bus=scsi0.0,drive=cdrom"},
		{"virt", "", "-drive if=none,id=cdrom,media=cdrom -device virtio-scsi-pci,id=scsi0 -device scsi-cd,bus=scsi0.0,drive=cdrom"},
	}
	for _, c := range cases {
		if got := strings.Join(cdromArgs(c.machine, c.path), " "); got != c.want {
			t.Errorf("cdromArgs(%q, %q)\n got %s\nwant %s", c.machine, c.path, got, c.want)
		}
	}
}

func TestBuildQEMUArgsCDROM(t *testing.T) {
	cfg := &VMConfig{Name: "t", CPU: 1, RAM: 128, Network: NetworkConfig{Type: NetworkNone}, CDROMPath: "/isos/a.iso"}
	_, args, _ := BuildQEMUArgs(cfg, t.TempDir())
	joined := strings.Join(args, " ")
	if !strings.Contains(joined, "-drive if=ide,index=2,id=cdrom,media=cdrom,format=raw,file=/isos/a.iso -boot order=dc") {
		t.Errorf("args with ISO: %s", joined)
	}
	if strings.Contains(joined, "-cdrom") {
		t.Errorf("-cdrom has no drive ID to swap by: %s", joined)
	}

	// Without an ISO the drive is still there, empty, and nothing steers the
	// boot order towards it.
	cfg.CDROMPath = ""
	_, args, _ = BuildQEMUArgs(cfg, t.TempDir())
	joined = strings.Join(args, " ")
	if !strings.Contains(joined, "-drive if=ide,index=2,id=cdrom,media=cdrom ") || strings.Contains(joined, "-boot") {
		t.Errorf("args without ISO: %s", joined)
	}

	cfg.Arch = "aarch64"
	_, args, _ = BuildQEMUArgs(cfg, t.TempDir())
	if joined = strings.Join(args, " "); !strings.Contains(joined, "scsi-cd,bus=scsi0.0,drive=cdrom") || strings.Contains(joined, "if=ide") {
		t.Errorf("virt machine must not get an IDE drive: %s", joined)
	}
}

// TestStartRefusesMissingBootISO checks Start fails up front, with the path,
// rather than launching a QEMU that dies on its discarded stderr.
func TestStartRefusesMissingBootISO(t *testing.T) {
	storage := t.TempDir()
	cfg := &VMConfig{Name: "t", CPU: 1, RAM: 128, Network: NetworkConfig{Type: NetworkNone},
		CDROMPath: filepath.Join(storage, "gone.iso")}
	if err := os.MkdirAll(VMDir(storage, cfg.Name), 0o755); err != nil {
		t.Fatal(err)
	}
	err := Start(storage, cfg)
	if err == nil || !strings.Contains(err.Error(), "gone.iso") || !strings.Contains(err.Error(), "boot ISO") {
		t.Errorf("Start error = %v", err)
	}
	if info, _ := Status(storage, cfg.Name); info.Status != StatusStopped {
		t.Errorf("a VM must not be started with a missing boot ISO: %+v", info)
	}
}

// TestCDROMSwapWithQEMU boots a real VM with an ISO in the drive, swaps it for
// another, ejects it and puts one back, checking the drive each time. Skipped
// without QEMU.
func TestCDROMSwapWithQEMU(t *testing.T) {
	for _, bin := range []string{"qemu-system-x86_64", "qemu-img"} {
		if _, err := exec.LookPath(bin); err != nil {
			t.Skipf("%s not installed", bin)
		}
	}
	isos := t.TempDir()
	first := writeImage(t, isos, "first.iso", 1<<20)
	second := writeImage(t, isos, "second disc, v2.iso", 2<<20) // space and comma: quoting and escaping

	storage := t.TempDir()
	cfg := &VMConfig{
		Name: "cdtest", CPU: 1, RAM: 128, DiskSize: 1,
		Network: NetworkConfig{Type: NetworkNone}, CDROMPath: first,
	}
	if err := NewManager(storage).Create(cfg); err != nil {
		t.Fatal(err)
	}
	if err := Start(storage, cfg); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = Stop(storage, cfg.Name) })
	waitForMonitor(t, storage, cfg.Name)

	drive := func() string {
		out, err := MonitorCommand(storage, cfg.Name, "info block "+cdromDriveID)
		if err != nil {
			t.Fatal(err)
		}
		return out
	}
	if out := drive(); !strings.Contains(out, first) || !strings.Contains(out, "read-only") {
		t.Errorf("boot-time ISO should be in the drive, read-only:\n%s", out)
	}

	if err := CDROMChange(storage, cfg.Name, second); err != nil {
		t.Fatalf("swap: %v", err)
	}
	if out := drive(); !strings.Contains(out, second) || strings.Contains(out, first) || !strings.Contains(out, "tray closed") {
		t.Errorf("after swap:\n%s", out)
	}

	if err := CDROMChange(storage, cfg.Name, ""); err != nil {
		t.Fatalf("eject: %v", err)
	}
	if out := drive(); !strings.Contains(out, "[not inserted]") {
		t.Errorf("after eject:\n%s", out)
	}
	if err := CDROMChange(storage, cfg.Name, ""); err != nil {
		t.Errorf("ejecting an empty drive must be fine: %v", err)
	}

	if err := CDROMChange(storage, cfg.Name, first); err != nil {
		t.Fatalf("insert into empty drive: %v", err)
	}
	if out := drive(); !strings.Contains(out, first) || !strings.Contains(out, "tray closed") {
		t.Errorf("after insert:\n%s", out)
	}

	// A missing file is caught before the drive is touched.
	err := CDROMChange(storage, cfg.Name, filepath.Join(isos, "gone.iso"))
	if err == nil || !strings.Contains(err.Error(), "not found") {
		t.Errorf("missing ISO should fail, got: %v", err)
	}
	if out := drive(); !strings.Contains(out, first) {
		t.Errorf("failed swap must leave the disc alone:\n%s", out)
	}

	if err := Stop(storage, cfg.Name); err != nil {
		t.Fatal(err)
	}
}
