//! Centred modal popups over the dashboard: a yes/no confirmation, a
//! scrollable message (long errors), the wait before quitting, and the key
//! help.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{
    Block, BorderType, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};
use unicode_width::UnicodeWidthStr;

use super::theme::Theme;
use super::widgets::{centered_rect, hints_line, pad_right, wrap_text};

/// What a confirmation runs when answered with `y`.
#[derive(Debug, Clone)]
pub enum ConfirmAction {
    DeleteVm(String),
    DeleteTemplate(String),
}

/// A modal popup.
#[derive(Debug, Clone)]
pub enum Popup {
    /// `y` confirms, any other key cancels.
    Confirm { text: String, action: ConfirmAction },
    /// Esc/Enter/q close; j/k scroll.
    Message {
        title: String,
        text: String,
        scroll: usize,
    },
    /// Quitting waits for `label` (`deleting debian-12…`) to be done. Esc
    /// or Enter stay (the quit is called off), any other key keeps waiting;
    /// Ctrl-c quits now, which the dashboard handles before the popup.
    Quitting { label: String },
    /// The key reference; j/k scroll when it is taller than the screen,
    /// any other key closes.
    Help { scroll: usize },
}

impl Popup {
    pub fn confirm(text: impl Into<String>, action: ConfirmAction) -> Self {
        Popup::Confirm {
            text: text.into(),
            action,
        }
    }

    pub fn message(title: impl Into<String>, text: impl Into<String>) -> Self {
        Popup::Message {
            title: title.into(),
            text: text.into(),
            scroll: 0,
        }
    }

    pub fn quitting(label: impl Into<String>) -> Self {
        Popup::Quitting {
            label: label.into(),
        }
    }

    pub fn help() -> Self {
        Popup::Help { scroll: 0 }
    }
}

/// The Quitting popup's text: `Still deleting debian-12…`, a blank line,
/// and what happens next.
fn quitting_text(label: &str) -> String {
    format!(
        "Still {}…\n\nOstrich quits as soon as that is done.",
        label.trim_end_matches('…')
    )
}

/// What a key press did to the open popup.
#[derive(Debug)]
pub enum KeyOutcome {
    /// The popup stays open, possibly scrolled.
    Keep(Popup),
    /// The popup closes.
    Close,
    /// The confirmation was answered `y`: the popup closes and this runs.
    Confirm(ConfirmAction),
    /// The Quitting popup was answered Esc or Enter: it closes and the
    /// quit is called off.
    Stay,
}

/// Applies a key to the open popup: `y` answers a confirmation (any other
/// key cancels it); j/k/↓/↑ scroll a message or the key help; Esc, Enter or
/// q close a message; Esc or Enter on the Quitting popup stay, any other
/// key leaves it up; any other key closes the help.
pub fn handle_key(popup: Popup, key: KeyEvent) -> KeyOutcome {
    match popup {
        Popup::Quitting { .. } => match key.code {
            KeyCode::Esc | KeyCode::Enter => KeyOutcome::Stay,
            _ => KeyOutcome::Keep(popup),
        },
        Popup::Confirm { action, .. } => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => KeyOutcome::Confirm(action),
            _ => KeyOutcome::Close,
        },
        Popup::Message {
            title,
            text,
            scroll,
        } => match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => KeyOutcome::Close,
            _ => KeyOutcome::Keep(Popup::Message {
                title,
                text,
                scroll: scrolled(scroll, key).unwrap_or(scroll),
            }),
        },
        Popup::Help { scroll } => match scrolled(scroll, key) {
            Some(scroll) => KeyOutcome::Keep(Popup::Help { scroll }),
            None => KeyOutcome::Close,
        },
    }
}

/// The scroll offset after a scroll key (j/k/↓/↑), or `None` for any other
/// key. Going past the end is pulled back when the popup is drawn.
fn scrolled(scroll: usize, key: KeyEvent) -> Option<usize> {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return None;
    }
    match key.code {
        KeyCode::Down | KeyCode::Char('j') => Some(scroll.saturating_add(1)),
        KeyCode::Up | KeyCode::Char('k') => Some(scroll.saturating_sub(1)),
        _ => None,
    }
}

/// The bottom-bar hints while a popup is open; the popup's own footer
/// shows the same.
pub fn key_hints(popup: &Popup) -> Vec<(String, String)> {
    let pairs: &[(&str, &str)] = match popup {
        Popup::Confirm { .. } => &[("y", "confirm"), ("any other key", "cancel")],
        Popup::Message { .. } => &[("j/k", "scroll"), ("Esc/Enter", "close")],
        Popup::Quitting { .. } => &[("Esc", "stay"), ("Ctrl-c", "quit now")],
        Popup::Help { .. } => &[("j/k", "scroll"), ("any other key", "close")],
    };
    pairs
        .iter()
        .map(|(k, d)| (k.to_string(), d.to_string()))
        .collect()
}

/// The dashboard's key reference: sections of (keys, what they do), each
/// under a heading that says where its keys work (none for the first).
pub const HELP: &[(&str, &[(&str, &str)])] = &[
    (
        "",
        &[
            ("j/k ↑/↓", "move in the focused list or scroll the console"),
            ("g/G", "top / bottom"),
            ("Ctrl-d/Ctrl-u", "half page down / up"),
            (
                "Tab / Shift-Tab",
                "focus the next / previous pane (VMs, templates, console)",
            ),
            (
                "Enter / l",
                "focus the console of the selected VM (VM list)",
            ),
            ("h / Esc", "back to the VM list"),
            ("T", "focus the templates pane"),
            (
                "n",
                "new VM (from the selected template when the templates pane is focused)",
            ),
            ("d", "delete the selected VM or template (asks first)"),
            ("r", "refresh now"),
            ("?", "this help"),
            ("q / Ctrl-c", "quit (VMs keep running)"),
        ],
    ),
    (
        "The selected VM (VM list and console)",
        &[
            ("s / x", "start / stop"),
            ("e", "edit"),
            ("u", "USB passthrough"),
            ("i", "ISO hot-plug"),
            ("t", "save as a template (it must be stopped)"),
            (
                "c",
                "connect to the serial console (socat; Ctrl-] to disconnect)",
            ),
            ("v", "launch a VNC viewer"),
        ],
    ),
];

/// The widest key in the key reference: the width of its key column.
fn help_key_width() -> usize {
    HELP.iter()
        .flat_map(|(_, keys)| keys.iter())
        .map(|(k, _)| k.width())
        .max()
        .unwrap_or(0)
}

/// The widest word in the key reference's descriptions: the narrowest the
/// description column can be without cutting one.
fn help_word_width() -> usize {
    HELP.iter()
        .flat_map(|(_, keys)| keys.iter())
        .flat_map(|(_, d)| d.split_whitespace())
        .map(UnicodeWidthStr::width)
        .max()
        .unwrap_or(0)
}

/// The key reference laid out `width` cells wide: headings flush left and
/// wrapped, the keys in a fixed column and each description wrapped beside
/// them. When the column beside the keys is too narrow for a word, each
/// description goes under its key instead, indented two cells.
fn help_lines(width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let key_w = help_key_width();
    let beside = width.saturating_sub(key_w + 2) >= help_word_width();
    let mut out = Vec::new();
    for (i, (heading, keys)) in HELP.iter().enumerate() {
        if i > 0 {
            out.push(Line::raw(""));
        }
        if !heading.is_empty() {
            for part in wrap_text(heading, width) {
                out.push(Line::styled(part, theme.label));
            }
        }
        for (k, d) in keys.iter() {
            if beside {
                let desc_w = width - (key_w + 2);
                for (j, part) in wrap_text(d, desc_w).into_iter().enumerate() {
                    let key = if j == 0 { *k } else { "" };
                    out.push(Line::from(vec![
                        Span::styled(pad_right(key, key_w + 2), theme.hint_key),
                        Span::styled(part, theme.normal),
                    ]));
                }
            } else {
                out.extend(
                    wrap_text(k, width)
                        .into_iter()
                        .map(|part| Line::styled(part, theme.hint_key)),
                );
                for part in wrap_text(d, width.saturating_sub(2)) {
                    out.push(Line::from(vec![
                        Span::raw("  "),
                        Span::styled(part, theme.normal),
                    ]));
                }
            }
        }
    }
    out
}

/// The key reference's width with nothing wrapped, borders and padding
/// included.
fn help_natural_width() -> u16 {
    let key_w = help_key_width();
    let widest = HELP
        .iter()
        .flat_map(|(heading, keys)| {
            std::iter::once(heading.width()).chain(keys.iter().map(|(_, d)| key_w + 2 + d.width()))
        })
        .max()
        .unwrap_or(0);
    widest as u16 + 4
}

/// The popup's key hints for its bottom border, in the hints bar's words.
fn footer(popup: &Popup, theme: &Theme) -> Line<'static> {
    let hints = key_hints(popup);
    let pairs: Vec<(&str, &str)> = hints
        .iter()
        .map(|(k, d)| (k.as_str(), d.as_str()))
        .collect();
    let mut line = hints_line(&pairs, theme);
    line.spans.insert(0, Span::raw(" "));
    line.spans.push(Span::raw(" "));
    line.right_aligned()
}

/// A scrollbar on the right border of `rect` when `total` rows do not fit
/// in `visible`.
fn render_scrollbar(
    frame: &mut Frame,
    rect: Rect,
    total: usize,
    visible: usize,
    scroll: usize,
    style: Style,
) {
    if total <= visible {
        return;
    }
    let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(None)
        .end_symbol(None)
        .track_symbol(Some("│"))
        .track_style(style)
        .thumb_style(style);
    let mut state = ScrollbarState::new(total - visible + 1)
        .position(scroll)
        .viewport_content_length(visible);
    frame.render_stateful_widget(bar, rect.inner(Margin::new(0, 1)), &mut state);
}

/// A short text in a box two thirds of the screen wide (30–80 cells, as
/// the footer needs, never wider than the screen) and as tall as the text
/// with a blank row above and below; the border and `title` in `style`
/// (border, title), the keys in `footer`. The confirmation and the
/// Quitting popup.
fn render_short(
    frame: &mut Frame,
    title: &str,
    text: &str,
    style: (Style, Style),
    footer: Line<'static>,
    theme: &Theme,
) {
    let area = frame.area();
    // Wide enough for the footer, and no wider than the screen.
    let min_w = footer.width() as u16 + 4;
    let width = (area.width * 2 / 3)
        .clamp(30, 80)
        .max(min_w)
        .min(area.width);
    let lines = wrap_text(text, width.saturating_sub(4) as usize);
    let height = lines.len() as u16 + 4;
    let rect = centered_rect(area, width, height);
    frame.render_widget(Clear, rect);
    let (border, title_style) = style;
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(border)
        .title(Line::styled(format!(" {title} "), title_style))
        .title_bottom(footer);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let body: Vec<Line> = lines
        .into_iter()
        .map(|l| Line::styled(l, theme.normal))
        .collect();
    frame.render_widget(Paragraph::new(body), inner.inner(Margin::new(1, 1)));
}

/// Draws the popup centred on the frame. A scroll offset past the end is
/// pulled back to the last page, so `k` moves at once after over-scrolling.
pub fn render(popup: &mut Popup, frame: &mut Frame, theme: &Theme) {
    let area = frame.area();
    let footer = footer(popup, theme);
    // Wide enough for the footer whatever the screen, as far as it goes.
    let min_w = (footer.width() as u16 + 4).min(area.width);
    match popup {
        Popup::Confirm { text, .. } => render_short(
            frame,
            "Confirm",
            text,
            (theme.warn, theme.warn),
            footer,
            theme,
        ),
        // Not an error: the border and title of a focused pane.
        Popup::Quitting { label } => render_short(
            frame,
            "Quitting",
            &quitting_text(label),
            (theme.border_focused, theme.title),
            footer,
            theme,
        ),
        Popup::Message {
            title,
            text,
            scroll,
        } => {
            let width = (area.width * 3 / 4).clamp(40, 110).max(min_w);
            let inner_w = width.saturating_sub(4) as usize;
            let lines = wrap_text(text, inner_w);
            let height = (lines.len() as u16 + 2).min(area.height * 3 / 4).max(5);
            let rect = centered_rect(area, width, height);
            frame.render_widget(Clear, rect);
            let block = Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(theme.error)
                .title(Line::styled(format!(" {title} "), theme.error))
                .title_bottom(footer);
            let inner = block.inner(rect);
            frame.render_widget(block, rect);
            let total = lines.len();
            let visible = inner.height as usize;
            *scroll = (*scroll).min(total.saturating_sub(visible));
            let body: Vec<Line> = lines
                .into_iter()
                .map(|l| Line::styled(l, theme.normal))
                .collect();
            frame.render_widget(
                Paragraph::new(body)
                    .wrap(Wrap { trim: false })
                    .scroll((*scroll as u16, 0)),
                inner.inner(Margin::new(1, 0)),
            );
            render_scrollbar(frame, rect, total, visible, *scroll, theme.error);
        }
        Popup::Help { scroll } => {
            // The whole reference on one line per key when the screen has
            // room; on a small screen all but two cells each side, wrapped;
            // never wider than the screen, so it is wrapped for the width it
            // is drawn at.
            let width = help_natural_width()
                .min(area.width.saturating_sub(4).max(40))
                .max(min_w)
                .min(area.width);
            let lines = help_lines(width.saturating_sub(4) as usize, theme);
            let height = (lines.len() as u16 + 2).min(area.height.saturating_sub(2));
            let rect = centered_rect(area, width, height);
            frame.render_widget(Clear, rect);
            let block = Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(theme.border_focused)
                .title(Line::styled(" Keys ", theme.title))
                .title_bottom(footer);
            let inner = block.inner(rect);
            frame.render_widget(block, rect);
            let total = lines.len();
            let visible = inner.height as usize;
            *scroll = (*scroll).min(total.saturating_sub(visible));
            frame.render_widget(
                Paragraph::new(lines).scroll((*scroll as u16, 0)),
                inner.inner(Margin::new(1, 0)),
            );
            render_scrollbar(frame, rect, total, visible, *scroll, theme.border_focused);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::testutil::{ch, ctrl, key};
    use ratatui::backend::TestBackend;

    /// The two screen sizes every popup is checked at.
    const SIZES: [(u16, u16); 2] = [(80, 24), (160, 45)];

    /// A popup drawn on an otherwise empty screen.
    struct Shot {
        rows: Vec<Vec<String>>,
        top: usize,
        bottom: usize,
        left: usize,
        right: usize,
    }

    impl Shot {
        fn take(popup: &mut Popup, w: u16, h: u16) -> Shot {
            let mut t = Terminal::new(TestBackend::new(w, h)).expect("terminal");
            t.draw(|f| render(popup, f, &Theme::default()))
                .expect("draw");
            let buf = t.backend().buffer();
            let rows: Vec<Vec<String>> = (0..h)
                .map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect())
                .collect();
            let find = |corner: &str| {
                rows.iter()
                    .enumerate()
                    .find_map(|(y, r)| r.iter().position(|c| c == corner).map(|x| (y, x)))
                    .unwrap_or_else(|| panic!("no {corner} on screen"))
            };
            let (top, left) = find("╭");
            let (bottom, right) = find("╯");
            Shot {
                rows,
                top,
                bottom,
                left,
                right,
            }
        }

        fn width(&self) -> usize {
            self.right - self.left + 1
        }

        /// A screen row between the popup's borders (borders excluded).
        fn inner(&self, y: usize) -> String {
            self.rows[y][self.left + 1..self.right].concat()
        }

        /// The rows between the top and bottom borders.
        fn body(&self) -> Vec<String> {
            (self.top + 1..self.bottom).map(|y| self.inner(y)).collect()
        }

        /// The bottom border, corners included.
        fn bottom_border(&self) -> String {
            self.rows[self.bottom][self.left..=self.right].concat()
        }

        /// Whether the right border carries a scrollbar thumb.
        fn has_scrollbar(&self) -> bool {
            (self.top + 1..self.bottom).any(|y| self.rows[y][self.right] == "█")
        }

        fn dump(&self) -> String {
            self.rows
                .iter()
                .map(|r| r.concat().trim_end().to_string())
                .collect::<Vec<_>>()
                .join("\n")
        }
    }

    fn confirm() -> Popup {
        Popup::confirm(
            "Delete \"debian-12\" and all of its disks?",
            ConfirmAction::DeleteVm("debian-12".into()),
        )
    }

    fn quitting() -> Popup {
        Popup::quitting("deleting debian-12…")
    }

    fn long_message() -> Popup {
        let text: String = (1..=40)
            .map(|i| format!("qemu-system-x86_64: line {i} of what QEMU said\n"))
            .collect();
        Popup::message("Error", text.trim_end())
    }

    /// The hints bar's text for a popup: `key desc  key desc`.
    fn bar_text(popup: &Popup) -> String {
        key_hints(popup)
            .iter()
            .map(|(k, d)| format!("{k} {d}"))
            .collect::<Vec<_>>()
            .join("  ")
    }

    #[test]
    fn every_popup_pads_its_body_one_cell_each_side() {
        for (w, h) in SIZES {
            let wide = Popup::confirm(
                format!("Delete {:?} and all of its disks?", "x".repeat(150)),
                ConfirmAction::DeleteVm("x".into()),
            );
            for mut popup in [confirm(), wide, quitting(), long_message(), Popup::help()] {
                let shot = Shot::take(&mut popup, w, h);
                for row in shot.body() {
                    let cells: Vec<char> = row.chars().collect();
                    assert!(
                        cells[0] == ' ' && cells[cells.len() - 1] == ' ',
                        "{w}x{h}: row not padded: {row:?}\n{}",
                        shot.dump()
                    );
                }
            }
        }
    }

    #[test]
    fn confirm_text_is_padded_and_its_keys_are_in_the_footer_only() {
        for (w, h) in SIZES {
            let shot = Shot::take(&mut confirm(), w, h);
            let body = shot.body();
            assert!(
                body.iter()
                    .any(|r| r.starts_with(" Delete \"debian-12\" and all of its disks?")),
                "{w}x{h}:\n{}",
                shot.dump()
            );
            assert!(
                !body.iter().any(|r| r.contains("confirm")),
                "{w}x{h}: key hints in the body:\n{}",
                shot.dump()
            );
            // A blank row above and below the question.
            assert_eq!(body.first().map(|r| r.trim()), Some(""));
            assert_eq!(body.last().map(|r| r.trim()), Some(""));
        }
    }

    #[test]
    fn footers_use_the_hints_bar_words() {
        assert_eq!(bar_text(&confirm()), "y confirm  any other key cancel");
        assert_eq!(bar_text(&long_message()), "j/k scroll  Esc/Enter close");
        assert_eq!(bar_text(&Popup::help()), "j/k scroll  any other key close");
        assert_eq!(bar_text(&quitting()), "Esc stay  Ctrl-c quit now");
        for (w, h) in SIZES {
            for mut popup in [confirm(), quitting(), long_message(), Popup::help()] {
                let want = format!(" {} ╯", bar_text(&popup));
                let shot = Shot::take(&mut popup, w, h);
                let border = shot.bottom_border();
                assert!(
                    border.ends_with(&want) && !border.contains(':'),
                    "{w}x{h}: footer {border:?}, want it to end with {want:?}"
                );
            }
        }
        // Still whole on the narrowest dashboard.
        let mut popup = confirm();
        let want = format!(" {} ╯", bar_text(&popup));
        assert!(Shot::take(&mut popup, 40, 12)
            .bottom_border()
            .ends_with(&want));
    }

    /// The Quitting popup says what it waits for, in a box sized like a
    /// confirmation, styled as a pane rather than as an error.
    #[test]
    fn quitting_says_what_it_waits_for() {
        let theme = Theme::default();
        for (w, h) in SIZES.into_iter().chain([(40, 12)]) {
            let mut popup = quitting();
            let shot = Shot::take(&mut popup, w, h);
            let body: Vec<String> = shot.body().iter().map(|r| r.trim().to_string()).collect();
            let want: &[&str] = if w >= 80 {
                &[
                    "",
                    "Still deleting debian-12…",
                    "",
                    "Ostrich quits as soon as that is done.",
                    "",
                ]
            } else {
                &[
                    "",
                    "Still deleting debian-12…",
                    "",
                    "Ostrich quits as soon as",
                    "that is done.",
                    "",
                ]
            };
            assert_eq!(body, want, "{w}x{h}:\n{}", shot.dump());
            assert!(
                shot.rows[shot.top][shot.left..]
                    .concat()
                    .starts_with("╭ Quitting "),
                "{}",
                shot.dump()
            );
            let mut t = Terminal::new(TestBackend::new(w, h)).expect("terminal");
            t.draw(|f| render(&mut popup, f, &theme)).expect("draw");
            let corner = &t.backend().buffer()[(shot.left as u16, shot.top as u16)];
            assert_eq!(corner.fg, theme.border_focused.fg.unwrap_or_default());
            assert_ne!(corner.fg, theme.error.fg.unwrap_or_default());
        }
    }

    #[test]
    fn quitting_keys_stay_or_keep_waiting() {
        for k in [key(KeyCode::Esc), key(KeyCode::Enter)] {
            assert!(
                matches!(handle_key(quitting(), k), KeyOutcome::Stay),
                "{k:?}"
            );
        }
        for k in [ch('q'), ch('j'), ch('y'), key(KeyCode::Tab)] {
            assert!(
                matches!(
                    handle_key(quitting(), k),
                    KeyOutcome::Keep(Popup::Quitting { label }) if label == "deleting debian-12…"
                ),
                "{k:?}"
            );
        }
    }

    #[test]
    fn message_scrolls_and_stops_at_the_end() {
        let mut popup = long_message();
        let shot = Shot::take(&mut popup, 80, 24);
        assert!(shot.body()[0].contains("line 1 of"), "{}", shot.dump());
        assert!(shot.has_scrollbar(), "{}", shot.dump());
        for _ in 0..100 {
            popup = match handle_key(popup, ch('j')) {
                KeyOutcome::Keep(p) => p,
                other => panic!("j closed the message: {other:?}"),
            };
        }
        let shot = Shot::take(&mut popup, 80, 24);
        let last = shot.body().last().cloned().unwrap_or_default();
        assert!(last.contains("line 40 of"), "{}", shot.dump());
        // The offset was pulled back to the last page: one k shows one more
        // line at the top straight away.
        let top_before = shot.body()[0].clone();
        popup = match handle_key(popup, ch('k')) {
            KeyOutcome::Keep(p) => p,
            other => panic!("k closed the message: {other:?}"),
        };
        let shot = Shot::take(&mut popup, 80, 24);
        assert_ne!(shot.body()[0], top_before, "{}", shot.dump());
    }

    #[test]
    fn help_uses_most_of_a_small_screen_and_wraps_under_the_description_column() {
        // Tall enough that nothing scrolls: the wrapping only depends on the width.
        let mut popup = Popup::help();
        let shot = Shot::take(&mut popup, 80, 60);
        assert_eq!(shot.width(), 76, "{}", shot.dump());
        let key_w = help_key_width();
        // Margin, key column, two spaces.
        let desc_col = 1 + key_w + 2;
        let body = shot.body();
        for (_, keys) in HELP {
            for (k, d) in keys.iter() {
                let y = body
                    .iter()
                    .position(|r| r[1..].starts_with(&format!("{k}  ")))
                    .unwrap_or_else(|| panic!("no row for {k}:\n{}", shot.dump()));
                let first: String = body[y].chars().skip(desc_col).collect();
                let mut text = first.trim_end().to_string();
                for row in &body[y + 1..] {
                    let (keys_part, desc_part): (String, String) = (
                        row.chars().take(desc_col).collect(),
                        row.chars().skip(desc_col).collect(),
                    );
                    if !keys_part.trim().is_empty() || desc_part.trim().is_empty() {
                        break;
                    }
                    // Continuation rows start right in the description column.
                    assert!(!desc_part.starts_with(' '), "{row:?}");
                    text.push(' ');
                    text.push_str(desc_part.trim_end());
                }
                assert_eq!(&text, d, "{k}:\n{}", shot.dump());
            }
        }
        // Some descriptions did need a second row at this width.
        assert!(body.len() > HELP.iter().map(|(_, k)| k.len()).sum::<usize>() + 2);
    }

    #[test]
    fn help_fits_a_large_screen_one_line_per_key() {
        let (w, h) = SIZES[1];
        let mut popup = Popup::help();
        let shot = Shot::take(&mut popup, w, h);
        let body = shot.body();
        let key_w = help_key_width();
        for (_, keys) in HELP {
            for (k, d) in keys.iter() {
                let want = format!(" {}{d}", pad_right(k, key_w + 2));
                assert!(
                    body.iter().any(|r| r.trim_end() == want),
                    "no row {want:?}:\n{}",
                    shot.dump()
                );
            }
        }
        assert!(!shot.has_scrollbar(), "{}", shot.dump());
        // Nothing to scroll: j changes nothing on screen.
        let KeyOutcome::Keep(mut popup) = handle_key(popup, ch('j')) else {
            panic!("j closed the help");
        };
        let again = Shot::take(&mut popup, w, h);
        assert_eq!(again.body(), body);
        assert!(matches!(popup, Popup::Help { scroll: 0 }));
    }

    #[test]
    fn help_scrolls_with_j_and_k_when_taller_than_the_screen() {
        let (w, h) = SIZES[0];
        let mut popup = Popup::help();
        let shot = Shot::take(&mut popup, w, h);
        let first = shot.body()[0].clone();
        assert!(first.starts_with(" j/k ↑/↓"), "{}", shot.dump());
        assert!(
            !shot.dump().contains("launch a VNC viewer"),
            "the help fits at {w}x{h}, nothing to scroll:\n{}",
            shot.dump()
        );
        assert!(shot.has_scrollbar(), "{}", shot.dump());
        // It stays clear of the status and hints bars.
        assert!(shot.bottom < h as usize - 1);

        for _ in 0..50 {
            popup = match handle_key(popup, key(KeyCode::Down)) {
                KeyOutcome::Keep(p) => p,
                other => panic!("↓ closed the help: {other:?}"),
            };
        }
        let shot = Shot::take(&mut popup, w, h);
        let body = shot.body();
        assert!(
            body.last()
                .is_some_and(|r| r.contains("launch a VNC viewer")),
            "{}",
            shot.dump()
        );
        assert_ne!(body[0], first);
        let Popup::Help { scroll } = popup else {
            unreachable!()
        };
        assert!(scroll > 0 && scroll < 50, "scroll {scroll} not pulled back");

        // One k moves at once, and k at the top stays there.
        popup = match handle_key(popup, ch('k')) {
            KeyOutcome::Keep(p) => p,
            other => panic!("k closed the help: {other:?}"),
        };
        assert!(matches!(popup, Popup::Help { scroll: s } if s == scroll - 1));
        for _ in 0..50 {
            popup = match handle_key(popup, key(KeyCode::Up)) {
                KeyOutcome::Keep(p) => p,
                other => panic!("↑ closed the help: {other:?}"),
            };
        }
        let shot = Shot::take(&mut popup, w, h);
        assert_eq!(shot.body()[0], first);
    }

    /// On a narrow screen the help is no wider than the screen and wrapped
    /// for the width it is drawn at: no description or heading loses a word
    /// or a bracket.
    #[test]
    fn help_is_whole_on_narrow_screens() {
        for w in [30u16, 36, 40, 44] {
            let mut popup = Popup::help();
            let shot = Shot::take(&mut popup, w, 100);
            assert!(shot.width() <= usize::from(w), "{w}:\n{}", shot.dump());
            assert!(!shot.has_scrollbar(), "{w}:\n{}", shot.dump());
            let squeezed = shot
                .body()
                .join(" ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            for (heading, keys) in HELP {
                assert!(
                    squeezed.contains(heading),
                    "{w}: heading {heading:?} cut:\n{}",
                    shot.dump()
                );
                for (k, d) in keys.iter() {
                    assert!(
                        squeezed.contains(&format!("{k} {d}")),
                        "{w}: {k} {d:?} cut:\n{}",
                        shot.dump()
                    );
                }
            }
        }
    }

    #[test]
    fn help_says_where_the_vm_keys_work() {
        let (heading, keys) = HELP[1];
        assert_eq!(heading, "The selected VM (VM list and console)");
        let listed: Vec<&str> = keys.iter().map(|(k, _)| *k).collect();
        assert_eq!(listed, ["s / x", "e", "u", "i", "t", "c", "v"]);
        for (w, h) in SIZES {
            let mut popup = Popup::help();
            let shot = Shot::take(&mut popup, w, h);
            // On a row of its own at the left margin.
            assert!(
                shot.body()
                    .iter()
                    .any(|r| r.starts_with(&format!(" {heading}")) && r.trim() == heading),
                "{w}x{h}:\n{}",
                shot.dump()
            );
        }
    }

    #[test]
    fn help_keys_scroll_or_close() {
        for k in [ch('j'), key(KeyCode::Down)] {
            assert!(matches!(
                handle_key(Popup::Help { scroll: 2 }, k),
                KeyOutcome::Keep(Popup::Help { scroll: 3 })
            ));
        }
        for k in [ch('k'), key(KeyCode::Up)] {
            assert!(matches!(
                handle_key(Popup::Help { scroll: 2 }, k),
                KeyOutcome::Keep(Popup::Help { scroll: 1 })
            ));
            assert!(matches!(
                handle_key(Popup::help(), k),
                KeyOutcome::Keep(Popup::Help { scroll: 0 })
            ));
        }
        for k in [
            key(KeyCode::Esc),
            key(KeyCode::Enter),
            ch('q'),
            ch('?'),
            ch('s'),
            key(KeyCode::Tab),
            ctrl('d'),
        ] {
            assert!(
                matches!(handle_key(Popup::help(), k), KeyOutcome::Close),
                "{k:?} did not close the help"
            );
        }
    }

    #[test]
    fn confirm_and_message_keys() {
        for k in [ch('y'), ch('Y')] {
            assert!(matches!(
                handle_key(confirm(), k),
                KeyOutcome::Confirm(ConfirmAction::DeleteVm(n)) if n == "debian-12"
            ));
        }
        for k in [ch('n'), key(KeyCode::Esc), key(KeyCode::Enter), ch('j')] {
            assert!(matches!(handle_key(confirm(), k), KeyOutcome::Close));
        }
        for k in [key(KeyCode::Esc), key(KeyCode::Enter), ch('q')] {
            assert!(matches!(handle_key(long_message(), k), KeyOutcome::Close));
        }
        assert!(matches!(
            handle_key(long_message(), ch('j')),
            KeyOutcome::Keep(Popup::Message { scroll: 1, .. })
        ));
        // Any other key leaves the message as it was.
        assert!(matches!(
            handle_key(long_message(), ch('x')),
            KeyOutcome::Keep(Popup::Message { scroll: 0, .. })
        ));
    }
}
