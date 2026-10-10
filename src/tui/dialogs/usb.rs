//! The USB passthrough picker: the host's devices with the ones passed
//! through to the VM marked, attach/detach with hot-plug on a running VM,
//! adding a device by ID, and the udev rule that grants access to a device
//! the user cannot open.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use super::super::actions::copy_to_clipboard;
use super::super::events::TaskResult;
use super::super::forms::common::form_block;
use super::super::panel::{Ctx, Panel};
use super::super::theme::Theme;
use super::super::widgets::{
    pad_right, shell_lines, spinner_frame, window_note, wrap_text, Cursor, TextInput,
};
use super::isopicker::{error_lines, flow};
use crate::vm::{
    self, match_usb, udev_rule_command, udev_rule_command_words, HostUsbDevice, UsbDevice, VmConfig,
};

/// The name column's width.
pub const USB_NAME_WIDTH: usize = 32;

/// The label in front of the add-by-ID input.
const ID_LABEL: &str = "Vendor:Product ID  ";
/// The add-by-ID input's width: `vvvv:pppp` and a cursor, with some air.
const ID_INPUT_WIDTH: u16 = 12;
/// The help after the add-by-ID input.
const ID_HELP: &str = "as shown by lsusb, e.g. 046d:085c";
/// The marker after a device the user cannot open.
const NO_ACCESS: &str = "  ✗ no access";

/// One picker line: a connected host device, a configured entry whose
/// device is not connected, or both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UsbRow {
    /// Index into the host devices; `None` when not connected.
    host: Option<usize>,
    /// Index into `cfg.usb_devices`; `None` when not passed through.
    cfg_idx: Option<usize>,
}

/// The USB picker (internal/tui/usb.go). Each toggle is saved to `vm.yaml`
/// immediately and, for a running VM, hot-plugged.
pub struct UsbDialog {
    cfg: VmConfig,
    /// The last scan.
    host_devs: Vec<HostUsbDevice>,
    /// Derived from the config and the scan by [`UsbDialog::rebuild_rows`].
    rows: Vec<UsbRow>,
    cursor: Cursor,
    running: bool,
    /// An attach/detach in flight, as the status bar names it.
    busy: Option<String>,
    /// Why the host could not be scanned, `""` when it could.
    scan_err: String,
    err: String,
    notice: String,
    /// The manual vendor:product entry is active.
    adding: bool,
    input: TextInput,
}

impl UsbDialog {
    /// A picker for `cfg`; host devices are scanned in `init`. `running` is
    /// the VM's state when it opened; the scan refreshes it.
    pub fn new(cfg: VmConfig, running: bool) -> Self {
        let mut d = UsbDialog {
            cfg,
            host_devs: Vec::new(),
            rows: Vec::new(),
            cursor: Cursor::default(),
            running,
            busy: None,
            scan_err: String::new(),
            err: String::new(),
            notice: String::new(),
            adding: false,
            input: TextInput::new()
                .with_placeholder("046d:085c")
                .with_char_limit(9),
        };
        d.rebuild_rows();
        d
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

    /// Whether the add-by-ID input is open.
    pub fn adding(&self) -> bool {
        self.adding
    }

    /// The row under the cursor.
    pub fn cursor(&self) -> usize {
        self.cursor.index
    }

    /// The rows as (host index, config index) pairs, in display order.
    pub fn rows(&self) -> Vec<(Option<usize>, Option<usize>)> {
        self.rows.iter().map(|r| (r.host, r.cfg_idx)).collect()
    }

    /// Lists every connected host device (marking those passed through)
    /// followed by the configured devices that are not connected. A host
    /// device is told from the others by its port and ID, since the match
    /// hands back a copy of it.
    fn rebuild_rows(&mut self) {
        let states = match_usb(&self.cfg.usb_devices, &self.host_devs);
        let mut rows = Vec::with_capacity(self.host_devs.len() + states.len());
        for (j, h) in self.host_devs.iter().enumerate() {
            let cfg_idx = states.iter().position(|s| {
                s.host.as_ref().is_some_and(|sh| {
                    sh.port == h.port
                        && sh.vendor_id == h.vendor_id
                        && sh.product_id == h.product_id
                })
            });
            rows.push(UsbRow {
                host: Some(j),
                cfg_idx,
            });
        }
        for (i, s) in states.iter().enumerate() {
            if s.host.is_none() {
                rows.push(UsbRow {
                    host: None,
                    cfg_idx: Some(i),
                });
            }
        }
        self.rows = rows;
        self.cursor.clamp(self.rows.len());
    }

    /// How many connected devices carry `id` (the device itself included).
    fn count_id(&self, id: &str) -> usize {
        self.host_devs.iter().filter(|h| h.id() == id).count()
    }

    /// The host device under the cursor, if the row has one.
    fn host_at_cursor(&self) -> Option<&HostUsbDevice> {
        self.rows
            .get(self.cursor.index)
            .and_then(|r| r.host)
            .and_then(|j| self.host_devs.get(j))
    }

    /// The device under the cursor when it is connected but not openable,
    /// i.e. when there is a udev rule to offer.
    fn fix_device(&self) -> Option<UsbDevice> {
        let h = self.host_at_cursor().filter(|h| !h.writable)?;
        Some(UsbDevice {
            vendor_id: h.vendor_id.clone(),
            product_id: h.product_id.clone(),
            name: h.label(),
            port: String::new(),
        })
    }

    // --- tasks ---

    /// Scans the host and the VM's state in the background.
    fn spawn_scan(&self, ctx: &mut Ctx) {
        let storage = ctx.storage_buf();
        let name = self.cfg.name.clone();
        ctx.spawn(move || {
            let (devs, err) = match vm::list_host_usb_devices() {
                Ok(devs) => (devs, None),
                Err(e) => (Vec::new(), Some(format!("{e:#}"))),
            };
            let status = vm::status(&storage, &name).unwrap_or_default();
            TaskResult::UsbScanned { devs, err, status }
        });
    }

    /// Marks a change as in flight: the spinner label, no error, no notice.
    fn start(&mut self, label: String) {
        self.busy = Some(label);
        self.err.clear();
        self.notice.clear();
    }

    /// Passes `dev` through: saved first, hot-plugged when running.
    fn attach(&mut self, dev: UsbDevice, ctx: &mut Ctx) {
        let mut cfg = self.cfg.clone();
        cfg.usb_devices.push(dev.clone());
        let idx = cfg.usb_devices.len() - 1;
        let name = dev.label();
        self.start(format!("attaching {name}…"));
        let storage = ctx.storage_buf();
        let running = self.running;
        ctx.spawn(move || {
            if let Err(e) = vm::save_config(&storage, &cfg) {
                return failed(None, format!("save VM config: {e:#}"));
            }
            if !running {
                return applied(cfg, format!("attached {name} — takes effect on next start"));
            }
            match vm::usb_hotplug(&storage, &cfg, idx) {
                Err(e) => failed(
                    Some(cfg),
                    format!("attached {name} in config, but hot-plug failed (takes effect on next start):\n{e:#}"),
                ),
                Ok(()) => applied(cfg, format!("attached {name} (hot-plugged)")),
            }
        });
    }

    /// Takes the device at `idx` back. The hot-unplug is named from the
    /// config as it was before the removal, since duplicate IDs get
    /// positional suffixes.
    fn detach(&mut self, idx: usize, ctx: &mut Ctx) {
        let old = self.cfg.clone();
        let Some(dev) = old.usb_devices.get(idx).cloned() else {
            return;
        };
        let mut cfg = old.clone();
        cfg.usb_devices.remove(idx);
        let name = dev.label();
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
            match vm::usb_hotunplug(&storage, &old, idx) {
                Err(e) => failed(
                    Some(cfg),
                    format!("detached {name} in config, but hot-unplug failed (takes effect on next start):\n{e:#}"),
                ),
                Ok(()) => applied(cfg, format!("detached {name} (hot-unplugged)")),
            }
        });
    }

    /// Copies the one-line udev command for `dev` to the clipboard.
    fn copy_udev_command(&self, dev: UsbDevice, ctx: &mut Ctx) {
        let command = udev_rule_command(std::slice::from_ref(&dev));
        ctx.spawn(move || TaskResult::Clipboard {
            err: copy_to_clipboard(&command).err().map(|e| format!("{e:#}")),
        });
    }

    // --- keys ---

    /// Space/Enter: attaches or detaches the device under the cursor.
    fn toggle(&mut self, ctx: &mut Ctx) {
        if self.busy.is_some() {
            return;
        }
        let Some(row) = self.rows.get(self.cursor.index).copied() else {
            return;
        };
        if let Some(i) = row.cfg_idx {
            self.detach(i, ctx);
            return;
        }
        let Some(h) = row.host.and_then(|j| self.host_devs.get(j)) else {
            return;
        };
        let mut dev = UsbDevice {
            vendor_id: h.vendor_id.clone(),
            product_id: h.product_id.clone(),
            name: h.label(),
            port: String::new(),
        };
        if self.count_id(&h.id()) > 1 {
            dev.port.clone_from(&h.port); // identical devices present: pin to this one
        }
        self.attach(dev, ctx);
    }

    /// The keys of the normal mode.
    fn normal_key(&mut self, key: KeyEvent, ctx: &mut Ctx) {
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let n = self.rows.len();
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
            KeyCode::Enter => self.toggle(ctx),
            KeyCode::Char(' ') if plain => self.toggle(ctx),
            KeyCode::Char('a') if plain => {
                self.adding = true;
                self.err.clear();
                self.notice.clear();
                self.input.set_value("");
                self.input.focus();
            }
            KeyCode::Char('r') if plain => {
                self.err.clear();
                self.notice.clear();
                self.spawn_scan(ctx);
            }
            KeyCode::Char('y') if plain => {
                if let Some(dev) = self.fix_device() {
                    self.copy_udev_command(dev, ctx);
                }
            }
            _ => {}
        }
    }

    /// The keys while the add-by-ID input is open. A refused Enter keeps
    /// the input open with the reason; Esc leaves it on screen. Enter waits
    /// while a change is in flight (the spinner shows under the input): a
    /// second save started from the config the first has not updated yet
    /// would undo it.
    fn add_key(&mut self, key: KeyEvent, ctx: &mut Ctx) {
        match key.code {
            KeyCode::Esc => {
                self.adding = false;
                self.input.blur();
            }
            KeyCode::Enter if self.busy.is_some() => {}
            KeyCode::Enter => {
                let mut dev = match vm::parse_usb_id(&self.input.value()) {
                    Ok(dev) => dev,
                    Err(e) => {
                        self.err = format!("{e:#}");
                        return;
                    }
                };
                if self
                    .cfg
                    .usb_devices
                    .iter()
                    .any(|d| d.id() == dev.id() && d.port.is_empty())
                {
                    self.err = format!("{} is already passed through to this VM", dev.id());
                    return;
                }
                if let Some(h) = self.host_devs.iter().find(|h| dev.matches(h)) {
                    dev.name = h.label();
                }
                self.adding = false;
                self.input.blur();
                self.attach(dev, ctx);
            }
            _ => {
                self.input.handle_key(key);
            }
        }
    }

    // --- drawing ---

    /// What a row shows about where the device is (`port 3-2.2.3`,
    /// `not connected (port 3-9)`) and whether it is connected but not
    /// openable; `None` for a row with nothing behind it.
    fn place_of(&self, row: UsbRow) -> Option<(String, bool)> {
        if let Some(h) = row.host.and_then(|j| self.host_devs.get(j)) {
            Some((format!("port {}", h.port), !h.writable))
        } else {
            let d = row.cfg_idx.and_then(|i| self.cfg.usb_devices.get(i))?;
            let place = if d.port.is_empty() {
                "not connected".to_string()
            } else {
                format!("not connected (port {})", d.port)
            };
            Some((place, false))
        }
    }

    /// One device row: the box, the ID, the name column, where it is and
    /// whether it can be opened. On a row with the no-access marker the
    /// place is padded to `place_w`, so the markers line up.
    fn row_line(
        &self,
        row: UsbRow,
        focused: bool,
        name_w: usize,
        place_w: usize,
        theme: &Theme,
    ) -> Line<'static> {
        let (id, name) = if let Some(h) = row.host.and_then(|j| self.host_devs.get(j)) {
            (h.id(), h.label())
        } else if let Some(d) = row.cfg_idx.and_then(|i| self.cfg.usb_devices.get(i)) {
            (d.id(), d.label())
        } else {
            return Line::raw("");
        };
        let Some((place, no_access)) = self.place_of(row) else {
            return Line::raw("");
        };
        let marker = if focused {
            Span::styled("▸ ", theme.label)
        } else {
            Span::raw("  ")
        };
        let tick = if row.cfg_idx.is_some() {
            Span::styled("[x]", theme.success)
        } else {
            Span::styled("[ ]", theme.help)
        };
        let name_style = if focused { theme.label } else { theme.normal };
        let mut spans = vec![
            marker,
            tick,
            Span::raw(" "),
            Span::styled(id, theme.normal),
            Span::raw("  "),
        ];
        if name_w > 0 {
            spans.push(Span::styled(pad_right(&name, name_w), name_style));
            spans.push(Span::raw("  "));
        }
        if no_access {
            spans.push(Span::styled(pad_right(&place, place_w), theme.help));
            spans.push(Span::styled(NO_ACCESS, theme.error));
        } else {
            spans.push(Span::styled(place, theme.help));
        }
        Line::from(spans)
    }

    /// The place column in front of the no-access markers: as wide as the
    /// widest place among the rows that carry one. (Only those rows are
    /// padded, so a long `not connected (port …)` elsewhere does not push
    /// the markers towards the edge.)
    fn place_width(&self) -> usize {
        self.rows
            .iter()
            .filter_map(|&r| self.place_of(r))
            .filter(|(_, no_access)| *no_access)
            .map(|(place, _)| place.chars().count())
            .max()
            .unwrap_or(0)
    }

    /// The name column: 32 cells, narrower when the pane cannot take the
    /// widest row at that. It gives way before the place and the
    /// no-access marker do, down to nothing at all.
    fn name_width(&self, width: usize) -> usize {
        let place_w = self.place_width();
        let tail = self
            .rows
            .iter()
            .filter_map(|&r| self.place_of(r))
            .map(|(place, no_access)| {
                if no_access {
                    place_w + NO_ACCESS.chars().count()
                } else {
                    place.chars().count()
                }
            })
            .max()
            .unwrap_or(0);
        // marker 2 + box 3 + 1 + id 9 + 2 + name + 2 + tail; a column of
        // one cell would hold only the ellipsis, so it goes too.
        match width.saturating_sub(19 + tail) {
            0 | 1 => 0,
            room => room.min(USB_NAME_WIDTH),
        }
    }

    /// Lays the panel out and draws it; the add-by-ID input is drawn on its
    /// own line so the terminal cursor lands in it.
    fn render_body(&mut self, frame: &mut Frame, inner: Rect, theme: &Theme, tick: u64) {
        let width = inner.width as usize;
        let mut head = if self.running {
            flow(
                vec![
                    Span::styled("● running", theme.running),
                    Span::styled(" ", theme.help),
                ],
                "— attaching or detaching hot-plugs the device in the guest",
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
        head.extend(flow(
            Vec::new(),
            "The host cannot use a device while the guest holds it.",
            theme.help,
            width,
        ));
        head.push(Line::raw(""));
        const DEVICES: &str = "Host USB devices";
        const DEVICES_HELP: &str = "[x] = passed through to this VM";
        if DEVICES.len() + 3 + DEVICES_HELP.len() <= width {
            head.push(Line::from(vec![
                Span::styled(DEVICES, theme.label),
                Span::styled(format!("   {DEVICES_HELP}"), theme.help),
            ]));
        } else {
            head.push(Line::styled(DEVICES, theme.label));
            for piece in wrap_text(DEVICES_HELP, width.saturating_sub(2).max(8)) {
                head.push(Line::styled(format!("  {piece}"), theme.help));
            }
        }
        head.push(Line::raw(""));
        if !self.scan_err.is_empty() {
            let wrapped = wrap_text(&self.scan_err, width.saturating_sub(2).max(8));
            head.extend(error_lines(&wrapped.join("\n"), theme));
            head.extend(flow(
                Vec::new(),
                "Devices can still be added by ID with a.",
                theme.help,
                width,
            ));
            head.push(Line::raw(""));
        }
        if self.rows.is_empty() {
            head.extend(flow(
                Vec::new(),
                "No USB devices found. Plug one in and press r to rescan, or press a to add one by ID.",
                theme.help,
                width,
            ));
        }

        let mut tail: Vec<Line<'static>> = vec![Line::raw("")];
        let mut input_line: Option<usize> = None; // index into tail
        if self.adding {
            input_line = Some(tail.len());
            let input_end = ID_LABEL.len() + ID_INPUT_WIDTH as usize;
            if input_end + 3 + ID_HELP.len() <= width {
                tail.push(Line::from(vec![
                    Span::styled(ID_LABEL, theme.label),
                    Span::raw(" ".repeat(ID_INPUT_WIDTH as usize)),
                    Span::styled(format!("   {ID_HELP}"), theme.help),
                ]));
            } else {
                // The help goes under the input when the line cannot take it.
                tail.push(Line::styled(ID_LABEL, theme.label));
                for piece in wrap_text(ID_HELP, width.saturating_sub(2).max(8)) {
                    tail.push(Line::styled(format!("  {piece}"), theme.help));
                }
            }
            tail.push(Line::raw(""));
        } else if let Some(dev) = self.fix_device() {
            let node = self
                .host_at_cursor()
                .map(|h| h.dev_node.clone())
                .unwrap_or_default();
            let what = format!("no write access to {node} — QEMU cannot open it");
            let wrapped = wrap_text(&what, width.saturating_sub(2).max(8));
            tail.extend(error_lines(&wrapped.join("\n"), theme));
            tail.extend(flow(
                Vec::new(),
                "Run this to grant access to your user (y copies it), then press r to rescan:",
                theme.help,
                width,
            ));
            // Plain text, broken only between shell words, so a mouse
            // selection of the block pastes as one working command.
            for line in shell_lines(&udev_rule_command_words(&[dev]), width.saturating_sub(8)) {
                tail.push(Line::styled(format!("    {line}"), theme.normal));
            }
            tail.push(Line::raw(""));
        }
        if let Some(label) = &self.busy {
            tail.push(Line::from(vec![
                Span::styled(spinner_frame(tick), theme.spinner),
                Span::styled(format!(" {label}"), theme.normal),
            ]));
        } else if !self.err.is_empty() {
            let wrapped: Vec<String> = self
                .err
                .lines()
                .flat_map(|raw| wrap_text(raw, width.saturating_sub(2).max(8)))
                .collect();
            tail.extend(error_lines(&wrapped.join("\n"), theme));
        } else if !self.notice.is_empty() {
            tail.push(Line::styled(format!("✓ {}", self.notice), theme.success));
        }

        // A short pane keeps the cursor row and the whole tail, so the udev
        // command stays selectable: the head's blank lines go first, then a
        // blank line closing the tail.
        if head.len() + 1 + tail.len() > inner.height as usize {
            head.retain(|l| l.width() > 0);
            if tail.len() > 1 && tail.last().is_some_and(|l| l.width() == 0) {
                tail.pop();
            }
        }

        // The rows get what is left, scrolled to keep the cursor in view;
        // when they do not all fit, a line under them says which are shown.
        let avail = (inner.height as usize).saturating_sub(head.len() + tail.len());
        let clipped = self.rows.len() > avail.max(1);
        let rows_h = avail.saturating_sub(usize::from(clipped)).max(1);
        let range = self.cursor.window(self.rows.len(), rows_h);
        let name_w = self.name_width(width);
        let place_w = self.place_width();
        let mut lines = head;
        for i in range.clone() {
            let focused = i == self.cursor.index;
            lines.push(self.row_line(self.rows[i], focused, name_w, place_w, theme));
        }
        if clipped {
            lines.push(Line::styled(
                window_note(range, self.rows.len()),
                theme.help,
            ));
        }
        let tail_start = lines.len();
        lines.extend(tail);
        frame.render_widget(Paragraph::new(lines), inner);

        if let Some(i) = input_line {
            let y = (tail_start + i) as u16;
            if y < inner.height {
                let x = inner.x + ID_LABEL.len() as u16;
                let w = ID_INPUT_WIDTH.min(inner.width.saturating_sub(ID_LABEL.len() as u16));
                if w > 0 {
                    self.input
                        .render(frame, Rect::new(x, inner.y + y, w, 1), theme);
                }
            }
        }
    }
}

/// A saved change with its success text.
fn applied(cfg: VmConfig, notice: String) -> TaskResult {
    TaskResult::UsbApplied {
        cfg: Some(Box::new(cfg)),
        notice,
        err: None,
    }
}

/// A failed change; `cfg` is the saved config when the save went through.
fn failed(cfg: Option<VmConfig>, err: String) -> TaskResult {
    TaskResult::UsbApplied {
        cfg: cfg.map(Box::new),
        notice: String::new(),
        err: Some(err),
    }
}

impl Panel for UsbDialog {
    fn title(&self) -> String {
        format!("USB Passthrough: {}", self.cfg.name)
    }

    fn init(&mut self, ctx: &mut Ctx) {
        self.spawn_scan(ctx);
    }

    fn handle_key(&mut self, key: KeyEvent, ctx: &mut Ctx) {
        if self.adding {
            self.add_key(key, ctx);
        } else {
            self.normal_key(key, ctx);
        }
    }

    fn handle_paste(&mut self, text: &str, _ctx: &mut Ctx) {
        if self.adding {
            self.input.insert_str(text);
        }
    }

    fn on_task(&mut self, result: TaskResult, ctx: &mut Ctx) {
        match result {
            TaskResult::UsbScanned { devs, err, status } => {
                self.host_devs = devs;
                self.scan_err = err.unwrap_or_default();
                self.running = status.running();
                self.rebuild_rows();
            }
            TaskResult::UsbApplied { cfg, notice, err } => {
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
                self.rebuild_rows();
                self.spawn_scan(ctx);
            }
            TaskResult::Clipboard { err } => match err {
                Some(err) => {
                    self.err = err;
                    self.notice.clear();
                }
                None => {
                    self.notice =
                        "copied the udev command — run it in a shell, then press r to rescan"
                            .to_string();
                    self.err.clear();
                }
            },
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
        if area.width == 0 || area.height == 0 {
            return;
        }
        let inner = form_block(frame, area, &self.title(), theme);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        self.render_body(frame, inner, theme, tick);
    }

    fn key_hints(&self) -> Vec<(String, String)> {
        // While a change is in flight the dialog does not close; Ctrl-c
        // quits once it is done (or at once, on a second press).
        let back = if self.busy.is_some() {
            ("Ctrl-c", "quit")
        } else {
            ("q/Esc", "back")
        };
        let pairs: &[(&str, &str)] = if self.adding {
            &[("Enter", "attach"), ("Esc", "cancel")]
        } else if self.fix_device().is_some() {
            &[
                ("Space/Enter", "attach/detach"),
                ("y", "copy udev command"),
                ("a", "add by ID"),
                ("r", "rescan"),
                ("j/k", "move"),
                back,
            ]
        } else {
            &[
                ("Space/Enter", "attach/detach"),
                ("a", "add by ID"),
                ("r", "rescan"),
                ("j/k", "move"),
                back,
            ]
        };
        pairs
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
    use std::time::Duration;

    use super::super::isopicker::tests::save_vm;
    use super::*;
    use crate::tui::panel::Action;
    use crate::tui::testutil::*;
    use crate::vm::{load_config, ProcessInfo, VmStatus};

    fn test_host_devs() -> Vec<HostUsbDevice> {
        vec![
            HostUsbDevice {
                vendor_id: "046d".into(),
                product_id: "085c".into(),
                product: "C922 Pro Stream Webcam".into(),
                port: "3-2.2.2".into(),
                dev_node: "/dev/bus/usb/003/016".into(),
                writable: true,
                ..HostUsbDevice::default()
            },
            HostUsbDevice {
                vendor_id: "0781".into(),
                product_id: "5583".into(),
                manufacturer: "SanDisk".into(),
                product: "Ultra Fit".into(),
                port: "3-2.3".into(),
                dev_node: "/dev/bus/usb/003/004".into(),
                writable: true,
                ..HostUsbDevice::default()
            },
            HostUsbDevice {
                vendor_id: "0781".into(),
                product_id: "5583".into(),
                manufacturer: "SanDisk".into(),
                product: "Ultra Fit".into(),
                port: "3-2.4".into(),
                dev_node: "/dev/bus/usb/003/005".into(),
                writable: false,
                ..HostUsbDevice::default()
            },
        ]
    }

    fn scanned(devs: Vec<HostUsbDevice>, err: Option<&str>, running: bool) -> TaskResult {
        TaskResult::UsbScanned {
            devs,
            err: err.map(str::to_string),
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

    /// Takes the next task result, which must be a successful apply, feeds
    /// it to the dialog and returns the actions. The apply triggers a
    /// rescan of the real host; that result is dropped so the hand-made
    /// devices stay.
    fn apply(h: &Harness, d: &mut UsbDialog) -> Vec<Action> {
        let r = h
            .next_result(Duration::from_secs(10))
            .expect("an apply result");
        assert!(
            matches!(&r, TaskResult::UsbApplied { err: None, .. }),
            "apply: {r:?}"
        );
        let mut ctx = h.ctx();
        d.on_task(r, &mut ctx);
        h.next_result(Duration::from_secs(10))
            .expect("a rescan result");
        ctx.actions
    }

    fn ids(d: &UsbDialog) -> Vec<String> {
        d.cfg.usb_devices.iter().map(UsbDevice::id).collect()
    }

    #[test]
    fn attach_detach_and_add_by_id() {
        let h = Harness::new();
        let cfg = VmConfig {
            name: "t".into(),
            cpu: 1,
            ram: 128,
            usb_devices: vec![UsbDevice {
                vendor_id: "dead".into(),
                product_id: "beef".into(),
                name: "Old Dongle".into(),
                port: String::new(),
            }],
            ..VmConfig::default()
        };
        save_vm(h.mgr.storage(), &cfg);

        let mut d = UsbDialog::new(cfg, false);
        assert_eq!(d.title(), "USB Passthrough: t");
        assert_eq!(
            d.rows(),
            [(None, Some(0))],
            "before the scan only the configured device is there"
        );
        d.on_task(scanned(test_host_devs(), None, false), &mut h.ctx());
        assert_eq!(
            d.rows(),
            [
                (Some(0), None),
                (Some(1), None),
                (Some(2), None),
                (None, Some(0))
            ]
        );
        let screen = h.render(&mut d, 100, 40);
        for want in [
            "USB Passthrough: t",
            "● stopped — changes take effect when the VM is started",
            "The host cannot use a device while the guest holds it.",
            "Host USB devices   [x] = passed through to this VM",
            "▸ [ ] 046d:085c  C922 Pro Stream Webcam            port 3-2.2.2",
            "  [ ] 0781:5583  SanDisk Ultra Fit                 port 3-2.3",
            "  [ ] 0781:5583  SanDisk Ultra Fit                 port 3-2.4  ✗ no access",
            "  [x] dead:beef  Old Dongle                        not connected",
        ] {
            assert!(
                screen_contains(&screen, want),
                "screen lacks {want:?}:\n{}",
                screen.join("\n")
            );
        }
        assert_eq!(
            d.key_hints()[0],
            ("Space/Enter".to_string(), "attach/detach".to_string())
        );
        assert!(!d.key_hints().iter().any(|(k, _)| k == "y"));

        // Attach the webcam (cursor at row 0).
        h.press(&mut d, ch(' '));
        assert_eq!(
            d.busy(),
            Some("attaching C922 Pro Stream Webcam…".to_string())
        );
        let screen = h.render(&mut d, 100, 40);
        assert!(
            screen_contains(&screen, "attaching C922 Pro Stream Webcam…"),
            "{}",
            screen.join("\n")
        );
        assert!(apply(&h, &mut d).is_empty(), "the dialog stays open");
        assert_eq!(ids(&d), ["dead:beef", "046d:085c"]);
        assert_eq!(d.cfg.usb_devices[1].port, "");
        assert_eq!(d.cfg.usb_devices[1].name, "C922 Pro Stream Webcam");
        assert_eq!(
            load_config(h.mgr.storage(), "t").unwrap().usb_devices.len(),
            2
        );
        assert_eq!(
            d.notice(),
            "attached C922 Pro Stream Webcam — takes effect on next start"
        );
        assert_eq!(
            d.rows(),
            [
                (Some(0), Some(1)),
                (Some(1), None),
                (Some(2), None),
                (None, Some(0))
            ]
        );
        let screen = h.render(&mut d, 100, 40);
        assert!(
            screen_contains(&screen, "▸ [x] 046d:085c  C922 Pro Stream Webcam"),
            "{}",
            screen.join("\n")
        );
        assert!(
            screen_contains(
                &screen,
                "✓ attached C922 Pro Stream Webcam — takes effect on next start"
            ),
            "{}",
            screen.join("\n")
        );

        // Attach the second of two identical sticks: it gets pinned to its port.
        h.press(&mut d, ch('j'));
        h.press(&mut d, ch('j'));
        assert_eq!(d.cursor(), 2);
        h.press(&mut d, key(KeyCode::Enter));
        apply(&h, &mut d);
        let pinned = &d.cfg.usb_devices[2];
        assert_eq!(
            (pinned.id(), pinned.port.as_str(), pinned.name.as_str()),
            ("0781:5583".to_string(), "3-2.4", "SanDisk Ultra Fit")
        );
        assert_eq!(
            d.rows(),
            [
                (Some(0), Some(1)),
                (Some(1), None),
                (Some(2), Some(2)),
                (None, Some(0))
            ]
        );
        let screen = h.render(&mut d, 100, 40);
        for want in [
            "▸ [x] 0781:5583  SanDisk Ultra Fit                 port 3-2.4  ✗ no access",
            "✗ no write access to /dev/bus/usb/003/005 — QEMU cannot open it",
            "Run this to grant access to your user (y copies it), then press r to rescan:",
            r#"'ATTR{idProduct}=="5583",'"#,
        ] {
            assert!(
                screen_contains(&screen, want),
                "screen lacks {want:?}:\n{}",
                screen.join("\n")
            );
        }
        assert!(d
            .key_hints()
            .iter()
            .any(|(k, v)| k == "y" && v == "copy udev command"));
        // The command lines reassemble into the one-liner that is copied.
        // (Screen lines carry the pane border and its one-cell margin.)
        let body = |l: &String| {
            l.trim_matches('│')
                .trim_end()
                .strip_prefix(' ')
                .unwrap_or("")
                .to_string()
        };
        let cmd_lines: Vec<String> = screen
            .iter()
            .skip_while(|l| !l.contains("Run this to grant access"))
            .skip(1)
            .map(body)
            .take_while(|l| l.starts_with("    "))
            .map(|l| l[4..].to_string())
            .collect();
        assert!(cmd_lines.len() > 1, "{}", screen.join("\n"));
        let dev = UsbDevice {
            vendor_id: "0781".into(),
            product_id: "5583".into(),
            ..UsbDevice::default()
        };
        assert_eq!(
            cmd_lines.join("\n").replace(" \\\n", " "),
            udev_rule_command(&[dev])
        );
        assert!(cmd_lines.iter().all(|l| l.len() <= 96 - 8), "{cmd_lines:?}");

        // Detach the old dongle (last row) and check the row disappears.
        h.press(&mut d, ch('G'));
        assert_eq!(d.cursor(), 3);
        h.press(&mut d, ch(' '));
        assert_eq!(d.busy(), Some("detaching Old Dongle…".to_string()));
        apply(&h, &mut d);
        assert_eq!(ids(&d), ["046d:085c", "0781:5583"]);
        assert_eq!(d.rows().len(), 3);
        assert_eq!(d.cursor(), 2, "the cursor is clamped to the rows left");
        assert_eq!(
            d.notice(),
            "detached Old Dongle — takes effect on next start"
        );

        // Manual entry by ID.
        h.press(&mut d, ch('a'));
        assert!(d.adding());
        assert_eq!(
            d.key_hints(),
            [
                ("Enter".to_string(), "attach".to_string()),
                ("Esc".to_string(), "cancel".to_string())
            ]
        );
        let screen = h.render(&mut d, 100, 40);
        // The placeholder sits in the 12-cell input, the help after it.
        assert!(
            screen_contains(
                &screen,
                "Vendor:Product ID  046d:085c      as shown by lsusb, e.g. 046d:085c"
            ),
            "placeholder:\n{}",
            screen.join("\n")
        );
        type_str(&mut d, &h, "1a2b:3c4d");
        let screen = h.render(&mut d, 100, 40);
        assert!(
            screen_contains(&screen, "Vendor:Product ID  1a2b:3c4d"),
            "{}",
            screen.join("\n")
        );
        h.press(&mut d, key(KeyCode::Enter));
        assert!(!d.adding());
        apply(&h, &mut d);
        assert_eq!(ids(&d)[2], "1a2b:3c4d");
        assert_eq!(
            d.cfg.usb_devices[2].name, "",
            "no connected device to take a name from"
        );
        assert_eq!(d.rows()[3], (None, Some(2)));
        h.press(&mut d, ch('a'));
        type_str(&mut d, &h, "1a2b:3c4d");
        h.press(&mut d, key(KeyCode::Enter));
        assert!(d.adding(), "a refused Enter keeps the input open");
        assert_eq!(d.error(), "1a2b:3c4d is already passed through to this VM");
        assert!(h.next_result(Duration::from_millis(200)).is_none());
        let screen = h.render(&mut d, 100, 40);
        assert!(
            screen_contains(&screen, "✗ 1a2b:3c4d is already passed through to this VM"),
            "{}",
            screen.join("\n")
        );
        // j/k type while adding; Esc leaves the error on screen.
        h.press(&mut d, ch('j'));
        h.press(&mut d, key(KeyCode::Esc));
        assert!(!d.adding());
        assert_eq!(d.error(), "1a2b:3c4d is already passed through to this VM");
        // A bad ID is refused with the parser's words.
        h.press(&mut d, ch('a'));
        assert!(d.error().is_empty(), "a clears the error");
        type_str(&mut d, &h, "046d:85c");
        h.press(&mut d, key(KeyCode::Enter));
        assert_eq!(
            d.error(),
            "invalid USB ID \"046d:85c\" — expected vendor:product as 4 hex digits each, e.g. 046d:085c"
        );
        h.press(&mut d, key(KeyCode::Esc));
        // A connected device added by ID takes its name, and the same ID
        // pinned to a port is no duplicate.
        d.cfg.usb_devices.push(UsbDevice {
            vendor_id: "046d".into(),
            product_id: "0001".into(),
            port: "1-1".into(),
            ..UsbDevice::default()
        });
        h.press(&mut d, ch('a'));
        type_str(&mut d, &h, "046D:0001");
        h.press(&mut d, key(KeyCode::Enter));
        apply(&h, &mut d);
        assert_eq!(ids(&d).last().unwrap(), "046d:0001");
        h.press(&mut d, ch('a'));
        let mut ctx = h.ctx();
        d.handle_paste("0781:5583", &mut ctx);
        h.press(&mut d, key(KeyCode::Enter));
        apply(&h, &mut d);
        let added = d.cfg.usb_devices.last().unwrap();
        assert_eq!(
            (added.name.as_str(), added.port.as_str()),
            ("SanDisk Ultra Fit", "")
        );

        // Esc, q and h close the dialog.
        for k in [key(KeyCode::Esc), ch('q'), ch('h')] {
            let acts = h.press(&mut d, k);
            assert!(matches!(acts[..], [Action::Close]), "{acts:?}");
        }
    }

    #[test]
    fn scan_errors_empty_lists_and_running_state() {
        let h = Harness::new();
        let cfg = VmConfig {
            name: "e".into(),
            ..VmConfig::default()
        };
        let mut d = UsbDialog::new(cfg, false);
        let screen = h.render(&mut d, 100, 30);
        assert!(screen_contains(&screen, "No USB devices found. Plug one in and press r to rescan, or press a to add one by ID."), "{}", screen.join("\n"));
        h.press(&mut d, ch(' '));
        h.press(&mut d, key(KeyCode::Enter));
        h.press(&mut d, ch('y'));
        assert!(
            h.next_result(Duration::from_millis(200)).is_none(),
            "nothing to toggle or copy"
        );

        d.on_task(scanned(Vec::new(), Some("list host USB devices (needs Linux sysfs): open /sys/bus/usb/devices: No such file or directory"), true), &mut h.ctx());
        let screen = h.render(&mut d, 120, 30);
        for want in [
            "● running — attaching or detaching hot-plugs the device in the guest",
            "✗ list host USB devices (needs Linux sysfs): open /sys/bus/usb/devices: No such file or directory",
            "Devices can still be added by ID with a.",
            "No USB devices found.",
        ] {
            assert!(screen_contains(&screen, want), "screen lacks {want:?}:\n{}", screen.join("\n"));
        }
        // A later scan that works clears the error.
        d.on_task(scanned(test_host_devs(), None, false), &mut h.ctx());
        let screen = h.render(&mut d, 100, 30);
        assert!(
            !screen_contains(&screen, "needs Linux sysfs"),
            "{}",
            screen.join("\n")
        );
        assert!(
            screen_contains(&screen, "● stopped"),
            "{}",
            screen.join("\n")
        );

        // A configured device pinned to a port that is not there.
        d.cfg.usb_devices.push(UsbDevice {
            vendor_id: "0781".into(),
            product_id: "5583".into(),
            name: "Stick".into(),
            port: "3-9".into(),
        });
        d.on_task(scanned(test_host_devs(), None, false), &mut h.ctx());
        let screen = h.render(&mut d, 100, 30);
        assert!(
            screen_contains(
                &screen,
                "[x] 0781:5583  Stick                             not connected (port 3-9)"
            ),
            "{}",
            screen.join("\n")
        );
        // y on a writable device does nothing; r rescans.
        h.press(&mut d, ch('y'));
        assert!(h.next_result(Duration::from_millis(200)).is_none());
        h.press(&mut d, ch('r'));
        assert!(h.next_result(Duration::from_secs(10)).is_some());
        // The clipboard result becomes a notice, or the error.
        d.on_task(TaskResult::Clipboard { err: None }, &mut h.ctx());
        assert_eq!(
            d.notice(),
            "copied the udev command — run it in a shell, then press r to rescan"
        );
        d.on_task(TaskResult::Clipboard { err: Some("no clipboard tool found — install wl-clipboard (Wayland) or xclip (X11), or select the command with the mouse".into()) }, &mut h.ctx());
        assert!(d.notice().is_empty());
        assert!(d.error().starts_with("no clipboard tool found"));
        // A failed hot-plug keeps the saved config and shows why.
        d.busy = Some("x".into());
        d.on_task(
            TaskResult::UsbApplied {
                cfg: Some(Box::new(VmConfig { name: "e".into(), usb_devices: vec![UsbDevice { vendor_id: "046d".into(), product_id: "085c".into(), ..UsbDevice::default() }], ..VmConfig::default() })),
                notice: String::new(),
                err: Some("attached 046d:085c in config, but hot-plug failed (takes effect on next start):\nQEMU: nope".into()),
            },
            &mut h.ctx(),
        );
        let _ = h.next_result(Duration::from_secs(10)); // the rescan
        assert_eq!(d.busy(), None);
        assert_eq!(ids(&d), ["046d:085c"]);
        let screen = h.render(&mut d, 100, 30);
        assert!(
            screen_contains(
                &screen,
                "✗ attached 046d:085c in config, but hot-plug failed (takes effect on next start):"
            ),
            "{}",
            screen.join("\n")
        );
        assert!(
            screen_contains(&screen, "  QEMU: nope"),
            "{}",
            screen.join("\n")
        );
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
        // Toggling while busy does nothing.
        d.busy = Some("x".into());
        h.press(&mut d, ch(' '));
        assert!(h.next_result(Duration::from_millis(200)).is_none());
        d.busy = None;
        // Tiny and odd sizes never panic, with and without the input.
        for (w, hgt) in [(0, 0), (1, 1), (3, 3), (10, 2), (30, 5), (60, 8)] {
            let _ = h.render(&mut d, w, hgt);
        }
        h.press(&mut d, ch('a'));
        for (w, hgt) in [(0, 0), (1, 1), (3, 3), (10, 2), (30, 5), (60, 8)] {
            let _ = h.render(&mut d, w, hgt);
        }
    }

    #[test]
    fn rows_scroll_to_the_cursor_and_narrow_panes_shrink_the_name() {
        let h = Harness::new();
        let devs: Vec<HostUsbDevice> = (0..20)
            .map(|i| HostUsbDevice {
                vendor_id: "1234".into(),
                product_id: format!("{i:04x}"),
                product: format!("Device number {i} with a rather long name"),
                port: format!("1-{}", i + 1),
                dev_node: format!("/dev/bus/usb/001/{:03}", i + 2),
                writable: true,
                ..HostUsbDevice::default()
            })
            .collect();
        let mut d = UsbDialog::new(
            VmConfig {
                name: "s".into(),
                ..VmConfig::default()
            },
            false,
        );
        d.on_task(scanned(devs, None, false), &mut h.ctx());
        h.press(&mut d, ch('G'));
        let screen = h.render(&mut d, 100, 14);
        assert!(
            screen_contains(&screen, "▸ [ ] 1234:0013"),
            "the last row is in view:\n{}",
            screen.join("\n")
        );
        // The list says it is cut, and where.
        let note = screen
            .iter()
            .position(|l| l.contains("↑ ") && l.contains(" of 20"))
            .unwrap_or_else(|| panic!("no window note:\n{}", screen.join("\n")));
        assert!(screen[note - 1].contains("1234:0013"), "under the rows");
        assert!(screen[note].contains("–20 of 20"), "{}", screen[note]);
        assert!(
            !screen_contains(&screen, "1234:0000"),
            "{}",
            screen.join("\n")
        );
        h.press(&mut d, ch('g'));
        let screen = h.render(&mut d, 100, 14);
        assert!(
            screen_contains(&screen, "▸ [ ] 1234:0000"),
            "{}",
            screen.join("\n")
        );
        assert!(screen_contains(&screen, "↓ 1–"), "{}", screen.join("\n"));
        // Everything in view: no note.
        let screen = h.render(&mut d, 100, 60);
        assert!(!screen_contains(&screen, " of 20"), "{}", screen.join("\n"));
        assert!(
            screen_contains(&screen, "Device number 0 with a rather l…  port 1-1"),
            "name cut at 32:\n{}",
            screen.join("\n")
        );
        let screen = h.render(&mut d, 50, 14);
        assert!(
            screen_contains(&screen, "▸ [ ] 1234:0000  Device number 0 w…  port 1-1"),
            "narrow:\n{}",
            screen.join("\n")
        );
    }

    /// The column, in chars, where `needle` starts in `line`.
    fn char_col(line: &str, needle: &str) -> Option<usize> {
        line.find(needle).map(|b| line[..b].chars().count())
    }

    /// A webcam the user can open, two devices they cannot, and a stick
    /// pinned to a port it is not plugged into.
    fn no_access_dialog(h: &Harness) -> UsbDialog {
        let dev = |vp: (&str, &str), mfr: &str, product: &str, port: &str, node: &str, writable| {
            HostUsbDevice {
                vendor_id: vp.0.into(),
                product_id: vp.1.into(),
                manufacturer: mfr.into(),
                product: product.into(),
                port: port.into(),
                dev_node: node.into(),
                writable,
                ..HostUsbDevice::default()
            }
        };
        let devs = vec![
            dev(
                ("046d", "085c"),
                "",
                "C922 Pro Stream Webcam",
                "3-2.2.2",
                "/dev/bus/usb/003/016",
                true,
            ),
            dev(
                ("046d", "c52b"),
                "Logitech",
                "USB Receiver",
                "3-2.2.3",
                "/dev/bus/usb/003/007",
                false,
            ),
            dev(
                ("0b05", "19af"),
                "AsusTek Computer Inc.",
                "AURA LED Controller",
                "1-5.3",
                "/dev/bus/usb/001/004",
                false,
            ),
        ];
        let cfg = VmConfig {
            name: "n".into(),
            usb_devices: vec![UsbDevice {
                vendor_id: "0781".into(),
                product_id: "5583".into(),
                name: "SanDisk Ultra Fit".into(),
                port: "3-9".into(),
            }],
            ..VmConfig::default()
        };
        let mut d = UsbDialog::new(cfg, true);
        d.on_task(scanned(devs, None, true), &mut h.ctx());
        d
    }

    #[test]
    fn no_access_markers_line_up_and_stay_whole() {
        let h = Harness::new();
        let mut d = no_access_dialog(&h);
        // 80x24 and 120x40 terminals, and the dashboard's right column at
        // those sizes and at 100 columns.
        for (w, hgt) in [(80, 24), (120, 40), (50, 22), (80, 38), (66, 28)] {
            let screen = h.render(&mut d, w, hgt);
            let cols: Vec<usize> = screen
                .iter()
                .filter_map(|l| char_col(l, "✗ no access"))
                .collect();
            assert_eq!(
                cols.len(),
                2,
                "{w}x{hgt}: both markers whole:\n{}",
                screen.join("\n")
            );
            assert_eq!(cols[0], cols[1], "{w}x{hgt}:\n{}", screen.join("\n"));
            for want in ["port 3-2.2.2", "not connected (port 3-9)"] {
                assert!(
                    screen_contains(&screen, want),
                    "{w}x{hgt}: lacks {want:?}:\n{}",
                    screen.join("\n")
                );
            }
        }
        // The name column is what gives way.
        let screen = h.render(&mut d, 120, 40);
        assert!(
            screen_contains(
                &screen,
                "  [ ] 046d:c52b  Logitech USB Receiver             port 3-2.2.3  ✗ no access"
            ),
            "{}",
            screen.join("\n")
        );
        assert!(
            screen_contains(
                &screen,
                "  [ ] 0b05:19af  AsusTek Computer Inc. AURA LED …  port 1-5.3    ✗ no access"
            ),
            "{}",
            screen.join("\n")
        );
        let screen = h.render(&mut d, 66, 28);
        assert!(
            screen_contains(
                &screen,
                "  [ ] 046d:c52b  Logitech USB Rece…  port 3-2.2.3  ✗ no access"
            ),
            "{}",
            screen.join("\n")
        );
        let screen = h.render(&mut d, 50, 22);
        assert!(
            screen_contains(&screen, "  [ ] 046d:c52b  L…  port 3-2.2.3  ✗ no access │"),
            "{}",
            screen.join("\n")
        );
    }

    #[test]
    fn narrow_panes_wrap_the_banner_header_and_help() {
        let h = Harness::new();
        let mut d = no_access_dialog(&h);
        h.press(&mut d, ch('j')); // onto the receiver, which cannot be opened
                                  // The right column of an 80x24 dashboard: the prose wraps instead of
                                  // being cut, and the command still fits below the cursor row.
        let screen = h.render(&mut d, 50, 22);
        for want in [
            "● running — attaching or detaching hot-plugs",
            "  the device in the guest",
            "The host cannot use a device while the guest",
            "  holds it.",
            "Host USB devices",
            "  [x] = passed through to this VM",
            "▸ [ ] 046d:c52b",
            "✗ no write access to /dev/bus/usb/003/007 —",
            "  QEMU cannot open it",
            "Run this to grant access to your user (y",
            "  copies it), then press r to rescan:",
            "    sudo udevadm trigger",
        ] {
            assert!(
                screen_contains(&screen, want),
                "lacks {want:?}:\n{}",
                screen.join("\n")
            );
        }
        // Wide enough, each stays on its line.
        let screen = h.render(&mut d, 120, 40);
        for want in [
            "● running — attaching or detaching hot-plugs the device in the guest",
            "Host USB devices   [x] = passed through to this VM",
            "✗ no write access to /dev/bus/usb/003/007 — QEMU cannot open it",
            "Run this to grant access to your user (y copies it), then press r to rescan:",
        ] {
            assert!(
                screen_contains(&screen, want),
                "lacks {want:?}:\n{}",
                screen.join("\n")
            );
        }
        // The add-by-ID help moves under the input.
        h.press(&mut d, ch('a'));
        let screen = h.render(&mut d, 50, 22);
        for want in [
            "Vendor:Product ID  046d:085c",
            "  as shown by lsusb, e.g. 046d:085c",
        ] {
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
                "Vendor:Product ID  046d:085c      as shown by lsusb, e.g. 046d:085c"
            ),
            "{}",
            screen.join("\n")
        );
        // A failed scan and an empty list.
        h.press(&mut d, key(KeyCode::Esc));
        d.cfg.usb_devices.clear();
        d.on_task(scanned(Vec::new(), Some("list host USB devices (needs Linux sysfs): open /sys/bus/usb/devices: No such file or directory"), false), &mut h.ctx());
        let screen = h.render(&mut d, 50, 22);
        for want in [
            "● stopped — changes take effect when the VM is",
            "  started",
            "Devices can still be added by ID with a.",
            "No USB devices found. Plug one in and press r",
            "  to rescan, or press a to add one by ID.",
        ] {
            assert!(
                screen_contains(&screen, want),
                "lacks {want:?}:\n{}",
                screen.join("\n")
            );
        }
    }

    #[test]
    fn add_by_id_waits_for_the_change_in_flight() {
        let h = Harness::new();
        let cfg = VmConfig {
            name: "b".into(),
            cpu: 1,
            ram: 128,
            ..VmConfig::default()
        };
        save_vm(h.mgr.storage(), &cfg);
        let mut d = UsbDialog::new(cfg, false);
        d.on_task(scanned(test_host_devs(), None, false), &mut h.ctx());

        // Attach the webcam; while that is in flight the input opens, but
        // Enter waits: started from the config the attach has not updated
        // yet, the second save would drop the webcam from vm.yaml.
        h.press(&mut d, ch(' '));
        assert_eq!(
            d.busy(),
            Some("attaching C922 Pro Stream Webcam…".to_string())
        );
        h.press(&mut d, ch('a'));
        type_str(&mut d, &h, "1a2b:3c4d");
        assert!(h.press(&mut d, key(KeyCode::Enter)).is_empty());
        assert!(d.adding(), "Enter waits while the attach is in flight");
        assert_eq!(d.input.value(), "1a2b:3c4d");
        let screen = h.render(&mut d, 80, 24);
        assert!(
            screen_contains(&screen, "attaching C922 Pro Stream Webcam…"),
            "the spinner shows why:\n{}",
            screen.join("\n")
        );
        apply(&h, &mut d); // the webcam, and nothing else, was spawned
        assert_eq!(ids(&d), ["046d:085c"]);

        // Once it has landed the same Enter goes through.
        h.press(&mut d, key(KeyCode::Enter));
        assert!(!d.adding());
        apply(&h, &mut d);
        assert_eq!(ids(&d), ["046d:085c", "1a2b:3c4d"]);
        let saved: Vec<String> = load_config(h.mgr.storage(), "b")
            .unwrap()
            .usb_devices
            .iter()
            .map(UsbDevice::id)
            .collect();
        assert_eq!(saved, ["046d:085c", "1a2b:3c4d"], "nothing lost");
    }

    /// While a change is in flight the dialog stays open, so quitting still
    /// waits for it: Esc, q and h do nothing, and the hints offer Ctrl-c
    /// instead of the way back. The add-by-ID input still closes on Esc.
    #[test]
    fn close_keys_wait_for_the_change_in_flight() {
        let h = Harness::new();
        let mut d = UsbDialog::new(
            VmConfig {
                name: "z".into(),
                ..VmConfig::default()
            },
            false,
        );
        d.busy = Some("attaching cam…".into());
        for k in [key(KeyCode::Esc), ch('q'), ch('h')] {
            assert!(h.press(&mut d, k).is_empty(), "{k:?} closed the dialog");
        }
        let hints = d.key_hints();
        assert_eq!(
            hints.last(),
            Some(&("Ctrl-c".to_string(), "quit".to_string()))
        );
        assert!(!hints.iter().any(|(k, _)| k.contains("Esc")), "{hints:?}");
        h.press(&mut d, ch('a'));
        assert!(d.adding);
        assert!(h.press(&mut d, key(KeyCode::Esc)).is_empty());
        assert!(!d.adding, "Esc leaves the input");

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
