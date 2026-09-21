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
)

// --- messages ---

type consolePollMsg struct{}
type consoleRefreshedMsg struct {
	lines  []string
	status vm.ProcessInfo
}
type detailActionErrMsg struct{ err error }
type detailActionOKMsg struct{ action string }

// --- VMDetailModel ---

// VMDetailModel shows a VM's config, running state and serial console tail.
type VMDetailModel struct {
	cfg          *vm.VMConfig
	storagePath  string
	status       vm.ProcessInfo
	vp           viewport.Model
	consoleLines []string
	err          string
	notice       string
	width        int
	height       int
	ready        bool
}

// NewVMDetailModel constructs a VMDetailModel.
func NewVMDetailModel(cfg *vm.VMConfig, storagePath string, width, height int) VMDetailModel {
	vpHeight := consoleViewHeight
	if height > 0 {
		vpHeight = height - 19
		if vpHeight < 5 {
			vpHeight = 5
		}
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
	m.width = w
	m.height = h
	vpHeight := h - 19
	if vpHeight < 5 {
		vpHeight = 5
	}
	m.vp.Width = w - 4
	m.vp.Height = vpHeight
}

func (m VMDetailModel) Init() tea.Cmd {
	return tea.Batch(refreshConsoleCmd(m.storagePath, m.cfg.Name), pollTickCmd())
}

func (m VMDetailModel) Update(msg tea.Msg) (VMDetailModel, tea.Cmd) {
	var (
		vpCmd  tea.Cmd
		ourCmd tea.Cmd
	)

	switch msg := msg.(type) {
	case consolePollMsg:
		ourCmd = refreshConsoleCmd(m.storagePath, m.cfg.Name)

	case consoleRefreshedMsg:
		m.status = msg.status
		m.consoleLines = msg.lines
		content := "(no console output yet)"
		if len(msg.lines) > 0 {
			content = strings.Join(msg.lines, "\n")
		}
		m.vp.SetContent(content)
		if !m.ready {
			m.vp.GotoBottom()
			m.ready = true
		}
		ourCmd = pollTickCmd()

	case detailActionErrMsg:
		m.err = msg.err.Error()
		m.notice = ""
		ourCmd = refreshConsoleCmd(m.storagePath, m.cfg.Name)

	case detailActionOKMsg:
		m.notice = "✓ " + msg.action
		m.err = ""
		ourCmd = refreshConsoleCmd(m.storagePath, m.cfg.Name)

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

	case "r":
		return refreshConsoleCmd(m.storagePath, m.cfg.Name)

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

	vncInfo := "disabled"
	if m.cfg.VNCPort > 0 {
		vncInfo = fmt.Sprintf("127.0.0.1:%d  (TCP port %d)", m.cfg.VNCPort, 5900+m.cfg.VNCPort)
	}

	info := fmt.Sprintf(
		"  Name:   %s\n  Status: %s\n  CPU:    %d cores\n  RAM:    %d MiB\n  Disk:   %d GiB\n  ISO:    %s\n  Net:    %s\n  VNC:    %s",
		m.cfg.Name, statusLine, m.cfg.CPU, m.cfg.RAM, m.cfg.DiskSize,
		ifEmpty(m.cfg.CDROMPath, "(none)"), netInfo, vncInfo,
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
		"e: edit", "c: serial console", "v: VNC viewer",
		"j/k: scroll", "g/G: top/bottom", "r: refresh",
		"q/h/Esc: back",
	}
	b.WriteString(styleHelp.Render("  " + strings.Join(helpItems, "   ")))

	return lipgloss.NewStyle().Width(m.width).Render(b.String())
}

// --- helpers ---

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

func refreshConsoleCmd(storagePath, name string) tea.Cmd {
	return func() tea.Msg {
		lines, _ := vm.ReadConsoleTail(storagePath, name, consoleTailLines)
		info, _ := vm.Status(storagePath, name)
		return consoleRefreshedMsg{lines: lines, status: info}
	}
}
