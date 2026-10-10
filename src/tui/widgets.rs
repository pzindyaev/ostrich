//! Small building blocks shared by every screen: a single-line text input,
//! a horizontal selector, a list cursor with scrolling, a spinner, and text
//! helpers.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::theme::Theme;

// ---------------------------------------------------------------------------
// Text input
// ---------------------------------------------------------------------------

/// A single-line text input with a cursor, like bubbles' textinput.
#[derive(Debug, Clone, Default)]
pub struct TextInput {
    value: Vec<char>,
    /// Cursor position in chars, 0..=value.len().
    cursor: usize,
    /// First visible char when the value is wider than the box.
    offset: usize,
    pub placeholder: String,
    /// Maximum number of chars; 0 for no limit.
    pub char_limit: usize,
    pub focused: bool,
}

impl TextInput {
    /// An empty input.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder: the initial value, cursor at its end.
    pub fn with_value(mut self, value: &str) -> Self {
        self.set_value(value);
        self
    }

    /// Builder: the placeholder shown while empty.
    pub fn with_placeholder(mut self, placeholder: &str) -> Self {
        self.placeholder = placeholder.to_string();
        self
    }

    /// Builder: the char limit.
    pub fn with_char_limit(mut self, limit: usize) -> Self {
        self.char_limit = limit;
        self
    }

    /// Builder: focused from the start.
    pub fn focused(mut self) -> Self {
        self.focused = true;
        self
    }

    /// The text as typed.
    pub fn value(&self) -> String {
        self.value.iter().collect()
    }

    /// The text with surrounding whitespace removed, which is what forms validate.
    pub fn trimmed(&self) -> String {
        self.value().trim().to_string()
    }

    /// Whether the value is empty.
    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    /// Replaces the value and puts the cursor at its end.
    pub fn set_value(&mut self, value: &str) {
        self.value = value.chars().collect();
        if self.char_limit > 0 {
            self.value.truncate(self.char_limit);
        }
        self.cursor = self.value.len();
        self.offset = 0;
    }

    /// Takes the focus (the cursor is drawn).
    pub fn focus(&mut self) {
        self.focused = true;
    }

    /// Gives the focus up.
    pub fn blur(&mut self) {
        self.focused = false;
    }

    /// Inserts text at the cursor (typing, or a paste).
    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars() {
            if c == '\n' || c == '\r' {
                continue;
            }
            if self.char_limit > 0 && self.value.len() >= self.char_limit {
                break;
            }
            self.value.insert(self.cursor, c);
            self.cursor += 1;
        }
    }

    /// Applies an editing key. Returns true when the key was consumed, so
    /// callers can let unconsumed keys fall through to their own bindings.
    /// Handles printable chars, Backspace, Delete, ←/→, Home/End, Ctrl-a/e,
    /// Ctrl-u (clear to start), Ctrl-k (clear to end), Ctrl-w (delete word),
    /// Alt-b/f (word moves).
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char(c) if !ctrl && !alt => {
                self.insert_str(&c.to_string());
                true
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.value.remove(self.cursor);
                }
                true
            }
            KeyCode::Delete => {
                if self.cursor < self.value.len() {
                    self.value.remove(self.cursor);
                }
                true
            }
            KeyCode::Left if alt => {
                self.cursor = self.prev_word();
                true
            }
            KeyCode::Right if alt => {
                self.cursor = self.next_word();
                true
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                true
            }
            KeyCode::Right => {
                if self.cursor < self.value.len() {
                    self.cursor += 1;
                }
                true
            }
            KeyCode::Home => {
                self.cursor = 0;
                true
            }
            KeyCode::End => {
                self.cursor = self.value.len();
                true
            }
            KeyCode::Char('a') if ctrl => {
                self.cursor = 0;
                true
            }
            KeyCode::Char('e') if ctrl => {
                self.cursor = self.value.len();
                true
            }
            KeyCode::Char('u') if ctrl => {
                self.value.drain(..self.cursor);
                self.cursor = 0;
                true
            }
            KeyCode::Char('k') if ctrl => {
                self.value.truncate(self.cursor);
                true
            }
            KeyCode::Char('w') if ctrl => {
                let start = self.prev_word();
                self.value.drain(start..self.cursor);
                self.cursor = start;
                true
            }
            KeyCode::Char('b') if alt => {
                self.cursor = self.prev_word();
                true
            }
            KeyCode::Char('f') if alt => {
                self.cursor = self.next_word();
                true
            }
            _ => false,
        }
    }

    fn prev_word(&self) -> usize {
        let mut i = self.cursor;
        while i > 0 && self.value[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !self.value[i - 1].is_whitespace() {
            i -= 1;
        }
        i
    }

    fn next_word(&self) -> usize {
        let mut i = self.cursor;
        let n = self.value.len();
        while i < n && !self.value[i].is_whitespace() {
            i += 1;
        }
        while i < n && self.value[i].is_whitespace() {
            i += 1;
        }
        i
    }

    /// Draws the input into `area` (one line high; wider values scroll
    /// horizontally to keep the cursor visible) and places the terminal
    /// cursor when focused.
    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let width = area.width as usize;
        if self.value.is_empty() {
            let placeholder: String = self.placeholder.chars().take(width).collect();
            frame.render_widget(Span::styled(placeholder, theme.placeholder), area);
            if self.focused {
                frame.set_cursor_position((area.x, area.y));
            }
            return;
        }
        // Keep the cursor inside the visible window.
        let visible = width.saturating_sub(1).max(1); // one cell for the cursor past the end
        if self.cursor < self.offset {
            self.offset = self.cursor;
        } else if self.cursor >= self.offset + visible {
            self.offset = self.cursor + 1 - visible;
        }
        if self.offset > self.value.len() {
            self.offset = self.value.len();
        }
        let shown: String = self.value.iter().skip(self.offset).take(width).collect();
        frame.render_widget(Span::styled(shown, theme.input), area);
        if self.focused {
            let col: usize = self.value[self.offset..self.cursor]
                .iter()
                .map(|c| c.width().unwrap_or(0))
                .sum();
            let x = area.x + col.min(width.saturating_sub(1)) as u16;
            frame.set_cursor_position((x, area.y));
        }
    }

    /// The input as a span for callers that compose it into a larger line
    /// (no terminal cursor; a `▏` marks the cursor position when focused).
    pub fn as_span(&self, theme: &Theme) -> Span<'static> {
        if self.value.is_empty() {
            if self.focused {
                return Span::styled(format!("▏{}", self.placeholder), theme.placeholder);
            }
            return Span::styled(self.placeholder.clone(), theme.placeholder);
        }
        if !self.focused {
            return Span::styled(self.value(), theme.input);
        }
        let mut s: String = self.value[..self.cursor].iter().collect();
        s.push('▏');
        s.extend(self.value[self.cursor..].iter());
        Span::styled(s, theme.input)
    }
}

// ---------------------------------------------------------------------------
// Selector
// ---------------------------------------------------------------------------

/// A horizontal choice between a few labels, cycled with h/l.
#[derive(Debug, Clone)]
pub struct Selector {
    pub labels: Vec<String>,
    pub index: usize,
}

impl Selector {
    /// A selector over `labels`, starting at `index`.
    pub fn new<S: Into<String>>(labels: impl IntoIterator<Item = S>, index: usize) -> Self {
        let labels: Vec<String> = labels.into_iter().map(Into::into).collect();
        let index = if labels.is_empty() {
            0
        } else {
            index.min(labels.len() - 1)
        };
        Selector { labels, index }
    }

    /// Moves by `delta`, wrapping around.
    pub fn cycle(&mut self, delta: i32) {
        let n = self.labels.len() as i32;
        if n == 0 {
            return;
        }
        self.index = (((self.index as i32 + delta) % n + n) % n) as usize;
    }

    /// The chosen label.
    pub fn label(&self) -> &str {
        self.labels
            .get(self.index)
            .map(String::as_str)
            .unwrap_or("")
    }

    /// The selector as one line: every label padded with a space on each
    /// side, the chosen one in the selected style.
    pub fn line(&self, theme: &Theme) -> Line<'static> {
        let mut spans = Vec::with_capacity(self.labels.len() * 2);
        for (i, l) in self.labels.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw(" "));
            }
            let style = if i == self.index {
                theme.selected
            } else {
                theme.normal
            };
            spans.push(Span::styled(format!(" {l} "), style));
        }
        Line::from(spans)
    }

    /// [`Selector::line`] wrapped to `width` cells: as many choices to a
    /// line as fit, so the chosen one is never cut off at the pane's edge. A
    /// choice wider than a whole line is cut with `…`.
    pub fn lines(&self, theme: &Theme, width: usize) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut used = 0;
        for (i, l) in self.labels.iter().enumerate() {
            let style = if i == self.index {
                theme.selected
            } else {
                theme.normal
            };
            let text = format!(" {} ", truncate(l, width.saturating_sub(2)));
            let w = text.chars().count();
            if !spans.is_empty() && used + 1 + w > width {
                lines.push(Line::from(std::mem::take(&mut spans)));
                used = 0;
            }
            if !spans.is_empty() {
                spans.push(Span::raw(" "));
                used += 1;
            }
            spans.push(Span::styled(text, style));
            used += w;
        }
        lines.push(Line::from(spans));
        lines
    }
}

// ---------------------------------------------------------------------------
// List cursor
// ---------------------------------------------------------------------------

/// Which rows of `total` a scrolled list shows, with an arrow for each side
/// that has hidden ones: `  ↑↓ 3–4 of 7`.
pub fn window_note(shown: std::ops::Range<usize>, total: usize) -> String {
    let up = if shown.start > 0 { "↑" } else { "" };
    let down = if shown.end < total { "↓" } else { "" };
    format!("  {up}{down} {}–{} of {total}", shown.start + 1, shown.end)
}

/// A cursor into a list plus the scroll offset that keeps it visible.
#[derive(Debug, Clone, Copy, Default)]
pub struct Cursor {
    pub index: usize,
    pub offset: usize,
}

impl Cursor {
    /// Keeps the cursor inside `0..len` (0 when empty).
    pub fn clamp(&mut self, len: usize) {
        if len == 0 {
            self.index = 0;
            self.offset = 0;
        } else if self.index >= len {
            self.index = len - 1;
        }
    }

    pub fn up(&mut self, n: usize) {
        self.index = self.index.saturating_sub(n);
    }

    pub fn down(&mut self, n: usize, len: usize) {
        if len > 0 {
            self.index = (self.index + n).min(len - 1);
        }
    }

    pub fn top(&mut self) {
        self.index = 0;
    }

    pub fn bottom(&mut self, len: usize) {
        self.index = len.saturating_sub(1);
    }

    /// Puts the cursor on `index` if it is inside the list.
    pub fn select(&mut self, index: usize, len: usize) {
        if index < len {
            self.index = index;
        }
    }

    /// Adjusts the scroll offset so the cursor is within a window of
    /// `height` rows, and returns the range of indices to draw.
    pub fn window(&mut self, len: usize, height: usize) -> std::ops::Range<usize> {
        if height == 0 || len == 0 {
            self.offset = 0;
            return 0..0;
        }
        self.clamp(len);
        if self.index < self.offset {
            self.offset = self.index;
        } else if self.index >= self.offset + height {
            self.offset = self.index + 1 - height;
        }
        if self.offset + height > len {
            self.offset = len.saturating_sub(height);
        }
        self.offset..(self.offset + height).min(len)
    }

    /// Applies the vim-style list keys (j/k/↑/↓, g/G, Ctrl-d/Ctrl-u, Home/End,
    /// PageUp/PageDown) and reports whether the key was one of them. `height`
    /// is the visible row count, for the half-page moves.
    pub fn handle_key(&mut self, key: KeyEvent, len: usize, height: usize) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let half = (height / 2).max(1);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') if !ctrl => self.up(1),
            KeyCode::Down | KeyCode::Char('j') if !ctrl => self.down(1, len),
            KeyCode::Char('g') | KeyCode::Home if !ctrl => self.top(),
            KeyCode::Char('G') | KeyCode::End if !ctrl => self.bottom(len),
            KeyCode::Char('d') if ctrl => self.down(half, len),
            KeyCode::Char('u') if ctrl => self.up(half),
            KeyCode::PageDown => self.down(height.max(1), len),
            KeyCode::PageUp => self.up(height.max(1)),
            _ => return false,
        }
        true
    }
}

// ---------------------------------------------------------------------------
// Spinner
// ---------------------------------------------------------------------------

/// The braille spinner frames, one per 100 ms tick.
pub const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The spinner glyph for a tick count.
pub fn spinner_frame(tick: u64) -> &'static str {
    SPINNER_FRAMES[(tick % SPINNER_FRAMES.len() as u64) as usize]
}

// ---------------------------------------------------------------------------
// Text helpers
// ---------------------------------------------------------------------------

/// Formats a byte count in binary units: `4.3 GiB`, `631 MiB`, `12 KiB`, `5 B`.
pub fn human_size(n: u64) -> String {
    const UNIT: u64 = 1024;
    if n >= UNIT * UNIT * UNIT {
        format!("{:.1} GiB", n as f64 / (UNIT * UNIT * UNIT) as f64)
    } else if n >= UNIT * UNIT {
        format!("{} MiB", n / (UNIT * UNIT))
    } else if n >= UNIT {
        format!("{} KiB", n / UNIT)
    } else {
        format!("{n} B")
    }
}

/// A count and its noun: `1 core`, `2 cores`.
pub fn plural(n: u32, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Shortens `s` to at most `n` chars, ending with an ellipsis.
pub fn truncate(s: &str, n: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= n {
        return s.to_string();
    }
    if n == 0 {
        return String::new();
    }
    let mut out: String = chars[..n - 1].iter().collect();
    out.push('…');
    out
}

/// Shortens `s` to at most `n` chars, keeping its end (for paths, so the
/// file name stays visible).
pub fn truncate_left(s: &str, n: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= n {
        return s.to_string();
    }
    if n == 0 {
        return String::new();
    }
    let mut out = String::from("…");
    out.extend(chars[chars.len() - n + 1..].iter());
    out
}

/// The longest start of `s` at most `width` cells wide.
fn head_w(s: &str, width: usize) -> &str {
    let mut used = 0;
    for (i, c) in s.char_indices() {
        let w = c.width().unwrap_or(0);
        if used + w > width {
            return &s[..i];
        }
        used += w;
    }
    s
}

/// The longest end of `s` at most `width` cells wide.
fn tail_w(s: &str, width: usize) -> &str {
    let mut used = 0;
    for (i, c) in s.char_indices().rev() {
        let w = c.width().unwrap_or(0);
        if used + w > width {
            return &s[i + c.len_utf8()..];
        }
        used += w;
    }
    s
}

/// `s` shortened to at most `width` cells ending in `…`, cut at a word
/// boundary when one is near enough, and without a dangling separator
/// (`UEFI + Secure Boot, TPM 2.0` at 9 cells is `UEFI…`, not `UEFI + S…`).
pub fn ellipsize(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let room = width - 1;
    let head = head_w(s, room);
    let mid_word = s[head.len()..]
        .chars()
        .next()
        .is_some_and(|c| !c.is_whitespace());
    let mut kept = head;
    if mid_word {
        // Back up to the last space if that keeps two thirds of the room.
        if let Some(i) = head.rfind(char::is_whitespace) {
            if head[..i].width() * 3 >= room * 2 {
                kept = &head[..i];
            }
        }
    }
    let trimmed = kept.trim_end_matches(|c: char| c.is_whitespace() || ",;:+-—·".contains(c));
    let kept = if trimmed.is_empty() { head } else { trimmed };
    format!("{kept}…")
}

/// `s` as it is when it fits in `width` cells, [`ellipsize`]d otherwise.
pub fn fit_words(s: &str, width: usize) -> String {
    if s.width() <= width {
        s.to_string()
    } else {
        ellipsize(s, width)
    }
}

/// `s` as it is when it fits in `width` cells, otherwise its end after a
/// `…`, so a path keeps its file name.
pub fn fit_left(s: &str, width: usize) -> String {
    if s.width() <= width {
        s.to_string()
    } else if width == 0 {
        String::new()
    } else {
        format!("…{}", tail_w(s, width - 1))
    }
}

/// Pads `s` with spaces on the right to `width` display cells, truncating
/// (with an ellipsis) when it is wider.
pub fn pad_right(s: &str, width: usize) -> String {
    let t = if s.width() > width {
        truncate(s, width)
    } else {
        s.to_string()
    };
    let w = t.width();
    let mut out = t;
    out.extend(std::iter::repeat_n(' ', width.saturating_sub(w)));
    out
}

/// `s` with every line prefixed.
pub fn indent(s: &str, prefix: &str) -> String {
    s.lines()
        .map(|l| format!("{prefix}{l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `s`, or `fallback` when `s` is empty.
pub fn if_empty<'a>(s: &'a str, fallback: &'a str) -> &'a str {
    if s.is_empty() {
        fallback
    } else {
        s
    }
}

/// Packs shell words into lines of at most `width` columns, ending every
/// line but the last with a backslash continuation. Lines never break inside
/// a word, so the block pastes into a shell as a single command.
pub fn shell_lines(words: &[String], width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    for w in words {
        if cur.is_empty() {
            cur = w.clone();
        } else if cur.len() + 1 + w.len() + 2 > width {
            lines.push(format!("{cur} \\"));
            cur = w.clone();
        } else {
            cur.push(' ');
            cur.push_str(w);
        }
    }
    lines.push(cur);
    lines
}

/// Word-wraps plain text to `width` cells, keeping existing line breaks and
/// breaking an over-long word where it must.
pub fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for raw in text.split('\n') {
        if raw.width() <= width {
            out.push(raw.to_string());
            continue;
        }
        let mut line = String::new();
        let mut line_w = 0;
        for word in raw.split(' ') {
            let ww = word.width();
            if line_w > 0 && line_w + 1 + ww > width {
                out.push(std::mem::take(&mut line));
                line_w = 0;
            }
            if ww > width {
                // Break the word itself.
                for c in word.chars() {
                    let cw = c.width().unwrap_or(0);
                    if line_w + cw > width && line_w > 0 {
                        out.push(std::mem::take(&mut line));
                        line_w = 0;
                    }
                    line.push(c);
                    line_w += cw;
                }
                continue;
            }
            if line_w > 0 {
                line.push(' ');
                line_w += 1;
            }
            line.push_str(word);
            line_w += ww;
        }
        out.push(line);
    }
    out
}

/// A rectangle of at most `width` × `height` centred in `area`.
pub fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    let x = area.x + (area.width - w) / 2;
    let y = area.y + (area.height - h) / 2;
    Rect::new(x, y, w, h)
}

/// A rectangle covering `percent_x` × `percent_y` of `area`, centred.
pub fn centered_percent(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let w = (area.width as u32 * percent_x as u32 / 100) as u16;
    let h = (area.height as u32 * percent_y as u32 / 100) as u16;
    centered_rect(area, w.max(1), h.max(1))
}

/// A line of key hints: `key desc   key desc ...`, keys bold.
pub fn hints_line(hints: &[(&str, &str)], theme: &Theme) -> Line<'static> {
    let mut spans = Vec::with_capacity(hints.len() * 3);
    for (i, (k, d)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled((*k).to_string(), theme.hint_key));
        spans.push(Span::styled(format!(" {d}"), theme.hint_desc));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventKind;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    #[test]
    fn text_input_edits() {
        let mut t = TextInput::new().with_value("ab");
        assert!(t.handle_key(key(KeyCode::Char('c'))));
        assert_eq!(t.value(), "abc");
        t.handle_key(key(KeyCode::Left));
        t.handle_key(key(KeyCode::Backspace));
        assert_eq!(t.value(), "ac");
        t.handle_key(key(KeyCode::Home));
        t.handle_key(key(KeyCode::Delete));
        assert_eq!(t.value(), "c");
        t.set_value("hello world");
        t.handle_key(ctrl('w'));
        assert_eq!(t.value(), "hello ");
        t.handle_key(ctrl('u'));
        assert_eq!(t.value(), "");
        assert!(
            !t.handle_key(key(KeyCode::Enter)),
            "enter is for the caller"
        );
        let mut lim = TextInput::new().with_char_limit(3);
        lim.insert_str("abcdef");
        assert_eq!(lim.value(), "abc");
        assert_eq!(TextInput::new().with_value("  x ").trimmed(), "x");
    }

    #[test]
    fn selector_wraps() {
        let mut s = Selector::new(["a", "b", "c"], 0);
        s.cycle(-1);
        assert_eq!(s.index, 2);
        s.cycle(1);
        assert_eq!(s.index, 0);
        s.cycle(4);
        assert_eq!(s.label(), "b");
    }

    #[test]
    fn selector_lines_wrap_whole_choices_and_keep_the_chosen_style() {
        let theme = Theme::default();
        let s = Selector::new(["BIOS", "UEFI", "UEFI + Secure Boot"], 2);
        let text =
            |lines: &[Line]| -> Vec<String> { lines.iter().map(|l| l.to_string()).collect() };
        // Room for all: the same as line().
        let wide = s.lines(&theme, 80);
        assert_eq!(text(&wide), [s.line(&theme).to_string()]);
        // " BIOS   UEFI " is 13 cells; the third choice goes below.
        let narrow = s.lines(&theme, 30);
        assert_eq!(text(&narrow), [" BIOS   UEFI ", " UEFI + Secure Boot "]);
        assert_eq!(narrow[1].spans[0].style, theme.selected);
        // Narrower than a choice: that choice is cut, never dropped.
        assert_eq!(
            text(&s.lines(&theme, 10)),
            [" BIOS ", " UEFI ", " UEFI + … "]
        );
    }

    #[test]
    fn window_note_names_the_shown_rows_and_the_hidden_sides() {
        assert_eq!(window_note(0..2, 7), "  ↓ 1–2 of 7");
        assert_eq!(window_note(2..4, 7), "  ↑↓ 3–4 of 7");
        assert_eq!(window_note(5..7, 7), "  ↑ 6–7 of 7");
    }

    #[test]
    fn cursor_window_follows_cursor() {
        let mut c = Cursor::default();
        assert_eq!(c.window(10, 3), 0..3);
        c.down(5, 10);
        assert_eq!(c.window(10, 3), 3..6);
        c.bottom(10);
        assert_eq!(c.window(10, 3), 7..10);
        c.top();
        assert_eq!(c.window(10, 3), 0..3);
        c.index = 9;
        c.clamp(4);
        assert_eq!(c.index, 3);
        c.clamp(0);
        assert_eq!(c.window(0, 3), 0..0);
        let mut c = Cursor::default();
        assert!(c.handle_key(ctrl('d'), 20, 8));
        assert_eq!(c.index, 4);
        assert!(c.handle_key(key(KeyCode::Char('G')), 20, 8));
        assert_eq!(c.index, 19);
        assert!(!c.handle_key(key(KeyCode::Char('x')), 20, 8));
    }

    #[test]
    fn text_helpers() {
        assert_eq!(human_size(5), "5 B");
        assert_eq!(human_size(2048), "2 KiB");
        assert_eq!(human_size(631 * 1024 * 1024), "631 MiB");
        assert_eq!(human_size(4617089843), "4.3 GiB");
        assert_eq!(plural(1, "core", "cores"), "1 core");
        assert_eq!(plural(0, "core", "cores"), "0 cores");
        assert_eq!(plural(4, "core", "cores"), "4 cores");
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("abc", 4), "abc");
        assert_eq!(truncate_left("/home/user/iso/debian.iso", 10), "…ebian.iso");
        assert_eq!(pad_right("ab", 4), "ab  ");
        assert_eq!(pad_right("abcdef", 4), "abc…");
        assert_eq!(indent("a\nb", "  "), "  a\n  b");
        assert_eq!(if_empty("", "x"), "x");
        assert_eq!(if_empty("y", "x"), "y");
        let words: Vec<String> = ["echo", "'a'", "|", "sudo", "tee"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(shell_lines(&words, 12), vec!["echo 'a' | \\", "sudo tee"]);
        assert_eq!(wrap_text("aaa bbb ccc", 7), vec!["aaa bbb", "ccc"]);
        assert_eq!(wrap_text("abcdefgh", 3), vec!["abc", "def", "gh"]);
        assert_eq!(wrap_text("x\ny", 10), vec!["x", "y"]);
        let r = centered_rect(Rect::new(0, 0, 100, 40), 50, 10);
        assert_eq!(r, Rect::new(25, 15, 50, 10));
        let r = centered_rect(Rect::new(0, 0, 10, 5), 50, 10);
        assert_eq!(r, Rect::new(0, 0, 10, 5));
    }

    #[test]
    fn text_fitting_cuts_at_words_and_keeps_path_ends() {
        assert_eq!(
            fit_words("UEFI + Secure Boot, TPM 2.0", 27),
            "UEFI + Secure Boot, TPM 2.0"
        );
        assert_eq!(
            fit_words("UEFI + Secure Boot, TPM 2.0", 26),
            "UEFI + Secure Boot, TPM…"
        );
        assert_eq!(
            fit_words("UEFI + Secure Boot, TPM 2.0", 20),
            "UEFI + Secure Boot…"
        );
        assert_eq!(fit_words("UEFI + Secure Boot, TPM 2.0", 9), "UEFI…");
        assert_eq!(fit_words("UEFI, TPM 2.0", 8), "UEFI…");
        // A word too long to back up over is cut inside.
        assert_eq!(fit_words("a-very-long-vm-name", 8), "a-very…");
        assert_eq!(fit_words("abcdefgh", 4), "abc…");
        assert_eq!(fit_words("abc", 1), "…");
        assert_eq!(fit_words("abc", 0), "");
        assert_eq!(
            fit_left("/home/user/iso/debian.iso", 25),
            "/home/user/iso/debian.iso"
        );
        assert_eq!(fit_left("/home/user/iso/debian.iso", 12), "…/debian.iso");
        assert_eq!(fit_left("/x", 0), "");
        // Wide characters count two cells.
        assert_eq!(fit_words("日本語のパス", 7), "日本語…");
        assert_eq!(fit_left("/データ/イメージ.iso", 9), "…ージ.iso");
        assert_eq!(fit_left("/データ/イメージ.iso", 9).width(), 9);
    }
}
