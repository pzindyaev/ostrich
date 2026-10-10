//! The create-VM wizard: eleven linear steps, the ISO step being the ISO
//! picker inline.

use anyhow::{anyhow, bail, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Paragraph};
use unicode_width::UnicodeWidthStr;

use super::super::dialogs::isopicker::{iso_entries, IsoPicker, PickOutcome};
use super::super::events::{err_text, TaskResult};
use super::super::panel::{Ctx, Panel};
use super::super::theme::Theme;
use super::super::widgets::{
    fit_left, fit_words, if_empty, plural, spinner_frame, wrap_text, Selector, TextInput,
};
use super::common::{
    bridge_hint_for, bridge_hint_lines, firmware_choice, form_block, validate_vm_name,
    FIRMWARE_LABELS, NETWORK_CHOICES, NETWORK_LABELS, TPM_LABELS,
};
use crate::config;
use crate::vm::{format_disks, parse_disks, Manager, NetworkConfig, VmConfig};

/// The width of a text input, as in Go (`t.Width = 45`).
pub(super) const INPUT_WIDTH: u16 = 45;
/// The character limit of every form input.
pub(super) const CHAR_LIMIT: usize = 256;

/// The wizard's steps, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Name,
    Cpu,
    Ram,
    Disk,
    Disks,
    Iso,
    Firmware,
    Tpm,
    Network,
    Vnc,
    Confirm,
}

impl Step {
    const ALL: [Step; 11] = [
        Step::Name,
        Step::Cpu,
        Step::Ram,
        Step::Disk,
        Step::Disks,
        Step::Iso,
        Step::Firmware,
        Step::Tpm,
        Step::Network,
        Step::Vnc,
        Step::Confirm,
    ];

    fn index(self) -> usize {
        Self::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }

    fn next(self) -> Option<Step> {
        Self::ALL.get(self.index() + 1).copied()
    }

    fn prev(self) -> Option<Step> {
        self.index().checked_sub(1).map(|i| Self::ALL[i])
    }

    fn label(self) -> &'static str {
        match self {
            Step::Name => "VM Name",
            Step::Cpu => "CPU Cores",
            Step::Ram => "RAM (MiB)",
            Step::Disk => "Disk Size (GiB)",
            Step::Disks => "Additional disks (optional — leave blank for none)",
            Step::Iso => "Boot ISO (optional)",
            Step::Firmware => "Firmware",
            Step::Tpm => "TPM 2.0",
            Step::Network => "Network type",
            Step::Vnc => "VNC Display Number (0 = disabled)",
            Step::Confirm => "Confirm",
        }
    }

    fn help(self) -> &'static str {
        match self {
            Step::Name => "Letters, digits, hyphens and underscores, e.g. debian-12",
            Step::Cpu => "Number of virtual CPU cores, e.g. 2",
            Step::Ram => "Memory in MiB, e.g. 2048 for 2 GiB",
            Step::Disk => "Disk size in GiB, e.g. 20",
            Step::Disks => "Comma-separated [name:]size in GiB, e.g. data:50, 100. Each arrives blank in the guest as /dev/disk/by-id/virtio-<name>: partition and format it there",
            Step::Iso => "An image used before, or the path of a new one on the last row (~ is your home directory); (none) to install from the disk. Enter picks it and moves on",
            Step::Firmware => "h/l/←/→ to select. Windows 11 needs UEFI + Secure Boot and a TPM (next step); Linux boots with any",
            Step::Tpm => "h/l/←/→ to select. Emulated by swtpm on the host — required by Windows 11",
            Step::Network => "h/l/←/→ to select: user (NAT) · tap (bridge) · none",
            Step::Vnc => "Display number 1–99 (TCP port = 5900+n). 0 to disable. Connect with vncviewer 127.0.0.1:<n>",
            Step::Confirm => "Press Enter to create the VM",
        }
    }

    /// The step's index in the inputs array, or `None` for non-text steps.
    fn input_index(self) -> Option<usize> {
        match self {
            Step::Name => Some(0),
            Step::Cpu => Some(1),
            Step::Ram => Some(2),
            Step::Disk => Some(3),
            Step::Disks => Some(4),
            Step::Vnc => Some(5),
            _ => None,
        }
    }

    /// Whether the step is a horizontal choice rather than text.
    fn is_selector(self) -> bool {
        matches!(self, Step::Firmware | Step::Tpm | Step::Network)
    }
}

/// The create-VM wizard: a linear multi-step form for defining a new VM.
/// See internal/tui/create.go for the behaviour it reproduces.
pub struct CreateForm {
    step: Step,
    /// name, cpu, ram, disk, disks, vnc
    inputs: [TextInput; 6],
    /// The boot ISO, `""` for none.
    iso: String,
    /// The ISO dialog, which is the ISO step; fresh on each entry.
    picker: Option<IsoPicker>,
    firmware: Selector,
    tpm: Selector,
    network: Selector,
    /// What the host lacks for tap networking, `""` when it is ready or
    /// another network is picked.
    bridge_hint: String,
    /// The name of the VM being created, while that is in flight.
    creating: Option<String>,
    err: String,
}

impl CreateForm {
    /// A fresh wizard with the defaults `my-vm`, 2 CPUs, 2048 MiB, 20 GiB,
    /// no extra disks, no ISO, BIOS, TPM disabled, user networking, VNC 0.
    pub fn new() -> Self {
        let defaults = ["my-vm", "2", "2048", "20", "", "0"];
        let mut inputs: [TextInput; 6] = std::array::from_fn(|i| {
            TextInput::new()
                .with_value(defaults[i])
                .with_char_limit(CHAR_LIMIT)
        });
        inputs[0].focus();
        CreateForm {
            step: Step::Name,
            inputs,
            iso: String::new(),
            picker: None,
            firmware: Selector::new(FIRMWARE_LABELS, 0),
            tpm: Selector::new(TPM_LABELS, 0),
            network: Selector::new(NETWORK_LABELS, 0),
            bridge_hint: String::new(),
            creating: None,
            err: String::new(),
        }
    }

    /// The trimmed value of a text step.
    fn value(&self, step: Step) -> String {
        step.input_index()
            .map(|i| self.inputs[i].trimmed())
            .unwrap_or_default()
    }

    /// Runs the ISO step, which is the picker: Enter takes its pick and
    /// moves on, Tab moves on with the ISO as it is (or picks a path typed
    /// but not entered yet), and the rest is the picker's own.
    fn handle_iso_key(&mut self, mut key: KeyEvent, ctx: &mut Ctx) {
        match key.code {
            KeyCode::Esc => {
                ctx.close();
                return;
            }
            KeyCode::BackTab => {
                self.retreat(ctx);
                return;
            }
            KeyCode::Tab => {
                if !self.picker.as_ref().is_some_and(IsoPicker::typed) {
                    self.advance(ctx);
                    return;
                }
                key = KeyEvent::from(KeyCode::Enter);
            }
            _ => {}
        }
        let Some(picker) = &mut self.picker else {
            return;
        };
        if let PickOutcome::Picked(path) = picker.handle_key(key, ctx.home) {
            self.iso = path;
            self.advance(ctx);
        }
    }

    /// Moves the current step's selector by `delta`, if it has one.
    fn cycle(&mut self, delta: i32) {
        match self.step {
            Step::Firmware => self.firmware.cycle(delta),
            Step::Tpm => self.tpm.cycle(delta),
            Step::Network => {
                self.network.cycle(delta);
                self.bridge_hint = bridge_hint_for(NETWORK_CHOICES[self.network.index]);
            }
            _ => {}
        }
    }

    fn advance(&mut self, ctx: &mut Ctx) {
        if let Some(idx) = self.step.input_index() {
            if let Err(e) = self.validate_step(self.step, ctx.mgr) {
                self.err = err_text(&e);
                return;
            }
            self.inputs[idx].blur();
        }
        self.err.clear();
        if self.step == Step::Confirm {
            self.create(ctx);
            return;
        }
        if let Some(next) = self.step.next() {
            self.step = next;
            self.enter_step(ctx);
        }
    }

    fn retreat(&mut self, ctx: &mut Ctx) {
        let Some(prev) = self.step.prev() else {
            ctx.close();
            return;
        };
        if let Some(idx) = self.step.input_index() {
            self.inputs[idx].blur();
        }
        self.step = prev;
        self.err.clear();
        self.enter_step(ctx);
    }

    /// Readies what the step just moved to edits: its text input, or for
    /// the ISO step a picker with the cursor on the ISO chosen so far.
    fn enter_step(&mut self, ctx: &mut Ctx) {
        self.picker = (self.step == Step::Iso).then(|| {
            IsoPicker::new(
                "(none) — no boot ISO",
                &self.iso,
                iso_entries(ctx.storage()),
            )
        });
        if let Some(idx) = self.step.input_index() {
            self.inputs[idx].focus();
        }
    }

    fn validate_step(&self, step: Step, mgr: &Manager) -> Result<()> {
        if step.input_index().is_none() {
            return Ok(());
        }
        let val = self.value(step);
        match step {
            Step::Name => {
                validate_vm_name(&val)?;
                if mgr.exists(&val) {
                    bail!("a VM named {val:?} already exists");
                }
            }
            // The config holds these as u32, so a larger number is refused
            // here rather than at the confirm step.
            Step::Cpu if !parse_u32(&val).is_some_and(|v| v >= 1) => {
                bail!("CPU must be a positive integer")
            }
            Step::Ram if !parse_u32(&val).is_some_and(|v| v >= 64) => {
                bail!("RAM must be at least 64 MiB")
            }
            Step::Disk if !parse_u32(&val).is_some_and(|v| v >= 1) => {
                bail!("disk size must be at least 1 GiB")
            }
            Step::Disks => {
                parse_disks(&val)?;
            }
            Step::Vnc if !parse_int(&val).is_some_and(|v| (0..=99).contains(&v)) => {
                bail!("VNC display must be 0 (disabled) or 1–99")
            }
            _ => {}
        }
        Ok(())
    }

    /// The config the wizard's answers describe. Arch, MAC and the creation
    /// time are left to the manager.
    fn build_config(&self) -> Result<VmConfig> {
        let cpu = parse_u32(&self.value(Step::Cpu)).ok_or_else(|| anyhow!("invalid CPU value"))?;
        let ram = parse_u32(&self.value(Step::Ram)).ok_or_else(|| anyhow!("invalid RAM value"))?;
        let disk_size =
            parse_u32(&self.value(Step::Disk)).ok_or_else(|| anyhow!("invalid disk size"))?;
        let disks = parse_disks(&self.inputs[4].value())?;
        let vnc_port = parse_int(&self.value(Step::Vnc))
            .and_then(|v| u16::try_from(v).ok())
            .unwrap_or(0);
        let (firmware, secure_boot) = firmware_choice(self.firmware.index);
        Ok(VmConfig {
            name: self.value(Step::Name),
            cpu,
            ram,
            disk_size,
            disks,
            cdrom_path: self.iso.clone(),
            firmware,
            secure_boot,
            tpm: self.tpm.index == 1,
            vnc_port,
            network: NetworkConfig {
                kind: NETWORK_CHOICES[self.network.index],
                ..NetworkConfig::default()
            },
            ..VmConfig::default()
        })
    }

    /// Starts the creation in the background. Until its result is back the
    /// form is busy: every key but Ctrl-c waits, and quitting waits for it,
    /// since a create cut short can leave a VM directory with disks but no
    /// `vm.yaml` (Go's wizard had no busy state).
    fn create(&mut self, ctx: &mut Ctx) {
        let mut cfg = match self.build_config() {
            Ok(cfg) => cfg,
            Err(e) => {
                self.err = err_text(&e);
                return;
            }
        };
        self.creating = Some(cfg.name.clone());
        let mgr = ctx.mgr.clone();
        ctx.spawn(move || {
            if let Err(e) = mgr.create(&mut cfg) {
                return TaskResult::VmCreateFailed { err: err_text(&e) };
            }
            if !cfg.cdrom_path.is_empty() {
                // For the picker; losing it costs nothing.
                let _ = config::remember_iso(&cfg.cdrom_path);
            }
            TaskResult::VmCreated { name: cfg.name }
        });
    }

    /// The rows of the confirm step's summary box.
    fn summary_rows(cfg: &VmConfig) -> Vec<SummaryRow> {
        let vnc = if cfg.vnc_port > 0 {
            format!(
                "display {} (port {})",
                cfg.vnc_port,
                5900 + u32::from(cfg.vnc_port)
            )
        } else {
            "disabled".to_string()
        };
        vec![
            SummaryRow::new("Name:", &cfg.name),
            SummaryRow::new("CPU:", plural(cfg.cpu, "core", "cores")),
            SummaryRow::new("RAM:", format!("{} MiB", cfg.ram)),
            SummaryRow::new("Disk:", format!("{} GiB", cfg.disk_size)),
            SummaryRow::new("Disks:", if_empty(&format_disks(&cfg.disks), "(none)")),
            SummaryRow::path("ISO:", if_empty(&cfg.cdrom_path, "(none)")),
            SummaryRow::new("Firmware:", cfg.firmware_label()),
            SummaryRow::new("Net:", cfg.network.kind.to_string()),
            SummaryRow::new("VNC:", vnc),
        ]
    }

    /// Jumps straight to a step, the way the Go tests set `m.step`.
    #[cfg(test)]
    fn jump_to(&mut self, step: Step) {
        for input in &mut self.inputs {
            input.blur();
        }
        self.picker = None;
        self.step = step;
        self.err.clear();
        if let Some(idx) = step.input_index() {
            self.inputs[idx].focus();
        }
    }
}

impl Default for CreateForm {
    fn default() -> Self {
        Self::new()
    }
}

impl Panel for CreateForm {
    fn title(&self) -> String {
        "Create VM".to_string()
    }

    fn handle_key(&mut self, key: KeyEvent, ctx: &mut Ctx) {
        if self.creating.is_some() {
            return; // the create cannot be interrupted from here
        }
        if self.step == Step::Iso {
            self.handle_iso_key(key, ctx);
            return;
        }
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let text_step = self.step.input_index().is_some();
        match key.code {
            KeyCode::Esc => {
                ctx.close();
                return;
            }
            KeyCode::Tab | KeyCode::Enter | KeyCode::Down => {
                self.advance(ctx);
                return;
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.retreat(ctx);
                return;
            }
            // vim-next/prev: only when no text input is active
            KeyCode::Char('j') if plain && !text_step => {
                self.advance(ctx);
                return;
            }
            KeyCode::Char('k') if plain && !text_step => {
                self.retreat(ctx);
                return;
            }
            KeyCode::Char('h') | KeyCode::Left if plain => self.cycle(-1),
            KeyCode::Char('l') | KeyCode::Right if plain => self.cycle(1),
            _ => {}
        }
        // Forward keystrokes to the active text input.
        if let Some(idx) = self.step.input_index() {
            self.inputs[idx].handle_key(key);
        }
    }

    fn handle_paste(&mut self, text: &str, _ctx: &mut Ctx) {
        if self.creating.is_some() {
            return;
        }
        if self.step == Step::Iso {
            if let Some(picker) = &mut self.picker {
                picker.handle_paste(text);
            }
            return;
        }
        if let Some(idx) = self.step.input_index() {
            self.inputs[idx].insert_str(text);
        }
    }

    fn on_task(&mut self, result: TaskResult, ctx: &mut Ctx) {
        match result {
            TaskResult::VmCreated { name } => {
                self.creating = None;
                ctx.close_select_vm(name.clone());
                ctx.notice_ok(format!("created {name}"));
            }
            TaskResult::VmCreateFailed { err } | TaskResult::Failed { err, .. } => {
                self.creating = None;
                self.err = err;
            }
            _ => {}
        }
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme, tick: u64) {
        let inner = form_block(frame, area, &self.title(), theme);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        let width = inner.width as usize;

        let step_line = format!("Step {} / {}", self.step.index() + 1, Step::ALL.len());
        let used = draw_lines(
            frame,
            inner,
            step_header(
                &step_line,
                self.step.label(),
                self.step.help(),
                width,
                theme,
            ),
        );
        let mut rest = below(inner, used);

        match self.step {
            Step::Iso => {
                if let Some(picker) = &mut self.picker {
                    let h = picker.height_for(rest.width).min(rest.height);
                    if h > 0 {
                        picker.render(frame, Rect { height: h, ..rest }, theme);
                    }
                    rest = below(rest, h);
                }
            }
            Step::Firmware | Step::Tpm | Step::Network => {
                let sel = match self.step {
                    Step::Firmware => &self.firmware,
                    Step::Tpm => &self.tpm,
                    _ => &self.network,
                };
                let mut lines = selector_lines(sel, theme, usize::from(rest.width));
                lines.push(Line::raw(""));
                let used = draw_lines(frame, rest, lines);
                rest = below(rest, used);
            }
            Step::Confirm => {
                if let Ok(cfg) = self.build_config() {
                    let used = draw_summary_box(frame, rest, &Self::summary_rows(&cfg), theme);
                    rest = below(rest, used);
                    rest = below(rest, draw_lines(frame, rest, vec![Line::raw("")]));
                }
            }
            _ => {
                if let Some(idx) = self.step.input_index() {
                    let used = draw_input(frame, rest, &mut self.inputs[idx], theme);
                    rest = below(rest, used);
                }
            }
        }

        let mut tail = Vec::new();
        if matches!(self.step, Step::Network | Step::Confirm) && !self.bridge_hint.is_empty() {
            tail.extend(bridge_hint_lines(&self.bridge_hint, width, theme));
            tail.push(Line::raw(""));
        }
        if let Some(name) = &self.creating {
            tail.push(Line::from(vec![
                Span::styled(spinner_frame(tick), theme.spinner),
                Span::styled(format!(" Creating {name}…"), theme.normal),
            ]));
        } else if !self.err.is_empty() {
            tail.extend(error_lines(&self.err, width, theme));
        }
        draw_lines(frame, rest, tail);
    }

    fn key_hints(&self) -> Vec<(String, String)> {
        // While creating every key but Ctrl-c is swallowed; the body shows
        // the progress line.
        let pairs: Vec<(&str, &str)> = if self.creating.is_some() {
            vec![("Ctrl-c", "quit")]
        } else if self.step == Step::Iso {
            let mut hints = self
                .picker
                .as_ref()
                .map(|p| p.key_hints("pick and go on"))
                .unwrap_or_default();
            hints.extend(
                [("Tab", "next"), ("Shift-Tab", "back"), ("Esc", "cancel")]
                    .map(|(k, d)| (k.to_string(), d.to_string())),
            );
            return hints;
        } else if self.step.is_selector() {
            vec![
                ("h/l/←/→", "select"),
                ("j/↓", "next"),
                ("k/↑", "back"),
                ("Esc", "cancel"),
            ]
        } else if self.step == Step::Confirm {
            vec![
                ("Enter/j", "create VM"),
                ("k/Shift-Tab", "back"),
                ("Esc", "cancel"),
            ]
        } else {
            vec![
                ("Tab/↓", "next"),
                ("Shift-Tab/↑", "back"),
                ("Esc", "cancel"),
            ]
        };
        pairs
            .into_iter()
            .map(|(k, d)| (k.to_string(), d.to_string()))
            .collect()
    }

    fn busy(&self) -> Option<String> {
        self.creating
            .as_ref()
            .map(|name| format!("creating {name}…"))
    }

    fn failed(&self) -> bool {
        !self.err.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Shared with the from-template wizard
// ---------------------------------------------------------------------------

/// Go's `strconv.Atoi`: an optional sign and digits, nothing else.
pub(super) fn parse_int(s: &str) -> Option<i64> {
    s.parse::<i64>().ok()
}

/// [`parse_int`] for a count the config holds as u32: `None` when it is
/// not a number or does not fit.
pub(super) fn parse_u32(s: &str) -> Option<u32> {
    parse_int(s).and_then(|v| u32::try_from(v).ok())
}

/// The lines every wizard step starts with: the step line, a blank, the
/// step's label, its help text wrapped to `width`, and a blank.
pub(super) fn step_header(
    step_line: &str,
    label: &str,
    help: &str,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = wrap_text(step_line, width)
        .into_iter()
        .map(|l| Line::styled(l, theme.help))
        .collect();
    lines.push(Line::raw(""));
    lines.push(Line::styled(label.to_string(), theme.label));
    lines.extend(
        wrap_text(help, width)
            .into_iter()
            .map(|l| Line::styled(l, theme.help)),
    );
    lines.push(Line::raw(""));
    lines
}

/// Draws `lines` from the top of `area` and returns the rows used; nothing
/// is drawn into an empty area.
pub(super) fn draw_lines(frame: &mut Frame, area: Rect, lines: Vec<Line<'static>>) -> u16 {
    if area.width == 0 || area.height == 0 || lines.is_empty() {
        return 0;
    }
    let height = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .min(area.height);
    frame.render_widget(Paragraph::new(lines), Rect { height, ..area });
    height
}

/// `area` without its first `rows` rows.
pub(super) fn below(area: Rect, rows: u16) -> Rect {
    let rows = rows.min(area.height);
    Rect {
        y: area.y.saturating_add(rows),
        height: area.height - rows,
        ..area
    }
}

/// The wizard's text control: the focused-row marker and the input (one
/// row, at most [`INPUT_WIDTH`] wide), then a blank row. Returns the rows used.
pub(super) fn draw_input(
    frame: &mut Frame,
    area: Rect,
    input: &mut TextInput,
    theme: &Theme,
) -> u16 {
    if area.width == 0 || area.height == 0 {
        return 0;
    }
    let row = Rect { height: 1, ..area };
    frame.render_widget(
        Span::styled("▸ ", theme.label),
        Rect {
            width: row.width.min(2),
            ..row
        },
    );
    let input_area = Rect {
        x: row.x.saturating_add(2),
        width: row.width.saturating_sub(2).min(INPUT_WIDTH),
        ..row
    };
    if input_area.width > 0 {
        input.render(frame, input_area, theme);
    }
    2.min(area.height)
}

/// A selector as the focused row: the `▸ ` marker and the choices, wrapped
/// to `width` cells under the first choice.
pub(super) fn selector_lines(sel: &Selector, theme: &Theme, width: usize) -> Vec<Line<'static>> {
    let mut lines = sel.lines(theme, width.saturating_sub(2));
    for (i, line) in lines.iter_mut().enumerate() {
        let lead = if i == 0 { "▸ " } else { "  " };
        line.spans.insert(0, Span::styled(lead, theme.label));
    }
    lines
}

/// The width of a summary row's key column (`Firmware: `).
const SUMMARY_KEY_W: usize = 10;

/// One row of a confirm step's summary box: a key such as `ISO:` padded to
/// the key column, then its value.
pub(super) struct SummaryRow {
    key: &'static str,
    value: String,
    /// The value is a path: when the box is too narrow it loses its start,
    /// not its end, so the file name stays visible.
    path: bool,
}

impl SummaryRow {
    pub(super) fn new(key: &'static str, value: impl Into<String>) -> Self {
        SummaryRow {
            key,
            value: value.into(),
            path: false,
        }
    }

    pub(super) fn path(key: &'static str, value: impl Into<String>) -> Self {
        SummaryRow {
            path: true,
            ..Self::new(key, value)
        }
    }

    /// The row in full, e.g. `CPU:      2 cores`.
    fn text(&self) -> String {
        format!("{:<SUMMARY_KEY_W$}{}", self.key, self.value)
    }

    /// The row cut to `width` cells with an ellipsis: at the end of the
    /// value (at a word boundary when one is near), or at the start of a
    /// path. Cells, not chars: a wide character takes two.
    fn fitted(&self, width: usize) -> String {
        let text = self.text();
        if text.width() <= width {
            return text;
        }
        if width <= SUMMARY_KEY_W {
            return fit_words(&text, width);
        }
        let room = width - SUMMARY_KEY_W;
        let value = if self.path {
            fit_left(&self.value, room)
        } else {
            fit_words(&self.value, room)
        };
        format!("{:<SUMMARY_KEY_W$}{value}", self.key)
    }
}

/// A rounded box in the border style hugging `rows`, drawn at the top left
/// of `area`; returns the rows used (0 when the area cannot hold a box).
/// Rows wider than the area are cut with an ellipsis ([`SummaryRow::fitted`]).
pub(super) fn draw_summary_box(
    frame: &mut Frame,
    area: Rect,
    rows: &[SummaryRow],
    theme: &Theme,
) -> u16 {
    if area.height < 3 || area.width < 5 || rows.is_empty() {
        return 0;
    }
    let content_w = rows.iter().map(|r| r.text().width()).max().unwrap_or(0);
    let width = u16::try_from(content_w + 4)
        .unwrap_or(u16::MAX)
        .min(area.width);
    let height = u16::try_from(rows.len() + 2)
        .unwrap_or(u16::MAX)
        .min(area.height);
    let rect = Rect {
        width,
        height,
        ..area
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme.border);
    let inner = block.inner(rect).inner(Margin::new(1, 0));
    frame.render_widget(block, rect);
    let lines: Vec<Line<'static>> = rows
        .iter()
        .map(|r| Line::styled(r.fitted(usize::from(inner.width)), theme.normal))
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
    height
}

/// `✗ <err>` in the error style, wrapped to `width`, every line after the
/// first indented under the text.
pub(super) fn error_lines(err: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    wrap_text(err, width.saturating_sub(2).max(8))
        .into_iter()
        .enumerate()
        .map(|(i, l)| {
            Line::styled(
                format!("{}{l}", if i == 0 { "✗ " } else { "  " }),
                theme.error,
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crossterm::event::KeyCode;

    use super::*;
    use crate::tui::panel::Action;
    use crate::tui::testutil::*;
    use crate::vm::{load_config, save_config, vm_dir, Disk, FirmwareType, NetworkType};

    /// Writes a `vm.yaml` for a VM so `exists` sees it.
    fn write_vm(h: &Harness, name: &str, cfg: VmConfig) {
        let storage = h.mgr.storage();
        fs::create_dir_all(vm_dir(storage, name)).unwrap();
        save_config(
            storage,
            &VmConfig {
                name: name.to_string(),
                ..cfg
            },
        )
        .unwrap();
    }

    /// Replaces the focused input's value by typing, as a user would.
    fn retype(form: &mut CreateForm, h: &Harness, s: &str) {
        h.press(form, ctrl('u'));
        type_str(form, h, s);
    }

    fn enter(form: &mut CreateForm, h: &Harness) -> Vec<Action> {
        h.press(form, key(KeyCode::Enter))
    }

    #[test]
    fn first_step_renders_the_defaults() {
        let h = Harness::new();
        let mut form = CreateForm::new();
        assert_eq!(form.title(), "Create VM");
        assert_eq!(form.step, Step::Name);
        let want = ["my-vm", "2", "2048", "20", "", "0"];
        for (i, w) in want.iter().enumerate() {
            assert_eq!(form.inputs[i].value(), *w, "input {i}");
        }
        assert!(form.inputs[0].focused);
        assert!(form.iso.is_empty() && form.picker.is_none());
        assert_eq!(
            (form.firmware.index, form.tpm.index, form.network.index),
            (0, 0, 0)
        );

        let screen = h.render(&mut form, 100, 40);
        for want in [
            "Create VM",
            "Step 1 / 11",
            "VM Name",
            "Letters, digits, hyphens and underscores, e.g. debian-12",
            "▸ my-vm",
        ] {
            assert!(
                screen_contains(&screen, want),
                "screen lacks {want:?}:\n{}",
                screen.join("\n")
            );
        }
        let hints = form.key_hints();
        assert_eq!(hints[0], ("Tab/↓".to_string(), "next".to_string()));
        assert_eq!(hints[2], ("Esc".to_string(), "cancel".to_string()));
        // The defaults carry through a fresh wizard's config.
        let cfg = form.build_config().unwrap();
        assert_eq!(
            (cfg.cpu, cfg.ram, cfg.disk_size, cfg.vnc_port),
            (2, 2048, 20, 0)
        );
        assert_eq!(cfg.firmware, FirmwareType::Bios);
        assert_eq!(cfg.network.kind, NetworkType::User);
    }

    #[test]
    fn name_step_validates() {
        let h = Harness::new();
        write_vm(
            &h,
            "taken",
            VmConfig {
                cpu: 1,
                ram: 128,
                disk_size: 1,
                ..VmConfig::default()
            },
        );
        let mut form = CreateForm::new();

        h.press(&mut form, ctrl('u'));
        assert!(enter(&mut form, &h).is_empty());
        assert_eq!(form.step, Step::Name);
        assert_eq!(form.err, "name cannot be empty");
        let screen = h.render(&mut form, 100, 40);
        assert!(
            screen_contains(&screen, "✗ name cannot be empty"),
            "{}",
            screen.join("\n")
        );

        type_str(&mut form, &h, "a b");
        enter(&mut form, &h);
        assert_eq!(
            form.err,
            "name may only contain letters, digits, hyphens and underscores"
        );

        retype(&mut form, &h, "taken");
        enter(&mut form, &h);
        assert_eq!(form.err, "a VM named \"taken\" already exists");
        assert_eq!(form.step, Step::Name);

        // A good name clears the error and moves on; the name input loses the focus.
        retype(&mut form, &h, "  deb-12_x  ");
        enter(&mut form, &h);
        assert_eq!(form.step, Step::Cpu);
        assert!(form.err.is_empty());
        assert!(!form.inputs[0].focused && form.inputs[1].focused);
        assert_eq!(form.value(Step::Name), "deb-12_x");
    }

    #[test]
    fn numeric_steps_validate() {
        let h = Harness::new();
        let mut form = CreateForm::new();
        let cases: [(Step, &[&str], &str, &str); 4] = [
            (
                Step::Cpu,
                &["0", "x", "", "1.5"],
                "CPU must be a positive integer",
                "+4",
            ),
            (
                Step::Ram,
                &["63", "lots", "-64"],
                "RAM must be at least 64 MiB",
                "64",
            ),
            (
                Step::Disk,
                &["0", "20G"],
                "disk size must be at least 1 GiB",
                "1",
            ),
            (
                Step::Vnc,
                &["100", "-1", "one"],
                "VNC display must be 0 (disabled) or 1–99",
                "99",
            ),
        ];
        for (step, bad, msg, good) in cases {
            form.jump_to(step);
            for b in bad {
                retype(&mut form, &h, b);
                enter(&mut form, &h);
                assert_eq!(form.step, step, "{b:?} advanced");
                assert_eq!(form.err, msg, "for {b:?}");
            }
            retype(&mut form, &h, good);
            enter(&mut form, &h);
            assert_ne!(form.step, step, "{good:?} refused: {}", form.err);
            assert!(form.err.is_empty());
        }
        let cfg = form.build_config().unwrap();
        assert_eq!(
            (cfg.cpu, cfg.ram, cfg.disk_size, cfg.vnc_port),
            (4, 64, 1, 99)
        );
    }

    /// A count beyond what the config holds (u32) is refused on its own
    /// step in the step's words, not at the confirm step.
    #[test]
    fn values_beyond_u32_are_refused_on_their_step() {
        let h = Harness::new();
        let mut form = CreateForm::new();
        let cases = [
            (Step::Cpu, "CPU must be a positive integer"),
            (Step::Ram, "RAM must be at least 64 MiB"),
            (Step::Disk, "disk size must be at least 1 GiB"),
        ];
        for (step, msg) in cases {
            form.jump_to(step);
            for big in ["4294967296", "5000000000"] {
                retype(&mut form, &h, big);
                enter(&mut form, &h);
                assert_eq!(form.step, step, "{big:?} advanced");
                assert_eq!(form.err, msg, "for {big:?}");
            }
            retype(&mut form, &h, "4294967295");
            enter(&mut form, &h);
            assert_ne!(form.step, step, "u32::MAX refused: {}", form.err);
        }
        form.jump_to(Step::Vnc);
        retype(&mut form, &h, "5900");
        enter(&mut form, &h);
        assert_eq!(form.err, "VNC display must be 0 (disabled) or 1–99");
        // What passed the steps makes a config, so the summary shows.
        form.jump_to(Step::Confirm);
        assert!(form.build_config().is_ok());
        let screen = h.render(&mut form, 120, 40);
        assert!(
            screen_contains(&screen, "CPU:      4294967295 cores"),
            "{}",
            screen.join("\n")
        );
    }

    /// Walks the wizard with Enter, picking Secure Boot and a TPM on the
    /// way, and checks every step renders and the result is right. The ISO
    /// step is jumped over here; the ISO tests drive it.
    #[test]
    fn firmware_steps() {
        let h = Harness::new();
        let mut form = CreateForm::new();
        for step in Step::ALL {
            if step == Step::Confirm {
                break;
            }
            if step == Step::Iso {
                continue; // jumped over from the Disks step
            }
            assert_eq!(form.step, step, "err {:?}", form.err);
            let screen = h.render(&mut form, 100, 40);
            assert!(
                screen_contains(&screen, step.label()),
                "step {step:?} view lacks its label"
            );
            assert!(screen_contains(
                &screen,
                &format!("Step {} / 11", step.index() + 1)
            ));
            match step {
                Step::Disks => {
                    // Over the ISO step, which the ISO tests drive.
                    assert!(form.validate_step(step, &h.mgr).is_ok());
                    form.jump_to(Step::Firmware);
                    continue;
                }
                Step::Firmware => {
                    h.press(&mut form, ch('l'));
                    h.press(&mut form, ch('l')); // BIOS → UEFI → UEFI + Secure Boot
                }
                Step::Tpm => {
                    h.press(&mut form, ch('l')); // disabled → enabled
                }
                _ => {}
            }
            enter(&mut form, &h);
        }
        assert_eq!(form.step, Step::Confirm);
        let screen = h.render(&mut form, 100, 40);
        for want in [
            "Confirm",
            "Press Enter to create the VM",
            "UEFI + Secure Boot, TPM 2.0",
            "Name:     my-vm",
            "VNC:      disabled",
            "╭",
            "╰",
        ] {
            assert!(
                screen_contains(&screen, want),
                "confirm view lacks {want:?}:\n{}",
                screen.join("\n")
            );
        }
        let cfg = form.build_config().unwrap();
        assert_eq!(cfg.firmware, FirmwareType::Uefi);
        assert!(cfg.secure_boot && cfg.tpm);

        // Wrapping backwards from BIOS lands on Secure Boot; TPM toggles.
        form.jump_to(Step::Firmware);
        form.firmware.index = 0;
        form.tpm.index = 1;
        h.press(&mut form, ch('h'));
        form.jump_to(Step::Tpm);
        h.press(&mut form, ch('l'));
        assert_eq!((form.firmware.index, form.tpm.index), (2, 0));
        let screen = h.render(&mut form, 100, 40);
        assert!(
            screen_contains(&screen, "▸  disabled   enabled"),
            "{}",
            screen.join("\n")
        );
    }

    /// The summary box is capped at the pane: a row too wide for it ends in
    /// an ellipsis after a whole word, and the ISO path loses its start so
    /// the file name stays. 50x22 is the right-hand pane of an 80x24
    /// terminal.
    #[test]
    fn summary_rows_are_cut_with_an_ellipsis() {
        let h = Harness::new();
        let mut form = CreateForm::new();
        form.iso =
            "/home/someone/Downloads/installers/debian/debian-12.3.0-amd64-netinst.iso".into();
        form.inputs[4].set_value(
            "database:500, logs:20, scratch:10, cache:5, swap:4, tmp:1, media:900, backup:2000",
        );
        form.jump_to(Step::Confirm);
        for (w, hgt) in [(50, 22), (80, 24)] {
            let screen = h.render(&mut form, w, hgt);
            let text = screen.join("\n");
            let row = |key: &str| {
                screen
                    .iter()
                    .find(|l| l.contains(key))
                    .unwrap_or_else(|| panic!("no {key:?} row at {w}x{hgt}:\n{text}"))
                    .clone()
            };
            let iso = row("│ ISO:      ");
            assert!(
                iso.contains("…") && iso.contains("debian-12.3.0-amd64-netinst.iso │"),
                "{w}x{hgt}: ISO row {iso:?}:\n{text}"
            );
            let disks = row("│ Disks:    database:500");
            assert!(
                disks.contains("… ") && disks.ends_with(" │ │"),
                "{w}x{hgt}: Disks row {disks:?}:\n{text}"
            );
            // Every row keeps the box's right border.
            for key in ["Name:", "CPU:", "RAM:", "Firmware:", "Net:", "VNC:"] {
                assert!(row(key).ends_with("│ │"), "{w}x{hgt}: {key}:\n{text}");
            }
        }
        // Wide enough, nothing is cut.
        let screen = h.render(&mut form, 160, 40);
        assert!(screen_contains(
            &screen,
            "ISO:      /home/someone/Downloads/installers/debian/debian-12.3.0-amd64-netinst.iso "
        ));
        assert!(screen_contains(&screen, "media:900, backup:2000 │"));
        assert!(!screen_contains(&screen, "…"), "{}", screen.join("\n"));

        // The cut itself.
        let row = SummaryRow::path("ISO:", "/a/b/debian.iso");
        assert_eq!(row.fitted(40), "ISO:      /a/b/debian.iso");
        assert_eq!(row.fitted(20), "ISO:      …ebian.iso");
        assert_eq!(
            SummaryRow::new("VNC:", "display 1 (port 5901)").fitted(20),
            "VNC:      display 1…"
        );
        assert_eq!(SummaryRow::new("Name:", "deb").fitted(4), "Nam…");
    }

    /// Wide characters take two cells: a row is cut by cells, so it fits
    /// the box and a path keeps its file name.
    #[test]
    fn summary_rows_are_cut_by_cells() {
        let path = "/home/user/ダウンロード/debian-12.3.0-amd64-netinst.iso";
        let row = SummaryRow::path("ISO:", path);
        for w in [46, 30, 20] {
            let fitted = row.fitted(w);
            assert!(fitted.width() <= w, "{fitted:?} is wider than {w}");
            assert!(fitted.ends_with("t.iso"), "{w}: {fitted:?}");
        }
        assert_eq!(row.fitted(65), format!("ISO:      {path}"));
        let fitted = SummaryRow::new("Name:", "日本語の仮想マシン").fitted(17);
        assert!(fitted.width() <= 17, "{fitted:?}");
        assert!(fitted.starts_with("Name:     日本語"), "{fitted:?}");

        let h = Harness::new();
        let mut form = CreateForm::new();
        form.iso = path.into();
        form.jump_to(Step::Confirm);
        let screen = h.render(&mut form, 60, 30);
        let iso = screen
            .iter()
            .find(|l| l.contains("ISO:"))
            .unwrap_or_else(|| panic!("{}", screen.join("\n")));
        assert!(
            iso.ends_with(" │ │") && iso.trim_end_matches([' ', '│']).ends_with("netinst.iso"),
            "the box keeps its border and the file name:\n{}",
            screen.join("\n")
        );
    }

    /// The create runs in the background; until its result is back the
    /// form is busy, so quitting waits for it, and only Ctrl-c is offered.
    #[test]
    fn the_create_in_flight_keeps_the_form_busy() {
        let h = Harness::new();
        let mut form = CreateForm::new();
        retype(&mut form, &h, "deb");
        form.jump_to(Step::Confirm);
        assert_eq!(form.busy(), None);
        assert!(enter(&mut form, &h).is_empty());
        assert_eq!(form.busy().as_deref(), Some("creating deb…"));
        assert_eq!(
            form.key_hints(),
            [("Ctrl-c".to_string(), "quit".to_string())]
        );
        let screen = h.render(&mut form, 100, 40);
        assert!(
            screen_contains(&screen, " Creating deb…"),
            "{}",
            screen.join("\n")
        );
        // Keys and pastes wait for the result.
        for k in [
            key(KeyCode::Esc),
            key(KeyCode::BackTab),
            key(KeyCode::Enter),
        ] {
            assert!(h.press(&mut form, k).is_empty(), "{k:?}");
        }
        assert_eq!(form.step, Step::Confirm);
        let wait = std::time::Duration::from_secs(10);
        assert!(h.next_result(wait).is_some(), "the create ran");
        assert!(
            h.next_result(std::time::Duration::from_secs(1)).is_none(),
            "Enter started no second create"
        );
        // The result ends the wait, whatever it is.
        let mut ctx = h.ctx();
        form.on_task(
            TaskResult::VmCreateFailed {
                err: "create disk image: boom".into(),
            },
            &mut ctx,
        );
        assert_eq!(form.busy(), None);
        assert_eq!(form.err, "create disk image: boom");
        assert_eq!(form.key_hints()[0].0, "Enter/j");

        for result in [
            TaskResult::VmCreated { name: "deb".into() },
            TaskResult::Failed {
                what: "background task".into(),
                err: "internal error: boom".into(),
            },
        ] {
            let h = Harness::new();
            let mut form = CreateForm::new();
            form.jump_to(Step::Confirm);
            enter(&mut form, &h);
            assert!(form.busy().is_some());
            // The real create is done with the scratch directory first.
            assert!(h.next_result(wait).is_some());
            let mut ctx = h.ctx();
            form.on_task(result, &mut ctx);
            assert_eq!(form.busy(), None);
        }
    }

    /// One core is `1 core`.
    #[test]
    fn summary_counts_cores() {
        let h = Harness::new();
        let mut form = CreateForm::new();
        form.inputs[1].set_value("1");
        form.jump_to(Step::Confirm);
        let screen = h.render(&mut form, 120, 40);
        assert!(
            screen_contains(&screen, "CPU:      1 core "),
            "{}",
            screen.join("\n")
        );
        assert!(!screen_contains(&screen, "1 cores"));
        form.inputs[1].set_value("2");
        let screen = h.render(&mut form, 80, 24);
        assert!(
            screen_contains(&screen, "CPU:      2 cores"),
            "{}",
            screen.join("\n")
        );
    }

    #[test]
    fn extra_disks_step() {
        let h = Harness::new();
        let mut form = CreateForm::new();
        form.jump_to(Step::Disks);
        let screen = h.render(&mut form, 100, 40);
        assert!(
            screen_contains(&screen, "Additional disks"),
            "{}",
            screen.join("\n")
        );
        assert!(screen_contains(&screen, "Step 5 / 11"));

        form.inputs[4].set_value("data:50, 100");
        assert!(form.validate_step(Step::Disks, &h.mgr).is_ok());
        form.jump_to(Step::Confirm);
        let screen = h.render(&mut form, 100, 40);
        assert!(
            screen_contains(&screen, "Disks:    data:50, disk1:100"),
            "{}",
            screen.join("\n")
        );
        let cfg = form.build_config().unwrap();
        assert_eq!(
            cfg.disks,
            vec![
                Disk {
                    name: "data".into(),
                    size: 50
                },
                Disk {
                    name: "disk1".into(),
                    size: 100
                }
            ]
        );

        // A bad entry keeps the wizard on the step and says what is wrong.
        form.jump_to(Step::Disks);
        form.inputs[4].set_value("disk:5");
        enter(&mut form, &h);
        assert_eq!(form.step, Step::Disks);
        assert!(form.err.contains("main disk"), "{}", form.err);
        form.inputs[4].set_value("data:1, nope");
        enter(&mut form, &h);
        assert!(form.err.contains("expected [name:]size"), "{}", form.err);

        // Blank means no extra disks.
        form.inputs[4].set_value("");
        assert!(form.validate_step(Step::Disks, &h.mgr).is_ok());
        form.jump_to(Step::Confirm);
        let screen = h.render(&mut form, 100, 40);
        assert!(
            screen_contains(&screen, "Disks:    (none)"),
            "{}",
            screen.join("\n")
        );
        assert!(form.build_config().unwrap().disks.is_empty());
    }

    #[test]
    fn selector_keys_and_the_bridge_hint() {
        let h = Harness::new();
        let mut form = CreateForm::new();
        form.jump_to(Step::Network);
        assert_eq!(form.key_hints()[0].0, "h/l/←/→");
        h.press(&mut form, ch('l'));
        assert_eq!(NETWORK_CHOICES[form.network.index], NetworkType::Tap);
        assert_eq!(form.bridge_hint, bridge_hint_for(NetworkType::Tap));
        let screen = h.render(&mut form, 100, 40);
        assert!(screen_contains(&screen, "tap (bridge)"));
        assert_eq!(screen_contains(&screen, "⚠"), !form.bridge_hint.is_empty());
        h.press(&mut form, key(KeyCode::Right));
        assert_eq!(NETWORK_CHOICES[form.network.index], NetworkType::None);
        assert!(form.bridge_hint.is_empty());
        h.press(&mut form, key(KeyCode::Left));
        h.press(&mut form, ch('h'));
        assert_eq!(NETWORK_CHOICES[form.network.index], NetworkType::User);

        // j/k move between steps off a text input ...
        h.press(&mut form, ch('j'));
        assert_eq!(form.step, Step::Vnc);
        assert!(form.inputs[5].focused);
        // ... and are typed on one, like h and l.
        h.press(&mut form, ch('k'));
        h.press(&mut form, ch('h'));
        h.press(&mut form, ch('l'));
        assert_eq!(form.step, Step::Vnc);
        assert_eq!(form.inputs[5].value(), "0khl");
        h.press(&mut form, key(KeyCode::Up));
        assert_eq!(form.step, Step::Network);
        h.press(&mut form, ch('k'));
        assert_eq!(form.step, Step::Tpm);
        h.press(&mut form, key(KeyCode::Down));
        h.press(&mut form, key(KeyCode::Tab));
        assert_eq!(form.step, Step::Vnc);
        // Left/Right on a text step move the cursor, not a selector.
        form.inputs[5].set_value("1");
        h.press(&mut form, key(KeyCode::Left));
        h.press(&mut form, ch('2'));
        assert_eq!(form.inputs[5].value(), "21");
        assert_eq!(form.network.index, 0);
    }

    #[test]
    fn esc_and_back_from_the_first_step_close() {
        let h = Harness::new();
        let mut form = CreateForm::new();
        assert!(matches!(
            h.press(&mut form, key(KeyCode::Esc)).as_slice(),
            [Action::Close]
        ));
        assert!(matches!(
            h.press(&mut form, backtab()).as_slice(),
            [Action::Close]
        ));
        assert!(matches!(
            h.press(&mut form, key(KeyCode::Up)).as_slice(),
            [Action::Close]
        ));
        form.jump_to(Step::Confirm);
        assert!(matches!(
            h.press(&mut form, key(KeyCode::Esc)).as_slice(),
            [Action::Close]
        ));
        // Back from the confirm step with k or Shift-Tab, no validation on the way back.
        h.press(&mut form, ch('k'));
        assert_eq!(form.step, Step::Vnc);
        form.inputs[5].set_value("bad");
        h.press(&mut form, backtab());
        assert_eq!(form.step, Step::Network);
        assert!(form.err.is_empty());
    }

    #[test]
    fn paste_goes_into_the_focused_input() {
        let h = Harness::new();
        let mut form = CreateForm::new();
        let mut ctx = h.ctx();
        form.handle_paste("-two\nlines", &mut ctx);
        assert_eq!(form.inputs[0].value(), "my-vm-twolines");
        form.jump_to(Step::Firmware);
        form.handle_paste("x", &mut ctx);
        assert_eq!(form.inputs[0].value(), "my-vm-twolines");
    }

    #[test]
    fn task_results_close_or_show_the_error() {
        let h = Harness::new();
        let mut form = CreateForm::new();
        form.jump_to(Step::Confirm);
        let mut ctx = h.ctx();
        form.on_task(
            TaskResult::VmCreateFailed {
                err: "create disk image: boom".into(),
            },
            &mut ctx,
        );
        assert!(ctx.actions.is_empty());
        assert_eq!(form.err, "create disk image: boom");
        let screen = h.render(&mut form, 100, 40);
        assert!(
            screen_contains(&screen, "✗ create disk image: boom"),
            "{}",
            screen.join("\n")
        );
        assert!(
            screen_contains(&screen, "Name:     my-vm"),
            "the summary stays"
        );
        // A multi-line error wraps under the glyph.
        form.err = "swtpm not found — install it:\n  sudo pacman -S swtpm".to_string();
        let screen = h.render(&mut form, 100, 40);
        assert!(screen_contains(&screen, "✗ swtpm not found — install it:"));
        assert!(screen_contains(&screen, "    sudo pacman -S swtpm"));

        let mut ctx = h.ctx();
        form.on_task(TaskResult::VmCreated { name: "deb".into() }, &mut ctx);
        match ctx.actions.as_slice() {
            [Action::CloseSelectVm(name), Action::Notice(n)] => {
                assert_eq!(name, "deb");
                assert_eq!(n.text(), "created deb");
            }
            other => panic!("{other:?}"),
        }
    }

    /// A long picker error wraps, and the ISO step makes room for all of it
    /// from the first frame on, before the picker was ever drawn this wide.
    #[test]
    fn iso_step_fits_a_wrapped_picker_error_on_the_first_frame() {
        let h = Harness::new();
        let mut form = CreateForm::new();
        form.jump_to(Step::Iso);
        let mut picker = IsoPicker::new("(none) — no boot ISO", "", Vec::new());
        picker.set_error(format!(
            "image not found: /tmp/{}/debian-12.3.0-amd64-netinst.iso",
            ["no-such-directory"; 4].join("/")
        ));
        form.picker = Some(picker);
        let screen = h.render(&mut form, 50, 30);
        assert!(
            screen_contains(&screen, "✗ image not found:"),
            "{}",
            screen.join("\n")
        );
        assert!(
            screen_contains(&screen, "netinst.iso"),
            "{}",
            screen.join("\n")
        );
    }

    #[test]
    fn renders_in_a_tiny_area_without_panicking() {
        let h = Harness::new();
        let mut form = CreateForm::new();
        for step in Step::ALL {
            if step == Step::Iso {
                continue;
            }
            form.jump_to(step);
            for (w, hgt) in [(0, 0), (1, 1), (3, 2), (8, 5), (20, 6), (30, 3)] {
                h.render(&mut form, w, hgt);
            }
        }
        form.jump_to(Step::Network);
        form.bridge_hint = "the host has no bridge br0\nRun this".to_string();
        form.err = "x".repeat(50);
        h.render(&mut form, 12, 4);
        h.render(&mut form, 40, 40);
    }

    #[test]
    fn creates_a_bios_vm_for_real() {
        if which::which("qemu-img").is_err() {
            eprintln!("skipping: qemu-img not installed");
            return;
        }
        let h = Harness::new();
        let mut form = CreateForm::new();
        retype(&mut form, &h, "deb");
        enter(&mut form, &h); // → CPU
        enter(&mut form, &h); // → RAM
        enter(&mut form, &h); // → Disk
        retype(&mut form, &h, "1");
        enter(&mut form, &h); // → Disks
        form.inputs[4].set_value("data:1");
        form.jump_to(Step::Firmware); // over the ISO step
        enter(&mut form, &h); // → TPM
        enter(&mut form, &h); // → Network
        h.press(&mut form, ch('l'));
        h.press(&mut form, ch('l')); // none
        enter(&mut form, &h); // → VNC
        retype(&mut form, &h, "3");
        enter(&mut form, &h); // → Confirm
        assert_eq!(form.step, Step::Confirm, "{}", form.err);
        let screen = h.render(&mut form, 100, 40);
        for want in [
            "Name:     deb",
            "Disk:     1 GiB",
            "Disks:    data:1",
            "Net:      none",
            "VNC:      display 3 (port 5903)",
        ] {
            assert!(screen_contains(&screen, want), "{}", screen.join("\n"));
        }
        assert!(
            enter(&mut form, &h).is_empty(),
            "nothing closes before the result"
        );
        let actions = h.deliver_next(&mut form);
        match actions.as_slice() {
            [Action::CloseSelectVm(name), Action::Notice(n)] => {
                assert_eq!(name, "deb");
                assert_eq!(n.text(), "created deb");
            }
            other => panic!("{other:?} (err {:?})", form.err),
        }
        let cfg = load_config(h.mgr.storage(), "deb").unwrap();
        assert_eq!(
            (cfg.cpu, cfg.ram, cfg.disk_size, cfg.vnc_port),
            (2, 2048, 1, 3)
        );
        assert_eq!(cfg.network.kind, NetworkType::None);
        assert!(!cfg.network.mac.is_empty());
        assert_eq!(
            cfg.disks,
            vec![Disk {
                name: "data".into(),
                size: 1
            }]
        );
        assert!(crate::vm::extra_disk_path(h.mgr.storage(), "deb", "data").exists());

        // The same name is now refused up front.
        let mut again = CreateForm::new();
        retype(&mut again, &h, "deb");
        enter(&mut again, &h);
        assert_eq!(again.err, "a VM named \"deb\" already exists");
    }

    /// Lays down a 1 MiB image file.
    fn write_image(dir: &std::path::Path, name: &str) -> String {
        let path = dir.join(name);
        fs::write(&path, vec![0u8; 1 << 20]).unwrap();
        path.to_string_lossy().into_owned()
    }

    /// Drives the ISO step, which is the picker. The images reach the
    /// picker through a VM that has one as its boot ISO, not through the
    /// app config, which lives under the real `$HOME`.
    #[test]
    fn iso_step() {
        let h = Harness::new();
        let isos = h.dir.path().join("isos");
        fs::create_dir_all(&isos).unwrap();
        let disc = write_image(&isos, "debian.iso");
        let other = write_image(&isos, "other.iso");
        write_vm(
            &h,
            "src",
            VmConfig {
                cpu: 1,
                ram: 128,
                disk_size: 1,
                cdrom_path: disc.clone(),
                ..VmConfig::default()
            },
        );

        let mut form = CreateForm::new();
        while form.step != Step::Iso {
            enter(&mut form, &h);
        }
        assert!(form.picker.is_some());
        let screen = h.render(&mut form, 100, 40);
        for want in [
            "Boot ISO (optional)",
            "(none) — no boot ISO",
            "debian.iso",
            "● 1 MiB",
            "New path",
            "in use by src",
        ] {
            assert!(
                screen_contains(&screen, want),
                "ISO step view lacks {want:?}:\n{}",
                screen.join("\n")
            );
        }
        let hints = form.key_hints();
        assert!(
            hints
                .iter()
                .any(|(k, d)| k == "Enter" && d == "pick and go on"),
            "{hints:?}"
        );
        assert!(
            hints.iter().any(|(k, d)| k == "Tab" && d == "next"),
            "{hints:?}"
        );

        // Enter on the disc takes it and moves on.
        h.press(&mut form, key(KeyCode::Down));
        enter(&mut form, &h);
        assert_eq!(
            (form.step, form.iso.as_str()),
            (Step::Firmware, disc.as_str())
        );
        assert!(form.picker.is_none());
        // Back on the step, Tab moves on without touching it.
        h.press(&mut form, backtab());
        assert_eq!(form.step, Step::Iso);
        assert!(!form.picker.as_ref().unwrap().typed());
        h.press(&mut form, key(KeyCode::Tab));
        assert_eq!(
            (form.step, form.iso.as_str()),
            (Step::Firmware, disc.as_str())
        );
        // A path typed but not entered is taken by Tab as well.
        h.press(&mut form, backtab());
        h.press(&mut form, ch('G'));
        type_str(&mut form, &h, &other);
        assert!(form.picker.as_ref().unwrap().typed());
        h.press(&mut form, key(KeyCode::Tab));
        assert_eq!(
            (form.step, form.iso.as_str()),
            (Step::Firmware, other.as_str())
        );
        // A path that is not there keeps the step, with the reason.
        h.press(&mut form, backtab());
        h.press(&mut form, ch('G')); // typed: the cursor is on the input row holding `other`
        type_str(&mut form, &h, "/nope.iso");
        enter(&mut form, &h);
        assert_eq!((form.step, form.iso.as_str()), (Step::Iso, other.as_str()));
        let screen = h.render(&mut form, 100, 40);
        assert!(
            screen_contains(&screen, "not found"),
            "{}",
            screen.join("\n")
        );
        // (none) clears the choice.
        h.press(&mut form, key(KeyCode::Up));
        h.press(&mut form, key(KeyCode::Up));
        enter(&mut form, &h);
        assert_eq!((form.step, form.iso.as_str()), (Step::Firmware, ""));
        // The summary shows the choice, and it goes into the config.
        h.press(&mut form, backtab());
        h.press(&mut form, key(KeyCode::Down));
        enter(&mut form, &h);
        while form.step != Step::Confirm {
            enter(&mut form, &h);
        }
        assert_eq!(form.build_config().unwrap().cdrom_path, disc);
        let screen = h.render(&mut form, 120, 40);
        assert!(
            screen_contains(&screen, &format!("ISO:      {disc}")),
            "{}",
            screen.join("\n")
        );
        // Esc on the ISO step leaves the wizard like anywhere else.
        form.jump_to(Step::Disks);
        enter(&mut form, &h);
        assert_eq!(form.step, Step::Iso);
        assert!(matches!(
            h.press(&mut form, key(KeyCode::Esc)).as_slice(),
            [Action::Close]
        ));
        // And so does Shift-Tab back to the first step.
        let mut ctx = h.ctx();
        form.handle_paste("/pasted.iso", &mut ctx);
        h.render(&mut form, 100, 40);
        h.render(&mut form, 20, 5);
    }
}
