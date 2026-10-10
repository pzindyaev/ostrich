package tui

import (
	"path/filepath"
	"reflect"
	"strings"
	"testing"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/pzindyaev/ostrich/internal/config"
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
	t.Setenv("HOME", t.TempDir())
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
	t.Setenv("HOME", t.TempDir())
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

// TestCreateFormISOStep drives the ISO step, which is the picker.
func TestCreateFormISOStep(t *testing.T) {
	storage, isos := t.TempDir(), t.TempDir()
	isolateConfig(t, storage)
	disc := writeImage(t, filepath.Join(isos, "debian.iso"))
	other := writeImage(t, filepath.Join(isos, "other.iso"))
	if err := config.RememberISO(disc); err != nil {
		t.Fatal(err)
	}
	m := NewCreateVMModel(vm.NewManager(storage), 100, 40)
	for m.step != stepISO {
		m, _ = m.Update(key("enter"))
	}
	view := m.View()
	for _, want := range []string{"Boot ISO (optional)", "(none) — no boot ISO", "debian.iso", "● 1 MiB", "New path", "pick and go on"} {
		if !strings.Contains(view, want) {
			t.Errorf("ISO step view lacks %q", want)
		}
	}
	if !m.picker.onNone() {
		t.Fatalf("cursor = %d, want the none row", m.picker.cursor)
	}
	t.Log("\n" + view)

	// Enter on the disc takes it and moves on.
	m, _ = m.Update(key("down"))
	m, _ = m.Update(key("enter"))
	if m.step != stepFirmware || m.iso != disc {
		t.Fatalf("after pick: step=%d iso=%q", m.step, m.iso)
	}
	// Back on the step, the cursor is on the chosen disc; Tab moves on
	// without touching it.
	m, _ = m.Update(key("shift+tab"))
	if e, ok := m.picker.entry(); m.step != stepISO || !ok || e.path != disc {
		t.Fatalf("back on the step: step=%d cursor=%d", m.step, m.picker.cursor)
	}
	m, _ = m.Update(key("tab"))
	if m.step != stepFirmware || m.iso != disc {
		t.Fatalf("after tab: step=%d iso=%q", m.step, m.iso)
	}
	// A path typed but not entered is taken by Tab as well.
	m, _ = m.Update(key("shift+tab"))
	m, _ = m.Update(key("G"))
	for _, r := range other {
		m, _ = m.Update(key(string(r)))
	}
	m, _ = m.Update(key("tab"))
	if m.step != stepFirmware || m.iso != other {
		t.Fatalf("after typing and tab: step=%d iso=%q", m.step, m.iso)
	}
	// A path that is not there keeps the step, with the reason.
	m, _ = m.Update(key("shift+tab"))
	m, _ = m.Update(key("G"))
	for _, r := range "/nope.iso" {
		m, _ = m.Update(key(string(r)))
	}
	m, _ = m.Update(key("enter"))
	if m.step != stepISO || !strings.Contains(m.picker.err, "not found") || m.iso != other {
		t.Fatalf("bad path: step=%d err=%q iso=%q", m.step, m.picker.err, m.iso)
	}
	// (none) clears the choice.
	for !m.picker.onNone() {
		m, _ = m.Update(key("up"))
	}
	m, _ = m.Update(key("enter"))
	if m.step != stepFirmware || m.iso != "" {
		t.Fatalf("after none: step=%d iso=%q", m.step, m.iso)
	}
	// The summary shows the choice, and it goes into the config.
	m, _ = m.Update(key("shift+tab"))
	m, _ = m.Update(key("down"))
	m, _ = m.Update(key("enter"))
	for m.step != stepConfirm {
		m, _ = m.Update(key("enter"))
	}
	if cfg, err := m.buildConfig(); err != nil || cfg.CDROMPath != disc || !strings.Contains(m.View(), disc) {
		t.Errorf("confirm: cfg=%+v err=%v", cfg, err)
	}
	// Esc on the ISO step leaves the wizard like anywhere else.
	m.step = stepISO
	_, cmd := m.Update(key("esc"))
	if cmd == nil {
		t.Fatal("esc should leave the wizard")
	}
	if nav, ok := cmd().(NavigateMsg); !ok || nav.To != screenList {
		t.Errorf("esc gave %T", cmd())
	}
}

// TestEditFormISOField drives the Boot ISO field, which opens the picker.
func TestEditFormISOField(t *testing.T) {
	storage, isos := t.TempDir(), t.TempDir()
	isolateConfig(t, storage)
	disc := writeImage(t, filepath.Join(isos, "debian.iso"))
	other := writeImage(t, filepath.Join(isos, "other.iso"))
	cfg := &vm.VMConfig{Name: "t", CPU: 1, RAM: 128, DiskSize: 1, CDROMPath: disc,
		Network: vm.NetworkConfig{Type: vm.NetworkUser, MAC: "52:54:00:00:00:01"}}
	saveVM(t, storage, cfg)
	m := NewEditVMModel(vm.NewManager(storage), cfg, 100, 40)
	if !strings.Contains(m.View(), disc) {
		t.Errorf("form lacks the boot ISO:\n%s", m.View())
	}
	for m.field != editISO {
		m, _ = m.Update(key("tab"))
	}
	// Enter opens the picker on the current disc; Esc comes back unchanged.
	m, _ = m.Update(key("enter"))
	view := m.View()
	if e, ok := m.picker.entry(); !m.picking || !ok || e.path != disc || !strings.Contains(view, "New path") || !strings.Contains(view, "in use by t") {
		t.Fatalf("enter should open the picker on the disc: picking=%v cursor=%d\n%s", m.picking, m.picker.cursor, view)
	}
	t.Log("\n" + view)
	m, _ = m.Update(key("esc"))
	if m.picking || m.iso != disc || m.field != editISO {
		t.Fatalf("after esc: picking=%v iso=%q field=%d", m.picking, m.iso, m.field)
	}
	// l opens it too; (none) clears the field.
	m, _ = m.Update(key("l"))
	m, _ = m.Update(key("g"))
	m, _ = m.Update(key("enter"))
	if m.picking || m.iso != "" || !strings.Contains(m.View(), "(none)") {
		t.Fatalf("after none: picking=%v iso=%q", m.picking, m.iso)
	}
	if out, _, err := m.buildConfig(); err != nil || out.CDROMPath != "" {
		t.Errorf("config after none: %+v %v", out, err)
	}
	// A new path typed in the dialog is saved and remembered.
	m, _ = m.Update(key("enter"))
	m, _ = m.Update(key("G"))
	for _, r := range other {
		m, _ = m.Update(key(string(r)))
	}
	m, _ = m.Update(key("enter"))
	if m.picking || m.iso != other {
		t.Fatalf("after typing: picking=%v iso=%q", m.picking, m.iso)
	}
	m, cmd := m.save()
	if cmd == nil {
		t.Fatalf("save: err=%q", m.err)
	}
	if msg, ok := cmd().(vmUpdatedMsg); !ok {
		t.Fatalf("save gave %+v", msg)
	}
	if saved, err := vm.LoadConfig(storage, "t"); err != nil || saved.CDROMPath != other {
		t.Errorf("saved config: %+v %v", saved, err)
	}
	if got := config.RecentISOs(); !reflect.DeepEqual(got, []string{other}) {
		t.Errorf("remembered = %v", got)
	}
	// The field takes no typing, so j and k move between fields there.
	m, _ = m.Update(key("j"))
	if m.field != editFirmware {
		t.Errorf("j on the ISO field: field=%d", m.field)
	}
}
