package tui

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/charmbracelet/bubbles/textinput"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
	"github.com/pzindyaev/ostrich/internal/config"
)

// SetupModel is the first-run wizard that asks for the VM storage directory.
type SetupModel struct {
	input  textinput.Model
	err    string
	width  int
	height int
}

// NewSetupModel constructs a SetupModel pre-filled with ~/VMs.
func NewSetupModel(width, height int) SetupModel {
	ti := textinput.New()
	ti.Placeholder = "e.g. ~/VMs"
	ti.CharLimit = 256
	ti.Width = 50

	home, _ := os.UserHomeDir()
	ti.SetValue(filepath.Join(home, "VMs"))

	return SetupModel{
		input:  ti,
		width:  width,
		height: height,
	}
}

func (m SetupModel) Init() tea.Cmd {
	return m.input.Focus()
}

func (m SetupModel) Update(msg tea.Msg) (SetupModel, tea.Cmd) {
	switch msg := msg.(type) {
	case tea.KeyMsg:
		switch msg.String() {
		case "enter":
			return m.submit()
		case "ctrl+c":
			return m, tea.Quit
		}
	}

	var cmd tea.Cmd
	m.input, cmd = m.input.Update(msg)
	return m, cmd
}

func (m SetupModel) submit() (SetupModel, tea.Cmd) {
	val := strings.TrimSpace(m.input.Value())
	if val == "" {
		m.err = "path cannot be empty"
		return m, nil
	}

	// Expand ~/
	if strings.HasPrefix(val, "~/") {
		home, _ := os.UserHomeDir()
		val = filepath.Join(home, val[2:])
	}

	if err := os.MkdirAll(val, 0755); err != nil {
		m.err = fmt.Sprintf("cannot create directory: %v", err)
		return m, nil
	}

	cfg := &config.AppConfig{VMStoragePath: val}
	if err := config.Save(cfg); err != nil {
		m.err = fmt.Sprintf("cannot save config: %v", err)
		return m, nil
	}

	return m, func() tea.Msg { return NavigateMsg{To: screenList} }
}

func (m SetupModel) View() string {
	var b strings.Builder
	b.WriteString("\n")
	b.WriteString(styleTitle.Render("  Ostrich — QEMU Manager"))
	b.WriteString("\n")
	b.WriteString(styleSubtitle.Render("  First-run setup"))
	b.WriteString("\n\n")

	b.WriteString(styleLabel.Render("VM Storage Directory"))
	b.WriteString("\n")
	b.WriteString(styleHelp.Render("Each VM will get its own sub-folder here containing its disk and config."))
	b.WriteString("\n\n")

	b.WriteString("  ")
	b.WriteString(m.input.View())
	b.WriteString("\n\n")

	if m.err != "" {
		b.WriteString("  ")
		b.WriteString(styleError.Render("✗ "+m.err))
		b.WriteString("\n\n")
	}

	b.WriteString(styleHelp.Render("  Enter — confirm   Ctrl+C — quit"))

	content := b.String()
	if m.width > 0 {
		return lipgloss.NewStyle().Width(m.width).Padding(2, 2).Render(content)
	}
	return lipgloss.NewStyle().Padding(2, 2).Render(content)
}
