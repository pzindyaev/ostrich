package tui

import (
	"fmt"
	"strings"

	"github.com/atotto/clipboard"
	"github.com/charmbracelet/bubbles/textinput"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
	"github.com/pzindyaev/ostrich/internal/vm"
)

// --- messages ---

type usbScannedMsg struct {
	devs   []vm.HostUSBDevice
	err    error
	status vm.ProcessInfo
}

// usbAppliedMsg reports an attach/detach. cfg is the saved config, or nil when
// nothing was written.
type usbAppliedMsg struct {
	cfg    *vm.VMConfig
	notice string
	err    error
}

// usbClipboardMsg reports copying the udev command to the clipboard.
type usbClipboardMsg struct{ err error }

// usbRow is one picker line: a connected host device, a configured entry whose
// device is not connected, or both.
type usbRow struct {
	host   *vm.HostUSBDevice // nil when not connected
	cfgIdx int               // index into cfg.USBDevices, -1 when not passed through
}

const usbNameWidth = 32

// USBModel lets the user pick which host USB devices are passed through to a VM.
// Each toggle is saved to vm.yaml immediately and, for a running VM, hot-plugged.
type USBModel struct {
	cfg         *vm.VMConfig
	storagePath string
	hostDevs    []vm.HostUSBDevice
	rows        []usbRow
	cursor      int
	running     bool
	busy        bool // an attach/detach is in flight
	scanErr     string
	err         string
	notice      string
	adding      bool // manual vendor:product entry active
	input       textinput.Model
	width       int
	height      int
}

// NewUSBModel constructs the picker for cfg; host devices are scanned in Init.
func NewUSBModel(cfg *vm.VMConfig, storagePath string, width, height int) USBModel {
	t := textinput.New()
	t.Prompt = ""
	t.Placeholder = "046d:085c"
	t.CharLimit = 9
	t.Width = 12
	return USBModel{
		cfg:         cfg,
		storagePath: storagePath,
		input:       t,
		width:       width,
		height:      height,
	}
}

func (m USBModel) Init() tea.Cmd {
	return scanUSBCmd(m.storagePath, m.cfg.Name)
}

func scanUSBCmd(storagePath, name string) tea.Cmd {
	return func() tea.Msg {
		devs, err := vm.ListHostUSBDevices()
		info, _ := vm.Status(storagePath, name)
		return usbScannedMsg{devs: devs, err: err, status: info}
	}
}

func (m USBModel) Update(msg tea.Msg) (USBModel, tea.Cmd) {
	switch msg := msg.(type) {
	case usbScannedMsg:
		m.hostDevs = msg.devs
		m.scanErr = ""
		if msg.err != nil {
			m.scanErr = msg.err.Error()
		}
		m.running = msg.status.Status == vm.StatusRunning
		m.rebuildRows()
		return m, nil

	case usbAppliedMsg:
		m.busy = false
		if msg.cfg != nil {
			m.cfg = msg.cfg
		}
		m.notice, m.err = msg.notice, ""
		if msg.err != nil {
			m.notice, m.err = "", msg.err.Error()
		}
		m.rebuildRows()
		return m, scanUSBCmd(m.storagePath, m.cfg.Name)

	case usbClipboardMsg:
		m.notice, m.err = "copied the udev command — run it in a shell, then press r to rescan", ""
		if msg.err != nil {
			m.notice, m.err = "", msg.err.Error()
		}
		return m, nil

	case tea.KeyMsg:
		if m.adding {
			return m.handleAddKey(msg)
		}
		return m.handleKey(msg)
	}

	if m.adding {
		var cmd tea.Cmd
		m.input, cmd = m.input.Update(msg)
		return m, cmd
	}
	return m, nil
}

func (m USBModel) handleKey(msg tea.KeyMsg) (USBModel, tea.Cmd) {
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
		if m.cursor < len(m.rows)-1 {
			m.cursor++
		}
	case "g":
		m.cursor = 0
	case "G":
		if len(m.rows) > 0 {
			m.cursor = len(m.rows) - 1
		}
	case " ", "enter":
		return m.toggle()
	case "a":
		m.adding = true
		m.err, m.notice = "", ""
		m.input.SetValue("")
		return m, m.input.Focus()
	case "r":
		m.err, m.notice = "", ""
		return m, scanUSBCmd(m.storagePath, m.cfg.Name)
	case "y":
		if dev, ok := m.fixDevice(); ok {
			return m, copyUdevCommandCmd(dev)
		}
	}
	return m, nil
}

// fixDevice returns the device under the cursor when it is connected but not
// openable, i.e. when there is a udev rule to offer.
func (m USBModel) fixDevice() (vm.USBDevice, bool) {
	if len(m.rows) == 0 || m.rows[m.cursor].host == nil || m.rows[m.cursor].host.Writable {
		return vm.USBDevice{}, false
	}
	h := m.rows[m.cursor].host
	return vm.USBDevice{VendorID: h.VendorID, ProductID: h.ProductID, Name: h.Label()}, true
}

func copyUdevCommandCmd(dev vm.USBDevice) tea.Cmd {
	command := vm.UdevRuleCommand(dev)
	return func() tea.Msg {
		if clipboard.Unsupported {
			return usbClipboardMsg{fmt.Errorf("no clipboard tool found — install wl-clipboard (Wayland) or xclip (X11), or select the command with the mouse")}
		}
		if err := clipboard.WriteAll(command); err != nil {
			return usbClipboardMsg{fmt.Errorf("copy to clipboard: %w", err)}
		}
		return usbClipboardMsg{}
	}
}

func (m USBModel) handleAddKey(msg tea.KeyMsg) (USBModel, tea.Cmd) {
	switch msg.String() {
	case "esc":
		m.adding = false
		m.input.Blur()
		return m, nil
	case "ctrl+c":
		return m, tea.Quit
	case "enter":
		dev, err := vm.ParseUSBID(m.input.Value())
		if err != nil {
			m.err = err.Error()
			return m, nil
		}
		for _, d := range m.cfg.USBDevices {
			if d.ID() == dev.ID() && d.Port == "" {
				m.err = fmt.Sprintf("%s is already passed through to this VM", dev.ID())
				return m, nil
			}
		}
		for _, h := range m.hostDevs {
			if dev.Matches(h) {
				dev.Name = h.Label()
				break
			}
		}
		m.adding = false
		m.input.Blur()
		return m.attach(dev)
	}
	var cmd tea.Cmd
	m.input, cmd = m.input.Update(msg)
	return m, cmd
}

// toggle attaches or detaches the device under the cursor.
func (m USBModel) toggle() (USBModel, tea.Cmd) {
	if m.busy || len(m.rows) == 0 {
		return m, nil
	}
	row := m.rows[m.cursor]
	if row.cfgIdx >= 0 {
		return m.detach(row.cfgIdx)
	}
	dev := vm.USBDevice{
		VendorID:  row.host.VendorID,
		ProductID: row.host.ProductID,
		Name:      row.host.Label(),
	}
	if m.countID(row.host.ID()) > 1 {
		dev.Port = row.host.Port // identical devices present: pin to this one
	}
	return m.attach(dev)
}

func (m USBModel) attach(dev vm.USBDevice) (USBModel, tea.Cmd) {
	cfg := m.cfgWith(append(m.devices(), dev))
	idx := len(cfg.USBDevices) - 1
	m.busy, m.err, m.notice = true, "", ""
	storagePath, running := m.storagePath, m.running
	return m, func() tea.Msg {
		if err := vm.SaveConfig(storagePath, cfg); err != nil {
			return usbAppliedMsg{err: fmt.Errorf("save VM config: %w", err)}
		}
		if !running {
			return usbAppliedMsg{cfg: cfg, notice: fmt.Sprintf("attached %s — takes effect on next start", dev.Label())}
		}
		if err := vm.USBHotplug(storagePath, cfg, idx); err != nil {
			return usbAppliedMsg{cfg: cfg, err: fmt.Errorf(
				"attached %s in config, but hot-plug failed (takes effect on next start):\n%w", dev.Label(), err)}
		}
		return usbAppliedMsg{cfg: cfg, notice: fmt.Sprintf("attached %s (hot-plugged)", dev.Label())}
	}
}

func (m USBModel) detach(idx int) (USBModel, tea.Cmd) {
	old := m.cfg
	dev := old.USBDevices[idx]
	devs := m.devices()
	cfg := m.cfgWith(append(devs[:idx], devs[idx+1:]...))
	m.busy, m.err, m.notice = true, "", ""
	storagePath, running := m.storagePath, m.running
	return m, func() tea.Msg {
		if err := vm.SaveConfig(storagePath, cfg); err != nil {
			return usbAppliedMsg{err: fmt.Errorf("save VM config: %w", err)}
		}
		if !running {
			return usbAppliedMsg{cfg: cfg, notice: fmt.Sprintf("detached %s — takes effect on next start", dev.Label())}
		}
		if err := vm.USBHotunplug(storagePath, old, idx); err != nil {
			return usbAppliedMsg{cfg: cfg, err: fmt.Errorf(
				"detached %s in config, but hot-unplug failed (takes effect on next start):\n%w", dev.Label(), err)}
		}
		return usbAppliedMsg{cfg: cfg, notice: fmt.Sprintf("detached %s (hot-unplugged)", dev.Label())}
	}
}

// devices returns a copy of the configured device list safe to mutate.
func (m USBModel) devices() []vm.USBDevice {
	return append([]vm.USBDevice(nil), m.cfg.USBDevices...)
}

// cfgWith returns a copy of the config with the given device list.
func (m USBModel) cfgWith(devs []vm.USBDevice) *vm.VMConfig {
	cfg := *m.cfg
	cfg.USBDevices = devs
	return &cfg
}

func (m USBModel) countID(id string) int {
	n := 0
	for _, h := range m.hostDevs {
		if h.ID() == id {
			n++
		}
	}
	return n
}

// rebuildRows lists every connected host device (marking those passed through)
// followed by configured devices that are not currently connected.
func (m *USBModel) rebuildRows() {
	states := vm.MatchUSB(m.cfg.USBDevices, m.hostDevs)
	rows := make([]usbRow, 0, len(m.hostDevs)+len(states))
	for j := range m.hostDevs {
		row := usbRow{host: &m.hostDevs[j], cfgIdx: -1}
		for i := range states {
			if states[i].Host == &m.hostDevs[j] {
				row.cfgIdx = i
				break
			}
		}
		rows = append(rows, row)
	}
	for i := range states {
		if states[i].Host == nil {
			rows = append(rows, usbRow{cfgIdx: i})
		}
	}
	m.rows = rows
	if m.cursor >= len(rows) {
		m.cursor = max(0, len(rows)-1)
	}
}

func (m USBModel) View() string {
	if m.width == 0 {
		return ""
	}

	var b strings.Builder

	header := styleHeader.Copy().Width(m.width).Render("  Ostrich — USB Passthrough: " + m.cfg.Name)
	b.WriteString(header)
	b.WriteString("\n\n")

	if m.running {
		b.WriteString(styleRunning.Render("  ● running"))
		b.WriteString(styleHelp.Render(" — attaching or detaching hot-plugs the device in the guest"))
	} else {
		b.WriteString(styleStopped.Render("  ● stopped"))
		b.WriteString(styleHelp.Render(" — changes take effect when the VM is started"))
	}
	b.WriteString("\n")
	b.WriteString(styleHelp.Render("  The host cannot use a device while the guest holds it."))
	b.WriteString("\n\n")

	b.WriteString(styleLabel.Render("  Host USB devices"))
	b.WriteString(styleHelp.Render("   [x] = passed through to this VM"))
	b.WriteString("\n\n")

	if m.scanErr != "" {
		b.WriteString(styleError.Render("  ✗ " + m.scanErr))
		b.WriteString("\n")
		b.WriteString(styleHelp.Render("  Devices can still be added by ID with a."))
		b.WriteString("\n\n")
	}
	if len(m.rows) == 0 {
		b.WriteString(styleHelp.Render("  No USB devices found. Plug one in and press r to rescan, or press a to add one by ID."))
		b.WriteString("\n")
	}
	for i, row := range m.rows {
		b.WriteString(m.renderRow(row, i == m.cursor))
		b.WriteString("\n")
	}
	b.WriteString("\n")

	if m.adding {
		b.WriteString(styleLabel.Render("  Vendor:Product ID  "))
		b.WriteString(m.input.View())
		b.WriteString(styleHelp.Render("   as shown by lsusb, e.g. 046d:085c"))
		b.WriteString("\n\n")
	} else if dev, ok := m.fixDevice(); ok {
		b.WriteString(styleError.Render(fmt.Sprintf("  ✗ no write access to %s — QEMU cannot open it", m.rows[m.cursor].host.DevNode)))
		b.WriteString("\n")
		b.WriteString(styleHelp.Render("  Run this to grant access to your user (y copies it), then press r to rescan:"))
		b.WriteString("\n")
		// Plain text, broken only between shell words, so a mouse selection of
		// the block pastes as one working command.
		for _, line := range shellLines(vm.UdevRuleCommandWords(dev), m.width-8) {
			b.WriteString("    " + line + "\n")
		}
		b.WriteString("\n")
	}

	if m.err != "" {
		b.WriteString(styleError.Render(indent("✗ "+m.err, "  ")))
		b.WriteString("\n\n")
	} else if m.notice != "" {
		b.WriteString(styleSuccess.Render("  ✓ " + m.notice))
		b.WriteString("\n\n")
	}

	keyHelp := "Space/Enter: attach/detach   a: add by ID   r: rescan   j/k: move   q/Esc: back"
	if _, ok := m.fixDevice(); ok {
		keyHelp = "Space/Enter: attach/detach   y: copy udev command   a: add by ID   r: rescan   j/k: move   q/Esc: back"
	}
	if m.adding {
		keyHelp = "Enter: attach   Esc: cancel"
	}
	b.WriteString(styleHelp.Render("  " + keyHelp))

	return lipgloss.NewStyle().Width(m.width).Render(b.String())
}

func (m USBModel) renderRow(row usbRow, focused bool) string {
	var id, name, where, access string
	if row.host != nil {
		id, name, where = row.host.ID(), row.host.Label(), "port "+row.host.Port
		if !row.host.Writable {
			access = styleError.Render("  ✗ no access")
		}
	} else {
		d := m.cfg.USBDevices[row.cfgIdx]
		id, name, where = d.ID(), d.Label(), "not connected"
		if d.Port != "" {
			where += " (port " + d.Port + ")"
		}
	}

	box := styleHelp.Render("[ ]")
	if row.cfgIdx >= 0 {
		box = styleSuccess.Render("[x]")
	}
	marker := "  "
	nameCol := fmt.Sprintf("%-*s", usbNameWidth, truncate(name, usbNameWidth))
	if focused {
		marker = styleLabel.Render("▸ ")
		nameCol = styleLabel.Render(nameCol)
	}
	return fmt.Sprintf("  %s%s %s  %s  %s%s", marker, box, id, nameCol, styleHelp.Render(where), access)
}

// truncate shortens s to at most n display runes, ending with an ellipsis.
func truncate(s string, n int) string {
	r := []rune(s)
	if len(r) <= n {
		return s
	}
	return string(r[:n-1]) + "…"
}

// indent prefixes every line of s.
func indent(s, prefix string) string {
	return prefix + strings.ReplaceAll(s, "\n", "\n"+prefix)
}

// shellLines packs shell words into lines of at most width columns, ending
// every line but the last with a backslash continuation. Lines never break
// inside a word, so the block pastes into a shell as a single command.
func shellLines(words []string, width int) []string {
	var lines []string
	cur := ""
	for _, w := range words {
		switch {
		case cur == "":
			cur = w
		case len(cur)+1+len(w)+2 > width: // +2 keeps room for " \\"
			lines = append(lines, cur+" \\")
			cur = w
		default:
			cur += " " + w
		}
	}
	return append(lines, cur)
}
