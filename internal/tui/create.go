package tui

import (
	"fmt"
	"strconv"
	"strings"

	"github.com/charmbracelet/bubbles/textinput"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
	"github.com/pzindyaev/ostrich/internal/vm"
)

type createStep int

const (
	stepName    createStep = iota // text input 0
	stepCPU                       // text input 1
	stepRAM                       // text input 2
	stepDisk                      // text input 3
	stepISO                       // text input 4
	stepNetwork                   // selector (no text input)
	stepVNC                       // text input 5
	stepConfirm                   // confirmation
	stepCount
)

var stepLabels = []string{
	"VM Name",
	"CPU Cores",
	"RAM (MiB)",
	"Disk Size (GiB)",
	"Boot ISO path (optional — leave blank to skip)",
	"Network type",
	"VNC Display Number (0 = disabled)",
	"Confirm",
}

var stepHelp = []string{
	"Alphanumeric and hyphens only, e.g. debian-12",
	"Number of virtual CPU cores, e.g. 2",
	"Memory in MiB, e.g. 2048 for 2 GiB",
	"Disk size in GiB, e.g. 20",
	"Full path to an ISO image for initial install, or leave blank",
	"h/l/←/→ to select: user (NAT) · tap (bridge) · none",
	"Display number 1–99 (TCP port = 5900+n). 0 to disable. Connect with vncviewer 127.0.0.1:<n>",
	"Press Enter to create the VM",
}

var networkChoices = []vm.NetworkType{vm.NetworkUser, vm.NetworkTap, vm.NetworkNone}
var networkLabels = []string{"user (NAT)", "tap (bridge)", "none"}

// inputForStep maps a step to its index in the inputs array, or -1 for non-text steps.
func inputForStep(s createStep) int {
	switch s {
	case stepName:
		return 0
	case stepCPU:
		return 1
	case stepRAM:
		return 2
	case stepDisk:
		return 3
	case stepISO:
		return 4
	case stepVNC:
		return 5
	default:
		return -1
	}
}

// vmCreatedMsg is sent after a successful VM creation.
type vmCreatedMsg struct{ name string }
type vmCreateErrMsg struct{ err error }

// CreateVMModel is a linear multi-step form for defining a new VM.
type CreateVMModel struct {
	step   createStep
	inputs [6]textinput.Model // name, cpu, ram, disk, iso, vnc
	netIdx int
	err    string
	mgr    *vm.Manager
	width  int
	height int
}

// NewCreateVMModel builds a CreateVMModel with sensible defaults.
func NewCreateVMModel(mgr *vm.Manager, width, height int) CreateVMModel {
	defaults := []string{"my-vm", "2", "2048", "20", "", "0"}

	var inputs [6]textinput.Model
	for i := range inputs {
		t := textinput.New()
		t.SetValue(defaults[i])
		t.CharLimit = 256
		t.Width = 45
		inputs[i] = t
	}

	return CreateVMModel{
		step:   stepName,
		inputs: inputs,
		mgr:    mgr,
		width:  width,
		height: height,
	}
}

func (m CreateVMModel) Init() tea.Cmd {
	return m.inputs[0].Focus()
}

func (m CreateVMModel) Update(msg tea.Msg) (CreateVMModel, tea.Cmd) {
	switch msg := msg.(type) {
	case vmCreatedMsg:
		return m, func() tea.Msg { return NavigateMsg{To: screenList} }

	case vmCreateErrMsg:
		m.err = msg.err.Error()
		return m, nil

	case tea.KeyMsg:
		return m.handleKey(msg)
	}

	// Forward non-key messages to the active text input (if any).
	if idx := inputForStep(m.step); idx >= 0 {
		var cmd tea.Cmd
		m.inputs[idx], cmd = m.inputs[idx].Update(msg)
		return m, cmd
	}
	return m, nil
}

func (m CreateVMModel) handleKey(msg tea.KeyMsg) (CreateVMModel, tea.Cmd) {
	switch msg.String() {
	case "esc":
		return m, func() tea.Msg { return NavigateMsg{To: screenList} }

	case "ctrl+c":
		return m, tea.Quit

	case "tab", "enter", "down":
		return m.advance()

	case "shift+tab", "up":
		return m.retreat()

	case "j":
		// vim-next: only when no text input is active
		if inputForStep(m.step) < 0 {
			return m.advance()
		}

	case "k":
		// vim-prev: only when no text input is active
		if inputForStep(m.step) < 0 {
			return m.retreat()
		}

	case "h", "left":
		if m.step == stepNetwork {
			m.netIdx = (m.netIdx - 1 + len(networkChoices)) % len(networkChoices)
		}

	case "l", "right":
		if m.step == stepNetwork {
			m.netIdx = (m.netIdx + 1) % len(networkChoices)
		}
	}

	// Forward keystrokes to active text input.
	if idx := inputForStep(m.step); idx >= 0 {
		var cmd tea.Cmd
		m.inputs[idx], cmd = m.inputs[idx].Update(msg)
		return m, cmd
	}
	return m, nil
}

func (m CreateVMModel) advance() (CreateVMModel, tea.Cmd) {
	if idx := inputForStep(m.step); idx >= 0 {
		if err := m.validateStep(m.step); err != nil {
			m.err = err.Error()
			return m, nil
		}
		m.err = ""
		m.inputs[idx].Blur()
	} else {
		m.err = ""
	}

	if m.step == stepConfirm {
		return m.createVM()
	}

	m.step++
	if idx := inputForStep(m.step); idx >= 0 {
		return m, m.inputs[idx].Focus()
	}
	return m, nil
}

func (m CreateVMModel) retreat() (CreateVMModel, tea.Cmd) {
	if m.step == 0 {
		return m, func() tea.Msg { return NavigateMsg{To: screenList} }
	}
	if idx := inputForStep(m.step); idx >= 0 {
		m.inputs[idx].Blur()
	}
	m.step--
	m.err = ""
	if idx := inputForStep(m.step); idx >= 0 {
		return m, m.inputs[idx].Focus()
	}
	return m, nil
}

func (m CreateVMModel) validateStep(s createStep) error {
	idx := inputForStep(s)
	if idx < 0 {
		return nil
	}
	val := strings.TrimSpace(m.inputs[idx].Value())
	switch s {
	case stepName:
		if val == "" {
			return fmt.Errorf("name cannot be empty")
		}
		for _, c := range val {
			if !((c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') ||
				(c >= '0' && c <= '9') || c == '-' || c == '_') {
				return fmt.Errorf("name may only contain letters, digits, hyphens and underscores")
			}
		}
		if m.mgr.Exists(val) {
			return fmt.Errorf("a VM named %q already exists", val)
		}
	case stepCPU:
		v, err := strconv.Atoi(val)
		if err != nil || v < 1 {
			return fmt.Errorf("CPU must be a positive integer")
		}
	case stepRAM:
		v, err := strconv.Atoi(val)
		if err != nil || v < 64 {
			return fmt.Errorf("RAM must be at least 64 MiB")
		}
	case stepDisk:
		v, err := strconv.Atoi(val)
		if err != nil || v < 1 {
			return fmt.Errorf("disk size must be at least 1 GiB")
		}
	case stepVNC:
		v, err := strconv.Atoi(val)
		if err != nil || v < 0 || v > 99 {
			return fmt.Errorf("VNC display must be 0 (disabled) or 1–99")
		}
	}
	return nil
}

func (m CreateVMModel) buildConfig() (*vm.VMConfig, error) {
	name := strings.TrimSpace(m.inputs[inputForStep(stepName)].Value())
	cpu, err := strconv.Atoi(strings.TrimSpace(m.inputs[inputForStep(stepCPU)].Value()))
	if err != nil {
		return nil, fmt.Errorf("invalid CPU value")
	}
	ram, err := strconv.Atoi(strings.TrimSpace(m.inputs[inputForStep(stepRAM)].Value()))
	if err != nil {
		return nil, fmt.Errorf("invalid RAM value")
	}
	disk, err := strconv.Atoi(strings.TrimSpace(m.inputs[inputForStep(stepDisk)].Value()))
	if err != nil {
		return nil, fmt.Errorf("invalid disk size")
	}
	iso := strings.TrimSpace(m.inputs[inputForStep(stepISO)].Value())
	vnc, _ := strconv.Atoi(strings.TrimSpace(m.inputs[inputForStep(stepVNC)].Value()))
	netType := networkChoices[m.netIdx]

	return &vm.VMConfig{
		Name:      name,
		CPU:       cpu,
		RAM:       ram,
		DiskSize:  disk,
		CDROMPath: iso,
		VNCPort:   vnc,
		Network:   vm.NetworkConfig{Type: netType},
	}, nil
}

func (m CreateVMModel) createVM() (CreateVMModel, tea.Cmd) {
	cfg, err := m.buildConfig()
	if err != nil {
		m.err = err.Error()
		return m, nil
	}
	return m, func() tea.Msg {
		if err := m.mgr.Create(cfg); err != nil {
			return vmCreateErrMsg{err}
		}
		return vmCreatedMsg{name: cfg.Name}
	}
}

func (m CreateVMModel) View() string {
	if m.width == 0 {
		return ""
	}

	var b strings.Builder

	header := styleHeader.Copy().Width(m.width).Render("  Ostrich — Create VM")
	b.WriteString(header)
	b.WriteString("\n\n")

	total := int(stepCount)
	cur := int(m.step)
	b.WriteString(styleHelp.Render(fmt.Sprintf("  Step %d / %d", cur+1, total)))
	b.WriteString("\n\n")

	label := stepLabels[m.step]
	hint := stepHelp[m.step]
	b.WriteString(styleLabel.Render("  " + label))
	b.WriteString("\n")
	b.WriteString(styleHelp.Render("  " + hint))
	b.WriteString("\n\n")

	switch m.step {
	case stepNetwork:
		var opts []string
		for i, lbl := range networkLabels {
			if i == m.netIdx {
				opts = append(opts, styleSelected.Render(" "+lbl+" "))
			} else {
				opts = append(opts, styleNormal.Render(" "+lbl+" "))
			}
		}
		b.WriteString("  ")
		b.WriteString(strings.Join(opts, "  "))
		b.WriteString("\n\n")

	case stepConfirm:
		cfg, err := m.buildConfig()
		if err == nil {
			vncStr := "disabled"
			if cfg.VNCPort > 0 {
				vncStr = fmt.Sprintf("display %d (port %d)", cfg.VNCPort, 5900+cfg.VNCPort)
			}
			summary := fmt.Sprintf(
				"  Name: %s\n  CPU:  %d cores\n  RAM:  %d MiB\n  Disk: %d GiB\n  ISO:  %s\n  Net:  %s\n  VNC:  %s",
				cfg.Name, cfg.CPU, cfg.RAM, cfg.DiskSize,
				ifEmpty(cfg.CDROMPath, "(none)"),
				cfg.Network.Type,
				vncStr,
			)
			b.WriteString(styleBox.Render(summary))
			b.WriteString("\n\n")
		}

	default:
		if idx := inputForStep(m.step); idx >= 0 {
			b.WriteString("  ")
			b.WriteString(m.inputs[idx].View())
			b.WriteString("\n\n")
		}
	}

	if m.err != "" {
		b.WriteString(styleError.Render("  ✗ " + m.err))
		b.WriteString("\n\n")
	}

	keyHelp := "Tab/j/↓: next   Shift+Tab/k/↑: back   Esc: cancel"
	if m.step == stepNetwork {
		keyHelp = "h/l/←/→: select   j/↓: next   k/↑: back   Esc: cancel"
	} else if m.step == stepConfirm {
		keyHelp = "Enter/j: create VM   k/Shift+Tab: back   Esc: cancel"
	}
	b.WriteString(styleHelp.Render("  " + keyHelp))

	return lipgloss.NewStyle().Width(m.width).Render(b.String())
}

func ifEmpty(s, fallback string) string {
	if s == "" {
		return fallback
	}
	return s
}
