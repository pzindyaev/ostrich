package tui

import (
	"fmt"
	"strings"

	"github.com/charmbracelet/bubbles/textinput"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/pzindyaev/ostrich/internal/config"
	"github.com/pzindyaev/ostrich/internal/vm"
)

// isoEntry is one image the picker offers: a path used before, with what the
// host shows for it now and the VMs that have it in their config.
type isoEntry struct {
	path   string
	state  vm.ImageState
	usedBy []string
}

// isoEntries lists the images to offer: the paths remembered in the app
// config, newest first, then any other image a VM under storagePath has as
// its boot ISO or a USB drive. Each is checked on the host.
func isoEntries(storagePath string) []isoEntry {
	var entries []isoEntry
	index := map[string]int{}
	add := func(path, vmName string) {
		if path == "" {
			return
		}
		i, ok := index[path]
		if !ok {
			i = len(entries)
			index[path] = i
			entries = append(entries, isoEntry{path: path})
		}
		if vmName != "" && !contains(entries[i].usedBy, vmName) {
			entries[i].usedBy = append(entries[i].usedBy, vmName)
		}
	}
	for _, p := range config.RecentISOs() {
		add(p, "")
	}
	cfgs, _ := vm.NewManager(storagePath).List()
	for _, cfg := range cfgs {
		add(cfg.CDROMPath, cfg.Name)
		for _, img := range cfg.USBImages {
			add(img.Path, cfg.Name)
		}
	}
	for i := range entries {
		entries[i].state = vm.ImageStateOf(entries[i].path)
	}
	return entries
}

func contains(list []string, s string) bool {
	for _, x := range list {
		if x == s {
			return true
		}
	}
	return false
}

// isoOutcome reports what the last key did to the picker: nothing yet, a
// pick (path, "" for the none row) or a cancel.
type isoOutcome struct {
	picked    bool
	cancelled bool
	path      string
}

// isoPicker is the selection dialog behind every ISO input: the images used
// before, one of which can be picked, and a last row to type the path of a
// new one. An optional first row stands for no image. A pick is validated
// here — the path must name a readable file — so the parent only ever gets
// an absolute path to a file that is there.
type isoPicker struct {
	none    string // label of the "no image" row; "" leaves the row out
	entries []isoEntry
	cursor  int
	input   textinput.Model
	err     string
}

// newISOPicker builds the dialog with the cursor on current's row: the none
// row when current is "" and there is one, else the first entry, else the
// input row. A current path that is not among the entries is put in the
// input instead. The command, when not nil, starts the input's cursor blink.
func newISOPicker(none, current string, entries []isoEntry) (isoPicker, tea.Cmd) {
	t := textinput.New()
	t.Prompt = ""
	t.Placeholder = "/path/to/image.iso"
	t.CharLimit = 1024
	p := isoPicker{none: none, entries: entries, input: t}
	start := p.inputRow()
	switch {
	case current == "" && none != "":
		start = 0
	case current == "" && len(entries) > 0:
		start = p.firstEntryRow()
	case current != "":
		start = p.inputRow()
		p.input.SetValue(current)
		for i, e := range entries {
			if e.path == current {
				start = p.firstEntryRow() + i
				p.input.SetValue("")
				break
			}
		}
	}
	return p.moveTo(start)
}

// --- rows: [none] entries... input ---

func (p isoPicker) firstEntryRow() int {
	if p.none != "" {
		return 1
	}
	return 0
}

func (p isoPicker) inputRow() int {
	return p.firstEntryRow() + len(p.entries)
}

func (p isoPicker) onNone() bool  { return p.none != "" && p.cursor == 0 }
func (p isoPicker) onInput() bool { return p.cursor == p.inputRow() }

// entry returns the entry under the cursor, if it is on one.
func (p isoPicker) entry() (isoEntry, bool) {
	i := p.cursor - p.firstEntryRow()
	if i < 0 || i >= len(p.entries) {
		return isoEntry{}, false
	}
	return p.entries[i], true
}

// typed reports whether the input row holds a path that has not been picked.
func (p isoPicker) typed() bool {
	return p.onInput() && strings.TrimSpace(p.input.Value()) != ""
}

func (p isoPicker) Update(msg tea.Msg) (isoPicker, tea.Cmd, isoOutcome) {
	if k, ok := msg.(tea.KeyMsg); ok {
		return p.handleKey(k)
	}
	var cmd tea.Cmd
	if p.onInput() {
		p.input, cmd = p.input.Update(msg)
	}
	return p, cmd, isoOutcome{}
}

func (p isoPicker) handleKey(msg tea.KeyMsg) (isoPicker, tea.Cmd, isoOutcome) {
	key := msg.String()
	switch key {
	case "esc":
		return p, nil, isoOutcome{cancelled: true}
	case "enter":
		return p.pick()
	case "up", "shift+tab":
		p, cmd := p.moveTo(p.cursor - 1)
		return p, cmd, isoOutcome{}
	case "down", "tab":
		p, cmd := p.moveTo(p.cursor + 1)
		return p, cmd, isoOutcome{}
	}
	if p.onInput() {
		// Letters type into the path.
		var cmd tea.Cmd
		p.input, cmd = p.input.Update(msg)
		return p, cmd, isoOutcome{}
	}
	switch key {
	case "k":
		p, cmd := p.moveTo(p.cursor - 1)
		return p, cmd, isoOutcome{}
	case "j":
		p, cmd := p.moveTo(p.cursor + 1)
		return p, cmd, isoOutcome{}
	case "g":
		p, cmd := p.moveTo(0)
		return p, cmd, isoOutcome{}
	case "G":
		p, cmd := p.moveTo(p.inputRow())
		return p, cmd, isoOutcome{}
	case "d":
		p.forget()
		return p, nil, isoOutcome{}
	}
	return p, nil, isoOutcome{}
}

// moveTo puts the cursor on a row, within bounds, giving the input the focus
// when the cursor lands on it and taking it away otherwise.
func (p isoPicker) moveTo(row int) (isoPicker, tea.Cmd) {
	p.cursor = max(0, min(row, p.inputRow()))
	p.err = ""
	if p.onInput() {
		return p, p.input.Focus()
	}
	p.input.Blur()
	return p, nil
}

// pick resolves the row under the cursor. A path that does not name a
// readable file keeps the dialog open with the reason.
func (p isoPicker) pick() (isoPicker, tea.Cmd, isoOutcome) {
	if p.onNone() {
		return p, nil, isoOutcome{picked: true}
	}
	raw := p.input.Value()
	if e, ok := p.entry(); ok {
		raw = e.path
	}
	img, err := resolveImagePath(raw)
	if err != nil {
		p.err = err.Error()
		return p, nil, isoOutcome{}
	}
	p.err = ""
	return p, nil, isoOutcome{picked: true, path: img.Path}
}

// forget drops the entry under the cursor from the remembered list. One a VM
// still has in its config stays: it would be back on the next open.
func (p *isoPicker) forget() {
	e, ok := p.entry()
	if !ok {
		return
	}
	if len(e.usedBy) > 0 {
		p.err = fmt.Sprintf("%s stays listed while a VM has it (%s)", e.path, strings.Join(e.usedBy, ", "))
		return
	}
	if err := config.ForgetISO(e.path); err != nil {
		p.err = fmt.Sprintf("forget %s: %v", e.path, err)
		return
	}
	i := p.cursor - p.firstEntryRow()
	p.entries = append(p.entries[:i], p.entries[i+1:]...)
	*p, _ = p.moveTo(p.cursor) // the next entry, or the input row
}

// keyHelp lists the picker's own keys, with what Enter does in the parent's
// words; the parent adds what Esc and Tab do.
func (p isoPicker) keyHelp(action string) string {
	return "Enter: " + action + "   ↑/↓: move   d: forget"
}

// View draws the rows and, when set, the error, fitted to width.
func (p isoPicker) View(width int) string {
	var b strings.Builder
	pathW := max(isoPathWidth, width-44)
	p.input.Width = max(20, width-20)
	row := 0
	line := func(focused bool, s string) {
		marker := "  "
		if focused {
			marker = styleLabel.Render("▸ ")
		}
		b.WriteString("  " + marker + s + "\n")
	}
	if p.none != "" {
		s := p.none
		if p.cursor == row {
			s = styleLabel.Render(s)
		}
		line(p.cursor == row, s)
		row++
	}
	for _, e := range p.entries {
		col := fmt.Sprintf("%-*s", pathW, truncateLeft(e.path, pathW))
		if p.cursor == row {
			col = styleLabel.Render(col)
		}
		s := col + "  " + imageState(e.state)
		if len(e.usedBy) > 0 {
			s += styleHelp.Render("  in use by " + strings.Join(e.usedBy, ", "))
		}
		line(p.cursor == row, s)
		row++
	}
	label := "New path  "
	if p.cursor == row {
		label = styleLabel.Render(label)
	}
	line(p.cursor == row, label+p.input.View())
	if p.err != "" {
		b.WriteString("\n")
		b.WriteString(styleError.Render(indent("✗ "+p.err, "  ")))
		b.WriteString("\n")
	}
	return b.String()
}
