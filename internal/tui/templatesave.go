package tui

import (
	"fmt"
	"os"
	"strings"

	"github.com/charmbracelet/bubbles/spinner"
	"github.com/charmbracelet/bubbles/textinput"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
	"github.com/pzindyaev/ostrich/internal/vm"
)

type saveTplField int

const (
	saveTplName saveTplField = iota
	saveTplDescription
	saveTplButton // no text input
	saveTplFieldCount
)

var saveTplLabels = [saveTplFieldCount]string{"Template name", "Description", ""}

var saveTplHelp = [saveTplFieldCount]string{
	"Letters, digits, hyphens and underscores, e.g. debian-12-base",
	"Optional — a line about what is installed, shown in the templates list",
	"Press Enter to save the template",
}

// isText reports whether the field is backed by a text input.
func (f saveTplField) isText() bool { return f != saveTplButton }

type templateSavedMsg struct{ name string }
type templateSaveErrMsg struct{ err error }

// SaveTemplateModel is the form that freezes a stopped VM as a template.
type SaveTemplateModel struct {
	cfg       *vm.VMConfig
	mgr       *vm.Manager
	field     saveTplField
	inputs    [saveTplFieldCount]textinput.Model // the button's entry is unused
	running   bool
	diskUsage int64 // the source disk image's size on the host
	busy      bool  // the copy is in flight
	spin      spinner.Model
	err       string
	width     int
	height    int
}

// NewSaveTemplateModel builds the form for cfg, with the VM's name as the
// suggested template name.
func NewSaveTemplateModel(mgr *vm.Manager, cfg *vm.VMConfig, width, height int) SaveTemplateModel {
	var inputs [saveTplFieldCount]textinput.Model
	for i := range inputs {
		t := textinput.New()
		t.Prompt = ""
		t.CharLimit = 256
		t.Width = 60
		inputs[i] = t
	}
	inputs[saveTplName].SetValue(cfg.Name)
	inputs[saveTplName].Focus()
	inputs[saveTplDescription].Placeholder = "e.g. Debian 12 with docker and my dotfiles"

	info, _ := vm.Status(mgr.StoragePath, cfg.Name)
	var usage int64
	if st, err := os.Stat(vm.DiskPath(mgr.StoragePath, cfg.Name)); err == nil {
		usage = st.Size()
	}

	return SaveTemplateModel{
		cfg:       cfg,
		mgr:       mgr,
		inputs:    inputs,
		running:   info.Status == vm.StatusRunning,
		diskUsage: usage,
		spin:      spinner.New(spinner.WithSpinner(spinner.MiniDot), spinner.WithStyle(styleLabel)),
		width:     width,
		height:    height,
	}
}

func (m SaveTemplateModel) Init() tea.Cmd {
	return textinput.Blink
}

func (m SaveTemplateModel) Update(msg tea.Msg) (SaveTemplateModel, tea.Cmd) {
	switch msg := msg.(type) {
	case templateSavedMsg:
		return m, func() tea.Msg { return NavigateMsg{To: screenTemplates} }

	case templateSaveErrMsg:
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

	if m.field.isText() {
		var cmd tea.Cmd
		m.inputs[m.field], cmd = m.inputs[m.field].Update(msg)
		return m, cmd
	}
	return m, nil
}

func (m SaveTemplateModel) handleKey(msg tea.KeyMsg) (SaveTemplateModel, tea.Cmd) {
	switch msg.String() {
	case "esc":
		name := m.cfg.Name
		return m, func() tea.Msg { return NavigateMsg{To: screenDetail, VMName: name} }

	case "ctrl+c":
		return m, tea.Quit

	case "ctrl+s":
		return m.save()

	case "enter":
		if m.field == saveTplButton {
			return m.save()
		}
		return m.moveTo((m.field + 1) % saveTplFieldCount)

	case "tab", "down":
		return m.moveTo((m.field + 1) % saveTplFieldCount)

	case "shift+tab", "up":
		return m.moveTo((m.field - 1 + saveTplFieldCount) % saveTplFieldCount)

	case "j":
		if !m.field.isText() {
			return m.moveTo((m.field + 1) % saveTplFieldCount)
		}

	case "k":
		if !m.field.isText() {
			return m.moveTo((m.field - 1 + saveTplFieldCount) % saveTplFieldCount)
		}
	}

	if m.field.isText() {
		var cmd tea.Cmd
		m.inputs[m.field], cmd = m.inputs[m.field].Update(msg)
		return m, cmd
	}
	return m, nil
}

func (m SaveTemplateModel) moveTo(f saveTplField) (SaveTemplateModel, tea.Cmd) {
	if m.field.isText() {
		m.inputs[m.field].Blur()
	}
	m.field = f
	if f.isText() {
		return m, m.inputs[f].Focus()
	}
	return m, nil
}

func (m SaveTemplateModel) value(f saveTplField) string {
	return strings.TrimSpace(m.inputs[f].Value())
}

// validate checks the form and the VM's state, returning the field to put
// the cursor on when something is wrong.
func (m SaveTemplateModel) validate() (saveTplField, error) {
	name := m.value(saveTplName)
	if err := validateVMName(name); err != nil {
		return saveTplName, err
	}
	if m.mgr.TemplateExists(name) {
		return saveTplName, fmt.Errorf("a template named %q already exists", name)
	}
	if m.running {
		return m.field, fmt.Errorf("the VM is running — shut it down from inside the guest first, so the disk is in a consistent state")
	}
	return 0, nil
}

func (m SaveTemplateModel) save() (SaveTemplateModel, tea.Cmd) {
	field, err := m.validate()
	if err != nil {
		m, cmd := m.moveTo(field)
		m.err = err.Error()
		return m, cmd
	}
	m.err = ""
	m.busy = true
	mgr, vmName := m.mgr, m.cfg.Name
	name, desc := m.value(saveTplName), m.value(saveTplDescription)
	return m, tea.Batch(m.spin.Tick, func() tea.Msg {
		if err := mgr.CreateTemplate(vmName, name, desc); err != nil {
			return templateSaveErrMsg{err}
		}
		return templateSavedMsg{name: name}
	})
}

func (m SaveTemplateModel) View() string {
	if m.width == 0 {
		return ""
	}

	var b strings.Builder

	header := styleHeader.Copy().Width(m.width).Render("  Ostrich — Save as Template: " + m.cfg.Name)
	b.WriteString(header)
	b.WriteString("\n\n")

	if m.running {
		b.WriteString(styleRunning.Render("  ● running"))
		b.WriteString(styleError.Render(" — stop the VM first: shut it down from inside the guest, so the disk is in a consistent state"))
	} else {
		b.WriteString(styleStopped.Render("  ● stopped"))
		b.WriteString(styleHelp.Render(" — the disk is in a consistent state and can be copied"))
	}
	b.WriteString("\n\n")

	b.WriteString(styleLabel.Render("  What goes into the template"))
	b.WriteString("\n")
	disk := fmt.Sprintf("%d GiB virtual", m.cfg.DiskSize)
	if m.diskUsage > 0 {
		disk += fmt.Sprintf(", %s on the host — copied in full", humanSize(m.diskUsage))
	}
	firmware := m.cfg.FirmwareLabel()
	switch {
	case m.cfg.UEFI() && m.cfg.TPM:
		firmware += " — with the UEFI NVRAM (boot entries) and the TPM state"
	case m.cfg.UEFI():
		firmware += " — with the UEFI NVRAM (boot entries)"
	case m.cfg.TPM:
		firmware += " — with the TPM state"
	}
	info := fmt.Sprintf(
		"  Disk:     %s\n  Firmware: %s\n  Defaults: %d cores, %d MiB RAM, %s network — chosen anew for each VM made from it",
		disk, firmware, m.cfg.CPU, m.cfg.RAM, m.cfg.Network.Type,
	)
	b.WriteString(styleBox.Copy().Width(m.width - 2).Render(info))
	b.WriteString("\n")
	b.WriteString(styleHelp.Render("  Left out, as they belong to one VM: MAC address, port forwards, VNC display, boot ISO, USB devices and images, additional disks."))
	b.WriteString("\n\n")

	for f := saveTplField(0); f < saveTplFieldCount; f++ {
		focused := f == m.field

		if f == saveTplButton {
			b.WriteString("\n  ")
			if focused {
				b.WriteString(styleSelected.Render(" Save template "))
			} else {
				b.WriteString(styleNormal.Render(" Save template "))
			}
			b.WriteString("\n")
			continue
		}

		marker := "  "
		label := styleHelp.Render(fmt.Sprintf("%-16s", saveTplLabels[f]))
		if focused {
			marker = styleLabel.Render("▸ ")
			label = styleLabel.Render(fmt.Sprintf("%-16s", saveTplLabels[f]))
		}
		b.WriteString(marker + label + " " + m.inputs[f].View() + "\n")
	}

	b.WriteString("\n")
	b.WriteString(styleHelp.Render("  " + saveTplHelp[m.field]))
	b.WriteString("\n\n")

	switch {
	case m.busy:
		what := "Copying the disk image"
		if m.diskUsage > 0 {
			what += " (" + humanSize(m.diskUsage) + ")"
		}
		b.WriteString("  " + m.spin.View() + " " + what + "… this takes a while for a large disk")
		b.WriteString("\n\n")
	case m.err != "":
		b.WriteString(styleError.Render(indent("✗ "+m.err, "  ")))
		b.WriteString("\n\n")
	}

	keyHelp := "Tab/↓: next   Shift+Tab/↑: back   Ctrl-s: save   Esc: cancel"
	if m.field == saveTplButton {
		keyHelp = "Enter: save   k/Shift+Tab: back   Esc: cancel"
	}
	if m.busy {
		keyHelp = "Please wait…"
	}
	b.WriteString(styleHelp.Render("  " + keyHelp))

	return lipgloss.NewStyle().Width(m.width).Render(b.String())
}
