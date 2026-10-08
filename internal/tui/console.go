package tui

import (
	"strings"

	"github.com/mattn/go-runewidth"
)

const consoleTabWidth = 8

// sanitizeConsoleLine turns one raw line of serial output into plain text whose
// width lipgloss can measure, so it cannot disturb the layout around it.
//
// The TUI measures text with an ANSI-aware width function that counts escape
// sequences and control characters as zero cells but still passes them to the
// terminal, where cursor moves, screen clears and tab stops shift everything
// after them. So escape sequences are dropped, \r and \b are applied the way a
// terminal would (overwriting earlier text), erase-in-line is honoured because
// progress output pairs it with \r, tabs are expanded to 8-column stops and the
// remaining control characters are removed.
func sanitizeConsoleLine(raw string) string {
	var (
		cells []rune // what the terminal would show
		col   int    // cursor position within cells
	)
	put := func(r rune) {
		for len(cells) < col {
			cells = append(cells, ' ')
		}
		if col < len(cells) {
			cells[col] = r
		} else {
			cells = append(cells, r)
		}
		col++
	}
	fill := func(from, to int) {
		for i := from; i < to && i < len(cells); i++ {
			cells[i] = ' '
		}
	}

	rs := []rune(raw)
	for i := 0; i < len(rs); i++ {
		r := rs[i]
		switch {
		case r == 0x1b:
			n := escapeLen(rs[i+1:])
			seq := rs[i+1 : i+1+n]
			if n >= 2 && seq[0] == '[' && seq[n-1] == 'K' { // EL: erase in line
				switch string(seq[1 : n-1]) {
				case "", "0": // cursor to end of line
					if col < len(cells) {
						cells = cells[:col]
					}
				case "1": // start of line to cursor
					fill(0, col+1)
				case "2": // whole line
					fill(0, len(cells))
				}
			}
			i += n
		case r == '\r':
			col = 0
		case r == '\b':
			if col > 0 {
				col--
			}
		case r == '\t':
			col = col/consoleTabWidth*consoleTabWidth + consoleTabWidth
		case r < 0x20 || r == 0x7f || (r >= 0x80 && r <= 0x9f):
			// other C0/C1 controls: nothing a log viewer can show
		default:
			put(r)
		}
	}
	return strings.TrimRight(string(cells), " ")
}

// escapeLen returns how many runes after an ESC belong to its escape sequence.
// An unterminated sequence runs to the end of the line.
func escapeLen(rest []rune) int {
	if len(rest) == 0 {
		return 0
	}
	switch rest[0] {
	case '[': // CSI: parameter and intermediate bytes, then one final byte
		i := 1
		for i < len(rest) && rest[i] >= 0x20 && rest[i] <= 0x3f {
			i++
		}
		if i < len(rest) && rest[i] >= 0x40 && rest[i] <= 0x7e {
			i++
		}
		return i
	case ']', 'P', 'X', '^', '_': // OSC, DCS, SOS, PM, APC: a string ended by BEL or ESC \
		for i := 1; i < len(rest); i++ {
			switch {
			case rest[i] == 0x07:
				return i + 1
			case rest[i] == 0x1b && i+1 < len(rest) && rest[i+1] == '\\':
				return i + 2
			case rest[i] == 0x1b: // a new sequence cuts this one short
				return i
			}
		}
		return len(rest)
	default: // ESC, intermediates, one final byte: charset designation, keypad mode, ...
		i := 0
		for i < len(rest) && rest[i] >= 0x20 && rest[i] <= 0x2f {
			i++
		}
		if i < len(rest) && rest[i] >= 0x30 && rest[i] <= 0x7e {
			i++
		}
		return i
	}
}

// wrapConsoleLine hard-wraps a sanitized line into pieces no wider than width
// cells, the way a terminal of that width shows it. A non-positive width
// leaves the line alone.
func wrapConsoleLine(line string, width int) []string {
	if width <= 0 || runewidth.StringWidth(line) <= width {
		return []string{line}
	}
	var (
		pieces []string
		cur    strings.Builder
		w      int
	)
	for _, r := range line {
		rw := runewidth.RuneWidth(r)
		if w+rw > width && w > 0 {
			pieces = append(pieces, cur.String())
			cur.Reset()
			w = 0
		}
		cur.WriteRune(r)
		w += rw
	}
	return append(pieces, cur.String())
}
