package tui

import (
	"os"
	"os/exec"
	"path/filepath"
	"reflect"
	"strings"
	"testing"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/pzindyaev/ostrich/internal/vm"
)

func TestCreateFormExtraDisks(t *testing.T) {
	m := NewCreateVMModel(vm.NewManager(t.TempDir()), 100, 40)
	m.step = stepDisks
	if !strings.Contains(m.View(), "Additional disks") {
		t.Errorf("step view:\n%s", m.View())
	}
	m.inputs[inputForStep(stepDisks)].SetValue("data:50, 100")
	m, _ = m.Update(press("enter"))
	if m.step != stepISO || m.err != "" {
		t.Fatalf("after enter: step %d err %q", m.step, m.err)
	}
	m.step = stepConfirm
	if v := m.View(); !strings.Contains(v, "Disks:    data:50, disk1:100") {
		t.Errorf("confirm view lacks the disks:\n%s", v)
	}
	cfg, err := m.buildConfig()
	if err != nil {
		t.Fatal(err)
	}
	if want := []vm.Disk{{Name: "data", Size: 50}, {Name: "disk1", Size: 100}}; !reflect.DeepEqual(cfg.Disks, want) {
		t.Errorf("Disks = %+v, want %+v", cfg.Disks, want)
	}

	// A bad entry keeps the wizard on the step and says what is wrong.
	m.step = stepDisks
	m.inputs[inputForStep(stepDisks)].SetValue("disk:5")
	m, _ = m.Update(press("enter"))
	if m.step != stepDisks || !strings.Contains(m.err, "main disk") {
		t.Errorf("bad entry: step %d err %q", m.step, m.err)
	}

	// Blank means no extra disks.
	m.inputs[inputForStep(stepDisks)].SetValue("")
	m, _ = m.Update(press("enter"))
	if m.step != stepISO || m.err != "" {
		t.Errorf("blank: step %d err %q", m.step, m.err)
	}
	m.step = stepConfirm
	if v := m.View(); !strings.Contains(v, "Disks:    (none)") {
		t.Errorf("confirm view with no disks:\n%s", v)
	}
	if cfg, _ := m.buildConfig(); len(cfg.Disks) != 0 {
		t.Errorf("Disks = %+v, want none", cfg.Disks)
	}
}

// notSaved runs the command a refused save returned, if any (moving the cursor
// yields the text input's blink), and fails if it turns out to be the save.
func notSaved(t *testing.T, what string, cmd tea.Cmd) {
	t.Helper()
	if cmd == nil {
		return
	}
	switch msg := cmd().(type) {
	case vmUpdatedMsg, vmUpdateErrMsg:
		t.Fatalf("%s: the save went ahead: %#v", what, msg)
	}
}

// makeExtraDisks lays down the qcow2 images of cfg's additional disks, as
// Manager.Create would have.
func makeExtraDisks(t *testing.T, mgr *vm.Manager, cfg *vm.VMConfig) {
	t.Helper()
	for _, d := range cfg.Disks {
		path := vm.ExtraDiskPath(mgr.StoragePath, cfg.Name, d.Name)
		out, err := exec.Command("qemu-img", "create", "-f", "qcow2", path, "1G").CombinedOutput()
		if err != nil {
			t.Fatalf("qemu-img create: %v\n%s", err, out)
		}
	}
}

func TestEditFormExtraDisks(t *testing.T) {
	mgr := vm.NewManager(t.TempDir())
	cfg := &vm.VMConfig{Name: "deb", CPU: 1, RAM: 512, DiskSize: 1, Network: vm.NetworkConfig{Type: vm.NetworkNone},
		Disks: []vm.Disk{{Name: "data", Size: 1}, {Name: "scratch", Size: 1}}}
	newTestVM(t, mgr, cfg)
	makeExtraDisks(t, mgr, cfg)
	scratch := vm.ExtraDiskPath(mgr.StoragePath, "deb", "scratch")

	m := NewEditVMModel(mgr, cfg, 110, 40)
	if got := m.inputs[editDisks].Value(); got != "data:1, scratch:1" {
		t.Errorf("pre-filled %q", got)
	}
	if !strings.Contains(m.View(), "Extra Disks") {
		t.Errorf("view lacks the field:\n%s", m.View())
	}

	// A bad entry lands on the field.
	m.inputs[editDisks].SetValue("data:1, nope")
	m, cmd := m.Update(ctrlS)
	notSaved(t, "bad entry", cmd)
	if m.field != editDisks || !strings.Contains(m.err, "expected [name:]size") {
		t.Errorf("bad entry: field=%d err=%q", m.field, m.err)
	}
	// So does a size below 1 GiB.
	m.inputs[editDisks].SetValue("data:1, scratch:0")
	m, cmd = m.Update(ctrlS)
	notSaved(t, "zero size", cmd)
	if m.field != editDisks || !strings.Contains(m.err, "at least 1 GiB") {
		t.Errorf("zero size: err=%q", m.err)
	}

	// Removing a disk takes two saves: the first only warns, deleting nothing.
	m.inputs[editDisks].SetValue("data:1")
	m, cmd = m.Update(ctrlS)
	notSaved(t, "first save with a removal", cmd)
	if !m.armed || !strings.Contains(m.warn, "scratch (1 GiB)") || !strings.Contains(m.View(), "press Ctrl-s again") {
		t.Fatalf("first save with a removal: armed=%v warn=%q", m.armed, m.warn)
	}
	if _, err := os.Stat(scratch); err != nil {
		t.Fatalf("scratch deleted before confirmation: %v", err)
	}
	// Editing the field disarms it; a save then warns afresh.
	m, _ = m.Update(press("x"))
	if m.armed || m.warn != "" {
		t.Errorf("still armed after editing the field: %q", m.warn)
	}
	m.inputs[editDisks].SetValue("data:1")
	m, cmd = m.Update(ctrlS)
	notSaved(t, "re-arm", cmd)
	if !m.armed {
		t.Fatal("not re-armed")
	}
	// The second save goes through and the image is gone.
	m, cmd = m.Update(ctrlS)
	if cmd == nil || m.armed || m.warn != "" {
		t.Fatalf("confirmed save: cmd=%v armed=%v warn=%q err=%q", cmd != nil, m.armed, m.warn, m.err)
	}
	if msg := cmd(); reflect.TypeOf(msg) != reflect.TypeOf(vmUpdatedMsg{}) {
		t.Fatalf("save result: %#v", msg)
	}
	if _, err := os.Stat(scratch); !os.IsNotExist(err) {
		t.Errorf("scratch still there after the confirmed removal (%v)", err)
	}
	if _, err := os.Stat(vm.ExtraDiskPath(mgr.StoragePath, "deb", "data")); err != nil {
		t.Errorf("data gone too: %v", err)
	}
	saved, _ := vm.LoadConfig(mgr.StoragePath, "deb")
	if want := []vm.Disk{{Name: "data", Size: 1}}; !reflect.DeepEqual(saved.Disks, want) {
		t.Errorf("saved disks %+v, want %+v", saved.Disks, want)
	}

	// Growing and adding on a stopped VM.
	m = NewEditVMModel(mgr, saved, 110, 40)
	m.inputs[editDisks].SetValue("data:2, logs:1")
	m, cmd = m.Update(ctrlS)
	if cmd == nil || m.armed {
		t.Fatalf("grow+add: cmd=%v err=%q", cmd != nil, m.err)
	}
	if msg := cmd(); reflect.TypeOf(msg) != reflect.TypeOf(vmUpdatedMsg{}) {
		t.Fatalf("grow+add result: %#v", msg)
	}
	if _, err := os.Stat(vm.ExtraDiskPath(mgr.StoragePath, "deb", "logs")); err != nil {
		t.Errorf("logs not created: %v", err)
	}
	saved, _ = vm.LoadConfig(mgr.StoragePath, "deb")
	if want := []vm.Disk{{Name: "data", Size: 2}, {Name: "logs", Size: 1}}; !reflect.DeepEqual(saved.Disks, want) {
		t.Errorf("saved disks %+v, want %+v", saved.Disks, want)
	}

	// On a running VM growing and removing are refused up front; adding goes
	// ahead and is hot-plugged, which fails here for want of a monitor, but
	// the disk is saved and reported as applying at the next start.
	m = NewEditVMModel(mgr, saved, 110, 40)
	m.running = true
	if !strings.Contains(m.View(), "disk removal") {
		t.Errorf("running banner lacks the disk lock:\n%s", m.View())
	}
	m.inputs[editDisks].SetValue("data:2")
	m, cmd = m.Update(ctrlS)
	notSaved(t, "remove while running", cmd)
	if m.field != editDisks || !strings.Contains(m.err, "stop the VM") {
		t.Errorf("remove while running: err=%q", m.err)
	}
	m.inputs[editDisks].SetValue("data:3, logs:1")
	m, cmd = m.Update(ctrlS)
	notSaved(t, "grow while running", cmd)
	if !strings.Contains(m.err, "stop the VM") {
		t.Errorf("grow while running: err=%q", m.err)
	}
	m.inputs[editDisks].SetValue("data:2, logs:1, tmp:1")
	m, cmd = m.Update(ctrlS)
	if cmd == nil {
		t.Fatalf("add while running refused: %q", m.err)
	}
	msg, ok := cmd().(vmUpdateErrMsg)
	if !ok || !strings.Contains(msg.err.Error(), "saved, but") || !strings.Contains(msg.err.Error(), `disk "tmp"`) {
		t.Errorf("add while running without a monitor: %#v", msg)
	}
	if _, err := os.Stat(vm.ExtraDiskPath(mgr.StoragePath, "deb", "tmp")); err != nil {
		t.Errorf("tmp not created: %v", err)
	}
	saved, _ = vm.LoadConfig(mgr.StoragePath, "deb")
	if len(saved.Disks) != 3 || saved.Disks[2].Name != "tmp" {
		t.Errorf("saved disks %+v", saved.Disks)
	}
}

func TestDetailShowsExtraDisks(t *testing.T) {
	storage := t.TempDir()
	cfg := &vm.VMConfig{Name: "t", CPU: 1, RAM: 128, DiskSize: 20,
		Disks: []vm.Disk{{Name: "data", Size: 50}, {Name: "gone", Size: 5}}}
	if err := os.MkdirAll(vm.VMDir(storage, "t"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(vm.ExtraDiskPath(storage, "t", "data"), make([]byte, 1<<20), 0o644); err != nil {
		t.Fatal(err)
	}
	m := NewVMDetailModel(cfg, storage, 100, 40)
	if got, want := m.vp.Height, 40-detailChromeLines-2; got != want {
		t.Errorf("viewport height = %d, want %d", got, want)
	}
	check := func(stage string) {
		view := m.View()
		for _, want := range []string{"Disk:     20 GiB", "data", "50 GiB", "● 1 MiB on host", "gone", "✗ not found"} {
			if !strings.Contains(view, want) {
				t.Errorf("%s: detail view missing %q:\n%s", stage, want, view)
			}
		}
	}
	check("before refresh") // falls back to checking the files itself
	m, _ = m.Update(consoleRefreshedMsg{disks: vm.DiskStates(storage, cfg)})
	check("after refresh")

	// A VM without extra disks shows the main disk alone and keeps its height.
	plain := &vm.VMConfig{Name: "p", CPU: 1, RAM: 128, DiskSize: 8}
	m = NewVMDetailModel(plain, filepath.Join(storage, "x"), 100, 40)
	if m.vp.Height != 40-detailChromeLines {
		t.Errorf("viewport height = %d", m.vp.Height)
	}
	if v := m.View(); !strings.Contains(v, "Disk:     8 GiB") || strings.Contains(v, "on host") {
		t.Errorf("plain view:\n%s", v)
	}
}
