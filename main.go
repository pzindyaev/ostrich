package main

import (
	"fmt"
	"os"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/pzindyaev/ostrich/internal/tui"
)

// Set at build time via -ldflags "-X main.version=... -X main.commit=... -X main.date=...".
// GoReleaser fills these in for release builds; see .goreleaser.yaml.
var (
	version = "dev"
	commit  = "none"
	date    = "unknown"
)

func main() {
	if len(os.Args) > 1 {
		switch os.Args[1] {
		case "version", "--version", "-v":
			fmt.Printf("ostrich %s (commit %s, built %s)\n", version, commit, date)
			return
		case "help", "--help", "-h":
			fmt.Printf("Usage: ostrich [--version]\n\nA TUI for managing QEMU virtual machines. Run without arguments to open the interface.\n")
			return
		}
	}

	app, err := tui.NewApp()
	if err != nil {
		fmt.Fprintf(os.Stderr, "error initializing: %v\n", err)
		os.Exit(1)
	}

	p := tea.NewProgram(app, tea.WithAltScreen())
	if _, err := p.Run(); err != nil {
		fmt.Fprintf(os.Stderr, "error: %v\n", err)
		os.Exit(1)
	}
}
