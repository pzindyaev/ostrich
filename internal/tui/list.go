package tui

import (
	"fmt"
	"strings"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/pzindyaev/ostrich/internal/vm"
)

// --- messages ---

type vmListLoadedMsg struct {
	cfgs   []*vm.VMConfig
	statuses map[string]vm.VMStatus
}

type vmActionErrMsg struct{ err error }
type vmActionOKMsg struct{ name string }

// --- tea.Cmd helpers ---

func loadVMsCmd(mgr *vm.Manager) tea.Cmd {
	return func() tea.Msg {
		cfgs, err := mgr.List()
		if err != nil {
			return vmActionErrMsg{err}
		}
		statuses := make(map[string]vm.VMStatus, len(cfgs))
		for _, c := range cfgs {
			info, _ := vm.Status(mgr.StoragePath, c.Name)
			statuses[c.Name] = info.Status
		}
		return vmListLoadedMsg{cfgs: cfgs, statuses: statuses}
	}
}

func startVMCmd(mgr *vm.Manager, cfg *vm.VMConfig) tea.Cmd {
	return func() tea.Msg {
		if err := vm.Start(mgr.StoragePath, cfg); err != nil {
			return vmActionErrMsg{err}
		}
		return vmActionOKMsg{name: cfg.Name}
	}
}

func stopVMCmd(mgr *vm.Manager, name string) tea.Cmd {
	return func() tea.Msg {
		if err := vm.Stop(mgr.StoragePath, name); err != nil {
			return vmActionErrMsg{err}
		}
		return vmActionOKMsg{name: name}
	}
}

func deleteVMCmd(mgr *vm.Manager, name string) tea.Cmd {
	return func() tea.Msg {
		if err := mgr.Delete(name); err != nil {
			return vmActionErrMsg{err}
		}
		return vmActionOKMsg{name: name}
	}
}

// --- list model ---

// VMListModel is the main screen showing all VMs with their running status.
type VMListModel struct {
	mgr      *vm.Manager
	cfgs     []*vm.VMConfig
	statuses map[string]vm.VMStatus
	cursor   int
	loading  bool
	err      string
	status   string // transient status message
	// delete confirmation
	confirming bool
	confirmName string
	width  int
	height int
}

// NewVMListModel creates a VMListModel; VMs are loaded lazily via Init.
func NewVMListModel(mgr *vm.Manager, width, height int) VMListModel {
	return VMListModel{
		mgr:      mgr,
		statuses: make(map[string]vm.VMStatus),
		loading:  true,
		width:    width,
		height:   height,
	}
}

func (m *VMListModel) setSize(w, h int) {
	m.width, m.height = w, h
}

func (m VMListModel) Init() tea.Cmd {
	return loadVMsCmd(m.mgr)
}

func (m VMListModel) Update(msg tea.Msg) (VMListModel, tea.Cmd) {
	switch msg := msg.(type) {
	case vmListLoadedMsg:
		m.loading = false
		m.cfgs = msg.cfgs
		m.statuses = msg.statuses
		if m.cursor >= len(m.cfgs) {
			m.cursor = max(0, len(m.cfgs)-1)
		}

	case vmActionErrMsg:
		m.err = msg.err.Error()
		m.loading = false
		return m, loadVMsCmd(m.mgr)

	case vmActionOKMsg:
		m.status = fmt.Sprintf("✓ %s", msg.name)
		return m, loadVMsCmd(m.mgr)

	case tea.KeyMsg:
		if m.confirming {
			return m.handleConfirmKey(msg)
		}
		return m.handleKey(msg)
	}
	return m, nil
}

func (m VMListModel) handleKey(msg tea.KeyMsg) (VMListModel, tea.Cmd) {
	switch msg.String() {
	case "q":
		return m, tea.Quit
	case "up", "k":
		if m.cursor > 0 {
			m.cursor--
		}
	case "down", "j":
		if m.cursor < len(m.cfgs)-1 {
			m.cursor++
		}
	case "g":
		m.cursor = 0
	case "G":
		if len(m.cfgs) > 0 {
			m.cursor = len(m.cfgs) - 1
		}
	case "ctrl+d":
		step := max(1, (m.height-6)/4)
		m.cursor = min(len(m.cfgs)-1, m.cursor+step)
	case "ctrl+u":
		step := max(1, (m.height-6)/4)
		m.cursor = max(0, m.cursor-step)
	case "n":
		return m, func() tea.Msg { return NavigateMsg{To: screenCreate} }
	case "enter", "l":
		if len(m.cfgs) > 0 {
			name := m.cfgs[m.cursor].Name
			return m, func() tea.Msg { return NavigateMsg{To: screenDetail, VMName: name} }
		}
	case "e":
		if len(m.cfgs) > 0 {
			name := m.cfgs[m.cursor].Name
			return m, func() tea.Msg { return NavigateMsg{To: screenEdit, VMName: name} }
		}
	case "s":
		if len(m.cfgs) > 0 {
			cfg := m.cfgs[m.cursor]
			m.loading = true
			m.err = ""
			m.status = ""
			return m, startVMCmd(m.mgr, cfg)
		}
	case "x":
		if len(m.cfgs) > 0 {
			name := m.cfgs[m.cursor].Name
			m.loading = true
			m.err = ""
			m.status = ""
			return m, stopVMCmd(m.mgr, name)
		}
	case "d":
		if len(m.cfgs) > 0 {
			m.confirming = true
			m.confirmName = m.cfgs[m.cursor].Name
			m.err = ""
		}
	case "r":
		m.loading = true
		m.err = ""
		return m, loadVMsCmd(m.mgr)
	}
	return m, nil
}

func (m VMListModel) handleConfirmKey(msg tea.KeyMsg) (VMListModel, tea.Cmd) {
	switch msg.String() {
	case "y", "Y":
		m.confirming = false
		name := m.confirmName
		m.confirmName = ""
		m.loading = true
		return m, deleteVMCmd(m.mgr, name)
	default:
		m.confirming = false
		m.confirmName = ""
	}
	return m, nil
}

func (m VMListModel) View() string {
	if m.width == 0 {
		return ""
	}

	var b strings.Builder

	// Header
	header := styleHeader.Copy().Width(m.width).Render("  Ostrich — Virtual Machines")
	b.WriteString(header)
	b.WriteString("\n\n")

	if m.loading {
		b.WriteString(styleHelp.Render("  Loading..."))
		b.WriteString("\n")
	} else if len(m.cfgs) == 0 {
		b.WriteString(styleHelp.Render("  No VMs yet. Press n to create one."))
		b.WriteString("\n")
	} else {
		for i, cfg := range m.cfgs {
			status := m.statuses[cfg.Name]
			statusBadge := styleStopped.Render("● stopped")
			if status == vm.StatusRunning {
				statusBadge = styleRunning.Render("● running")
			}

			line := fmt.Sprintf("%-20s  %s   CPU: %d  RAM: %d MiB  Disk: %d GiB",
				cfg.Name, statusBadge, cfg.CPU, cfg.RAM, cfg.DiskSize)

			if i == m.cursor {
				b.WriteString(styleSelected.Copy().Width(m.width - 2).Render("  " + line))
			} else {
				b.WriteString(styleNormal.Copy().Width(m.width - 2).Render("  " + line))
			}
			b.WriteString("\n")
		}
	}

	// Status / error / confirm area
	b.WriteString("\n")
	if m.confirming {
		b.WriteString(styleError.Render(fmt.Sprintf("  Delete %q? Press y to confirm, any other key to cancel.", m.confirmName)))
		b.WriteString("\n")
	} else if m.err != "" {
		b.WriteString(styleError.Render("  ✗ " + m.err))
		b.WriteString("\n")
	} else if m.status != "" {
		b.WriteString(styleSuccess.Render("  " + m.status))
		b.WriteString("\n")
	}

	// Help bar
	b.WriteString("\n")
	helpItems := []string{
		"j/k: navigate",
		"g/G: top/bottom",
		"^d/^u: half-page",
		"l/enter: open",
		"n: new",
		"e: edit",
		"s: start",
		"x: stop",
		"d: delete",
		"r: refresh",
		"q: quit",
	}
	b.WriteString(styleHelp.Render("  " + strings.Join(helpItems, "  ")))

	return b.String()
}

func max(a, b int) int {
	if a > b {
		return a
	}
	return b
}

func min(a, b int) int {
	if a < b {
		return a
	}
	return b
}
