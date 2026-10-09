package tui

import (
	"os"
	"os/exec"
	"strconv"
	"strings"
	"testing"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/pzindyaev/ostrich/internal/config"
	"github.com/pzindyaev/ostrich/internal/vm"
)

// newTestVM lays down a VM directory with a small qcow2 disk and vm.yaml.
func newTestVM(t *testing.T, mgr *vm.Manager, cfg *vm.VMConfig) {
	t.Helper()
	if _, err := exec.LookPath("qemu-img"); err != nil {
		t.Skip("qemu-img not installed")
	}
	if err := os.MkdirAll(vm.VMDir(mgr.StoragePath, cfg.Name), 0o755); err != nil {
		t.Fatal(err)
	}
	out, err := exec.Command("qemu-img", "create", "-f", "qcow2", vm.DiskPath(mgr.StoragePath, cfg.Name), "64M").CombinedOutput()
	if err != nil {
		t.Fatalf("qemu-img create: %v\n%s", err, out)
	}
	if err := vm.SaveConfig(mgr.StoragePath, cfg); err != nil {
		t.Fatal(err)
	}
}

// runCmd runs a Cmd, which may be a batch of the spinner's tick and the
// copy, and returns the copy's result message.
func runCmd(t *testing.T, cmd tea.Cmd) tea.Msg {
	t.Helper()
	if cmd == nil {
		t.Fatal("expected a command")
	}
	msg := cmd()
	batch, ok := msg.(tea.BatchMsg)
	if !ok {
		return msg
	}
	for _, c := range batch {
		switch m := c().(type) {
		case templateSavedMsg, templateSaveErrMsg, vmCreatedMsg, vmCreateErrMsg:
			return m
		}
	}
	t.Fatal("batch carried no result message")
	return nil
}

var ctrlS = tea.KeyMsg{Type: tea.KeyCtrlS}

func navTarget(t *testing.T, cmd tea.Cmd) NavigateMsg {
	t.Helper()
	if cmd == nil {
		t.Fatal("expected a navigation command")
	}
	nav, ok := cmd().(NavigateMsg)
	if !ok {
		t.Fatalf("got %T, want NavigateMsg", cmd())
	}
	return nav
}

func TestSaveTemplateScreen(t *testing.T) {
	mgr := vm.NewManager(t.TempDir())
	cfg := &vm.VMConfig{Name: "deb", CPU: 2, RAM: 2048, DiskSize: 20, Firmware: vm.FirmwareUEFI, TPM: true,
		Network: vm.NetworkConfig{Type: vm.NetworkUser, MAC: "52:54:00:00:00:01"}, VNCPort: 1}
	newTestVM(t, mgr, cfg)

	m := NewSaveTemplateModel(mgr, cfg, 110, 40)
	view := m.View()
	for _, want := range []string{"Save as Template: deb", "● stopped", "UEFI, TPM 2.0", "UEFI NVRAM", "TPM state", "20 GiB virtual", "Template name", "Description", "Save template"} {
		if !strings.Contains(view, want) {
			t.Errorf("view lacks %q", want)
		}
	}
	t.Log("\n" + view)
	if m.value(saveTplName) != "deb" {
		t.Errorf("suggested name = %q, want the VM's", m.value(saveTplName))
	}

	// A bad name is refused on the spot.
	m.inputs[saveTplName].SetValue("bad name!")
	m, _ = m.Update(ctrlS)
	if !strings.Contains(m.err, "letters, digits") || m.field != saveTplName {
		t.Errorf("bad name: err=%q field=%d", m.err, m.field)
	}

	// Name, then description, then the button: Enter walks down and saves.
	m.inputs[saveTplName].SetValue("deb-base")
	m, _ = m.Update(press("enter"))
	if m.field != saveTplDescription {
		t.Fatalf("field = %d, want description", m.field)
	}
	for _, r := range "golden image" {
		m, _ = m.Update(press(string(r)))
	}
	m, _ = m.Update(press("enter"))
	if m.field != saveTplButton {
		t.Fatalf("field = %d, want the button", m.field)
	}
	m, cmd := m.Update(press("enter"))
	if !m.busy || cmd == nil {
		t.Fatalf("save should start the copy: busy=%v cmd=%v", m.busy, cmd)
	}
	if view := m.View(); !strings.Contains(view, "Copying the disk image") || !strings.Contains(view, "Please wait") {
		t.Errorf("busy view:\n%s", view)
	}
	// Keys are ignored while the copy runs.
	if m2, c := m.Update(press("esc")); c != nil || !m2.busy {
		t.Error("esc should do nothing while busy")
	}
	msg := runCmd(t, cmd)
	saved, ok := msg.(templateSavedMsg)
	if !ok {
		t.Fatalf("got %T %v, want templateSavedMsg", msg, msg)
	}
	if saved.name != "deb-base" {
		t.Errorf("saved %q", saved.name)
	}
	m, cmd = m.Update(msg)
	if nav := navTarget(t, cmd); nav.To != screenTemplates {
		t.Errorf("after saving: navigate to %d, want templates", nav.To)
	}
	tpl, err := vm.LoadTemplate(mgr.StoragePath, "deb-base")
	if err != nil {
		t.Fatal(err)
	}
	if tpl.Description != "golden image" || tpl.SourceVM != "deb" || !tpl.VNC || tpl.Firmware != vm.FirmwareUEFI || !tpl.TPM {
		t.Errorf("template = %+v", tpl)
	}

	// The same name again is refused before anything is copied.
	m = NewSaveTemplateModel(mgr, cfg, 110, 40)
	m.inputs[saveTplName].SetValue("deb-base")
	m, cmd = m.Update(ctrlS)
	if m.busy || !strings.Contains(m.err, "already exists") {
		t.Errorf("duplicate: busy=%v err=%q", m.busy, m.err)
	}

	// A running VM is refused, and the screen says so up front.
	if err := os.WriteFile(vm.PIDPath(mgr.StoragePath, "deb"), []byte(strconv.Itoa(os.Getpid())), 0o644); err != nil {
		t.Fatal(err)
	}
	m = NewSaveTemplateModel(mgr, cfg, 110, 40)
	if !m.running || !strings.Contains(m.View(), "● running") || !strings.Contains(m.View(), "shut it down from inside the guest") {
		t.Errorf("running VM not flagged:\n%s", m.View())
	}
	m.inputs[saveTplName].SetValue("deb-running")
	m, _ = m.Update(ctrlS)
	if m.busy || !strings.Contains(m.err, "running") {
		t.Errorf("running VM accepted: busy=%v err=%q", m.busy, m.err)
	}
	if mgr.TemplateExists("deb-running") {
		t.Error("template made from a running VM")
	}

	// Esc goes back to the VM.
	if nav := navTarget(t, func() tea.Cmd { _, c := m.Update(press("esc")); return c }()); nav.To != screenDetail || nav.VMName != "deb" {
		t.Errorf("esc: %+v", nav)
	}
}

func TestTemplatesScreen(t *testing.T) {
	mgr := vm.NewManager(t.TempDir())
	m := NewTemplatesModel(mgr, 120, 40)
	m, _ = m.Update(m.Init()())
	if view := m.View(); !strings.Contains(view, "No templates yet") {
		t.Errorf("empty view:\n%s", view)
	}
	// Enter and d do nothing on an empty list.
	if _, cmd := m.Update(key("enter")); cmd != nil {
		t.Error("enter on an empty list navigated")
	}
	if m, _ = m.Update(key("d")); m.confirming {
		t.Error("d on an empty list asked for confirmation")
	}

	newTestVM(t, mgr, &vm.VMConfig{Name: "a", CPU: 1, RAM: 512, DiskSize: 8})
	newTestVM(t, mgr, &vm.VMConfig{Name: "b", CPU: 4, RAM: 4096, DiskSize: 32, SecureBoot: true, TPM: true,
		Network: vm.NetworkConfig{Type: vm.NetworkTap}})
	for _, p := range [][3]string{{"a", "alpha", "first"}, {"b", "beta", ""}} {
		if err := mgr.CreateTemplate(p[0], p[1], p[2]); err != nil {
			t.Fatal(err)
		}
	}

	m, _ = m.Update(key("r"))
	m, _ = m.Update(loadTemplatesCmd(mgr)())
	view := m.View()
	for _, want := range []string{"alpha", "beta", "CPU: 4", "RAM: 4096 MiB", "Disk: 32 GiB", "UEFI + Secure Boot, TPM 2.0", "tap", "About:    first", "From VM:  a", "on the host"} {
		if !strings.Contains(view, want) {
			t.Errorf("view lacks %q", want)
		}
	}
	t.Log("\n" + view)

	// Enter opens the wizard for the template under the cursor.
	m, _ = m.Update(key("j"))
	_, cmd := m.Update(key("enter"))
	if nav := navTarget(t, cmd); nav.To != screenFromTemplate || nav.Template != "beta" {
		t.Errorf("enter: %+v", nav)
	}
	if !strings.Contains(m.View(), "(no description)") {
		t.Errorf("beta's info should say it has no description:\n%s", m.View())
	}

	// Delete asks first; any other key cancels; y deletes.
	m, _ = m.Update(key("d"))
	if !m.confirming || !strings.Contains(m.View(), `Delete template "beta"`) {
		t.Fatalf("d should ask: confirming=%v\n%s", m.confirming, m.View())
	}
	m, _ = m.Update(key("n"))
	if m.confirming || !mgr.TemplateExists("beta") {
		t.Error("n should cancel the delete")
	}
	m, _ = m.Update(key("d"))
	m, cmd = m.Update(key("y"))
	if cmd == nil {
		t.Fatal("y should delete")
	}
	msg := cmd()
	if _, ok := msg.(templateActionOKMsg); !ok {
		t.Fatalf("delete: %T %v", msg, msg)
	}
	m, cmd = m.Update(msg)
	m, _ = m.Update(cmd())
	if mgr.TemplateExists("beta") || len(m.tpls) != 1 || m.cursor != 0 {
		t.Errorf("after delete: exists=%v tpls=%d cursor=%d", mgr.TemplateExists("beta"), len(m.tpls), m.cursor)
	}
	if !strings.Contains(m.View(), "✓ deleted template beta") {
		t.Errorf("no notice:\n%s", m.View())
	}

	// Back to the list.
	if nav := navTarget(t, func() tea.Cmd { _, c := m.Update(key("q")); return c }()); nav.To != screenList {
		t.Errorf("q: %+v", nav)
	}
}

func TestFromTemplateWizard(t *testing.T) {
	mgr := vm.NewManager(t.TempDir())
	src := &vm.VMConfig{Name: "win", CPU: 4, RAM: 8192, DiskSize: 64, Firmware: vm.FirmwareUEFI, SecureBoot: true, TPM: true,
		Network: vm.NetworkConfig{Type: vm.NetworkUser, MAC: "52:54:00:00:00:01",
			PortForwards: []vm.PortForward{{Host: 3389, Guest: 3389, Proto: "tcp"}}}, VNCPort: 1}
	newTestVM(t, mgr, src)
	if err := mgr.CreateTemplate("win", "win-base", ""); err != nil {
		t.Fatal(err)
	}
	tpl, err := vm.LoadTemplate(mgr.StoragePath, "win-base")
	if err != nil {
		t.Fatal(err)
	}

	m := NewFromTemplateModel(mgr, tpl, 110, 40)
	if got := m.value(tplStepName); got != "win-base-1" {
		t.Errorf("suggested name = %q", got)
	}
	if m.value(tplStepCPU) != "4" || m.value(tplStepRAM) != "8192" || networkChoices[m.netIdx] != vm.NetworkUser {
		t.Errorf("defaults not from the template: cpu=%s ram=%s net=%d", m.value(tplStepCPU), m.value(tplStepRAM), m.netIdx)
	}
	if m.value(tplStepForwards) != "" {
		t.Errorf("port forwards should start blank, got %q", m.value(tplStepForwards))
	}

	// Walk the steps: keep the name, halve the resources, pick tap, then
	// try forwards with tap (refused), go back to user, set forwards.
	for s := tplStep(0); s < tplStepConfirm; s++ {
		if m.step != s {
			t.Fatalf("at step %d, want %d (err %q)", m.step, s, m.err)
		}
		if v := m.View(); !strings.Contains(v, tplStepLabels[s]) || !strings.Contains(v, "64 GiB disk, UEFI + Secure Boot, TPM 2.0") {
			t.Errorf("step %d view:\n%s", s, v)
		}
		switch s {
		case tplStepCPU:
			m.inputs[tplInputForStep(s)].SetValue("2")
		case tplStepRAM:
			m.inputs[tplInputForStep(s)].SetValue("4096")
		case tplStepNetwork:
			m, _ = m.Update(press("l")) // user → tap
		case tplStepForwards:
			m.inputs[tplInputForStep(s)].SetValue("2222:22")
			m, _ = m.Update(press("enter"))
			if m.step != tplStepForwards || !strings.Contains(m.err, "user (NAT)") {
				t.Fatalf("forwards with tap accepted: step=%d err=%q", m.step, m.err)
			}
			m, _ = m.Update(tea.KeyMsg{Type: tea.KeyShiftTab}) // k would type into the field

			if m.step != tplStepNetwork {
				t.Fatalf("shift+tab: step=%d", m.step)
			}
			m, _ = m.Update(press("h")) // tap → user
			m, _ = m.Update(press("enter"))
			m.inputs[tplInputForStep(tplStepForwards)].SetValue("2222:22")
		}
		m, _ = m.Update(press("enter"))
	}
	if m.step != tplStepConfirm {
		t.Fatalf("step=%d err=%q", m.step, m.err)
	}
	view := m.View()
	for _, want := range []string{"Name:     win-base-1", "Template: win-base", "CPU:      2 cores", "RAM:      4096 MiB", "64 GiB (copied from the template)", "UEFI + Secure Boot, TPM 2.0", "user [tcp:2222:22]", "display 2 (port 5902)"} {
		if !strings.Contains(view, want) {
			t.Errorf("confirm view lacks %q", want)
		}
	}
	t.Log("\n" + view)

	// Enter creates the VM; the UI is busy until the copy is done.
	m, cmd := m.Update(press("enter"))
	if !m.busy || cmd == nil {
		t.Fatalf("create should start: busy=%v", m.busy)
	}
	if !strings.Contains(m.View(), "Copying the disk image") {
		t.Errorf("busy view:\n%s", m.View())
	}
	msg := runCmd(t, cmd)
	if _, ok := msg.(vmCreatedMsg); !ok {
		t.Fatalf("got %T %v", msg, msg)
	}
	m, cmd = m.Update(msg)
	if nav := navTarget(t, cmd); nav.To != screenList {
		t.Errorf("after create: %+v", nav)
	}
	clone, err := vm.LoadConfig(mgr.StoragePath, "win-base-1")
	if err != nil {
		t.Fatal(err)
	}
	if clone.CPU != 2 || clone.RAM != 4096 || clone.Network.Type != vm.NetworkUser || clone.VNCPort != 2 ||
		len(clone.Network.PortForwards) != 1 || clone.Network.PortForwards[0].Host != 2222 {
		t.Errorf("clone = %+v", clone)
	}
	if clone.DiskSize != 64 || !clone.SecureBoot || !clone.TPM || clone.Network.MAC == src.Network.MAC || clone.Network.MAC == "" {
		t.Errorf("clone machine = %+v", clone)
	}

	// The next wizard suggests the next free name and display.
	m = NewFromTemplateModel(mgr, tpl, 110, 40)
	if got := m.value(tplStepName); got != "win-base-2" {
		t.Errorf("suggested name = %q", got)
	}
	cfg, err := m.buildConfig()
	if err != nil || cfg.VNCPort != 3 {
		t.Errorf("second clone VNC = %d (%v), want 3", cfg.VNCPort, err)
	}
	// A taken name is refused.
	m.inputs[0].SetValue("win")
	m, _ = m.Update(press("enter"))
	if m.step != tplStepName || !strings.Contains(m.err, "already exists") {
		t.Errorf("taken name: step=%d err=%q", m.step, m.err)
	}
	// Esc returns to the templates list.
	if nav := navTarget(t, func() tea.Cmd { _, c := m.Update(press("esc")); return c }()); nav.To != screenTemplates {
		t.Errorf("esc: %+v", nav)
	}
}

func TestListAndDetailOfferTemplates(t *testing.T) {
	mgr := vm.NewManager(t.TempDir())
	cfg := &vm.VMConfig{Name: "deb", CPU: 1, RAM: 512, DiskSize: 8}
	list := NewVMListModel(mgr, 120, 40)
	list, _ = list.Update(vmListLoadedMsg{cfgs: []*vm.VMConfig{cfg}, statuses: map[string]vm.VMStatus{}})
	if v := list.View(); !strings.Contains(v, "t: save as template") || !strings.Contains(v, "T: templates") {
		t.Errorf("list help lacks the template keys:\n%s", v)
	}
	if nav := navTarget(t, func() tea.Cmd { _, c := list.Update(key("t")); return c }()); nav.To != screenTemplateSave || nav.VMName != "deb" {
		t.Errorf("t: %+v", nav)
	}
	if nav := navTarget(t, func() tea.Cmd { _, c := list.Update(key("T")); return c }()); nav.To != screenTemplates {
		t.Errorf("T: %+v", nav)
	}
	empty := NewVMListModel(mgr, 120, 40)
	empty, _ = empty.Update(vmListLoadedMsg{})
	if _, c := empty.Update(key("t")); c != nil {
		t.Error("t with no VMs navigated")
	}

	detail := NewVMDetailModel(cfg, mgr.StoragePath, 120, 40)
	if v := detail.View(); !strings.Contains(v, "t: save as template") {
		t.Errorf("detail help lacks the template key:\n%s", v)
	}
	if nav := navTarget(t, detail.handleKey(key("t"))); nav.To != screenTemplateSave || nav.VMName != "deb" {
		t.Errorf("detail t: %+v", nav)
	}
}

// TestAppNavigatesTemplateScreens drives the root model through the new
// screens, so the wiring in app.go is covered, not only the sub-models.
func TestAppNavigatesTemplateScreens(t *testing.T) {
	mgr := vm.NewManager(t.TempDir())
	newTestVM(t, mgr, &vm.VMConfig{Name: "deb", CPU: 1, RAM: 512, DiskSize: 8})
	if err := mgr.CreateTemplate("deb", "deb-base", ""); err != nil {
		t.Fatal(err)
	}
	app := App{appCfg: &config.AppConfig{VMStoragePath: mgr.StoragePath}, vmMgr: mgr, screen: screenList, width: 100, height: 40}

	nav := func(msg NavigateMsg) App {
		model, cmd := app.Update(msg)
		app = model.(App)
		if cmd != nil {
			if m := cmd(); m != nil {
				model, _ = app.Update(m)
				app = model.(App)
			}
		}
		return app
	}

	app = nav(NavigateMsg{To: screenTemplates})
	if app.screen != screenTemplates || !strings.Contains(app.View(), "deb-base") {
		t.Errorf("templates screen:\n%s", app.View())
	}
	app = nav(NavigateMsg{To: screenFromTemplate, Template: "deb-base"})
	if app.screen != screenFromTemplate || !strings.Contains(app.View(), "New VM from Template: deb-base") {
		t.Errorf("wizard screen:\n%s", app.View())
	}
	// A template that is gone lands back on the templates list.
	app = nav(NavigateMsg{To: screenFromTemplate, Template: "nope"})
	if app.screen != screenTemplates {
		t.Errorf("missing template: screen=%d", app.screen)
	}
	app = nav(NavigateMsg{To: screenTemplateSave, VMName: "deb"})
	if app.screen != screenTemplateSave || !strings.Contains(app.View(), "Save as Template: deb") {
		t.Errorf("save screen:\n%s", app.View())
	}
	// A resize reaches the active screen.
	model, _ := app.Update(tea.WindowSizeMsg{Width: 80, Height: 30})
	app = model.(App)
	if app.saveTpl.width != 80 {
		t.Errorf("save screen width = %d after resize", app.saveTpl.width)
	}
	app = nav(NavigateMsg{To: screenTemplateSave, VMName: "nope"})
	if app.screen != screenList {
		t.Errorf("missing VM: screen=%d", app.screen)
	}
}
