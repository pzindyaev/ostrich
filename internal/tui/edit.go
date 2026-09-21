package tui

import (
	"fmt"
	"os"
	"strconv"
	"strings"

	"github.com/charmbracelet/bubbles/textinput"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
	"github.com/pzindyaev/ostrich/internal/vm"
)

type editField int

const (
	editName editField = iota
	editCPU
	editRAM
	editDisk
	editISO
	editNetwork // selector (no text input)
	editMAC
	editForwards
	editVNC
	editSave // button (no text input)
	editFieldCount
)

var editLabels = [editFieldCount]string{
	"Name",
	"CPU Cores",
	"RAM (MiB)",
	"Disk Size (GiB)",
	"Boot ISO",
	"Network",
	"MAC Address",
	"Port Forwards",
	"VNC Display",
	"",
}

var editHelp = [editFieldCount]string{
	"Letters, digits, hyphens and underscores. Renaming requires the VM to be stopped",
	"Number of virtual CPU cores, e.g. 2",
	"Memory in MiB, e.g. 2048 for 2 GiB",
	"Can only grow, and the VM must be stopped. The guest must extend its own partitions",
	"Full path to an ISO image to boot from, or leave blank to boot from disk",
	"h/l/←/→ to select: user (NAT) · tap (bridge) · none",
	"Leave blank to generate a new random address",
	"user networking only. Comma-separated [tcp|udp:]host:guest, e.g. 2222:22, udp:5353:53",
	"Display number 1–99 (TCP port = 5900+n). 0 to disable",
	"Press Enter to save changes",
}

// isText reports whether the field is backed by a text input.
func (f editField) isText() bool {
	return f != editNetwork && f != editSave
}

type vmUpdatedMsg struct{ name string }
type vmUpdateErrMsg struct{ err error }

// EditVMModel is a single-page form for editing an existing VM's properties.
type EditVMModel struct {
	orig    *vm.VMConfig
	field   editField
	inputs  [editFieldCount]textinput.Model // entries for non-text fields are unused
	netIdx  int
	running bool
	err     string
	mgr     *vm.Manager
	width   int
	height  int
}

// NewEditVMModel builds an EditVMModel pre-filled from cfg.
func NewEditVMModel(mgr *vm.Manager, cfg *vm.VMConfig, width, height int) EditVMModel {
	values := [editFieldCount]string{
		editName:     cfg.Name,
		editCPU:      strconv.Itoa(cfg.CPU),
		editRAM:      strconv.Itoa(cfg.RAM),
		editDisk:     strconv.Itoa(cfg.DiskSize),
		editISO:      cfg.CDROMPath,
		editMAC:      cfg.Network.MAC,
		editForwards: vm.FormatPortForwards(cfg.Network.PortForwards),
		editVNC:      strconv.Itoa(cfg.VNCPort),
	}

	var inputs [editFieldCount]textinput.Model
	for i := range inputs {
		t := textinput.New()
		t.Prompt = ""
		t.SetValue(values[i])
		t.CharLimit = 256
		t.Width = 45
		inputs[i] = t
	}
	inputs[editName].Focus()

	netIdx := 0
	for i, nt := range networkChoices {
		if nt == cfg.Network.Type {
			netIdx = i
		}
	}

	info, _ := vm.Status(mgr.StoragePath, cfg.Name)

	return EditVMModel{
		orig:    cfg,
		inputs:  inputs,
		netIdx:  netIdx,
		running: info.Status == vm.StatusRunning,
		mgr:     mgr,
		width:   width,
		height:  height,
	}
}

func (m EditVMModel) Init() tea.Cmd {
	return textinput.Blink
}

func (m EditVMModel) Update(msg tea.Msg) (EditVMModel, tea.Cmd) {
	switch msg := msg.(type) {
	case vmUpdatedMsg:
		return m, func() tea.Msg { return NavigateMsg{To: screenDetail, VMName: msg.name} }

	case vmUpdateErrMsg:
		m.err = msg.err.Error()
		return m, nil

	case tea.KeyMsg:
		return m.handleKey(msg)
	}

	// Forward non-key messages to the active text input (if any).
	if m.field.isText() {
		var cmd tea.Cmd
		m.inputs[m.field], cmd = m.inputs[m.field].Update(msg)
		return m, cmd
	}
	return m, nil
}

func (m EditVMModel) handleKey(msg tea.KeyMsg) (EditVMModel, tea.Cmd) {
	switch msg.String() {
	case "esc":
		name := m.orig.Name
		return m, func() tea.Msg { return NavigateMsg{To: screenDetail, VMName: name} }

	case "ctrl+c":
		return m, tea.Quit

	case "ctrl+s":
		return m.save()

	case "enter":
		if m.field == editSave {
			return m.save()
		}
		return m.moveTo((m.field + 1) % editFieldCount)

	case "tab", "down":
		return m.moveTo((m.field + 1) % editFieldCount)

	case "shift+tab", "up":
		return m.moveTo((m.field - 1 + editFieldCount) % editFieldCount)

	case "j":
		// vim-next: only when no text input is active
		if !m.field.isText() {
			return m.moveTo((m.field + 1) % editFieldCount)
		}

	case "k":
		// vim-prev: only when no text input is active
		if !m.field.isText() {
			return m.moveTo((m.field - 1 + editFieldCount) % editFieldCount)
		}

	case "h", "left":
		if m.field == editNetwork {
			m.netIdx = (m.netIdx - 1 + len(networkChoices)) % len(networkChoices)
		}

	case "l", "right":
		if m.field == editNetwork {
			m.netIdx = (m.netIdx + 1) % len(networkChoices)
		}
	}

	// Forward keystrokes to active text input.
	if m.field.isText() {
		var cmd tea.Cmd
		m.inputs[m.field], cmd = m.inputs[m.field].Update(msg)
		return m, cmd
	}
	return m, nil
}

func (m EditVMModel) moveTo(f editField) (EditVMModel, tea.Cmd) {
	if m.field.isText() {
		m.inputs[m.field].Blur()
	}
	m.field = f
	if f.isText() {
		return m, m.inputs[f].Focus()
	}
	return m, nil
}

func (m EditVMModel) value(f editField) string {
	return strings.TrimSpace(m.inputs[f].Value())
}

// buildConfig validates the form and returns the edited config. On failure it
// also returns the offending field so the cursor can be moved there.
func (m EditVMModel) buildConfig() (*vm.VMConfig, editField, error) {
	cfg := *m.orig

	cfg.Name = m.value(editName)
	if err := validateVMName(cfg.Name); err != nil {
		return nil, editName, err
	}
	if cfg.Name != m.orig.Name && m.mgr.Exists(cfg.Name) {
		return nil, editName, fmt.Errorf("a VM named %q already exists", cfg.Name)
	}
	if cfg.Name != m.orig.Name && m.running {
		return nil, editName, fmt.Errorf("stop the VM before renaming it")
	}

	var err error
	if cfg.CPU, err = strconv.Atoi(m.value(editCPU)); err != nil || cfg.CPU < 1 {
		return nil, editCPU, fmt.Errorf("CPU must be a positive integer")
	}
	if cfg.RAM, err = strconv.Atoi(m.value(editRAM)); err != nil || cfg.RAM < 64 {
		return nil, editRAM, fmt.Errorf("RAM must be at least 64 MiB")
	}

	if cfg.DiskSize, err = strconv.Atoi(m.value(editDisk)); err != nil {
		return nil, editDisk, fmt.Errorf("disk size must be an integer")
	}
	if cfg.DiskSize < m.orig.DiskSize {
		return nil, editDisk, fmt.Errorf("disk can only grow (currently %d GiB)", m.orig.DiskSize)
	}
	if cfg.DiskSize != m.orig.DiskSize && m.running {
		return nil, editDisk, fmt.Errorf("stop the VM before resizing its disk")
	}

	cfg.CDROMPath = m.value(editISO)
	if cfg.CDROMPath != "" {
		if st, err := os.Stat(cfg.CDROMPath); err != nil || st.IsDir() {
			return nil, editISO, fmt.Errorf("ISO file not found: %s", cfg.CDROMPath)
		}
	}

	cfg.Network.Type = networkChoices[m.netIdx]
	cfg.Network.MAC = m.value(editMAC)
	if cfg.Network.MAC != "" {
		if err := vm.ValidateMAC(cfg.Network.MAC); err != nil {
			return nil, editMAC, err
		}
	}
	if cfg.Network.PortForwards, err = vm.ParsePortForwards(m.value(editForwards)); err != nil {
		return nil, editForwards, err
	}

	if cfg.VNCPort, err = strconv.Atoi(m.value(editVNC)); err != nil || cfg.VNCPort < 0 || cfg.VNCPort > 99 {
		return nil, editVNC, fmt.Errorf("VNC display must be 0 (disabled) or 1–99")
	}

	return &cfg, 0, nil
}

func (m EditVMModel) save() (EditVMModel, tea.Cmd) {
	cfg, field, err := m.buildConfig()
	if err != nil {
		m, cmd := m.moveTo(field)
		m.err = err.Error()
		return m, cmd
	}
	m.err = ""
	oldName := m.orig.Name
	return m, func() tea.Msg {
		if err := m.mgr.Update(oldName, cfg); err != nil {
			return vmUpdateErrMsg{err}
		}
		return vmUpdatedMsg{name: cfg.Name}
	}
}

func (m EditVMModel) View() string {
	if m.width == 0 {
		return ""
	}

	var b strings.Builder

	header := styleHeader.Copy().Width(m.width).Render("  Ostrich — Edit VM: " + m.orig.Name)
	b.WriteString(header)
	b.WriteString("\n\n")

	if m.running {
		b.WriteString(styleRunning.Render("  ● running"))
		b.WriteString(styleHelp.Render(" — changes take effect on next start; name and disk size are locked"))
		b.WriteString("\n\n")
	}

	for f := editField(0); f < editFieldCount; f++ {
		focused := f == m.field

		if f == editSave {
			b.WriteString("\n  ")
			if focused {
				b.WriteString(styleSelected.Render(" Save "))
			} else {
				b.WriteString(styleNormal.Render(" Save "))
			}
			b.WriteString("\n")
			continue
		}

		marker := "  "
		label := styleHelp.Render(fmt.Sprintf("%-16s", editLabels[f]))
		if focused {
			marker = styleLabel.Render("▸ ")
			label = styleLabel.Render(fmt.Sprintf("%-16s", editLabels[f]))
		}
		b.WriteString(marker + label + " ")

		if f == editNetwork {
			var opts []string
			for i, lbl := range networkLabels {
				if i == m.netIdx {
					opts = append(opts, styleSelected.Render(" "+lbl+" "))
				} else {
					opts = append(opts, styleNormal.Render(" "+lbl+" "))
				}
			}
			b.WriteString(strings.Join(opts, " "))
		} else {
			b.WriteString(m.inputs[f].View())
		}
		b.WriteString("\n")
	}

	b.WriteString("\n")
	b.WriteString(styleHelp.Render("  " + editHelp[m.field]))
	b.WriteString("\n\n")

	if m.err != "" {
		b.WriteString(styleError.Render("  ✗ " + m.err))
		b.WriteString("\n\n")
	}

	keyHelp := "Tab/↓: next   Shift+Tab/↑: back   Ctrl-s: save   Esc: cancel"
	if m.field == editNetwork {
		keyHelp = "h/l/←/→: select   j/↓: next   k/↑: back   Ctrl-s: save   Esc: cancel"
	} else if m.field == editSave {
		keyHelp = "Enter: save   k/Shift+Tab: back   Esc: cancel"
	}
	b.WriteString(styleHelp.Render("  " + keyHelp))

	return lipgloss.NewStyle().Width(m.width).Render(b.String())
}
