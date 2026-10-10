//! Drawing the dashboard: the VM and template lists on the left, the details
//! card and the console title on the right, the status and key-hint bars.
//!
//! Layout (left column ~34 %, right column the rest; two one-line bars):
//!
//! ```text
//! ╭ Virtual Machines (3) ──────────╮╭ debian-12 ───────────────────────────────╮
//! │ ▸ debian-12     ● running      ││ Status    ● running  (PID 94231)         │
//! │   ubuntu-24     ● stopped      ││ CPU       2 cores                        │
//! │   windows-11    ● stopped      ││ RAM       2048 MiB                       │
//! │                                ││ …                                        │
//! │                                ││ … 3 more — enlarge the terminal          │
//! ╰────────────────────────────────╯╰──────────────────────────────────────────╯
//! ╭ Templates (2) ─────────────────╮╭ Serial console · debian-12 · last 200 lines ╮
//! │   debian-12-base  UEFI, TPM…   ││ [    0.000000] Linux version 6.1.0 …        │
//! │   win11-base      UEFI + Secu… ││ debian login:                               │
//! ╰────────────────────────────────╯╰────────────────────────────────────────────╯
//!  ✓ started debian-12                                        ⠋ starting ubuntu-24…
//!  j/k move  s start  x stop  n new  e edit  d delete  ? keys  q quit
//! ```
//!
//! Nothing is cut by a border: a list row leaves a cell free on the right, a
//! card value too wide for the card ends in `…` (paths keep their end), a
//! card too short for its rows says how many are hidden, and the hints bar
//! drops whole hints, the least useful first, keeping `? keys` and `q quit`.

use chrono::{DateTime, Local, Utc};
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Paragraph};
use unicode_width::UnicodeWidthStr;

use super::app::{App, Focus};
use super::console::{CONSOLE_POLL_SECONDS, CONSOLE_TAIL_LINES};
use super::events::{Details, Notice, TemplateEntry, VmEntry};
use super::theme::Theme;
use super::widgets::{
    ellipsize, fit_left, fit_words, hints_line, human_size, if_empty, pad_right, plural,
    spinner_frame, truncate, wrap_text,
};
use crate::vm::{
    ImageState, NetworkType, ProcessInfo, UsbState, VmConfig, USER_NET_GATEWAY, USER_NET_GUEST_IP,
};

/// The label column of the details cards.
const LABEL_WIDTH: usize = 10;
/// The widest the name column of a list grows; longer names are cut.
const NAME_WIDTH_MAX: usize = 20;
/// The widest the USB device label column of the details card grows, as on
/// the Go screen.
const USB_NAME_WIDTH: usize = 32;
/// The extra-disk name column of the details card, as on the Go screen.
const DISK_NAME_WIDTH: usize = 20;
/// The widest a state counts for when a card lines its states up; a longer
/// one (an unusual error) is cut rather than squeezing every path.
const STATE_ALIGN_MAX: usize = 18;
/// Margin + marker + two separators + `● running` + a free cell on the
/// right: what a VM row needs besides the name.
const VM_ROW_FIXED: usize = 1 + 2 + 2 + 9 + 1;
/// Margin + marker + two separators + a free cell on the right: what a
/// template row needs besides the name.
const TPL_ROW_FIXED: usize = 1 + 2 + 2 + 1;
/// What a details card shows for a column the first refresh has not filled yet.
const PENDING: &str = "…";

// ---------------------------------------------------------------------------
// Text fitting
// ---------------------------------------------------------------------------

/// `s` padded with spaces to `width` cells (never cut).
fn pad_w(mut s: String, width: usize) -> String {
    let w = s.width();
    s.extend(std::iter::repeat_n(' ', width.saturating_sub(w)));
    s
}

/// Cuts a line to `width` cells; a line that had to be cut ends with an
/// ellipsis in the style of the span it was cut in, at a word boundary when
/// one is near.
fn truncate_line(line: Line<'static>, width: usize) -> Line<'static> {
    if line.width() <= width {
        return line;
    }
    if width == 0 {
        return Line::default();
    }
    let mut out = Vec::new();
    let mut used = 0;
    for span in line.spans {
        let w = span.width();
        // A span that leaves a cell for the ellipsis stays whole.
        if used + w < width {
            used += w;
            out.push(span);
            continue;
        }
        out.push(Span::styled(
            ellipsize(&span.content, width - used),
            span.style,
        ));
        break;
    }
    Line::from(out).style(line.style)
}

// ---------------------------------------------------------------------------
// Panes
// ---------------------------------------------------------------------------

/// A rounded bordered block for a dashboard pane: the focused pane's border
/// and title stand out, every other one is dimmed.
fn pane_block(title: String, focused: bool, theme: &Theme) -> Block<'static> {
    let (border, title_style) = if focused {
        (theme.border_focused, theme.title)
    } else {
        (theme.border, theme.help)
    };
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(border)
        .title(Line::styled(title, title_style))
}

/// Pads a list row with spaces to `width` cells so the row style covers the
/// whole row, not just its text.
fn fill_row(mut line: Line<'static>, width: usize) -> Line<'static> {
    let w = line.width();
    if w < width {
        line.spans.push(Span::raw(" ".repeat(width - w)));
    }
    line
}

/// The style of a list row: the cursor row is highlighted, strongly in the
/// focused pane and faintly elsewhere.
fn row_style(selected: bool, focused: bool, theme: &Theme) -> Style {
    match (selected, focused) {
        (true, true) => theme.selected,
        (true, false) => theme.selected_unfocused,
        _ => theme.normal,
    }
}

/// The widest name among `names`, capped at [`NAME_WIDTH_MAX`] and at what
/// the pane has left after `fixed` cells of other content.
fn name_column(names: impl Iterator<Item = usize>, width: usize, fixed: usize) -> usize {
    names
        .max()
        .unwrap_or(0)
        .min(NAME_WIDTH_MAX)
        .min(width.saturating_sub(fixed))
        .max(1)
}

/// `● running` / `● stopped`, styled.
fn status_glyph(running: bool, theme: &Theme) -> Span<'static> {
    if running {
        Span::styled("● running", theme.running)
    } else {
        Span::styled("● stopped", theme.stopped)
    }
}

/// How much of a resource summary the VM rows carry: the Go list's
/// `CPU: n  RAM: n MiB  Disk: n GiB[ +k]` (`+k` counts the additional disks),
/// the shorter `n CPU · n MiB · n GiB[ +k]`, or none. One choice for the whole
/// list, so the column lines up, and never a cut-off summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Summary {
    Long,
    Short,
    None,
}

/// A VM's resource summary in one of the two wordings.
fn vm_summary(cfg: &VmConfig, long: bool) -> String {
    let extra = if cfg.disks.is_empty() {
        String::new()
    } else {
        format!(" +{}", cfg.disks.len())
    };
    if long {
        format!(
            "   CPU: {}  RAM: {} MiB  Disk: {} GiB{extra}",
            cfg.cpu, cfg.ram, cfg.disk_size
        )
    } else {
        format!(
            "  {} CPU · {} MiB · {} GiB{extra}",
            cfg.cpu, cfg.ram, cfg.disk_size
        )
    }
}

/// The widest summary tier that fits every VM's summary in `room` cells.
fn summary_tier(vms: &[VmEntry], room: usize) -> Summary {
    let fits = |long: bool| vms.iter().all(|v| vm_summary(&v.cfg, long).width() <= room);
    if fits(true) {
        Summary::Long
    } else if fits(false) {
        Summary::Short
    } else {
        Summary::None
    }
}

/// One row of the VM list: a cell of margin, marker, name, status glyph and,
/// when the pane has room for it, the resource summary dimmed; a cell is
/// left free before the border.
fn vm_row(
    v: &VmEntry,
    selected: bool,
    focused: bool,
    name_w: usize,
    summary: Summary,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let marker = if selected { " ▸ " } else { "   " };
    // Secondary text keeps the row's own colour on the cursor row so it stays
    // readable on the highlight background.
    let dim = if selected {
        Style::default()
    } else {
        theme.help
    };
    let mut spans = vec![
        Span::raw(marker),
        Span::raw(pad_right(&v.cfg.name, name_w)),
        Span::raw("  "),
        status_glyph(v.status.running(), theme),
    ];
    match summary {
        Summary::Long => spans.push(Span::styled(vm_summary(&v.cfg, true), dim)),
        Summary::Short => spans.push(Span::styled(vm_summary(&v.cfg, false), dim)),
        Summary::None => {}
    }
    fill_row(Line::from(spans), width).style(row_style(selected, focused, theme))
}

/// One row of the templates list: a cell of margin, marker, name and the
/// firmware label dimmed, cut with an ellipsis to the room the pane has
/// (`UEFI + Secure…`) and a cell clear of the border.
fn template_row(
    t: &TemplateEntry,
    selected: bool,
    focused: bool,
    name_w: usize,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let marker = if selected { " ▸ " } else { "   " };
    let dim = if selected {
        Style::default()
    } else {
        theme.help
    };
    let room = width.saturating_sub(TPL_ROW_FIXED + name_w);
    let spans = vec![
        Span::raw(marker),
        Span::raw(pad_right(&t.tpl.name, name_w)),
        Span::raw("  "),
        Span::styled(fit_words(&t.tpl.firmware_label(), room), dim),
    ];
    fill_row(Line::from(spans), width).style(row_style(selected, focused, theme))
}

/// Draws the VM list pane into `area` and returns the number of rows it can
/// show (for page moves). Title `Virtual Machines (n)`; the focused pane's
/// border and title use the focused style. Each row: `▸ ` marker on the
/// cursor row, the name padded, `● running` / `● stopped`, and when the pane
/// has room `CPU: n  RAM: n MiB  Disk: n GiB[ +k]` dimmed. `Loading…` before
/// the first load, `No VMs yet. Press n to create one.` when empty, the VM
/// listing's error when it failed (a templates listing error does not hide
/// the VMs: it shows in the templates pane).
pub fn render_vm_list(app: &mut App, frame: &mut Frame, area: Rect) -> usize {
    let theme = app.theme.clone();
    let focused = app.focus == Focus::Vms;
    let title = if app.loading {
        " Virtual Machines ".to_string()
    } else {
        format!(" Virtual Machines ({}) ", app.vms.len())
    };
    let block = pane_block(title, focused, &theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return 0;
    }
    let rows = inner.height as usize;
    let width = inner.width as usize;
    // Messages sit a cell in from the borders, like the card's text; rows
    // span the pane so the cursor highlight runs border to border.
    let text_area = inner.inner(Margin::new(1, 0));
    let text_w = text_area.width as usize;
    let message = |text: String, style: Style| -> Vec<Line<'static>> {
        wrap_text(&text, text_w)
            .into_iter()
            .map(|l| Line::styled(l, style))
            .collect()
    };
    if let Some(err) = &app.vm_list_err {
        frame.render_widget(
            Paragraph::new(message(format!("✗ {err}"), theme.error)),
            text_area,
        );
    } else if app.loading {
        frame.render_widget(
            Paragraph::new(message("Loading…".to_string(), theme.help)),
            text_area,
        );
    } else if app.vms.is_empty() {
        let lines = message("No VMs yet. Press n to create one.".to_string(), theme.help);
        frame.render_widget(Paragraph::new(lines), text_area);
    } else {
        let name_w = name_column(
            app.vms.iter().map(|v| v.cfg.name.width()),
            width,
            VM_ROW_FIXED,
        );
        let summary = summary_tier(&app.vms, width.saturating_sub(VM_ROW_FIXED + name_w));
        let cursor = app.vm_cursor.index;
        let range = app.vm_cursor.window(app.vms.len(), rows);
        let lines: Vec<Line> = app.vms[range.clone()]
            .iter()
            .zip(range)
            .map(|(v, i)| vm_row(v, i == cursor, focused, name_w, summary, width, &theme))
            .collect();
        frame.render_widget(Paragraph::new(lines), inner);
    }
    rows
}

/// Draws the templates pane and returns its visible row count. Title
/// `Templates (n)`; rows: marker, name padded, firmware label dimmed. Empty:
/// `No templates yet — t saves a stopped VM as one.`, or a shorter form of
/// it where that does not fit; the templates listing's error when it failed.
pub fn render_templates(app: &mut App, frame: &mut Frame, area: Rect) -> usize {
    let theme = app.theme.clone();
    let focused = app.focus == Focus::Templates;
    let title = if app.loading {
        " Templates ".to_string()
    } else {
        format!(" Templates ({}) ", app.templates.len())
    };
    let block = pane_block(title, focused, &theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return 0;
    }
    let rows = inner.height as usize;
    let width = inner.width as usize;
    let text_area = inner.inner(Margin::new(1, 0));
    let text_w = text_area.width as usize;
    let message = |text: &str, style: Style| -> Vec<Line<'static>> {
        wrap_text(text, text_w)
            .into_iter()
            .map(|l| Line::styled(l, style))
            .collect()
    };
    if let Some(err) = &app.tpl_list_err {
        frame.render_widget(
            Paragraph::new(message(&format!("✗ {err}"), theme.error)),
            text_area,
        );
    } else if app.loading {
        frame.render_widget(Paragraph::new(message("Loading…", theme.help)), text_area);
    } else if app.templates.is_empty() {
        const EMPTY: [&str; 3] = [
            "No templates yet — t saves a stopped VM as one.",
            "No templates yet — t saves one.",
            "No templates yet.",
        ];
        let text = EMPTY
            .iter()
            .find(|t| wrap_text(t, text_w).len() <= rows)
            .unwrap_or(&EMPTY[2]);
        frame.render_widget(Paragraph::new(message(text, theme.help)), text_area);
    } else {
        let name_w = name_column(
            app.templates.iter().map(|t| t.tpl.name.width()),
            width,
            TPL_ROW_FIXED,
        );
        let cursor = app.tpl_cursor.index;
        let range = app.tpl_cursor.window(app.templates.len(), rows);
        let lines: Vec<Line> = app.templates[range.clone()]
            .iter()
            .zip(range)
            .map(|(t, i)| template_row(t, i == cursor, focused, name_w, width, &theme))
            .collect();
        frame.render_widget(Paragraph::new(lines), inner);
    }
    rows
}

// ---------------------------------------------------------------------------
// Details cards
// ---------------------------------------------------------------------------

/// The rows of a details card, laid out for the card's size when it is
/// drawn ([`render_details`]). A value too wide for the card is cut with an
/// ellipsis (a path on the left, so the file name stays), the state columns
/// of the image and device rows line up, and a card too short for its rows
/// ends with `… n more — enlarge the terminal`. The row count does not
/// depend on the width.
pub struct Card {
    rows: Vec<CardRow>,
    /// The style of the label column and of the `… n more` row.
    muted: Style,
}

/// The rows whose shared column lines their states up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    /// The extra disks, the boot ISO and the USB images.
    Files,
    /// The passed-through USB devices.
    Usb,
}

/// The shared column of a [`CardRow::Column`].
enum Cell {
    /// A path, cut on the left so the file name stays visible.
    Path(String),
    /// A name in a `name_w`-cell column, then `tail` (`   50 GiB`,
    /// `  046d:085c`). A wider column pads the name, a narrower one cuts it;
    /// the tail stays whole.
    Name {
        name: String,
        name_w: usize,
        tail: String,
    },
}

impl Cell {
    /// The cells this column wants.
    fn width(&self) -> usize {
        match self {
            Cell::Path(p) => p.width(),
            Cell::Name { name_w, tail, .. } => name_w + tail.width(),
        }
    }

    /// The column laid out in exactly `width` cells (more only when the
    /// tail alone is wider).
    fn layout(&self, width: usize) -> String {
        match self {
            Cell::Path(p) => pad_w(fit_left(p, width), width),
            Cell::Name { name, tail, .. } => {
                let room = width.saturating_sub(tail.width());
                format!("{}{tail}", pad_w(fit_words(name, room), room))
            }
        }
    }
}

enum CardRow {
    /// A line of its own, without the label column.
    Text(Line<'static>),
    /// A label and its value in one or more wordings, longest first: the
    /// first that fits is drawn, the last is cut when none does.
    Field {
        label: &'static str,
        values: Vec<Span<'static>>,
    },
    /// A label (empty on a continuation row), a column as wide as its
    /// group's widest (or the room the states leave), and a state.
    Column {
        label: &'static str,
        group: Group,
        cell: Cell,
        style: Style,
        state: Span<'static>,
    },
}

impl Card {
    fn new(theme: &Theme) -> Self {
        Card {
            rows: Vec::new(),
            muted: theme.help,
        }
    }

    /// How many rows the card has.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether the card has no rows.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    fn text(&mut self, line: Line<'static>) {
        self.rows.push(CardRow::Text(line));
    }

    fn field(&mut self, label: &'static str, value: Span<'static>) {
        self.fields(label, vec![value]);
    }

    fn fields(&mut self, label: &'static str, values: Vec<Span<'static>>) {
        self.rows.push(CardRow::Field { label, values });
    }

    fn column(
        &mut self,
        label: &'static str,
        group: Group,
        cell: Cell,
        style: Style,
        state: Span<'static>,
    ) {
        self.rows.push(CardRow::Column {
            label,
            group,
            cell,
            style,
            state,
        });
    }

    /// The width of `group`'s column in a card `width` cells wide: its
    /// widest cell, or what is left once the label, the gap and the widest
    /// state have their room.
    fn column_width(&self, group: Group, width: usize) -> usize {
        let (mut want, mut state_w) = (0, 0);
        for row in &self.rows {
            if let CardRow::Column {
                group: g,
                cell,
                state,
                ..
            } = row
            {
                if *g == group {
                    want = want.max(cell.width());
                    state_w = state_w.max(state.width().min(STATE_ALIGN_MAX));
                }
            }
        }
        let room = width.saturating_sub(LABEL_WIDTH + 2 + state_w);
        want.min(room).max(want.min(1))
    }

    fn label_span(&self, label: &str) -> Span<'static> {
        Span::styled(format!("{label:<LABEL_WIDTH$}"), self.muted)
    }

    /// The card laid out in `width` × `height` cells.
    fn lines(&self, width: usize, height: usize) -> Vec<Line<'static>> {
        let files = self.column_width(Group::Files, width);
        let usb = self.column_width(Group::Usb, width);
        let value_room = width.saturating_sub(LABEL_WIDTH);
        let shown = if self.rows.len() > height {
            height.saturating_sub(1)
        } else {
            self.rows.len()
        };
        let mut lines: Vec<Line<'static>> = self.rows[..shown]
            .iter()
            .map(|row| {
                let line = match row {
                    CardRow::Text(line) => line.clone(),
                    CardRow::Field { label, values } => {
                        let value = values
                            .iter()
                            .find(|v| v.width() <= value_room)
                            .or(values.last())
                            .cloned()
                            .unwrap_or_default();
                        Line::from(vec![self.label_span(label), value])
                    }
                    CardRow::Column {
                        label,
                        group,
                        cell,
                        style,
                        state,
                    } => {
                        let col_w = match group {
                            Group::Files => files,
                            Group::Usb => usb,
                        };
                        Line::from(vec![
                            self.label_span(label),
                            Span::styled(cell.layout(col_w), *style),
                            Span::raw("  "),
                            state.clone(),
                        ])
                    }
                };
                truncate_line(line, width)
            })
            .collect();
        if shown < self.rows.len() {
            let more = format!("… {} more — enlarge the terminal", self.rows.len() - shown);
            lines.push(truncate_line(Line::styled(more, self.muted), width));
        }
        lines
    }
}

fn normal(text: impl Into<String>, theme: &Theme) -> Span<'static> {
    Span::styled(text.into(), theme.normal)
}

fn pending(theme: &Theme) -> Span<'static> {
    Span::styled(PENDING, theme.help)
}

/// A timestamp as local `YYYY-MM-DD HH:MM`; `—` for the zero value a
/// hand-written YAML without the field comes out as.
fn local_time(t: DateTime<Utc>) -> String {
    if t == DateTime::<Utc>::default() {
        "—".to_string()
    } else {
        t.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string()
    }
}

/// How an image file is doing on the host, the way the Go screens put it:
/// `✗ not found`, `✗ no access`, `✗ <error>`, `● <size>` or `● present`.
fn image_state_span(s: &ImageState, theme: &Theme) -> Span<'static> {
    match &s.err {
        Some(e) if e.contains("not found") => Span::styled("✗ not found", theme.error),
        Some(e) if e.contains("no read access") => Span::styled("✗ no access", theme.error),
        Some(e) => Span::styled(format!("✗ {e}"), theme.error),
        None if s.size > 0 => Span::styled(format!("● {}", human_size(s.size)), theme.success),
        None => Span::styled("● present", theme.success),
    }
}

/// An additional disk's image: `● <size> on host` when it is there, the
/// image state otherwise.
fn disk_state_span(s: &ImageState, theme: &Theme) -> Span<'static> {
    if s.ok() {
        Span::styled(format!("● {} on host", human_size(s.size)), theme.success)
    } else {
        image_state_span(s, theme)
    }
}

/// Whether a passed-through device is connected to the host right now and
/// openable by QEMU.
fn usb_state_span(s: &UsbState, theme: &Theme) -> Span<'static> {
    match &s.host {
        Some(h) if !h.writable => Span::styled("✗ no access", theme.error),
        Some(_) => Span::styled("● connected", theme.success),
        None => Span::styled("○ not connected", theme.help),
    }
}

/// The `Net` value: the type, then the port forwards as `host→guest`.
fn net_info(cfg: &VmConfig) -> String {
    let mut s = cfg.network.kind.to_string();
    if !cfg.network.port_forwards.is_empty() {
        let fwds: Vec<String> = cfg
            .network
            .port_forwards
            .iter()
            .map(|pf| format!("{}→{}", pf.host, pf.guest))
            .collect();
        s.push_str(&format!(" [{}]", fwds.join(", ")));
    }
    s
}

/// The `IP` value in its wordings, longest first. `—` while stopped and for
/// a VM without a network. With user networking the guest always has the
/// fixed SLIRP lease, only reachable from the host through port forwards,
/// so the first TCP forward to port 22 is turned into a ready `ssh` command;
/// a narrow card drops the gateway, then the lease note, but keeps that
/// command. With tap networking the address comes from the DHCP leases or
/// the ARP table and may take a while.
fn ip_info(
    cfg: &VmConfig,
    status: ProcessInfo,
    details: Option<&Details>,
    theme: &Theme,
) -> Vec<Span<'static>> {
    if !status.running() {
        return vec![normal("—", theme)];
    }
    match cfg.network.kind {
        NetworkType::User => {
            let Some(d) = details else {
                return vec![pending(theme)];
            };
            let ip = d
                .guest_ip
                .clone()
                .unwrap_or_else(|| USER_NET_GUEST_IP.to_string());
            let ssh = cfg
                .network
                .port_forwards
                .iter()
                .find(|pf| pf.guest == 22 && pf.proto() == "tcp")
                .map(|pf| format!("  —  ssh -p {} localhost", pf.host))
                .unwrap_or_default();
            vec![
                normal(
                    format!("{ip} (DHCP, guest-internal)  gw {USER_NET_GATEWAY}{ssh}"),
                    theme,
                ),
                normal(format!("{ip} (DHCP, guest-internal){ssh}"), theme),
                normal(format!("{ip}{ssh}"), theme),
            ]
        }
        NetworkType::Tap => {
            let Some(d) = details else {
                return vec![pending(theme)];
            };
            match d.guest_ip.as_deref() {
                Some(ip) if !ip.is_empty() => vec![normal(ip, theme)],
                _ => vec![
                    Span::styled(
                        format!("(not seen yet — looking for {})", cfg.network.mac),
                        theme.help,
                    ),
                    Span::styled("(not seen yet)", theme.help),
                ],
            }
        }
        NetworkType::None => vec![normal("—", theme)],
    }
}

/// The `VNC` value: `disabled`, or the listen address with the TCP port.
fn vnc_info(cfg: &VmConfig) -> String {
    if cfg.vnc_port > 0 {
        format!(
            "127.0.0.1:{}  (TCP port {})",
            cfg.vnc_port,
            5900 + cfg.vnc_port
        )
    } else {
        "disabled".to_string()
    }
}

/// The details card for the selected VM: the pane title (the VM name) and
/// its rows, label column 10 wide. Rows: Status (● running (PID n) /
/// ● stopped), CPU, RAM, Disk (main size, then one line per additional disk
/// with its size and host state), ISO (path + state or (none)), Firmware,
/// Net (type [host→guest, …]), IP (user: `10.0.2.15 (DHCP, guest-internal)
/// gw 10.0.2.2  —  ssh -p N localhost` when a forward to 22 exists; tap: the
/// address or `(not seen yet — looking for <mac>)`; `—` when stopped), VNC,
/// USB (one line per device with ● connected / ✗ no access / ○ not
/// connected), USB ISO (one line per image with its state), Created. With
/// no VM: title `Ostrich` and a one-line hint. The disk, ISO and USB image
/// rows line their states up, as do the USB device rows.
///
/// Columns that come from the 2 s refresh (image and device states, the
/// guest IP) show `…` until the refresh for this VM has landed; the card
/// never touches the filesystem itself.
pub fn vm_details(app: &App) -> (Line<'static>, Card) {
    let theme = &app.theme;
    let mut card = Card::new(theme);
    let Some(entry) = app.selected_vm() else {
        if app.loading {
            card.text(Line::styled("Loading…", theme.help));
        } else {
            card.text(Line::styled("No VMs yet.", theme.normal));
            card.text(Line::styled(
                "Press n to create one, or T for templates.",
                theme.help,
            ));
        }
        return (Line::from(" Ostrich "), card);
    };
    let cfg = &entry.cfg;
    let details = app.details.as_ref().filter(|d| d.name == cfg.name);
    // The list's status is the fallback: it is a refresh older at most.
    let status = details.map_or(entry.status, |d| d.status);

    let status_span = if status.running() {
        Span::styled(format!("● running  (PID {})", status.pid), theme.running)
    } else {
        Span::styled("● stopped", theme.stopped)
    };
    card.field("Status", status_span);
    card.field("CPU", normal(plural(cfg.cpu, "core", "cores"), theme));
    card.field("RAM", normal(format!("{} MiB", cfg.ram), theme));

    // The main disk, then each additional disk on its own line: its name,
    // size and whether its image is still there on the host.
    card.field("Disk", normal(format!("{} GiB", cfg.disk_size), theme));
    let disk_states = details
        .map(|d| &d.disks)
        .filter(|s| s.len() == cfg.disks.len());
    for (i, d) in cfg.disks.iter().enumerate() {
        let state = disk_states.map_or_else(|| pending(theme), |s| disk_state_span(&s[i], theme));
        let cell = Cell::Name {
            name: d.name.clone(),
            name_w: DISK_NAME_WIDTH,
            tail: format!("  {:>3} GiB", d.size),
        };
        card.column("", Group::Files, cell, theme.normal, state);
    }

    // The boot ISO with whether the file is there on the host.
    if cfg.cdrom_path.is_empty() {
        card.field("ISO", normal("(none)", theme));
    } else {
        let state = details
            .map(|d| &d.cdrom)
            .filter(|s| s.path == cfg.cdrom_path)
            .map_or_else(|| pending(theme), |s| image_state_span(s, theme));
        let cell = Cell::Path(cfg.cdrom_path.clone());
        card.column("ISO", Group::Files, cell, theme.normal, state);
    }

    card.field("Firmware", normal(cfg.firmware_label(), theme));
    card.field("Net", normal(net_info(cfg), theme));
    card.fields("IP", ip_info(cfg, status, details, theme));
    card.field("VNC", normal(vnc_info(cfg), theme));

    // The passed-through devices, one per line, with whether each is
    // connected to the host right now and openable by QEMU.
    if cfg.usb_devices.is_empty() {
        card.field("USB", normal("(none)", theme));
    } else {
        let states = details
            .map(|d| &d.usb)
            .filter(|s| s.len() == cfg.usb_devices.len());
        let name_w = cfg
            .usb_devices
            .iter()
            .map(|dev| dev.label().width())
            .max()
            .unwrap_or(0)
            .min(USB_NAME_WIDTH);
        for (i, dev) in cfg.usb_devices.iter().enumerate() {
            let state = states.map_or_else(|| pending(theme), |s| usb_state_span(&s[i], theme));
            let cell = Cell::Name {
                name: dev.label(),
                name_w,
                tail: format!("  {}", dev.id()),
            };
            let label = if i == 0 { "USB" } else { "" };
            card.column(label, Group::Usb, cell, theme.normal, state);
        }
    }

    // The images attached as USB drives, one per line, with whether each
    // file is still there on the host; the path keeps its end visible.
    if cfg.usb_images.is_empty() {
        card.field("USB ISO", normal("(none)", theme));
    } else {
        let states = details
            .map(|d| &d.images)
            .filter(|s| s.len() == cfg.usb_images.len());
        for (i, img) in cfg.usb_images.iter().enumerate() {
            let state = states.map_or_else(|| pending(theme), |s| image_state_span(&s[i], theme));
            let label = if i == 0 { "USB ISO" } else { "" };
            let cell = Cell::Path(img.path.clone());
            card.column(label, Group::Files, cell, theme.normal, state);
        }
    }

    card.field("Created", normal(local_time(cfg.created_at), theme));
    (Line::from(format!(" {} ", cfg.name)), card)
}

/// The details card for the selected template, like the Go templates
/// screen's box: Template, About, From VM (+ saved date, local time
/// `YYYY-MM-DD HH:MM`), Disk (virtual + on host), Firmware, Defaults, VNC.
pub fn template_details(app: &App) -> (Line<'static>, Card) {
    let theme = &app.theme;
    let mut card = Card::new(theme);
    let Some(entry) = app.selected_template() else {
        card.text(Line::styled(
            "No templates yet — t saves a stopped VM as one.",
            theme.help,
        ));
        return (Line::from(" Templates "), card);
    };
    let t = &entry.tpl;
    let on_disk = if entry.disk_usage > 0 {
        human_size(entry.disk_usage)
    } else {
        "unknown".to_string()
    };
    card.field("Template", normal(t.name.clone(), theme));
    card.field(
        "About",
        normal(if_empty(&t.description, "(no description)"), theme),
    );
    card.field(
        "From VM",
        normal(
            format!("{}, saved {}", t.source_vm, local_time(t.created_at)),
            theme,
        ),
    );
    card.field(
        "Disk",
        normal(
            format!("{} GiB virtual, {on_disk} on the host", t.disk_size),
            theme,
        ),
    );
    card.field("Firmware", normal(t.firmware_label(), theme));
    let defaults = format!(
        "{}, {} MiB RAM, {} network",
        plural(t.cpu, "core", "cores"),
        t.ram,
        t.network
    );
    card.fields(
        "Defaults",
        vec![
            normal(format!("{defaults} — chosen anew for each VM"), theme),
            normal(defaults, theme),
        ],
    );
    if t.vnc {
        card.fields(
            "VNC",
            vec![
                normal("enabled — a new VM gets a free display number", theme),
                normal("enabled", theme),
            ],
        );
    } else {
        card.field("VNC", normal("disabled", theme));
    }
    (Line::from(" Template "), card)
}

/// Draws a details card: a rounded bordered block titled `title` holding
/// the card's rows laid out for the block, border styled focused or not.
pub fn render_details(
    app: &App,
    frame: &mut Frame,
    area: Rect,
    title: Line<'static>,
    card: Card,
    focused: bool,
) {
    let theme = &app.theme;
    let (border, title_style) = if focused {
        (theme.border_focused, theme.title)
    } else {
        (theme.border, theme.help)
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(border)
        .title(title.style(title_style));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let body = inner.inner(Margin::new(1, 0));
    if body.width == 0 || body.height == 0 {
        return;
    }
    let lines = card.lines(body.width as usize, body.height as usize);
    frame.render_widget(Paragraph::new(lines), body);
}

/// The console pane's title: ` Serial console · <vm> · last 200 lines · 2s `
/// (just ` Serial console ` without a VM), whatever the pane's width; see
/// [`console_title_fit`] for one that fits the pane.
pub fn console_title(app: &App) -> Line<'static> {
    console_title_fit(app, u16::MAX)
}

/// The console pane's title for a pane `width` cells wide, borders
/// included: ` Serial console · <vm> · last 200 lines · 2s `, which sheds
/// ` · 2s`, then ` · last 200 lines`, then cuts the VM name with an
/// ellipsis when the pane is narrow, down to ` Serial console `, itself cut
/// with an ellipsis in a pane narrower than that (none at all below 5 cells).
pub fn console_title_fit(app: &App, width: u16) -> Line<'static> {
    const BARE: &str = " Serial console ";
    let room = usize::from(width).saturating_sub(2);
    let bare = || {
        if BARE.width() <= room {
            Line::from(BARE)
        } else if room >= 3 {
            Line::from(format!(" {} ", fit_words(BARE.trim(), room - 2)))
        } else {
            Line::default()
        }
    };
    let Some(v) = app.selected_vm() else {
        return bare();
    };
    let name = &v.cfg.name;
    let wordings = [
        format!(
            " Serial console · {name} · last {CONSOLE_TAIL_LINES} lines · {CONSOLE_POLL_SECONDS}s "
        ),
        format!(" Serial console · {name} · last {CONSOLE_TAIL_LINES} lines "),
        format!(" Serial console · {name} "),
    ];
    if let Some(title) = wordings.into_iter().find(|t| t.width() <= room) {
        return Line::from(title);
    }
    let name_room = room.saturating_sub(" Serial console ·  ".width());
    if name_room >= 4 {
        Line::from(format!(" Serial console · {} ", ellipsize(name, name_room)))
    } else {
        bare()
    }
}

// ---------------------------------------------------------------------------
// Bars
// ---------------------------------------------------------------------------

/// The status bar: the notice on the left (`✓ text` in the success style,
/// `✗ first line` in the error style), the spinner and label of the
/// dashboard's own action in flight on the right. A panel's work shows in
/// the panel, so it gets no second spinner here.
pub fn render_status_bar(app: &App, frame: &mut Frame, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let theme = &app.theme;
    let width = area.width as usize;

    let busy = app.busy.as_ref().map(|label| {
        Line::from(vec![
            Span::styled(spinner_frame(app.tick), theme.spinner),
            Span::raw(" "),
            Span::styled(format!("{label} "), theme.help),
        ])
    });
    let busy_w = busy.as_ref().map_or(0, |l| l.width());
    // The notice yields to the busy label, with a gap between them.
    let notice_w = if busy_w > 0 {
        width.saturating_sub(busy_w + 2)
    } else {
        width
    };

    if let Some(notice) = &app.notice {
        let (text, style) = match notice {
            Notice::Ok(t) => (format!(" ✓ {t}"), theme.success),
            Notice::Err(t) => (
                format!(" ✗ {}", t.lines().next().unwrap_or("")),
                theme.error,
            ),
        };
        if notice_w > 0 {
            let line = Line::styled(truncate(&text, notice_w), style);
            frame.render_widget(
                Paragraph::new(line),
                Rect {
                    width: notice_w as u16,
                    ..area
                },
            );
        }
    }
    if let Some(line) = busy {
        frame.render_widget(
            Paragraph::new(truncate_line(line, width)).alignment(Alignment::Right),
            area,
        );
    }
}

/// The order the dashboard drops its key hints in when the bar is too
/// narrow for all of them, first to go first. `? keys` and `q quit` always
/// stay.
fn hint_drop_order(focus: Focus) -> &'static [&'static str] {
    match focus {
        Focus::Vms => &[
            "r", "Tab", "Enter", "v", "c", "t", "i", "u", "d", "x", "s", "e", "n", "j/k",
        ],
        Focus::Templates => &["Tab", "h", "d", "Enter/n", "j/k"],
        Focus::Console => &["r", "v", "c", "Ctrl-d/u", "g/G", "s/x", "h", "j/k"],
    }
}

/// The hints line that fits in `width` cells: whole hints are dropped, the
/// ones `drop_order` names first, in its order, then the others from the
/// right, but never a `pinned` one. A line still too wide is cut.
fn fit_hints(
    pairs: &[(&str, &str)],
    width: usize,
    drop_order: &[&str],
    pinned: impl Fn(&str) -> bool,
    theme: &Theme,
) -> Line<'static> {
    let mut order: Vec<usize> = drop_order
        .iter()
        .filter_map(|key| pairs.iter().position(|(k, _)| k == key))
        .collect();
    for i in (0..pairs.len()).rev() {
        if !order.contains(&i) {
            order.push(i);
        }
    }
    let mut keep = vec![true; pairs.len()];
    let line_of = |keep: &[bool]| {
        let kept: Vec<(&str, &str)> = pairs
            .iter()
            .zip(keep)
            .filter(|(_, k)| **k)
            .map(|(p, _)| *p)
            .collect();
        hints_line(&kept, theme)
    };
    for i in order {
        if line_of(&keep).width() <= width {
            break;
        }
        if !pinned(pairs[i].0) {
            keep[i] = false;
        }
    }
    truncate_line(line_of(&keep), width)
}

/// The bottom bar of key hints for the current context, fitted to the
/// width by dropping whole hints: on the dashboard in `hint_drop_order`,
/// keeping `? keys` and `q quit`; in a panel or popup from the right,
/// keeping the `Esc` hint, or `Ctrl-c quit` while a panel's work runs.
pub fn render_hints(app: &App, frame: &mut Frame, area: Rect) {
    if area.width < 2 || area.height == 0 {
        return;
    }
    let hints = app.key_hints();
    let pairs: Vec<(&str, &str)> = hints
        .iter()
        .map(|(k, d)| (k.as_str(), d.as_str()))
        .collect();
    let bar = Rect {
        x: area.x + 1,
        width: area.width - 1,
        ..area
    };
    let width = bar.width as usize;
    let line = if app.popup.is_none() && app.panel_title().is_none() {
        fit_hints(
            &pairs,
            width,
            hint_drop_order(app.focus),
            |k| k == "?" || k == "q",
            &app.theme,
        )
    } else {
        // The way out stays: Esc, or Ctrl-c while a panel's work runs.
        fit_hints(
            &pairs,
            width,
            &[],
            |k| k.contains("Esc") || k == "Ctrl-c",
            &app.theme,
        )
    };
    frame.render_widget(Paragraph::new(line), bar);
}

/// The dashboard's key hints for the focused pane (VMs: move, Tab, Enter,
/// s, x, n, e, u, i, t, c, v, d, r, ?, q; Templates: move, Enter new VM, d,
/// h back, …; Console: scroll keys, h back, the VM action keys). Without a
/// VM (or a template) a pane offers only the keys that do something there.
pub fn key_hints(app: &App) -> Vec<(String, String)> {
    let pairs: &[(&str, &str)] = match app.focus {
        // The VM keys need a VM; n makes the first one.
        Focus::Vms if app.vms.is_empty() => &[
            ("n", "new"),
            ("Tab", "pane"),
            ("r", "refresh"),
            ("?", "keys"),
            ("q", "quit"),
        ],
        Focus::Vms => &[
            ("j/k", "move"),
            ("Tab", "pane"),
            ("Enter", "console"),
            ("s", "start"),
            ("x", "stop"),
            ("n", "new"),
            ("e", "edit"),
            ("u", "USB"),
            ("i", "ISO"),
            ("t", "template"),
            ("c", "serial"),
            ("v", "VNC"),
            ("d", "delete"),
            ("r", "refresh"),
            ("?", "keys"),
            ("q", "quit"),
        ],
        // With none to pick, n (and Enter) open the plain create form.
        Focus::Templates if app.templates.is_empty() => &[
            ("Enter/n", "new VM"),
            ("h", "back"),
            ("Tab", "pane"),
            ("?", "keys"),
            ("q", "quit"),
        ],
        Focus::Templates => &[
            ("j/k", "move"),
            ("Enter/n", "new VM from template"),
            ("d", "delete"),
            ("h", "back"),
            ("Tab", "pane"),
            ("?", "keys"),
            ("q", "quit"),
        ],
        Focus::Console if app.vms.is_empty() => &[
            ("h", "back"),
            ("Tab", "pane"),
            ("r", "refresh"),
            ("?", "keys"),
            ("q", "quit"),
        ],
        Focus::Console => &[
            ("j/k", "scroll"),
            ("Ctrl-d/u", "half page"),
            ("g/G", "top/bottom"),
            ("h", "back"),
            ("s/x", "start/stop"),
            ("c", "serial"),
            ("v", "VNC"),
            ("r", "refresh"),
            ("?", "keys"),
            ("q", "quit"),
        ],
    };
    pairs
        .iter()
        .map(|(k, d)| (k.to_string(), d.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use ratatui::backend::TestBackend;

    use super::*;
    use crate::config::AppConfig;
    use crate::tui::testutil::*;
    use crate::tui::widgets::SPINNER_FRAMES;
    use crate::vm::{
        image_state_of, match_usb, usb_image_states, Disk, FirmwareType, HostUsbDevice,
        NetworkConfig, PortForward, Template, UsbDevice, UsbImage, VmStatus,
    };

    /// A dashboard over a scratch storage directory, as after the first load.
    fn test_app() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = AppConfig {
            vm_storage_path: dir.path().to_string_lossy().into_owned(),
            recent_isos: Vec::new(),
        };
        let mut app = App::new(cfg);
        app.loading = false;
        (dir, app)
    }

    fn draw(app: &mut App, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal.draw(|f| app.draw(f)).expect("draw");
        buffer_lines(terminal.backend().buffer())
    }

    fn entry(name: &str, cpu: u32, ram: u32, disk: u32, running: bool) -> VmEntry {
        let cfg = VmConfig {
            name: name.to_string(),
            cpu,
            ram,
            disk_size: disk,
            ..VmConfig::default()
        };
        let status = if running {
            ProcessInfo {
                pid: 4242,
                status: VmStatus::Running,
            }
        } else {
            ProcessInfo::default()
        };
        VmEntry { cfg, status }
    }

    fn template(name: &str, cpu: u32, ram: u32, disk: u32) -> TemplateEntry {
        let tpl = Template {
            name: name.to_string(),
            description: String::new(),
            source_vm: "src".to_string(),
            cpu,
            ram,
            disk_size: disk,
            arch: "x86_64".to_string(),
            firmware: FirmwareType::Bios,
            secure_boot: false,
            tpm: false,
            network: NetworkType::User,
            vnc: false,
            created_at: "2026-10-09T13:33:00Z".parse().unwrap(),
        };
        TemplateEntry { tpl, disk_usage: 0 }
    }

    fn write_image(path: &Path) {
        fs::write(path, vec![0u8; 1 << 20]).expect("image");
    }

    fn host_dev(vendor: &str, product: &str, writable: bool) -> HostUsbDevice {
        HostUsbDevice {
            vendor_id: vendor.to_string(),
            product_id: product.to_string(),
            manufacturer: String::new(),
            product: String::new(),
            bus: 3,
            dev: 7,
            port: "3-2".to_string(),
            dev_node: "/dev/bus/usb/003/007".to_string(),
            writable,
        }
    }

    fn line_with<'a>(lines: &'a [String], needle: &str) -> &'a str {
        lines
            .iter()
            .find(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("no line contains {needle:?}:\n{}", lines.join("\n")))
    }

    /// The column (in chars) of the last state glyph on a screen line.
    fn state_column(line: &str) -> usize {
        line.chars()
            .collect::<Vec<_>>()
            .iter()
            .rposition(|c| matches!(c, '●' | '✗' | '○'))
            .unwrap_or_else(|| panic!("no state in {line:?}"))
    }

    #[test]
    fn empty_dashboard_offers_create() {
        let (_dir, mut app) = test_app();
        let s = draw(&mut app, 140, 40);
        assert!(
            screen_contains(&s, " Virtual Machines (0) "),
            "{}",
            s.join("\n")
        );
        assert!(screen_contains(&s, "No VMs yet. Press n to create one."));
        assert!(screen_contains(&s, " Templates (0) "));
        assert!(
            screen_contains(&s, "No templates yet — t saves a stopped VM"),
            "{}",
            s.join("\n")
        );
        assert!(
            screen_contains(&s, "one."),
            "the text wraps inside the pane: {}",
            s.join("\n")
        );
        // Where the long text would be cut, a shorter one is whole.
        for (w, h) in [(60, 24), (50, 12)] {
            let s = draw(&mut app, w, h);
            let text = s.join("\n");
            let tpl_top = s
                .iter()
                .position(|l| l.contains(" Templates (0) "))
                .unwrap_or_else(|| panic!("{w}x{h}:\n{text}"));
            let pane: String = s[tpl_top + 1..]
                .iter()
                .take_while(|l| !l.contains('└') && !l.contains('╰'))
                .map(|l| l.split('│').nth(1).unwrap_or("").trim().to_string())
                .collect::<Vec<_>>()
                .join(" ");
            assert!(
                pane.starts_with("No templates yet") && pane.ends_with('.'),
                "{w}x{h}: {pane:?}\n{text}"
            );
        }
        // Its hints offer what n does there.
        app.focus = Focus::Templates;
        let hints = key_hints(&app);
        assert!(hints.contains(&("Enter/n".to_string(), "new VM".to_string())));
        assert!(
            !hints.iter().any(|(k, _)| k == "d" || k == "j/k"),
            "{hints:?}"
        );
        app.focus = Focus::Vms;
        assert!(screen_contains(&s, " Ostrich "));
        assert!(screen_contains(&s, "No VMs yet."));
        assert!(screen_contains(
            &s,
            "Press n to create one, or T for templates."
        ));
        assert!(screen_contains(&s, " Serial console "));
        assert!(!screen_contains(&s, "Serial console ·"));
    }

    #[test]
    fn loading_state_before_the_first_load() {
        let (_dir, mut app) = test_app();
        app.loading = true;
        let s = draw(&mut app, 140, 40);
        assert!(screen_contains(&s, " Virtual Machines "));
        assert!(!screen_contains(&s, "Virtual Machines (0)"));
        let text = s.join("\n");
        assert_eq!(
            text.matches("Loading…").count(),
            3,
            "both lists and the card: {text}"
        );
        assert!(!screen_contains(&s, "No VMs yet"));
        assert!(screen_contains(&s, " Ostrich "));
    }

    #[test]
    fn list_error_replaces_the_rows() {
        let (_dir, mut app) = test_app();
        app.templates.push(template("base", 1, 512, 8));
        app.vm_list_err = Some("open /vms: permission denied".to_string());
        let s = draw(&mut app, 140, 40);
        assert!(
            screen_contains(&s, "✗ open /vms: permission denied"),
            "{}",
            s.join("\n")
        );
        assert!(
            screen_contains(&s, "▸ base  BIOS"),
            "the templates still show: {}",
            s.join("\n")
        );
    }

    #[test]
    fn a_templates_error_does_not_hide_the_vms() {
        let (_dir, mut app) = test_app();
        app.vms.push(entry("debian-12", 2, 2048, 20, false));
        app.tpl_list_err = Some("read /vms/.templates: permission denied".to_string());
        for (w, h) in [(80, 24), (120, 40), (160, 45)] {
            let s = draw(&mut app, w, h);
            let text = s.join("\n");
            let tpl_top = s
                .iter()
                .position(|l| l.contains(" Templates (0) "))
                .unwrap_or_else(|| panic!("{w}x{h}:\n{text}"));
            let vm_pane = s[..tpl_top].join("\n");
            assert!(vm_pane.contains("▸ debian-12"), "{w}x{h}:\n{text}");
            assert!(!vm_pane.contains('✗'), "{w}x{h}:\n{text}");
            // The error shows in the templates pane, wrapped to it.
            let tpl_pane = s[tpl_top..].join("\n");
            assert!(
                tpl_pane.contains("│ ✗ read /vms/.templates:"),
                "{w}x{h}:\n{text}"
            );
            assert!(tpl_pane.contains("denied"), "{w}x{h}:\n{text}");
            assert!(!tpl_pane.contains("No templates yet"), "{w}x{h}:\n{text}");
        }
    }

    #[test]
    fn vm_rows_show_status_and_names() {
        let (_dir, mut app) = test_app();
        app.vms.push(entry("debian-12", 2, 2048, 20, true));
        let mut win = entry("windows-11", 4, 8192, 64, false);
        win.cfg.disks = vec![
            Disk {
                name: "data".into(),
                size: 50,
            },
            Disk {
                name: "scratch".into(),
                size: 10,
            },
        ];
        app.vms.push(win);
        app.vms.push(entry(
            "a-very-long-vm-name-exceeding-twenty",
            1,
            512,
            8,
            false,
        ));
        let s = draw(&mut app, 140, 40);
        assert!(
            screen_contains(&s, " Virtual Machines (3) "),
            "{}",
            s.join("\n")
        );
        let row = line_with(&s, "▸ debian-12");
        assert!(row.contains("● running"), "{row}");
        let row = line_with(&s, "  windows-11");
        assert!(row.contains("● stopped"), "{row}");
        // The long name is cut to the 20-cell column instead of pushing the row apart.
        assert!(
            screen_contains(&s, "a-very-long-vm-name…"),
            "{}",
            s.join("\n")
        );
        assert!(!screen_contains(&s, "exceeding-twenty"));
        // Nothing runs into the pane's right border.
        for l in s.iter().filter(|l| l.contains("● ")) {
            assert_eq!(l.chars().nth(46), Some('│'), "{l:?}");
        }
        assert_eq!(
            app.vm_rows, 32,
            "38 rows of main area minus a 4-row templates pane minus borders"
        );
    }

    #[test]
    fn vm_rows_add_the_summary_when_it_fits() {
        let (_dir, mut app) = test_app();
        app.vms.push(entry("debian-12", 2, 2048, 20, true));
        let mut win = entry("win11", 4, 512, 8, false);
        win.cfg.disks = vec![
            Disk {
                name: "data".into(),
                size: 50,
            },
            Disk {
                name: "scratch".into(),
                size: 10,
            },
        ];
        app.vms.push(win);

        // 80 columns: the left column is 30 wide, name and status only.
        let s = draw(&mut app, 80, 30);
        let row = line_with(&s, "▸ debian-12");
        assert!(row.contains("● running"), "{row:?}");
        assert!(!row.contains("CPU"), "{row:?}");

        // 170 columns: the left column is at its widest (54), which fits the
        // short summary next to short names, a cell clear of the border.
        let s = draw(&mut app, 170, 40);
        let row = line_with(&s, "▸ debian-12");
        assert!(
            row.contains("● running  2 CPU · 2048 MiB · 20 GiB"),
            "{row:?}"
        );
        let row = line_with(&s, "  win11");
        assert!(
            row.contains("● stopped  4 CPU · 512 MiB · 8 GiB +2 │"),
            "{row:?}"
        );
        // The highlight and the rows start a cell in from the border.
        assert!(s[1].starts_with("│ ▸ debian-12"), "{:?}", s[1]);

        // A pane wide enough gets the Go list's wording.
        let mut terminal = Terminal::new(TestBackend::new(90, 10)).unwrap();
        terminal
            .draw(|f| {
                render_vm_list(&mut app, f, f.area());
            })
            .unwrap();
        let s = buffer_lines(terminal.backend().buffer());
        let row = line_with(&s, "▸ debian-12");
        assert!(
            row.contains("● running   CPU: 2  RAM: 2048 MiB  Disk: 20 GiB"),
            "{row:?}"
        );
        assert!(!row.contains("+"), "{row:?}");
        let row = line_with(&s, "  win11");
        assert!(
            row.contains("● stopped   CPU: 4  RAM: 512 MiB  Disk: 8 GiB +2"),
            "{row:?}"
        );

        // The summary is dropped rather than cut when the name takes the room.
        app.vms[0].cfg.name = "a-name-of-twenty-ch".into();
        let s = draw(&mut app, 170, 40);
        let row = line_with(&s, "▸ a-name-of-twenty-ch");
        assert!(row.contains("● running"), "{row:?}");
        assert!(!row.contains("CPU"), "{row:?}");
        assert_eq!(summary_tier(&app.vms, 0), Summary::None);
        assert_eq!(summary_tier(&app.vms, 100), Summary::Long);

        // One tier for the whole list: a row whose summary would not fit
        // takes the summary off its neighbours too, so the column lines up.
        app.vms[0].cfg.name = "debian-12".into();
        app.vms[1].cfg.ram = 16384;
        app.vms[1].cfg.disk_size = 512;
        let s = draw(&mut app, 170, 40);
        let no_summary = |l: &str| !l.contains("CPU:") && !l.contains(" CPU ·");
        assert!(no_summary(line_with(&s, "▸ debian-12")), "{:?}", s[1]);
        assert!(no_summary(line_with(&s, "  win11")), "{:?}", s[2]);
    }

    #[test]
    fn cursor_row_highlight_follows_the_focus() {
        let (_dir, mut app) = test_app();
        app.vms.push(entry("debian-12", 2, 2048, 20, false));
        app.vms.push(entry("ubuntu-24", 2, 2048, 20, false));
        app.templates.push(template("base", 1, 512, 8));
        let primary = app.theme.primary;
        let unfocused_bg = app.theme.selected_unfocused.bg;

        let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        // Row 1 is the first list row (row 0 is the border); x=1 is the
        // margin cell, x=2 the marker.
        assert_eq!(buf[(2, 1)].symbol(), "▸");
        assert_eq!(
            buf[(1, 1)].style().bg,
            Some(primary),
            "focused pane: cursor row in the selected style"
        );
        assert_eq!(buf[(2, 1)].style().bg, Some(primary));
        assert_eq!(buf[(2, 2)].symbol(), " ");
        assert_ne!(
            buf[(2, 2)].style().bg,
            Some(primary),
            "other rows are plain"
        );
        // The highlight runs to the pane edge, not just under the text.
        assert_eq!(buf[(45, 1)].style().bg, Some(primary));
        assert_eq!(buf[(46, 1)].symbol(), "│");
        // The focused pane's title and border use the focused styles, the other pane's are dim.
        assert_eq!(buf[(2, 0)].style().fg, Some(primary));
        assert_eq!(buf[(0, 0)].style().fg, Some(primary));
        let muted = app.theme.muted;
        let tpl_top = 40 - 2 - 4; // two bars, a 4-row templates pane
        assert_eq!(buf[(0, tpl_top)].style().fg, Some(muted));
        assert_eq!(buf[(2, tpl_top)].style().fg, Some(muted));

        app.focus = Focus::Templates;
        terminal.draw(|f| app.draw(f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        assert_eq!(
            buf[(1, 1)].style().bg,
            unfocused_bg,
            "unfocused pane: faint cursor row"
        );
        assert_eq!(buf[(0, 0)].style().fg, Some(muted));
        assert_eq!(buf[(0, tpl_top)].style().fg, Some(primary));
        assert_eq!(buf[(2, tpl_top + 1)].symbol(), "▸");
        assert_eq!(buf[(1, tpl_top + 1)].style().bg, Some(primary));
    }

    #[test]
    fn vm_list_scrolls_to_keep_the_cursor_visible() {
        let (_dir, mut app) = test_app();
        for i in 0..60 {
            app.vms.push(entry(&format!("vm-{i:02}"), 1, 512, 8, false));
        }
        app.vm_cursor.index = 59;
        let s = draw(&mut app, 140, 40);
        assert!(screen_contains(&s, "▸ vm-59"), "{}", s.join("\n"));
        assert!(!screen_contains(&s, "vm-00"));
        assert_eq!(app.vm_rows, 32);
        app.vm_cursor.index = 0;
        let s = draw(&mut app, 140, 40);
        assert!(screen_contains(&s, "▸ vm-00"));
        assert!(!screen_contains(&s, "vm-59"));
    }

    #[test]
    fn vm_details_card_shows_every_row() {
        let (dir, mut app) = test_app();
        let isos = dir.path().join("isos");
        fs::create_dir_all(&isos).unwrap();
        let disc = isos.join("debian.iso");
        write_image(&disc);
        let present = isos.join("virtio-win.iso");
        write_image(&present);
        let gone = isos.join("gone.iso");
        let storage = dir.path().join("t");
        fs::create_dir_all(&storage).unwrap();
        write_image(&dir.path().join("t").join("data.qcow2"));

        let cfg = VmConfig {
            name: "t".into(),
            cpu: 4,
            ram: 8192,
            disk_size: 64,
            cdrom_path: disc.to_string_lossy().into_owned(),
            firmware: FirmwareType::Uefi,
            secure_boot: true,
            tpm: true,
            network: NetworkConfig {
                kind: NetworkType::User,
                mac: "52:54:00:00:00:03".into(),
                port_forwards: vec![
                    PortForward {
                        host: 2222,
                        guest: 22,
                        proto: "tcp".into(),
                    },
                    PortForward {
                        host: 5353,
                        guest: 53,
                        proto: "udp".into(),
                    },
                ],
            },
            vnc_port: 2,
            usb_devices: vec![
                UsbDevice {
                    vendor_id: "046d".into(),
                    product_id: "085c".into(),
                    name: "C922 Pro Stream Webcam".into(),
                    port: String::new(),
                },
                UsbDevice {
                    vendor_id: "dead".into(),
                    product_id: "beef".into(),
                    name: String::new(),
                    port: String::new(),
                },
                UsbDevice {
                    vendor_id: "0781".into(),
                    product_id: "5583".into(),
                    name: String::new(),
                    port: String::new(),
                },
            ],
            usb_images: vec![
                UsbImage {
                    path: present.to_string_lossy().into_owned(),
                },
                UsbImage {
                    path: gone.to_string_lossy().into_owned(),
                },
            ],
            disks: vec![
                Disk {
                    name: "data".into(),
                    size: 50,
                },
                Disk {
                    name: "gone".into(),
                    size: 5,
                },
            ],
            created_at: "2026-03-17T09:00:00Z".parse().unwrap(),
            ..VmConfig::default()
        };
        let host = vec![
            host_dev("046d", "085c", true),
            host_dev("0781", "5583", false),
        ];
        let details = Details {
            name: "t".into(),
            status: ProcessInfo {
                pid: 4242,
                status: VmStatus::Running,
            },
            guest_ip: Some("10.0.2.15".into()),
            usb: match_usb(&cfg.usb_devices, &host),
            cdrom: image_state_of(&cfg.cdrom_path),
            images: usb_image_states(&cfg.usb_images),
            disks: crate::vm::disk_states(dir.path(), &cfg),
            console: vec!["login:".into()],
        };
        app.vms.push(VmEntry {
            cfg,
            status: ProcessInfo {
                pid: 4242,
                status: VmStatus::Running,
            },
        });
        app.details = Some(details);

        let s = draw(&mut app, 170, 45);
        let text = s.join("\n");
        for want in [
            " t ",
            "Status    ● running  (PID 4242)",
            "CPU       4 cores",
            "RAM       8192 MiB",
            "Disk      64 GiB",
            "50 GiB  ● 1 MiB on host",
            " 5 GiB  ✗ not found",
            "● 1 MiB",
            "Firmware  UEFI + Secure Boot, TPM 2.0",
            "Net       user [2222→22, 5353→53]",
            "IP        10.0.2.15 (DHCP, guest-internal)  gw 10.0.2.2  —  ssh -p 2222 localhost",
            "VNC       127.0.0.1:2  (TCP port 5902)",
            "USB       C922 Pro Stream Webcam  046d:085c  ● connected",
            "          dead:beef               dead:beef  ○ not connected",
            "          0781:5583               0781:5583  ✗ no access",
            "USB ISO   ",
            "virtio-win.iso",
            "gone.iso",
            "✗ not found",
            "Created   ",
            "Serial console · t · last 200 lines · 2s",
        ] {
            assert!(
                text.contains(want),
                "details card missing {want:?}:\n{text}"
            );
        }
        let iso = line_with(&s, "ISO       ");
        assert!(iso.contains("debian.iso      ● 1 MiB"), "{iso}");
        // The disk, ISO and USB image rows put their states in one column
        // (the longest path's), as do the USB device rows (the longest label's).
        let files: Vec<usize> = ["data  ", "gone  ", "ISO  ", "virtio-win.iso", "gone.iso"]
            .iter()
            .map(|n| state_column(line_with(&s, n)))
            .collect();
        assert!(files.iter().all(|c| *c == files[0]), "{files:?}\n{text}");
        let usb: Vec<usize> = ["046d:085c", "dead:beef  ○", "0781:5583  ✗"]
            .iter()
            .map(|n| state_column(line_with(&s, n)))
            .collect();
        assert!(usb.iter().all(|c| *c == usb[0]), "{usb:?}\n{text}");
        let created = line_with(&s, "Created   ");
        assert!(
            created.contains(&local_time("2026-03-17T09:00:00Z".parse().unwrap())),
            "{created}"
        );
        assert!(
            !text.contains(PENDING),
            "nothing is pending once the details are in:\n{text}"
        );
    }

    #[test]
    fn details_pending_until_the_refresh_lands() {
        let (dir, mut app) = test_app();
        let mut v = entry("t", 1, 128, 8, true);
        v.cfg.cdrom_path = dir.path().join("x.iso").to_string_lossy().into_owned();
        v.cfg.usb_devices = vec![UsbDevice {
            vendor_id: "dead".into(),
            product_id: "beef".into(),
            name: String::new(),
            port: String::new(),
        }];
        v.cfg.usb_images = vec![UsbImage {
            path: "/iso/a.iso".into(),
        }];
        v.cfg.disks = vec![Disk {
            name: "data".into(),
            size: 1,
        }];
        app.vms.push(v);
        let s = draw(&mut app, 140, 40);
        // The list's status fills in while the refresh is out.
        assert!(
            screen_contains(&s, "Status    ● running  (PID 4242)"),
            "{}",
            s.join("\n")
        );
        for label in [
            "IP        …",
            "ISO       ",
            "USB       dead:beef",
            "USB ISO   /iso/a.iso",
            "data                    1 GiB  …",
        ] {
            let line = line_with(&s, label);
            assert!(
                line.contains(PENDING),
                "{label:?} should be pending: {line}"
            );
        }
        assert!(!screen_contains(&s, "○ not connected"));
        assert!(!screen_contains(&s, "✗ not found"));

        // Details for another VM do not count.
        app.details = Some(Details {
            name: "other".into(),
            ..Details::default()
        });
        let s = draw(&mut app, 140, 40);
        assert!(screen_contains(&s, "IP        …"));

        // A stopped VM has no IP, pending or not.
        app.vms[0].status = ProcessInfo::default();
        app.details = None;
        let s = draw(&mut app, 140, 40);
        assert!(screen_contains(&s, "Status    ● stopped"));
        assert!(screen_contains(&s, "IP        —"), "{}", s.join("\n"));
    }

    #[test]
    fn ip_wording_per_network_type() {
        let (_dir, mut app) = test_app();
        let mut v = entry("tapvm", 1, 128, 8, true);
        v.cfg.network = NetworkConfig {
            kind: NetworkType::Tap,
            mac: "52:54:00:00:00:03".into(),
            port_forwards: vec![],
        };
        app.vms.push(v);
        let running = ProcessInfo {
            pid: 7,
            status: VmStatus::Running,
        };
        app.details = Some(Details {
            name: "tapvm".into(),
            status: running,
            guest_ip: None,
            ..Details::default()
        });
        let s = draw(&mut app, 140, 40);
        assert!(
            screen_contains(
                &s,
                "IP        (not seen yet — looking for 52:54:00:00:00:03)"
            ),
            "{}",
            s.join("\n")
        );
        assert!(screen_contains(&s, "Net       tap"));

        app.details = Some(Details {
            name: "tapvm".into(),
            status: running,
            guest_ip: Some("192.168.1.42".into()),
            ..Details::default()
        });
        let s = draw(&mut app, 140, 40);
        assert!(screen_contains(&s, "IP        192.168.1.42"));

        // User networking without a forward to 22: no ssh hint.
        app.vms[0].cfg.network = NetworkConfig {
            kind: NetworkType::User,
            ..NetworkConfig::default()
        };
        app.details = Some(Details {
            name: "tapvm".into(),
            status: running,
            guest_ip: Some("10.0.2.15".into()),
            ..Details::default()
        });
        let s = draw(&mut app, 140, 40);
        let ip = line_with(&s, "IP        ");
        assert!(
            ip.contains("10.0.2.15 (DHCP, guest-internal)  gw 10.0.2.2"),
            "{ip}"
        );
        assert!(!ip.contains("ssh"), "{ip}");

        // No network: a dash even while running.
        app.vms[0].cfg.network = NetworkConfig {
            kind: NetworkType::None,
            ..NetworkConfig::default()
        };
        let s = draw(&mut app, 140, 40);
        assert!(screen_contains(&s, "IP        —"));
        assert!(screen_contains(&s, "Net       none"));
        assert!(screen_contains(&s, "ISO       (none)"));
        assert!(screen_contains(&s, "USB       (none)"));
        assert!(screen_contains(&s, "USB ISO   (none)"));
        assert!(screen_contains(&s, "VNC       disabled"));
        assert!(screen_contains(&s, "Firmware  BIOS"));
        assert!(
            screen_contains(&s, "Created   —"),
            "the zero timestamp is a dash"
        );
    }

    #[test]
    fn template_rows_and_card() {
        let (_dir, mut app) = test_app();
        let mut alpha = template("alpha", 1, 512, 8);
        alpha.tpl.description = "first".into();
        alpha.tpl.source_vm = "a".into();
        alpha.tpl.vnc = true;
        alpha.disk_usage = 196 * 1024;
        let mut beta = template("beta", 4, 4096, 32);
        beta.tpl.source_vm = "b".into();
        beta.tpl.firmware = FirmwareType::Uefi;
        beta.tpl.secure_boot = true;
        beta.tpl.tpm = true;
        beta.tpl.network = NetworkType::Tap;
        app.templates = vec![alpha, beta];
        app.focus = Focus::Templates;
        app.tpl_cursor.index = 1;

        let s = draw(&mut app, 140, 40);
        let text = s.join("\n");
        for want in [
            " Templates (2) ",
            "  alpha  BIOS",
            "▸ beta   UEFI + Secure Boot, TPM 2.0",
            " Template ",
            "Template  beta",
            "About     (no description)",
            "From VM   b, saved ",
            "Disk      32 GiB virtual, unknown on the host",
            "Firmware  UEFI + Secure Boot, TPM 2.0",
            "Defaults  4 cores, 4096 MiB RAM, tap network — chosen anew for each VM",
            "VNC       disabled",
        ] {
            assert!(text.contains(want), "templates missing {want:?}:\n{text}");
        }
        let saved = line_with(&s, "From VM   ");
        assert!(
            saved.contains(&local_time("2026-10-09T13:33:00Z".parse().unwrap())),
            "{saved}"
        );
        assert_eq!(app.tpl_rows, 2, "the templates pane grows to its rows");

        app.tpl_cursor.index = 0;
        let s = draw(&mut app, 140, 40);
        assert!(screen_contains(&s, "About     first"), "{}", s.join("\n"));
        assert!(screen_contains(&s, "From VM   a, saved "));
        assert!(screen_contains(
            &s,
            "Disk      8 GiB virtual, 196 KiB on the host"
        ));
        assert!(screen_contains(
            &s,
            "VNC       enabled — a new VM gets a free display number"
        ));
        assert!(screen_contains(
            &s,
            "Defaults  1 core, 512 MiB RAM, user network — chosen anew for each VM"
        ));

        // With the VM pane focused the right column shows the VM card again.
        app.focus = Focus::Vms;
        let s = draw(&mut app, 140, 40);
        assert!(screen_contains(&s, " Ostrich "));
        assert!(!screen_contains(&s, "Template  alpha"));
    }

    #[test]
    fn status_bar_shows_notice_and_busy() {
        let (_dir, mut app) = test_app();
        app.notice = Some(Notice::Ok("started debian-12".into()));
        app.busy = Some("starting ubuntu-24…".into());
        app.tick = 1;
        let s = draw(&mut app, 140, 40);
        let bar = &s[38];
        assert!(bar.starts_with(" ✓ started debian-12"), "{bar:?}");
        assert!(bar.ends_with("⠙ starting ubuntu-24…"), "{bar:?}");

        app.notice = Some(Notice::Err("start x: boom\nsecond line".into()));
        app.busy = None;
        let s = draw(&mut app, 140, 40);
        assert!(s[38].starts_with(" ✗ start x: boom"), "{:?}", s[38]);
        assert!(!s[38].contains("second line"));

        // A long notice is cut to the bar.
        app.notice = Some(Notice::Err("x".repeat(100)));
        let s = draw(&mut app, 60, 20);
        assert_eq!(s[18].chars().count(), 60, "{:?}", s[18]);
        assert!(s[18].ends_with('…'));

        // And makes room for the busy label.
        app.busy = Some("deleting y…".into());
        let s = draw(&mut app, 60, 20);
        assert!(s[18].ends_with("deleting y…"), "{:?}", s[18]);
        assert!(s[18].contains("…  "), "a gap before the label: {:?}", s[18]);

        app.notice = None;
        app.busy = None;
        let s = draw(&mut app, 60, 20);
        assert_eq!(s[18], "");
    }

    #[test]
    fn hints_follow_the_focus() {
        let (_dir, mut app) = test_app();
        app.vms.push(entry("debian-12", 2, 2048, 20, false));
        let hints = key_hints(&app);
        assert_eq!(hints[0], ("j/k".to_string(), "move".to_string()));
        let keys: Vec<&str> = hints.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                "j/k", "Tab", "Enter", "s", "x", "n", "e", "u", "i", "t", "c", "v", "d", "r", "?",
                "q"
            ]
        );
        assert!(hints.contains(&("t".to_string(), "template".to_string())));

        app.focus = Focus::Templates;
        let hints = key_hints(&app);
        let keys: Vec<&str> = hints.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["Enter/n", "h", "Tab", "?", "q"], "no templates yet");
        app.templates.push(template("base", 2, 2048, 20));
        let hints = key_hints(&app);
        let keys: Vec<&str> = hints.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["j/k", "Enter/n", "d", "h", "Tab", "?", "q"]);
        assert!(hints.contains(&("Enter/n".to_string(), "new VM from template".to_string())));

        app.focus = Focus::Console;
        let hints = key_hints(&app);
        let keys: Vec<&str> = hints.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            ["j/k", "Ctrl-d/u", "g/G", "h", "s/x", "c", "v", "r", "?", "q"]
        );
        assert!(hints.contains(&("Ctrl-d/u".to_string(), "half page".to_string())));

        // Rendered in the bottom bar, whole when it fits.
        app.focus = Focus::Vms;
        let s = draw(&mut app, 160, 40);
        let bar = &s[39];
        assert!(
            bar.starts_with(" j/k move  Tab pane  Enter console  s start  x stop  n new"),
            "{bar:?}"
        );
        assert!(bar.ends_with("r refresh  ? keys  q quit"), "{bar:?}");
        app.focus = Focus::Console;
        let s = draw(&mut app, 140, 40);
        assert!(s[39].contains("h back"), "{:?}", s[39]);
        assert!(s[39].contains("Ctrl-d/u half page"));
    }

    #[test]
    fn narrow_hint_bars_drop_whole_hints_and_keep_keys_and_quit() {
        let (_dir, mut app) = test_app();
        app.vms.push(entry("debian-12", 2, 2048, 20, false));
        let bar = |app: &mut App, w: u16, h: u16| draw(app, w, h)[h as usize - 1].clone();
        for (w, want) in [
            (
                160,
                " j/k move  Tab pane  Enter console  s start  x stop  n new  e edit  u USB  i ISO  \
                 t template  c serial  v VNC  d delete  r refresh  ? keys  q quit",
            ),
            (
                140,
                " j/k move  Tab pane  Enter console  s start  x stop  n new  e edit  u USB  i ISO  \
                 t template  c serial  v VNC  d delete  ? keys  q quit",
            ),
            (
                120,
                " j/k move  s start  x stop  n new  e edit  u USB  i ISO  t template  c serial  \
                 v VNC  d delete  ? keys  q quit",
            ),
            (
                100,
                " j/k move  s start  x stop  n new  e edit  u USB  i ISO  t template  d delete  \
                 ? keys  q quit",
            ),
            (
                80,
                " j/k move  s start  x stop  n new  e edit  u USB  d delete  ? keys  q quit",
            ),
            (60, " j/k move  s start  x stop  n new  e edit  ? keys  q quit"),
            (40, " j/k move  n new  e edit  ? keys  q quit"),
        ] {
            assert_eq!(bar(&mut app, w, 24), want, "VMs at {w}");
        }

        app.focus = Focus::Templates;
        app.templates.push(template("base", 2, 2048, 20));
        assert_eq!(
            bar(&mut app, 80, 24),
            " j/k move  Enter/n new VM from template  d delete  h back  ? keys  q quit"
        );
        assert_eq!(
            bar(&mut app, 100, 30),
            " j/k move  Enter/n new VM from template  d delete  h back  Tab pane  ? keys  q quit"
        );

        app.focus = Focus::Console;
        assert_eq!(
            bar(&mut app, 120, 40),
            " j/k scroll  Ctrl-d/u half page  g/G top/bottom  h back  s/x start/stop  c serial  \
             v VNC  r refresh  ? keys  q quit"
        );
        assert_eq!(
            bar(&mut app, 100, 30),
            " j/k scroll  Ctrl-d/u half page  g/G top/bottom  h back  s/x start/stop  c serial  \
             ? keys  q quit"
        );
        assert_eq!(
            bar(&mut app, 80, 24),
            " j/k scroll  g/G top/bottom  h back  s/x start/stop  ? keys  q quit"
        );

        // A panel's hints go from the right, keeping the Esc one.
        let theme = Theme::default();
        const ISO: &[(&str, &str)] = &[
            ("c", "change boot ISO"),
            ("e", "eject"),
            ("a", "attach USB image"),
            ("Space/Enter/d", "detach"),
            ("r", "refresh"),
            ("j/k", "move"),
            ("q/Esc", "back"),
        ];
        let iso = ISO;
        let esc = |k: &str| k.contains("Esc");
        assert_eq!(
            fit_hints(iso, 79, &[], esc, &theme).to_string(),
            "c change boot ISO  e eject  a attach USB image  q/Esc back"
        );
        assert_eq!(
            fit_hints(iso, 39, &[], esc, &theme).to_string(),
            "c change boot ISO  e eject  q/Esc back"
        );
        assert_eq!(
            fit_hints(iso, 12, &[], esc, &theme).to_string(),
            "q/Esc back"
        );
        // Too narrow even for the pinned hint: cut as a last resort.
        assert_eq!(fit_hints(iso, 8, &[], esc, &theme).to_string(), "q/Esc b…");

        // Through the bar while a panel is open.
        app.open_panel(Box::new(TestPanel {
            busy: None,
            hints: ISO,
        }));
        assert_eq!(
            bar(&mut app, 80, 24),
            " c change boot ISO  e eject  a attach USB image  q/Esc back"
        );
        // While its change is in flight a dialog's way out is Ctrl-c, kept
        // the same way.
        const ISO_BUSY: &[(&str, &str)] = &[
            ("c", "change boot ISO"),
            ("e", "eject"),
            ("a", "attach USB image"),
            ("Space/Enter/d", "detach"),
            ("r", "refresh"),
            ("j/k", "move"),
            ("Ctrl-c", "quit"),
        ];
        app.open_panel(Box::new(TestPanel {
            busy: Some("attaching x.iso…".into()),
            hints: ISO_BUSY,
        }));
        assert_eq!(
            bar(&mut app, 80, 24),
            " c change boot ISO  e eject  a attach USB image  Ctrl-c quit"
        );
    }

    #[test]
    fn console_title_names_the_vm() {
        let (_dir, mut app) = test_app();
        assert_eq!(console_title(&app).to_string(), " Serial console ");
        app.vms.push(entry("debian-12", 2, 2048, 20, false));
        assert_eq!(
            console_title(&app).to_string(),
            " Serial console · debian-12 · last 200 lines · 2s "
        );
    }

    #[test]
    fn small_areas_do_not_panic() {
        let (_dir, mut app) = test_app();
        app.vms.push(entry("debian-12", 2, 2048, 20, true));
        app.templates.push(template("base", 1, 512, 8));
        app.notice = Some(Notice::Ok("ok".into()));
        app.busy = Some("busy…".into());
        let mut terminal = Terminal::new(TestBackend::new(10, 4)).unwrap();
        terminal
            .draw(|f| {
                for r in [
                    Rect::new(0, 0, 0, 0),
                    Rect::new(0, 0, 1, 1),
                    Rect::new(0, 0, 2, 2),
                    Rect::new(0, 0, 3, 3),
                    Rect::new(0, 0, 10, 4),
                ] {
                    render_vm_list(&mut app, f, r);
                    render_templates(&mut app, f, r);
                    let (title, lines) = vm_details(&app);
                    render_details(&app, f, r, title, lines, true);
                    let (title, lines) = template_details(&app);
                    render_details(&app, f, r, title, lines, false);
                    render_status_bar(&app, f, r);
                    render_hints(&app, f, r);
                }
            })
            .unwrap();
        // A small dashboard, and one below the app's minimum.
        draw(&mut app, 40, 10);
        assert!(screen_contains(
            &draw(&mut app, 39, 7),
            "terminal too small"
        ));
    }

    #[test]
    fn line_truncation_keeps_whole_spans() {
        let line = Line::from(vec![Span::raw("abc"), Span::raw("defgh")]);
        assert_eq!(truncate_line(line.clone(), 8).to_string(), "abcdefgh");
        assert_eq!(truncate_line(line.clone(), 5).to_string(), "abcd…");
        assert_eq!(truncate_line(line.clone(), 4).to_string(), "abc…");
        assert_eq!(truncate_line(line.clone(), 3).to_string(), "ab…");
        assert_eq!(truncate_line(line.clone(), 1).to_string(), "…");
        assert_eq!(truncate_line(line, 0).to_string(), "");
    }

    #[test]
    fn image_states_read_like_the_go_screen() {
        let theme = Theme::default();
        let gone = ImageState {
            path: "/x".into(),
            size: 0,
            err: Some("image not found: /x".into()),
        };
        assert_eq!(image_state_span(&gone, &theme).content, "✗ not found");
        let locked = ImageState {
            path: "/x".into(),
            size: 0,
            err: Some("no read access to image /x".into()),
        };
        assert_eq!(image_state_span(&locked, &theme).content, "✗ no access");
        let odd = ImageState {
            path: "/x".into(),
            size: 0,
            err: Some("image is a directory: /x".into()),
        };
        assert_eq!(
            image_state_span(&odd, &theme).content,
            "✗ image is a directory: /x"
        );
        let sized = ImageState {
            path: "/x".into(),
            size: 4617089843,
            err: None,
        };
        assert_eq!(image_state_span(&sized, &theme).content, "● 4.3 GiB");
        assert_eq!(disk_state_span(&sized, &theme).content, "● 4.3 GiB on host");
        let empty = ImageState {
            path: "/x".into(),
            size: 0,
            err: None,
        };
        assert_eq!(image_state_span(&empty, &theme).content, "● present");
        assert_eq!(disk_state_span(&gone, &theme).content, "✗ not found");
    }

    /// A panel that draws nothing, with the given hints and busy label.
    struct TestPanel {
        busy: Option<String>,
        hints: &'static [(&'static str, &'static str)],
    }

    impl crate::tui::panel::Panel for TestPanel {
        fn title(&self) -> String {
            "Test".to_string()
        }
        fn handle_key(
            &mut self,
            _key: crossterm::event::KeyEvent,
            _ctx: &mut crate::tui::panel::Ctx,
        ) {
        }
        fn on_task(
            &mut self,
            _result: crate::tui::events::TaskResult,
            _ctx: &mut crate::tui::panel::Ctx,
        ) {
        }
        fn render(&mut self, _frame: &mut Frame, _area: Rect, _theme: &Theme, _tick: u64) {}
        fn key_hints(&self) -> Vec<(String, String)> {
            self.hints
                .iter()
                .map(|(k, d)| (k.to_string(), d.to_string()))
                .collect()
        }
        fn busy(&self) -> Option<String> {
            self.busy.clone()
        }
    }

    /// A running VM with every kind of row filled in by a refresh, a
    /// stopped one-core VM, and two templates.
    fn busy_dashboard() -> (tempfile::TempDir, App) {
        let (dir, mut app) = test_app();
        let cfg = VmConfig {
            name: "debian-12".into(),
            cpu: 2,
            ram: 2048,
            disk_size: 20,
            cdrom_path: "/home/user/iso/debian-12.3.0-amd64-netinst.iso".into(),
            firmware: FirmwareType::Uefi,
            tpm: true,
            network: NetworkConfig {
                kind: NetworkType::User,
                mac: "52:54:00:00:00:03".into(),
                port_forwards: vec![PortForward {
                    host: 2222,
                    guest: 22,
                    proto: "tcp".into(),
                }],
            },
            vnc_port: 1,
            usb_devices: vec![
                UsbDevice {
                    vendor_id: "046d".into(),
                    product_id: "085c".into(),
                    name: "C922 Pro Stream Webcam".into(),
                    port: String::new(),
                },
                UsbDevice {
                    vendor_id: "0781".into(),
                    product_id: "5583".into(),
                    name: "SanDisk Ultra Fit".into(),
                    port: String::new(),
                },
            ],
            usb_images: vec![
                UsbImage {
                    path: "/home/user/iso/virtio-win.iso".into(),
                },
                UsbImage {
                    path: "/home/user/iso/gone.iso".into(),
                },
            ],
            disks: vec![
                Disk {
                    name: "data".into(),
                    size: 50,
                },
                Disk {
                    name: "scratch".into(),
                    size: 196,
                },
            ],
            created_at: "2026-03-17T09:00:00Z".parse().unwrap(),
            ..VmConfig::default()
        };
        let running = ProcessInfo {
            pid: 94231,
            status: VmStatus::Running,
        };
        let present = |path: &str, size: u64| ImageState {
            path: path.into(),
            size,
            err: None,
        };
        app.details = Some(Details {
            name: "debian-12".into(),
            status: running,
            guest_ip: Some("10.0.2.15".into()),
            usb: match_usb(&cfg.usb_devices, &[host_dev("046d", "085c", true)]),
            cdrom: present(&cfg.cdrom_path, 661_651_456),
            images: vec![
                present(&cfg.usb_images[0].path, 640_000_000),
                ImageState {
                    path: cfg.usb_images[1].path.clone(),
                    size: 0,
                    err: Some("image not found: /home/user/iso/gone.iso".into()),
                },
            ],
            disks: vec![present("a", 4_617_089_843), present("b", 196 << 20)],
            console: vec!["debian login:".into()],
        });
        app.vms.push(VmEntry {
            cfg,
            status: running,
        });
        app.vms.push(entry("windows-11", 1, 8192, 64, false));
        let mut deb = template("debian-12-base", 4, 8192, 64);
        deb.tpl.description = "Windows 11 23H2, updates applied, virtio drivers installed".into();
        deb.tpl.firmware = FirmwareType::Uefi;
        deb.tpl.tpm = true;
        deb.tpl.vnc = true;
        deb.disk_usage = 12_000_000_000;
        let mut win = template("win11-base", 4, 8192, 64);
        win.tpl.firmware = FirmwareType::Uefi;
        win.tpl.secure_boot = true;
        win.tpl.tpm = true;
        app.templates = vec![deb, win];
        (dir, app)
    }

    /// The lines of the right-hand card (between its borders, margins
    /// stripped), from a full-screen render.
    fn card_lines(s: &[String], left_w: usize) -> Vec<String> {
        let mut out = Vec::new();
        for l in s.iter().skip(1) {
            let right: String = l.chars().skip(left_w).collect();
            if right.starts_with('╰') {
                break;
            }
            let inner: String = right.chars().skip(2).collect();
            let inner: String = inner.chars().take(inner.chars().count() - 2).collect();
            out.push(inner.trim_end().to_string());
        }
        out
    }

    #[test]
    fn card_rows_are_cut_with_an_ellipsis_and_keep_their_states() {
        let (_dir, mut app) = busy_dashboard();

        // 80x24: a 46-cell card body.
        let s = draw(&mut app, 80, 24);
        let card = card_lines(&s, 30);
        let text = s.join("\n");
        assert_eq!(
            card,
            [
                "Status    ● running  (PID 94231)",
                "CPU       2 cores",
                "RAM       2048 MiB",
                "Disk      20 GiB",
                "          data       50 GiB  ● 4.3 GiB on host",
                "          scratch   196 GiB  ● 196 MiB on host",
                "ISO       …md64-netinst.iso  ● 631 MiB",
                "Firmware  UEFI, TPM 2.0",
                "Net       user [2222→22]",
                "IP        10.0.2.15  —  ssh -p 2222 localhost",
                "VNC       127.0.0.1:1  (TCP port 5901)",
                "USB       C922 Pr…  046d:085c  ● connected",
                "… 4 more — enlarge the terminal",
            ],
            "{text}"
        );

        // 120x40: an 76-cell body holds every row; the paths are whole, the
        // IP drops the gateway but keeps the ssh command.
        let s = draw(&mut app, 120, 40);
        let card = card_lines(&s, 40);
        let text = s.join("\n");
        assert_eq!(
            card[4..],
            [
                "          data                                    50 GiB  ● 4.3 GiB on host",
                "          scratch                                196 GiB  ● 196 MiB on host",
                "ISO       /home/user/iso/debian-12.3.0-amd64-netinst.iso  ● 631 MiB",
                "Firmware  UEFI, TPM 2.0",
                "Net       user [2222→22]",
                "IP        10.0.2.15 (DHCP, guest-internal)  —  ssh -p 2222 localhost",
                "VNC       127.0.0.1:1  (TCP port 5901)",
                "USB       C922 Pro Stream Webcam  046d:085c  ● connected",
                "          SanDisk Ultra Fit       0781:5583  ○ not connected",
                "USB ISO   /home/user/iso/virtio-win.iso                   ● 610 MiB",
                "          /home/user/iso/gone.iso                         ✗ not found",
                &format!(
                    "Created   {}",
                    local_time("2026-03-17T09:00:00Z".parse().unwrap())
                ),
            ],
            "{text}"
        );

        // 160x45: the whole IP wording.
        let s = draw(&mut app, 160, 45);
        assert!(
            screen_contains(
                &s,
                "IP        10.0.2.15 (DHCP, guest-internal)  gw 10.0.2.2  —  ssh -p 2222 localhost"
            ),
            "{}",
            s.join("\n")
        );
        assert!(!s.join("\n").contains("more — enlarge"));

        // The templates card: cut at a word, or a shorter wording.
        app.focus = Focus::Templates;
        let s = draw(&mut app, 80, 24);
        let card = card_lines(&s, 30);
        assert_eq!(
            card,
            [
                "Template  debian-12-base",
                "About     Windows 11 23H2, updates applied…",
                &format!(
                    "From VM   src, saved {}",
                    local_time("2026-10-09T13:33:00Z".parse().unwrap())
                ),
                "Disk      64 GiB virtual, 11.2 GiB on the host",
                "Firmware  UEFI, TPM 2.0",
                "Defaults  4 cores, 8192 MiB RAM, user network",
                "VNC       enabled",
            ],
            "{}",
            s.join("\n")
        );
        let s = draw(&mut app, 160, 45);
        assert!(screen_contains(
            &s,
            "Defaults  4 cores, 8192 MiB RAM, user network — chosen anew for each VM"
        ));
        assert!(screen_contains(
            &s,
            "VNC       enabled — a new VM gets a free display number"
        ));
    }

    #[test]
    fn card_lines_fit_any_width() {
        let (_dir, app) = busy_dashboard();
        let (_, card) = vm_details(&app);
        assert_eq!(card.len(), 16);
        for width in 0..120 {
            for line in card.lines(width, 40) {
                assert!(line.width() <= width, "{width}: {line:?}");
            }
        }
        // A state too long to line up with the others is cut, not the paths.
        let mut app = app;
        if let Some(d) = app.details.as_mut() {
            d.images[1].err = Some("stat /home/user/iso/gone.iso: input/output error".into());
        }
        let (_, card) = vm_details(&app);
        let lines: Vec<String> = card.lines(76, 40).iter().map(|l| l.to_string()).collect();
        let row = line_with(&lines, "gone.iso");
        assert!(
            row.starts_with("          /home/user/iso/gone.iso "),
            "{row}"
        );
        assert!(row.ends_with('…'), "{row}");
        assert_eq!(row.width(), 76, "{row}");
    }

    #[test]
    fn short_card_says_how_many_rows_are_hidden() {
        let (_dir, mut app) = busy_dashboard();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let s = buffer_lines(&buf);
        let y = s
            .iter()
            .position(|l| l.contains("… 4 more — enlarge the terminal"))
            .unwrap_or_else(|| panic!("{}", s.join("\n")));
        // The last row the card has room for, in the muted help style.
        assert!(s[y + 1].contains('╰'), "{}", s.join("\n"));
        assert_eq!(buf[(32, y as u16)].symbol(), "…");
        assert_eq!(buf[(32, y as u16)].style().fg, Some(app.theme.muted));

        let (_, card) = vm_details(&app);
        let lines = card.lines(60, 3);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[2].to_string(), "… 14 more — enlarge the terminal");
        assert_eq!(
            card.lines(60, 1)[0].to_string(),
            "… 16 more — enlarge the terminal"
        );
        assert_eq!(card.lines(60, 16).len(), 16, "an exact fit needs no note");
        assert!(!card.lines(60, 16)[15].to_string().contains("more"));
    }

    #[test]
    fn template_rows_cut_the_firmware_label_before_the_border() {
        let (_dir, mut app) = busy_dashboard();
        let rows = |s: &[String]| -> (String, String) {
            (
                line_with(s, "debian-12-base  ").to_string(),
                line_with(s, "win11-base  ").to_string(),
            )
        };
        for (w, h, deb, win) in [
            (
                80,
                24,
                "│ ▸ debian-12-base  UEFI…    │",
                "│   win11-base      UEFI…    │",
            ),
            (
                100,
                30,
                "│ ▸ debian-12-base  UEFI, TPM…   │",
                "│   win11-base      UEFI + Secu… │",
            ),
            (
                120,
                40,
                "│ ▸ debian-12-base  UEFI, TPM 2.0      │",
                "│   win11-base      UEFI + Secure…     │",
            ),
            (
                160,
                45,
                "│ ▸ debian-12-base  UEFI, TPM 2.0                    │",
                "│   win11-base      UEFI + Secure Boot, TPM 2.0      │",
            ),
        ] {
            let s = draw(&mut app, w, h);
            let (d, n) = rows(&s);
            assert!(d.starts_with(deb), "{w}x{h}: {d:?}");
            assert!(n.starts_with(win), "{w}x{h}: {n:?}");
        }
    }

    #[test]
    fn list_rows_keep_a_cell_free_before_the_border() {
        let (_dir, mut app) = test_app();
        app.vms.push(entry(
            "a-very-long-vm-name-exceeding-twenty",
            1,
            512,
            8,
            true,
        ));
        app.vms.push(entry("b", 1, 512, 8, false));
        app.templates
            .push(template("a-template-named-twenty", 1, 512, 8));
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let s = buffer_lines(&buf);
        // The left pane is 30 wide: border at 0 and 29, rows in 1..=28.
        for y in [1, 2, 19] {
            let l: Vec<char> = s[y].chars().collect();
            assert_eq!(l[29], '│', "{:?}", s[y]);
            assert_eq!(l[28], ' ', "a free cell before the border: {:?}", s[y]);
            assert_ne!(l[27], ' ', "the row runs up to it: {:?}", s[y]);
        }
        assert!(s[1].contains("▸ a-very-long-…  ● running"), "{:?}", s[1]);
        // The highlight still runs border to border.
        assert_eq!(buf[(28, 1)].style().bg, Some(app.theme.primary));
    }

    #[test]
    fn console_title_sheds_parts_to_fit_the_pane() {
        let (_dir, mut app) = test_app();
        assert_eq!(console_title_fit(&app, 30).to_string(), " Serial console ");
        app.vms.push(entry("debian-12", 2, 2048, 20, false));
        let full = " Serial console · debian-12 · last 200 lines · 2s ";
        assert_eq!(console_title(&app).to_string(), full);
        for (width, want) in [
            (200, full),
            (52, full),
            (51, " Serial console · debian-12 · last 200 lines "),
            (47, " Serial console · debian-12 · last 200 lines "),
            (46, " Serial console · debian-12 "),
            (30, " Serial console · debian-12 "),
            (29, " Serial console · debian… "),
            (26, " Serial console · debi… "),
            (25, " Serial console · deb… "),
            (24, " Serial console "),
            (18, " Serial console "),
        ] {
            let got = console_title_fit(&app, width).to_string();
            assert_eq!(got, want, "{width}");
            assert!(got.width() <= usize::from(width) - 2, "{width}: {got:?}");
        }
        // On the dashboard: an 80-column screen has a 50-cell console pane.
        let s = draw(&mut app, 80, 24);
        let top = line_with(&s, "╭ Serial console");
        assert!(
            top.contains("╭ Serial console · debian-12 · last 200 lines ─"),
            "{top}"
        );
        assert!(!top.contains("· 2"), "{top}");
        let s = draw(&mut app, 120, 40);
        assert!(screen_contains(
            &s,
            "╭ Serial console · debian-12 · last 200 lines · 2s ─"
        ));
    }

    /// A pane too narrow even for ` Serial console ` gets it cut with an
    /// ellipsis, not mid-word by the border.
    #[test]
    fn console_title_is_cut_with_an_ellipsis_when_nothing_fits() {
        let (_dir, mut app) = test_app();
        for vm in [false, true] {
            if vm {
                app.vms.push(entry("debian-12", 2, 2048, 20, false));
            }
            for (width, want) in [
                (18, " Serial console "),
                (17, " Serial conso… "),
                (16, " Serial cons… "),
                (12, " Serial… "),
                (8, " Ser… "),
                (4, ""),
            ] {
                let got = console_title_fit(&app, width).to_string();
                assert_eq!(got, want, "{width} (VM: {vm})");
            }
        }
        // The narrowest dashboard has a 16-cell console pane.
        let s = draw(&mut app, 40, 12);
        let top = line_with(&s, "╭ Serial");
        assert!(top.ends_with("╭ Serial cons… ╮"), "{top}");
    }

    /// Without a VM the VM list offers only the keys that do something
    /// there, and so does the console pane.
    #[test]
    fn an_empty_vm_list_offers_only_what_works_without_a_vm() {
        let (_dir, mut app) = test_app();
        let keys =
            |app: &App| -> Vec<String> { key_hints(app).into_iter().map(|(k, _)| k).collect() };
        assert_eq!(keys(&app), ["n", "Tab", "r", "?", "q"]);
        app.loading = true;
        assert_eq!(keys(&app), ["n", "Tab", "r", "?", "q"], "loading");
        app.loading = false;
        let bar = |app: &mut App, w: u16| draw(app, w, 24)[23].clone();
        assert_eq!(
            bar(&mut app, 80),
            " n new  Tab pane  r refresh  ? keys  q quit"
        );
        assert_eq!(bar(&mut app, 40), " n new  Tab pane  ? keys  q quit");
        app.focus = Focus::Console;
        assert_eq!(keys(&app), ["h", "Tab", "r", "?", "q"]);
        assert_eq!(
            bar(&mut app, 80),
            " h back  Tab pane  r refresh  ? keys  q quit"
        );
        // A VM brings its keys back.
        app.vms.push(entry("debian-12", 2, 2048, 20, false));
        assert!(keys(&app).contains(&"s/x".to_string()));
        app.focus = Focus::Vms;
        assert!(keys(&app).contains(&"s".to_string()));
    }

    #[test]
    fn status_bar_spins_only_for_the_dashboards_own_action() {
        let (_dir, mut app) = test_app();
        app.vms.push(entry("debian-12", 2, 2048, 20, false));
        app.open_panel(Box::new(TestPanel {
            busy: Some("copying the disk image…".into()),
            hints: &[("Esc", "cancel")],
        }));
        let s = draw(&mut app, 120, 40);
        assert!(!s[38].contains("copying"), "{:?}", s[38]);
        assert!(
            !s[38]
                .chars()
                .any(|c| SPINNER_FRAMES.contains(&c.to_string().as_str())),
            "{:?}",
            s[38]
        );
        app.busy = Some("starting debian-12…".into());
        let s = draw(&mut app, 120, 40);
        assert!(s[38].ends_with("starting debian-12…"), "{:?}", s[38]);
    }

    #[test]
    fn cards_say_one_core_or_n_cores() {
        let (_dir, mut app) = busy_dashboard();
        app.vm_cursor.index = 1;
        let s = draw(&mut app, 120, 40);
        assert!(screen_contains(&s, "CPU       1 core"), "{}", s.join("\n"));
        assert!(!screen_contains(&s, "1 cores"));
        app.vm_cursor.index = 0;
        assert!(screen_contains(
            &draw(&mut app, 120, 40),
            "CPU       2 cores"
        ));
    }
}
