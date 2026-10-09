package tui

import (
	"fmt"
	"strings"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
	"github.com/pzindyaev/ostrich/internal/vm"
)

// --- messages ---

type templatesLoadedMsg struct {
	tpls  []*vm.Template
	usage map[string]int64 // disk image size on the host, by template name
}

type templateActionErrMsg struct{ err error }
type templateActionOKMsg struct{ notice string }

// --- tea.Cmd helpers ---

func loadTemplatesCmd(mgr *vm.Manager) tea.Cmd {
	return func() tea.Msg {
		tpls, err := mgr.ListTemplates()
		if err != nil {
			return templateActionErrMsg{err}
		}
		usage := make(map[string]int64, len(tpls))
		for _, t := range tpls {
			usage[t.Name] = vm.TemplateDiskUsage(mgr.StoragePath, t.Name)
		}
		return templatesLoadedMsg{tpls: tpls, usage: usage}
	}
}

func deleteTemplateCmd(mgr *vm.Manager, name string) tea.Cmd {
	return func() tea.Msg {
		if err := mgr.DeleteTemplate(name); err != nil {
			return templateActionErrMsg{err}
		}
		return templateActionOKMsg{"deleted template " + name}
	}
}

// --- model ---

// TemplatesModel lists the VM templates. From here a template becomes a new
// VM, or is deleted.
type TemplatesModel struct {
	mgr     *vm.Manager
	tpls    []*vm.Template
	usage   map[string]int64
	cursor  int
	loading bool
	err     string
	status  string
	// delete confirmation
	confirming  bool
	confirmName string
	width       int
	height      int
}

// NewTemplatesModel creates the screen; templates are loaded in Init.
func NewTemplatesModel(mgr *vm.Manager, width, height int) TemplatesModel {
	return TemplatesModel{
		mgr:     mgr,
		usage:   map[string]int64{},
		loading: true,
		width:   width,
		height:  height,
	}
}

func (m TemplatesModel) Init() tea.Cmd {
	return loadTemplatesCmd(m.mgr)
}

func (m TemplatesModel) Update(msg tea.Msg) (TemplatesModel, tea.Cmd) {
	switch msg := msg.(type) {
	case templatesLoadedMsg:
		m.loading = false
		m.tpls = msg.tpls
		m.usage = msg.usage
		if m.cursor >= len(m.tpls) {
			m.cursor = max(0, len(m.tpls)-1)
		}

	case templateActionErrMsg:
		m.err = msg.err.Error()
		m.loading = false
		return m, loadTemplatesCmd(m.mgr)

	case templateActionOKMsg:
		m.status = "✓ " + msg.notice
		return m, loadTemplatesCmd(m.mgr)

	case tea.KeyMsg:
		if m.confirming {
			return m.handleConfirmKey(msg)
		}
		return m.handleKey(msg)
	}
	return m, nil
}

func (m TemplatesModel) handleKey(msg tea.KeyMsg) (TemplatesModel, tea.Cmd) {
	switch msg.String() {
	case "esc", "q", "h":
		return m, func() tea.Msg { return NavigateMsg{To: screenList} }
	case "ctrl+c":
		return m, tea.Quit
	case "up", "k":
		if m.cursor > 0 {
			m.cursor--
		}
	case "down", "j":
		if m.cursor < len(m.tpls)-1 {
			m.cursor++
		}
	case "g":
		m.cursor = 0
	case "G":
		if len(m.tpls) > 0 {
			m.cursor = len(m.tpls) - 1
		}
	case "enter", "l", "n":
		if len(m.tpls) > 0 {
			name := m.tpls[m.cursor].Name
			return m, func() tea.Msg { return NavigateMsg{To: screenFromTemplate, Template: name} }
		}
	case "d":
		if len(m.tpls) > 0 {
			m.confirming = true
			m.confirmName = m.tpls[m.cursor].Name
			m.err, m.status = "", ""
		}
	case "r":
		m.loading = true
		m.err, m.status = "", ""
		return m, loadTemplatesCmd(m.mgr)
	}
	return m, nil
}

func (m TemplatesModel) handleConfirmKey(msg tea.KeyMsg) (TemplatesModel, tea.Cmd) {
	name := m.confirmName
	m.confirming = false
	m.confirmName = ""
	switch msg.String() {
	case "y", "Y":
		m.loading = true
		return m, deleteTemplateCmd(m.mgr, name)
	}
	return m, nil
}

func (m TemplatesModel) View() string {
	if m.width == 0 {
		return ""
	}

	var b strings.Builder

	header := styleHeader.Copy().Width(m.width).Render("  Ostrich — VM Templates")
	b.WriteString(header)
	b.WriteString("\n\n")

	switch {
	case m.loading && len(m.tpls) == 0:
		b.WriteString(styleHelp.Render("  Loading..."))
		b.WriteString("\n")
	case len(m.tpls) == 0:
		b.WriteString(styleHelp.Render("  No templates yet. Press t on a stopped VM in the list to save it as a template."))
		b.WriteString("\n")
	default:
		for i, t := range m.tpls {
			line := fmt.Sprintf("%-20s  CPU: %d  RAM: %d MiB  Disk: %d GiB   %-28s %s",
				t.Name, t.CPU, t.RAM, t.DiskSize, t.FirmwareLabel(), t.Network)
			if i == m.cursor {
				b.WriteString(styleSelected.Copy().Width(m.width - 2).Render("  " + line))
			} else {
				b.WriteString(styleNormal.Copy().Width(m.width - 2).Render("  " + line))
			}
			b.WriteString("\n")
		}
		b.WriteString("\n")
		b.WriteString(styleBox.Copy().Width(m.width - 2).Render(m.selectedInfo()))
		b.WriteString("\n")
	}

	// Status / error / confirm area
	b.WriteString("\n")
	if m.confirming {
		b.WriteString(styleError.Render(fmt.Sprintf("  Delete template %q? VMs made from it are not affected. Press y to confirm, any other key to cancel.", m.confirmName)))
		b.WriteString("\n")
	} else if m.err != "" {
		b.WriteString(styleError.Render(indent("✗ "+m.err, "  ")))
		b.WriteString("\n")
	} else if m.status != "" {
		b.WriteString(styleSuccess.Render("  " + m.status))
		b.WriteString("\n")
	}

	b.WriteString("\n")
	helpItems := []string{
		"j/k: navigate",
		"g/G: top/bottom",
		"l/enter/n: new VM from template",
		"d: delete",
		"r: refresh",
		"q/h/Esc: back",
	}
	b.WriteString(styleHelp.Render("  " + strings.Join(helpItems, "  ")))

	return lipgloss.NewStyle().Width(m.width).Render(b.String())
}

// selectedInfo describes the template under the cursor.
func (m TemplatesModel) selectedInfo() string {
	t := m.tpls[m.cursor]
	onDisk := "unknown"
	if n := m.usage[t.Name]; n > 0 {
		onDisk = humanSize(n)
	}
	vnc := "disabled"
	if t.VNC {
		vnc = "enabled — a new VM gets a free display number"
	}
	return fmt.Sprintf(
		"  Template: %s\n  About:    %s\n  From VM:  %s, saved %s\n  Disk:     %d GiB virtual, %s on the host\n  Firmware: %s\n  Defaults: %d cores, %d MiB RAM, %s network — chosen anew for each VM\n  VNC:      %s",
		t.Name, ifEmpty(t.Description, "(no description)"),
		t.SourceVM, t.CreatedAt.Local().Format("2006-01-02 15:04"),
		t.DiskSize, onDisk,
		t.FirmwareLabel(),
		t.CPU, t.RAM, t.Network,
		vnc,
	)
}
