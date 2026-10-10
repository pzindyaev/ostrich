package tui

import (
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"

	"github.com/pzindyaev/ostrich/internal/config"
	"github.com/pzindyaev/ostrich/internal/vm"
)

// writeImage makes a 1 MiB file standing in for an ISO.
func writeImage(t *testing.T, path string) string {
	t.Helper()
	if err := os.WriteFile(path, make([]byte, 1<<20), 0o644); err != nil {
		t.Fatal(err)
	}
	return path
}

// saveVM writes cfg as a VM under storage.
func saveVM(t *testing.T, storage string, cfg *vm.VMConfig) {
	t.Helper()
	if err := os.MkdirAll(vm.VMDir(storage, cfg.Name), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := vm.SaveConfig(storage, cfg); err != nil {
		t.Fatal(err)
	}
}

func TestISOEntries(t *testing.T) {
	storage, isos := t.TempDir(), t.TempDir()
	isolateConfig(t, storage)
	a := filepath.Join(isos, "a.iso") // remembered, gone from the host
	b := writeImage(t, filepath.Join(isos, "b.iso"))
	c := writeImage(t, filepath.Join(isos, "c.iso"))
	for _, p := range []string{b, a} {
		if err := config.RememberISO(p); err != nil {
			t.Fatal(err)
		}
	}
	saveVM(t, storage, &vm.VMConfig{Name: "x", CPU: 1, RAM: 64, CDROMPath: b, USBImages: []vm.USBImage{{Path: c}}})
	saveVM(t, storage, &vm.VMConfig{Name: "y", CPU: 1, RAM: 64, CDROMPath: c, USBImages: []vm.USBImage{{Path: c}}})

	got := isoEntries(storage)
	var paths []string
	for _, e := range got {
		paths = append(paths, e.path)
	}
	// Remembered ones first, newest at the top; then what only VMs have.
	if want := []string{a, b, c}; !reflect.DeepEqual(paths, want) {
		t.Fatalf("paths = %v, want %v", paths, want)
	}
	if got[0].usedBy != nil || !reflect.DeepEqual(got[1].usedBy, []string{"x"}) || !reflect.DeepEqual(got[2].usedBy, []string{"x", "y"}) {
		t.Errorf("usedBy = %v %v %v", got[0].usedBy, got[1].usedBy, got[2].usedBy)
	}
	if got[0].state.Err == nil || got[1].state.Size != 1<<20 || got[2].state.Err != nil {
		t.Errorf("states = %+v %+v %+v", got[0].state, got[1].state, got[2].state)
	}
}

func TestISOPicker(t *testing.T) {
	storage, isos := t.TempDir(), t.TempDir()
	isolateConfig(t, storage)
	present := writeImage(t, filepath.Join(isos, "present.iso"))
	gone := filepath.Join(isos, "gone.iso")
	home := writeImage(t, filepath.Join(os.Getenv("HOME"), "home.iso"))
	for _, p := range []string{gone, present} {
		if err := config.RememberISO(p); err != nil {
			t.Fatal(err)
		}
	}
	entries := isoEntries(storage) // present, gone

	p, _ := newISOPicker("(none) — no boot ISO", "", entries)
	if !p.onNone() {
		t.Fatalf("cursor = %d, want the none row", p.cursor)
	}
	view := p.View(100)
	for _, want := range []string{"(none) — no boot ISO", "present.iso", "● 1 MiB", "gone.iso", "✗ not found", "New path"} {
		if !strings.Contains(view, want) {
			t.Errorf("view lacks %q:\n%s", want, view)
		}
	}
	t.Log("\n" + view)

	// Enter on the none row picks nothing; on an entry, its path.
	if _, _, out := p.Update(key("enter")); !out.picked || out.path != "" {
		t.Errorf("none row: %+v", out)
	}
	p, _, _ = p.Update(key("j"))
	if _, _, out := p.Update(key("enter")); !out.picked || out.path != present {
		t.Errorf("entry: %+v", out)
	}
	// A file that is gone is refused with the reason; the dialog stays.
	p, _, _ = p.Update(key("down"))
	p, _, out := p.Update(key("enter"))
	if out.picked || !strings.Contains(p.err, "not found") {
		t.Errorf("gone entry: %+v err=%q", out, p.err)
	}
	// G lands on the input row, clearing the error; letters type there.
	p, _, _ = p.Update(key("G"))
	if !p.onInput() || p.err != "" || !p.input.Focused() {
		t.Fatalf("after G: cursor=%d err=%q focused=%v", p.cursor, p.err, p.input.Focused())
	}
	for _, r := range "jkdgG" {
		p, _, _ = p.Update(key(string(r)))
	}
	if p.input.Value() != "jkdgG" || p.onNone() {
		t.Errorf("typed on the input row: %q cursor=%d", p.input.Value(), p.cursor)
	}
	if !p.typed() {
		t.Error("typed() should report the waiting path")
	}
	// Shift+Tab and Tab move too; leaving the input row blurs it.
	p, _, _ = p.Update(key("shift+tab"))
	if p.onInput() || p.input.Focused() {
		t.Errorf("after shift+tab: cursor=%d focused=%v", p.cursor, p.input.Focused())
	}
	p, _, _ = p.Update(key("tab"))
	if !p.onInput() || !p.input.Focused() {
		t.Errorf("after tab: cursor=%d focused=%v", p.cursor, p.input.Focused())
	}
	// A typed path is resolved: ~ is the home directory.
	p.input.SetValue("")
	for _, r := range "~/home.iso" {
		p, _, _ = p.Update(key(string(r)))
	}
	if _, _, out := p.Update(key("enter")); !out.picked || out.path != home {
		t.Errorf("typed path: %+v", out)
	}
	// An empty one is not.
	p.input.SetValue("  ")
	if p, _, out := p.Update(key("enter")); out.picked || p.err == "" {
		t.Errorf("blank path: %+v err=%q", out, p.err)
	}
	if _, _, out := p.Update(key("esc")); !out.cancelled {
		t.Error("esc should cancel")
	}
	// Off the input row, g goes to the top.
	p, _, _ = p.Update(key("up"))
	p, _, _ = p.Update(key("g"))
	if !p.onNone() {
		t.Errorf("after g: cursor=%d", p.cursor)
	}

	// d forgets the entry under the cursor, here and in the config; the
	// cursor moves to what follows it.
	p, _, _ = p.Update(key("j"))
	p, _, _ = p.Update(key("j"))
	p, _, _ = p.Update(key("d"))
	if len(p.entries) != 1 || p.entries[0].path != present || !p.onInput() {
		t.Errorf("after forget: entries=%+v cursor=%d", p.entries, p.cursor)
	}
	if got := config.RecentISOs(); !reflect.DeepEqual(got, []string{present}) {
		t.Errorf("remembered after forget = %v", got)
	}
	// One a VM has stays, with a word why.
	q, _ := newISOPicker("", "", []isoEntry{{path: present, usedBy: []string{"x"}}})
	q, _, _ = q.Update(key("d"))
	if len(q.entries) != 1 || !strings.Contains(q.err, "stays listed") || !strings.Contains(q.err, "x") {
		t.Errorf("forget in use: entries=%d err=%q", len(q.entries), q.err)
	}

	// The cursor starts on the current image; an unknown one is typed in.
	if q, _ := newISOPicker("(none)", present, entries); q.cursor != 1 {
		t.Errorf("current entry: cursor=%d", q.cursor)
	}
	if q, _ := newISOPicker("(none)", "/elsewhere.iso", entries); !q.onInput() || q.input.Value() != "/elsewhere.iso" {
		t.Errorf("unknown current: cursor=%d input=%q", q.cursor, q.input.Value())
	}
	// With no none row it starts on the first entry; with nothing at all,
	// on the input.
	if q, _ := newISOPicker("", "", entries); q.cursor != 0 || q.input.Focused() {
		t.Errorf("no none row: cursor=%d", q.cursor)
	}
	if q, cmd := newISOPicker("", "", nil); !q.onInput() || cmd == nil {
		t.Errorf("empty: cursor=%d cmd=%v", q.cursor, cmd)
	}
}
