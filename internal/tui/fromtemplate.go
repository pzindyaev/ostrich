package tui

import (
	"fmt"
	"strconv"
	"strings"

	"github.com/charmbracelet/bubbles/spinner"
	"github.com/charmbracelet/bubbles/textinput"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
	"github.com/pzindyaev/ostrich/internal/vm"
)

type tplStep int

const (
	tplStepName     tplStep = iota // text input 0
	tplStepCPU                     // text input 1
	tplStepRAM                     // text input 2
	tplStepNetwork                 // selector
	tplStepForwards                // text input 3
	tplStepConfirm                 // confirmation
	tplStepCount
)

var tplStepLabels = []string{
	"VM Name",
	"CPU Cores",
	"RAM (MiB)",
	"Network type",
	"Port forwards (optional — leave blank for none)",
	"Confirm",
}

var tplStepHelp = []string{
	"Alphanumeric and hyphens only, e.g. debian-12-test",
	"Number of virtual CPU cores, e.g. 2",
	"Memory in MiB, e.g. 2048 for 2 GiB",
	"h/l/←/→ to select: user (NAT) · tap (bridge) · none",
	"user networking only. Comma-separated [tcp|udp:]host:guest, e.g. 2222:22, udp:5353:53 — pick ports no other VM uses",
	"Press Enter to create the VM. The disk is copied from the template, which takes a while for a large disk",
}

// tplInputForStep maps a step to its index in the inputs array, or -1 for non-text steps.
func tplInputForStep(s tplStep) int {
	switch s {
	case tplStepName:
		return 0
	case tplStepCPU:
		return 1
	case tplStepRAM:
		return 2
	case tplStepForwards:
		return 3
	default:
		return -1
	}
}

// FromTemplateModel is the short wizard that makes a new VM from a template:
// the disk, firmware and TPM come from the template; the name, CPU, RAM and
// network are chosen here.
type FromTemplateModel struct {
	tpl        *vm.Template
	mgr        *vm.Manager
	step       tplStep
	inputs     [4]textinput.Model // name, cpu, ram, forwards
	netIdx     int
	bridgeHint string // what the host lacks for tap networking; "" when ready or not picked
	vncDisplay int    // the display the new VM gets when the template has VNC
	busy       bool   // the copy is in flight
	spin       spinner.Model
	err        string
	width      int
	height     int
}

// NewFromTemplateModel builds the wizard pre-filled with the template's defaults.
func NewFromTemplateModel(mgr *vm.Manager, tpl *vm.Template, width, height int) FromTemplateModel {
	defaults := []string{suggestVMName(mgr, tpl.Name), strconv.Itoa(tpl.CPU), strconv.Itoa(tpl.RAM), ""}

	var inputs [4]textinput.Model
	for i := range inputs {
		t := textinput.New()
		t.SetValue(defaults[i])
		t.CharLimit = 256
		t.Width = 45
		inputs[i] = t
	}
	inputs[0].Focus()

	netIdx := 0
	for i, nt := range networkChoices {
		if nt == tpl.Network {
			netIdx = i
		}
	}

	m := FromTemplateModel{
		tpl:        tpl,
		mgr:        mgr,
		step:       tplStepName,
		inputs:     inputs,
		netIdx:     netIdx,
		bridgeHint: bridgeHintFor(networkChoices[netIdx]),
		spin:       spinner.New(spinner.WithSpinner(spinner.MiniDot), spinner.WithStyle(styleLabel)),
		width:      width,
		height:     height,
	}
	if tpl.VNC {
		m.vncDisplay = mgr.FreeVNCDisplay()
	}
	return m
}

// suggestVMName returns "<base>-1", or the first "<base>-N" not taken by a VM.
func suggestVMName(mgr *vm.Manager, base string) string {
	for n := 1; ; n++ {
		name := fmt.Sprintf("%s-%d", base, n)
		if !mgr.Exists(name) {
			return name
		}
	}
}

func (m FromTemplateModel) Init() tea.Cmd {
	return textinput.Blink
}

func (m FromTemplateModel) Update(msg tea.Msg) (FromTemplateModel, tea.Cmd) {
	switch msg := msg.(type) {
	case vmCreatedMsg:
		return m, func() tea.Msg { return NavigateMsg{To: screenList} }

	case vmCreateErrMsg:
		m.busy = false
		m.err = msg.err.Error()
		return m, nil

	case spinner.TickMsg:
		if !m.busy {
			return m, nil
		}
		var cmd tea.Cmd
		m.spin, cmd = m.spin.Update(msg)
		return m, cmd

	case tea.KeyMsg:
		if m.busy {
			if msg.String() == "ctrl+c" {
				return m, tea.Quit
			}
			return m, nil // the copy cannot be interrupted from here
		}
		return m.handleKey(msg)
	}

	if idx := tplInputForStep(m.step); idx >= 0 {
		var cmd tea.Cmd
		m.inputs[idx], cmd = m.inputs[idx].Update(msg)
		return m, cmd
	}
	return m, nil
}

func (m FromTemplateModel) handleKey(msg tea.KeyMsg) (FromTemplateModel, tea.Cmd) {
	switch msg.String() {
	case "esc":
		return m, func() tea.Msg { return NavigateMsg{To: screenTemplates} }

	case "ctrl+c":
		return m, tea.Quit

	case "tab", "enter", "down":
		return m.advance()

	case "shift+tab", "up":
		return m.retreat()

	case "j":
		if tplInputForStep(m.step) < 0 {
			return m.advance()
		}

	case "k":
		if tplInputForStep(m.step) < 0 {
			return m.retreat()
		}

	case "h", "left":
		if m.step == tplStepNetwork {
			m.netIdx = wrap(m.netIdx-1, len(networkChoices))
			m.bridgeHint = bridgeHintFor(networkChoices[m.netIdx])
		}

	case "l", "right":
		if m.step == tplStepNetwork {
			m.netIdx = wrap(m.netIdx+1, len(networkChoices))
			m.bridgeHint = bridgeHintFor(networkChoices[m.netIdx])
		}
	}

	if idx := tplInputForStep(m.step); idx >= 0 {
		var cmd tea.Cmd
		m.inputs[idx], cmd = m.inputs[idx].Update(msg)
		return m, cmd
	}
	return m, nil
}

func (m FromTemplateModel) advance() (FromTemplateModel, tea.Cmd) {
	if idx := tplInputForStep(m.step); idx >= 0 {
		if err := m.validateStep(m.step); err != nil {
			m.err = err.Error()
			return m, nil
		}
		m.inputs[idx].Blur()
	}
	m.err = ""

	if m.step == tplStepConfirm {
		return m.createVM()
	}

	m.step++
	if idx := tplInputForStep(m.step); idx >= 0 {
		return m, m.inputs[idx].Focus()
	}
	return m, nil
}

func (m FromTemplateModel) retreat() (FromTemplateModel, tea.Cmd) {
	if m.step == 0 {
		return m, func() tea.Msg { return NavigateMsg{To: screenTemplates} }
	}
	if idx := tplInputForStep(m.step); idx >= 0 {
		m.inputs[idx].Blur()
	}
	m.step--
	m.err = ""
	if idx := tplInputForStep(m.step); idx >= 0 {
		return m, m.inputs[idx].Focus()
	}
	return m, nil
}

func (m FromTemplateModel) value(s tplStep) string {
	return strings.TrimSpace(m.inputs[tplInputForStep(s)].Value())
}

func (m FromTemplateModel) validateStep(s tplStep) error {
	switch s {
	case tplStepName:
		val := m.value(s)
		if err := validateVMName(val); err != nil {
			return err
		}
		if m.mgr.Exists(val) {
			return fmt.Errorf("a VM named %q already exists", val)
		}
	case tplStepCPU:
		v, err := strconv.Atoi(m.value(s))
		if err != nil || v < 1 {
			return fmt.Errorf("CPU must be a positive integer")
		}
	case tplStepRAM:
		v, err := strconv.Atoi(m.value(s))
		if err != nil || v < 64 {
			return fmt.Errorf("RAM must be at least 64 MiB")
		}
	case tplStepForwards:
		fwds, err := vm.ParsePortForwards(m.value(s))
		if err != nil {
			return err
		}
		if len(fwds) > 0 && networkChoices[m.netIdx] != vm.NetworkUser {
			return fmt.Errorf("port forwards need user (NAT) networking — go back and pick it, or leave this blank")
		}
	}
	return nil
}

// buildConfig returns the config for the new VM: the user's choices on top of
// the template's machine. A template whose source had a VNC display gives
// the new VM the lowest display no other VM used when the wizard opened.
func (m FromTemplateModel) buildConfig() (*vm.VMConfig, error) {
	cfg := m.tpl.NewVMConfig()
	cfg.Name = m.value(tplStepName)
	var err error
	if cfg.CPU, err = strconv.Atoi(m.value(tplStepCPU)); err != nil {
		return nil, fmt.Errorf("invalid CPU value")
	}
	if cfg.RAM, err = strconv.Atoi(m.value(tplStepRAM)); err != nil {
		return nil, fmt.Errorf("invalid RAM value")
	}
	cfg.Network.Type = networkChoices[m.netIdx]
	if cfg.Network.PortForwards, err = vm.ParsePortForwards(m.value(tplStepForwards)); err != nil {
		return nil, err
	}
	cfg.VNCPort = m.vncDisplay
	return cfg, nil
}

func (m FromTemplateModel) createVM() (FromTemplateModel, tea.Cmd) {
	cfg, err := m.buildConfig()
	if err != nil {
		m.err = err.Error()
		return m, nil
	}
	m.busy = true
	mgr, tplName := m.mgr, m.tpl.Name
	return m, tea.Batch(m.spin.Tick, func() tea.Msg {
		if err := mgr.CreateFromTemplate(tplName, cfg); err != nil {
			return vmCreateErrMsg{err}
		}
		return vmCreatedMsg{name: cfg.Name}
	})
}

func (m FromTemplateModel) View() string {
	if m.width == 0 {
		return ""
	}

	var b strings.Builder

	header := styleHeader.Copy().Width(m.width).Render("  Ostrich — New VM from Template: " + m.tpl.Name)
	b.WriteString(header)
	b.WriteString("\n\n")

	b.WriteString(styleHelp.Render(fmt.Sprintf("  Step %d / %d", int(m.step)+1, int(tplStepCount))))
	b.WriteString(styleHelp.Render(fmt.Sprintf("   —   %d GiB disk, %s: from the template", m.tpl.DiskSize, m.tpl.FirmwareLabel())))
	b.WriteString("\n\n")

	b.WriteString(styleLabel.Render("  " + tplStepLabels[m.step]))
	b.WriteString("\n")
	b.WriteString(styleHelp.Render("  " + tplStepHelp[m.step]))
	b.WriteString("\n\n")

	switch m.step {
	case tplStepNetwork:
		b.WriteString("  " + renderChoices(networkLabels, m.netIdx) + "\n\n")
		if m.bridgeHint != "" {
			b.WriteString(renderBridgeHint(m.bridgeHint))
		}

	case tplStepConfirm:
		if cfg, err := m.buildConfig(); err == nil {
			net := string(cfg.Network.Type)
			if len(cfg.Network.PortForwards) > 0 {
				net += " [" + vm.FormatPortForwards(cfg.Network.PortForwards) + "]"
			}
			vncStr := "disabled"
			if cfg.VNCPort > 0 {
				vncStr = fmt.Sprintf("display %d (port %d) — the lowest one free", cfg.VNCPort, 5900+cfg.VNCPort)
			}
			summary := fmt.Sprintf(
				"  Name:     %s\n  Template: %s\n  CPU:      %d cores\n  RAM:      %d MiB\n  Disk:     %d GiB (copied from the template)\n  Firmware: %s\n  Net:      %s\n  VNC:      %s",
				cfg.Name, m.tpl.Name, cfg.CPU, cfg.RAM, cfg.DiskSize, cfg.FirmwareLabel(), net, vncStr,
			)
			b.WriteString(styleBox.Render(summary))
			b.WriteString("\n\n")
		}
		if m.bridgeHint != "" {
			b.WriteString(renderBridgeHint(m.bridgeHint))
		}

	default:
		if idx := tplInputForStep(m.step); idx >= 0 {
			b.WriteString("  ")
			b.WriteString(m.inputs[idx].View())
			b.WriteString("\n\n")
		}
	}

	switch {
	case m.busy:
		b.WriteString("  " + m.spin.View() + " Copying the disk image… this takes a while for a large disk")
		b.WriteString("\n\n")
	case m.err != "":
		b.WriteString(styleError.Render(indent("✗ "+m.err, "  ")))
		b.WriteString("\n\n")
	}

	keyHelp := "Tab/j/↓: next   Shift+Tab/k/↑: back   Esc: cancel"
	switch {
	case m.busy:
		keyHelp = "Please wait…"
	case m.step == tplStepNetwork:
		keyHelp = "h/l/←/→: select   j/↓: next   k/↑: back   Esc: cancel"
	case m.step == tplStepConfirm:
		keyHelp = "Enter/j: create VM   k/Shift+Tab: back   Esc: cancel"
	}
	b.WriteString(styleHelp.Render("  " + keyHelp))

	return lipgloss.NewStyle().Width(m.width).Render(b.String())
}
