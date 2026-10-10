//! The six-step wizard that makes a new VM from a template.

use anyhow::{anyhow, bail, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;

use super::super::events::{err_text, TaskResult};
use super::super::panel::{Ctx, Panel};
use super::super::theme::Theme;
use super::super::widgets::{plural, spinner_frame, Selector, TextInput};
use super::common::{
    bridge_hint_for, bridge_hint_lines, form_block, network_index, validate_vm_name,
    NETWORK_CHOICES, NETWORK_LABELS,
};
use super::create::{
    below, draw_input, draw_lines, draw_summary_box, error_lines, parse_u32, selector_lines,
    step_header, SummaryRow, CHAR_LIMIT,
};
use crate::vm::{
    format_port_forwards, parse_port_forwards, Manager, NetworkType, Template, VmConfig,
};

/// What the panel shows while the disk is being copied.
const COPYING: &str = "Copying the disk image… this takes a while for a large disk";

/// The wizard's steps, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Name,
    Cpu,
    Ram,
    Network,
    Forwards,
    Confirm,
}

impl Step {
    const ALL: [Step; 6] = [
        Step::Name,
        Step::Cpu,
        Step::Ram,
        Step::Network,
        Step::Forwards,
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
            Step::Network => "Network type",
            Step::Forwards => "Port forwards (optional — leave blank for none)",
            Step::Confirm => "Confirm",
        }
    }

    fn help(self) -> &'static str {
        match self {
            Step::Name => "Letters, digits, hyphens and underscores, e.g. debian-12-test",
            Step::Cpu => "Number of virtual CPU cores, e.g. 2",
            Step::Ram => "Memory in MiB, e.g. 2048 for 2 GiB",
            Step::Network => "h/l/←/→ to select: user (NAT) · tap (bridge) · none",
            Step::Forwards => "user networking only. Comma-separated [tcp|udp:]host:guest, e.g. 2222:22, udp:5353:53 — pick ports no other VM uses",
            Step::Confirm => "Press Enter to create the VM. The disk is copied from the template, which takes a while for a large disk",
        }
    }

    /// The step's index in the inputs array, or `None` for non-text steps.
    fn input_index(self) -> Option<usize> {
        match self {
            Step::Name => Some(0),
            Step::Cpu => Some(1),
            Step::Ram => Some(2),
            Step::Forwards => Some(3),
            _ => None,
        }
    }
}

/// The short wizard that makes a new VM from a template: the disk,
/// firmware and TPM come from the template; the name, CPU, RAM and network
/// are chosen here. See internal/tui/fromtemplate.go.
pub struct FromTemplateForm {
    tpl: Template,
    step: Step,
    /// name, cpu, ram, forwards
    inputs: [TextInput; 4],
    network: Selector,
    /// What the host lacks for tap networking; `""` when ready or not picked.
    bridge_hint: String,
    /// The display the new VM gets when the template has VNC.
    vnc_display: u16,
    /// The copy is in flight.
    busy: bool,
    err: String,
}

impl FromTemplateForm {
    /// A wizard pre-filled with the template's defaults. The suggested name
    /// (`<template>-N`) and the free VNC display are computed in `init`.
    pub fn new(tpl: Template) -> Self {
        let defaults = [
            String::new(),
            tpl.cpu.to_string(),
            tpl.ram.to_string(),
            String::new(),
        ];
        let mut inputs: [TextInput; 4] = std::array::from_fn(|i| {
            TextInput::new()
                .with_value(&defaults[i])
                .with_char_limit(CHAR_LIMIT)
        });
        inputs[0].focus();
        let network = Selector::new(NETWORK_LABELS, network_index(tpl.network));
        let bridge_hint = bridge_hint_for(NETWORK_CHOICES[network.index]);
        FromTemplateForm {
            tpl,
            step: Step::Name,
            inputs,
            network,
            bridge_hint,
            vnc_display: 0,
            busy: false,
            err: String::new(),
        }
    }

    /// The trimmed value of a text step.
    fn value(&self, step: Step) -> String {
        step.input_index()
            .map(|i| self.inputs[i].trimmed())
            .unwrap_or_default()
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
            if let Some(idx) = next.input_index() {
                self.inputs[idx].focus();
            }
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
        if let Some(idx) = prev.input_index() {
            self.inputs[idx].focus();
        }
    }

    fn validate_step(&self, step: Step, mgr: &Manager) -> Result<()> {
        match step {
            Step::Name => {
                let val = self.value(step);
                validate_vm_name(&val)?;
                if mgr.exists(&val) {
                    bail!("a VM named {val:?} already exists");
                }
            }
            // The config holds these as u32, so a larger number is refused
            // here rather than at the confirm step.
            Step::Cpu => {
                if !parse_u32(&self.value(step)).is_some_and(|v| v >= 1) {
                    bail!("CPU must be a positive integer");
                }
            }
            Step::Ram => {
                if !parse_u32(&self.value(step)).is_some_and(|v| v >= 64) {
                    bail!("RAM must be at least 64 MiB");
                }
            }
            Step::Forwards => {
                let fwds = parse_port_forwards(&self.value(step))?;
                if !fwds.is_empty() && NETWORK_CHOICES[self.network.index] != NetworkType::User {
                    bail!("port forwards need user (NAT) networking — go back and pick it, or leave this blank");
                }
            }
            Step::Network | Step::Confirm => {}
        }
        Ok(())
    }

    /// The config for the new VM: the user's choices on top of the
    /// template's machine. A template whose source had a VNC display gives
    /// the new VM the lowest display no other VM used when the wizard opened.
    fn build_config(&self) -> Result<VmConfig> {
        let mut cfg = self.tpl.new_vm_config();
        cfg.name = self.value(Step::Name);
        cfg.cpu = parse_u32(&self.value(Step::Cpu)).ok_or_else(|| anyhow!("invalid CPU value"))?;
        cfg.ram = parse_u32(&self.value(Step::Ram)).ok_or_else(|| anyhow!("invalid RAM value"))?;
        cfg.network.kind = NETWORK_CHOICES[self.network.index];
        cfg.network.port_forwards = parse_port_forwards(&self.value(Step::Forwards))?;
        cfg.vnc_port = self.vnc_display;
        Ok(cfg)
    }

    fn create(&mut self, ctx: &mut Ctx) {
        let mut cfg = match self.build_config() {
            Ok(cfg) => cfg,
            Err(e) => {
                self.err = err_text(&e);
                return;
            }
        };
        self.busy = true;
        let mgr = ctx.mgr.clone();
        let tpl_name = self.tpl.name.clone();
        ctx.spawn(
            move || match mgr.create_from_template(&tpl_name, &mut cfg) {
                Ok(()) => TaskResult::VmCreated { name: cfg.name },
                Err(e) => TaskResult::VmCreateFailed { err: err_text(&e) },
            },
        );
    }

    /// The rows of the confirm step's summary box.
    fn summary_rows(&self, cfg: &VmConfig) -> Vec<SummaryRow> {
        let mut net = cfg.network.kind.to_string();
        if !cfg.network.port_forwards.is_empty() {
            net.push_str(&format!(
                " [{}]",
                format_port_forwards(&cfg.network.port_forwards)
            ));
        }
        let vnc = if cfg.vnc_port > 0 {
            format!(
                "display {} (port {}) — the lowest one free",
                cfg.vnc_port,
                5900 + u32::from(cfg.vnc_port)
            )
        } else {
            "disabled".to_string()
        };
        vec![
            SummaryRow::new("Name:", &cfg.name),
            SummaryRow::new("Template:", &self.tpl.name),
            SummaryRow::new("CPU:", plural(cfg.cpu, "core", "cores")),
            SummaryRow::new("RAM:", format!("{} MiB", cfg.ram)),
            SummaryRow::new(
                "Disk:",
                format!("{} GiB (copied from the template)", cfg.disk_size),
            ),
            SummaryRow::new("Firmware:", cfg.firmware_label()),
            SummaryRow::new("Net:", net),
            SummaryRow::new("VNC:", vnc),
        ]
    }
}

/// `<base>-1`, or the first `<base>-N` not taken by a VM.
pub fn suggest_vm_name(mgr: &Manager, base: &str) -> String {
    (1u64..)
        .map(|n| format!("{base}-{n}"))
        .find(|name| !mgr.exists(name))
        .unwrap_or_else(|| format!("{base}-1"))
}

impl Panel for FromTemplateForm {
    fn title(&self) -> String {
        format!("New VM from Template: {}", self.tpl.name)
    }

    fn init(&mut self, ctx: &mut Ctx) {
        self.inputs[0].set_value(&suggest_vm_name(ctx.mgr, &self.tpl.name));
        if self.tpl.vnc {
            self.vnc_display = ctx.mgr.free_vnc_display();
        }
    }

    fn handle_key(&mut self, key: KeyEvent, ctx: &mut Ctx) {
        if self.busy {
            return; // the copy cannot be interrupted from here
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
            KeyCode::Char('j') if plain && !text_step => {
                self.advance(ctx);
                return;
            }
            KeyCode::Char('k') if plain && !text_step => {
                self.retreat(ctx);
                return;
            }
            KeyCode::Char('h') | KeyCode::Left if plain && self.step == Step::Network => {
                self.network.cycle(-1);
                self.bridge_hint = bridge_hint_for(NETWORK_CHOICES[self.network.index]);
            }
            KeyCode::Char('l') | KeyCode::Right if plain && self.step == Step::Network => {
                self.network.cycle(1);
                self.bridge_hint = bridge_hint_for(NETWORK_CHOICES[self.network.index]);
            }
            _ => {}
        }
        if let Some(idx) = self.step.input_index() {
            self.inputs[idx].handle_key(key);
        }
    }

    fn handle_paste(&mut self, text: &str, _ctx: &mut Ctx) {
        if self.busy {
            return;
        }
        if let Some(idx) = self.step.input_index() {
            self.inputs[idx].insert_str(text);
        }
    }

    fn on_task(&mut self, result: TaskResult, ctx: &mut Ctx) {
        match result {
            TaskResult::VmCreated { name } => {
                self.busy = false;
                ctx.close_select_vm(name.clone());
                ctx.notice_ok(format!("created {name}"));
            }
            TaskResult::VmCreateFailed { err } | TaskResult::Failed { err, .. } => {
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
        let width = inner.width as usize;

        let step_line = format!(
            "Step {} / {}   —   {} GiB disk, {}: from the template",
            self.step.index() + 1,
            Step::ALL.len(),
            self.tpl.disk_size,
            self.tpl.firmware_label()
        );
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
            Step::Network => {
                let mut lines = selector_lines(&self.network, theme, usize::from(rest.width));
                lines.push(Line::raw(""));
                let used = draw_lines(frame, rest, lines);
                rest = below(rest, used);
            }
            Step::Confirm => {
                if let Ok(cfg) = self.build_config() {
                    let used = draw_summary_box(frame, rest, &self.summary_rows(&cfg), theme);
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
        if self.busy {
            tail.push(Line::from(vec![
                Span::styled(spinner_frame(tick), theme.spinner),
                Span::styled(format!(" {COPYING}"), theme.normal),
            ]));
        } else if !self.err.is_empty() {
            tail.extend(error_lines(&self.err, width, theme));
        }
        draw_lines(frame, rest, tail);
    }

    fn key_hints(&self) -> Vec<(String, String)> {
        // While copying every key but Ctrl-c is swallowed; the body shows
        // the progress line.
        let pairs: Vec<(&str, &str)> = if self.busy {
            vec![("Ctrl-c", "quit")]
        } else if self.step == Step::Network {
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
        self.busy.then(|| "copying the disk image…".to_string())
    }

    fn failed(&self) -> bool {
        !self.err.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;

    use chrono::Utc;
    use crossterm::event::KeyCode;

    use super::*;
    use crate::tui::panel::Action;
    use crate::tui::testutil::*;
    use crate::vm::{
        load_config, save_config, save_template, template_dir, template_disk_path, vm_dir,
        FirmwareType, NetworkType, PortForward,
    };

    fn template(name: &str, cpu: u32, ram: u32, disk_size: u32) -> Template {
        Template {
            name: name.to_string(),
            description: String::new(),
            source_vm: "src".to_string(),
            cpu,
            ram,
            disk_size,
            arch: "x86_64".to_string(),
            firmware: FirmwareType::Bios,
            secure_boot: false,
            tpm: false,
            network: NetworkType::User,
            vnc: false,
            created_at: Utc::now(),
        }
    }

    /// A Windows-style template: UEFI + Secure Boot, TPM, VNC.
    fn win_template() -> Template {
        Template {
            firmware: FirmwareType::Uefi,
            secure_boot: true,
            tpm: true,
            vnc: true,
            ..template("win-base", 4, 8192, 64)
        }
    }

    /// Writes a `vm.yaml` so the VM counts for `exists` and the VNC displays.
    fn write_vm(h: &Harness, name: &str, vnc_port: u16) {
        let storage = h.mgr.storage();
        fs::create_dir_all(vm_dir(storage, name)).unwrap();
        let cfg = VmConfig {
            name: name.to_string(),
            cpu: 1,
            ram: 128,
            disk_size: 1,
            vnc_port,
            ..VmConfig::default()
        };
        save_config(storage, &cfg).unwrap();
    }

    /// Lays down a template directory with a small qcow2 disk and its yaml,
    /// the way `create_template` would have. Skips (returns false) without
    /// qemu-img.
    fn write_template(h: &Harness, tpl: &Template) -> bool {
        if which::which("qemu-img").is_err() {
            eprintln!("skipping: qemu-img not installed");
            return false;
        }
        let storage = h.mgr.storage();
        fs::create_dir_all(template_dir(storage, &tpl.name)).unwrap();
        let out = Command::new("qemu-img")
            .args(["create", "-f", "qcow2"])
            .arg(template_disk_path(storage, &tpl.name))
            .arg("64M")
            .output()
            .expect("qemu-img create");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        save_template(storage, tpl).unwrap();
        true
    }

    fn retype(form: &mut FromTemplateForm, h: &Harness, s: &str) {
        h.press(form, ctrl('u'));
        type_str(form, h, s);
    }

    fn enter(form: &mut FromTemplateForm, h: &Harness) -> Vec<Action> {
        h.press(form, key(KeyCode::Enter))
    }

    #[test]
    fn suggests_the_next_free_name() {
        let h = Harness::new();
        assert_eq!(suggest_vm_name(&h.mgr, "deb"), "deb-1");
        write_vm(&h, "deb-1", 0);
        write_vm(&h, "deb-2", 0);
        assert_eq!(suggest_vm_name(&h.mgr, "deb"), "deb-3");
        write_vm(&h, "deb", 0);
        assert_eq!(
            suggest_vm_name(&h.mgr, "deb"),
            "deb-3",
            "only <base>-N counts"
        );
    }

    #[test]
    fn prefilled_from_the_template() {
        let h = Harness::new();
        write_vm(&h, "win", 1);
        let mut form = FromTemplateForm::new(win_template());
        assert!(h.init(&mut form).is_empty());
        assert_eq!(form.title(), "New VM from Template: win-base");
        assert_eq!(form.value(Step::Name), "win-base-1");
        assert_eq!(form.value(Step::Cpu), "4");
        assert_eq!(form.value(Step::Ram), "8192");
        assert_eq!(NETWORK_CHOICES[form.network.index], NetworkType::User);
        assert_eq!(form.value(Step::Forwards), "", "port forwards start blank");
        assert_eq!(form.vnc_display, 2, "the lowest free display");
        assert!(form.inputs[0].focused);
        assert!(form.busy().is_none());

        let screen = h.render(&mut form, 110, 40);
        for want in [
            "New VM from Template: win-base",
            "Step 1 / 6   —   64 GiB disk, UEFI + Secure Boot, TPM 2.0: from the template",
            "VM Name",
            "Letters, digits, hyphens and underscores, e.g. debian-12-test",
            "▸ win-base-1",
        ] {
            assert!(
                screen_contains(&screen, want),
                "view lacks {want:?}:\n{}",
                screen.join("\n")
            );
        }
        assert_eq!(form.key_hints()[0].0, "Tab/↓");

        // A template without VNC gives none, and a tap template starts on tap.
        let mut plain = FromTemplateForm::new(Template {
            network: NetworkType::Tap,
            ..template("deb-base", 1, 512, 8)
        });
        h.init(&mut plain);
        assert_eq!(plain.vnc_display, 0);
        assert_eq!(NETWORK_CHOICES[plain.network.index], NetworkType::Tap);
        assert_eq!(plain.bridge_hint, bridge_hint_for(NetworkType::Tap));
    }

    /// Walks the steps: keep the name, halve the resources, pick tap, then
    /// try forwards with tap (refused), go back to user, set forwards.
    #[test]
    fn walks_to_the_summary() {
        let h = Harness::new();
        write_vm(&h, "win", 1);
        let mut form = FromTemplateForm::new(win_template());
        h.init(&mut form);
        for step in Step::ALL {
            if step == Step::Confirm {
                break;
            }
            assert_eq!(form.step, step, "err {:?}", form.err);
            let screen = h.render(&mut form, 110, 40);
            assert!(
                screen_contains(&screen, step.label()),
                "step {step:?} view lacks its label"
            );
            assert!(screen_contains(
                &screen,
                "64 GiB disk, UEFI + Secure Boot, TPM 2.0"
            ));
            match step {
                Step::Cpu => retype(&mut form, &h, "2"),
                Step::Ram => retype(&mut form, &h, "4096"),
                Step::Network => {
                    h.press(&mut form, ch('l')); // user → tap
                    assert_eq!(form.bridge_hint, bridge_hint_for(NetworkType::Tap));
                }
                Step::Forwards => {
                    type_str(&mut form, &h, "2222:22");
                    enter(&mut form, &h);
                    assert_eq!(form.step, Step::Forwards);
                    assert_eq!(
                        form.err,
                        "port forwards need user (NAT) networking — go back and pick it, or leave this blank"
                    );
                    let screen = h.render(&mut form, 110, 40);
                    assert!(screen_contains(
                        &screen,
                        "✗ port forwards need user (NAT) networking"
                    ));
                    h.press(&mut form, backtab()); // k would type into the field
                    assert_eq!(form.step, Step::Network);
                    assert!(form.err.is_empty());
                    h.press(&mut form, ch('h')); // tap → user
                    enter(&mut form, &h);
                    assert_eq!(
                        form.value(Step::Forwards),
                        "2222:22",
                        "the field keeps its value"
                    );
                }
                _ => {}
            }
            enter(&mut form, &h);
        }
        assert_eq!(form.step, Step::Confirm, "err {:?}", form.err);
        assert_eq!(
            form.key_hints()[0],
            ("Enter/j".to_string(), "create VM".to_string())
        );
        let screen = h.render(&mut form, 110, 40);
        for want in [
            "Step 6 / 6",
            "Name:     win-base-1",
            "Template: win-base",
            "CPU:      2 cores",
            "RAM:      4096 MiB",
            "64 GiB (copied from the template)",
            "UEFI + Secure Boot, TPM 2.0",
            "user [tcp:2222:22]",
            "display 2 (port 5902) — the lowest one free",
        ] {
            assert!(
                screen_contains(&screen, want),
                "confirm view lacks {want:?}:\n{}",
                screen.join("\n")
            );
        }
        let cfg = form.build_config().unwrap();
        assert_eq!(
            (cfg.cpu, cfg.ram, cfg.disk_size, cfg.vnc_port),
            (2, 4096, 64, 2)
        );
        assert!(cfg.secure_boot && cfg.tpm);
        assert_eq!(
            cfg.network.port_forwards,
            vec![PortForward {
                host: 2222,
                guest: 22,
                proto: "tcp".into()
            }]
        );
        assert_eq!(cfg.name, "win-base-1");

        // A bad forward is reported in the parser's words.
        form.step = Step::Forwards;
        form.inputs[3].set_value("2222");
        enter(&mut form, &h);
        assert!(
            form.err.contains("expected [tcp|udp:]host:guest"),
            "{}",
            form.err
        );
    }

    #[test]
    fn name_cpu_and_ram_validate() {
        let h = Harness::new();
        write_vm(&h, "win", 0);
        let mut form = FromTemplateForm::new(win_template());
        h.init(&mut form);
        assert_eq!(form.value(Step::Name), "win-base-1");
        retype(&mut form, &h, "win");
        enter(&mut form, &h);
        assert_eq!(form.step, Step::Name);
        assert_eq!(form.err, "a VM named \"win\" already exists");
        retype(&mut form, &h, "");
        enter(&mut form, &h);
        assert_eq!(form.err, "name cannot be empty");
        retype(&mut form, &h, "ok");
        enter(&mut form, &h);
        assert_eq!(form.step, Step::Cpu);
        retype(&mut form, &h, "0");
        enter(&mut form, &h);
        assert_eq!(form.err, "CPU must be a positive integer");
        retype(&mut form, &h, "1");
        enter(&mut form, &h);
        retype(&mut form, &h, "63");
        enter(&mut form, &h);
        assert_eq!(form.err, "RAM must be at least 64 MiB");
        assert_eq!(form.step, Step::Ram);
        // Esc leaves at any step; back from the first step too.
        assert!(matches!(
            h.press(&mut form, key(KeyCode::Esc)).as_slice(),
            [Action::Close]
        ));
        form.step = Step::Name;
        assert!(matches!(
            h.press(&mut form, backtab()).as_slice(),
            [Action::Close]
        ));
        // h/l only cycle on the network step: typed on text steps.
        h.press(&mut form, ch('h'));
        assert!(form.inputs[0].value().ends_with('h'));
        assert_eq!(form.network.index, 0);
    }

    /// A count beyond what the config holds (u32) is refused on its own
    /// step in the step's words, not at the confirm step.
    #[test]
    fn values_beyond_u32_are_refused_on_their_step() {
        let h = Harness::new();
        let mut form = FromTemplateForm::new(win_template());
        h.init(&mut form);
        enter(&mut form, &h);
        for (step, msg) in [
            (Step::Cpu, "CPU must be a positive integer"),
            (Step::Ram, "RAM must be at least 64 MiB"),
        ] {
            assert_eq!(form.step, step);
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
        enter(&mut form, &h);
        enter(&mut form, &h);
        assert_eq!(form.step, Step::Confirm, "err {:?}", form.err);
        let screen = h.render(&mut form, 120, 40);
        assert!(
            screen_contains(&screen, "CPU:      4294967295 cores"),
            "{}",
            screen.join("\n")
        );
    }

    /// The summary box is capped at the pane: a row too wide for it ends in
    /// an ellipsis after a whole word instead of being cut mid-word by the
    /// border. 50x22 is the right-hand pane of an 80x24 terminal; one core
    /// is `1 core`.
    #[test]
    fn summary_rows_are_cut_with_an_ellipsis() {
        let h = Harness::new();
        write_vm(&h, "win", 1);
        let mut form = FromTemplateForm::new(Template {
            cpu: 1,
            ..win_template()
        });
        h.init(&mut form);
        form.step = Step::Confirm;
        let screen = h.render(&mut form, 50, 22);
        let text = screen.join("\n");
        let row = |key: &str| {
            screen
                .iter()
                .find(|l| l.contains(key))
                .unwrap_or_else(|| panic!("no {key:?} row:\n{text}"))
                .clone()
        };
        for key in [
            "│ Disk:     64 GiB (copied from the… ",
            "│ VNC:      display 2 (port 5902) — the… ",
        ] {
            assert!(row(key).ends_with(" │ │"), "{key}:\n{text}");
        }
        assert!(row("│ CPU:      1 core ").ends_with(" │ │"), "{text}");
        assert!(!text.contains("1 cores"), "{text}");
        // Wide enough, nothing is cut.
        for (w, hgt) in [(80, 24), (120, 40)] {
            let screen = h.render(&mut form, w, hgt);
            let text = screen.join("\n");
            for want in [
                "Disk:     64 GiB (copied from the template) ",
                "VNC:      display 2 (port 5902) — the lowest one free │",
                "CPU:      1 core ",
            ] {
                assert!(
                    screen_contains(&screen, want),
                    "{w}x{hgt} lacks {want:?}:\n{text}"
                );
            }
            assert!(!text.contains('…'), "{w}x{hgt}:\n{text}");
        }
    }

    #[test]
    fn creates_a_vm_for_real() {
        let h = Harness::new();
        let tpl = template("deb-base", 1, 512, 1);
        if !write_template(&h, &tpl) {
            return;
        }
        let mut form = FromTemplateForm::new(tpl.clone());
        h.init(&mut form);
        assert_eq!(form.value(Step::Name), "deb-base-1");
        for _ in 0..5 {
            assert!(enter(&mut form, &h).is_empty(), "err {:?}", form.err);
        }
        assert_eq!(form.step, Step::Confirm, "err {:?}", form.err);
        let screen = h.render(&mut form, 110, 40);
        assert!(
            screen_contains(&screen, "Disk:     1 GiB (copied from the template)"),
            "{}",
            screen.join("\n")
        );
        assert!(screen_contains(&screen, "VNC:      disabled"));

        // Enter creates the VM; the UI is busy until the copy is done.
        assert!(enter(&mut form, &h).is_empty());
        assert!(form.busy);
        assert_eq!(form.busy().as_deref(), Some("copying the disk image…"));
        assert_eq!(
            form.key_hints(),
            vec![("Ctrl-c".to_string(), "quit".to_string())],
            "only the key that still works"
        );
        let screen = h.render(&mut form, 110, 40);
        assert!(
            screen_contains(
                &screen,
                "⠋ Copying the disk image… this takes a while for a large disk"
            ),
            "{}",
            screen.join("\n")
        );
        // Keys are ignored while the copy runs.
        assert!(h.press(&mut form, key(KeyCode::Esc)).is_empty());
        assert!(form.busy && form.step == Step::Confirm);

        let actions = h.deliver_next(&mut form);
        match actions.as_slice() {
            [Action::CloseSelectVm(name), Action::Notice(n)] => {
                assert_eq!(name, "deb-base-1");
                assert_eq!(n.text(), "created deb-base-1");
            }
            other => panic!("{other:?} (err {:?})", form.err),
        }
        assert!(!form.busy);
        let clone = load_config(h.mgr.storage(), "deb-base-1").unwrap();
        assert_eq!(
            (clone.cpu, clone.ram, clone.disk_size, clone.vnc_port),
            (1, 512, 1, 0)
        );
        assert_eq!(clone.network.kind, NetworkType::User);
        assert!(!clone.network.mac.is_empty());
        assert!(crate::vm::disk_path(h.mgr.storage(), "deb-base-1").exists());

        // The next wizard suggests the next free name; a taken name is refused.
        let mut again = FromTemplateForm::new(tpl);
        h.init(&mut again);
        assert_eq!(again.value(Step::Name), "deb-base-2");
        retype(&mut again, &h, "deb-base-1");
        enter(&mut again, &h);
        assert_eq!(again.step, Step::Name);
        assert!(again.err.contains("already exists"), "{}", again.err);
    }

    #[test]
    fn a_failed_copy_shows_the_error_and_stays() {
        let h = Harness::new();
        // The yaml is there but the disk is not: the copy fails.
        let tpl = template("broken", 1, 512, 1);
        fs::create_dir_all(template_dir(h.mgr.storage(), "broken")).unwrap();
        save_template(h.mgr.storage(), &tpl).unwrap();
        let mut form = FromTemplateForm::new(tpl);
        h.init(&mut form);
        form.step = Step::Confirm;
        enter(&mut form, &h);
        assert!(form.busy);
        // Busy: the bar offers only the key that still works; the body and
        // the status bar show the progress.
        assert_eq!(
            form.key_hints(),
            vec![("Ctrl-c".to_string(), "quit".to_string())]
        );
        let actions = h.deliver_next(&mut form);
        assert!(actions.is_empty(), "{actions:?}");
        assert!(!form.busy);
        assert!(!form.err.is_empty());
        assert_eq!(form.step, Step::Confirm);
        let screen = h.render(&mut form, 110, 40);
        assert!(screen_contains(&screen, "✗ "), "{}", screen.join("\n"));
        assert!(
            screen_contains(&screen, "Name:     broken-1"),
            "the summary stays"
        );
        assert!(
            !vm_dir(h.mgr.storage(), "broken-1").exists(),
            "a failed clone is cleaned up"
        );
        assert_eq!(form.key_hints()[0].0, "Enter/j", "the hints are back");
        // Keys work again.
        h.press(&mut form, ch('k'));
        assert_eq!(form.step, Step::Forwards);
        assert!(form.err.is_empty());

        // A task that failed some other way is shown the same.
        let mut ctx = h.ctx();
        form.busy = true;
        form.on_task(
            TaskResult::Failed {
                what: "background task".into(),
                err: "internal error: x".into(),
            },
            &mut ctx,
        );
        assert!(!form.busy);
        assert_eq!(form.err, "internal error: x");
    }

    #[test]
    fn renders_in_a_tiny_area_without_panicking() {
        let h = Harness::new();
        let mut form = FromTemplateForm::new(win_template());
        h.init(&mut form);
        form.bridge_hint = "the host has no bridge br0\nRun this".to_string();
        for step in Step::ALL {
            form.step = step;
            for (w, hgt) in [(0, 0), (1, 1), (3, 2), (8, 5), (20, 6), (30, 3)] {
                h.render(&mut form, w, hgt);
            }
        }
        form.busy = true;
        h.render(&mut form, 10, 3);
        form.busy = false;
        form.err = "x".repeat(80);
        h.render(&mut form, 12, 4);
    }
}
