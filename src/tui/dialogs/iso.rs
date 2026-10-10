//! The ISO hot-plug dialog: swap or eject the boot ISO in a VM's CD-ROM
//! drive, and attach disk images (ISOs) to it as read-only USB drives. Each
//! change is saved to `vm.yaml` immediately and, for a running VM, applied
//! through the QEMU monitor on the spot. An image is chosen in the ISO
//! picker, which offers the ones used before and takes the path of a new one.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

use super::super::events::TaskResult;
use super::super::forms::common::form_block;
use super::super::panel::{Ctx, Panel};
use super::super::theme::Theme;
use super::super::widgets::{
    centered_rect, hints_line, pad_right, spinner_frame, truncate_left, window_note, wrap_text,
    Cursor,
};
use super::isopicker::{
    config_path_in, error_lines, flow, image_state_span, image_state_text, iso_entries_at,
    IsoPicker, PickOutcome, MIN_PATH_WIDTH,
};
use crate::config;
use crate::vm::{self, image_state_of, usb_image_states, ImageState, UsbImage, VmConfig};

/// What the ISO picker, when open, is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prompt {
    /// Attach an image as a USB drive.
    UsbImage,
    /// Put an ISO in the CD-ROM drive.
    BootIso,
}

impl Prompt {
    /// The popup title, the hint under it and what Enter does.
    fn texts(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Prompt::UsbImage => (
                "USB drive to attach",
                "an .iso (or any raw disk image) on the host: one used before, or the path of a new one; ~ is your home directory",
                "attach",
            ),
            Prompt::BootIso => (
                "Boot ISO (CD-ROM drive)",
                "the ISO to put in the drive: one used before, or the path of a new one; ~ is your home directory",
                "insert",
            ),
        }
    }
}

/// The picker while it is open, and what it is for.
struct OpenPicker {
    kind: Prompt,
    picker: IsoPicker,
}

/// The ISO hot-plug dialog (internal/tui/iso.go).
pub struct IsoDialog {
    cfg: VmConfig,
    /// The checked state of the boot ISO.
    cdrom: ImageState,
    /// The checked state of each USB image.
    states: Vec<ImageState>,
    /// Over `cfg.usb_images`.
    cursor: Cursor,
    running: bool,
    /// A change in flight, as the status bar names it.
    busy: Option<String>,
    err: String,
    notice: String,
    open: Option<OpenPicker>,
}

impl IsoDialog {
    /// A dialog for `cfg`; `running` is the VM's state when it opened (the
    /// first scan in `init` refreshes it). The images are checked here, on
    /// the spot, so the first draw already shows what is missing.
    pub fn new(cfg: VmConfig, running: bool) -> Self {
        IsoDialog {
            cdrom: image_state_of(&cfg.cdrom_path),
            states: usb_image_states(&cfg.usb_images),
            cfg,
            cursor: Cursor::default(),
            running,
            busy: None,
            err: String::new(),
            notice: String::new(),
            open: None,
        }
    }

    /// What the open picker is for, if one is open.
    pub fn prompt(&self) -> Option<Prompt> {
        self.open.as_ref().map(|o| o.kind)
    }

    /// The open picker, if any.
    pub fn picker(&self) -> Option<&IsoPicker> {
        self.open.as_ref().map(|o| &o.picker)
    }

    /// The config as the dialog last saw it.
    pub fn config(&self) -> &VmConfig {
        &self.cfg
    }

    /// The last success text, `""` when none.
    pub fn notice(&self) -> &str {
        &self.notice
    }

    /// The last error text, `""` when none.
    pub fn error(&self) -> &str {
        &self.err
    }

    /// The row under the cursor, an index into the USB images.
    pub fn cursor(&self) -> usize {
        self.cursor.index
    }

    // --- tasks ---

    /// Re-checks the images and the VM's state in the background.
    fn spawn_scan(&self, ctx: &mut Ctx) {
        let storage = ctx.storage_buf();
        let cfg = self.cfg.clone();
        ctx.spawn(move || {
            let status = vm::status(&storage, &cfg.name).unwrap_or_default();
            TaskResult::IsoScanned {
                cdrom: image_state_of(&cfg.cdrom_path),
                states: usb_image_states(&cfg.usb_images),
                status,
            }
        });
    }

    /// Marks a change as in flight: the spinner label, no error, no notice.
    fn start(&mut self, label: String) {
        self.busy = Some(label);
        self.err.clear();
        self.notice.clear();
    }

    /// Puts `path` into the CD-ROM drive, or empties it when `path` is `""`.
    fn set_boot_iso(&mut self, path: String, ctx: &mut Ctx) {
        let mut cfg = self.cfg.clone();
        cfg.cdrom_path.clone_from(&path);
        let (what, label) = if path.is_empty() {
            (
                "ejected the boot ISO".to_string(),
                "ejecting the boot ISO…".to_string(),
            )
        } else {
            let name = UsbImage { path: path.clone() }.label();
            (format!("inserted {name}"), format!("inserting {name}…"))
        };
        self.start(label);
        let storage = ctx.storage_buf();
        let config_path = config_path_in(ctx.home);
        let running = self.running;
        ctx.spawn(move || {
            if let Err(e) = vm::save_config(&storage, &cfg) {
                return failed(None, format!("save VM config: {e:#}"));
            }
            if !path.is_empty() {
                let _ = config::remember_iso_at(&config_path, &path); // for the picker; losing it costs nothing
            }
            if !running {
                return applied(cfg, format!("{what} — takes effect on next start"));
            }
            match vm::cdrom_change(&storage, &cfg.name, &path) {
                Err(e) => failed(
                    Some(cfg),
                    format!(
                        "{what} in config, but the running VM's drive could not be changed (takes effect on next start):\n{e:#}"
                    ),
                ),
                Ok(()) => applied(cfg, format!("{what} (hot-swapped)")),
            }
        });
    }

    /// Attaches `img` as a USB drive: saved first, hot-plugged when running.
    fn attach(&mut self, img: UsbImage, ctx: &mut Ctx) {
        let mut cfg = self.cfg.clone();
        cfg.usb_images.push(img.clone());
        let idx = cfg.usb_images.len() - 1;
        let name = img.label();
        self.start(format!("attaching {name}…"));
        let storage = ctx.storage_buf();
        let config_path = config_path_in(ctx.home);
        let running = self.running;
        ctx.spawn(move || {
            if let Err(e) = vm::save_config(&storage, &cfg) {
                return failed(None, format!("save VM config: {e:#}"));
            }
            let _ = config::remember_iso_at(&config_path, &img.path); // for the picker; losing it costs nothing
            if !running {
                return applied(cfg, format!("attached {name} — takes effect on next start"));
            }
            match vm::usb_image_hotplug(&storage, &cfg, idx) {
                Err(e) => failed(
                    Some(cfg),
                    format!("attached {name} in config, but hot-plug failed (takes effect on next start):\n{e:#}"),
                ),
                Ok(()) => applied(cfg, format!("attached {name} (hot-plugged as a USB drive)")),
            }
        });
    }

    /// Detaches the image at `idx`. The hot-unplug is named from the config
    /// as it was before the removal, since duplicate names get positional
    /// suffixes.
    fn detach(&mut self, idx: usize, ctx: &mut Ctx) {
        let old = self.cfg.clone();
        let Some(img) = old.usb_images.get(idx).cloned() else {
            return;
        };
        let mut cfg = old.clone();
        cfg.usb_images.remove(idx);
        let name = img.label();
        self.start(format!("detaching {name}…"));
        let storage = ctx.storage_buf();
        let running = self.running;
        ctx.spawn(move || {
            if let Err(e) = vm::save_config(&storage, &cfg) {
                return failed(None, format!("save VM config: {e:#}"));
            }
            if !running {
                return applied(cfg, format!("detached {name} — takes effect on next start"));
            }
            match vm::usb_image_hotunplug(&storage, &old, idx) {
                Err(e) => failed(
                    Some(cfg),
                    format!("detached {name} in config, but hot-unplug failed (takes effect on next start):\n{e:#}"),
                ),
                Ok(()) => applied(cfg, format!("detached {name} (hot-unplugged)")),
            }
        });
    }

    // --- keys ---

    /// Space/Enter/d: detaches the image under the cursor, unless a change
    /// is in flight or there is none.
    fn detach_current(&mut self, ctx: &mut Ctx) {
        if self.busy.is_some() || self.cfg.usb_images.is_empty() {
            return;
        }
        self.detach(self.cursor.index, ctx);
    }

    /// e: empties the drive; an empty one is only reported.
    fn eject(&mut self, ctx: &mut Ctx) {
        if self.busy.is_some() {
            return;
        }
        if self.cfg.cdrom_path.is_empty() {
            self.err.clear();
            self.notice = "the CD-ROM drive is already empty".to_string();
            return;
        }
        self.set_boot_iso(String::new(), ctx);
    }

    /// Opens the picker over the images used before (no none row, no
    /// current path). The entries are read right here: a few small files.
    fn open_prompt(&mut self, kind: Prompt, ctx: &mut Ctx) {
        self.err.clear();
        self.notice.clear();
        let entries = iso_entries_at(ctx.storage(), &config_path_in(ctx.home));
        self.open = Some(OpenPicker {
            kind,
            picker: IsoPicker::new("", "", entries),
        });
    }

    /// Forwards a key to the open picker and acts on its pick: the image
    /// goes into the drive or onto the USB bus, as the prompt says.
    fn picker_key(&mut self, key: KeyEvent, ctx: &mut Ctx) {
        let Some(open) = self.open.as_mut() else {
            return;
        };
        match open.picker.handle_key(key, ctx.home) {
            PickOutcome::Nothing => {}
            PickOutcome::Cancelled => self.open = None,
            PickOutcome::Picked(path) => {
                let kind = open.kind;
                // One change at a time: a second save started from the
                // config the first has not updated yet would undo it.
                if let Some(label) = &self.busy {
                    open.picker.set_error(format!(
                        "still {label} — press Enter again when that is done"
                    ));
                    return;
                }
                if kind == Prompt::UsbImage && self.cfg.usb_images.iter().any(|i| i.path == path) {
                    open.picker
                        .set_error(format!("{path} is already attached to this VM"));
                    return;
                }
                self.open = None;
                match kind {
                    Prompt::BootIso => self.set_boot_iso(path, ctx),
                    Prompt::UsbImage => self.attach(UsbImage { path }, ctx),
                }
            }
        }
    }

    // --- drawing ---

    /// The checked state of the boot ISO, or an unchecked one when the
    /// state has not caught up with the config yet.
    fn cdrom_state(&self) -> ImageState {
        if self.cdrom.path == self.cfg.cdrom_path {
            self.cdrom.clone()
        } else {
            ImageState {
                path: self.cfg.cdrom_path.clone(),
                ..ImageState::default()
            }
        }
    }

    /// The checked state of image `i`, or an unchecked one when the states
    /// have not caught up with the config yet.
    fn state_of(&self, i: usize) -> ImageState {
        let path = self
            .cfg
            .usb_images
            .get(i)
            .map(|img| img.path.as_str())
            .unwrap_or("");
        match self.states.get(i) {
            Some(s) if s.path == path => s.clone(),
            _ => ImageState {
                path: path.to_string(),
                ..ImageState::default()
            },
        }
    }

    /// The path column the boot ISO and the USB images share, so their
    /// states line up: as wide as the longest path, as long as the widest
    /// state still fits on the right, and never under [`MIN_PATH_WIDTH`].
    fn path_width(&self, width: usize) -> usize {
        let mut longest = 0;
        let mut state_w = 0;
        let mut add = |path: &str, state: ImageState| {
            longest = longest.max(path.chars().count());
            state_w = state_w.max(image_state_text(&state).chars().count());
        };
        if !self.cfg.cdrom_path.is_empty() {
            add(&self.cfg.cdrom_path, self.cdrom_state());
        }
        for (i, img) in self.cfg.usb_images.iter().enumerate() {
            add(&img.path, self.state_of(i));
        }
        // The two-cell prefix, the path, two spaces, the state.
        width
            .saturating_sub(2 + 2 + state_w)
            .max(MIN_PATH_WIDTH)
            .min(longest.max(MIN_PATH_WIDTH))
    }

    /// A path cut on the left so the file name stays visible, padded to the
    /// shared column `path_w`, with the image's state after it.
    fn path_line(
        prefix: Span<'static>,
        path: &str,
        state: &ImageState,
        path_w: usize,
        focused: bool,
        theme: &Theme,
    ) -> Line<'static> {
        let col = pad_right(&truncate_left(path, path_w), path_w);
        let style = if focused { theme.label } else { theme.normal };
        Line::from(vec![
            prefix,
            Span::styled(col, style),
            Span::raw("  "),
            image_state_span(state, theme),
        ])
    }

    /// The body of the panel, picker or not, for an area `width` × `height`:
    /// the head, the USB-image rows scrolled to keep the cursor in view, and
    /// the outcome line under them, which always stays on screen.
    fn body_lines(
        &mut self,
        width: usize,
        height: usize,
        theme: &Theme,
        tick: u64,
    ) -> Vec<Line<'static>> {
        let path_w = self.path_width(width);
        let mut head = if self.running {
            flow(
                vec![
                    Span::styled("● running", theme.running),
                    Span::styled(" ", theme.help),
                ],
                "— changes are applied in the guest right away",
                theme.help,
                width,
            )
        } else {
            flow(
                vec![
                    Span::styled("● stopped", theme.stopped),
                    Span::styled(" ", theme.help),
                ],
                "— changes take effect when the VM is started",
                theme.help,
                width,
            )
        };
        head.push(Line::raw(""));

        head.push(Line::styled("Boot ISO (CD-ROM drive)", theme.label));
        head.push(Line::raw(""));
        if self.cfg.cdrom_path.is_empty() {
            head.push(Line::styled("  (empty)", theme.help));
        } else {
            let state = self.cdrom_state();
            head.push(Self::path_line(
                Span::raw("  "),
                &self.cfg.cdrom_path,
                &state,
                path_w,
                false,
                theme,
            ));
        }
        head.push(Line::raw(""));

        const DRIVES: &str = "USB drives";
        const DRIVES_HELP: &str =
            "images attached read-only; the guest sees each one as a USB stick";
        if DRIVES.len() + 3 + DRIVES_HELP.len() <= width {
            head.push(Line::from(vec![
                Span::styled(DRIVES, theme.label),
                Span::styled(format!("   {DRIVES_HELP}"), theme.help),
            ]));
        } else {
            head.push(Line::styled(DRIVES, theme.label));
            for piece in wrap_text(DRIVES_HELP, width.saturating_sub(2).max(8)) {
                head.push(Line::styled(format!("  {piece}"), theme.help));
            }
        }
        head.push(Line::raw(""));

        if self.cfg.usb_images.is_empty() {
            head.extend(flow(
                Vec::new(),
                "No images attached. Press a to attach one.",
                theme.help,
                width,
            ));
        }

        let mut tail: Vec<Line<'static>> = vec![Line::raw("")];
        if let Some(label) = &self.busy {
            tail.push(Line::from(vec![
                Span::styled(spinner_frame(tick), theme.spinner),
                Span::styled(format!(" {label}"), theme.normal),
            ]));
        } else if !self.err.is_empty() {
            // Each error line wrapped on its own, `✗ ` before the first
            // piece and two spaces of indent on the rest.
            let wrapped: Vec<String> = self
                .err
                .lines()
                .flat_map(|raw| wrap_text(raw, width.saturating_sub(2).max(8)))
                .collect();
            tail.extend(error_lines(&wrapped.join("\n"), theme));
        } else if !self.notice.is_empty() {
            tail.push(Line::styled(format!("✓ {}", self.notice), theme.success));
        }

        // The rows get what is left, scrolled to keep the cursor in view;
        // when they do not all fit, a line under them says which are shown.
        let total = self.cfg.usb_images.len();
        let avail = height.saturating_sub(head.len() + tail.len());
        let clipped = total > avail.max(1);
        let rows_h = avail.saturating_sub(usize::from(clipped)).max(1);
        let range = self.cursor.window(total, rows_h);
        let mut lines = head;
        for i in range.clone() {
            let focused = i == self.cursor.index;
            let marker = if focused {
                Span::styled("▸ ", theme.label)
            } else {
                Span::raw("  ")
            };
            let state = self.state_of(i);
            lines.push(Self::path_line(
                marker,
                &self.cfg.usb_images[i].path,
                &state,
                path_w,
                focused,
                theme,
            ));
        }
        if clipped {
            lines.push(Line::styled(window_note(range, total), theme.help));
        }
        lines.extend(tail);
        lines
    }

    /// The picker as a centred modal: its title, the hint and the rows.
    fn render_picker(&mut self, frame: &mut Frame, theme: &Theme) {
        let Some(open) = self.open.as_mut() else {
            return;
        };
        let area = frame.area();
        if area.width < 12 || area.height < 5 {
            return;
        }
        let (title, hint, action) = open.kind.texts();
        let width = (area.width * 4 / 5).clamp(40, 110).min(area.width);
        let inner_w = width.saturating_sub(4) as usize;
        let hint_lines = wrap_text(hint, inner_w.max(8));
        let hint_h = hint_lines.len() as u16;
        let height = (hint_h + 1 + open.picker.height_for(inner_w as u16) + 2).min(area.height);
        let rect = centered_rect(area, width, height);
        frame.render_widget(Clear, rect);
        // The keys along the bottom border, as on the edit form's popup.
        let hints = picker_hints(&open.picker, action);
        let pairs: Vec<(&str, &str)> = hints
            .iter()
            .map(|(k, d)| (k.as_str(), d.as_str()))
            .collect();
        let mut footer = hints_line(&pairs, theme);
        footer.spans.insert(0, Span::raw(" "));
        footer.spans.push(Span::raw(" "));
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme.border_focused)
            .title(Line::styled(format!(" {title} "), theme.title))
            .title_bottom(footer.right_aligned());
        let inner = block.inner(rect).inner(Margin::new(1, 0));
        frame.render_widget(block, rect);
        let [hint_area, _, picker_area] = Layout::vertical([
            Constraint::Length(hint_h),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .areas(inner);
        let hint: Vec<Line> = hint_lines
            .into_iter()
            .map(|l| Line::styled(l, theme.help))
            .collect();
        frame.render_widget(Paragraph::new(hint), hint_area);
        open.picker.render(frame, picker_area, theme);
    }
}

/// The open picker's key hints: its own, with what Enter does here, and
/// the way back to the dialog.
fn picker_hints(picker: &IsoPicker, action: &str) -> Vec<(String, String)> {
    let mut hints = picker.key_hints(action);
    hints.push(("Esc".to_string(), "cancel".to_string()));
    hints
}

/// A saved change with its success text.
fn applied(cfg: VmConfig, notice: String) -> TaskResult {
    TaskResult::IsoApplied {
        cfg: Some(Box::new(cfg)),
        notice,
        err: None,
    }
}

/// A failed change; `cfg` is the saved config when the save went through.
fn failed(cfg: Option<VmConfig>, err: String) -> TaskResult {
    TaskResult::IsoApplied {
        cfg: cfg.map(Box::new),
        notice: String::new(),
        err: Some(err),
    }
}

impl Panel for IsoDialog {
    fn title(&self) -> String {
        format!("ISO Hot-plug: {}", self.cfg.name)
    }

    fn init(&mut self, ctx: &mut Ctx) {
        self.spawn_scan(ctx);
    }

    fn handle_key(&mut self, key: KeyEvent, ctx: &mut Ctx) {
        if self.open.is_some() {
            self.picker_key(key, ctx);
            return;
        }
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let n = self.cfg.usb_images.len();
        // Closing waits for the change in flight, so quitting still waits
        // for it; the spinner says why.
        let closable = self.busy.is_none();
        match key.code {
            KeyCode::Esc if closable => ctx.close(),
            KeyCode::Char('q') | KeyCode::Char('h') if plain && closable => ctx.close(),
            KeyCode::Up => self.cursor.up(1),
            KeyCode::Char('k') if plain => self.cursor.up(1),
            KeyCode::Down => self.cursor.down(1, n),
            KeyCode::Char('j') if plain => self.cursor.down(1, n),
            KeyCode::Char('g') if plain => self.cursor.top(),
            KeyCode::Char('G') if plain => self.cursor.bottom(n),
            KeyCode::Enter => self.detach_current(ctx),
            KeyCode::Char(' ') | KeyCode::Char('d') if plain => self.detach_current(ctx),
            KeyCode::Char('a') if plain => self.open_prompt(Prompt::UsbImage, ctx),
            KeyCode::Char('c') if plain => self.open_prompt(Prompt::BootIso, ctx),
            KeyCode::Char('e') if plain => self.eject(ctx),
            KeyCode::Char('r') if plain => {
                self.err.clear();
                self.notice.clear();
                self.spawn_scan(ctx);
            }
            _ => {}
        }
    }

    fn handle_paste(&mut self, text: &str, _ctx: &mut Ctx) {
        if let Some(open) = self.open.as_mut() {
            open.picker.handle_paste(text);
        }
    }

    fn on_task(&mut self, result: TaskResult, ctx: &mut Ctx) {
        match result {
            TaskResult::IsoScanned {
                cdrom,
                states,
                status,
            } => {
                self.cdrom = cdrom;
                self.states = states;
                self.running = status.running();
                self.cursor.clamp(self.cfg.usb_images.len());
            }
            TaskResult::IsoApplied { cfg, notice, err } => {
                self.busy = None;
                if let Some(cfg) = cfg {
                    self.cfg = *cfg;
                }
                match err {
                    Some(err) => {
                        self.err = err;
                        self.notice.clear();
                    }
                    None => {
                        self.notice = notice;
                        self.err.clear();
                    }
                }
                self.cdrom = image_state_of(&self.cfg.cdrom_path);
                self.states = usb_image_states(&self.cfg.usb_images);
                self.cursor.clamp(self.cfg.usb_images.len());
                self.spawn_scan(ctx);
            }
            TaskResult::Failed { err, .. } => {
                // A change that died on its thread: say so instead of
                // spinning forever.
                self.busy = None;
                self.err = err;
                self.notice.clear();
            }
            _ => {}
        }
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme, tick: u64) {
        if area.width > 0 && area.height > 0 {
            let inner = form_block(frame, area, &self.title(), theme);
            if inner.width > 0 && inner.height > 0 {
                let lines =
                    self.body_lines(inner.width as usize, inner.height as usize, theme, tick);
                frame.render_widget(Paragraph::new(lines), inner);
            }
        }
        self.render_picker(frame, theme);
    }

    fn key_hints(&self) -> Vec<(String, String)> {
        if let Some(open) = &self.open {
            let (_, _, action) = open.kind.texts();
            return picker_hints(&open.picker, action);
        }
        // While a change is in flight the dialog does not close; Ctrl-c
        // quits once it is done (or at once, on a second press).
        let back = if self.busy.is_some() {
            ("Ctrl-c", "quit")
        } else {
            ("q/Esc", "back")
        };
        [
            ("c", "change boot ISO"),
            ("e", "eject"),
            ("a", "attach USB image"),
            ("Space/Enter/d", "detach"),
            ("r", "refresh"),
            ("j/k", "move"),
            back,
        ]
        .iter()
        .map(|(k, d)| (k.to_string(), d.to_string()))
        .collect()
    }

    fn busy(&self) -> Option<String> {
        self.busy.clone()
    }

    fn failed(&self) -> bool {
        !self.err.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::Duration;

    use super::super::isopicker::tests::{isolate_config, save_vm, write_image};
    use super::*;
    use crate::tui::panel::Action;
    use crate::tui::testutil::*;
    use crate::vm::{load_config, ProcessInfo, VmStatus};

    /// Takes the next task result, which must be a successful apply, feeds
    /// it to the dialog and returns the actions. The apply triggers a
    /// rescan; that result is taken off the queue too, and delivered only
    /// when it is a scan (a stub `vm::status` would make it a failure).
    fn apply(h: &Harness, d: &mut IsoDialog) -> Vec<Action> {
        let r = h
            .next_result(Duration::from_secs(10))
            .expect("an apply result");
        assert!(
            matches!(&r, TaskResult::IsoApplied { err: None, .. }),
            "apply: {r:?}"
        );
        let mut ctx = h.ctx();
        d.on_task(r, &mut ctx);
        let actions = ctx.actions;
        let rescan = h
            .next_result(Duration::from_secs(10))
            .expect("a rescan result");
        if matches!(rescan, TaskResult::IsoScanned { .. }) {
            d.on_task(rescan, &mut h.ctx());
        }
        actions
    }

    /// Moves the open picker's cursor to its New path row, types `path`
    /// there and presses Enter.
    fn pick_new(h: &Harness, d: &mut IsoDialog, path: &str) -> Vec<Action> {
        assert!(d.prompt().is_some(), "the picker is not open");
        while !d.picker().unwrap().on_input() {
            h.press(d, key(KeyCode::Down));
        }
        type_str(d, h, path);
        h.press(d, key(KeyCode::Enter))
    }

    /// A scan result for the dialog's current config, computed here.
    fn scanned(d: &IsoDialog, running: bool) -> TaskResult {
        TaskResult::IsoScanned {
            cdrom: image_state_of(&d.cfg.cdrom_path),
            states: usb_image_states(&d.cfg.usb_images),
            status: ProcessInfo {
                pid: 0,
                status: if running {
                    VmStatus::Running
                } else {
                    VmStatus::Stopped
                },
            },
        }
    }

    fn image_paths(d: &IsoDialog) -> Vec<String> {
        d.cfg.usb_images.iter().map(|i| i.path.clone()).collect()
    }

    #[test]
    fn attach_and_detach_usb_images() {
        let h = Harness::new();
        let cfg_path = isolate_config(&h);
        let isos = h.dir.path().join("isos");
        fs::create_dir_all(&isos).unwrap();
        let present = write_image(&isos.join("virtio-win.iso"), 3 << 20);
        let missing = isos.join("gone.iso").to_string_lossy().into_owned();
        let cfg = VmConfig {
            name: "t".into(),
            cpu: 1,
            ram: 128,
            usb_images: vec![UsbImage {
                path: missing.clone(),
            }],
            ..VmConfig::default()
        };
        save_vm(h.mgr.storage(), &cfg);

        let mut d = IsoDialog::new(cfg, false);
        assert_eq!(d.title(), "ISO Hot-plug: t");
        let screen = h.render(&mut d, 100, 40);
        for want in [
            "ISO Hot-plug: t",
            "● stopped — changes take effect when the VM is started",
            "Boot ISO (CD-ROM drive)",
            "(empty)",
            "USB drives   images attached read-only; the guest sees each one as a USB stick",
            "gone.iso",
            "✗ not found",
        ] {
            assert!(
                screen_contains(&screen, want),
                "screen lacks {want:?}:\n{}",
                screen.join("\n")
            );
        }
        assert!(
            screen
                .iter()
                .any(|l| l.contains("▸ ") && l.contains("gone.iso")),
            "{}",
            screen.join("\n")
        );

        // Attach by typing a path in the picker. Nothing was used before,
        // but the VM's own missing image is listed, as this VM has it.
        assert!(h.press(&mut d, ch('a')).is_empty());
        assert_eq!(d.prompt(), Some(Prompt::UsbImage));
        let screen = h.render(&mut d, 100, 40);
        for want in [
            "USB drive to attach",
            "an .iso (or any raw disk image) on the host",
            "New path",
            "gone.iso",
        ] {
            assert!(
                screen_contains(&screen, want),
                "a should open the picker; screen lacks {want:?}:\n{}",
                screen.join("\n")
            );
        }
        let hints = d.key_hints();
        assert_eq!(hints[0], ("Enter".to_string(), "attach".to_string()));
        assert_eq!(
            hints.last().unwrap(),
            &("Esc".to_string(), "cancel".to_string())
        );
        {
            let p = d.picker().unwrap();
            assert_eq!(p.entries().len(), 1);
            assert_eq!(p.entries()[0].path, missing);
            assert_eq!(p.cursor(), 0);
        }
        assert!(pick_new(&h, &mut d, &present).is_empty());
        assert_eq!(d.prompt(), None);
        assert_eq!(d.busy(), Some("attaching virtio-win.iso…".to_string()));
        let screen = h.render(&mut d, 100, 40);
        assert!(
            screen_contains(&screen, "attaching virtio-win.iso…"),
            "{}",
            screen.join("\n")
        );
        assert!(apply(&h, &mut d).is_empty(), "the dialog stays open");
        assert_eq!(d.busy(), None);
        assert_eq!(image_paths(&d), [missing.clone(), present.clone()]);
        let saved = load_config(h.mgr.storage(), "t").unwrap();
        assert_eq!(saved.usb_images.len(), 2);
        assert_eq!(saved.usb_images[1].path, present);
        assert_eq!(
            d.notice(),
            "attached virtio-win.iso — takes effect on next start"
        );
        assert_eq!(
            config::recent_isos_at(&cfg_path),
            std::slice::from_ref(&present)
        );
        d.on_task(scanned(&d, false), &mut h.ctx());
        let screen = h.render(&mut d, 100, 40);
        assert!(screen_contains(&screen, "● 3 MiB"), "{}", screen.join("\n"));
        assert!(
            screen_contains(
                &screen,
                "✓ attached virtio-win.iso — takes effect on next start"
            ),
            "{}",
            screen.join("\n")
        );

        // The same image twice, and a file that is not there, are refused
        // and the picker stays open. The remembered image is now listed
        // first.
        h.press(&mut d, ch('a'));
        {
            let p = d.picker().unwrap();
            assert_eq!(p.entries().len(), 2);
            assert_eq!(p.entries()[0].path, present);
            assert_eq!(p.entries()[1].path, missing);
        }
        h.press(&mut d, key(KeyCode::Enter));
        assert_eq!(d.prompt(), Some(Prompt::UsbImage));
        assert_eq!(
            d.picker().unwrap().error(),
            format!("{present} is already attached to this VM")
        );
        let screen = h.render(&mut d, 100, 40);
        assert!(
            screen_contains(&screen, "is already attached to this VM"),
            "{}",
            screen.join("\n")
        );
        pick_new(&h, &mut d, &isos.join("nope.iso").to_string_lossy());
        assert_eq!(d.prompt(), Some(Prompt::UsbImage));
        assert!(
            d.picker().unwrap().error().contains("not found"),
            "{:?}",
            d.picker().unwrap().error()
        );
        assert!(
            h.press(&mut d, key(KeyCode::Esc)).is_empty(),
            "Esc closes the picker, not the dialog"
        );
        assert_eq!(d.prompt(), None);
        assert!(
            h.next_result(Duration::from_millis(200)).is_none(),
            "refusals spawn nothing"
        );

        // Detach the missing one (row 0) with Space; the present one stays.
        h.press(&mut d, ch('g'));
        h.press(&mut d, ch(' '));
        apply(&h, &mut d);
        assert_eq!(image_paths(&d), std::slice::from_ref(&present));
        assert_eq!(d.notice(), "detached gone.iso — takes effect on next start");
        // Detach the last one with d: the list is empty and the cursor safe.
        h.press(&mut d, ch('d'));
        apply(&h, &mut d);
        assert!(d.cfg.usb_images.is_empty());
        assert_eq!(d.cursor(), 0);
        assert!(load_config(h.mgr.storage(), "t")
            .unwrap()
            .usb_images
            .is_empty());
        h.press(&mut d, ch(' '));
        assert!(
            h.next_result(Duration::from_millis(200)).is_none(),
            "detach on an empty list must do nothing"
        );
        let screen = h.render(&mut d, 100, 40);
        assert!(
            screen_contains(&screen, "No images attached. Press a to attach one."),
            "{}",
            screen.join("\n")
        );

        // Esc, q and h close the dialog.
        for k in [key(KeyCode::Esc), ch('q'), ch('h')] {
            let acts = h.press(&mut d, k);
            assert!(matches!(acts[..], [Action::Close]), "{acts:?}");
        }
        assert_eq!(
            d.key_hints()[0],
            ("c".to_string(), "change boot ISO".to_string())
        );
    }

    #[test]
    fn insert_and_eject_boot_iso() {
        let h = Harness::new();
        let cfg_path = isolate_config(&h);
        let isos = h.dir.path().join("isos");
        fs::create_dir_all(&isos).unwrap();
        let disc = write_image(&isos.join("debian.iso"), 2 << 20);
        let cfg = VmConfig {
            name: "t".into(),
            cpu: 1,
            ram: 128,
            ..VmConfig::default()
        };
        save_vm(h.mgr.storage(), &cfg);

        let mut d = IsoDialog::new(cfg, false);
        let screen = h.render(&mut d, 100, 40);
        assert!(
            screen_contains(&screen, "Boot ISO (CD-ROM drive)")
                && screen_contains(&screen, "(empty)"),
            "{}",
            screen.join("\n")
        );

        // Nothing to eject yet.
        h.press(&mut d, ch('e'));
        assert!(h.next_result(Duration::from_millis(200)).is_none());
        assert_eq!(d.notice(), "the CD-ROM drive is already empty");

        // Put a disc in. Nothing was used before, so the picker opens on
        // its New path row.
        h.press(&mut d, ch('c'));
        assert_eq!(d.prompt(), Some(Prompt::BootIso));
        assert!(
            d.notice().is_empty(),
            "opening the picker clears the notice"
        );
        let screen = h.render(&mut d, 100, 40);
        for want in [
            "Boot ISO (CD-ROM drive)",
            "the ISO to put in the drive",
            "New path",
        ] {
            assert!(
                screen_contains(&screen, want),
                "c should open the picker; screen lacks {want:?}:\n{}",
                screen.join("\n")
            );
        }
        assert_eq!(
            d.key_hints()[0],
            ("Enter".to_string(), "insert".to_string())
        );
        assert!(d.picker().unwrap().entries().is_empty() && d.picker().unwrap().on_input());
        type_str(&mut d, &h, &disc);
        h.press(&mut d, key(KeyCode::Enter));
        assert_eq!(d.prompt(), None);
        assert_eq!(d.busy(), Some("inserting debian.iso…".to_string()));
        apply(&h, &mut d);
        assert_eq!(d.cfg.cdrom_path, disc);
        assert_eq!(load_config(h.mgr.storage(), "t").unwrap().cdrom_path, disc);
        assert_eq!(
            d.notice(),
            "inserted debian.iso — takes effect on next start"
        );
        d.on_task(scanned(&d, false), &mut h.ctx());
        let screen = h.render(&mut d, 100, 40);
        assert!(
            screen_contains(&screen, &disc) && screen_contains(&screen, "● 2 MiB"),
            "{}",
            screen.join("\n")
        );

        // A path that is not there is refused and the picker stays open.
        // The disc is listed now, in use by this VM.
        h.press(&mut d, ch('c'));
        {
            let p = d.picker().unwrap();
            assert_eq!(p.entries().len(), 1);
            assert_eq!(p.entries()[0].path, disc);
            assert_eq!(p.entries()[0].used_by, ["t"]);
        }
        let screen = h.render(&mut d, 100, 40);
        assert!(
            screen_contains(&screen, "in use by t"),
            "{}",
            screen.join("\n")
        );
        pick_new(&h, &mut d, &isos.join("nope.iso").to_string_lossy());
        assert_eq!(d.prompt(), Some(Prompt::BootIso));
        assert!(d.picker().unwrap().error().contains("not found"));
        h.press(&mut d, key(KeyCode::Esc));

        // Eject.
        h.press(&mut d, ch('e'));
        assert_eq!(d.busy(), Some("ejecting the boot ISO…".to_string()));
        apply(&h, &mut d);
        assert_eq!(d.cfg.cdrom_path, "");
        assert_eq!(
            d.notice(),
            "ejected the boot ISO — takes effect on next start"
        );
        assert_eq!(load_config(h.mgr.storage(), "t").unwrap().cdrom_path, "");
        let screen = h.render(&mut d, 100, 40);
        assert!(screen_contains(&screen, "(empty)"), "{}", screen.join("\n"));

        // The ejected disc is still remembered: put it back by picking it.
        assert_eq!(
            config::recent_isos_at(&cfg_path),
            std::slice::from_ref(&disc)
        );
        h.press(&mut d, ch('c'));
        {
            let p = d.picker().unwrap();
            assert_eq!(p.entries().len(), 1);
            assert!(p.entries()[0].used_by.is_empty());
            assert_eq!(p.cursor(), 0);
        }
        h.press(&mut d, key(KeyCode::Enter));
        apply(&h, &mut d);
        assert_eq!(d.cfg.cdrom_path, disc);
        assert_eq!(
            d.notice(),
            "inserted debian.iso — takes effect on next start"
        );
        // Re-inserting the disc that is already in the drive is allowed.
        h.press(&mut d, ch('c'));
        h.press(&mut d, key(KeyCode::Enter));
        assert_eq!(d.prompt(), None);
        apply(&h, &mut d);
        assert_eq!(d.cfg.cdrom_path, disc);
    }

    #[test]
    fn running_state_and_errors_render() {
        let h = Harness::new();
        let mut d = IsoDialog::new(
            VmConfig {
                name: "r".into(),
                ..VmConfig::default()
            },
            true,
        );
        let screen = h.render(&mut d, 80, 30);
        assert!(
            screen_contains(
                &screen,
                "● running — changes are applied in the guest right away"
            ),
            "{}",
            screen.join("\n")
        );
        // The scan has the last word on the state.
        d.on_task(scanned(&d, false), &mut h.ctx());
        let screen = h.render(&mut d, 80, 30);
        assert!(
            screen_contains(&screen, "● stopped"),
            "{}",
            screen.join("\n")
        );
        // A failed change keeps the config it saved and shows why, over
        // several lines.
        d.busy = Some("x".into());
        d.on_task(
            TaskResult::IsoApplied {
                cfg: Some(Box::new(VmConfig { name: "r".into(), cdrom_path: "/isos/a.iso".into(), ..VmConfig::default() })),
                notice: String::new(),
                err: Some("inserted a.iso in config, but the running VM's drive could not be changed (takes effect on next start):\nQEMU: nope".into()),
            },
            &mut h.ctx(),
        );
        let _ = h.next_result(Duration::from_secs(10)); // the rescan
        assert_eq!(d.busy(), None);
        assert_eq!(d.cfg.cdrom_path, "/isos/a.iso");
        let screen = h.render(&mut d, 120, 30);
        assert!(
            screen_contains(
                &screen,
                "✗ inserted a.iso in config, but the running VM's drive could not be changed"
            ),
            "{}",
            screen.join("\n")
        );
        assert!(
            screen_contains(&screen, "  QEMU: nope"),
            "{}",
            screen.join("\n")
        );
        assert!(
            screen_contains(&screen, "/isos/a.iso"),
            "{}",
            screen.join("\n")
        );
        assert!(
            screen_contains(&screen, "✗ not found"),
            "{}",
            screen.join("\n")
        );
        // r clears it and rescans.
        h.press(&mut d, ch('r'));
        assert!(d.error().is_empty());
        assert!(h.next_result(Duration::from_secs(10)).is_some());
        // A task that died is reported instead of spinning forever.
        d.busy = Some("x".into());
        d.on_task(
            TaskResult::Failed {
                what: "background task".into(),
                err: "internal error: boom".into(),
            },
            &mut h.ctx(),
        );
        assert_eq!(d.busy(), None);
        assert_eq!(d.error(), "internal error: boom");
        // Tiny and odd sizes never panic.
        for (w, hgt) in [(0, 0), (1, 1), (3, 3), (10, 2), (30, 5)] {
            let _ = h.render(&mut d, w, hgt);
        }
        h.press(&mut d, ch('a'));
        for (w, hgt) in [(0, 0), (1, 1), (3, 3), (10, 2), (30, 5), (45, 6)] {
            let _ = h.render(&mut d, w, hgt);
        }
        // Keys while busy: detach and eject are ignored, the picker opens.
        h.press(&mut d, key(KeyCode::Esc));
        d.busy = Some("x".into());
        d.cfg.usb_images.push(UsbImage {
            path: "/isos/b.iso".into(),
        });
        h.press(&mut d, ch('d'));
        h.press(&mut d, ch('e'));
        assert!(h.next_result(Duration::from_millis(200)).is_none());
        h.press(&mut d, ch('c'));
        assert_eq!(d.prompt(), Some(Prompt::BootIso));
        // Pasted text lands in the picker's input.
        while !d.picker().unwrap().on_input() {
            h.press(&mut d, key(KeyCode::Down));
        }
        let mut ctx = h.ctx();
        d.handle_paste("/pasted.iso", &mut ctx);
        assert_eq!(d.picker().unwrap().input_value(), "/pasted.iso");
    }

    #[test]
    fn cursor_moves_over_the_images() {
        let h = Harness::new();
        let cfg = VmConfig {
            name: "m".into(),
            usb_images: vec![
                UsbImage {
                    path: "/isos/a.iso".into(),
                },
                UsbImage {
                    path: "/isos/b.iso".into(),
                },
                UsbImage {
                    path: "/isos/c.iso".into(),
                },
            ],
            ..VmConfig::default()
        };
        let mut d = IsoDialog::new(cfg, false);
        h.press(&mut d, ch('j'));
        assert_eq!(d.cursor(), 1);
        h.press(&mut d, key(KeyCode::Down));
        h.press(&mut d, key(KeyCode::Down));
        assert_eq!(d.cursor(), 2, "no wrap");
        h.press(&mut d, ch('k'));
        assert_eq!(d.cursor(), 1);
        h.press(&mut d, ch('G'));
        assert_eq!(d.cursor(), 2);
        h.press(&mut d, ch('g'));
        assert_eq!(d.cursor(), 0);
        h.press(&mut d, key(KeyCode::Up));
        assert_eq!(d.cursor(), 0);
        let screen = h.render(&mut d, 100, 30);
        assert!(
            screen.iter().any(|l| l.contains("▸ /isos/a.iso")),
            "{}",
            screen.join("\n")
        );
        assert!(
            screen.iter().any(|l| l.contains("  /isos/b.iso")),
            "{}",
            screen.join("\n")
        );
        // The help under "USB drives" moves to its own lines in a narrow pane.
        let screen = h.render(&mut d, 50, 30);
        assert!(
            screen_contains(&screen, "USB drives"),
            "{}",
            screen.join("\n")
        );
        assert!(
            screen_contains(&screen, "  images attached read-only;"),
            "{}",
            screen.join("\n")
        );
    }

    /// The column, in chars, where `needle` starts in `line`.
    fn char_col(line: &str, needle: &str) -> Option<usize> {
        line.find(needle).map(|b| line[..b].chars().count())
    }

    #[test]
    fn usb_image_rows_scroll_and_the_outcome_stays_visible() {
        let h = Harness::new();
        let cfg = VmConfig {
            name: "s".into(),
            usb_images: (0..15)
                .map(|i| UsbImage {
                    path: format!("/isos/img{i:02}.iso"),
                })
                .collect(),
            ..VmConfig::default()
        };
        let mut d = IsoDialog::new(cfg, false);
        d.err = "attached img14.iso in config, but hot-plug failed (takes effect on next start):\nQEMU: nope".into();
        h.press(&mut d, ch('G'));
        // 80x24 and 120x40 terminals, and the dashboard's right column at
        // those sizes.
        for (w, hgt) in [(80, 24), (120, 40), (50, 22), (80, 38)] {
            let screen = h.render(&mut d, w, hgt);
            assert!(
                screen.iter().any(|l| l.contains("▸ /isos/img14.iso")),
                "{w}x{hgt}: the cursor row is in view:\n{}",
                screen.join("\n")
            );
            for want in ["✗ attached img14.iso in config, but hot-plug", "QEMU: nope"] {
                assert!(
                    screen_contains(&screen, want),
                    "{w}x{hgt}: the outcome lacks {want:?}:\n{}",
                    screen.join("\n")
                );
            }
        }
        // Not all fit at 80x24: the first ones scrolled away, and come back
        // with the cursor.
        let screen = h.render(&mut d, 80, 24);
        assert!(
            !screen_contains(&screen, "img00.iso"),
            "{}",
            screen.join("\n")
        );
        assert!(
            screen
                .iter()
                .any(|l| l.contains("↑ ") && l.contains("–15 of 15")),
            "the list says it is cut:\n{}",
            screen.join("\n")
        );
        h.press(&mut d, ch('g'));
        let screen = h.render(&mut d, 80, 24);
        assert!(
            screen.iter().any(|l| l.contains("▸ /isos/img00.iso")),
            "{}",
            screen.join("\n")
        );
        assert!(
            !screen_contains(&screen, "img14.iso  "),
            "{}",
            screen.join("\n")
        );
        assert!(
            screen_contains(&screen, "QEMU: nope"),
            "{}",
            screen.join("\n")
        );
        // The spinner of a change in flight stays in view too.
        d.busy = Some("detaching img00.iso…".into());
        h.press(&mut d, ch('G'));
        let screen = h.render(&mut d, 80, 24);
        assert!(
            screen_contains(&screen, "detaching img00.iso…")
                && screen_contains(&screen, "▸ /isos/img14.iso"),
            "{}",
            screen.join("\n")
        );
    }

    #[test]
    fn boot_iso_and_usb_image_states_line_up() {
        let h = Harness::new();
        let isos = h.dir.path().join("iso");
        fs::create_dir_all(&isos).unwrap();
        let present = write_image(&isos.join("virtio-win.iso"), 3 << 20);
        let path = |name: &str| isos.join(name).to_string_lossy().into_owned();
        let cfg = VmConfig {
            name: "a".into(),
            cdrom_path: path("debian-12.3.0-amd64-netinst.iso"),
            usb_images: vec![
                UsbImage { path: present },
                UsbImage {
                    path: path("old-drivers.iso"),
                },
            ],
            ..VmConfig::default()
        };
        let mut d = IsoDialog::new(cfg, false);
        for (w, hgt) in [(80, 24), (120, 40), (50, 22), (80, 38)] {
            let screen = h.render(&mut d, w, hgt);
            let cols: Vec<usize> = screen
                .iter()
                .filter_map(|l| char_col(l, "✗ not found").or_else(|| char_col(l, "● 3 MiB")))
                .collect();
            assert_eq!(cols.len(), 3, "{w}x{hgt}:\n{}", screen.join("\n"));
            assert!(
                cols.iter().all(|&c| c == cols[0]),
                "{w}x{hgt}: the states start at {cols:?}:\n{}",
                screen.join("\n")
            );
            for name in ["netinst.iso  ", "virtio-win.iso  ", "old-drivers.iso  "] {
                assert!(
                    screen_contains(&screen, name),
                    "{w}x{hgt}: lacks {name:?}:\n{}",
                    screen.join("\n")
                );
            }
        }
    }

    #[test]
    fn a_pick_waits_for_the_change_in_flight() {
        let h = Harness::new();
        isolate_config(&h);
        let isos = h.dir.path().join("isos");
        fs::create_dir_all(&isos).unwrap();
        let a = write_image(&isos.join("a.iso"), 1 << 20);
        let b = write_image(&isos.join("b.iso"), 1 << 20);
        let cfg = VmConfig {
            name: "t".into(),
            cpu: 1,
            ram: 128,
            usb_images: vec![UsbImage { path: a }],
            ..VmConfig::default()
        };
        save_vm(h.mgr.storage(), &cfg);
        let mut d = IsoDialog::new(cfg, false);

        // Detach a.iso; while that is in flight the picker opens, but its
        // pick waits: started from the config the detach has not updated
        // yet, the attach would put a.iso back in vm.yaml.
        h.press(&mut d, ch('d'));
        assert_eq!(d.busy(), Some("detaching a.iso…".to_string()));
        h.press(&mut d, ch('a'));
        assert_eq!(d.prompt(), Some(Prompt::UsbImage));
        assert!(pick_new(&h, &mut d, &b).is_empty());
        assert_eq!(d.prompt(), Some(Prompt::UsbImage), "the picker stays open");
        assert_eq!(
            d.picker().unwrap().error(),
            "still detaching a.iso… — press Enter again when that is done"
        );
        assert_eq!(d.busy(), Some("detaching a.iso…".to_string()));
        let screen = h.render(&mut d, 80, 24);
        assert!(
            screen_contains(&screen, "press Enter again"),
            "{}",
            screen.join("\n")
        );
        apply(&h, &mut d); // the detach, and nothing else, was spawned
        assert!(image_paths(&d).is_empty());

        // Once it has landed the same Enter goes through, from the config
        // the detach left.
        h.press(&mut d, key(KeyCode::Enter));
        assert_eq!(d.prompt(), None);
        apply(&h, &mut d);
        assert_eq!(image_paths(&d), std::slice::from_ref(&b));
        let saved = load_config(h.mgr.storage(), "t").unwrap();
        let saved: Vec<&str> = saved.usb_images.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(saved, [b.as_str()], "the detach is not undone");

        // The boot ISO waits the same way.
        d.busy = Some("x".into());
        h.press(&mut d, ch('c'));
        pick_new(&h, &mut d, &b);
        assert_eq!(d.prompt(), Some(Prompt::BootIso));
        assert!(d.picker().unwrap().error().starts_with("still x"));
        assert!(h.next_result(Duration::from_millis(200)).is_none());
    }

    #[test]
    fn narrow_panes_wrap_and_the_picker_has_a_footer() {
        let h = Harness::new();
        let mut d = IsoDialog::new(
            VmConfig {
                name: "n".into(),
                ..VmConfig::default()
            },
            true,
        );
        // The right column of an 80x24 dashboard: the banner wraps instead
        // of being cut.
        let screen = h.render(&mut d, 50, 22);
        for want in [
            "● running — changes are applied in the guest",
            "  right away",
            "No images attached. Press a to attach one.",
        ] {
            assert!(
                screen_contains(&screen, want),
                "lacks {want:?}:\n{}",
                screen.join("\n")
            );
        }
        let screen = h.render(&mut d, 40, 22);
        for want in ["No images attached. Press a to", "  attach one."] {
            assert!(
                screen_contains(&screen, want),
                "lacks {want:?}:\n{}",
                screen.join("\n")
            );
        }
        let screen = h.render(&mut d, 80, 24);
        assert!(
            screen_contains(
                &screen,
                "● running — changes are applied in the guest right away"
            ),
            "{}",
            screen.join("\n")
        );
        // The picker popup names its keys along its bottom border.
        h.press(&mut d, ch('a'));
        for (w, hgt) in [(80, 24), (120, 40)] {
            let screen = h.render(&mut d, w, hgt);
            assert!(
                screen
                    .iter()
                    .any(|l| l.contains("─ Enter attach  ↑/↓ move  d forget  Esc cancel ╯")),
                "{w}x{hgt}:\n{}",
                screen.join("\n")
            );
        }
        h.press(&mut d, key(KeyCode::Esc));
        h.press(&mut d, ch('c'));
        let screen = h.render(&mut d, 80, 24);
        assert!(
            screen
                .iter()
                .any(|l| l.contains("─ Enter insert  ↑/↓ move  d forget  Esc cancel ╯")),
            "{}",
            screen.join("\n")
        );
    }

    /// While a change is in flight the dialog stays open, so quitting still
    /// waits for it: Esc, q and h do nothing, the spinner says why, and the
    /// hints offer Ctrl-c instead of the way back.
    #[test]
    fn close_keys_wait_for_the_change_in_flight() {
        let h = Harness::new();
        let mut d = IsoDialog::new(
            VmConfig {
                name: "z".into(),
                ..VmConfig::default()
            },
            false,
        );
        d.busy = Some("attaching x.iso…".into());
        for k in [key(KeyCode::Esc), ch('q'), ch('h')] {
            assert!(h.press(&mut d, k).is_empty(), "{k:?} closed the dialog");
        }
        let screen = h.render(&mut d, 100, 30);
        assert!(
            screen_contains(&screen, " attaching x.iso…"),
            "{}",
            screen.join("\n")
        );
        let hints = d.key_hints();
        assert_eq!(
            hints.last(),
            Some(&("Ctrl-c".to_string(), "quit".to_string()))
        );
        assert!(!hints.iter().any(|(k, _)| k.contains("Esc")), "{hints:?}");

        d.busy = None;
        for k in [key(KeyCode::Esc), ch('q'), ch('h')] {
            let acts = h.press(&mut d, k);
            assert!(
                matches!(acts.as_slice(), [Action::Close]),
                "{k:?}: {acts:?}"
            );
        }
        assert_eq!(
            d.key_hints().last(),
            Some(&("q/Esc".to_string(), "back".to_string()))
        );
    }
}
