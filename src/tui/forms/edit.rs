//! The single-page edit form for an existing VM, with the ISO picker as a
//! popup and the two-step confirmation for disk removal.

use std::fs;
use std::path::PathBuf;

use anyhow::anyhow;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

use super::super::dialogs::isopicker::{self, IsoPicker, PickOutcome};
use super::super::events::TaskResult;
use super::super::panel::{Ctx, Panel};
use super::super::theme::Theme;
use super::super::widgets::{
    centered_rect, hints_line, spinner_frame, truncate_left, wrap_text, Selector, TextInput,
};
use super::common::{
    bridge_hint_for, bridge_hint_lines, firmware_choice, firmware_index, form_block, network_index,
    validate_vm_name, FIRMWARE_LABELS, NETWORK_CHOICES, NETWORK_LABELS, TPM_LABELS,
};
use crate::config;
use crate::vm::{
    self, diff_disks, format_disks, format_port_forwards, parse_disks, parse_port_forwards,
    validate_mac, Disk, Manager, VmConfig,
};

/// The fields of the form, in the order they are shown and tabbed through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Name,
    Cpu,
    Ram,
    Disk,
    Disks,
    /// Picker: Enter opens the ISO dialog.
    Iso,
    /// Selector.
    Firmware,
    /// Selector.
    Tpm,
    /// Selector (no text input).
    Network,
    Mac,
    Forwards,
    Vnc,
    /// Button (no text input).
    Save,
}

const FIELDS: [Field; 13] = [
    Field::Name,
    Field::Cpu,
    Field::Ram,
    Field::Disk,
    Field::Disks,
    Field::Iso,
    Field::Firmware,
    Field::Tpm,
    Field::Network,
    Field::Mac,
    Field::Forwards,
    Field::Vnc,
    Field::Save,
];

impl Field {
    fn index(self) -> usize {
        FIELDS.iter().position(|f| *f == self).unwrap_or(0)
    }

    /// The field after this one, wrapping from Save to Name.
    fn next(self) -> Field {
        FIELDS[(self.index() + 1) % FIELDS.len()]
    }

    /// The field before this one, wrapping from Name to Save.
    fn prev(self) -> Field {
        FIELDS[(self.index() + FIELDS.len() - 1) % FIELDS.len()]
    }

    /// Whether the field is a horizontal choice.
    fn selector(self) -> bool {
        matches!(self, Field::Firmware | Field::Tpm | Field::Network)
    }

    /// Whether the field is backed by a text input.
    fn is_text(self) -> bool {
        !self.selector() && self != Field::Save && self != Field::Iso
    }

    fn label(self) -> &'static str {
        match self {
            Field::Name => "Name",
            Field::Cpu => "CPU Cores",
            Field::Ram => "RAM (MiB)",
            Field::Disk => "Disk Size (GiB)",
            Field::Disks => "Extra Disks",
            Field::Iso => "Boot ISO",
            Field::Firmware => "Firmware",
            Field::Tpm => "TPM 2.0",
            Field::Network => "Network",
            Field::Mac => "MAC Address",
            Field::Forwards => "Port Forwards",
            Field::Vnc => "VNC Display",
            Field::Save => "",
        }
    }

    fn help(self) -> &'static str {
        match self {
            Field::Name => "Letters, digits, hyphens and underscores. Renaming requires the VM to be stopped",
            Field::Cpu => "Number of virtual CPU cores, e.g. 2",
            Field::Ram => "Memory in MiB, e.g. 2048 for 2 GiB",
            Field::Disk => "Can only grow, and the VM must be stopped. The guest must extend its own partitions",
            Field::Disks => "Comma-separated [name:]size in GiB, e.g. data:50, scratch:10. A new disk is hot-plugged into a running VM and arrives blank: partition and format it in the guest. Grow or remove only when stopped; removing deletes the image",
            Field::Iso => "Enter opens the images used before, with a row to type the path of a new one; (none) boots from disk. A running VM gets the new disc right away",
            Field::Firmware => "h/l/←/→ to select. VM must be stopped; turning Secure Boot on rebuilds the UEFI NVRAM (boot entries)",
            Field::Tpm => "h/l/←/→ to select. Emulated TPM 2.0 via swtpm — required by Windows 11",
            Field::Network => "h/l/←/→ to select: user (NAT) · tap (bridge) · none",
            Field::Mac => "Leave blank to generate a new random address",
            Field::Forwards => "user networking only. Comma-separated [tcp|udp:]host:guest, e.g. 2222:22, udp:5353:53",
            Field::Vnc => "Display number 1–99 (TCP port = 5900+n). 0 to disable",
            Field::Save => "Press Enter to save changes",
        }
    }
}

/// The banner text after `● running`.
const RUNNING_TEXT: &str =
    "— changes take effect on next start; name, disk sizes, disk removal and firmware are locked; new disks are hot-plugged";
/// The hint over the ISO picker's rows.
const PICKER_HINT: &str =
    "an image used before, or the path of a new one; ~ is your home directory. (none) boots from disk";
/// `▸ ` + a 16-cell label + a space: where the controls start.
const CONTROL_COL: u16 = 19;

/// The edit form. See the spec (06-tui-forms.md) and internal/tui/edit.go.
pub struct EditForm {
    /// The config as loaded; never mutated, so every save diffs against it.
    orig: VmConfig,
    field: Field,
    /// One input per field; the entries of non-text fields are unused.
    inputs: Vec<TextInput>,
    firmware: Selector,
    tpm: Selector,
    network: Selector,
    /// The boot ISO, `""` to boot from disk.
    iso: String,
    /// The ISO dialog, shown over the form while picking.
    picker: Option<IsoPicker>,
    /// What the host lacks for tap networking, `""` when it is ready or
    /// another network is picked.
    bridge_hint: String,
    running: bool,
    /// Removing a disk deletes its image, so the first save with removals
    /// only arms the warning; a second save with the field unchanged
    /// confirms it.
    armed: bool,
    armed_value: String,
    warn: String,
    err: String,
    /// The update is in flight.
    saving: bool,
}

/// What the save task needs, captured before it starts.
struct Save {
    mgr: Manager,
    storage: PathBuf,
    old_name: String,
    cfg: VmConfig,
    /// A boot ISO that was not set before: remember it for the picker.
    new_iso: bool,
    /// The running VM's drive takes the new disc (or none).
    swap_iso: bool,
    /// Indexes into `cfg.disks` of the disks added to the running VM.
    hotplug: Vec<usize>,
}

impl EditForm {
    /// A form pre-filled from `cfg`; `running` is the VM's state when it opened.
    pub fn new(cfg: VmConfig, running: bool) -> Self {
        let mut inputs: Vec<TextInput> = FIELDS
            .iter()
            .map(|f| {
                let value = match f {
                    Field::Name => cfg.name.clone(),
                    Field::Cpu => cfg.cpu.to_string(),
                    Field::Ram => cfg.ram.to_string(),
                    Field::Disk => cfg.disk_size.to_string(),
                    Field::Disks => format_disks(&cfg.disks),
                    Field::Mac => cfg.network.mac.clone(),
                    Field::Forwards => format_port_forwards(&cfg.network.port_forwards),
                    Field::Vnc => cfg.vnc_port.to_string(),
                    _ => String::new(),
                };
                TextInput::new().with_char_limit(256).with_value(&value)
            })
            .collect();
        inputs[Field::Name.index()].focus();
        let net_index = network_index(cfg.network.kind);
        EditForm {
            firmware: Selector::new(FIRMWARE_LABELS, firmware_index(&cfg)),
            tpm: Selector::new(TPM_LABELS, usize::from(cfg.tpm)),
            network: Selector::new(NETWORK_LABELS, net_index),
            iso: cfg.cdrom_path.clone(),
            picker: None,
            bridge_hint: bridge_hint_for(NETWORK_CHOICES[net_index]),
            running,
            armed: false,
            armed_value: String::new(),
            warn: String::new(),
            err: String::new(),
            saving: false,
            field: Field::Name,
            inputs,
            orig: cfg,
        }
    }

    /// The trimmed value of a text field.
    fn value(&self, f: Field) -> String {
        self.inputs[f.index()].trimmed()
    }

    fn move_to(&mut self, f: Field) {
        if self.field.is_text() {
            self.inputs[self.field.index()].blur();
        }
        self.field = f;
        if f.is_text() {
            self.inputs[f.index()].focus();
        }
    }

    /// Moves the current field's selector by `delta`, if it has one.
    fn cycle(&mut self, delta: i32) {
        match self.field {
            Field::Firmware => self.firmware.cycle(delta),
            Field::Tpm => self.tpm.cycle(delta),
            Field::Network => {
                self.network.cycle(delta);
                self.bridge_hint = bridge_hint_for(NETWORK_CHOICES[self.network.index]);
            }
            _ => {}
        }
    }

    fn selector(&self, f: Field) -> &Selector {
        match f {
            Field::Firmware => &self.firmware,
            Field::Tpm => &self.tpm,
            _ => &self.network,
        }
    }

    /// Shows the ISO dialog with the cursor on the current boot ISO.
    fn open_picker(&mut self, ctx: &Ctx) {
        self.err.clear();
        self.picker = Some(IsoPicker::new(
            "(none) — boot from disk",
            &self.iso,
            isopicker::iso_entries(ctx.storage()),
        ));
    }

    /// The removal being confirmed is not the one on screen any more.
    fn disarm_if_changed(&mut self) {
        if self.armed && self.value(Field::Disks) != self.armed_value {
            self.armed = false;
            self.armed_value.clear();
            self.warn.clear();
        }
    }

    /// Validates the form and returns the edited config. On failure it also
    /// returns the offending field so the cursor can be moved there.
    fn build_config(&self, mgr: &Manager) -> Result<VmConfig, (Field, anyhow::Error)> {
        let mut cfg = self.orig.clone();

        cfg.name = self.value(Field::Name);
        validate_vm_name(&cfg.name).map_err(|e| (Field::Name, e))?;
        if cfg.name != self.orig.name && mgr.exists(&cfg.name) {
            return Err((
                Field::Name,
                anyhow!("a VM named {:?} already exists", cfg.name),
            ));
        }
        if cfg.name != self.orig.name && self.running {
            return Err((Field::Name, anyhow!("stop the VM before renaming it")));
        }

        cfg.cpu = match parse_int(&self.value(Field::Cpu)).and_then(|n| u32::try_from(n).ok()) {
            Some(n) if n >= 1 => n,
            _ => return Err((Field::Cpu, anyhow!("CPU must be a positive integer"))),
        };
        cfg.ram = match parse_int(&self.value(Field::Ram)).and_then(|n| u32::try_from(n).ok()) {
            Some(n) if n >= 64 => n,
            _ => return Err((Field::Ram, anyhow!("RAM must be at least 64 MiB"))),
        };

        let Some(disk_size) = parse_int(&self.value(Field::Disk)) else {
            return Err((Field::Disk, anyhow!("disk size must be an integer")));
        };
        if disk_size < i64::from(self.orig.disk_size) {
            return Err((
                Field::Disk,
                anyhow!("disk can only grow (currently {} GiB)", self.orig.disk_size),
            ));
        }
        cfg.disk_size = u32::try_from(disk_size)
            .map_err(|_| (Field::Disk, anyhow!("disk size must be an integer")))?;
        if cfg.disk_size != self.orig.disk_size && self.running {
            return Err((Field::Disk, anyhow!("stop the VM before resizing its disk")));
        }

        cfg.disks = parse_disks(&self.value(Field::Disks)).map_err(|e| (Field::Disks, e))?;
        let disks = diff_disks(&self.orig.disks, &cfg.disks).map_err(|e| (Field::Disks, e))?;
        if self.running && (!disks.grown.is_empty() || !disks.removed.is_empty()) {
            return Err((
                Field::Disks,
                anyhow!("stop the VM before resizing or removing disks"),
            ));
        }

        cfg.cdrom_path.clone_from(&self.iso);
        if !cfg.cdrom_path.is_empty() {
            match fs::metadata(&cfg.cdrom_path) {
                Ok(st) if !st.is_dir() => {}
                _ => {
                    return Err((
                        Field::Iso,
                        anyhow!("ISO file not found: {}", cfg.cdrom_path),
                    ))
                }
            }
        }

        let (firmware, secure_boot) = firmware_choice(self.firmware.index);
        cfg.firmware = firmware;
        cfg.secure_boot = secure_boot;
        if self.running
            && (cfg.uefi() != self.orig.uefi() || cfg.secure_boot != self.orig.secure_boot)
        {
            return Err((
                Field::Firmware,
                anyhow!("stop the VM before changing its firmware"),
            ));
        }
        cfg.tpm = self.tpm.index == 1;

        cfg.network.kind = NETWORK_CHOICES[self.network.index];
        cfg.network.mac = self.value(Field::Mac);
        if !cfg.network.mac.is_empty() {
            validate_mac(&cfg.network.mac).map_err(|e| (Field::Mac, e))?;
        }
        cfg.network.port_forwards =
            parse_port_forwards(&self.value(Field::Forwards)).map_err(|e| (Field::Forwards, e))?;

        cfg.vnc_port = match parse_int(&self.value(Field::Vnc)) {
            Some(n) if (0..=99).contains(&n) => n as u16,
            _ => {
                return Err((
                    Field::Vnc,
                    anyhow!("VNC display must be 0 (disabled) or 1–99"),
                ))
            }
        };

        Ok(cfg)
    }

    fn save(&mut self, ctx: &mut Ctx) {
        let cfg = match self.build_config(ctx.mgr) {
            Ok(cfg) => cfg,
            Err((field, err)) => {
                self.move_to(field);
                self.err = format!("{err:#}");
                return;
            }
        };
        self.err.clear();

        // Removing a disk deletes its image for good, so it takes a second
        // save with the same field value to confirm. build_config validated
        // the diff.
        let disks = diff_disks(&self.orig.disks, &cfg.disks).unwrap_or_default();
        if !disks.removed.is_empty()
            && !(self.armed && self.value(Field::Disks) == self.armed_value)
        {
            self.armed = true;
            self.armed_value = self.value(Field::Disks);
            self.warn = format!(
                "Removing {} deletes the image files and everything on them — press Ctrl-s again to confirm",
                describe_disks(&disks.removed)
            );
            self.move_to(Field::Disks);
            return;
        }
        self.armed = false;
        self.armed_value.clear();
        self.warn.clear();

        let new_iso = !cfg.cdrom_path.is_empty() && cfg.cdrom_path != self.orig.cdrom_path;
        let swap_iso = self.running && cfg.cdrom_path != self.orig.cdrom_path;
        // A disk added to a running VM is hot-plugged once its image exists.
        let hotplug = if self.running {
            cfg.disks
                .iter()
                .enumerate()
                .filter(|(_, d)| disks.added.iter().any(|a| a.name == d.name))
                .map(|(i, _)| i)
                .collect()
        } else {
            Vec::new()
        };
        let save = Save {
            mgr: ctx.mgr.clone(),
            storage: ctx.storage_buf(),
            old_name: self.orig.name.clone(),
            cfg,
            new_iso,
            swap_iso,
            hotplug,
        };
        self.saving = true;
        ctx.spawn(move || save.run());
    }

    /// The picker's hints plus the way back to the form.
    fn picker_hints(picker: &IsoPicker) -> Vec<(String, String)> {
        let mut hints = picker.key_hints("pick");
        hints.push(("Esc".to_string(), "back to the form".to_string()));
        hints
    }

    /// Draws the ISO dialog centred on the frame, over the form.
    fn render_picker(&mut self, frame: &mut Frame, theme: &Theme) {
        let Some(picker) = self.picker.as_mut() else {
            return;
        };
        let area = frame.area();
        let width = (area.width * 3 / 4).clamp(40, 110).min(area.width);
        let hint = wrap_text(PICKER_HINT, usize::from(width.saturating_sub(4)).max(1));
        let height = (hint.len() as u16)
            .saturating_add(3)
            // The rows' width: inside the border and the 1-cell margins.
            .saturating_add(picker.height_for(width.saturating_sub(4)))
            .min(area.height);
        let rect = centered_rect(area, width, height);
        if rect.width < 4 || rect.height < 3 {
            return;
        }
        let hints = Self::picker_hints(picker);
        let pairs: Vec<(&str, &str)> = hints
            .iter()
            .map(|(k, d)| (k.as_str(), d.as_str()))
            .collect();
        // Padded like the other popups' footers, clear of the corner.
        let mut footer = hints_line(&pairs, theme);
        footer.spans.insert(0, Span::raw(" "));
        footer.spans.push(Span::raw(" "));
        frame.render_widget(Clear, rect);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme.border_focused)
            .title(Line::styled(" Boot ISO ", theme.title))
            .title_bottom(footer.right_aligned());
        let inner = block.inner(rect).inner(Margin::new(1, 0));
        frame.render_widget(block, rect);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        let [hint_area, _, rows_area] = Layout::vertical([
            Constraint::Length(hint.len() as u16),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .areas(inner);
        let hint: Vec<Line> = hint
            .into_iter()
            .map(|l| Line::styled(l, theme.help))
            .collect();
        frame.render_widget(Paragraph::new(hint), hint_area);
        picker.render(frame, rows_area, theme);
    }
}

impl Save {
    fn run(self) -> TaskResult {
        let Save {
            mgr,
            storage,
            old_name,
            mut cfg,
            new_iso,
            swap_iso,
            hotplug,
        } = self;
        if let Err(err) = mgr.update(&old_name, &mut cfg) {
            return TaskResult::VmUpdateFailed {
                err: format!("{err:#}"),
            };
        }
        if new_iso {
            // For the picker; losing it costs nothing.
            let _ = config::remember_iso(&cfg.cdrom_path);
        }
        // What can change under a running VM goes through the monitor on the
        // spot: the CD-ROM drive takes the new disc (or none), new disks are
        // plugged in. The config is saved either way.
        let mut errs = Vec::new();
        if swap_iso {
            if let Err(err) = vm::cdrom_change(&storage, &cfg.name, &cfg.cdrom_path) {
                errs.push(format!("the CD-ROM drive could not be changed:\n{err:#}"));
            }
        }
        for i in hotplug {
            if let Err(err) = vm::disk_hotplug(&storage, &cfg, i) {
                errs.push(format!(
                    "disk {:?} could not be hot-plugged:\n{err:#}",
                    cfg.disks[i].name
                ));
            }
        }
        if !errs.is_empty() {
            return TaskResult::VmUpdateFailed {
                err: format!(
                    "saved, but this could not be applied to the running VM (takes effect on next start):\n{}",
                    errs.join("\n")
                ),
            };
        }
        TaskResult::VmUpdated { name: cfg.name }
    }
}

/// Go's `strconv.Atoi`: an optional sign and digits, nothing else.
fn parse_int(s: &str) -> Option<i64> {
    s.parse().ok()
}

/// Lists disks as `data (50 GiB), scratch (10 GiB)`.
fn describe_disks(disks: &[Disk]) -> String {
    disks
        .iter()
        .map(|d| format!("{} ({} GiB)", d.name, d.size))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `prefix` followed by `text` in `style`, word-wrapped to `width` cells;
/// the lines after the first are indented by two cells.
fn flow(prefix: Vec<Span<'static>>, text: &str, style: Style, width: usize) -> Vec<Line<'static>> {
    let prefix_w: usize = prefix.iter().map(Span::width).sum();
    let mut out = Vec::new();
    let mut prefix = Some(prefix);
    for raw in text.split('\n') {
        let avail = if prefix.is_some() {
            width.saturating_sub(prefix_w)
        } else {
            width.saturating_sub(2)
        };
        for piece in wrap_text(raw, avail.max(8)) {
            match prefix.take() {
                Some(mut spans) => {
                    spans.push(Span::styled(piece, style));
                    out.push(Line::from(spans));
                }
                None => out.push(Line::styled(format!("  {piece}"), style)),
            }
        }
    }
    out
}

/// One screen row of the form.
enum Row {
    Line(Line<'static>),
    /// The focused text field: its marker and label, then the input itself
    /// (drawn with the terminal cursor).
    Input(Line<'static>, usize),
}

impl Panel for EditForm {
    fn title(&self) -> String {
        format!("Edit VM: {}", self.orig.name)
    }

    fn handle_key(&mut self, key: KeyEvent, ctx: &mut Ctx) {
        if self.saving {
            return;
        }
        if let Some(picker) = self.picker.as_mut() {
            match picker.handle_key(key, ctx.home) {
                PickOutcome::Nothing => {}
                PickOutcome::Picked(path) => {
                    self.iso = path;
                    self.picker = None;
                }
                PickOutcome::Cancelled => self.picker = None,
            }
            return;
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let plain = !ctrl && !key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc => {
                ctx.close();
                return;
            }
            KeyCode::Char('s') if ctrl => {
                self.save(ctx);
                return;
            }
            KeyCode::Enter => {
                match self.field {
                    Field::Save => self.save(ctx),
                    Field::Iso => self.open_picker(ctx),
                    _ => self.move_to(self.field.next()),
                }
                return;
            }
            KeyCode::Tab | KeyCode::Down => {
                self.move_to(self.field.next());
                return;
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.move_to(self.field.prev());
                return;
            }
            // vim-next/prev: only when no text input is active.
            KeyCode::Char('j') if plain && !self.field.is_text() => {
                self.move_to(self.field.next());
                return;
            }
            KeyCode::Char('k') if plain && !self.field.is_text() => {
                self.move_to(self.field.prev());
                return;
            }
            KeyCode::Char('h') | KeyCode::Left if plain => self.cycle(-1),
            KeyCode::Char('l') | KeyCode::Right if plain => {
                if self.field == Field::Iso {
                    self.open_picker(ctx);
                    return;
                }
                self.cycle(1);
            }
            _ => {}
        }

        // Forward keystrokes to the active text input.
        if self.field.is_text() {
            self.inputs[self.field.index()].handle_key(key);
            self.disarm_if_changed();
        }
    }

    fn handle_paste(&mut self, text: &str, _ctx: &mut Ctx) {
        if self.saving {
            return;
        }
        if let Some(picker) = self.picker.as_mut() {
            picker.handle_paste(text);
            return;
        }
        if self.field.is_text() {
            self.inputs[self.field.index()].insert_str(text);
            self.disarm_if_changed();
        }
    }

    fn on_task(&mut self, result: TaskResult, ctx: &mut Ctx) {
        match result {
            TaskResult::VmUpdated { name } => {
                self.saving = false;
                ctx.close_select_vm(name.clone());
                ctx.notice_ok(format!("saved {name}"));
            }
            TaskResult::VmUpdateFailed { err } | TaskResult::Failed { err, .. } => {
                self.saving = false;
                self.err = err;
            }
            _ => {}
        }
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme, tick: u64) {
        let inner = form_block(frame, area, &self.title(), theme);
        if inner.width == 0 || inner.height == 0 {
            self.render_picker(frame, theme);
            return;
        }
        let width = usize::from(inner.width);
        let blank = || Row::Line(Line::raw(""));

        let mut rows: Vec<Row> = Vec::new();
        let mut focus_row = 0;
        if self.running {
            let banner = vec![Span::styled("● running ", theme.running)];
            rows.extend(
                flow(banner, RUNNING_TEXT, theme.help, width)
                    .into_iter()
                    .map(Row::Line),
            );
            rows.push(blank());
        }
        for f in FIELDS {
            let focused = f == self.field;
            if f == Field::Save {
                rows.push(blank());
                if focused {
                    focus_row = rows.len();
                }
                let style = if focused {
                    theme.selected
                } else {
                    theme.normal
                };
                rows.push(Row::Line(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(" Save ", style),
                ])));
                continue;
            }
            let (marker, label_style) = if focused {
                ("▸ ", theme.label)
            } else {
                ("  ", theme.help)
            };
            // Every choice of a selector is padded with a space on each
            // side, so its label takes no gap of its own: the first choice's
            // text then starts at CONTROL_COL like the text values.
            let label = if f.selector() {
                format!("{:<16}", f.label())
            } else {
                format!("{:<16} ", f.label())
            };
            let mut spans = vec![
                Span::styled(marker, theme.label),
                Span::styled(label, label_style),
            ];
            if focused {
                focus_row = rows.len();
            }
            if f.selector() {
                // Wrapped under the first choice where the pane is narrow,
                // so the current value never falls off its right edge.
                let lead = usize::from(CONTROL_COL) - 1;
                let lines = self.selector(f).lines(theme, width.saturating_sub(lead));
                for (i, line) in lines.into_iter().enumerate() {
                    if i > 0 {
                        spans = vec![Span::raw(" ".repeat(lead))];
                    }
                    spans.extend(line.spans);
                    rows.push(Row::Line(Line::from(std::mem::take(&mut spans))));
                }
            } else if f == Field::Iso {
                if self.iso.is_empty() {
                    spans.push(Span::styled("(none)", theme.help));
                } else {
                    let avail = width.saturating_sub(usize::from(CONTROL_COL));
                    spans.push(Span::styled(truncate_left(&self.iso, avail), theme.normal));
                }
                rows.push(Row::Line(Line::from(spans)));
            } else if focused {
                rows.push(Row::Input(Line::from(spans), f.index()));
            } else {
                spans.push(self.inputs[f.index()].as_span(theme));
                rows.push(Row::Line(Line::from(spans)));
            }
        }

        rows.push(blank());
        // The paragraphs under the fields, as row ranges.
        let mut paragraphs = Vec::new();
        let start = rows.len();
        for l in wrap_text(self.field.help(), width.saturating_sub(2).max(8)) {
            rows.push(Row::Line(Line::styled(format!("  {l}"), theme.help)));
        }
        paragraphs.push(start..rows.len());
        rows.push(blank());
        let hint = bridge_hint_lines(&self.bridge_hint, width, theme);
        if !hint.is_empty() {
            let start = rows.len();
            rows.extend(hint.into_iter().map(Row::Line));
            paragraphs.push(start..rows.len());
            rows.push(blank());
        }
        // The outcome under the form: the warning, the error, the spinner,
        // a blank row between them.
        let mut groups: Vec<Vec<Line<'static>>> = Vec::new();
        if !self.warn.is_empty() {
            groups.push(flow(
                vec![Span::styled("⚠ ", theme.warn)],
                &self.warn,
                theme.warn,
                width,
            ));
        }
        if !self.err.is_empty() {
            groups.push(flow(
                vec![Span::styled("✗ ", theme.error)],
                &self.err,
                theme.error,
                width,
            ));
        }
        if self.saving {
            groups.push(vec![Line::from(vec![
                Span::styled(spinner_frame(tick), theme.spinner),
                Span::styled(" Saving…", theme.normal),
            ])]);
        }
        let mut outcome: Vec<Line<'static>> = Vec::new();
        for (i, lines) in groups.into_iter().enumerate() {
            if i > 0 {
                outcome.push(Line::raw(""));
            }
            outcome.extend(lines);
        }

        // When everything fits, the outcome follows the form. Otherwise the
        // outcome keeps the rows it needs at the bottom of the pane (and a
        // blank row above it when there is room), and the form scrolls in
        // the rows left to keep the focused row in view: the field the
        // cursor is on and what Ctrl-s said about it are both on screen
        // (the field keeps at least one row).
        let height = usize::from(inner.height);
        let (body_h, out_h, gap) = if rows.len() + outcome.len() <= height {
            (rows.len(), outcome.len(), 0)
        } else {
            let out_h = outcome.len().min(height - 1);
            let gap = usize::from(out_h > 0 && height - out_h >= 2);
            (height - out_h - gap, out_h, gap)
        };
        let offset = (focus_row + 1).saturating_sub(body_h);
        let mut end = offset + body_h;
        let mut gap = gap;
        if out_h > 0 {
            // Over the outcome, a paragraph the window would cut is left out
            // rather than read as the start of the message; the blank row
            // before it then separates the outcome.
            for p in &paragraphs {
                if p.start < end && end < p.end {
                    end = p.start;
                    gap = 0;
                }
            }
        }
        let out_y = inner.y + (end - offset + gap) as u16;
        for (y, line) in (out_y..).zip(outcome.into_iter().take(out_h)) {
            frame.render_widget(line, Rect::new(inner.x, y, inner.width, 1));
        }
        for (y, row) in (inner.y..).zip(rows.into_iter().take(end).skip(offset)) {
            match row {
                Row::Line(line) => frame.render_widget(line, Rect::new(inner.x, y, inner.width, 1)),
                Row::Input(prefix, idx) => {
                    let pw = CONTROL_COL.min(inner.width);
                    frame.render_widget(prefix, Rect::new(inner.x, y, pw, 1));
                    self.inputs[idx].render(
                        frame,
                        Rect::new(inner.x + pw, y, inner.width - pw, 1),
                        theme,
                    );
                }
            }
        }

        self.render_picker(frame, theme);
    }

    fn key_hints(&self) -> Vec<(String, String)> {
        if let Some(picker) = &self.picker {
            return Self::picker_hints(picker);
        }
        if self.saving {
            // Every key but Ctrl-c is swallowed while saving; the body shows
            // the progress line.
            return vec![("Ctrl-c".to_string(), "quit".to_string())];
        }
        let pairs: &[(&str, &str)] = if self.field.selector() {
            &[
                ("h/l/←/→", "select"),
                ("j/↓", "next"),
                ("k/↑", "back"),
                ("Ctrl-s", "save"),
                ("Esc", "cancel"),
            ]
        } else if self.field == Field::Iso {
            &[
                ("Enter/l", "pick ISO"),
                ("j/↓", "next"),
                ("k/↑", "back"),
                ("Ctrl-s", "save"),
                ("Esc", "cancel"),
            ]
        } else if self.field == Field::Save {
            &[
                ("Enter", "save"),
                ("k/Shift-Tab", "back"),
                ("Esc", "cancel"),
            ]
        } else {
            &[
                ("Tab/↓", "next"),
                ("Shift-Tab/↑", "back"),
                ("Ctrl-s", "save"),
                ("Esc", "cancel"),
            ]
        };
        pairs
            .iter()
            .map(|(k, d)| (k.to_string(), d.to_string()))
            .collect()
    }

    fn busy(&self) -> Option<String> {
        self.saving.then(|| format!("saving {}…", self.orig.name))
    }

    fn failed(&self) -> bool {
        !self.err.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use super::*;
    use crate::tui::panel::Action;
    use crate::tui::testutil::*;
    use crate::vm::{
        extra_disk_path, load_config, save_config, vm_dir, FirmwareType, NetworkConfig, NetworkType,
    };

    fn disk(name: &str, size: u32) -> Disk {
        Disk {
            name: name.into(),
            size,
        }
    }

    /// A BIOS VM with no network, the shape the Go tests start from.
    fn bios_vm(name: &str, disk_size: u32, disks: Vec<Disk>) -> VmConfig {
        VmConfig {
            name: name.into(),
            cpu: 1,
            ram: 512,
            disk_size,
            network: NetworkConfig {
                kind: NetworkType::None,
                ..Default::default()
            },
            disks,
            ..Default::default()
        }
    }

    /// Writes a `vm.yaml` for `cfg` without going through `create`.
    fn write_vm(storage: &Path, cfg: &VmConfig) {
        fs::create_dir_all(vm_dir(storage, &cfg.name)).unwrap();
        save_config(storage, cfg).unwrap();
    }

    fn set(form: &mut EditForm, f: Field, value: &str) {
        form.inputs[f.index()].set_value(value);
    }

    fn tab_to(h: &Harness, form: &mut EditForm, f: Field) {
        while form.field != f {
            h.press(form, key(KeyCode::Tab));
        }
    }

    /// Presses Ctrl-s and checks that the save did not go ahead.
    fn not_saved(h: &Harness, form: &mut EditForm, what: &str) {
        let acts = h.press(form, ctrl('s'));
        assert!(acts.is_empty(), "{what}: actions {acts:?}");
        assert!(!form.saving, "{what}: the save went ahead");
        assert!(
            h.next_result(Duration::from_millis(50)).is_none(),
            "{what}: the save went ahead"
        );
    }

    /// Presses Ctrl-s expecting a refusal, and returns the error shown.
    fn refused(h: &Harness, form: &mut EditForm, field: Field, what: &str) -> String {
        not_saved(h, form, what);
        assert_eq!(
            form.field, field,
            "{what}: cursor on {:?}, want {field:?}",
            form.field
        );
        assert!(!form.err.is_empty(), "{what}: no error shown");
        form.err.clone()
    }

    #[test]
    fn prefills_from_the_config() {
        let cfg = VmConfig {
            name: "win".into(),
            cpu: 2,
            ram: 4096,
            disk_size: 64,
            cdrom_path: "/isos/win11.iso".into(),
            secure_boot: true,
            tpm: true,
            network: NetworkConfig {
                kind: NetworkType::Tap,
                mac: "52:54:00:00:00:01".into(),
                port_forwards: crate::vm::parse_port_forwards("2222:22, udp:5353:53").unwrap(),
            },
            vnc_port: 3,
            disks: vec![disk("data", 50)],
            ..Default::default()
        };
        let form = EditForm::new(cfg, true);
        assert_eq!(form.title(), "Edit VM: win");
        assert_eq!(form.value(Field::Name), "win");
        assert_eq!(form.value(Field::Cpu), "2");
        assert_eq!(form.value(Field::Ram), "4096");
        assert_eq!(form.value(Field::Disk), "64");
        assert_eq!(form.value(Field::Disks), "data:50");
        assert_eq!(form.value(Field::Mac), "52:54:00:00:00:01");
        assert_eq!(form.value(Field::Forwards), "tcp:2222:22, udp:5353:53");
        assert_eq!(form.value(Field::Vnc), "3");
        assert_eq!(form.iso, "/isos/win11.iso");
        assert_eq!(
            (form.firmware.index, form.tpm.index, form.network.index),
            (2, 1, 1)
        );
        assert_eq!(
            form.bridge_hint,
            bridge_hint_for(NetworkType::Tap),
            "tap inspects the host on open"
        );
        assert!(form.running && form.field == Field::Name && form.inputs[0].focused);
        assert!(form.busy().is_none());
    }

    /// Port of TestEditFormFirmwareFields.
    #[test]
    fn firmware_fields() {
        let h = Harness::new();
        let cfg = VmConfig {
            name: "win".into(),
            cpu: 2,
            ram: 4096,
            disk_size: 64,
            secure_boot: true,
            tpm: true,
            network: NetworkConfig {
                kind: NetworkType::User,
                mac: "52:54:00:00:00:01".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut form = EditForm::new(cfg, false);
        assert_eq!(
            (form.firmware.index, form.tpm.index),
            (2, 1),
            "selectors not pre-filled"
        );
        let lines = h.render(&mut form, 100, 40);
        for want in ["Firmware", "TPM 2.0", "UEFI + Secure Boot", "enabled"] {
            assert!(
                screen_contains(&lines, want),
                "view lacks {want:?}:\n{}",
                lines.join("\n")
            );
        }
        tab_to(&h, &mut form, Field::Firmware);
        h.press(&mut form, ch('h')); // Secure Boot → UEFI
        h.press(&mut form, key(KeyCode::Tab));
        h.press(&mut form, ch('l')); // TPM enabled → disabled
        let out = form.build_config(&h.mgr).unwrap();
        assert_eq!(out.firmware, FirmwareType::Uefi);
        assert!(!out.secure_boot && !out.tpm, "got {out:?}");

        // A running VM may not change firmware.
        form.running = true;
        let (field, err) = form.build_config(&h.mgr).unwrap_err();
        assert_eq!(field, Field::Firmware);
        assert_eq!(err.to_string(), "stop the VM before changing its firmware");
        let err = refused(&h, &mut form, Field::Firmware, "running firmware change");
        assert_eq!(err, "stop the VM before changing its firmware");
        let lines = h.render(&mut form, 100, 40);
        assert!(
            screen_contains(&lines, "✗ stop the VM before changing its firmware"),
            "{}",
            lines.join("\n")
        );
    }

    #[test]
    fn keys_move_between_fields_like_go() {
        let h = Harness::new();
        let mut form = EditForm::new(bios_vm("deb", 1, vec![]), false);
        // Tab/Enter/↓ go down and wrap; Shift-Tab/↑ go up and wrap.
        h.press(&mut form, key(KeyCode::Tab));
        assert_eq!(form.field, Field::Cpu);
        h.press(&mut form, key(KeyCode::Enter));
        assert_eq!(form.field, Field::Ram);
        h.press(&mut form, key(KeyCode::Down));
        assert_eq!(form.field, Field::Disk);
        h.press(&mut form, backtab());
        h.press(&mut form, key(KeyCode::Up));
        assert_eq!(form.field, Field::Cpu);
        assert!(
            form.inputs[Field::Cpu.index()].focused && !form.inputs[Field::Name.index()].focused
        );
        h.press(&mut form, backtab());
        h.press(&mut form, backtab());
        assert_eq!(form.field, Field::Save, "Shift-Tab wraps from Name to Save");
        h.press(&mut form, key(KeyCode::Tab));
        assert_eq!(form.field, Field::Name, "Tab wraps from Save to Name");

        // j/k type into a text field, move off one.
        h.press(&mut form, ch('j'));
        h.press(&mut form, ch('k'));
        assert_eq!(form.value(Field::Name), "debjk");
        tab_to(&h, &mut form, Field::Iso);
        h.press(&mut form, ch('j'));
        assert_eq!(form.field, Field::Firmware, "j on the ISO field");
        h.press(&mut form, ch('k'));
        assert_eq!(form.field, Field::Iso, "k on the ISO field");
        h.press(&mut form, ch('h'));
        assert_eq!(form.field, Field::Iso, "h on the ISO field does nothing");

        // Selectors cycle with h/l/←/→ and wrap.
        tab_to(&h, &mut form, Field::Firmware);
        h.press(&mut form, ch('l'));
        assert_eq!(form.firmware.index, 1);
        h.press(&mut form, key(KeyCode::Right));
        assert_eq!(form.firmware.index, 2);
        h.press(&mut form, ch('l'));
        assert_eq!(form.firmware.index, 0, "wraps forward");
        h.press(&mut form, key(KeyCode::Left));
        assert_eq!(form.firmware.index, 2, "wraps backward");
        h.press(&mut form, ch('j'));
        h.press(&mut form, ch('l'));
        assert_eq!((form.field, form.tpm.index), (Field::Tpm, 1));
        h.press(&mut form, ch('j'));
        assert_eq!(
            (form.field, form.network.index),
            (Field::Network, 2),
            "the VM has no network"
        );
        assert!(form.bridge_hint.is_empty(), "no hint for no networking");
        h.press(&mut form, ch('l'));
        assert_eq!(form.network.index, 0, "wraps to user");
        assert!(form.bridge_hint.is_empty(), "no hint for user networking");
        h.press(&mut form, key(KeyCode::Right));
        assert_eq!(form.network.index, 1);
        assert_eq!(
            form.bridge_hint,
            bridge_hint_for(NetworkType::Tap),
            "tap inspects the host"
        );
        h.press(&mut form, ch('h'));
        assert_eq!(form.network.index, 0);
        assert!(form.bridge_hint.is_empty(), "the hint goes with tap");

        // The Save button: j → Name, k → VNC.
        tab_to(&h, &mut form, Field::Save);
        h.press(&mut form, ch('k'));
        assert_eq!(form.field, Field::Vnc);
        h.press(&mut form, key(KeyCode::Tab));
        h.press(&mut form, ch('j'));
        assert_eq!(form.field, Field::Name);

        // Esc leaves, discarding the edits.
        let acts = h.press(&mut form, key(KeyCode::Esc));
        assert!(matches!(acts.as_slice(), [Action::Close]), "{acts:?}");
    }

    #[test]
    fn key_hints_follow_the_field() {
        let h = Harness::new();
        let mut form = EditForm::new(bios_vm("deb", 1, vec![]), false);
        let keys = |form: &EditForm| {
            form.key_hints()
                .into_iter()
                .map(|(k, _)| k)
                .collect::<Vec<_>>()
        };
        assert_eq!(keys(&form), ["Tab/↓", "Shift-Tab/↑", "Ctrl-s", "Esc"]);
        tab_to(&h, &mut form, Field::Iso);
        assert_eq!(keys(&form)[0], "Enter/l");
        tab_to(&h, &mut form, Field::Network);
        assert_eq!(keys(&form)[0], "h/l/←/→");
        tab_to(&h, &mut form, Field::Save);
        assert_eq!(keys(&form), ["Enter", "k/Shift-Tab", "Esc"]);
        form.saving = true;
        assert_eq!(
            form.key_hints(),
            vec![("Ctrl-c".to_string(), "quit".to_string())],
            "saving: only the key that still works"
        );
        assert_eq!(form.busy().as_deref(), Some("saving deb…"));
        // Keys are swallowed while the update runs.
        let acts = h.press(&mut form, key(KeyCode::Esc));
        assert!(acts.is_empty());
        assert_eq!(form.field, Field::Save);
    }

    #[test]
    fn validation_errors_name_the_field() {
        let h = Harness::new();
        let storage = h.mgr.storage();
        write_vm(storage, &bios_vm("other", 1, vec![]));
        let orig = VmConfig {
            cdrom_path: String::new(),
            network: NetworkConfig {
                kind: NetworkType::User,
                mac: "52:54:00:00:00:01".into(),
                ..Default::default()
            },
            ..bios_vm("deb", 2, vec![disk("data", 1)])
        };
        let mut form = EditForm::new(orig.clone(), false);
        tab_to(&h, &mut form, Field::Vnc);

        set(&mut form, Field::Name, "");
        assert_eq!(
            refused(&h, &mut form, Field::Name, "empty name"),
            "name cannot be empty"
        );
        set(&mut form, Field::Name, "a b");
        assert_eq!(
            refused(&h, &mut form, Field::Name, "bad name"),
            "name may only contain letters, digits, hyphens and underscores"
        );
        set(&mut form, Field::Name, "other");
        assert_eq!(
            refused(&h, &mut form, Field::Name, "taken name"),
            "a VM named \"other\" already exists"
        );
        form.running = true;
        set(&mut form, Field::Name, "deb2");
        assert_eq!(
            refused(&h, &mut form, Field::Name, "rename while running"),
            "stop the VM before renaming it"
        );
        form.running = false;
        set(&mut form, Field::Name, "deb2");
        assert!(
            form.build_config(&h.mgr).is_ok(),
            "a free name on a stopped VM: {:?}",
            form.err
        );
        set(&mut form, Field::Name, "deb");

        for bad in ["0", "-1", "x", "", "1.5"] {
            set(&mut form, Field::Cpu, bad);
            assert_eq!(
                refused(&h, &mut form, Field::Cpu, "cpu"),
                "CPU must be a positive integer",
                "cpu {bad:?}"
            );
        }
        set(&mut form, Field::Cpu, "+2");
        set(&mut form, Field::Ram, "63");
        assert_eq!(
            refused(&h, &mut form, Field::Ram, "ram"),
            "RAM must be at least 64 MiB"
        );
        set(&mut form, Field::Ram, "64");

        set(&mut form, Field::Disk, "2G");
        assert_eq!(
            refused(&h, &mut form, Field::Disk, "disk"),
            "disk size must be an integer"
        );
        set(&mut form, Field::Disk, "1");
        assert_eq!(
            refused(&h, &mut form, Field::Disk, "shrink"),
            "disk can only grow (currently 2 GiB)"
        );
        form.running = true;
        set(&mut form, Field::Disk, "3");
        assert_eq!(
            refused(&h, &mut form, Field::Disk, "resize running"),
            "stop the VM before resizing its disk"
        );
        set(&mut form, Field::Disk, "2");
        // A bad disk list hides a bad VNC display: the first failing field wins.
        set(&mut form, Field::Vnc, "100");
        set(&mut form, Field::Disks, "data:1, nope");
        assert!(refused(&h, &mut form, Field::Disks, "bad disk").contains("expected [name:]size"));
        set(&mut form, Field::Disks, "data:2");
        assert_eq!(
            refused(&h, &mut form, Field::Disks, "grow running"),
            "stop the VM before resizing or removing disks"
        );
        set(&mut form, Field::Disks, "");
        assert_eq!(
            refused(&h, &mut form, Field::Disks, "remove running"),
            "stop the VM before resizing or removing disks"
        );
        form.running = false;
        set(&mut form, Field::Disks, "data:1");

        form.iso = "/nope.iso".into();
        assert_eq!(
            refused(&h, &mut form, Field::Iso, "missing iso"),
            "ISO file not found: /nope.iso"
        );
        form.iso = h.dir.path().to_string_lossy().into_owned();
        assert!(
            refused(&h, &mut form, Field::Iso, "iso is a dir").starts_with("ISO file not found: ")
        );
        form.iso.clear();

        set(&mut form, Field::Mac, "nope");
        assert_eq!(
            refused(&h, &mut form, Field::Mac, "mac"),
            "invalid MAC address \"nope\" — expected e.g. 52:54:00:12:34:56"
        );
        set(&mut form, Field::Mac, "");
        set(&mut form, Field::Forwards, "2222");
        assert_eq!(
            refused(&h, &mut form, Field::Forwards, "forwards"),
            "invalid port forward \"2222\" — expected [tcp|udp:]host:guest"
        );
        set(&mut form, Field::Forwards, "2222:22, udp:5353:53");
        for bad in ["100", "-1", "x"] {
            set(&mut form, Field::Vnc, bad);
            assert_eq!(
                refused(&h, &mut form, Field::Vnc, "vnc"),
                "VNC display must be 0 (disabled) or 1–99",
                "vnc {bad:?}"
            );
        }
        set(&mut form, Field::Vnc, "99");

        let cfg = form.build_config(&h.mgr).unwrap();
        assert_eq!(
            (cfg.cpu, cfg.ram, cfg.disk_size, cfg.vnc_port),
            (2, 64, 2, 99)
        );
        assert_eq!(cfg.disks, vec![disk("data", 1)]);
        assert_eq!(cfg.network.kind, NetworkType::User);
        assert!(
            cfg.network.mac.is_empty(),
            "a blank MAC is left for update to fill"
        );
        assert_eq!(
            format_port_forwards(&cfg.network.port_forwards),
            "tcp:2222:22, udp:5353:53"
        );
        assert_eq!(cfg.created_at, orig.created_at, "the rest is carried over");
        // Forwards with non-user networking are accepted by the edit form.
        form.network.index = 1;
        assert!(form.build_config(&h.mgr).is_ok());
    }

    #[test]
    fn renders_the_form() {
        let h = Harness::new();
        let cfg = VmConfig {
            cdrom_path: "/isos/debian-12.iso".into(),
            network: NetworkConfig {
                kind: NetworkType::User,
                mac: "52:54:00:00:00:01".into(),
                port_forwards: crate::vm::parse_port_forwards("2222:22").unwrap(),
            },
            ..bios_vm("deb", 20, vec![disk("data", 50)])
        };
        let mut form = EditForm::new(cfg.clone(), false);
        let lines = h.render(&mut form, 100, 40);
        let text = lines.join("\n");
        for want in [
            " Edit VM: deb ",
            "▸ Name             deb",
            "  CPU Cores        1",
            "  RAM (MiB)        512",
            "  Disk Size (GiB)  20",
            "  Extra Disks      data:50",
            "  Boot ISO         /isos/debian-12.iso",
            "  Firmware         BIOS   UEFI   UEFI + Secure Boot",
            "  TPM 2.0          disabled   enabled",
            "  Network          user (NAT)   tap (bridge)   none",
            "  MAC Address      52:54:00:00:00:01",
            "  Port Forwards    tcp:2222:22",
            "  VNC Display      0",
            "   Save",
            "  Letters, digits, hyphens and underscores. Renaming requires the VM to be stopped",
        ] {
            assert!(
                screen_contains(&lines, want),
                "view lacks {want:?}:\n{text}"
            );
        }
        assert!(!text.contains("● running") && !text.contains('✗') && !text.contains('⚠'));

        // The focused field's help, the running banner, an error and the
        // confirmation warning.
        let mut form = EditForm::new(
            VmConfig {
                cdrom_path: String::new(),
                ..cfg
            },
            true,
        );
        tab_to(&h, &mut form, Field::Disks);
        set(&mut form, Field::Cpu, "x");
        h.press(&mut form, ctrl('s'));
        form.warn = "Removing data (50 GiB) deletes the image files and everything on them — press Ctrl-s again to confirm".into();
        let lines = h.render(&mut form, 100, 40);
        let text = lines.join("\n");
        for want in [
            "● running — changes take effect on next start; name, disk sizes, disk removal and firmware are",
            "  locked; new disks are hot-plugged",
            "▸ CPU Cores        x",
            "  Boot ISO         (none)",
            "  Number of virtual CPU cores, e.g. 2",
            "⚠ Removing data (50 GiB) deletes the image files and everything on them — press Ctrl-s again to",
            "✗ CPU must be a positive integer",
        ] {
            assert!(screen_contains(&lines, want), "view lacks {want:?}:\n{text}");
        }
        assert!(
            !text.contains("Letters, digits"),
            "only the focused field's help is shown"
        );
        tab_to(&h, &mut form, Field::Save);
        let lines = h.render(&mut form, 100, 40);
        assert!(
            !screen_contains(&lines, "▸ "),
            "the button has no marker:\n{}",
            lines.join("\n")
        );
        assert!(screen_contains(&lines, "  Press Enter to save changes"));

        // A multi-line error is shown whole; a spinner while saving.
        form.err = "saved, but this could not be applied to the running VM (takes effect on next start):\ndisk \"tmp\" could not be hot-plugged:\nconnect to QEMU monitor: No such file or directory".into();
        form.saving = true;
        let lines = h.render(&mut form, 100, 40);
        assert!(
            screen_contains(&lines, "✗ saved, but this could not be applied"),
            "{}",
            lines.join("\n")
        );
        assert!(screen_contains(
            &lines,
            "  disk \"tmp\" could not be hot-plugged:"
        ));
        assert!(screen_contains(
            &lines,
            "  connect to QEMU monitor: No such file or directory"
        ));
        assert!(screen_contains(&lines, "⠋ Saving…"));

        // Small and tiny areas: the focused row stays visible, nothing panics.
        form.saving = false;
        tab_to(&h, &mut form, Field::Vnc);
        let lines = h.render(&mut form, 60, 8);
        assert!(
            screen_contains(&lines, "▸ VNC Display"),
            "{}",
            lines.join("\n")
        );
        h.render(&mut form, 20, 3);
        h.render(&mut form, 3, 2);
        h.render(&mut form, 1, 1);
    }

    /// The screen's text with the borders and runs of spaces squeezed out,
    /// so a message can be found whole however it wrapped.
    fn shown(lines: &[String]) -> String {
        lines
            .iter()
            .map(|l| l.trim_matches(|c: char| c == '│' || c == ' '))
            .collect::<Vec<_>>()
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// What Ctrl-s says stays on screen together with the field it is
    /// about, however short the pane. 50x22 is the right-hand pane of an
    /// 80x24 terminal, where the form is taller than the pane.
    #[test]
    fn outcome_stays_in_view_when_the_form_is_taller_than_the_pane() {
        let h = Harness::new();
        let cfg = VmConfig {
            network: NetworkConfig {
                kind: NetworkType::User,
                mac: "52:54:00:12:34:56".into(),
                port_forwards: crate::vm::parse_port_forwards("2222:22, 8080:80").unwrap(),
            },
            vnc_port: 1,
            ..bios_vm("deb", 20, vec![disk("data", 50), disk("scratch", 10)])
        };
        let mut form = EditForm::new(cfg.clone(), false);
        let check = |form: &mut EditForm, focus: &str, msgs: &[&str]| {
            for (w, hgt) in [(50, 22), (80, 24), (120, 40)] {
                let lines = h.render(form, w, hgt);
                let text = lines.join("\n");
                assert!(
                    screen_contains(&lines, focus),
                    "{w}x{hgt}: {focus:?} not on screen:\n{text}"
                );
                for msg in msgs {
                    assert!(
                        shown(&lines).contains(msg),
                        "{w}x{hgt}: {msg:?} not on screen whole:\n{text}"
                    );
                }
                // The field help is shown whole or not at all.
                let help = form.field.help();
                let head: String = help.chars().take(20).collect();
                assert_eq!(
                    shown(&lines).contains(&head),
                    shown(&lines).contains(help),
                    "{w}x{hgt}: help cut:\n{text}"
                );
            }
        };

        // The removal warning: the first Ctrl-s only arms it, and the
        // second deletes the image, so it must be seen.
        tab_to(&h, &mut form, Field::Disks);
        set(&mut form, Field::Disks, "data:50");
        not_saved(&h, &mut form, "first save with a removal");
        assert!(form.armed);
        let warn = format!("⚠ {}", form.warn);
        check(&mut form, "▸ Extra Disks", &[&warn]);
        // A refusal moves the cursor up to its field; the error shows with it.
        set(&mut form, Field::Cpu, "x");
        let err = refused(&h, &mut form, Field::Cpu, "bad cpu");
        check(&mut form, "▸ CPU Cores", &[&warn, &format!("✗ {err}")]);
        // The focused row may be the last one.
        set(&mut form, Field::Cpu, "2");
        tab_to(&h, &mut form, Field::Save);
        form.saving = true;
        check(
            &mut form,
            "Save",
            &[&warn, "✗ CPU must be a positive integer", "Saving…"],
        );

        // A long error on a running VM (the banner takes rows too), the
        // cursor on the first field.
        let mut form = EditForm::new(cfg, true);
        form.err = "saved, but this could not be applied to the running VM (takes effect on next start):\ndisk \"tmp\" could not be hot-plugged:\nconnect to QEMU monitor: No such file or directory".into();
        check(
            &mut form,
            "▸ Name",
            &[
                "✗ saved, but this could not be applied to the running VM (takes effect on next start):",
                "disk \"tmp\" could not be hot-plugged:",
                "connect to QEMU monitor: No such file or directory",
            ],
        );

        // A pane too short for the outcome keeps the focused row and as much
        // of the outcome as fits; nothing panics.
        let lines = h.render(&mut form, 60, 6);
        assert!(screen_contains(&lines, "▸ Name"), "{}", lines.join("\n"));
        assert!(
            screen_contains(&lines, "✗ saved, but"),
            "{}",
            lines.join("\n")
        );
        for (w, hgt) in [(20, 3), (20, 4), (3, 2), (1, 1)] {
            h.render(&mut form, w, hgt);
        }
        // Everything fitting, the outcome follows the form as before.
        let lines = h.render(&mut form, 120, 40);
        let help = lines
            .iter()
            .position(|l| l.contains("Letters, digits, hyphens and underscores"))
            .expect("help shown");
        let err_row = lines.iter().position(|l| l.contains("✗ saved")).unwrap();
        assert_eq!(err_row, help + 2, "{}", lines.join("\n"));
    }

    /// The ISO picker popup's footer is padded clear of the border corner,
    /// like the other popups' footers.
    #[test]
    fn picker_footer_is_padded() {
        let h = Harness::new();
        let mut form = EditForm::new(bios_vm("deb", 1, vec![]), false);
        tab_to(&h, &mut form, Field::Iso);
        form.picker = Some(IsoPicker::new("(none) — boot from disk", "", Vec::new()));
        for (w, hgt) in [(80, 24), (120, 40)] {
            let lines = h.render(&mut form, w, hgt);
            let footer = lines
                .iter()
                .find(|l| l.contains("Esc back to the form"))
                .unwrap_or_else(|| panic!("{w}x{hgt}: no footer:\n{}", lines.join("\n")));
            assert!(
                footer.contains("─ Enter pick  ↑/↓ move  d forget  Esc back to the form ╯"),
                "{w}x{hgt}: {footer:?}"
            );
        }
    }

    /// A long picker error wraps, and the Boot ISO popup is tall enough for
    /// all of it from the first frame on.
    #[test]
    fn picker_popup_fits_a_wrapped_error_on_the_first_frame() {
        let h = Harness::new();
        let mut form = EditForm::new(bios_vm("deb", 1, vec![]), false);
        tab_to(&h, &mut form, Field::Iso);
        let mut picker = IsoPicker::new("(none) — boot from disk", "", Vec::new());
        picker.set_error(format!(
            "image not found: /tmp/{}/debian-12.3.0-amd64-netinst.iso",
            ["no-such-directory"; 4].join("/")
        ));
        form.picker = Some(picker);
        let lines = h.render(&mut form, 80, 24);
        assert!(
            screen_contains(&lines, "✗ image not found:"),
            "{}",
            lines.join("\n")
        );
        assert!(
            screen_contains(&lines, "netinst.iso"),
            "{}",
            lines.join("\n")
        );
    }

    /// The column of the first non-blank cell after `label` on its row.
    fn value_col(lines: &[String], label: &str) -> usize {
        let line = lines
            .iter()
            .find(|l| l.contains(label))
            .unwrap_or_else(|| panic!("no {label:?} row:\n{}", lines.join("\n")));
        let start = line[..line.find(label).unwrap()].chars().count() + label.chars().count();
        start + line.chars().skip(start).take_while(|c| *c == ' ').count()
    }

    /// Text values and the selectors' first choice start in one column.
    #[test]
    fn controls_start_in_one_column() {
        let h = Harness::new();
        let cfg = VmConfig {
            cdrom_path: "/isos/debian-12.iso".into(),
            ..bios_vm("deb", 20, vec![disk("data", 50)])
        };
        let mut form = EditForm::new(cfg, false);
        for focus in [Field::Name, Field::Firmware, Field::Network] {
            tab_to(&h, &mut form, focus);
            for (w, hgt) in [(80, 24), (120, 40)] {
                let lines = h.render(&mut form, w, hgt);
                let col = value_col(&lines, "CPU Cores");
                assert_eq!(col, usize::from(CONTROL_COL) + 2, "border and margin first");
                for label in [
                    "RAM (MiB)",
                    "Extra Disks",
                    "Boot ISO",
                    "Firmware",
                    "TPM 2.0",
                    "Network",
                    "VNC Display",
                ] {
                    assert_eq!(
                        value_col(&lines, label),
                        col,
                        "{w}x{hgt}, focus {focus:?}: {label} off the column:\n{}",
                        lines.join("\n")
                    );
                }
            }
        }
    }

    /// In a narrow pane (the right column at 80x24) a selector wraps under
    /// its first choice instead of losing its last ones, `none` and
    /// `UEFI + Secure Boot`, at the border.
    #[test]
    fn selectors_wrap_in_a_narrow_pane() {
        let h = Harness::new();
        let mut form = EditForm::new(bios_vm("deb", 20, vec![]), false);
        let lines = h.render(&mut form, 50, 40);
        let row = |needle: &str| {
            lines
                .iter()
                .position(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("no {needle:?} in:\n{}", lines.join("\n")))
        };
        let net = row("Network");
        assert!(lines[net].contains("user (NAT)"), "{}", lines[net]);
        assert_eq!(
            lines[net + 1].trim_matches(|c| c == '│' || c == ' '),
            "none",
            "the chosen network wraps to the next row:\n{}",
            lines.join("\n")
        );
        let col = value_col(&lines, "Network");
        assert_eq!(
            lines[net + 1]
                .find("none")
                .map(|b| lines[net + 1][..b].chars().count()),
            Some(col)
        );
        let fw = row("Firmware");
        assert!(
            lines[fw + 1].contains("UEFI + Secure Boot"),
            "{}",
            lines.join("\n")
        );
    }

    #[test]
    fn task_results_close_or_keep_the_form() {
        let h = Harness::new();
        let mut form = EditForm::new(bios_vm("deb", 1, vec![]), false);
        form.saving = true;
        let mut ctx = h.ctx();
        form.on_task(
            TaskResult::VmUpdateFailed {
                err: "load VM config: gone".into(),
            },
            &mut ctx,
        );
        assert!(ctx.actions.is_empty() && !form.saving);
        assert_eq!(form.err, "load VM config: gone");

        form.saving = true;
        let mut ctx = h.ctx();
        form.on_task(
            TaskResult::VmUpdated {
                name: "deb2".into(),
            },
            &mut ctx,
        );
        assert!(!form.saving);
        match ctx.actions.as_slice() {
            [Action::CloseSelectVm(name), Action::Notice(n)] => {
                assert_eq!(name, "deb2");
                assert_eq!(n.text(), "saved deb2");
            }
            other => panic!("{other:?}"),
        }
        // A result for somebody else is ignored.
        let mut ctx = h.ctx();
        form.on_task(TaskResult::Done { what: "x".into() }, &mut ctx);
        assert!(ctx.actions.is_empty());
    }

    #[test]
    fn paste_goes_into_the_focused_input_and_disarms() {
        let h = Harness::new();
        let mut form = EditForm::new(bios_vm("deb", 1, vec![disk("data", 1)]), false);
        tab_to(&h, &mut form, Field::Disks);
        form.armed = true;
        form.armed_value = "data:1".into();
        form.warn = "w".into();
        let mut ctx = h.ctx();
        form.handle_paste(", logs:2", &mut ctx);
        assert_eq!(form.value(Field::Disks), "data:1, logs:2");
        assert!(
            !form.armed && form.warn.is_empty(),
            "still armed after a paste changed the field"
        );
        tab_to(&h, &mut form, Field::Iso);
        form.handle_paste("x", &mut ctx);
        assert_eq!(
            form.value(Field::Disks),
            "data:1, logs:2",
            "a paste off a text field goes nowhere"
        );
    }

    /// Port of TestEditFormExtraDisks: a real update of a BIOS VM.
    #[test]
    fn extra_disks() {
        let h = Harness::new();
        let storage = h.mgr.storage().to_path_buf();
        let mut cfg = bios_vm("deb", 1, vec![disk("data", 1), disk("scratch", 1)]);
        h.mgr.create(&mut cfg).expect("create VM");
        let scratch = extra_disk_path(&storage, "deb", "scratch");
        assert!(scratch.exists());

        let mut form = EditForm::new(cfg, false);
        h.init(&mut form);
        assert_eq!(form.value(Field::Disks), "data:1, scratch:1", "pre-filled");
        assert!(screen_contains(
            &h.render(&mut form, 110, 40),
            "Extra Disks"
        ));

        // A bad entry lands on the field.
        set(&mut form, Field::Disks, "data:1, nope");
        let err = refused(&h, &mut form, Field::Disks, "bad entry");
        assert!(err.contains("expected [name:]size"), "{err}");
        // So does a size below 1 GiB.
        set(&mut form, Field::Disks, "data:1, scratch:0");
        let err = refused(&h, &mut form, Field::Disks, "zero size");
        assert!(err.contains("at least 1 GiB"), "{err}");

        // Removing a disk takes two saves: the first only warns, deleting nothing.
        set(&mut form, Field::Disks, "data:1");
        not_saved(&h, &mut form, "first save with a removal");
        assert!(
            form.armed && form.warn.contains("scratch (1 GiB)"),
            "armed={} warn={:?}",
            form.armed,
            form.warn
        );
        assert!(form.err.is_empty());
        assert!(screen_contains(
            &h.render(&mut form, 110, 40),
            "press Ctrl-s again"
        ));
        assert!(scratch.exists(), "scratch deleted before confirmation");
        // Editing the field disarms it; a save then warns afresh.
        h.press(&mut form, ch('x'));
        assert!(
            !form.armed && form.warn.is_empty(),
            "still armed after editing the field: {:?}",
            form.warn
        );
        set(&mut form, Field::Disks, "data:1");
        not_saved(&h, &mut form, "re-arm");
        assert!(form.armed, "not re-armed");
        // Moving between fields does not disarm.
        h.press(&mut form, key(KeyCode::Tab));
        h.press(&mut form, backtab());
        assert!(form.armed);
        // The second save goes through and the image is gone.
        let acts = h.press(&mut form, ctrl('s'));
        assert!(
            acts.is_empty() && form.saving && !form.armed && form.warn.is_empty(),
            "confirmed save: {acts:?}"
        );
        let acts = h.deliver_next(&mut form);
        match acts.as_slice() {
            [Action::CloseSelectVm(name), Action::Notice(n)] => {
                assert_eq!(name, "deb");
                assert_eq!(n.text(), "saved deb");
            }
            other => panic!("save result: {other:?} (err {:?})", form.err),
        }
        assert!(!form.saving);
        assert!(
            !scratch.exists(),
            "scratch still there after the confirmed removal"
        );
        assert!(
            extra_disk_path(&storage, "deb", "data").exists(),
            "data gone too"
        );
        let saved = load_config(&storage, "deb").unwrap();
        assert_eq!(saved.disks, vec![disk("data", 1)]);

        // Growing and adding on a stopped VM.
        let mut form = EditForm::new(saved, false);
        set(&mut form, Field::Disks, "data:2, logs:1");
        h.press(&mut form, ctrl('s'));
        assert!(form.saving && !form.armed, "grow+add: err={:?}", form.err);
        let acts = h.deliver_next(&mut form);
        assert!(
            matches!(acts.as_slice(), [Action::CloseSelectVm(n), Action::Notice(_)] if n == "deb"),
            "{acts:?} {:?}",
            form.err
        );
        assert!(
            extra_disk_path(&storage, "deb", "logs").exists(),
            "logs not created"
        );
        let saved = load_config(&storage, "deb").unwrap();
        assert_eq!(saved.disks, vec![disk("data", 2), disk("logs", 1)]);

        // On a running VM growing and removing are refused up front; adding
        // goes ahead and is hot-plugged, which fails here for want of a
        // monitor, but the disk is saved and reported as applying at the
        // next start.
        let mut form = EditForm::new(saved, true);
        assert!(
            screen_contains(&h.render(&mut form, 110, 40), "disk removal"),
            "running banner lacks the disk lock"
        );
        set(&mut form, Field::Disks, "data:2");
        let err = refused(&h, &mut form, Field::Disks, "remove while running");
        assert!(err.contains("stop the VM"), "{err}");
        set(&mut form, Field::Disks, "data:3, logs:1");
        let err = refused(&h, &mut form, Field::Disks, "grow while running");
        assert!(err.contains("stop the VM"), "{err}");
        set(&mut form, Field::Disks, "data:2, logs:1, tmp:1");
        h.press(&mut form, ctrl('s'));
        assert!(form.saving, "add while running refused: {:?}", form.err);
        let acts = h.deliver_next(&mut form);
        assert!(acts.is_empty(), "the form stays open: {acts:?}");
        assert!(!form.saving);
        assert!(
            form.err.contains("saved, but") && form.err.contains("disk \"tmp\""),
            "{:?}",
            form.err
        );
        assert!(
            extra_disk_path(&storage, "deb", "tmp").exists(),
            "tmp not created"
        );
        let saved = load_config(&storage, "deb").unwrap();
        assert_eq!(saved.disks.len(), 3);
        assert_eq!(saved.disks[2].name, "tmp");
        let lines = h.render(&mut form, 110, 40);
        assert!(
            screen_contains(
                &lines,
                "✗ saved, but this could not be applied to the running VM"
            ),
            "{}",
            lines.join("\n")
        );
    }

    /// A 1 MiB zero file standing in for an image.
    fn write_image(path: &Path) -> String {
        fs::write(path, vec![0u8; 1 << 20]).unwrap();
        path.to_string_lossy().into_owned()
    }

    /// Port of TestEditFormISOField: the Boot ISO field opens the picker.
    #[test]
    fn iso_field_opens_the_picker() {
        let h = Harness::new();
        let storage = h.mgr.storage().to_path_buf();
        // The picker reads and the save writes the app config under $HOME.
        std::env::set_var("HOME", &h.home);
        config::save(&config::AppConfig {
            vm_storage_path: storage.to_string_lossy().into_owned(),
            recent_isos: vec![],
        })
        .unwrap();
        let isos = h.dir.path().join("isos");
        fs::create_dir_all(&isos).unwrap();
        let disc = write_image(&isos.join("debian.iso"));
        let other = write_image(&isos.join("other.iso"));
        let cfg = VmConfig {
            name: "t".into(),
            cpu: 1,
            ram: 128,
            disk_size: 1,
            cdrom_path: disc.clone(),
            network: NetworkConfig {
                kind: NetworkType::User,
                mac: "52:54:00:00:00:01".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        write_vm(&storage, &cfg);
        let mut form = EditForm::new(cfg, false);
        assert!(
            screen_contains(&h.render(&mut form, 160, 40), &disc),
            "form lacks the boot ISO"
        );
        tab_to(&h, &mut form, Field::Iso);

        // Enter opens the picker on the current disc; Esc comes back unchanged.
        h.press(&mut form, key(KeyCode::Enter));
        assert!(form.picker.is_some(), "enter should open the picker");
        let lines = h.render(&mut form, 160, 40);
        for want in [
            " Boot ISO ",
            "New path",
            "in use by t",
            "(none) — boot from disk",
            "Esc back to the form",
        ] {
            assert!(
                screen_contains(&lines, want),
                "picker view lacks {want:?}:\n{}",
                lines.join("\n")
            );
        }
        assert_eq!(
            form.key_hints().last().map(|(k, _)| k.as_str()),
            Some("Esc")
        );
        h.press(&mut form, key(KeyCode::Esc));
        assert!(form.picker.is_none() && form.iso == disc && form.field == Field::Iso);

        // l opens it too; (none) clears the field.
        h.press(&mut form, ch('l'));
        assert!(form.picker.is_some());
        h.press(&mut form, ch('g'));
        h.press(&mut form, key(KeyCode::Enter));
        assert!(
            form.picker.is_none() && form.iso.is_empty(),
            "after none: iso={:?}",
            form.iso
        );
        assert!(screen_contains(&h.render(&mut form, 160, 40), "(none)"));
        assert_eq!(form.build_config(&h.mgr).unwrap().cdrom_path, "");

        // A new path typed in the dialog is saved and remembered.
        h.press(&mut form, key(KeyCode::Enter));
        h.press(&mut form, ch('G'));
        type_str(&mut form, &h, &other);
        h.press(&mut form, key(KeyCode::Enter));
        assert!(
            form.picker.is_none() && form.iso == other,
            "after typing: iso={:?}",
            form.iso
        );
        h.press(&mut form, ctrl('s'));
        assert!(form.saving, "save: err={:?}", form.err);
        let acts = h.deliver_next(&mut form);
        assert!(
            matches!(acts.as_slice(), [Action::CloseSelectVm(n), Action::Notice(_)] if n == "t"),
            "{acts:?} {:?}",
            form.err
        );
        assert_eq!(load_config(&storage, "t").unwrap().cdrom_path, other);
        assert_eq!(config::recent_isos(), vec![other.clone()]);

        // The field takes no typing, so j and k move between fields there.
        h.press(&mut form, ch('j'));
        assert_eq!(form.field, Field::Firmware);
    }
}
