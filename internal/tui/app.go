package tui

import (
	tea "github.com/charmbracelet/bubbletea"
	"github.com/pzindyaev/ostrich/internal/config"
	"github.com/pzindyaev/ostrich/internal/vm"
)

type screen int

const (
	screenSetup  screen = iota
	screenList
	screenCreate
	screenDetail
	screenEdit
)

// NavigateMsg is returned by sub-models to request a screen transition.
type NavigateMsg struct {
	To     screen
	VMName string // used when navigating to screenDetail / screenEdit
}

// App is the root Bubbletea model. It owns all sub-models and delegates
// Update/View to whichever screen is currently active.
type App struct {
	screen screen
	width  int
	height int

	appCfg *config.AppConfig
	vmMgr  *vm.Manager

	setup  SetupModel
	list   VMListModel
	create CreateVMModel
	detail VMDetailModel
	edit   EditVMModel
}

// NewApp constructs the root model. It detects first-run by checking for config.
func NewApp() (App, error) {
	var m App

	cfg, err := config.Load()
	if err != nil {
		if err == config.ErrNotFound {
			m.screen = screenSetup
			m.setup = NewSetupModel(0, 0)
			return m, nil
		}
		return m, err
	}

	m.appCfg = cfg
	m.vmMgr = vm.NewManager(cfg.VMStoragePath)
	m.screen = screenList
	m.list = NewVMListModel(m.vmMgr, 0, 0)
	return m, nil
}

func (m App) Init() tea.Cmd {
	switch m.screen {
	case screenSetup:
		return m.setup.Init()
	case screenList:
		return m.list.Init()
	}
	return nil
}

func (m App) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	switch msg := msg.(type) {
	case tea.WindowSizeMsg:
		m.width, m.height = msg.Width, msg.Height
		return m.propagateSize()

	case NavigateMsg:
		return m.handleNavigate(msg)

	case tea.KeyMsg:
		if msg.String() == "ctrl+c" {
			return m, tea.Quit
		}
	}

	return m.delegateUpdate(msg)
}

func (m App) View() string {
	switch m.screen {
	case screenSetup:
		return m.setup.View()
	case screenList:
		return m.list.View()
	case screenCreate:
		return m.create.View()
	case screenDetail:
		return m.detail.View()
	case screenEdit:
		return m.edit.View()
	}
	return ""
}

// propagateSize pushes the current terminal dimensions into the active sub-model.
func (m App) propagateSize() (tea.Model, tea.Cmd) {
	var cmd tea.Cmd
	switch m.screen {
	case screenSetup:
		m.setup.width = m.width
		m.setup.height = m.height
	case screenList:
		m.list.setSize(m.width, m.height)
	case screenCreate:
		m.create.width = m.width
		m.create.height = m.height
	case screenDetail:
		m.detail.setSize(m.width, m.height)
	case screenEdit:
		m.edit.width = m.width
		m.edit.height = m.height
	}
	return m, cmd
}

// delegateUpdate forwards a message to the active sub-model.
func (m App) delegateUpdate(msg tea.Msg) (tea.Model, tea.Cmd) {
	var cmd tea.Cmd
	switch m.screen {
	case screenSetup:
		m.setup, cmd = m.setup.Update(msg)
	case screenList:
		m.list, cmd = m.list.Update(msg)
	case screenCreate:
		m.create, cmd = m.create.Update(msg)
	case screenDetail:
		m.detail, cmd = m.detail.Update(msg)
	case screenEdit:
		m.edit, cmd = m.edit.Update(msg)
	}
	return m, cmd
}

// handleNavigate transitions to a new screen, (re-)initialising its sub-model.
func (m App) handleNavigate(msg NavigateMsg) (tea.Model, tea.Cmd) {
	switch msg.To {
	case screenSetup:
		m.setup = NewSetupModel(m.width, m.height)
		m.screen = screenSetup
		return m, m.setup.Init()

	case screenList:
		// Re-read config in case setup just saved it.
		if m.appCfg == nil {
			cfg, err := config.Load()
			if err != nil {
				// Fallback: go back to setup.
				m.setup = NewSetupModel(m.width, m.height)
				m.screen = screenSetup
				return m, m.setup.Init()
			}
			m.appCfg = cfg
			m.vmMgr = vm.NewManager(cfg.VMStoragePath)
		}
		m.list = NewVMListModel(m.vmMgr, m.width, m.height)
		m.screen = screenList
		return m, m.list.Init()

	case screenCreate:
		m.create = NewCreateVMModel(m.vmMgr, m.width, m.height)
		m.screen = screenCreate
		return m, m.create.Init()

	case screenDetail:
		cfg, err := vm.LoadConfig(m.appCfg.VMStoragePath, msg.VMName)
		if err != nil {
			// Can't load VM — fall back to list.
			m.list = NewVMListModel(m.vmMgr, m.width, m.height)
			m.screen = screenList
			return m, m.list.Init()
		}
		m.detail = NewVMDetailModel(cfg, m.appCfg.VMStoragePath, m.width, m.height)
		m.screen = screenDetail
		return m, m.detail.Init()

	case screenEdit:
		cfg, err := vm.LoadConfig(m.appCfg.VMStoragePath, msg.VMName)
		if err != nil {
			m.list = NewVMListModel(m.vmMgr, m.width, m.height)
			m.screen = screenList
			return m, m.list.Init()
		}
		m.edit = NewEditVMModel(m.vmMgr, cfg, m.width, m.height)
		m.screen = screenEdit
		return m, m.edit.Init()
	}
	return m, nil
}
