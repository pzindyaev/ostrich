package tui

import "github.com/charmbracelet/lipgloss"

var (
	colorPrimary = lipgloss.Color("#7C3AED") // purple
	colorAccent  = lipgloss.Color("#10B981") // green
	colorDanger  = lipgloss.Color("#EF4444") // red
	colorMuted   = lipgloss.Color("#6B7280") // gray
	colorText    = lipgloss.Color("#F3F4F6") // near-white

	styleTitle = lipgloss.NewStyle().
			Bold(true).
			Foreground(colorPrimary)

	styleSubtitle = lipgloss.NewStyle().
			Foreground(colorMuted).
			Italic(true)

	styleRunning = lipgloss.NewStyle().
			Foreground(colorAccent).
			Bold(true)

	styleStopped = lipgloss.NewStyle().
			Foreground(colorDanger)

	styleBox = lipgloss.NewStyle().
			Border(lipgloss.RoundedBorder()).
			BorderForeground(colorMuted).
			Padding(0, 1)

	styleError = lipgloss.NewStyle().
			Foreground(colorDanger)

	styleSuccess = lipgloss.NewStyle().
			Foreground(colorAccent)

	styleHelp = lipgloss.NewStyle().
			Foreground(colorMuted)

	styleLabel = lipgloss.NewStyle().
			Bold(true).
			Foreground(colorPrimary)

	styleSelected = lipgloss.NewStyle().
			Foreground(colorText).
			Background(colorPrimary).
			Padding(0, 1)

	styleNormal = lipgloss.NewStyle().
			Foreground(colorText).
			Padding(0, 1)

	styleHeader = lipgloss.NewStyle().
			Bold(true).
			Foreground(colorText).
			Background(colorPrimary).
			Padding(0, 2).
			Width(0) // overridden at render time
)
