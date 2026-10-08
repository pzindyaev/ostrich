package tui

import (
	"strings"
	"testing"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/pzindyaev/ostrich/internal/vm"
)

func press(s string) tea.KeyMsg {
	switch s {
	case "enter":
		return tea.KeyMsg{Type: tea.KeyEnter}
	case "tab":
		return tea.KeyMsg{Type: tea.KeyTab}
	}
	return tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune(s)}
}

// TestCreateFormFirmwareSteps walks the wizard with Enter, picking Secure Boot
// and a TPM on the way, and checks every step renders and the result is right.
func TestCreateFormFirmwareSteps(t *testing.T) {
	m := NewCreateVMModel(vm.NewManager(t.TempDir()), 100, 40)
	keys := map[createStep][]string{
		stepFirmware: {"l", "l"}, // BIOS → UEFI → UEFI + Secure Boot
		stepTPM:      {"l"},      // disabled → enabled
	}
	for s := createStep(0); s < stepConfirm; s++ {
		if m.step != s {
			t.Fatalf("at step %d, want %d (err %q)", m.step, s, m.err)
		}
		if v := m.View(); !strings.Contains(v, stepLabels[s]) {
			t.Errorf("step %d view lacks its label %q", s, stepLabels[s])
		}
		for _, k := range keys[s] {
			m, _ = m.Update(press(k))
		}
		m, _ = m.Update(press("enter"))
	}
	if v := m.View(); !strings.Contains(v, "UEFI + Secure Boot, TPM 2.0") {
		t.Errorf("confirm view lacks the firmware summary:\n%s", v)
	}
	cfg, err := m.buildConfig()
	if err != nil {
		t.Fatal(err)
	}
	if cfg.Firmware != vm.FirmwareUEFI || !cfg.SecureBoot || !cfg.TPM {
		t.Errorf("got %+v", cfg)
	}

	// Wrapping backwards from BIOS lands on Secure Boot; TPM toggles.
	m.step = stepFirmware
	m.fwIdx, m.tpmIdx = 0, 1
	m, _ = m.Update(press("h"))
	m.step = stepTPM
	m, _ = m.Update(press("l"))
	if m.fwIdx != 2 || m.tpmIdx != 0 {
		t.Errorf("fwIdx=%d tpmIdx=%d", m.fwIdx, m.tpmIdx)
	}
}

func TestEditFormFirmwareFields(t *testing.T) {
	mgr := vm.NewManager(t.TempDir())
	cfg := &vm.VMConfig{Name: "win", CPU: 2, RAM: 4096, DiskSize: 64, SecureBoot: true, TPM: true,
		Network: vm.NetworkConfig{Type: vm.NetworkUser, MAC: "52:54:00:00:00:01"}}
	m := NewEditVMModel(mgr, cfg, 100, 40)
	if m.fwIdx != 2 || m.tpmIdx != 1 {
		t.Fatalf("selectors not pre-filled: fw=%d tpm=%d", m.fwIdx, m.tpmIdx)
	}
	v := m.View()
	for _, want := range []string{"Firmware", "TPM 2.0", "UEFI + Secure Boot", "enabled"} {
		if !strings.Contains(v, want) {
			t.Errorf("view lacks %q", want)
		}
	}
	for m.field != editFirmware {
		m, _ = m.Update(press("tab"))
	}
	m, _ = m.Update(press("h")) // Secure Boot → UEFI
	m, _ = m.Update(press("tab"))
	m, _ = m.Update(press("l")) // TPM enabled → disabled
	out, _, err := m.buildConfig()
	if err != nil {
		t.Fatal(err)
	}
	if out.Firmware != vm.FirmwareUEFI || out.SecureBoot || out.TPM {
		t.Errorf("got firmware=%q secure_boot=%v tpm=%v", out.Firmware, out.SecureBoot, out.TPM)
	}

	// A running VM may not change firmware.
	m.running = true
	if _, field, err := m.buildConfig(); err == nil || field != editFirmware {
		t.Errorf("running VM firmware change accepted: %v (field %d)", err, field)
	}
}
