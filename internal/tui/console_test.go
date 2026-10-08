package tui

import (
	"os"
	"reflect"
	"strings"
	"testing"

	"github.com/muesli/reflow/ansi"
	"github.com/pzindyaev/ostrich/internal/vm"
)

func TestSanitizeConsoleLine(t *testing.T) {
	tests := []struct{ name, in, want string }{
		{"plain", "hello", "hello"},
		{"sgr stripped", "\x1b[1;32mok\x1b[0m", "ok"},
		{"clear screen and home", "\x1b[2J\x1b[Hlogin:", "login:"},
		{"private mode", "\x1b[?25lx\x1b[?25h", "x"},
		{"osc ended by bel", "\x1b]0;title\x07x", "x"},
		{"osc ended by st", "\x1b]0;title\x1b\\x", "x"},
		{"osc cut by csi", "\x1b]0;title\x1b[0mx", "x"},
		{"charset designation", "\x1b(Bfoo", "foo"},
		{"keypad mode", "\x1b=foo", "foo"},
		{"unterminated csi", "foo\x1b[12", "foo"},
		{"trailing esc", "foo\x1b", "foo"},
		{"carriage return overwrites", "Loading 10%\rLoading 100%", "Loading 100%"},
		{"carriage return keeps tail", "abcdef\rXY", "XYcdef"},
		{"erase to end of line", "Loading 10%\rDone\x1b[K", "Done"},
		{"erase to end of line explicit", "Loading 10%\rDone\x1b[0K", "Done"},
		{"erase to start of line", "abcdef\r\x1b[1Kxy", "xycdef"},
		{"erase whole line", "abcdef\x1b[2K\rxy", "xy"},
		{"backspace", "abc\b\bX", "aXc"},
		{"backspace at start", "\b\bX", "X"},
		{"tab expands to stop", "a\tb", "a" + strings.Repeat(" ", 7) + "b"},
		{"tab at stop", "12345678\tb", "12345678" + strings.Repeat(" ", 8) + "b"},
		{"tab moves without erasing", "abcdefghij\rX\tY", "XbcdefghYj"},
		{"control chars dropped", "a\x07b\x00c\x7fd\x0ce", "abcde"},
		{"c1 dropped", "a\u009bb", "ab"},
		{"wide runes kept", "日本語", "日本語"},
		{"trailing spaces trimmed", "foo\t", "foo"},
		{"empty", "", ""},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got := sanitizeConsoleLine(tt.in)
			if got != tt.want {
				t.Errorf("sanitizeConsoleLine(%q) = %q, want %q", tt.in, got, tt.want)
			}
			// The whole point: the width lipgloss measures must be the width
			// the terminal shows, so nothing can spill out of the box.
			if ansi.PrintableRuneWidth(got) != len([]rune(got)) && !strings.ContainsAny(got, "日本語") {
				t.Errorf("sanitized %q still has zero-width content", got)
			}
			for _, r := range got {
				if r < 0x20 || r == 0x7f {
					t.Errorf("sanitized %q still contains control %U", got, r)
				}
			}
		})
	}
}

func TestWrapConsoleLine(t *testing.T) {
	tests := []struct {
		name  string
		in    string
		width int
		want  []string
	}{
		{"short", "abc", 5, []string{"abc"}},
		{"exact", "abcde", 5, []string{"abcde"}},
		{"split", "abcdefgh", 5, []string{"abcde", "fgh"}},
		{"multiple splits", "abcdefghijkl", 5, []string{"abcde", "fghij", "kl"}},
		{"wide rune does not straddle", "日本語", 5, []string{"日本", "語"}},
		{"keeps spaces", "ab cd ef", 3, []string{"ab ", "cd ", "ef"}},
		{"no width", "abcdefgh", 0, []string{"abcdefgh"}},
		{"empty", "", 5, []string{""}},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			if got := wrapConsoleLine(tt.in, tt.width); !reflect.DeepEqual(got, tt.want) {
				t.Errorf("wrapConsoleLine(%q, %d) = %q, want %q", tt.in, tt.width, got, tt.want)
			}
		})
	}
}

// TestDetailConsoleFollowsTail checks the viewport sticks to the newest output
// until the user scrolls up, re-wraps on resize, and never renders a line wider
// than the console box.
func TestDetailConsoleFollowsTail(t *testing.T) {
	cfg := &vm.VMConfig{Name: "x"}
	m := NewVMDetailModel(cfg, t.TempDir(), 60, 40)

	lines := make([]string, 0, 100)
	for i := 0; i < 100; i++ {
		lines = append(lines, strings.Repeat("x", i))
	}
	m, _ = m.Update(consoleRefreshedMsg{lines: lines})
	if !m.vp.AtBottom() {
		t.Fatalf("first refresh should show the tail, YOffset=%d", m.vp.YOffset)
	}

	m.vp.LineUp(5)
	off := m.vp.YOffset
	m, _ = m.Update(consoleRefreshedMsg{lines: append(lines, "new")})
	if m.vp.YOffset != off {
		t.Errorf("refresh moved a scrolled-up view: YOffset %d, want %d", m.vp.YOffset, off)
	}

	m.vp.GotoBottom()
	m, _ = m.Update(consoleRefreshedMsg{lines: append(lines, "new", "newer")})
	if !m.vp.AtBottom() {
		t.Errorf("refresh at the bottom should follow the tail, YOffset=%d", m.vp.YOffset)
	}

	m.setSize(30, 40)
	if !m.vp.AtBottom() {
		t.Errorf("resize at the bottom should keep the tail, YOffset=%d", m.vp.YOffset)
	}
	for i, l := range strings.Split(m.vp.View(), "\n") {
		if w := ansi.PrintableRuneWidth(l); w > m.vp.Width {
			t.Errorf("viewport line %d is %d cells wide, box allows %d", i, w, m.vp.Width)
		}
	}
	// Every view line must fit the terminal, or the renderer's line count drifts.
	for i, l := range strings.Split(m.View(), "\n") {
		if w := ansi.PrintableRuneWidth(l); w > m.width {
			t.Errorf("view line %d is %d cells wide, terminal is %d", i, w, m.width)
		}
	}
}

// TestDetailConsoleFromRawLog runs a raw serial log with the sequences a guest
// really emits (clear screen, colours, tabs, progress bars) through the whole
// poll pipeline and checks the rendered view stays within the terminal grid.
func TestDetailConsoleFromRawLog(t *testing.T) {
	storage := t.TempDir()
	cfg := &vm.VMConfig{Name: "raw"}
	if err := os.MkdirAll(vm.VMDir(storage, cfg.Name), 0o755); err != nil {
		t.Fatal(err)
	}
	log := strings.Join([]string{
		"\x1b[2J\x1b[H\x1b[?25lGNU GRUB  version 2.12",
		"\x1b]0;qemu\x07\x1b(B\x1b[0;1;32m[  OK  ]\x1b[0m Started \x1b[0;1;39mNetwork Service\x1b[0m.",
		"[    0.000000] Linux version 6.8.0\tx86_64\t#1 SMP",
		"Downloading 10%\rDownloading 55%\rDownloading 100%\x1b[K",
		strings.Repeat("a very long kernel command line that keeps going ", 6),
		"\r\n",
		"login: \x1b[?25h",
	}, "\r\n")
	if err := os.WriteFile(vm.ConsolePath(storage, cfg.Name), []byte(log), 0o644); err != nil {
		t.Fatal(err)
	}

	m := NewVMDetailModel(cfg, storage, 80, 40)
	m, _ = m.Update(refreshConsoleCmd(storage, cfg)())

	view := m.View()
	for i, l := range strings.Split(view, "\n") {
		if w := ansi.PrintableRuneWidth(l); w > m.width {
			t.Errorf("view line %d is %d cells wide, terminal is %d: %q", i, w, m.width, l)
		}
		for _, r := range l {
			if r == 0x1b {
				// Only the TUI's own styling may emit escapes, and those are
				// SGR sequences; anything else came from the guest.
				continue
			}
			if r < 0x20 || r == 0x7f {
				t.Errorf("view line %d carries control %U from the guest: %q", i, r, l)
			}
		}
	}
	for _, seq := range []string{"\x1b[2J", "\x1b[H", "\x1b[?25l", "\x1b]0;", "\x1b(B", "\x1b[K"} {
		if strings.Contains(view, seq) {
			t.Errorf("guest sequence %q leaked into the view", seq)
		}
	}
	for _, want := range []string{"Downloading 100%", "[  OK  ] Started Network Service.", "login:"} {
		if !strings.Contains(view, want) {
			t.Errorf("view lacks %q", want)
		}
	}
	if strings.Contains(view, "Downloading 10%") {
		t.Errorf("overwritten progress text survived in the view")
	}
}
