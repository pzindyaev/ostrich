//! The serial console pane: turning raw serial output into plain lines the
//! layout can measure, and a scrollable, auto-following view of them.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{
    Block, BorderType, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::theme::Theme;

/// Tab stops in the console view.
pub const CONSOLE_TAB_WIDTH: usize = 8;
/// How many lines of `console.log` the dashboard shows.
pub const CONSOLE_TAIL_LINES: usize = 200;
/// How often the console and details refresh, in seconds.
pub const CONSOLE_POLL_SECONDS: u64 = 2;

/// What the console pane shows while there is no output.
pub const NO_OUTPUT_TEXT: &str = "(no console output yet)";

/// Turns one raw line of serial output into plain text whose width can be
/// measured, so it cannot disturb the layout around it.
///
/// A width function that is aware of escape sequences would count them and
/// control characters as zero cells but still pass them to the terminal,
/// where cursor moves, screen clears and tab stops shift everything after
/// them. So escape sequences are dropped, `\r` and `\b` are applied the way
/// a terminal would (overwriting earlier text), erase-in-line is honoured
/// because progress output pairs it with `\r`, tabs are expanded to 8-column
/// stops and the remaining control characters are removed. Trailing spaces
/// are trimmed.
pub fn sanitize_console_line(raw: &str) -> String {
    // What the terminal would show, and the cursor position within it.
    // Every char takes one cell here, wide ones included: this is about
    // overwrites, not layout, and the wrapper measures real widths later.
    let mut cells: Vec<char> = Vec::with_capacity(raw.len());
    let mut col = 0usize;

    fn put(cells: &mut Vec<char>, col: &mut usize, c: char) {
        while cells.len() < *col {
            cells.push(' ');
        }
        if *col < cells.len() {
            cells[*col] = c;
        } else {
            cells.push(c);
        }
        *col += 1;
    }
    fn fill(cells: &mut [char], from: usize, to: usize) {
        for c in cells.iter_mut().take(to).skip(from) {
            *c = ' ';
        }
    }

    let rs: Vec<char> = raw.chars().collect();
    let mut i = 0;
    while i < rs.len() {
        let r = rs[i];
        match r {
            '\x1b' => {
                let n = escape_len(&rs[i + 1..]);
                let seq = &rs[i + 1..i + 1 + n];
                if n >= 2 && seq[0] == '[' && seq[n - 1] == 'K' {
                    // EL: erase in line.
                    let param: String = seq[1..n - 1].iter().collect();
                    match param.as_str() {
                        // Cursor to end of line.
                        "" | "0" => {
                            if col < cells.len() {
                                cells.truncate(col);
                            }
                        }
                        // Start of line to cursor.
                        "1" => fill(&mut cells, 0, col + 1),
                        // Whole line.
                        "2" => {
                            let n = cells.len();
                            fill(&mut cells, 0, n);
                        }
                        _ => {}
                    }
                }
                i += n;
            }
            '\r' => col = 0,
            '\x08' => col = col.saturating_sub(1),
            '\t' => col = col / CONSOLE_TAB_WIDTH * CONSOLE_TAB_WIDTH + CONSOLE_TAB_WIDTH,
            // Other C0/C1 controls: nothing a log viewer can show.
            c if (c as u32) < 0x20 || c == '\x7f' || (0x80..=0x9f).contains(&(c as u32)) => {}
            c => put(&mut cells, &mut col, c),
        }
        i += 1;
    }
    let s: String = cells.into_iter().collect();
    s.trim_end_matches(' ').to_string()
}

/// How many chars after an ESC belong to its escape sequence. An
/// unterminated sequence runs to the end of the line.
pub(crate) fn escape_len(rest: &[char]) -> usize {
    let Some(&first) = rest.first() else {
        return 0;
    };
    let between = |c: char, lo: u32, hi: u32| (lo..=hi).contains(&(c as u32));
    match first {
        // CSI: parameter and intermediate bytes, then one final byte.
        '[' => {
            let mut i = 1;
            while i < rest.len() && between(rest[i], 0x20, 0x3f) {
                i += 1;
            }
            if i < rest.len() && between(rest[i], 0x40, 0x7e) {
                i += 1;
            }
            i
        }
        // OSC, DCS, SOS, PM, APC: a string ended by BEL or ESC \.
        ']' | 'P' | 'X' | '^' | '_' => {
            for (i, &c) in rest.iter().enumerate().skip(1) {
                if c == '\x07' {
                    return i + 1;
                }
                if c == '\x1b' {
                    // ESC \ ends the string; any other ESC starts a new
                    // sequence and cuts this one short.
                    return if rest.get(i + 1) == Some(&'\\') {
                        i + 2
                    } else {
                        i
                    };
                }
            }
            rest.len()
        }
        // ESC, intermediates, one final byte: charset designation, keypad mode, ...
        _ => {
            let mut i = 0;
            while i < rest.len() && between(rest[i], 0x20, 0x2f) {
                i += 1;
            }
            if i < rest.len() && between(rest[i], 0x30, 0x7e) {
                i += 1;
            }
            i
        }
    }
}

/// Hard-wraps a sanitised line into pieces no wider than `width` cells, the
/// way a terminal of that width shows it. A zero width leaves the line alone.
pub fn wrap_console_line(line: &str, width: usize) -> Vec<String> {
    if width == 0 || line.width() <= width {
        return vec![line.to_string()];
    }
    let mut pieces = Vec::new();
    let mut cur = String::new();
    let mut w = 0;
    for c in line.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw > width && w > 0 {
            pieces.push(std::mem::take(&mut cur));
            w = 0;
        }
        cur.push(c);
        w += cw;
    }
    pieces.push(cur);
    pieces
}

/// A scrollable view of the console tail that follows the newest output
/// until the user scrolls up.
#[derive(Debug, Default)]
pub struct ConsoleView {
    /// Sanitised lines, newest last.
    lines: Vec<String>,
    /// The lines hard-wrapped to `wrap_width`.
    wrapped: Vec<String>,
    wrap_width: usize,
    /// First visible wrapped line.
    scroll: usize,
    /// Whether the view sticks to the bottom as new output arrives.
    follow: bool,
    /// Visible height at the last render, for page moves.
    viewport: usize,
}

impl ConsoleView {
    /// An empty view that follows the tail.
    pub fn new() -> Self {
        ConsoleView {
            follow: true,
            ..Default::default()
        }
    }

    /// The largest scroll offset: the one that shows the last wrapped line
    /// at the bottom of the pane.
    fn max_scroll(&self) -> usize {
        self.wrapped.len().saturating_sub(self.viewport)
    }

    /// Keeps the offset inside the content and records whether the view is
    /// at the bottom, which is what following means: a view whose content
    /// fits is always at the bottom and starts following as soon as the
    /// content overflows, and scrolling back down to the end re-attaches.
    fn settle(&mut self) {
        let max = self.max_scroll();
        if self.scroll > max {
            self.scroll = max;
        }
        self.follow = self.scroll >= max;
    }

    /// Lays the content out again after it or the pane changed. Whether to
    /// jump to the tail is decided by where the view was *before* the
    /// change, so new output never moves a view the user scrolled up.
    fn relayout(&mut self) {
        let follow = self.follow;
        self.wrapped = self
            .lines
            .iter()
            .flat_map(|l| wrap_console_line(l, self.wrap_width))
            .collect();
        if follow {
            self.scroll = self.max_scroll();
        }
        self.settle();
    }

    /// Replaces the lines; keeps following the tail unless the user has
    /// scrolled up. The same lines again (nothing new on the console since
    /// the last refresh) leave the layout as it is.
    pub fn set_lines(&mut self, lines: Vec<String>) {
        if lines == self.lines {
            return;
        }
        self.lines = lines;
        self.relayout();
    }

    /// The lines shown, before wrapping.
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// Whether the view is at the bottom (following).
    pub fn at_bottom(&self) -> bool {
        self.follow
    }

    pub fn scroll_up(&mut self, n: usize) {
        self.scroll = self.scroll.saturating_sub(n);
        self.settle();
    }

    pub fn scroll_down(&mut self, n: usize) {
        self.scroll = self.scroll.saturating_add(n);
        self.settle();
    }

    pub fn goto_top(&mut self) {
        self.scroll = 0;
        self.settle();
    }

    /// Jumps to the newest output and follows it again.
    pub fn goto_bottom(&mut self) {
        self.scroll = self.max_scroll();
        self.settle();
    }

    /// Applies the scroll keys (j/k/↑/↓, Ctrl-d/Ctrl-u half page, Ctrl-f/
    /// Ctrl-b and PageDown/PageUp full page, g/G, Home/End) and reports
    /// whether the key was one of them.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let page = self.viewport.max(1);
        let half = (self.viewport / 2).max(1);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') if !ctrl => self.scroll_up(1),
            KeyCode::Down | KeyCode::Char('j') if !ctrl => self.scroll_down(1),
            KeyCode::Char('u') if ctrl => self.scroll_up(half),
            KeyCode::Char('d') if ctrl => self.scroll_down(half),
            KeyCode::Char('b') if ctrl => self.scroll_up(page),
            KeyCode::Char('f') if ctrl => self.scroll_down(page),
            KeyCode::PageUp => self.scroll_up(page),
            KeyCode::PageDown => self.scroll_down(page),
            KeyCode::Char('g') | KeyCode::Home if !ctrl => self.goto_top(),
            KeyCode::Char('G') | KeyCode::End if !ctrl => self.goto_bottom(),
            _ => return false,
        }
        true
    }

    /// Draws the view inside a bordered block titled `title`, with a
    /// scrollbar when the content is taller than the pane, and
    /// `(no console output yet)` when there is nothing. Re-wraps when the
    /// width changed.
    pub fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        theme: &Theme,
        title: Line<'static>,
        focused: bool,
    ) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let border = if focused {
            theme.border_focused
        } else {
            theme.border
        };

        // Lay the content out for this pane first, so the following marker
        // in the title tells the truth about what is drawn under it.
        let text_area = area.inner(Margin::new(2, 1));
        let (w, h) = (text_area.width as usize, text_area.height as usize);
        if w > 0 && h > 0 && (w != self.wrap_width || h != self.viewport) {
            self.wrap_width = w;
            self.viewport = h;
            self.relayout();
        }

        let mut block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(border)
            .title(title);
        if !self.lines.is_empty() {
            let marker = if self.follow {
                "↓ following"
            } else {
                "↑ scrolled"
            };
            block =
                block.title_bottom(Line::styled(format!(" {marker} "), theme.help).right_aligned());
        }
        frame.render_widget(block, area);
        if w == 0 || h == 0 {
            return;
        }

        if self.lines.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::styled(NO_OUTPUT_TEXT, theme.help)),
                text_area,
            );
            return;
        }
        let end = (self.scroll + h).min(self.wrapped.len());
        let start = self.scroll.min(end);
        let body: Vec<Line> = self.wrapped[start..end]
            .iter()
            .map(|l| Line::styled(l.clone(), theme.normal))
            .collect();
        frame.render_widget(Paragraph::new(body), text_area);

        if self.wrapped.len() > h {
            // The bar sits on the right border, between the corners.
            let thumb = if focused {
                theme.border_focused
            } else {
                theme.help
            };
            let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some("│"))
                .track_style(border)
                .thumb_style(thumb);
            let mut state = ScrollbarState::new(self.max_scroll() + 1)
                .position(self.scroll)
                .viewport_content_length(h);
            frame.render_stateful_widget(bar, area.inner(Margin::new(0, 1)), &mut state);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::testutil::*;
    use ratatui::backend::TestBackend;

    #[test]
    fn sanitize_console_line_cases() {
        let seven = " ".repeat(7);
        let eight = " ".repeat(8);
        let cases: Vec<(&str, &str, String)> = vec![
            ("plain", "hello", "hello".into()),
            ("sgr stripped", "\x1b[1;32mok\x1b[0m", "ok".into()),
            (
                "clear screen and home",
                "\x1b[2J\x1b[Hlogin:",
                "login:".into(),
            ),
            ("private mode", "\x1b[?25lx\x1b[?25h", "x".into()),
            ("osc ended by bel", "\x1b]0;title\x07x", "x".into()),
            ("osc ended by st", "\x1b]0;title\x1b\\x", "x".into()),
            ("osc cut by csi", "\x1b]0;title\x1b[0mx", "x".into()),
            ("charset designation", "\x1b(Bfoo", "foo".into()),
            ("keypad mode", "\x1b=foo", "foo".into()),
            ("unterminated csi", "foo\x1b[12", "foo".into()),
            ("trailing esc", "foo\x1b", "foo".into()),
            (
                "carriage return overwrites",
                "Loading 10%\rLoading 100%",
                "Loading 100%".into(),
            ),
            ("carriage return keeps tail", "abcdef\rXY", "XYcdef".into()),
            (
                "erase to end of line",
                "Loading 10%\rDone\x1b[K",
                "Done".into(),
            ),
            (
                "erase to end of line explicit",
                "Loading 10%\rDone\x1b[0K",
                "Done".into(),
            ),
            (
                "erase to start of line",
                "abcdef\r\x1b[1Kxy",
                "xycdef".into(),
            ),
            ("erase whole line", "abcdef\x1b[2K\rxy", "xy".into()),
            ("backspace", "abc\x08\x08X", "aXc".into()),
            ("backspace at start", "\x08\x08X", "X".into()),
            ("tab expands to stop", "a\tb", format!("a{seven}b")),
            ("tab at stop", "12345678\tb", format!("12345678{eight}b")),
            (
                "tab moves without erasing",
                "abcdefghij\rX\tY",
                "XbcdefghYj".into(),
            ),
            (
                "control chars dropped",
                "a\x07b\x00c\x7fd\x0ce",
                "abcde".into(),
            ),
            ("c1 dropped", "a\u{9b}b", "ab".into()),
            ("wide runes kept", "日本語", "日本語".into()),
            ("trailing spaces trimmed", "foo\t", "foo".into()),
            ("empty", "", String::new()),
        ];
        for (name, input, want) in cases {
            let got = sanitize_console_line(input);
            assert_eq!(got, want, "{name}: sanitize_console_line({input:?})");
            // The whole point: the width the layout measures must be the
            // width the terminal shows, so nothing can spill out of the box.
            for c in got.chars() {
                assert!(
                    (c as u32) >= 0x20 && c != '\x7f',
                    "{name}: sanitized {got:?} still contains control {:#x}",
                    c as u32
                );
                assert!(
                    c.width().unwrap_or(0) >= 1,
                    "{name}: sanitized {got:?} still has zero-width content"
                );
            }
        }
    }

    #[test]
    fn escape_len_cases() {
        let chars = |s: &str| s.chars().collect::<Vec<_>>();
        assert_eq!(escape_len(&chars("")), 0);
        assert_eq!(escape_len(&chars("[1;32mok")), 6);
        assert_eq!(
            escape_len(&chars("[12")),
            3,
            "an unterminated CSI runs to the end"
        );
        assert_eq!(escape_len(&chars("]0;title\x07x")), 9);
        assert_eq!(escape_len(&chars("]0;title\x1b\\x")), 10);
        assert_eq!(
            escape_len(&chars("]0;title\x1b[0m")),
            8,
            "a new ESC cuts an OSC short"
        );
        assert_eq!(
            escape_len(&chars("]0;title")),
            8,
            "an OSC without a terminator eats the line"
        );
        assert_eq!(escape_len(&chars("(Bfoo")), 2);
        assert_eq!(escape_len(&chars("=foo")), 1);
    }

    #[test]
    fn wrap_console_line_cases() {
        let cases: Vec<(&str, &str, usize, Vec<&str>)> = vec![
            ("short", "abc", 5, vec!["abc"]),
            ("exact", "abcde", 5, vec!["abcde"]),
            ("split", "abcdefgh", 5, vec!["abcde", "fgh"]),
            (
                "multiple splits",
                "abcdefghijkl",
                5,
                vec!["abcde", "fghij", "kl"],
            ),
            (
                "wide rune does not straddle",
                "日本語",
                5,
                vec!["日本", "語"],
            ),
            ("keeps spaces", "ab cd ef", 3, vec!["ab ", "cd ", "ef"]),
            ("no width", "abcdefgh", 0, vec!["abcdefgh"]),
            ("empty", "", 5, vec![""]),
        ];
        for (name, input, width, want) in cases {
            assert_eq!(
                wrap_console_line(input, width),
                want,
                "{name}: wrap_console_line({input:?}, {width})"
            );
        }
    }

    /// Renders the view into a test terminal and returns the screen lines.
    fn render_view(view: &mut ConsoleView, width: u16, height: u16, focused: bool) -> Vec<String> {
        let theme = Theme::default();
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|f| view.render(f, f.area(), &theme, Line::raw(" Serial console "), focused))
            .expect("draw");
        buffer_lines(terminal.backend().buffer())
    }

    /// Lines 0..n where line i is i × `x`, like the Go test's.
    fn x_lines(n: usize) -> Vec<String> {
        (0..n).map(|i| "x".repeat(i)).collect()
    }

    /// The viewport sticks to the newest output until the user scrolls up,
    /// re-wraps on resize, and never renders a line wider than the box.
    #[test]
    fn console_follows_tail() {
        let mut v = ConsoleView::new();
        assert!(v.at_bottom(), "an empty view follows");
        // A 60×19 pane gives a 56×17 text area, the Go viewport's size.
        render_view(&mut v, 60, 19, true);
        v.set_lines(x_lines(100));
        render_view(&mut v, 60, 19, true);
        assert!(
            v.at_bottom(),
            "first refresh should show the tail, scroll={}",
            v.scroll
        );
        assert_eq!(v.scroll, v.max_scroll());
        assert!(v.wrapped.len() > 100, "lines wider than 56 cells wrap");

        v.scroll_up(5);
        assert!(!v.at_bottom());
        let off = v.scroll;
        let mut more = x_lines(100);
        more.push("new".into());
        v.set_lines(more.clone());
        assert_eq!(v.scroll, off, "refresh moved a scrolled-up view");
        assert!(!v.at_bottom());

        v.goto_bottom();
        more.push("newer".into());
        v.set_lines(more);
        assert!(
            v.at_bottom(),
            "refresh at the bottom should follow the tail"
        );

        // Resize at the bottom keeps the tail; every line fits the new box.
        let lines = render_view(&mut v, 30, 19, true);
        assert!(
            v.at_bottom(),
            "resize at the bottom should keep the tail, scroll={}",
            v.scroll
        );
        assert_eq!(v.wrap_width, 26);
        for (i, l) in lines.iter().enumerate() {
            assert!(
                l.width() <= 30,
                "screen line {i} is {} cells wide, terminal is 30: {l:?}",
                l.width()
            );
        }
        assert!(
            screen_contains(&lines, "newer"),
            "the tail is visible: {lines:#?}"
        );

        // Scrolling back down to the end re-attaches; the top detaches.
        v.goto_top();
        assert!(!v.at_bottom());
        assert_eq!(v.scroll, 0);
        v.scroll_down(usize::MAX / 2);
        assert!(v.at_bottom());
    }

    #[test]
    fn console_keys() {
        let mut v = ConsoleView::new();
        v.set_lines(x_lines(40));
        render_view(&mut v, 20, 12, true); // 10 visible rows
        assert_eq!(v.viewport, 10);
        let bottom = v.scroll;
        assert!(v.handle_key(ch('k')));
        assert_eq!(v.scroll, bottom - 1);
        assert!(v.handle_key(key(KeyCode::Up)));
        assert_eq!(v.scroll, bottom - 2);
        assert!(v.handle_key(ch('j')));
        assert!(v.handle_key(key(KeyCode::Down)));
        assert_eq!(v.scroll, bottom);
        assert!(v.at_bottom());
        assert!(v.handle_key(ctrl('u')));
        assert_eq!(v.scroll, bottom - 5, "Ctrl-u is half a page");
        assert!(v.handle_key(ctrl('d')));
        assert_eq!(v.scroll, bottom);
        assert!(v.handle_key(ctrl('b')));
        assert_eq!(v.scroll, bottom - 10, "Ctrl-b is a full page");
        assert!(v.handle_key(key(KeyCode::PageUp)));
        assert_eq!(v.scroll, bottom - 20);
        assert!(v.handle_key(ctrl('f')));
        assert!(v.handle_key(key(KeyCode::PageDown)));
        assert_eq!(v.scroll, bottom);
        assert!(v.handle_key(ch('g')));
        assert_eq!(v.scroll, 0);
        assert!(!v.at_bottom());
        assert!(v.handle_key(ch('G')));
        assert!(v.at_bottom());
        assert!(v.handle_key(key(KeyCode::Home)));
        assert!(v.handle_key(key(KeyCode::End)));
        assert!(v.at_bottom());
        // Letters the dashboard uses for VM actions are left alone.
        for c in ['s', 'x', 'd', 'u', 'f', 'b', 'h', 'q'] {
            assert!(
                !v.handle_key(ch(c)),
                "{c} must fall through to the dashboard"
            );
        }
        assert!(!v.handle_key(ctrl('k')));
    }

    #[test]
    fn console_render() {
        let mut v = ConsoleView::new();
        let lines = render_view(&mut v, 40, 6, false);
        assert!(screen_contains(&lines, NO_OUTPUT_TEXT), "{lines:#?}");
        assert!(screen_contains(&lines, "Serial console"), "{lines:#?}");
        assert!(
            !screen_contains(&lines, "following"),
            "no marker without output: {lines:#?}"
        );

        // A long line wraps instead of being cut, and the marker says the
        // view follows.
        v.set_lines(vec!["a".repeat(50), "tail".into()]);
        let lines = render_view(&mut v, 40, 6, true);
        assert!(!screen_contains(&lines, NO_OUTPUT_TEXT));
        assert!(screen_contains(&lines, &"a".repeat(36)), "{lines:#?}");
        assert!(screen_contains(&lines, &"a".repeat(14)), "{lines:#?}");
        assert!(
            !screen_contains(&lines, &"a".repeat(37)),
            "a wrapped piece is at most 36 cells: {lines:#?}"
        );
        assert!(screen_contains(&lines, "tail"), "{lines:#?}");
        assert!(screen_contains(&lines, "↓ following"), "{lines:#?}");
        for l in &lines {
            assert!(l.width() <= 40, "{l:?}");
        }

        // More lines than rows: the oldest scroll out, a scrollbar appears,
        // and scrolling up flips the marker.
        v.set_lines(x_lines(30));
        let lines = render_view(&mut v, 40, 6, true);
        assert!(
            screen_contains(&lines, &"x".repeat(29)),
            "the newest line is visible: {lines:#?}"
        );
        assert!(
            !screen_contains(&lines, "│ x "),
            "the oldest lines scrolled out: {lines:#?}"
        );
        assert!(
            lines.iter().any(|l| l.ends_with('█')),
            "a scrollbar thumb on the right border: {lines:#?}"
        );
        v.scroll_up(1);
        let lines = render_view(&mut v, 40, 6, true);
        assert!(screen_contains(&lines, "↑ scrolled"), "{lines:#?}");

        // Tiny and empty areas never panic.
        for (w, h) in [(0, 0), (1, 1), (2, 2), (3, 3), (4, 2), (40, 2), (3, 10)] {
            render_view(&mut v, w.max(1), h.max(1), true);
        }
        let theme = Theme::default();
        let mut terminal = Terminal::new(TestBackend::new(10, 10)).expect("terminal");
        terminal
            .draw(|f| v.render(f, Rect::new(0, 0, 0, 0), &theme, Line::raw("t"), false))
            .expect("draw");
    }

    /// A raw serial log with the sequences a guest really emits (clear
    /// screen, colours, tabs, progress bars), run through the sanitiser and
    /// the view: nothing leaks and nothing is wider than the pane.
    #[test]
    fn console_from_raw_log() {
        let log = [
            "\x1b[2J\x1b[H\x1b[?25lGNU GRUB  version 2.12".to_string(),
            "\x1b]0;qemu\x07\x1b(B\x1b[0;1;32m[  OK  ]\x1b[0m Started \x1b[0;1;39mNetwork Service\x1b[0m.".to_string(),
            "[    0.000000] Linux version 6.8.0\tx86_64\t#1 SMP".to_string(),
            "Downloading 10%\rDownloading 55%\rDownloading 100%\x1b[K".to_string(),
            "a very long kernel command line that keeps going ".repeat(6),
            String::new(),
            "login: \x1b[?25h".to_string(),
        ];
        let mut v = ConsoleView::new();
        v.set_lines(log.iter().map(|l| sanitize_console_line(l)).collect());
        let lines = render_view(&mut v, 80, 40, true);
        for (i, l) in lines.iter().enumerate() {
            assert!(
                l.width() <= 80,
                "view line {i} is {} cells wide: {l:?}",
                l.width()
            );
            for c in l.chars() {
                assert!(
                    (c as u32) >= 0x20 && c != '\x7f',
                    "view line {i} carries control {:#x}: {l:?}",
                    c as u32
                );
            }
        }
        for seq in [
            "\x1b[2J",
            "\x1b[H",
            "\x1b[?25l",
            "\x1b]0;",
            "\x1b(B",
            "\x1b[K",
        ] {
            assert!(
                !screen_contains(&lines, seq),
                "guest sequence {seq:?} leaked into the view"
            );
        }
        for want in [
            "GNU GRUB  version 2.12",
            "Downloading 100%",
            "[  OK  ] Started Network Service.",
            "login:",
        ] {
            assert!(
                screen_contains(&lines, want),
                "view lacks {want:?}: {lines:#?}"
            );
        }
        assert!(
            !screen_contains(&lines, "Downloading 10%"),
            "overwritten progress text survived"
        );
        assert!(
            screen_contains(&lines, "6.8.0      x86_64  #1 SMP"),
            "tabs expand to 8-column stops: {lines:#?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("│ line that keeps going")),
            "the long line wraps onto the next row at 76 cells: {lines:#?}"
        );
    }
}
