package tui

import (
	"errors"
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
	editDisks
	editISO
	editFirmware // selector
	editTPM      // selector
	editNetwork  // selector (no text input)
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
	"Extra Disks",
	"Boot ISO",
	"Firmware",
	"TPM 2.0",
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
	"Comma-separated [name:]size in GiB, e.g. data:50, scratch:10. A new disk is hot-plugged into a running VM and arrives blank: partition and format it in the guest. Grow or remove only when stopped; removing deletes the image",
	"Full path to an ISO image to boot from, or leave blank to boot from disk; a running VM gets the new disc right away",
	"h/l/←/→ to select. VM must be stopped; turning Secure Boot on rebuilds the UEFI NVRAM (boot entries)",
	"h/l/←/→ to select. Emulated TPM 2.0 via swtpm — required by Windows 11",
	"h/l/←/→ to select: user (NAT) · tap (bridge) · none",
	"Leave blank to generate a new random address",
	"user networking only. Comma-separated [tcp|udp:]host:guest, e.g. 2222:22, udp:5353:53",
	"Display number 1–99 (TCP port = 5900+n). 0 to disable",
	"Press Enter to save changes",
}

// selector reports whether the field is a horizontal choice.
func (f editField) selector() bool {
	return f == editFirmware || f == editTPM || f == editNetwork
}

// isText reports whether the field is backed by a text input.
func (f editField) isText() bool {
	return !f.selector() && f != editSave
}

type vmUpdatedMsg struct{ name string }
type vmUpdateErrMsg struct{ err error }

// EditVMModel is a single-page form for editing an existing VM's properties.
type EditVMModel struct {
	orig    *vm.VMConfig
	field   editField
	inputs  [editFieldCount]textinput.Model // entries for non-text fields are unused
	fwIdx   int
	tpmIdx  int
	netIdx  int
	running bool
	// Removing a disk deletes its image, so the first save with removals only
	// arms the warning; a second save with the field unchanged confirms it.
	armed      bool
	armedValue string
	warn       string
	err        string
	mgr        *vm.Manager
	width      int
	height     int
}

// NewEditVMModel builds an EditVMModel pre-filled from cfg.
func NewEditVMModel(mgr *vm.Manager, cfg *vm.VMConfig, width, height int) EditVMModel {
	values := [editFieldCount]string{
		editName:     cfg.Name,
		editCPU:      strconv.Itoa(cfg.CPU),
		editRAM:      strconv.Itoa(cfg.RAM),
		editDisk:     strconv.Itoa(cfg.DiskSize),
		editDisks:    vm.FormatDisks(cfg.Disks),
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
		fwIdx:   firmwareIndex(cfg),
		tpmIdx:  boolIndex(cfg.TPM),
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
		m.cycle(-1)

	case "l", "right":
		m.cycle(1)
	}

	// Forward keystrokes to active text input.
	if m.field.isText() {
		var cmd tea.Cmd
		m.inputs[m.field], cmd = m.inputs[m.field].Update(msg)
		if m.armed && m.value(editDisks) != m.armedValue {
			// The removal being confirmed is not the one on screen any more.
			m.armed, m.armedValue, m.warn = false, "", ""
		}
		return m, cmd
	}
	return m, nil
}

// cycle moves the current field's selector by delta, if it has one.
func (m *EditVMModel) cycle(delta int) {
	switch m.field {
	case editFirmware:
		m.fwIdx = wrap(m.fwIdx+delta, len(firmwareChoices))
	case editTPM:
		m.tpmIdx = wrap(m.tpmIdx+delta, len(tpmLabels))
	case editNetwork:
		m.netIdx = wrap(m.netIdx+delta, len(networkChoices))
	}
}

// choices returns a selector field's labels and current index.
func (m EditVMModel) choices(f editField) ([]string, int) {
	switch f {
	case editFirmware:
		return firmwareLabels(), m.fwIdx
	case editTPM:
		return tpmLabels, m.tpmIdx
	default:
		return networkLabels, m.netIdx
	}
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

	if cfg.Disks, err = vm.ParseDisks(m.value(editDisks)); err != nil {
		return nil, editDisks, err
	}
	disks, err := vm.DiffDisks(m.orig.Disks, cfg.Disks)
	if err != nil {
		return nil, editDisks, err
	}
	if m.running && (len(disks.Grown) > 0 || len(disks.Removed) > 0) {
		return nil, editDisks, fmt.Errorf("stop the VM before resizing or removing disks")
	}

	cfg.CDROMPath = m.value(editISO)
	if cfg.CDROMPath != "" {
		if st, err := os.Stat(cfg.CDROMPath); err != nil || st.IsDir() {
			return nil, editISO, fmt.Errorf("ISO file not found: %s", cfg.CDROMPath)
		}
	}

	fw := firmwareChoices[m.fwIdx]
	cfg.Firmware, cfg.SecureBoot = fw.firmware, fw.secureBoot
	if m.running && (cfg.UEFI() != m.orig.UEFI() || cfg.SecureBoot != m.orig.SecureBoot) {
		return nil, editFirmware, fmt.Errorf("stop the VM before changing its firmware")
	}
	cfg.TPM = m.tpmIdx == 1

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

	// Removing a disk deletes its image for good, so it takes a second save
	// with the same field value to confirm. buildConfig validated the diff.
	disks, _ := vm.DiffDisks(m.orig.Disks, cfg.Disks)
	if len(disks.Removed) > 0 && !(m.armed && m.value(editDisks) == m.armedValue) {
		m.armed, m.armedValue = true, m.value(editDisks)
		m.warn = fmt.Sprintf("Removing %s deletes the image files and everything on them — press Ctrl-s again to confirm",
			describeDisks(disks.Removed))
		return m.moveTo(editDisks)
	}
	m.armed, m.armedValue, m.warn = false, "", ""

	oldName := m.orig.Name
	swapISO := m.running && cfg.CDROMPath != m.orig.CDROMPath
	// A disk added to a running VM is hot-plugged once its image exists.
	var hotplug []int
	if m.running {
		added := map[string]bool{}
		for _, d := range disks.Added {
			added[d.Name] = true
		}
		for i, d := range cfg.Disks {
			if added[d.Name] {
				hotplug = append(hotplug, i)
			}
		}
	}
	storagePath := m.mgr.StoragePath
	return m, func() tea.Msg {
		if err := m.mgr.Update(oldName, cfg); err != nil {
			return vmUpdateErrMsg{err}
		}
		// What can change under a running VM goes through the monitor on the
		// spot: the CD-ROM drive takes the new disc (or none), new disks are
		// plugged in. The config is saved either way.
		var errs []error
		if swapISO {
			if err := vm.CDROMChange(storagePath, cfg.Name, cfg.CDROMPath); err != nil {
				errs = append(errs, fmt.Errorf("the CD-ROM drive could not be changed:\n%w", err))
			}
		}
		for _, i := range hotplug {
			if err := vm.DiskHotplug(storagePath, cfg, i); err != nil {
				errs = append(errs, fmt.Errorf("disk %q could not be hot-plugged:\n%w", cfg.Disks[i].Name, err))
			}
		}
		if len(errs) > 0 {
			return vmUpdateErrMsg{fmt.Errorf(
				"saved, but this could not be applied to the running VM (takes effect on next start):\n%w", errors.Join(errs...))}
		}
		return vmUpdatedMsg{name: cfg.Name}
	}
}

// describeDisks lists disks as "data (50 GiB), scratch (10 GiB)".
func describeDisks(disks []vm.Disk) string {
	parts := make([]string, len(disks))
	for i, d := range disks {
		parts[i] = fmt.Sprintf("%s (%d GiB)", d.Name, d.Size)
	}
	return strings.Join(parts, ", ")
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
		b.WriteString(styleHelp.Render(" — changes take effect on next start; name, disk sizes, disk removal and firmware are locked; new disks are hot-plugged"))
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

		if f.selector() {
			b.WriteString(renderChoices(m.choices(f)))
		} else {
			b.WriteString(m.inputs[f].View())
		}
		b.WriteString("\n")
	}

	b.WriteString("\n")
	b.WriteString(styleHelp.Render("  " + editHelp[m.field]))
	b.WriteString("\n\n")

	if m.warn != "" {
		b.WriteString(styleWarning.Render("  ⚠ " + m.warn))
		b.WriteString("\n\n")
	}
	if m.err != "" {
		b.WriteString(styleError.Render("  ✗ " + m.err))
		b.WriteString("\n\n")
	}

	keyHelp := "Tab/↓: next   Shift+Tab/↑: back   Ctrl-s: save   Esc: cancel"
	if m.field.selector() {
		keyHelp = "h/l/←/→: select   j/↓: next   k/↑: back   Ctrl-s: save   Esc: cancel"
	} else if m.field == editSave {
		keyHelp = "Enter: save   k/Shift+Tab: back   Esc: cancel"
	}
	b.WriteString(styleHelp.Render("  " + keyHelp))

	return lipgloss.NewStyle().Width(m.width).Render(b.String())
}
