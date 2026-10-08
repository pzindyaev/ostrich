package tui

import (
	"os"
	"path/filepath"
	"strings"
	"testing"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/pzindyaev/ostrich/internal/vm"
)

// applyISO runs the Cmd returned by an attach/detach and feeds its message back.
func applyISO(t *testing.T, m ISOModel, cmd tea.Cmd) ISOModel {
	t.Helper()
	if cmd == nil {
		t.Fatal("expected a command")
	}
	msg, ok := cmd().(isoAppliedMsg)
	if !ok {
		t.Fatalf("unexpected message %T", cmd())
	}
	if msg.err != nil {
		t.Fatalf("apply: %v", msg.err)
	}
	m, _ = m.Update(msg)
	return m
}

func typeKeys(m ISOModel, s string) ISOModel {
	for _, r := range s {
		m, _ = m.Update(key(string(r)))
	}
	return m
}

func TestISOScreenAttachDetach(t *testing.T) {
	storage, isos := t.TempDir(), t.TempDir()
	present := filepath.Join(isos, "virtio-win.iso")
	if err := os.WriteFile(present, make([]byte, 3<<20), 0o644); err != nil {
		t.Fatal(err)
	}
	missing := filepath.Join(isos, "gone.iso")
	cfg := &vm.VMConfig{Name: "t", CPU: 1, RAM: 128, USBImages: []vm.USBImage{{Path: missing}}}
	if err := os.MkdirAll(vm.VMDir(storage, cfg.Name), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := vm.SaveConfig(storage, cfg); err != nil {
		t.Fatal(err)
	}

	m := NewISOModel(cfg, storage, 100, 40)
	m, _ = m.Update(m.Init()())
	view := m.View()
	if !strings.Contains(view, "gone.iso") || !strings.Contains(view, "✗ not found") || !strings.Contains(view, "● stopped") {
		t.Errorf("view should list the missing image:\n%s", view)
	}
	t.Log("\n" + view)

	// Attach by typing a path.
	m, _ = m.Update(key("a"))
	if m.prompt != promptUSBImage || !strings.Contains(m.View(), "Image path") {
		t.Fatalf("a should open the path entry:\n%s", m.View())
	}
	m = typeKeys(m, present)
	m, cmd := m.Update(key("enter"))
	m = applyISO(t, m, cmd)
	if m.prompt != promptNone || len(m.cfg.USBImages) != 2 || m.cfg.USBImages[1].Path != present {
		t.Fatalf("after attach: prompt=%v images=%+v", m.prompt, m.cfg.USBImages)
	}
	saved, err := vm.LoadConfig(storage, "t")
	if err != nil || len(saved.USBImages) != 2 || saved.USBImages[1].Path != present {
		t.Fatalf("saved config: %+v %v", saved, err)
	}
	if !strings.Contains(m.notice, "attached virtio-win.iso") || !strings.Contains(m.notice, "next start") {
		t.Errorf("notice = %q", m.notice)
	}
	m, _ = m.Update(m.Init()())
	view = m.View()
	if !strings.Contains(view, "● 3 MiB") {
		t.Errorf("view should show the image size:\n%s", view)
	}
	t.Log("\n" + view)

	// The same image twice, and a file that is not there, are refused.
	m, _ = m.Update(key("a"))
	m = typeKeys(m, present)
	m, _ = m.Update(key("enter"))
	if !strings.Contains(m.err, "already attached") || m.prompt != promptUSBImage {
		t.Errorf("duplicate: err=%q prompt=%v", m.err, m.prompt)
	}
	m.input.SetValue("")
	m = typeKeys(m, filepath.Join(isos, "nope.iso"))
	m, _ = m.Update(key("enter"))
	if !strings.Contains(m.err, "not found") {
		t.Errorf("missing: err=%q", m.err)
	}
	m, _ = m.Update(key("esc"))
	if m.prompt != promptNone {
		t.Error("esc should cancel the entry")
	}

	// Detach the missing one (row 0) with Space; the present one stays.
	m, _ = m.Update(key("g"))
	m, cmd = m.Update(key(" "))
	m = applyISO(t, m, cmd)
	if len(m.cfg.USBImages) != 1 || m.cfg.USBImages[0].Path != present {
		t.Fatalf("after detach: %+v", m.cfg.USBImages)
	}
	if !strings.Contains(m.notice, "detached gone.iso") {
		t.Errorf("notice = %q", m.notice)
	}
	// Detach the last one with d: the list is empty and the cursor safe.
	m, cmd = m.Update(key("d"))
	m = applyISO(t, m, cmd)
	if len(m.cfg.USBImages) != 0 || m.cursor != 0 {
		t.Fatalf("after last detach: %+v cursor=%d", m.cfg.USBImages, m.cursor)
	}
	if _, cmd = m.Update(key(" ")); cmd != nil {
		t.Error("detach on an empty list must do nothing")
	}
	if !strings.Contains(m.View(), "No images attached") {
		t.Errorf("empty view:\n%s", m.View())
	}
}

func TestISOScreenBootISO(t *testing.T) {
	storage, isos := t.TempDir(), t.TempDir()
	disc := filepath.Join(isos, "debian.iso")
	if err := os.WriteFile(disc, make([]byte, 2<<20), 0o644); err != nil {
		t.Fatal(err)
	}
	cfg := &vm.VMConfig{Name: "t", CPU: 1, RAM: 128}
	if err := os.MkdirAll(vm.VMDir(storage, cfg.Name), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := vm.SaveConfig(storage, cfg); err != nil {
		t.Fatal(err)
	}

	m := NewISOModel(cfg, storage, 100, 40)
	m, _ = m.Update(m.Init()())
	if view := m.View(); !strings.Contains(view, "Boot ISO (CD-ROM drive)") || !strings.Contains(view, "(empty)") {
		t.Errorf("view should show an empty drive:\n%s", view)
	}

	// Nothing to eject yet.
	m, cmd := m.Update(key("e"))
	if cmd != nil || !strings.Contains(m.notice, "already empty") {
		t.Errorf("eject on empty drive: cmd=%v notice=%q", cmd, m.notice)
	}

	// Put a disc in.
	m, _ = m.Update(key("c"))
	if m.prompt != promptBootISO || !strings.Contains(m.View(), "Boot ISO path") {
		t.Fatalf("c should open the boot ISO prompt:\n%s", m.View())
	}
	m = typeKeys(m, disc)
	m, cmd = m.Update(key("enter"))
	m = applyISO(t, m, cmd)
	if m.prompt != promptNone || m.cfg.CDROMPath != disc {
		t.Fatalf("after insert: prompt=%v cdrom=%q", m.prompt, m.cfg.CDROMPath)
	}
	if saved, err := vm.LoadConfig(storage, "t"); err != nil || saved.CDROMPath != disc {
		t.Fatalf("saved config: %+v %v", saved, err)
	}
	if !strings.Contains(m.notice, "inserted debian.iso") || !strings.Contains(m.notice, "next start") {
		t.Errorf("notice = %q", m.notice)
	}
	m, _ = m.Update(m.Init()())
	view := m.View()
	if !strings.Contains(view, disc) || !strings.Contains(view, "● 2 MiB") {
		t.Errorf("view should show the disc and its size:\n%s", view)
	}
	t.Log("\n" + view)

	// A path that is not there is refused and the prompt stays open.
	m, _ = m.Update(key("c"))
	m = typeKeys(m, filepath.Join(isos, "nope.iso"))
	m, _ = m.Update(key("enter"))
	if !strings.Contains(m.err, "not found") || m.prompt != promptBootISO {
		t.Errorf("missing: err=%q prompt=%v", m.err, m.prompt)
	}
	m, _ = m.Update(key("esc"))

	// Eject.
	m, cmd = m.Update(key("e"))
	m = applyISO(t, m, cmd)
	if m.cfg.CDROMPath != "" || !strings.Contains(m.notice, "ejected") {
		t.Errorf("after eject: cdrom=%q notice=%q", m.cfg.CDROMPath, m.notice)
	}
	if saved, _ := vm.LoadConfig(storage, "t"); saved.CDROMPath != "" {
		t.Errorf("eject not saved: %+v", saved)
	}
	if !strings.Contains(m.View(), "(empty)") {
		t.Errorf("view after eject:\n%s", m.View())
	}
}

func TestDetailShowsBootISO(t *testing.T) {
	isos := t.TempDir()
	disc := filepath.Join(isos, "debian.iso")
	if err := os.WriteFile(disc, make([]byte, 1<<20), 0o644); err != nil {
		t.Fatal(err)
	}
	cfg := &vm.VMConfig{Name: "t", CPU: 1, RAM: 128, CDROMPath: disc}
	m := NewVMDetailModel(cfg, t.TempDir(), 100, 40)
	m, _ = m.Update(consoleRefreshedMsg{cdrom: vm.ImageStateOf(disc)})
	if view := m.View(); !strings.Contains(view, disc) || !strings.Contains(view, "● 1 MiB") {
		t.Errorf("detail view should show the boot ISO and its size:\n%s", view)
	}
	cfg.CDROMPath = filepath.Join(isos, "gone.iso")
	m = NewVMDetailModel(cfg, t.TempDir(), 100, 40)
	if view := m.View(); !strings.Contains(view, "✗ not found") {
		t.Errorf("detail view should flag a missing boot ISO:\n%s", view)
	}
}

func TestResolveImagePath(t *testing.T) {
	dir := t.TempDir()
	p := filepath.Join(dir, "a.iso")
	if err := os.WriteFile(p, []byte("x"), 0o644); err != nil {
		t.Fatal(err)
	}
	if img, err := resolveImagePath("  " + p + "  "); err != nil || img.Path != p {
		t.Errorf("absolute: %+v %v", img, err)
	}
	t.Setenv("HOME", dir)
	if img, err := resolveImagePath("~/a.iso"); err != nil || img.Path != p {
		t.Errorf("tilde: %+v %v", img, err)
	}
	if _, err := resolveImagePath(""); err == nil {
		t.Error("empty path accepted")
	}
	if _, err := resolveImagePath(dir); err == nil {
		t.Error("directory accepted")
	}
}

func TestDetailShowsUSBImages(t *testing.T) {
	isos := t.TempDir()
	present := filepath.Join(isos, "virtio-win.iso")
	if err := os.WriteFile(present, make([]byte, 1<<20), 0o644); err != nil {
		t.Fatal(err)
	}
	cfg := &vm.VMConfig{Name: "t", CPU: 1, RAM: 128, USBImages: []vm.USBImage{
		{Path: present},
		{Path: filepath.Join(isos, "gone.iso")},
	}}
	m := NewVMDetailModel(cfg, t.TempDir(), 100, 40)
	if got, want := m.vp.Height, 40-detailChromeLines-1; got != want {
		t.Errorf("viewport height = %d, want %d", got, want)
	}
	m, _ = m.Update(consoleRefreshedMsg{images: vm.USBImageStates(cfg.USBImages)})
	view := m.View()
	for _, want := range []string{"USB ISO:", "virtio-win.iso", "● 1 MiB", "gone.iso", "✗ not found", "i: ISO hot-plug"} {
		if !strings.Contains(view, want) {
			t.Errorf("detail view missing %q", want)
		}
	}
	t.Log("\n" + view)
}

func TestTruncateLeftAndHumanSize(t *testing.T) {
	if got := truncateLeft("/a/b/c.iso", 6); got != "…c.iso" {
		t.Errorf("truncateLeft = %q", got)
	}
	if got := truncateLeft("short", 10); got != "short" {
		t.Errorf("truncateLeft short = %q", got)
	}
	for n, want := range map[int64]string{512: "512 B", 2048: "2 KiB", 5 << 20: "5 MiB", 3 << 30: "3.0 GiB", 1610612736: "1.5 GiB"} {
		if got := humanSize(n); got != want {
			t.Errorf("humanSize(%d) = %q, want %q", n, got, want)
		}
	}
}
