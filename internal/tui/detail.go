package tui

import (
	"fmt"
	"os/exec"
	"path/filepath"
	"strings"
	"time"

	"github.com/charmbracelet/bubbles/viewport"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
	"github.com/pzindyaev/ostrich/internal/vm"
)

const (
	consoleTailLines   = 200
	consolePollSeconds = 2
	consoleViewHeight  = 20
	// detailChromeLines is everything on the screen besides the console
	// viewport: header, info card (one USB line, one USB ISO line), labels,
	// notice and help.
	detailChromeLines = 23
)

// --- messages ---

type consolePollMsg struct{}
type consoleRefreshedMsg struct {
	lines   []string // already passed through sanitizeConsoleLine
	status  vm.ProcessInfo
	guestIP string
	usb     []vm.USBState
	cdrom   vm.ImageState // the boot ISO; meaningless when none is configured
	images  []vm.ImageState
}
type detailActionErrMsg struct{ err error }
type detailActionOKMsg struct{ action string }

// --- VMDetailModel ---

// VMDetailModel shows a VM's config, running state and serial console tail.
type VMDetailModel struct {
	cfg          *vm.VMConfig
	storagePath  string
	status       vm.ProcessInfo
	guestIP      string
	usb          []vm.USBState
	cdrom        vm.ImageState
	images       []vm.ImageState
	vp           viewport.Model
	consoleLines []string
	err          string
	notice       string
	width        int
	height       int
}

// NewVMDetailModel constructs a VMDetailModel.
func NewVMDetailModel(cfg *vm.VMConfig, storagePath string, width, height int) VMDetailModel {
	vpHeight := consoleViewHeight
	if height > 0 {
		vpHeight = consoleHeight(height, cfg)
	}
	vp := viewport.New(width-4, vpHeight)
	vp.SetContent("(no console output yet)")

	return VMDetailModel{
		cfg:         cfg,
		storagePath: storagePath,
		vp:          vp,
		width:       width,
		height:      height,
	}
}

func (m *VMDetailModel) setSize(w, h int) {
	follow := m.vp.AtBottom()
	m.width = w
	m.height = h
	m.vp.Width = w - 4
	m.vp.Height = consoleHeight(h, m.cfg)
	m.reloadConsole(follow)
}

// reloadConsole puts the console lines into the viewport, hard-wrapped to its
// width so no line can spill past the box. With follow it jumps to the tail.
func (m *VMDetailModel) reloadConsole(follow bool) {
	content := "(no console output yet)"
	if len(m.consoleLines) > 0 {
		wrapped := make([]string, 0, len(m.consoleLines))
		for _, l := range m.consoleLines {
			wrapped = append(wrapped, wrapConsoleLine(l, m.vp.Width)...)
		}
		content = strings.Join(wrapped, "\n")
	}
	m.vp.SetContent(content)
	if follow {
		m.vp.GotoBottom()
	}
}

// consoleHeight is the viewport height that fits the terminal: every USB device
// or image beyond the first adds a line to the info card.
func consoleHeight(termHeight int, cfg *vm.VMConfig) int {
	h := termHeight - detailChromeLines - max(0, len(cfg.USBDevices)-1) - max(0, len(cfg.USBImages)-1)
	if h < 5 {
		h = 5
	}
	return h
}

func (m VMDetailModel) Init() tea.Cmd {
	return tea.Batch(refreshConsoleCmd(m.storagePath, m.cfg), pollTickCmd())
}

func (m VMDetailModel) Update(msg tea.Msg) (VMDetailModel, tea.Cmd) {
	var (
		vpCmd  tea.Cmd
		ourCmd tea.Cmd
	)

	switch msg := msg.(type) {
	case consolePollMsg:
		ourCmd = refreshConsoleCmd(m.storagePath, m.cfg)

	case consoleRefreshedMsg:
		m.status = msg.status
		m.guestIP = msg.guestIP
		m.usb = msg.usb
		m.cdrom = msg.cdrom
		m.images = msg.images
		m.consoleLines = msg.lines
		// Keep following the newest output unless the user has scrolled up.
		m.reloadConsole(m.vp.AtBottom())
		ourCmd = pollTickCmd()

	case detailActionErrMsg:
		m.err = msg.err.Error()
		m.notice = ""
		ourCmd = refreshConsoleCmd(m.storagePath, m.cfg)

	case detailActionOKMsg:
		m.notice = "✓ " + msg.action
		m.err = ""
		ourCmd = refreshConsoleCmd(m.storagePath, m.cfg)

	case tea.KeyMsg:
		ourCmd = m.handleKey(msg)
	}

	m.vp, vpCmd = m.vp.Update(msg)
	return m, tea.Batch(vpCmd, ourCmd)
}

func (m VMDetailModel) handleKey(msg tea.KeyMsg) tea.Cmd {
	switch msg.String() {
	case "esc", "b", "h", "q":
		return func() tea.Msg { return NavigateMsg{To: screenList} }

	case "ctrl+c":
		return tea.Quit

	case "s":
		if m.status.Status != vm.StatusRunning {
			return func() tea.Msg {
				if err := vm.Start(m.storagePath, m.cfg); err != nil {
					return detailActionErrMsg{err}
				}
				return detailActionOKMsg{"started " + m.cfg.Name}
			}
		}

	case "x":
		if m.status.Status == vm.StatusRunning {
			name := m.cfg.Name
			return func() tea.Msg {
				if err := vm.Stop(m.storagePath, name); err != nil {
					return detailActionErrMsg{err}
				}
				return detailActionOKMsg{"stopped " + name}
			}
		}

	case "c":
		// Connect interactively to the serial console via socat.
		// socat puts the local TTY in raw mode; Ctrl-] exits.
		if m.status.Status != vm.StatusRunning {
			return func() tea.Msg {
				return detailActionErrMsg{fmt.Errorf("VM is not running")}
			}
		}
		sockPath := vm.SerialSockPath(m.storagePath, m.cfg.Name)
		socat, err := exec.LookPath("socat")
		if err != nil {
			return func() tea.Msg {
				return detailActionErrMsg{fmt.Errorf(
					"socat not found — install it to connect interactively\n" +
						"  sudo apt install socat   # Debian/Ubuntu\n" +
						"  sudo dnf install socat   # Fedora\n" +
						"  brew install socat       # macOS",
				)}
			}
		}
		cmd := exec.Command(socat, "-,escape=0x1d", "UNIX-CONNECT:"+sockPath)
		return tea.ExecProcess(cmd, func(err error) tea.Msg {
			// socat exits with status 1 on normal Ctrl-] disconnect; treat as success.
			return detailActionOKMsg{"disconnected from serial console"}
		})

	case "v":
		// Launch a VNC viewer in the background (GUI app).
		if m.cfg.VNCPort <= 0 {
			return func() tea.Msg {
				return detailActionErrMsg{fmt.Errorf("VNC is not enabled for this VM (vnc_port: 0)")}
			}
		}
		if m.status.Status != vm.StatusRunning {
			return func() tea.Msg {
				return detailActionErrMsg{fmt.Errorf("VM is not running")}
			}
		}
		port := m.cfg.VNCPort
		return func() tea.Msg {
			v, path, err := findVNCViewer()
			if err != nil {
				return detailActionErrMsg{err}
			}
			args := v.argFunc("127.0.0.1", 5900+port)
			cmd := exec.Command(path, args...)
			if err := cmd.Start(); err != nil {
				return detailActionErrMsg{fmt.Errorf("launch VNC viewer: %w", err)}
			}
			_ = cmd.Process.Release()
			return detailActionOKMsg{fmt.Sprintf("launched %s → port %d", filepath.Base(path), 5900+port)}
		}

	case "e":
		name := m.cfg.Name
		return func() tea.Msg { return NavigateMsg{To: screenEdit, VMName: name} }

	case "u":
		name := m.cfg.Name
		return func() tea.Msg { return NavigateMsg{To: screenUSB, VMName: name} }

	case "i":
		name := m.cfg.Name
		return func() tea.Msg { return NavigateMsg{To: screenISO, VMName: name} }

	case "r":
		return refreshConsoleCmd(m.storagePath, m.cfg)

	case "G":
		m.vp.GotoBottom()

	case "g":
		m.vp.GotoTop()
	}
	return nil
}

func (m VMDetailModel) View() string {
	if m.width == 0 {
		return ""
	}

	var b strings.Builder

	header := styleHeader.Copy().Width(m.width).Render("  Ostrich — VM Detail")
	b.WriteString(header)
	b.WriteString("\n\n")

	// VM info card
	statusLine := styleStopped.Render("● stopped")
	if m.status.Status == vm.StatusRunning {
		statusLine = styleRunning.Render(fmt.Sprintf("● running  (PID %d)", m.status.PID))
	}

	netInfo := string(m.cfg.Network.Type)
	if len(m.cfg.Network.PortForwards) > 0 {
		var fwds []string
		for _, pf := range m.cfg.Network.PortForwards {
			fwds = append(fwds, fmt.Sprintf("%d→%d", pf.Host, pf.Guest))
		}
		netInfo += " [" + strings.Join(fwds, ", ") + "]"
	}

	ipInfo := "—"
	if m.status.Status == vm.StatusRunning {
		switch m.cfg.Network.Type {
		case vm.NetworkUser:
			// Fixed SLIRP lease, only reachable from the host through port forwards.
			ipInfo = fmt.Sprintf("%s (DHCP, guest-internal)  gw %s", m.guestIP, vm.UserNetGateway)
			for _, pf := range m.cfg.Network.PortForwards {
				if pf.Guest == 22 && (pf.Proto == "" || pf.Proto == "tcp") {
					ipInfo += fmt.Sprintf("  —  ssh -p %d localhost", pf.Host)
					break
				}
			}
		case vm.NetworkTap:
			ipInfo = ifEmpty(m.guestIP, fmt.Sprintf("(not seen yet — looking for %s)", m.cfg.Network.MAC))
		}
	}

	vncInfo := "disabled"
	if m.cfg.VNCPort > 0 {
		vncInfo = fmt.Sprintf("127.0.0.1:%d  (TCP port %d)", m.cfg.VNCPort, 5900+m.cfg.VNCPort)
	}

	info := fmt.Sprintf(
		"  Name:     %s\n  Status:   %s\n  CPU:      %d cores\n  RAM:      %d MiB\n  Disk:     %d GiB\n  ISO:      %s\n  Firmware: %s\n  Net:      %s\n  IP:       %s\n  VNC:      %s\n  USB:      %s\n  USB ISO:  %s",
		m.cfg.Name, statusLine, m.cfg.CPU, m.cfg.RAM, m.cfg.DiskSize,
		m.cdromInfo(), m.cfg.FirmwareLabel(), netInfo, ipInfo, vncInfo, m.usbInfo(), m.imageInfo(),
	)
	b.WriteString(styleBox.Copy().Width(m.width - 2).Render(info))
	b.WriteString("\n\n")

	// Console section
	b.WriteString(styleLabel.Render("  Serial Console"))
	b.WriteString(styleHelp.Render(fmt.Sprintf(
		"  (last %d lines, auto-refresh every %ds — press c to connect interactively)",
		consoleTailLines, consolePollSeconds,
	)))
	b.WriteString("\n")

	consoleBox := styleBox.Copy().Width(m.width - 2)
	b.WriteString(consoleBox.Render(m.vp.View()))
	b.WriteString("\n")

	if m.err != "" {
		b.WriteString(styleError.Render("  ✗ " + m.err))
		b.WriteString("\n")
	} else if m.notice != "" {
		b.WriteString(styleSuccess.Render("  " + m.notice))
		b.WriteString("\n")
	}
	b.WriteString("\n")

	helpItems := []string{
		"s: start", "x: stop",
		"e: edit", "u: USB", "i: ISO hot-plug", "c: serial console", "v: VNC viewer",
		"j/k: scroll", "g/G: top/bottom", "r: refresh",
		"q/h/Esc: back",
	}
	b.WriteString(styleHelp.Render("  " + strings.Join(helpItems, "   ")))

	return lipgloss.NewStyle().Width(m.width).Render(b.String())
}

// --- helpers ---

// usbInfo renders the passed-through devices, one per line, with whether each
// is connected to the host right now and openable by QEMU.
func (m VMDetailModel) usbInfo() string {
	if len(m.cfg.USBDevices) == 0 {
		return "(none)"
	}
	states := m.usb
	if len(states) != len(m.cfg.USBDevices) {
		states = vm.MatchUSB(m.cfg.USBDevices, nil) // not refreshed yet: show as not connected
	}
	var lines []string
	for _, s := range states {
		state := styleHelp.Render("○ not connected")
		switch {
		case s.Host != nil && !s.Host.Writable:
			state = styleError.Render("✗ no access")
		case s.Host != nil:
			state = styleSuccess.Render("● connected")
		}
		lines = append(lines, fmt.Sprintf("%-*s  %s  %s", usbNameWidth, truncate(s.Device.Label(), usbNameWidth), s.Device.ID(), state))
	}
	return strings.Join(lines, "\n            ")
}

// cdromInfo renders the boot ISO with whether the file is there on the host.
func (m VMDetailModel) cdromInfo() string {
	if m.cfg.CDROMPath == "" {
		return "(none)"
	}
	s := m.cdrom
	if s.Path != m.cfg.CDROMPath {
		s = vm.ImageStateOf(m.cfg.CDROMPath) // not refreshed yet
	}
	return m.cfg.CDROMPath + "  " + imageState(s)
}

// imageInfo renders the images attached as USB drives, one per line, with
// whether each file is still there on the host.
func (m VMDetailModel) imageInfo() string {
	if len(m.cfg.USBImages) == 0 {
		return "(none)"
	}
	states := m.images
	if len(states) != len(m.cfg.USBImages) {
		states = vm.USBImageStates(m.cfg.USBImages) // not refreshed yet
	}
	var lines []string
	for _, s := range states {
		lines = append(lines, fmt.Sprintf("%-*s  %s", isoPathWidth, truncateLeft(s.Path, isoPathWidth), imageState(s)))
	}
	return strings.Join(lines, "\n            ")
}

// vncViewer describes a known VNC viewer and how to build its arguments.
type vncViewer struct {
	name    string
	argFunc func(host string, port int) []string
}

// knownVNCViewers lists supported viewers in preference order.
// Each viewer has a different CLI convention for specifying host+port.
var knownVNCViewers = []vncViewer{
	// TigerVNC / TightVNC: host::port (double-colon = explicit TCP port)
	{"vncviewer", func(h string, p int) []string { return []string{fmt.Sprintf("%s::%d", h, p)} }},
	{"tigervnc", func(h string, p int) []string { return []string{fmt.Sprintf("%s::%d", h, p)} }},
	{"xtightvncviewer", func(h string, p int) []string { return []string{fmt.Sprintf("%s::%d", h, p)} }},
	// Remmina: -c vnc://host:port
	{"remmina", func(h string, p int) []string { return []string{"-c", fmt.Sprintf("vnc://%s:%d", h, p)} }},
	// KRDC / Vinagre: vnc://host:port URI
	{"krdc", func(h string, p int) []string { return []string{fmt.Sprintf("vnc://%s:%d", h, p)} }},
	{"vinagre", func(h string, p int) []string { return []string{fmt.Sprintf("vnc://%s:%d", h, p)} }},
}

// findVNCViewer returns the first available viewer and its path.
func findVNCViewer() (vncViewer, string, error) {
	for _, v := range knownVNCViewers {
		if p, err := exec.LookPath(v.name); err == nil {
			return v, p, nil
		}
	}
	return vncViewer{}, "", fmt.Errorf(
		"no VNC viewer found — install one, e.g.:\n" +
			"  sudo apt install tigervnc-viewer   # Debian/Ubuntu\n" +
			"  sudo dnf install tigervnc          # Fedora\n" +
			"  brew install --cask tigervnc-viewer # macOS",
	)
}

// --- polling commands ---

func pollTickCmd() tea.Cmd {
	return tea.Tick(consolePollSeconds*time.Second, func(time.Time) tea.Msg {
		return consolePollMsg{}
	})
}

func refreshConsoleCmd(storagePath string, cfg *vm.VMConfig) tea.Cmd {
	return func() tea.Msg {
		lines, _ := vm.ReadConsoleTail(storagePath, cfg.Name, consoleTailLines)
		for i, l := range lines {
			lines[i] = sanitizeConsoleLine(l)
		}
		info, _ := vm.Status(storagePath, cfg.Name)
		var ip string
		if info.Status == vm.StatusRunning {
			ip = vm.GuestIP(cfg)
		}
		return consoleRefreshedMsg{
			lines: lines, status: info, guestIP: ip,
			usb:    vm.USBStates(cfg.USBDevices),
			cdrom:  vm.ImageStateOf(cfg.CDROMPath),
			images: vm.USBImageStates(cfg.USBImages),
		}
	}
}
