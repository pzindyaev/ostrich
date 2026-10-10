//! The selection dialog behind every ISO input: the images used before, one
//! of which can be picked, and a last row to type the path of a new one. An
//! optional first row stands for no image. A pick is validated here — the
//! path must name a readable file — so the caller only ever gets an absolute
//! path to a file that is there.

use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context as _, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;
use unicode_width::UnicodeWidthStr;

use super::super::theme::Theme;
use super::super::widgets::{
    human_size, pad_right, truncate, truncate_left, wrap_text, Cursor, TextInput,
};
use crate::config;
use crate::vm::{image_state_of, ImageState, Manager, UsbImage};

/// The narrowest a path column gets; beyond that the state column is pushed
/// off the right edge rather than the file name out of the path.
pub const MIN_PATH_WIDTH: usize = 20;

/// The `▸ ` / `  ` marker in front of every row.
const MARKER_WIDTH: usize = 2;
/// The label of the input row.
const INPUT_LABEL: &str = "New path  ";

/// One image the picker offers: a path used before, with what the host shows
/// for it now and the VMs that have it in their config.
#[derive(Debug, Clone, Default)]
pub struct IsoEntry {
    pub path: String,
    pub state: ImageState,
    /// VM names, in listing order; empty when no VM has it.
    pub used_by: Vec<String>,
}

/// `~/.config/ostrich/config.json` under an explicit home directory: the
/// app config the picker remembers and forgets images in. The dialogs get
/// the home from their context instead of reading `$HOME` themselves, which
/// also lets the tests keep their remembered images to a scratch home.
pub fn config_path_in(home: &Path) -> PathBuf {
    home.join(".config").join("ostrich").join("config.json")
}

/// Lists the images to offer: the paths remembered in the app config,
/// newest first, then any other image a VM under `storage` has as its boot
/// ISO or a USB drive. Each is checked on the host. (Blocking: call it from
/// a task or accept the stat calls.)
pub fn iso_entries(storage: &Path) -> Vec<IsoEntry> {
    iso_entries_at(storage, &config::config_path())
}

/// [`iso_entries`] with the app config read from an explicit path.
pub fn iso_entries_at(storage: &Path, config_path: &Path) -> Vec<IsoEntry> {
    fn add(entries: &mut Vec<IsoEntry>, path: &str, vm_name: &str) {
        if path.is_empty() {
            return;
        }
        let i = match entries.iter().position(|e| e.path == path) {
            Some(i) => i,
            None => {
                entries.push(IsoEntry {
                    path: path.to_string(),
                    ..IsoEntry::default()
                });
                entries.len() - 1
            }
        };
        if !vm_name.is_empty() && !entries[i].used_by.iter().any(|v| v == vm_name) {
            entries[i].used_by.push(vm_name.to_string());
        }
    }
    let mut entries = Vec::new();
    for p in config::recent_isos_at(config_path) {
        add(&mut entries, &p, "");
    }
    for cfg in Manager::new(storage).list().unwrap_or_default() {
        add(&mut entries, &cfg.cdrom_path, &cfg.name);
        for img in &cfg.usb_images {
            add(&mut entries, &img.path, &cfg.name);
        }
    }
    for e in &mut entries {
        e.state = image_state_of(&e.path);
    }
    entries
}

/// What the last key did to the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickOutcome {
    Nothing,
    /// A pick: the absolute path, or `""` for the none row.
    Picked(String),
    Cancelled,
}

/// The ISO dialog. Rows: `[none]`, the entries, the `New path` input. The
/// cursor rules, keys (Enter pick, ↑/↓ Tab/Shift-Tab move, j/k/g/G off the
/// input row, d forget, Esc cancel) and the validation of a typed path are
/// those of internal/tui/isopicker.go.
#[derive(Debug, Clone)]
pub struct IsoPicker {
    /// Label of the "no image" row; `""` leaves the row out.
    none: String,
    entries: Vec<IsoEntry>,
    /// Over the rows: `0..=input_row()`, with the scroll offset.
    cursor: Cursor,
    input: TextInput,
    /// A validation or forget error, shown under the rows.
    err: String,
    /// The width of the last render, which [`IsoPicker::height`] wraps the
    /// error to; 0 before the first.
    last_width: u16,
}

impl IsoPicker {
    /// A picker with an optional none row (its label, `""` leaves it out),
    /// the cursor on `current`'s row: the none row when `current` is `""`
    /// and there is one, else the first entry, else the input row. A
    /// `current` path that is not among the entries is put in the input
    /// instead, with the cursor on it.
    pub fn new(none_label: &str, current: &str, entries: Vec<IsoEntry>) -> Self {
        let input = TextInput::new()
            .with_placeholder("/path/to/image.iso")
            .with_char_limit(1024);
        let mut p = IsoPicker {
            none: none_label.to_string(),
            entries,
            cursor: Cursor::default(),
            input,
            err: String::new(),
            last_width: 0,
        };
        let mut start = p.input_row();
        if current.is_empty() {
            if !p.none.is_empty() {
                start = 0;
            } else if !p.entries.is_empty() {
                start = p.first_entry_row();
            }
        } else {
            p.input.set_value(current);
            if let Some(i) = p.entries.iter().position(|e| e.path == current) {
                start = p.first_entry_row() + i;
                p.input.set_value("");
            }
        }
        p.move_to(start as isize);
        p
    }

    // --- rows: [none] entries... input ---

    fn first_entry_row(&self) -> usize {
        usize::from(!self.none.is_empty())
    }

    fn input_row(&self) -> usize {
        self.first_entry_row() + self.entries.len()
    }

    fn row_count(&self) -> usize {
        self.input_row() + 1
    }

    /// Whether the cursor is on the none row.
    pub fn on_none(&self) -> bool {
        !self.none.is_empty() && self.cursor.index == 0
    }

    /// Whether the cursor is on the input row.
    pub fn on_input(&self) -> bool {
        self.cursor.index == self.input_row()
    }

    /// The entry under the cursor, if it is on one.
    pub fn entry(&self) -> Option<&IsoEntry> {
        self.cursor
            .index
            .checked_sub(self.first_entry_row())
            .and_then(|i| self.entries.get(i))
    }

    /// The entries on offer, in row order.
    pub fn entries(&self) -> &[IsoEntry] {
        &self.entries
    }

    /// The cursor row (the none row is 0 when there is one).
    pub fn cursor(&self) -> usize {
        self.cursor.index
    }

    /// The error shown under the rows, `""` when none.
    pub fn error(&self) -> &str {
        &self.err
    }

    /// The text in the path input.
    pub fn input_value(&self) -> String {
        self.input.value()
    }

    /// Whether the path input has the focus (the cursor is on it).
    pub fn input_focused(&self) -> bool {
        self.input.focused
    }

    /// Replaces the text in the path input.
    pub fn set_input(&mut self, value: &str) {
        self.input.set_value(value);
    }

    /// Whether the input row holds a path that has not been picked.
    pub fn typed(&self) -> bool {
        self.on_input() && !self.input.trimmed().is_empty()
    }

    /// Sets the error shown under the rows (e.g. `… is already attached`).
    pub fn set_error(&mut self, err: impl Into<String>) {
        self.err = err.into();
    }

    /// Puts the cursor on a row, within bounds, giving the input the focus
    /// when the cursor lands on it and taking it away otherwise. Every move
    /// clears the error.
    fn move_to(&mut self, row: isize) {
        self.cursor.index = row.clamp(0, self.input_row() as isize) as usize;
        self.err.clear();
        if self.on_input() {
            self.input.focus();
        } else {
            self.input.blur();
        }
    }

    /// Handles a key; `home` expands `~` and locates the app config.
    pub fn handle_key(&mut self, key: KeyEvent, home: &Path) -> PickOutcome {
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let row = self.cursor.index as isize;
        match key.code {
            KeyCode::Esc => return PickOutcome::Cancelled,
            KeyCode::Enter => return self.pick(home),
            KeyCode::Up | KeyCode::BackTab => {
                self.move_to(row - 1);
                return PickOutcome::Nothing;
            }
            KeyCode::Down | KeyCode::Tab => {
                self.move_to(row + 1);
                return PickOutcome::Nothing;
            }
            _ => {}
        }
        if self.on_input() {
            // Letters type into the path.
            self.input.handle_key(key);
            return PickOutcome::Nothing;
        }
        match key.code {
            KeyCode::Char('k') if plain => self.move_to(row - 1),
            KeyCode::Char('j') if plain => self.move_to(row + 1),
            KeyCode::Char('g') if plain => self.move_to(0),
            KeyCode::Char('G') if plain => self.move_to(self.input_row() as isize),
            KeyCode::Char('d') if plain => self.forget(home),
            _ => {}
        }
        PickOutcome::Nothing
    }

    /// Resolves the row under the cursor. A path that does not name a
    /// readable file keeps the dialog open with the reason.
    fn pick(&mut self, home: &Path) -> PickOutcome {
        if self.on_none() {
            return PickOutcome::Picked(String::new());
        }
        let raw = match self.entry() {
            Some(e) => e.path.clone(),
            None => self.input.value(),
        };
        match resolve_image_path(&raw, home) {
            Ok(img) => {
                self.err.clear();
                PickOutcome::Picked(img.path)
            }
            Err(err) => {
                self.err = format!("{err:#}");
                PickOutcome::Nothing
            }
        }
    }

    /// Drops the entry under the cursor from the remembered list. One a VM
    /// still has in its config stays: it would be back on the next open.
    fn forget(&mut self, home: &Path) {
        let Some(e) = self.entry() else {
            return;
        };
        if !e.used_by.is_empty() {
            self.err = format!(
                "{} stays listed while a VM has it ({})",
                e.path,
                e.used_by.join(", ")
            );
            return;
        }
        if let Err(err) = config::forget_iso_at(&config_path_in(home), &e.path) {
            self.err = format!("forget {}: {err:#}", e.path);
            return;
        }
        let i = self.cursor.index - self.first_entry_row();
        self.entries.remove(i);
        self.move_to(self.cursor.index as isize); // the next entry, or the input row
    }

    /// Pasted text goes into the path input when the cursor is on it.
    pub fn handle_paste(&mut self, text: &str) {
        if self.on_input() {
            self.input.insert_str(text);
        }
    }

    /// The picker's own key hints, with what Enter does in the caller's
    /// words (`pick`, `insert`, `attach`, `pick and go on`); the caller adds
    /// what Esc and Tab do.
    pub fn key_hints(&self, action: &str) -> Vec<(String, String)> {
        vec![
            ("Enter".to_string(), action.to_string()),
            ("↑/↓".to_string(), "move".to_string()),
            ("d".to_string(), "forget".to_string()),
        ]
    }

    /// The height the rows (plus a blank line and the error, if any) need,
    /// with the error wrapped to the width of the last render.
    pub fn height(&self) -> u16 {
        self.height_for(self.last_width)
    }

    /// The height the rows (plus a blank line and the error, if any) need
    /// in an area `width` cells wide; 0 counts the error unwrapped.
    pub fn height_for(&self, width: u16) -> u16 {
        let err = if self.err.is_empty() {
            0
        } else if width == 0 {
            1 + self.err.lines().count()
        } else {
            1 + wrapped_error(&self.err, width as usize).len()
        };
        (self.row_count() + err).min(u16::MAX as usize) as u16
    }

    /// Draws the rows into `area` (no border of its own): the none row, the
    /// entries with the path column cut on the left to the room there is,
    /// their state and who uses them, the `New path` input, then the error.
    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        self.last_width = area.width;
        let width = area.width as usize;
        let err_lines = error_lines(&wrapped_error(&self.err, width).join("\n"), theme);
        let err_h = if err_lines.is_empty() {
            0
        } else {
            1 + err_lines.len()
        };
        let rows_h = (area.height as usize).saturating_sub(err_h).max(1);
        let range = self.cursor.window(self.row_count(), rows_h);

        // The path column: as wide as the longest path, as long as the
        // state and the users still fit on the right.
        let tail_w = self
            .entries
            .iter()
            .map(|e| image_state_text(&e.state).width() + used_by_text(&e.used_by).width())
            .max()
            .unwrap_or(0);
        let longest = self
            .entries
            .iter()
            .map(|e| e.path.chars().count())
            .max()
            .unwrap_or(0);
        let path_w = width
            .saturating_sub(MARKER_WIDTH + 2 + tail_w)
            .max(MIN_PATH_WIDTH)
            .min(longest.max(MIN_PATH_WIDTH));

        let mut y = area.y;
        for row in range {
            let focused = row == self.cursor.index;
            let marker = if focused {
                Span::styled("▸ ", theme.label)
            } else {
                Span::raw("  ")
            };
            let line_area = Rect::new(area.x, y, area.width, 1);
            if !self.none.is_empty() && row == 0 {
                let style = if focused { theme.label } else { theme.normal };
                frame.render_widget(
                    Line::from(vec![marker, Span::styled(self.none.clone(), style)]),
                    line_area,
                );
            } else if row == self.input_row() {
                let style = if focused { theme.label } else { theme.help };
                let label = Span::styled(INPUT_LABEL, style);
                if focused {
                    frame.render_widget(Line::from(vec![marker, label]), line_area);
                    let used = (MARKER_WIDTH + INPUT_LABEL.len()) as u16;
                    let input_area =
                        Rect::new(area.x + used, y, area.width.saturating_sub(used), 1);
                    self.input.render(frame, input_area, theme);
                } else {
                    frame.render_widget(
                        Line::from(vec![marker, label, self.input.as_span(theme)]),
                        line_area,
                    );
                }
            } else if let Some(e) = self.entries.get(row - self.first_entry_row()) {
                let col = pad_right(&truncate_left(&e.path, path_w), path_w);
                let style = if focused { theme.label } else { theme.normal };
                let mut spans = vec![marker, Span::styled(col, style), Span::raw("  ")];
                // What is right of the path, shortened with an ellipsis
                // instead of cut at the edge: the users go first, the state
                // only when there is not even room for it.
                let room = width.saturating_sub(MARKER_WIDTH + path_w + 2);
                let mut state = image_state_span(&e.state, theme);
                if state.width() > room {
                    state.content = truncate(&state.content, room).into();
                }
                let used_room = room.saturating_sub(state.width());
                spans.push(state);
                let used_by = used_by_text(&e.used_by);
                if used_by.width() <= used_room {
                    spans.push(Span::styled(used_by, theme.help));
                } else if used_room > USED_BY_LEAD.width() {
                    // At least `  in use by …`.
                    spans.push(Span::styled(truncate(&used_by, used_room), theme.help));
                }
                frame.render_widget(Line::from(spans), line_area);
            }
            y += 1;
        }
        if !err_lines.is_empty() {
            y += 1; // a blank line before the error
            for line in err_lines {
                if y >= area.bottom() {
                    break;
                }
                frame.render_widget(line, Rect::new(area.x, y, area.width, 1));
                y += 1;
            }
        }
    }
}

/// What [`used_by_text`] starts with.
const USED_BY_LEAD: &str = "  in use by ";

/// `  in use by a, b`, or `""` when no VM has the image.
fn used_by_text(used_by: &[String]) -> String {
    if used_by.is_empty() {
        String::new()
    } else {
        format!("{USED_BY_LEAD}{}", used_by.join(", "))
    }
}

/// An error's lines, each wrapped on its own to fit `width` cells behind
/// the `✗ ` / two-space lead [`error_lines`] puts in front; a long path is
/// broken across lines rather than cut, so its file name stays on screen.
fn wrapped_error(err: &str, width: usize) -> Vec<String> {
    err.lines()
        .flat_map(|raw| wrap_text(raw, width.saturating_sub(2).max(8)))
        .collect()
}

/// Turns what the user typed into a validated image entry: a leading `~` is
/// the home directory and relative paths are made absolute (against the
/// current directory) and cleaned. Errors: `enter the path of an image file`,
/// then whatever `UsbImage::validate` says.
pub fn resolve_image_path(s: &str, home: &Path) -> Result<UsbImage> {
    let s = s.trim();
    if s.is_empty() {
        bail!("enter the path of an image file");
    }
    // `~` alone or `~/…`; `~user` is left as it is.
    let path = if s == "~" || s.starts_with("~/") {
        if home.as_os_str().is_empty() {
            // Go: os.UserHomeDir() fails with "$HOME is not defined".
            bail!("expand ~: $HOME is not defined");
        }
        home.join(s[1..].trim_start_matches('/'))
    } else {
        PathBuf::from(s)
    };
    let abs = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .context("resolve relative path")?
            .join(path)
    };
    let img = UsbImage {
        path: clean_path(&abs).to_string_lossy().into_owned(),
    };
    img.validate()?;
    Ok(img)
}

/// Lexically cleans an absolute path the way Go's `filepath.Abs` does:
/// `.` and empty components go, `..` takes the component before it along
/// (or is dropped at the root). Symlinks are left alone.
fn clean_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Prefix(_) | Component::RootDir => out.push(c.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(s) => out.push(s),
        }
    }
    out
}

/// An image's host-side state as text: `● 631 MiB` / `● present` when it is
/// there, `✗ not found` / `✗ no access` / `✗ <error>` when it is not. An
/// unchecked state (no error, no size) reads as present.
pub fn image_state_text(s: &ImageState) -> String {
    match &s.err {
        Some(e) if e.contains("not found") => "✗ not found".to_string(),
        Some(e) if e.contains("no read access") => "✗ no access".to_string(),
        Some(e) => format!("✗ {e}"),
        None if s.size > 0 => format!("● {}", human_size(s.size)),
        None => "● present".to_string(),
    }
}

/// [`image_state_text`] styled: success when the image is there, error otherwise.
pub fn image_state_span(s: &ImageState, theme: &Theme) -> Span<'static> {
    let style = if s.err.is_some() {
        theme.error
    } else {
        theme.success
    };
    Span::styled(image_state_text(s), style)
}

/// `prefix` followed by `text` in `style`, word-wrapped to `width` cells;
/// the lines after the first are indented by two cells (the forms' banner
/// layout). The first line holds what fits after the prefix, the others
/// what fits after the indent.
pub fn flow(
    prefix: Vec<Span<'static>>,
    text: &str,
    style: Style,
    width: usize,
) -> Vec<Line<'static>> {
    let prefix_w: usize = prefix.iter().map(Span::width).sum();
    let rest_w = width.saturating_sub(2).max(8);
    let mut out = Vec::new();
    let mut prefix = Some(prefix);
    for raw in text.split('\n') {
        let mut rest = raw.to_string();
        if let Some(mut spans) = prefix.take() {
            let pieces = wrap_text(raw, width.saturating_sub(prefix_w).max(8));
            let first = pieces.first().cloned().unwrap_or_default();
            // What the first line did not take, as written (a word broken
            // at the edge goes on without a space).
            rest = match raw.strip_prefix(first.as_str()) {
                Some(r) => r.strip_prefix(' ').unwrap_or(r).to_string(),
                None => pieces[1..].join(" "),
            };
            spans.push(Span::styled(first, style));
            out.push(Line::from(spans));
            if rest.is_empty() {
                continue;
            }
        }
        for piece in wrap_text(&rest, rest_w) {
            out.push(Line::styled(format!("  {piece}"), style));
        }
    }
    out
}

/// An error as lines in the error style: `✗ ` before the first, two spaces
/// of indent on the rest. Empty for an empty error.
pub fn error_lines(err: &str, theme: &Theme) -> Vec<Line<'static>> {
    err.lines()
        .enumerate()
        .map(|(i, l)| {
            Line::styled(
                format!("{}{l}", if i == 0 { "✗ " } else { "  " }),
                theme.error,
            )
        })
        .collect()
}

/// The picker's tests, and the fixture helpers the dialog tests share.
#[cfg(test)]
pub(crate) mod tests {
    use std::fs;

    use ratatui::backend::TestBackend;

    use super::*;
    use crate::config::AppConfig;
    use crate::tui::testutil::*;
    use crate::vm::{save_config, vm_dir, VmConfig};

    /// A zero-filled file standing in for an ISO; returns its path as text.
    pub(crate) fn write_image(path: &Path, size: usize) -> String {
        fs::write(path, vec![0u8; size]).expect("write image");
        path.to_string_lossy().into_owned()
    }

    /// Writes `cfg` as a VM under `storage`.
    pub(crate) fn save_vm(storage: &Path, cfg: &VmConfig) {
        fs::create_dir_all(vm_dir(storage, &cfg.name)).expect("vm dir");
        save_config(storage, cfg).expect("save config");
    }

    /// Points the app config at the harness's scratch home with the storage
    /// path set, so the images the tests use are remembered there and
    /// nowhere else. Returns the config path.
    pub(crate) fn isolate_config(h: &Harness) -> PathBuf {
        let path = config_path_in(&h.home);
        let cfg = AppConfig {
            vm_storage_path: h.mgr.storage().to_string_lossy().into_owned(),
            recent_isos: vec![],
        };
        config::save_at(&path, &cfg).expect("save app config");
        path
    }

    fn render_picker(p: &mut IsoPicker, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|f| p.render(f, f.area(), &Theme::default()))
            .expect("draw");
        buffer_lines(terminal.backend().buffer())
    }

    #[test]
    fn iso_entries_lists_remembered_then_configured() {
        let h = Harness::new();
        let cfg_path = isolate_config(&h);
        let isos = h.dir.path().join("isos");
        fs::create_dir_all(&isos).unwrap();
        let a = isos.join("a.iso").to_string_lossy().into_owned(); // remembered, gone from the host
        let b = write_image(&isos.join("b.iso"), 1 << 20);
        let c = write_image(&isos.join("c.iso"), 1 << 20);
        for p in [&b, &a] {
            config::remember_iso_at(&cfg_path, p).unwrap();
        }
        let storage = h.mgr.storage();
        save_vm(
            storage,
            &VmConfig {
                name: "x".into(),
                cpu: 1,
                ram: 64,
                cdrom_path: b.clone(),
                usb_images: vec![UsbImage { path: c.clone() }],
                ..VmConfig::default()
            },
        );
        save_vm(
            storage,
            &VmConfig {
                name: "y".into(),
                cpu: 1,
                ram: 64,
                cdrom_path: c.clone(),
                usb_images: vec![UsbImage { path: c.clone() }],
                ..VmConfig::default()
            },
        );

        let got = iso_entries_at(storage, &cfg_path);
        let paths: Vec<&str> = got.iter().map(|e| e.path.as_str()).collect();
        // Remembered ones first, newest at the top; then what only VMs have.
        assert_eq!(paths, [a.as_str(), b.as_str(), c.as_str()]);
        assert!(got[0].used_by.is_empty(), "{:?}", got[0].used_by);
        assert_eq!(got[1].used_by, ["x"]);
        assert_eq!(got[2].used_by, ["x", "y"]);
        assert!(got[0].state.err.is_some(), "{:?}", got[0].state);
        assert_eq!(got[1].state.size, 1 << 20);
        assert!(got[2].state.err.is_none(), "{:?}", got[2].state);
        // Without an app config only the VMs' images are listed.
        let without = iso_entries_at(storage, &h.dir.path().join("nope.json"));
        assert_eq!(without.len(), 2);
        assert_eq!(without[0].path, b);
    }

    #[test]
    fn picker_keys_and_picks() {
        let h = Harness::new();
        let cfg_path = isolate_config(&h);
        let home = h.home.clone();
        let isos = h.dir.path().join("isos");
        fs::create_dir_all(&isos).unwrap();
        let present = write_image(&isos.join("present.iso"), 1 << 20);
        let gone = isos.join("gone.iso").to_string_lossy().into_owned();
        let home_iso = write_image(&home.join("home.iso"), 1 << 20);
        for p in [&gone, &present] {
            config::remember_iso_at(&cfg_path, p).unwrap();
        }
        let entries = iso_entries_at(h.mgr.storage(), &cfg_path); // present, gone
        assert_eq!(entries.len(), 2);

        let mut p = IsoPicker::new("(none) — no boot ISO", "", entries.clone());
        assert!(p.on_none(), "cursor = {}, want the none row", p.cursor());
        assert_eq!(p.height(), 4);
        let screen = render_picker(&mut p, 100, 10);
        for want in [
            "▸ (none) — no boot ISO",
            "present.iso",
            "● 1 MiB",
            "gone.iso",
            "✗ not found",
            "New path",
        ] {
            assert!(
                screen_contains(&screen, want),
                "screen lacks {want:?}:\n{}",
                screen.join("\n")
            );
        }
        // The state column lines up across the entries.
        let col = |needle: &str| screen.iter().find_map(|l| l.find(needle)).unwrap();
        assert_eq!(col("● 1 MiB"), col("✗ not found"));

        // Enter on the none row picks nothing; on an entry, its path.
        assert_eq!(
            p.handle_key(key(KeyCode::Enter), &home),
            PickOutcome::Picked(String::new())
        );
        p.handle_key(ch('j'), &home);
        assert_eq!(
            p.handle_key(key(KeyCode::Enter), &home),
            PickOutcome::Picked(present.clone())
        );
        // A file that is gone is refused with the reason; the dialog stays.
        p.handle_key(key(KeyCode::Down), &home);
        assert_eq!(
            p.handle_key(key(KeyCode::Enter), &home),
            PickOutcome::Nothing
        );
        assert!(p.error().contains("not found"), "err = {:?}", p.error());
        let screen = render_picker(&mut p, 100, 10);
        assert!(
            screen_contains(&screen, &format!("✗ image not found: {gone}")),
            "{}",
            screen.join("\n")
        );
        assert_eq!(p.height_for(u16::MAX), 6);
        assert_eq!(p.height(), p.height_for(100), "wrapped to the last render");
        // G lands on the input row, clearing the error; letters type there.
        p.handle_key(ch('G'), &home);
        assert!(
            p.on_input() && p.error().is_empty() && p.input_focused(),
            "after G: {p:?}"
        );
        for c in "jkdgG".chars() {
            p.handle_key(ch(c), &home);
        }
        assert_eq!(p.input_value(), "jkdgG");
        assert!(!p.on_none());
        assert!(p.typed(), "typed() should report the waiting path");
        // Shift-Tab and Tab move too; leaving the input row blurs it.
        p.handle_key(backtab(), &home);
        assert!(
            !p.on_input() && !p.input_focused(),
            "after shift-tab: {p:?}"
        );
        assert!(!p.typed());
        p.handle_key(key(KeyCode::Tab), &home);
        assert!(p.on_input() && p.input_focused(), "after tab: {p:?}");
        // A typed path is resolved: ~ is the home directory.
        p.set_input("");
        for c in "~/home.iso".chars() {
            p.handle_key(ch(c), &home);
        }
        assert_eq!(
            p.handle_key(key(KeyCode::Enter), &home),
            PickOutcome::Picked(home_iso.clone())
        );
        // An empty one is not.
        p.set_input("  ");
        assert_eq!(
            p.handle_key(key(KeyCode::Enter), &home),
            PickOutcome::Nothing
        );
        assert_eq!(p.error(), "enter the path of an image file");
        assert_eq!(
            p.handle_key(key(KeyCode::Esc), &home),
            PickOutcome::Cancelled
        );
        // Pasting types into the input; off it, nothing happens.
        p.set_input("");
        p.handle_paste("/x");
        assert_eq!(p.input_value(), "/x");
        // Off the input row, g goes to the top.
        p.handle_key(key(KeyCode::Up), &home);
        p.handle_key(ch('g'), &home);
        assert!(p.on_none(), "after g: cursor={}", p.cursor());
        p.handle_paste("/y");
        assert_eq!(p.input_value(), "/x");

        // d forgets the entry under the cursor, here and in the config; the
        // cursor moves to what follows it.
        p.handle_key(ch('j'), &home);
        p.handle_key(ch('j'), &home);
        p.handle_key(ch('d'), &home);
        assert_eq!(p.entries().len(), 1);
        assert_eq!(p.entries()[0].path, present);
        assert!(p.on_input(), "after forget: cursor={}", p.cursor());
        assert_eq!(
            config::recent_isos_at(&cfg_path),
            std::slice::from_ref(&present)
        );
        // d on the none row does nothing.
        p.handle_key(ch('g'), &home);
        p.handle_key(ch('d'), &home);
        assert_eq!(p.entries().len(), 1);
        assert!(p.error().is_empty());
        // One a VM has stays, with a word why.
        let mut q = IsoPicker::new(
            "",
            "",
            vec![IsoEntry {
                path: present.clone(),
                used_by: vec!["x".into()],
                ..IsoEntry::default()
            }],
        );
        q.handle_key(ch('d'), &home);
        assert_eq!(q.entries().len(), 1);
        assert_eq!(
            q.error(),
            format!("{present} stays listed while a VM has it (x)")
        );
        let screen = render_picker(&mut q, 100, 10);
        assert!(
            screen_contains(&screen, "in use by x"),
            "{}",
            screen.join("\n")
        );
        assert!(
            screen_contains(&screen, "stays listed while a VM has it (x)"),
            "{}",
            screen.join("\n")
        );
        // Without a config to forget from, the reason is shown.
        let mut r = IsoPicker::new(
            "",
            "",
            vec![IsoEntry {
                path: present.clone(),
                ..IsoEntry::default()
            }],
        );
        r.handle_key(ch('d'), &h.dir.path().join("nohome"));
        assert_eq!(r.entries().len(), 1);
        assert!(
            r.error().starts_with(&format!("forget {present}: ")),
            "{:?}",
            r.error()
        );

        // The cursor starts on the current image; an unknown one is typed in.
        assert_eq!(
            IsoPicker::new("(none)", &present, entries.clone()).cursor(),
            1
        );
        let q = IsoPicker::new("(none)", "/elsewhere.iso", entries.clone());
        assert!(
            q.on_input() && q.input_value() == "/elsewhere.iso",
            "unknown current: {q:?}"
        );
        // With no none row it starts on the first entry; with nothing at
        // all, on the input.
        let q = IsoPicker::new("", "", entries.clone());
        assert!(
            q.cursor() == 0 && !q.input_focused() && !q.on_none(),
            "no none row: {q:?}"
        );
        let q = IsoPicker::new("", "", Vec::new());
        assert!(q.on_input() && q.input_focused(), "empty: {q:?}");
        assert_eq!(q.height(), 1);
        assert_eq!(
            q.key_hints("attach"),
            [
                ("Enter".to_string(), "attach".to_string()),
                ("↑/↓".to_string(), "move".to_string()),
                ("d".to_string(), "forget".to_string())
            ]
        );
    }

    #[test]
    fn picker_render_fits_the_width() {
        let long = format!(
            "/very/long/directory/name/that/goes/on/and/on/{}.iso",
            "x".repeat(40)
        );
        let entries = vec![
            IsoEntry {
                path: long.clone(),
                state: ImageState {
                    size: 3 << 20,
                    ..Default::default()
                },
                used_by: vec![],
            },
            IsoEntry {
                path: "/short.iso".into(),
                state: ImageState {
                    err: Some("no read access to image /short.iso".into()),
                    ..Default::default()
                },
                used_by: vec!["a".into(), "b".into()],
            },
        ];
        let mut p = IsoPicker::new("(none)", "/short.iso", entries);
        let screen = render_picker(&mut p, 60, 6);
        assert!(
            screen[1].starts_with("  …"),
            "long path cut on the left:\n{}",
            screen.join("\n")
        );
        assert!(
            screen[1].ends_with(".iso  ● 3 MiB"),
            "{}",
            screen.join("\n")
        );
        assert!(
            screen[2].starts_with("▸ /short.iso"),
            "{}",
            screen.join("\n")
        );
        assert!(
            screen[2].contains("✗ no access  in use by a, b"),
            "{}",
            screen.join("\n")
        );
        assert!(
            screen[3].contains("New path  /path/to/image.iso"),
            "placeholder when unfocused:\n{}",
            screen.join("\n")
        );
        assert!(screen.iter().all(|l| l.chars().count() <= 60));
        // A tiny area never panics, and the cursor row stays visible.
        let _ = render_picker(&mut p, 0, 0);
        let _ = render_picker(&mut p, 5, 1);
        let screen = render_picker(&mut p, 60, 1);
        assert!(
            screen[0].starts_with("▸ /short.iso"),
            "{}",
            screen.join("\n")
        );
        p.handle_key(ch('G'), Path::new("/"));
        let screen = render_picker(&mut p, 60, 2);
        assert!(screen[1].starts_with("▸ New path"), "{}", screen.join("\n"));
    }

    #[test]
    fn resolve_image_paths() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.iso");
        fs::write(&p, b"x").unwrap();
        let p_str = p.to_string_lossy().into_owned();
        let home = dir.path();
        assert_eq!(
            resolve_image_path(&format!("  {p_str}  "), home)
                .unwrap()
                .path,
            p_str
        );
        assert_eq!(resolve_image_path("~/a.iso", home).unwrap().path, p_str);
        assert_eq!(
            resolve_image_path(&format!("{p_str}/../a.iso"), home)
                .unwrap()
                .path,
            p_str,
            "cleaned"
        );
        assert_eq!(
            resolve_image_path("", home).unwrap_err().to_string(),
            "enter the path of an image file"
        );
        assert_eq!(
            resolve_image_path("   ", home).unwrap_err().to_string(),
            "enter the path of an image file"
        );
        let err = resolve_image_path(&dir.path().to_string_lossy(), home)
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            format!("image is a directory: {}", dir.path().display())
        );
        let err = resolve_image_path("~", home).unwrap_err().to_string();
        assert_eq!(
            err,
            format!("image is a directory: {}", dir.path().display())
        );
        let gone = dir.path().join("gone.iso");
        let err = resolve_image_path(&gone.to_string_lossy(), home)
            .unwrap_err()
            .to_string();
        assert_eq!(err, format!("image not found: {}", gone.display()));
        // A relative path is taken from the current directory (the package
        // root under cargo test, where Cargo.toml is a readable file).
        let cwd = std::env::current_dir().unwrap();
        let want = cwd.join("Cargo.toml").to_string_lossy().into_owned();
        assert_eq!(resolve_image_path("Cargo.toml", home).unwrap().path, want);
        assert_eq!(
            resolve_image_path("./src/../Cargo.toml", home)
                .unwrap()
                .path,
            want
        );
        // `~user` is not expanded.
        assert!(resolve_image_path("~nobody/a.iso", home).is_err());
    }

    #[test]
    fn clean_path_collapses_dots() {
        assert_eq!(
            clean_path(Path::new("/a/./b/../c//d/")),
            PathBuf::from("/a/c/d")
        );
        assert_eq!(clean_path(Path::new("/../a")), PathBuf::from("/a"));
        assert_eq!(clean_path(Path::new("/")), PathBuf::from("/"));
    }

    #[test]
    fn image_state_texts() {
        let theme = Theme::default();
        let st = |size: u64, err: Option<&str>| ImageState {
            path: "/x".into(),
            size,
            err: err.map(str::to_string),
        };
        assert_eq!(
            image_state_text(&st(0, Some("image not found: /x"))),
            "✗ not found"
        );
        assert_eq!(
            image_state_text(&st(0, Some("no read access to image /x"))),
            "✗ no access"
        );
        assert_eq!(
            image_state_text(&st(0, Some("image is a directory: /x"))),
            "✗ image is a directory: /x"
        );
        assert_eq!(image_state_text(&st(631 << 20, None)), "● 631 MiB");
        assert_eq!(image_state_text(&st(0, None)), "● present");
        assert_eq!(image_state_span(&st(0, None), &theme).style, theme.success);
        assert_eq!(
            image_state_span(&st(0, Some("x")), &theme).style,
            theme.error
        );
        let lines = error_lines("one\ntwo", &theme);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].to_string(), "✗ one");
        assert_eq!(lines[1].to_string(), "  two");
        assert!(error_lines("", &theme).is_empty());
    }

    #[test]
    fn picker_error_wraps_and_keeps_the_file_name() {
        let path = format!(
            "/tmp/{}/debian-12.3.0-amd64-netinst.iso",
            ["no-such-directory"; 4].join("/")
        );
        let mut p = IsoPicker::new("", "", Vec::new());
        p.set_input(&path);
        assert_eq!(
            p.handle_key(key(KeyCode::Enter), Path::new("/")),
            PickOutcome::Nothing
        );
        let msg = format!("image not found: {path}");
        assert_eq!(p.error(), msg);
        // The create form's ISO step in an 80x24 and a 120x40 terminal.
        for width in [46u16, 76] {
            let height = p.height_for(width);
            let screen = render_picker(&mut p, width, height);
            assert_eq!(p.height(), height, "height() wraps to the last render");
            assert!(
                screen.iter().all(|l| l.chars().count() <= width as usize),
                "{}",
                screen.join("\n")
            );
            // The input row, a blank line, then the whole error.
            let err = &screen[2..];
            assert!(
                err[0].starts_with("✗ image not found:"),
                "{}",
                screen.join("\n")
            );
            assert!(
                err[1..].iter().all(|l| l.starts_with("  ")),
                "{}",
                screen.join("\n")
            );
            let joined: String = err
                .iter()
                .map(|l| l.chars().skip(2).collect::<String>())
                .collect();
            assert_eq!(
                joined.replace(' ', ""),
                msg.replace(' ', ""),
                "{}",
                screen.join("\n")
            );
            assert!(
                screen_contains(&screen, "netinst.iso"),
                "{}",
                screen.join("\n")
            );
        }
        assert!(p.height_for(46) > p.height_for(200));
        // In too short an area the cursor row still wins.
        let screen = render_picker(&mut p, 46, 2);
        assert!(screen[0].starts_with("▸ New path"), "{}", screen.join("\n"));
    }

    #[test]
    fn picker_row_tail_is_shortened_not_cut() {
        let entries = vec![
            IsoEntry {
                path: "/home/user/iso/debian-12.3.0-amd64-netinst.iso".into(),
                state: ImageState {
                    size: 3 << 20,
                    ..Default::default()
                },
                used_by: vec!["debian-12".into(), "win11".into()],
            },
            IsoEntry {
                path: "/home/user/iso/old-drivers.iso".into(),
                state: ImageState {
                    err: Some("image not found: /home/user/iso/old-drivers.iso".into()),
                    ..Default::default()
                },
                used_by: vec!["debian-12".into()],
            },
        ];
        let mut p = IsoPicker::new("", "", entries);
        // The create form's ISO step at 80x24: the users are shortened, or
        // left out where not even `in use by …` fits; the state stays.
        let screen = render_picker(&mut p, 46, 3);
        assert!(
            screen[0].ends_with("netinst.iso  ● 3 MiB  in use by de…"),
            "{}",
            screen.join("\n")
        );
        assert!(
            screen[1].ends_with("old-drivers.iso  ✗ not found"),
            "{}",
            screen.join("\n")
        );
        // At 120x40 all of it fits.
        let screen = render_picker(&mut p, 76, 3);
        assert!(
            screen[0].ends_with("netinst.iso  ● 3 MiB  in use by debian-12, win11"),
            "{}",
            screen.join("\n")
        );
        assert!(
            screen[1].ends_with("✗ not found  in use by debian-12"),
            "{}",
            screen.join("\n")
        );
        // Narrower still, the state is shortened too rather than cut.
        let screen = render_picker(&mut p, 30, 3);
        assert!(screen[0].ends_with("  ● 3 M…"), "{}", screen.join("\n"));
        assert!(screen[1].ends_with("  ✗ not…"), "{}", screen.join("\n"));
        assert!(screen.iter().all(|l| l.chars().count() <= 30));
    }
}
