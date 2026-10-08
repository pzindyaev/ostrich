package tui

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/charmbracelet/bubbles/textinput"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
	"github.com/pzindyaev/ostrich/internal/vm"
)

// --- messages ---

// isoScannedMsg carries the state of the VM's images and of the VM itself.
type isoScannedMsg struct {
	cdrom  vm.ImageState   // the boot ISO; meaningless when none is configured
	states []vm.ImageState // one per cfg.USBImages entry
	status vm.ProcessInfo
}

// isoAppliedMsg reports a change. cfg is the saved config, or nil when
// nothing was written.
type isoAppliedMsg struct {
	cfg    *vm.VMConfig
	notice string
	err    error
}

// isoPrompt says what the path entry, when open, is for.
type isoPrompt int

const (
	promptNone     isoPrompt = iota
	promptUSBImage           // attach an image as a USB drive
	promptBootISO            // put an ISO in the CD-ROM drive
)

const isoPathWidth = 48

// ISOModel lets the user swap or eject the boot ISO in a VM's CD-ROM drive
// and attach disk images (ISOs) to it as read-only USB drives. Each change is
// saved to vm.yaml immediately and, for a running VM, applied through the
// QEMU monitor on the spot.
type ISOModel struct {
	cfg         *vm.VMConfig
	storagePath string
	cdrom       vm.ImageState
	states      []vm.ImageState
	cursor      int
	running     bool
	busy        bool // a change is in flight
	err         string
	notice      string
	prompt      isoPrompt
	input       textinput.Model
	width       int
	height      int
}

// NewISOModel constructs the screen for cfg; the images are checked in Init.
func NewISOModel(cfg *vm.VMConfig, storagePath string, width, height int) ISOModel {
	t := textinput.New()
	t.Prompt = ""
	t.Placeholder = "/path/to/image.iso"
	return ISOModel{
		cfg:         cfg,
		storagePath: storagePath,
		cdrom:       vm.ImageStateOf(cfg.CDROMPath),
		states:      vm.USBImageStates(cfg.USBImages),
		input:       t,
		width:       width,
		height:      height,
	}
}

func (m ISOModel) Init() tea.Cmd {
	return scanISOCmd(m.storagePath, m.cfg)
}

func scanISOCmd(storagePath string, cfg *vm.VMConfig) tea.Cmd {
	return func() tea.Msg {
		info, _ := vm.Status(storagePath, cfg.Name)
		return isoScannedMsg{
			cdrom:  vm.ImageStateOf(cfg.CDROMPath),
			states: vm.USBImageStates(cfg.USBImages),
			status: info,
		}
	}
}

func (m ISOModel) Update(msg tea.Msg) (ISOModel, tea.Cmd) {
	switch msg := msg.(type) {
	case isoScannedMsg:
		m.cdrom = msg.cdrom
		m.states = msg.states
		m.running = msg.status.Status == vm.StatusRunning
		m.clampCursor()
		return m, nil

	case isoAppliedMsg:
		m.busy = false
		if msg.cfg != nil {
			m.cfg = msg.cfg
		}
		m.notice, m.err = msg.notice, ""
		if msg.err != nil {
			m.notice, m.err = "", msg.err.Error()
		}
		m.cdrom = vm.ImageStateOf(m.cfg.CDROMPath)
		m.states = vm.USBImageStates(m.cfg.USBImages)
		m.clampCursor()
		return m, scanISOCmd(m.storagePath, m.cfg)

	case tea.KeyMsg:
		if m.prompt != promptNone {
			return m.handlePromptKey(msg)
		}
		return m.handleKey(msg)
	}

	if m.prompt != promptNone {
		var cmd tea.Cmd
		m.input, cmd = m.input.Update(msg)
		return m, cmd
	}
	return m, nil
}

func (m *ISOModel) clampCursor() {
	if m.cursor >= len(m.cfg.USBImages) {
		m.cursor = max(0, len(m.cfg.USBImages)-1)
	}
}

func (m ISOModel) handleKey(msg tea.KeyMsg) (ISOModel, tea.Cmd) {
	switch msg.String() {
	case "esc", "q", "h":
		name := m.cfg.Name
		return m, func() tea.Msg { return NavigateMsg{To: screenDetail, VMName: name} }
	case "ctrl+c":
		return m, tea.Quit
	case "up", "k":
		if m.cursor > 0 {
			m.cursor--
		}
	case "down", "j":
		if m.cursor < len(m.cfg.USBImages)-1 {
			m.cursor++
		}
	case "g":
		m.cursor = 0
	case "G":
		if n := len(m.cfg.USBImages); n > 0 {
			m.cursor = n - 1
		}
	case " ", "enter", "d":
		if m.busy || len(m.cfg.USBImages) == 0 {
			return m, nil
		}
		return m.detach(m.cursor)
	case "a":
		return m.openPrompt(promptUSBImage)
	case "c":
		return m.openPrompt(promptBootISO)
	case "e":
		if m.busy {
			return m, nil
		}
		if m.cfg.CDROMPath == "" {
			m.err, m.notice = "", "the CD-ROM drive is already empty"
			return m, nil
		}
		return m.setBootISO("")
	case "r":
		m.err, m.notice = "", ""
		return m, scanISOCmd(m.storagePath, m.cfg)
	}
	return m, nil
}

func (m ISOModel) openPrompt(kind isoPrompt) (ISOModel, tea.Cmd) {
	m.prompt = kind
	m.err, m.notice = "", ""
	m.input.SetValue("")
	m.input.Width = max(20, m.width-20)
	return m, m.input.Focus()
}

func (m ISOModel) handlePromptKey(msg tea.KeyMsg) (ISOModel, tea.Cmd) {
	switch msg.String() {
	case "esc":
		m.prompt = promptNone
		m.input.Blur()
		return m, nil
	case "ctrl+c":
		return m, tea.Quit
	case "enter":
		img, err := resolveImagePath(m.input.Value())
		if err != nil {
			m.err = err.Error()
			return m, nil
		}
		kind := m.prompt
		if kind == promptUSBImage {
			for _, existing := range m.cfg.USBImages {
				if existing.Path == img.Path {
					m.err = fmt.Sprintf("%s is already attached to this VM", img.Path)
					return m, nil
				}
			}
		}
		m.prompt = promptNone
		m.input.Blur()
		if kind == promptBootISO {
			return m.setBootISO(img.Path)
		}
		return m.attach(img)
	}
	var cmd tea.Cmd
	m.input, cmd = m.input.Update(msg)
	return m, cmd
}

// resolveImagePath turns what the user typed into a validated image entry:
// a leading "~" is the home directory and relative paths are made absolute.
func resolveImagePath(s string) (vm.USBImage, error) {
	s = strings.TrimSpace(s)
	if s == "" {
		return vm.USBImage{}, fmt.Errorf("enter the path of an image file")
	}
	if s == "~" || strings.HasPrefix(s, "~/") {
		home, err := os.UserHomeDir()
		if err != nil {
			return vm.USBImage{}, fmt.Errorf("expand ~: %w", err)
		}
		s = filepath.Join(home, s[1:])
	}
	abs, err := filepath.Abs(s)
	if err != nil {
		return vm.USBImage{}, err
	}
	img := vm.USBImage{Path: abs}
	if err := img.Validate(); err != nil {
		return vm.USBImage{}, err
	}
	return img, nil
}

// setBootISO puts path into the CD-ROM drive, or empties it when path is "".
func (m ISOModel) setBootISO(path string) (ISOModel, tea.Cmd) {
	cfg := *m.cfg
	cfg.CDROMPath = path
	m.busy, m.err, m.notice = true, "", ""
	storagePath, running := m.storagePath, m.running
	what := "ejected the boot ISO"
	if path != "" {
		what = "inserted " + filepath.Base(path)
	}
	return m, func() tea.Msg {
		if err := vm.SaveConfig(storagePath, &cfg); err != nil {
			return isoAppliedMsg{err: fmt.Errorf("save VM config: %w", err)}
		}
		if !running {
			return isoAppliedMsg{cfg: &cfg, notice: what + " — takes effect on next start"}
		}
		if err := vm.CDROMChange(storagePath, cfg.Name, path); err != nil {
			return isoAppliedMsg{cfg: &cfg, err: fmt.Errorf(
				"%s in config, but the running VM's drive could not be changed (takes effect on next start):\n%w", what, err)}
		}
		return isoAppliedMsg{cfg: &cfg, notice: what + " (hot-swapped)"}
	}
}

func (m ISOModel) attach(img vm.USBImage) (ISOModel, tea.Cmd) {
	cfg := m.cfgWith(append(m.images(), img))
	idx := len(cfg.USBImages) - 1
	m.busy, m.err, m.notice = true, "", ""
	storagePath, running := m.storagePath, m.running
	return m, func() tea.Msg {
		if err := vm.SaveConfig(storagePath, cfg); err != nil {
			return isoAppliedMsg{err: fmt.Errorf("save VM config: %w", err)}
		}
		if !running {
			return isoAppliedMsg{cfg: cfg, notice: fmt.Sprintf("attached %s — takes effect on next start", img.Label())}
		}
		if err := vm.USBImageHotplug(storagePath, cfg, idx); err != nil {
			return isoAppliedMsg{cfg: cfg, err: fmt.Errorf(
				"attached %s in config, but hot-plug failed (takes effect on next start):\n%w", img.Label(), err)}
		}
		return isoAppliedMsg{cfg: cfg, notice: fmt.Sprintf("attached %s (hot-plugged as a USB drive)", img.Label())}
	}
}

func (m ISOModel) detach(idx int) (ISOModel, tea.Cmd) {
	old := m.cfg
	img := old.USBImages[idx]
	imgs := m.images()
	cfg := m.cfgWith(append(imgs[:idx], imgs[idx+1:]...))
	m.busy, m.err, m.notice = true, "", ""
	storagePath, running := m.storagePath, m.running
	return m, func() tea.Msg {
		if err := vm.SaveConfig(storagePath, cfg); err != nil {
			return isoAppliedMsg{err: fmt.Errorf("save VM config: %w", err)}
		}
		if !running {
			return isoAppliedMsg{cfg: cfg, notice: fmt.Sprintf("detached %s — takes effect on next start", img.Label())}
		}
		if err := vm.USBImageHotunplug(storagePath, old, idx); err != nil {
			return isoAppliedMsg{cfg: cfg, err: fmt.Errorf(
				"detached %s in config, but hot-unplug failed (takes effect on next start):\n%w", img.Label(), err)}
		}
		return isoAppliedMsg{cfg: cfg, notice: fmt.Sprintf("detached %s (hot-unplugged)", img.Label())}
	}
}

// images returns a copy of the configured image list safe to mutate.
func (m ISOModel) images() []vm.USBImage {
	return append([]vm.USBImage(nil), m.cfg.USBImages...)
}

// cfgWith returns a copy of the config with the given image list.
func (m ISOModel) cfgWith(imgs []vm.USBImage) *vm.VMConfig {
	cfg := *m.cfg
	cfg.USBImages = imgs
	return &cfg
}

func (m ISOModel) View() string {
	if m.width == 0 {
		return ""
	}

	var b strings.Builder

	header := styleHeader.Copy().Width(m.width).Render("  Ostrich — ISO Hot-plug: " + m.cfg.Name)
	b.WriteString(header)
	b.WriteString("\n\n")

	if m.running {
		b.WriteString(styleRunning.Render("  ● running"))
		b.WriteString(styleHelp.Render(" — changes are applied in the guest right away"))
	} else {
		b.WriteString(styleStopped.Render("  ● stopped"))
		b.WriteString(styleHelp.Render(" — changes take effect when the VM is started"))
	}
	b.WriteString("\n\n")

	b.WriteString(styleLabel.Render("  Boot ISO (CD-ROM drive)"))
	b.WriteString("\n\n")
	if m.cfg.CDROMPath == "" {
		b.WriteString(styleHelp.Render("    (empty)"))
	} else {
		b.WriteString("    " + m.pathCol(m.cfg.CDROMPath, false) + "  " + imageState(m.cdromState()))
	}
	b.WriteString("\n\n")

	b.WriteString(styleLabel.Render("  USB drives"))
	b.WriteString(styleHelp.Render("   images attached read-only; the guest sees each one as a USB stick"))
	b.WriteString("\n\n")

	if len(m.cfg.USBImages) == 0 {
		b.WriteString(styleHelp.Render("  No images attached. Press a to attach one."))
		b.WriteString("\n")
	}
	for i := range m.cfg.USBImages {
		b.WriteString(m.renderRow(i, i == m.cursor))
		b.WriteString("\n")
	}
	b.WriteString("\n")

	if m.prompt != promptNone {
		label, hint := "  Image path     ", "an .iso (or any raw disk image) on the host; ~ is your home directory"
		if m.prompt == promptBootISO {
			label, hint = "  Boot ISO path  ", "the ISO to put in the CD-ROM drive; ~ is your home directory"
		}
		b.WriteString(styleLabel.Render(label))
		b.WriteString(m.input.View())
		b.WriteString("\n")
		b.WriteString(styleHelp.Render("                 " + hint))
		b.WriteString("\n\n")
	}

	if m.err != "" {
		b.WriteString(styleError.Render(indent("✗ "+m.err, "  ")))
		b.WriteString("\n\n")
	} else if m.notice != "" {
		b.WriteString(styleSuccess.Render("  ✓ " + m.notice))
		b.WriteString("\n\n")
	}

	keyHelp := "c: change boot ISO   e: eject   a: attach USB image   Space/Enter/d: detach   r: refresh   j/k: move   q/Esc: back"
	switch m.prompt {
	case promptBootISO:
		keyHelp = "Enter: insert   Esc: cancel"
	case promptUSBImage:
		keyHelp = "Enter: attach   Esc: cancel"
	}
	b.WriteString(styleHelp.Render("  " + keyHelp))

	return lipgloss.NewStyle().Width(m.width).Render(b.String())
}

func (m ISOModel) renderRow(i int, focused bool) string {
	marker := "  "
	if focused {
		marker = styleLabel.Render("▸ ")
	}
	return fmt.Sprintf("  %s%s  %s", marker, m.pathCol(m.cfg.USBImages[i].Path, focused), imageState(m.stateOf(i)))
}

// pathCol renders a path in the column width of the screen, cutting long
// paths on the left so the file name stays visible.
func (m ISOModel) pathCol(path string, focused bool) string {
	width := max(isoPathWidth, m.width-24)
	col := fmt.Sprintf("%-*s", width, truncateLeft(path, width))
	if focused {
		return styleLabel.Render(col)
	}
	return col
}

// cdromState returns the checked state of the boot ISO, or an unchecked one
// when the state has not caught up with the config yet.
func (m ISOModel) cdromState() vm.ImageState {
	if m.cdrom.Path == m.cfg.CDROMPath {
		return m.cdrom
	}
	return vm.ImageState{Path: m.cfg.CDROMPath}
}

// stateOf returns the checked state for image i, or an unchecked one when the
// states have not caught up with the config yet.
func (m ISOModel) stateOf(i int) vm.ImageState {
	if i < len(m.states) && m.states[i].Path == m.cfg.USBImages[i].Path {
		return m.states[i]
	}
	return vm.ImageState{Path: m.cfg.USBImages[i].Path}
}

// imageState renders an image's host-side state: its size, or what is wrong.
func imageState(s vm.ImageState) string {
	switch {
	case s.Err != nil && strings.Contains(s.Err.Error(), "not found"):
		return styleError.Render("✗ not found")
	case s.Err != nil && strings.Contains(s.Err.Error(), "no read access"):
		return styleError.Render("✗ no access")
	case s.Err != nil:
		return styleError.Render("✗ " + s.Err.Error())
	case s.Size > 0:
		return styleSuccess.Render("● " + humanSize(s.Size))
	}
	return styleSuccess.Render("● present")
}

// truncateLeft shortens s to at most n display runes, keeping its end.
func truncateLeft(s string, n int) string {
	r := []rune(s)
	if len(r) <= n {
		return s
	}
	return "…" + string(r[len(r)-n+1:])
}

// humanSize formats a byte count in binary units.
func humanSize(n int64) string {
	const unit = 1024
	switch {
	case n >= unit*unit*unit:
		return fmt.Sprintf("%.1f GiB", float64(n)/(unit*unit*unit))
	case n >= unit*unit:
		return fmt.Sprintf("%d MiB", n/(unit*unit))
	case n >= unit:
		return fmt.Sprintf("%d KiB", n/unit)
	}
	return fmt.Sprintf("%d B", n)
}
