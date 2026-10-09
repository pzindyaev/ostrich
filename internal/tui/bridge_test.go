package tui

import (
	"strings"
	"testing"

	"github.com/pzindyaev/ostrich/internal/vm"
)

// fakeBridgeHint stands in for the host lookup: a fixed hint, with a count of
// how often it was asked.
func fakeBridgeHint(t *testing.T, hint string) *int {
	t.Helper()
	saved := hostBridgeHint
	calls := new(int)
	hostBridgeHint = func(name string) string {
		*calls++
		if name != vm.BridgeName {
			t.Errorf("asked about bridge %q", name)
		}
		return hint
	}
	t.Cleanup(func() { hostBridgeHint = saved })
	return calls
}

const testHint = "tap networking is not set up on this host: the host has no bridge br0.\nRun this, then start the VM:\n\n  sudo nmcli con up br0"

// TestCreateFormBridgeHint: picking tap shows what the host lacks, on the
// network step and again on the confirm step; picking another network hides it.
func TestCreateFormBridgeHint(t *testing.T) {
	fakeBridgeHint(t, testHint)
	m := NewCreateVMModel(vm.NewManager(t.TempDir()), 100, 40)
	for m.step != stepNetwork {
		m, _ = m.Update(press("enter"))
	}
	if v := m.View(); strings.Contains(v, "sudo nmcli") {
		t.Error("hint shown for user networking")
	}
	m, _ = m.Update(press("l")) // user → tap
	v := m.View()
	if !strings.Contains(v, "⚠ tap networking is not set up") || !strings.Contains(v, "sudo nmcli con up br0") {
		t.Errorf("tap step lacks the hint:\n%s", v)
	}
	for m.step != stepConfirm {
		m, _ = m.Update(press("enter"))
	}
	if v := m.View(); !strings.Contains(v, "sudo nmcli con up br0") {
		t.Errorf("confirm step lacks the hint:\n%s", v)
	}
	m.step = stepNetwork
	m, _ = m.Update(press("l")) // tap → none
	if v := m.View(); strings.Contains(v, "sudo nmcli") {
		t.Error("hint still shown after leaving tap")
	}

	// A ready host shows nothing for tap.
	fakeBridgeHint(t, "")
	m, _ = m.Update(press("h")) // none → tap
	if v := m.View(); strings.Contains(v, "⚠") {
		t.Errorf("hint shown for a ready host:\n%s", v)
	}
}

// TestEditFormBridgeHint: a tap VM opens with the hint; switching away hides it.
func TestEditFormBridgeHint(t *testing.T) {
	fakeBridgeHint(t, testHint)
	mgr := vm.NewManager(t.TempDir())
	cfg := &vm.VMConfig{Name: "tapvm", CPU: 2, RAM: 2048, DiskSize: 20,
		Network: vm.NetworkConfig{Type: vm.NetworkTap, MAC: "52:54:00:00:00:01"}}
	m := NewEditVMModel(mgr, cfg, 100, 40)
	if v := m.View(); !strings.Contains(v, "sudo nmcli con up br0") {
		t.Errorf("edit form of a tap VM lacks the hint:\n%s", v)
	}
	for m.field != editNetwork {
		m, _ = m.Update(press("tab"))
	}
	m, _ = m.Update(press("h")) // tap → user
	if v := m.View(); strings.Contains(v, "sudo nmcli") {
		t.Error("hint still shown after switching to user networking")
	}
	m, _ = m.Update(press("l")) // user → tap
	if v := m.View(); !strings.Contains(v, "sudo nmcli con up br0") {
		t.Error("hint not back after switching to tap")
	}
}

// TestFromTemplateBridgeHint: the template wizard behaves like the create one.
func TestFromTemplateBridgeHint(t *testing.T) {
	fakeBridgeHint(t, testHint)
	mgr := vm.NewManager(t.TempDir())
	tpl := &vm.Template{Name: "base", CPU: 1, RAM: 512, DiskSize: 5, Network: vm.NetworkTap}
	m := NewFromTemplateModel(mgr, tpl, 100, 40)
	for m.step != tplStepNetwork {
		m, _ = m.Update(press("enter"))
	}
	if v := m.View(); !strings.Contains(v, "sudo nmcli con up br0") {
		t.Errorf("network step lacks the hint for a tap template:\n%s", v)
	}
	m, _ = m.Update(press("l")) // tap → none
	if v := m.View(); strings.Contains(v, "sudo nmcli") {
		t.Error("hint still shown after leaving tap")
	}
}
