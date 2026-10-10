//! The form that freezes a stopped VM as a template.

use std::fs;

use anyhow::anyhow;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Paragraph};

use super::super::events::TaskResult;
use super::super::panel::{Ctx, Panel};
use super::super::theme::Theme;
use super::super::widgets::{human_size, plural, spinner_frame, wrap_text, TextInput};
use super::common::{form_block, validate_vm_name};
use crate::vm::{self, Manager, VmConfig};

/// The fields of the form, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Name,
    Description,
    /// No text input.
    Button,
}

const FIELDS: [Field; 3] = [Field::Name, Field::Description, Field::Button];

impl Field {
    fn index(self) -> usize {
        FIELDS.iter().position(|f| *f == self).unwrap_or(0)
    }

    /// The field after this one, wrapping from the button to the name.
    fn next(self) -> Field {
        FIELDS[(self.index() + 1) % FIELDS.len()]
    }

    /// The field before this one, wrapping from the name to the button.
    fn prev(self) -> Field {
        FIELDS[(self.index() + FIELDS.len() - 1) % FIELDS.len()]
    }

    /// Whether the field is backed by a text input.
    fn is_text(self) -> bool {
        self != Field::Button
    }

    fn label(self) -> &'static str {
        match self {
            Field::Name => "Template name",
            Field::Description => "Description",
            Field::Button => "",
        }
    }

    fn help(self) -> &'static str {
        match self {
            Field::Name => "Letters, digits, hyphens and underscores, e.g. debian-12-base",
            Field::Description => {
                "Optional — a line about what is installed, shown in the templates list"
            }
            Field::Button => "Press Enter to save the template",
        }
    }
}

const RUNNING_TEXT: &str =
    "— stop the VM first: shut it down from inside the guest, so the disk is in a consistent state";
const STOPPED_TEXT: &str = "— the disk is in a consistent state and can be copied";
const LEFT_OUT: &str = "Left out, as they belong to one VM: MAC address, port forwards, VNC display, boot ISO, USB devices and images, additional disks.";
/// `▸ ` + a 16-cell label + a space: where the inputs start.
const CONTROL_COL: u16 = 19;
/// The width of `Disk:     ` and friends in the info box.
const INFO_KEY_W: usize = 10;

/// The save-as-template form. See the spec (06-tui-forms.md) and
/// internal/tui/templatesave.go.
pub struct SaveTemplateForm {
    cfg: VmConfig,
    field: Field,
    /// The name and the description; the button has none.
    inputs: [TextInput; 2],
    running: bool,
    /// The source disk image's size on the host, 0 when unknown.
    disk_usage: u64,
    /// The copy is in flight.
    busy: bool,
    err: String,
}

impl SaveTemplateForm {
    /// A form for `cfg` with the VM's name as the suggested template name;
    /// `running` is the VM's state when it opened. The source disk's size on
    /// the host is read in `init`.
    pub fn new(cfg: VmConfig, running: bool) -> Self {
        let inputs = [
            TextInput::new()
                .with_char_limit(256)
                .with_value(&cfg.name)
                .focused(),
            TextInput::new()
                .with_char_limit(256)
                .with_placeholder("e.g. Debian 12 with docker and my dotfiles"),
        ];
        SaveTemplateForm {
            cfg,
            field: Field::Name,
            inputs,
            running,
            disk_usage: 0,
            busy: false,
            err: String::new(),
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

    /// Checks the form and the VM's state, returning the field to put the
    /// cursor on when something is wrong.
    fn validate(&self, mgr: &Manager) -> Result<(), (Field, anyhow::Error)> {
        let name = self.value(Field::Name);
        validate_vm_name(&name).map_err(|e| (Field::Name, e))?;
        if mgr.template_exists(&name) {
            return Err((
                Field::Name,
                anyhow!("a template named {name:?} already exists"),
            ));
        }
        if self.running {
            return Err((
                self.field,
                anyhow!("the VM is running — shut it down from inside the guest first, so the disk is in a consistent state"),
            ));
        }
        Ok(())
    }

    fn save(&mut self, ctx: &mut Ctx) {
        if let Err((field, err)) = self.validate(ctx.mgr) {
            self.move_to(field);
            self.err = format!("{err:#}");
            return;
        }
        self.err.clear();
        self.busy = true;
        let mgr = ctx.mgr.clone();
        let vm_name = self.cfg.name.clone();
        let name = self.value(Field::Name);
        let desc = self.value(Field::Description);
        ctx.spawn(move || match mgr.create_template(&vm_name, &name, &desc) {
            Ok(()) => TaskResult::TemplateSaved { name },
            Err(err) => TaskResult::TemplateSaveFailed {
                err: format!("{err:#}"),
            },
        });
    }

    /// The `Disk` / `Firmware` / `Defaults` rows of the info box, each as a
    /// key and its text.
    fn info_rows(&self) -> [(&'static str, String); 3] {
        let mut disk = format!("{} GiB virtual", self.cfg.disk_size);
        if self.disk_usage > 0 {
            disk.push_str(&format!(
                ", {} on the host — copied in full",
                human_size(self.disk_usage)
            ));
        }
        let mut firmware = self.cfg.firmware_label();
        firmware.push_str(match (self.cfg.uefi(), self.cfg.tpm) {
            (true, true) => " — with the UEFI NVRAM (boot entries) and the TPM state",
            (true, false) => " — with the UEFI NVRAM (boot entries)",
            (false, true) => " — with the TPM state",
            (false, false) => "",
        });
        let defaults = format!(
            "{}, {} MiB RAM, {} network — chosen anew for each VM made from it",
            plural(self.cfg.cpu, "core", "cores"),
            self.cfg.ram,
            self.cfg.network.kind
        );
        [
            ("Disk:", disk),
            ("Firmware:", firmware),
            ("Defaults:", defaults),
        ]
    }

    /// What the busy line says: `Copying the disk image (196 KiB)`.
    fn copying_text(&self) -> String {
        let mut what = "Copying the disk image".to_string();
        if self.disk_usage > 0 {
            what.push_str(&format!(" ({})", human_size(self.disk_usage)));
        }
        what
    }
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
    /// The info box: a bordered block around these lines.
    Box(Vec<Line<'static>>),
}

impl Row {
    fn height(&self) -> usize {
        match self {
            Row::Line(_) | Row::Input(..) => 1,
            Row::Box(lines) => lines.len() + 2,
        }
    }
}

impl Panel for SaveTemplateForm {
    fn title(&self) -> String {
        format!("Save as Template: {}", self.cfg.name)
    }

    fn init(&mut self, ctx: &mut Ctx) {
        self.disk_usage =
            fs::metadata(vm::disk_path(ctx.storage(), &self.cfg.name)).map_or(0, |st| st.len());
    }

    fn handle_key(&mut self, key: KeyEvent, ctx: &mut Ctx) {
        if self.busy {
            // The copy cannot be interrupted from here.
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
                if self.field == Field::Button {
                    self.save(ctx);
                } else {
                    self.move_to(self.field.next());
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
            KeyCode::Char('j') if plain && !self.field.is_text() => {
                self.move_to(self.field.next());
                return;
            }
            KeyCode::Char('k') if plain && !self.field.is_text() => {
                self.move_to(self.field.prev());
                return;
            }
            _ => {}
        }
        if self.field.is_text() {
            self.inputs[self.field.index()].handle_key(key);
        }
    }

    fn handle_paste(&mut self, text: &str, _ctx: &mut Ctx) {
        if !self.busy && self.field.is_text() {
            self.inputs[self.field.index()].insert_str(text);
        }
    }

    fn on_task(&mut self, result: TaskResult, ctx: &mut Ctx) {
        match result {
            TaskResult::TemplateSaved { name } => {
                self.busy = false;
                ctx.close_select_template(name.clone());
                ctx.notice_ok(format!("saved template {name}"));
            }
            TaskResult::TemplateSaveFailed { err } | TaskResult::Failed { err, .. } => {
                self.busy = false;
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
        let width = usize::from(inner.width);
        let blank = || Row::Line(Line::raw(""));

        let mut rows: Vec<Row> = Vec::new();
        let status = if self.running {
            flow(
                vec![Span::styled("● running ", theme.running)],
                RUNNING_TEXT,
                theme.error,
                width,
            )
        } else {
            flow(
                vec![Span::styled("● stopped ", theme.stopped)],
                STOPPED_TEXT,
                theme.help,
                width,
            )
        };
        rows.extend(status.into_iter().map(Row::Line));
        rows.push(blank());

        // The section starts flush left, like the dialogs' section labels;
        // only the field help is indented, under the field labels.
        rows.push(Row::Line(Line::styled(
            "What goes into the template",
            theme.label,
        )));
        let box_w = width.saturating_sub(4).max(8); // borders and a cell of margin on each side
        let mut info = Vec::new();
        for (key, text) in self.info_rows() {
            for (i, piece) in wrap_text(&text, box_w.saturating_sub(INFO_KEY_W).max(8))
                .into_iter()
                .enumerate()
            {
                let lead = if i == 0 {
                    format!("{key:<INFO_KEY_W$}")
                } else {
                    " ".repeat(INFO_KEY_W)
                };
                info.push(Line::from(vec![
                    Span::styled(lead, theme.help),
                    Span::styled(piece, theme.normal),
                ]));
            }
        }
        rows.push(Row::Box(info));
        for l in wrap_text(LEFT_OUT, width.max(8)) {
            rows.push(Row::Line(Line::styled(l, theme.help)));
        }
        rows.push(blank());

        let mut focus_row = 0;
        for f in FIELDS {
            let focused = f == self.field;
            if f == Field::Button {
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
                    Span::styled(" Save template ", style),
                ])));
                continue;
            }
            let (marker, label_style) = if focused {
                ("▸ ", theme.label)
            } else {
                ("  ", theme.help)
            };
            let mut spans = vec![
                Span::styled(marker, theme.label),
                Span::styled(format!("{:<16} ", f.label()), label_style),
            ];
            if focused {
                focus_row = rows.len();
                rows.push(Row::Input(Line::from(spans), f.index()));
            } else {
                spans.push(self.inputs[f.index()].as_span(theme));
                rows.push(Row::Line(Line::from(spans)));
            }
        }

        rows.push(blank());
        for l in wrap_text(self.field.help(), width.saturating_sub(2).max(8)) {
            rows.push(Row::Line(Line::styled(format!("  {l}"), theme.help)));
        }
        rows.push(blank());
        // The outcome under the form: the spinner while copying, else the error.
        let has_outcome = self.busy || !self.err.is_empty();
        if self.busy {
            let prefix = vec![
                Span::styled(spinner_frame(tick), theme.spinner),
                Span::raw(" "),
            ];
            let text = format!(
                "{}… this takes a while for a large disk",
                self.copying_text()
            );
            rows.extend(
                flow(prefix, &text, theme.normal, width)
                    .into_iter()
                    .map(Row::Line),
            );
        } else if !self.err.is_empty() {
            let prefix = vec![Span::styled("✗ ", theme.error)];
            rows.extend(
                flow(prefix, &self.err, theme.error, width)
                    .into_iter()
                    .map(Row::Line),
            );
        }

        // Scroll so the outcome shows as well, as long as that keeps the
        // focused row on screen; the focused row wins otherwise.
        let height = usize::from(inner.height);
        let first_visible = |last: usize| {
            let mut first = 0;
            while first < last && rows[first..=last].iter().map(Row::height).sum::<usize>() > height
            {
                first += 1;
            }
            first
        };
        let mut offset = if has_outcome {
            first_visible(rows.len() - 1)
        } else {
            first_visible(focus_row)
        };
        if offset > focus_row {
            offset = first_visible(focus_row);
        }
        let bottom = inner.y + inner.height;
        let mut y = inner.y;
        for row in rows.into_iter().skip(offset) {
            if y >= bottom {
                break;
            }
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
                Row::Box(lines) => {
                    let h = (lines.len() as u16 + 2).min(bottom - y);
                    let rect = Rect::new(inner.x, y, inner.width, h);
                    let block = Block::bordered()
                        .border_type(BorderType::Rounded)
                        .border_style(theme.border);
                    let body = block.inner(rect).inner(Margin::new(1, 0));
                    frame.render_widget(block, rect);
                    frame.render_widget(Paragraph::new(lines), body);
                    y += h;
                    continue;
                }
            }
            y += 1;
        }
    }

    fn key_hints(&self) -> Vec<(String, String)> {
        if self.busy {
            // Every key but Ctrl-c is swallowed while copying; the body
            // shows the progress line.
            return vec![("Ctrl-c".to_string(), "quit".to_string())];
        }
        let pairs: &[(&str, &str)] = if self.field == Field::Button {
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
        self.busy
            .then(|| format!("saving template {}…", self.value(Field::Name)))
    }

    fn failed(&self) -> bool {
        !self.err.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;
    use std::time::Duration;

    use super::*;
    use crate::tui::panel::Action;
    use crate::tui::testutil::*;
    use crate::vm::{
        disk_path, load_template, save_config, vm_dir, FirmwareType, NetworkConfig, NetworkType,
    };

    /// Lays down a VM directory with a small qcow2 disk and `vm.yaml`, like
    /// the Go tests' newTestVM.
    fn new_test_vm(storage: &Path, cfg: &VmConfig) {
        fs::create_dir_all(vm_dir(storage, &cfg.name)).unwrap();
        let out = Command::new("qemu-img")
            .args(["create", "-f", "qcow2"])
            .arg(disk_path(storage, &cfg.name))
            .arg("64M")
            .output()
            .expect("qemu-img");
        assert!(
            out.status.success(),
            "qemu-img create: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        save_config(storage, cfg).unwrap();
    }

    fn deb() -> VmConfig {
        VmConfig {
            name: "deb".into(),
            cpu: 2,
            ram: 2048,
            disk_size: 20,
            firmware: FirmwareType::Uefi,
            tpm: true,
            network: NetworkConfig {
                kind: NetworkType::User,
                mac: "52:54:00:00:00:01".into(),
                ..Default::default()
            },
            vnc_port: 1,
            ..Default::default()
        }
    }

    fn set_name(form: &mut SaveTemplateForm, value: &str) {
        form.inputs[Field::Name.index()].set_value(value);
    }

    /// Port of TestSaveTemplateScreen: a real template round trip.
    #[test]
    fn save_template_round_trip() {
        if which::which("qemu-img").is_err() {
            eprintln!("skipping: qemu-img not installed");
            return;
        }
        let h = Harness::new();
        let storage = h.mgr.storage().to_path_buf();
        let cfg = deb();
        new_test_vm(&storage, &cfg);

        let mut form = SaveTemplateForm::new(cfg.clone(), false);
        assert_eq!(form.disk_usage, 0, "the size is read in init");
        h.init(&mut form);
        assert!(form.disk_usage > 0, "disk usage not read");
        let lines = h.render(&mut form, 110, 40);
        let text = lines.join("\n");
        for want in [
            "Save as Template: deb",
            "● stopped",
            "UEFI, TPM 2.0",
            "UEFI NVRAM",
            "TPM state",
            "20 GiB virtual",
            "on the host — copied in full",
            "Template name",
            "Description",
            "Save template",
        ] {
            assert!(
                screen_contains(&lines, want),
                "view lacks {want:?}:\n{text}"
            );
        }
        assert_eq!(form.value(Field::Name), "deb", "suggested name");

        // A bad name is refused on the spot.
        set_name(&mut form, "bad name!");
        let acts = h.press(&mut form, ctrl('s'));
        assert!(acts.is_empty() && !form.busy);
        assert!(
            form.err.contains("letters, digits") && form.field == Field::Name,
            "err={:?}",
            form.err
        );

        // Name, then description, then the button: Enter walks down and saves.
        set_name(&mut form, "deb-base");
        h.press(&mut form, key(KeyCode::Enter));
        assert_eq!(form.field, Field::Description);
        type_str(&mut form, &h, "golden image");
        h.press(&mut form, key(KeyCode::Enter));
        assert_eq!(form.field, Field::Button);
        let acts = h.press(&mut form, key(KeyCode::Enter));
        assert!(form.busy && acts.is_empty(), "save should start the copy");
        assert!(form.err.is_empty());
        let lines = h.render(&mut form, 110, 40);
        assert!(
            screen_contains(&lines, "Copying the disk image ("),
            "busy view:\n{}",
            lines.join("\n")
        );
        assert!(screen_contains(
            &lines,
            "… this takes a while for a large disk"
        ));
        assert_eq!(
            form.key_hints(),
            vec![("Ctrl-c".to_string(), "quit".to_string())],
            "busy: only the key that still works"
        );
        assert_eq!(form.busy().as_deref(), Some("saving template deb-base…"));
        // Keys are ignored while the copy runs.
        let acts = h.press(&mut form, key(KeyCode::Esc));
        assert!(
            acts.is_empty() && form.busy,
            "esc should do nothing while busy"
        );
        let acts = h.deliver_next(&mut form);
        match acts.as_slice() {
            [Action::CloseSelectTemplate(name), Action::Notice(n)] => {
                assert_eq!(name, "deb-base");
                assert_eq!(n.text(), "saved template deb-base");
            }
            other => panic!("after saving: {other:?} (err {:?})", form.err),
        }
        assert!(!form.busy);
        let tpl = load_template(&storage, "deb-base").unwrap();
        assert_eq!(tpl.description, "golden image");
        assert_eq!(tpl.source_vm, "deb");
        assert!(
            tpl.vnc && tpl.tpm && tpl.firmware == FirmwareType::Uefi,
            "template = {tpl:?}"
        );

        // The same name again is refused before anything is copied.
        let mut form = SaveTemplateForm::new(cfg.clone(), false);
        set_name(&mut form, "deb-base");
        h.press(&mut form, ctrl('s'));
        assert!(
            !form.busy && form.err.contains("already exists"),
            "duplicate: err={:?}",
            form.err
        );
        assert_eq!(form.err, "a template named \"deb-base\" already exists");
        assert!(h.next_result(Duration::from_millis(50)).is_none());

        // A running VM is refused, and the screen says so up front.
        let mut form = SaveTemplateForm::new(cfg, true);
        h.init(&mut form);
        let lines = h.render(&mut form, 110, 40);
        assert!(
            screen_contains(
                &lines,
                "● running — stop the VM first: shut it down from inside the guest, so the disk is"
            ),
            "{}",
            lines.join("\n")
        );
        h.press(&mut form, key(KeyCode::Tab)); // the running error leaves the cursor where it is
        set_name(&mut form, "deb-running");
        h.press(&mut form, ctrl('s'));
        assert!(
            !form.busy && form.err.contains("running"),
            "running VM accepted: err={:?}",
            form.err
        );
        assert_eq!(form.field, Field::Description);
        assert!(
            !h.mgr.template_exists("deb-running"),
            "template made from a running VM"
        );
        let lines = h.render(&mut form, 110, 40);
        assert!(screen_contains(&lines, "✗ the VM is running — shut it down from inside the guest first, so the disk is in a"), "{}", lines.join("\n"));

        // Esc goes back to the VM.
        let acts = h.press(&mut form, key(KeyCode::Esc));
        assert!(matches!(acts.as_slice(), [Action::Close]), "{acts:?}");
    }

    #[test]
    fn renders_the_form() {
        let h = Harness::new();
        let cfg = VmConfig {
            firmware: FirmwareType::Bios,
            tpm: false,
            ..deb()
        };
        let mut form = SaveTemplateForm::new(cfg, false);
        assert_eq!(form.title(), "Save as Template: deb");
        let lines = h.render(&mut form, 100, 40);
        let text = lines.join("\n");
        for want in [
            " Save as Template: deb ",
            "● stopped — the disk is in a consistent state and can be copied",
            "│ What goes into the template",
            "╭",
            "│ Disk:     20 GiB virtual",
            "│ Firmware: BIOS",
            "│ Defaults: 2 cores, 2048 MiB RAM, user network — chosen anew for each VM made from it",
            "╰",
            "│ Left out, as they belong to one VM: MAC address, port forwards, VNC display, boot ISO, USB",
            "│ devices and images, additional disks.",
            "▸ Template name    deb",
            "  Description      e.g. Debian 12 with docker and my dotfiles",
            "   Save template",
            "  Letters, digits, hyphens and underscores, e.g. debian-12-base",
        ] {
            assert!(screen_contains(&lines, want), "view lacks {want:?}:\n{text}");
        }
        assert!(!text.contains("on the host"), "no usage known yet:\n{text}");
        assert!(!text.contains("UEFI") && !text.contains("TPM"), "{text}");

        // UEFI alone and TPM alone get their own wording; usage shows once known.
        form.cfg.firmware = FirmwareType::Uefi;
        form.disk_usage = 196 * 1024;
        let lines = h.render(&mut form, 100, 40);
        assert!(
            screen_contains(
                &lines,
                "Firmware: UEFI — with the UEFI NVRAM (boot entries)"
            ),
            "{}",
            lines.join("\n")
        );
        assert!(screen_contains(
            &lines,
            "Disk:     20 GiB virtual, 196 KiB on the host — copied in full"
        ));
        form.cfg.firmware = FirmwareType::Bios;
        form.cfg.tpm = true;
        let lines = h.render(&mut form, 100, 40);
        assert!(
            screen_contains(&lines, "Firmware: BIOS, TPM 2.0 — with the TPM state"),
            "{}",
            lines.join("\n")
        );

        // The focused field moves the marker and the help; the button lights up.
        h.press(&mut form, key(KeyCode::Tab));
        let lines = h.render(&mut form, 100, 40);
        assert!(screen_contains(&lines, "  Template name    deb"));
        assert!(screen_contains(&lines, "▸ Description"));
        assert!(screen_contains(
            &lines,
            "  Optional — a line about what is installed, shown in the templates list"
        ));
        h.press(&mut form, key(KeyCode::Tab));
        let lines = h.render(&mut form, 100, 40);
        assert!(!screen_contains(&lines, "▸ "));
        assert!(screen_contains(
            &lines,
            "  Press Enter to save the template"
        ));

        // Busy and error lines; small areas keep the focused row and do not panic.
        form.busy = true;
        let lines = h.render(&mut form, 100, 40);
        assert!(
            screen_contains(
                &lines,
                "⠋ Copying the disk image (196 KiB)… this takes a while for a large disk"
            ),
            "{}",
            lines.join("\n")
        );
        form.busy = false;
        form.err = "copy disk image: qemu-img convert: exit status 1\nno space left".into();
        let lines = h.render(&mut form, 100, 40);
        assert!(
            screen_contains(&lines, "✗ copy disk image: qemu-img convert: exit status 1"),
            "{}",
            lines.join("\n")
        );
        assert!(screen_contains(&lines, "  no space left"));
        let lines = h.render(&mut form, 60, 10);
        assert!(
            screen_contains(&lines, "✗ copy disk image"),
            "{}",
            lines.join("\n")
        );
        h.press(&mut form, key(KeyCode::Tab));
        let lines = h.render(&mut form, 60, 8);
        assert!(
            screen_contains(&lines, "▸ Template name"),
            "{}",
            lines.join("\n")
        );
        h.render(&mut form, 20, 3);
        h.render(&mut form, 3, 2);
        h.render(&mut form, 1, 1);
    }

    /// The section label and the note under the box start where the
    /// status line and the dialogs' section labels do; the field help stays
    /// indented under the field labels. One core is `1 core`.
    #[test]
    fn section_starts_flush_left() {
        let h = Harness::new();
        let mut form = SaveTemplateForm::new(VmConfig { cpu: 1, ..deb() }, false);
        for (w, hgt) in [(80, 24), (120, 40)] {
            let lines = h.render(&mut form, w, hgt);
            let text = lines.join("\n");
            for want in [
                "│ ● stopped",
                "│ What goes into the template",
                "│ ╭",
                "│ Left out, as they belong to one VM:",
                "│ ▸ Template name",
                "│   Letters, digits, hyphens and underscores",
            ] {
                assert!(
                    lines.iter().any(|l| l.starts_with(want)),
                    "{w}x{hgt}: no line starts with {want:?}:\n{text}"
                );
            }
            assert!(
                screen_contains(&lines, "Defaults: 1 core, 2048 MiB RAM"),
                "{w}x{hgt}:\n{text}"
            );
            assert!(!text.contains("1 cores"), "{text}");
        }
    }

    #[test]
    fn keys_move_between_fields_like_go() {
        let h = Harness::new();
        let mut form = SaveTemplateForm::new(deb(), false);
        assert!(form.inputs[0].focused && !form.inputs[1].focused);
        let keys = |form: &SaveTemplateForm| {
            form.key_hints()
                .into_iter()
                .map(|(k, _)| k)
                .collect::<Vec<_>>()
        };
        assert_eq!(keys(&form), ["Tab/↓", "Shift-Tab/↑", "Ctrl-s", "Esc"]);

        // j/k type into a text field; Tab/↓ and Shift-Tab/↑ wrap around.
        h.press(&mut form, ch('j'));
        h.press(&mut form, ch('k'));
        assert_eq!(form.value(Field::Name), "debjk");
        h.press(&mut form, backtab());
        assert_eq!(
            form.field,
            Field::Button,
            "Shift-Tab wraps from the name to the button"
        );
        assert!(!form.inputs[0].focused);
        assert_eq!(keys(&form), ["Enter", "k/Shift-Tab", "Esc"]);
        h.press(&mut form, ch('j'));
        assert_eq!(form.field, Field::Name, "j on the button");
        h.press(&mut form, key(KeyCode::Down));
        assert_eq!(form.field, Field::Description);
        assert!(form.inputs[1].focused);
        h.press(&mut form, key(KeyCode::Down));
        assert_eq!(form.field, Field::Button);
        h.press(&mut form, ch('k'));
        assert_eq!(form.field, Field::Description, "k on the button");
        h.press(&mut form, key(KeyCode::Up));
        h.press(&mut form, key(KeyCode::Up));
        assert_eq!(form.field, Field::Button, "↑ wraps too");
        h.press(&mut form, key(KeyCode::Tab));
        assert_eq!(
            form.field,
            Field::Name,
            "Tab wraps from the button to the name"
        );

        // A paste lands in the focused input.
        let mut ctx = h.ctx();
        form.handle_paste("-x", &mut ctx);
        assert_eq!(form.value(Field::Name), "debjk-x");

        // Esc leaves.
        let acts = h.press(&mut form, key(KeyCode::Esc));
        assert!(matches!(acts.as_slice(), [Action::Close]), "{acts:?}");
    }

    #[test]
    fn task_results_close_or_keep_the_form() {
        let h = Harness::new();
        let mut form = SaveTemplateForm::new(deb(), false);
        form.busy = true;
        let mut ctx = h.ctx();
        form.on_task(
            TaskResult::TemplateSaveFailed {
                err: "copy disk image: boom".into(),
            },
            &mut ctx,
        );
        assert!(ctx.actions.is_empty() && !form.busy);
        assert_eq!(form.err, "copy disk image: boom");
        // Keys work again after an error.
        h.press(&mut form, key(KeyCode::Tab));
        assert_eq!(form.field, Field::Description);

        form.busy = true;
        let mut ctx = h.ctx();
        form.on_task(
            TaskResult::TemplateSaved {
                name: "deb-base".into(),
            },
            &mut ctx,
        );
        assert!(!form.busy);
        match ctx.actions.as_slice() {
            [Action::CloseSelectTemplate(name), Action::Notice(n)] => {
                assert_eq!(name, "deb-base");
                assert_eq!(n.text(), "saved template deb-base");
            }
            other => panic!("{other:?}"),
        }
        let mut ctx = h.ctx();
        form.on_task(TaskResult::Done { what: "x".into() }, &mut ctx);
        assert!(ctx.actions.is_empty());
    }
}
