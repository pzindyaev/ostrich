package tui

import (
	"os"
	"strings"
	"testing"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/pzindyaev/ostrich/internal/vm"
)

func key(s string) tea.KeyMsg {
	switch s {
	case " ":
		return tea.KeyMsg{Type: tea.KeySpace, Runes: []rune{' '}}
	case "enter":
		return tea.KeyMsg{Type: tea.KeyEnter}
	case "esc":
		return tea.KeyMsg{Type: tea.KeyEsc}
	case "tab":
		return tea.KeyMsg{Type: tea.KeyTab}
	case "shift+tab":
		return tea.KeyMsg{Type: tea.KeyShiftTab}
	case "up":
		return tea.KeyMsg{Type: tea.KeyUp}
	case "down":
		return tea.KeyMsg{Type: tea.KeyDown}
	}
	return tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune(s)}
}

func testHostDevs() []vm.HostUSBDevice {
	return []vm.HostUSBDevice{
		{VendorID: "046d", ProductID: "085c", Product: "C922 Pro Stream Webcam", Port: "3-2.2.2", DevNode: "/dev/bus/usb/003/016", Writable: true},
		{VendorID: "0781", ProductID: "5583", Manufacturer: "SanDisk", Product: "Ultra Fit", Port: "3-2.3", DevNode: "/dev/bus/usb/003/004", Writable: true},
		{VendorID: "0781", ProductID: "5583", Manufacturer: "SanDisk", Product: "Ultra Fit", Port: "3-2.4", DevNode: "/dev/bus/usb/003/005", Writable: false},
	}
}

// apply runs the Cmd returned by a toggle and feeds its message back.
func apply(t *testing.T, m USBModel, cmd tea.Cmd) USBModel {
	t.Helper()
	if cmd == nil {
		t.Fatal("expected a command")
	}
	msg, ok := cmd().(usbAppliedMsg)
	if !ok {
		t.Fatalf("unexpected message %T", cmd())
	}
	if msg.err != nil {
		t.Fatalf("apply: %v", msg.err)
	}
	m, _ = m.Update(msg)
	return m
}

func TestUSBPickerAttachDetach(t *testing.T) {
	storage := t.TempDir()
	cfg := &vm.VMConfig{Name: "t", CPU: 1, RAM: 128, USBDevices: []vm.USBDevice{{VendorID: "dead", ProductID: "beef", Name: "Old Dongle"}}}
	if err := os.MkdirAll(vm.VMDir(storage, cfg.Name), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := vm.SaveConfig(storage, cfg); err != nil {
		t.Fatal(err)
	}

	m := NewUSBModel(cfg, storage, 100, 40)
	m, _ = m.Update(usbScannedMsg{devs: testHostDevs()})
	if len(m.rows) != 4 || m.rows[3].host != nil || m.rows[3].cfgIdx != 0 {
		t.Fatalf("rows = %+v", m.rows)
	}
	t.Log("\n" + m.View())

	// Attach the webcam (cursor at row 0).
	m, cmd := m.Update(key(" "))
	m = apply(t, m, cmd)
	if len(m.cfg.USBDevices) != 2 || m.cfg.USBDevices[1].ID() != "046d:085c" || m.cfg.USBDevices[1].Port != "" {
		t.Fatalf("after attach: %+v", m.cfg.USBDevices)
	}
	saved, err := vm.LoadConfig(storage, "t")
	if err != nil || len(saved.USBDevices) != 2 {
		t.Fatalf("saved config: %+v %v", saved, err)
	}

	// Attach the second of two identical sticks: it gets pinned to its port.
	m, _ = m.Update(key("j"))
	m, _ = m.Update(key("j"))
	m, cmd = m.Update(key("enter"))
	m = apply(t, m, cmd)
	if d := m.cfg.USBDevices[2]; d.ID() != "0781:5583" || d.Port != "3-2.4" || d.Name != "SanDisk Ultra Fit" {
		t.Fatalf("pinned stick: %+v", d)
	}
	view := m.View()
	if !strings.Contains(view, "no access") || !strings.Contains(view, `'ATTR{idProduct}=="5583",'`) ||
		!strings.Contains(view, "y: copy udev command") {
		t.Errorf("view should warn about access and offer the fix:\n%s", view)
	}
	t.Log("\n" + view)

	// Detach the old dongle (last row) and check the row disappears.
	m, _ = m.Update(key("G"))
	m, cmd = m.Update(key(" "))
	m = apply(t, m, cmd)
	if len(m.cfg.USBDevices) != 2 || len(m.rows) != 3 {
		t.Fatalf("after detach: %+v rows=%d", m.cfg.USBDevices, len(m.rows))
	}
	if !strings.Contains(m.notice, "detached Old Dongle") {
		t.Errorf("notice = %q", m.notice)
	}

	// Manual entry by ID.
	m, _ = m.Update(key("a"))
	for _, r := range "1a2b:3c4d" {
		m, _ = m.Update(key(string(r)))
	}
	m, cmd = m.Update(key("enter"))
	m = apply(t, m, cmd)
	if d := m.cfg.USBDevices[2]; d.ID() != "1a2b:3c4d" || m.adding {
		t.Fatalf("manual add: %+v adding=%v", d, m.adding)
	}
	m, _ = m.Update(key("a"))
	for _, r := range "1a2b:3c4d" {
		m, _ = m.Update(key(string(r)))
	}
	m, _ = m.Update(key("enter"))
	if !strings.Contains(m.err, "already") {
		t.Errorf("duplicate manual add err = %q", m.err)
	}
	t.Log("\n" + m.View())
}

func TestDetailShowsUSB(t *testing.T) {
	cfg := &vm.VMConfig{Name: "t", CPU: 1, RAM: 128, USBDevices: []vm.USBDevice{
		{VendorID: "046d", ProductID: "085c", Name: "C922 Pro Stream Webcam"},
		{VendorID: "dead", ProductID: "beef"},
	}}
	m := NewVMDetailModel(cfg, t.TempDir(), 100, 40)
	if got, want := m.vp.Height, 40-detailChromeLines-1; got != want {
		t.Errorf("viewport height = %d, want %d", got, want)
	}
	host := testHostDevs()
	m, _ = m.Update(consoleRefreshedMsg{usb: vm.MatchUSB(cfg.USBDevices, host)})
	view := m.View()
	for _, want := range []string{"● connected", "○ not connected", "dead:beef", "u: USB"} {
		if !strings.Contains(view, want) {
			t.Errorf("detail view missing %q", want)
		}
	}
	t.Log("\n" + view)
}

func TestShellLines(t *testing.T) {
	words := vm.UdevRuleCommandWords(vm.USBDevice{VendorID: "046d", ProductID: "085c"})
	for _, width := range []int{40, 72, 100, 1000} {
		lines := shellLines(words, width)
		for i, l := range lines {
			if len(l) > width && width >= 40 {
				t.Errorf("width %d: line %d too long (%d): %s", width, i, len(l), l)
			}
			if last := i == len(lines)-1; strings.HasSuffix(l, " \\") == last {
				t.Errorf("width %d: line %d continuation wrong: %q", width, i, l)
			}
		}
		// Undoing the continuations must give back the exact one-liner.
		joined := strings.ReplaceAll(strings.Join(lines, "\n"), " \\\n", " ")
		if joined != vm.UdevRuleCommand(vm.USBDevice{VendorID: "046d", ProductID: "085c"}) {
			t.Errorf("width %d: lines do not reassemble:\n%s", width, joined)
		}
	}
	if got := shellLines(words, 1000); len(got) != 1 {
		t.Errorf("wide screen should give one line, got %d", len(got))
	}
}
