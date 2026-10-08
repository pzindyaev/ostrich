package vm

import (
	"os"
	"os/exec"
	"strings"
	"testing"
)

// TestUEFIBootWithQEMU starts VMs on the host's real OVMF and checks QEMU got
// the firmware image and the VM's own NVRAM copy on its pflash units. The
// Secure Boot case runs when this host can enroll the keys (a template that
// has them, or virt-fw-vars). Skipped without QEMU or UEFI firmware.
func TestUEFIBootWithQEMU(t *testing.T) {
	for _, bin := range []string{"qemu-system-x86_64", "qemu-img"} {
		if _, err := exec.LookPath(bin); err != nil {
			t.Skipf("%s not installed", bin)
		}
	}
	if _, err := FindFirmware("x86_64", "q35", false); err != nil {
		t.Skip(err)
	}

	for _, secureBoot := range []bool{false, true} {
		name := "uefi"
		if secureBoot {
			name = "secureboot"
		}
		t.Run(name, func(t *testing.T) {
			fw, err := FindFirmware("x86_64", "q35", secureBoot)
			if err != nil {
				t.Skip(err)
			}
			if secureBoot && !fw.EnrolledKeys {
				if _, err := exec.LookPath(enrollTool); err != nil {
					t.Skipf("%s not installed", enrollTool)
				}
			}

			storage := t.TempDir()
			cfg := &VMConfig{
				Name: name, CPU: 1, RAM: 512, DiskSize: 1,
				Firmware: FirmwareUEFI, SecureBoot: secureBoot,
				Network: NetworkConfig{Type: NetworkNone},
			}
			if err := NewManager(storage).Create(cfg); err != nil {
				t.Fatal(err)
			}
			vars := FirmwareVarsPath(storage, name)
			st, err := os.Stat(vars)
			if err != nil {
				t.Fatalf("NVRAM store not created: %v", err)
			}
			if tmpl, err := os.Stat(fw.VarsTemplate); err == nil && fw.VarsFormat == "raw" && st.Size() != tmpl.Size() {
				t.Errorf("NVRAM store is %d bytes, template %d — pflash needs them equal", st.Size(), tmpl.Size())
			}

			if err := Start(storage, cfg); err != nil {
				t.Fatal(err)
			}
			t.Cleanup(func() { _ = Stop(storage, cfg.Name) })
			waitForMonitor(t, storage, cfg.Name)

			block, _ := MonitorCommand(storage, cfg.Name, "info block")
			for _, want := range []string{fw.Code, vars} {
				if !strings.Contains(block, want) {
					t.Errorf("pflash image %s not attached:\n%s", want, block)
				}
			}
			if err := Stop(storage, cfg.Name); err != nil {
				t.Fatal(err)
			}
		})
	}
}

// TestTPMWithQEMU checks swtpm is started with the VM, the guest gets a TPM
// device and the daemon is gone after Stop. Skipped without QEMU or swtpm.
func TestTPMWithQEMU(t *testing.T) {
	for _, bin := range []string{"qemu-system-x86_64", "qemu-img", tpmBin} {
		if _, err := exec.LookPath(bin); err != nil {
			t.Skipf("%s not installed", bin)
		}
	}
	storage := t.TempDir()
	cfg := &VMConfig{
		Name: "tpm", CPU: 1, RAM: 128, DiskSize: 1, TPM: true,
		Network: NetworkConfig{Type: NetworkNone},
	}
	if err := NewManager(storage).Create(cfg); err != nil {
		t.Fatal(err)
	}
	if err := Start(storage, cfg); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = Stop(storage, cfg.Name) })

	qtree := waitForMonitor(t, storage, cfg.Name)
	if !strings.Contains(qtree, "dev: tpm-tis") {
		t.Errorf("guest has no TPM device:\n%s", qtree)
	}
	if _, err := os.Stat(TPMPIDPath(storage, cfg.Name)); err != nil {
		t.Errorf("swtpm PID file missing: %v", err)
	}
	if entries, _ := os.ReadDir(TPMDir(storage, cfg.Name)); len(entries) == 0 {
		t.Error("swtpm wrote no state")
	}

	if err := Stop(storage, cfg.Name); err != nil {
		t.Fatal(err)
	}
	for _, p := range []string{TPMPIDPath(storage, cfg.Name), TPMSockPath(storage, cfg.Name)} {
		if _, err := os.Stat(p); err == nil {
			t.Errorf("%s still present after Stop", p)
		}
	}
}
