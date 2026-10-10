//! The dashboard: the event loop, the two lists on the left, the details and
//! console on the right, and the glue that opens panels and popups and runs
//! VM actions in the background.

use std::fmt;
use std::io::{self, stdout};
use std::os::unix::process::CommandExt as _;
use std::panic::{self, PanicHookInfo};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Receiver;
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::cursor::Show;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use nix::sys::signal::{sigaction, signal, SaFlags, SigAction, SigHandler, SigSet, Signal};
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;
use ratatui::DefaultTerminal;

use super::actions;
use super::console::{ConsoleView, CONSOLE_POLL_SECONDS};
use super::dashboard;
use super::dialogs::{iso::IsoDialog, usb::UsbDialog};
use super::events::{
    failure_text, Details, Event, Notice, PanelId, Target, TaskResult, Tasks, TemplateEntry,
    VmEntry,
};
use super::forms::{
    create::CreateForm, edit::EditForm, from_template::FromTemplateForm,
    save_template::SaveTemplateForm,
};
use super::panel::{Action, Ctx, Panel};
use super::popups::{self, ConfirmAction, Popup};
use super::setup;
use super::theme::Theme;
use super::widgets::Cursor;
use crate::config::{self, AppConfig};
use crate::vm::{self, Manager, ProcessInfo};

/// The spinner and refresh heartbeat.
const TICK: Duration = Duration::from_millis(100);

/// The narrowest dashboard drawn; below it the screen says `terminal too
/// small`.
const MIN_WIDTH: u16 = 40;
/// The templates pane's least height: its borders and two rows.
const TPL_PANE_MIN: u16 = 4;
/// The shortest dashboard drawn: the two bars, a VM list with one row
/// (borders included) and the templates pane at its least. The right column
/// then has 7 rows, a details card of up to 4 (70 %) and a console of 3,
/// each showing a row.
const MIN_HEIGHT: u16 = 2 + 3 + TPL_PANE_MIN;

/// Which pane has the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Vms,
    Templates,
    Console,
}

/// A row to put the cursor on once the lists have reloaded.
#[derive(Debug, Clone)]
enum Select {
    Vm(String),
    Template(String),
}

struct OpenPanel {
    id: PanelId,
    panel: Box<dyn Panel>,
}

/// Runs the dashboard until the user quits. On the first run it asks for
/// the VM storage directory first. A config file that cannot be read is an
/// [`InitError`], reported before the terminal is touched.
pub fn run() -> Result<()> {
    // Like Go's NewApp: a broken config is fatal before any UI.
    let cfg = config::load().map_err(InitError)?;
    let mut terminal = init_terminal()?;
    let _ = execute!(stdout(), EnableBracketedPaste);
    // Dropped only once the terminal is restored, so a late SIGTERM cannot
    // leave it raw.
    let mut quit_signals = None;
    let result = run_inner(&mut terminal, cfg, &mut quit_signals);
    restore_terminal();
    drop(quit_signals);
    result
}

fn run_inner(
    terminal: &mut DefaultTerminal,
    cfg: Option<AppConfig>,
    quit_signals: &mut Option<SignalGuard>,
) -> Result<()> {
    // Caught from the first screen on: both the setup wizard and the
    // dashboard look for a caught signal between keys and quit the normal way.
    *quit_signals = Some(SignalGuard::set(
        &QUIT_SIGNALS,
        SigHandler::Handler(note_quit_signal),
    ));
    let cfg = match cfg {
        Some(cfg) => cfg,
        None => match setup::run(terminal, || take_quit_signals() > 0)? {
            Some(cfg) => cfg,
            None => return Ok(()),
        },
    };
    let mut app = App::new(cfg);
    app.main_loop(terminal)
}

/// A failure before the dashboard could start (the config file could not be
/// read or parsed); `main` prints it the way the Go program did:
/// `error initializing: <err>`.
#[derive(Debug)]
pub struct InitError(pub anyhow::Error);

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#}", self.0)
    }
}

impl std::error::Error for InitError {}

// ---------------------------------------------------------------------------
// Terminal and signals
// ---------------------------------------------------------------------------

type PanicHook = Box<dyn Fn(&PanicHookInfo<'_>) + Sync + Send + 'static>;

/// Raw mode and the alternate screen. A panic on this (the UI) thread puts
/// the terminal back before the message is printed; a panic on a task
/// thread does not touch it, since the task reports it as a notice and the
/// dashboard carries on (ratatui's own hook would restore the terminal under
/// the live dashboard, on any thread).
fn init_terminal() -> Result<DefaultTerminal> {
    let prev = panic::take_hook();
    let terminal = ratatui::try_init();
    drop(panic::take_hook()); // ratatui's hook
    match terminal {
        Ok(terminal) => {
            panic::set_hook(ui_panic_hook(
                thread::current().id(),
                restore_terminal,
                prev,
            ));
            Ok(terminal)
        }
        Err(err) => {
            panic::set_hook(prev);
            let _ = disable_raw_mode();
            Err(anyhow::Error::new(err).context("initialize terminal"))
        }
    }
}

/// A panic hook that runs `restore` and then `prev` for a panic on the `ui`
/// thread, and stays silent for any other thread: a task's panic is caught
/// and shown in the status bar, and printing it would scribble over the
/// dashboard.
fn ui_panic_hook(ui: ThreadId, restore: fn(), prev: PanicHook) -> PanicHook {
    Box::new(move |info| {
        if thread::current().id() == ui {
            restore();
            prev(info);
        }
    })
}

/// Gives the shell its terminal back: raw mode off, bracketed paste off,
/// the main screen, the cursor shown. Used on quit and by the panic hook.
fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(stdout(), DisableBracketedPaste, LeaveAlternateScreen, Show);
}

/// The signals that make Ostrich quit the normal way, terminal restored, as
/// Go's Bubble Tea did for SIGINT and SIGTERM. SIGINT only comes from
/// outside: in raw mode Ctrl-c is a key.
///
/// SIGHUP keeps its default action, as in Go: it comes when the terminal
/// is gone, and on a hung-up terminal crossterm's event reader never
/// returns (it takes a read of 0 bytes for "more to come" and reads again),
/// so a caught hangup would never be acted on and Ostrich would spin on the
/// closed terminal forever.
const QUIT_SIGNALS: [Signal; 2] = [Signal::SIGINT, Signal::SIGTERM];

/// The signals the terminal sends its foreground process group. Ostrich
/// ignores them while the serial console runs, so they reach socat (and the
/// guest) only.
const JOB_SIGNALS: [Signal; 3] = [Signal::SIGINT, Signal::SIGQUIT, Signal::SIGTSTP];

/// Quit signals caught since the event loop last looked.
static QUIT_SIGNALS_CAUGHT: AtomicUsize = AtomicUsize::new(0);

/// The handler for [`QUIT_SIGNALS`]: counts, nothing else (async-signal-safe).
extern "C" fn note_quit_signal(_sig: nix::libc::c_int) {
    QUIT_SIGNALS_CAUGHT.fetch_add(1, Ordering::SeqCst);
}

/// How many quit signals arrived since the last call.
fn take_quit_signals() -> usize {
    QUIT_SIGNALS_CAUGHT.swap(0, Ordering::SeqCst)
}

/// Sets the disposition of some signals and puts the previous ones back
/// when dropped.
struct SignalGuard {
    saved: Vec<(Signal, SigAction)>,
}

impl SignalGuard {
    fn set(signals: &[Signal], handler: SigHandler) -> SignalGuard {
        let action = SigAction::new(handler, SaFlags::SA_RESTART, SigSet::empty());
        let saved = signals
            .iter()
            .filter_map(|&sig| {
                // SAFETY: the handlers installed here are SIG_IGN, SIG_DFL or
                // note_quit_signal, which only touches an atomic.
                unsafe { sigaction(sig, &action) }
                    .ok()
                    .map(|old| (sig, old))
            })
            .collect();
        SignalGuard { saved }
    }
}

impl Drop for SignalGuard {
    fn drop(&mut self) {
        for (sig, old) in self.saved.iter().rev() {
            // SAFETY: reinstalls the disposition that was there before.
            let _ = unsafe { sigaction(*sig, old) };
        }
    }
}

/// Makes `cmd`'s process start with the default job-control signals: Ostrich
/// ignores them while it runs, and an ignored signal stays ignored across
/// exec, so Ctrl-C would not end a socat outside raw mode.
fn default_job_signals_in_child(cmd: &mut Command) {
    let reset = || {
        for sig in JOB_SIGNALS {
            // SAFETY: SIG_DFL installs no handler.
            unsafe { signal(sig, SigHandler::SigDfl) }.map_err(io::Error::from)?;
        }
        Ok(())
    };
    // SAFETY: the closure runs between fork and exec and only calls
    // sigaction(2), which is async-signal-safe; it allocates nothing.
    unsafe {
        cmd.pre_exec(reset);
    }
}

/// An interactive command to run with the TUI suspended (the serial console).
struct Suspend {
    cmd: Command,
    /// Shown when the command returns, whatever its exit status.
    done: String,
}

/// A background load of one kind — the lists, or the selected VM's details.
/// At most one runs at a time: asking for another while one runs marks the
/// load dirty, and it runs again once the running one lands. Results carry
/// the load's number, and one older than the newest applied is dropped.
#[derive(Debug, Default)]
struct Load {
    /// The number of the last load started.
    started: u64,
    /// The number of the newest load whose result came back.
    landed: u64,
    /// The running load's number.
    in_flight: Option<u64>,
    /// Asked for again while one was running.
    dirty: bool,
}

impl Load {
    /// The number to start a load with, or `None` when one is running (the
    /// load is then marked dirty).
    fn begin(&mut self) -> Option<u64> {
        if self.in_flight.is_some() {
            self.dirty = true;
            return None;
        }
        self.started += 1;
        self.in_flight = Some(self.started);
        Some(self.started)
    }

    /// Load `seq` came back. Whether its result is newer than anything
    /// before it and should be applied.
    fn land(&mut self, seq: u64) -> bool {
        if self.in_flight == Some(seq) {
            self.in_flight = None;
        }
        if seq <= self.landed {
            return false;
        }
        self.landed = seq;
        true
    }

    /// Whether another load was asked for while the one that landed ran
    /// (and forget that it was).
    fn take_dirty(&mut self) -> bool {
        self.in_flight.is_none() && std::mem::take(&mut self.dirty)
    }
}

/// The dashboard state.
pub struct App {
    pub mgr: Manager,
    pub home: PathBuf,
    pub theme: Theme,

    pub vms: Vec<VmEntry>,
    pub templates: Vec<TemplateEntry>,
    /// The first load has not come back yet.
    pub loading: bool,
    /// Listing the VMs failed; shown in place of the VM list.
    pub vm_list_err: Option<String>,
    /// Listing the templates failed; shown in place of the templates list.
    pub tpl_list_err: Option<String>,
    pub vm_cursor: Cursor,
    pub tpl_cursor: Cursor,
    pub focus: Focus,
    /// Visible rows of the two lists at the last draw, for page moves.
    pub vm_rows: usize,
    pub tpl_rows: usize,

    /// The selected VM's refreshed state; `None` until the first refresh
    /// after a selection change comes back.
    pub details: Option<Details>,
    pub console: ConsoleView,

    panel: Option<OpenPanel>,
    next_panel: u64,
    pub popup: Option<Popup>,
    pub notice: Option<Notice>,
    /// A dashboard action in flight (`starting debian-12…`).
    pub busy: Option<String>,
    /// The id of the action `busy` describes: only its result clears it.
    busy_id: Option<u64>,
    next_action: u64,

    tasks: Tasks,
    rx: Receiver<(Target, TaskResult)>,
    pub tick: u64,
    last_refresh: Instant,
    list_load: Load,
    details_load: Load,
    /// A row to select, honoured by the first list load started after the
    /// request (the number is the last list load started before it).
    pending_select: Option<(Select, u64)>,
    suspend: Option<Suspend>,
    /// Quitting was asked for while something was in flight: quit as soon
    /// as it is done, unless it fails or the user stays. The Quitting popup
    /// is up meanwhile.
    quit_pending: bool,
    quit: bool,
}

impl App {
    /// A dashboard over the configured storage directory. Nothing is loaded
    /// until the loop starts.
    pub fn new(cfg: AppConfig) -> Self {
        let (tasks, rx) = Tasks::new();
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        App {
            mgr: Manager::new(cfg.vm_storage_path),
            home,
            theme: Theme::default(),
            vms: Vec::new(),
            templates: Vec::new(),
            loading: true,
            vm_list_err: None,
            tpl_list_err: None,
            vm_cursor: Cursor::default(),
            tpl_cursor: Cursor::default(),
            focus: Focus::Vms,
            vm_rows: 1,
            tpl_rows: 1,
            details: None,
            console: ConsoleView::new(),
            panel: None,
            next_panel: 1,
            popup: None,
            notice: None,
            busy: None,
            busy_id: None,
            next_action: 0,
            tasks,
            rx,
            tick: 0,
            last_refresh: Instant::now(),
            list_load: Load::default(),
            details_load: Load::default(),
            pending_select: None,
            suspend: None,
            quit_pending: false,
            quit: false,
        }
    }

    /// The storage directory.
    pub fn storage(&self) -> &Path {
        self.mgr.storage()
    }

    /// The VM under the cursor.
    pub fn selected_vm(&self) -> Option<&VmEntry> {
        self.vms.get(self.vm_cursor.index)
    }

    /// The template under the cursor.
    pub fn selected_template(&self) -> Option<&TemplateEntry> {
        self.templates.get(self.tpl_cursor.index)
    }

    /// Whether the selected VM is running, as far as the last refresh knows
    /// (details first, the list's status otherwise).
    pub fn selected_running(&self) -> bool {
        self.selected_status().running()
    }

    /// The selected VM's process state, as far as the last refresh knows.
    fn selected_status(&self) -> ProcessInfo {
        match (&self.details, self.selected_vm()) {
            (Some(d), Some(v)) if d.name == v.cfg.name => d.status,
            (_, Some(v)) => v.status,
            _ => ProcessInfo::default(),
        }
    }

    /// The title of the open panel, if any.
    pub fn panel_title(&self) -> Option<String> {
        self.panel.as_ref().map(|p| p.panel.title())
    }

    /// What is in flight: a dashboard action or the open panel's work. Quit
    /// waits while there is something; the status bar shows only
    /// [`App::busy`], since a panel draws its own progress.
    pub fn busy_label(&self) -> Option<String> {
        self.busy
            .clone()
            .or_else(|| self.panel.as_ref().and_then(|p| p.panel.busy()))
    }

    /// The key hints for the bottom bar.
    pub fn key_hints(&self) -> Vec<(String, String)> {
        if let Some(p) = &self.popup {
            return popups::key_hints(p);
        }
        if let Some(op) = &self.panel {
            return op.panel.key_hints();
        }
        dashboard::key_hints(self)
    }

    // -----------------------------------------------------------------------
    // Event loop
    // -----------------------------------------------------------------------

    fn main_loop(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        self.reload();
        let mut last_tick = Instant::now();
        while !self.quit {
            terminal.draw(|f| self.draw(f))?;

            let timeout = TICK.saturating_sub(last_tick.elapsed());
            if event::poll(timeout)? {
                match event::read()? {
                    event::Event::Key(k) if k.kind == KeyEventKind::Press => {
                        self.on_event(Event::Key(k))
                    }
                    event::Event::Paste(s) => self.on_event(Event::Paste(s)),
                    event::Event::Resize(w, h) => self.on_event(Event::Resize(w, h)),
                    _ => {}
                }
            }
            if last_tick.elapsed() >= TICK {
                last_tick = Instant::now();
                self.on_event(Event::Tick);
            }
            while let Ok((target, result)) = self.rx.try_recv() {
                self.on_event(Event::Task(target, result));
            }
            for _ in 0..take_quit_signals() {
                self.on_event(Event::QuitSignal);
            }
            if let Some(s) = self.suspend.take() {
                if !self.quit {
                    self.suspend_and_run(terminal, s)?;
                }
            }
        }
        Ok(())
    }

    /// Handles one event. Public so tests can drive the dashboard without a
    /// terminal.
    pub fn on_event(&mut self, ev: Event) {
        match ev {
            Event::Key(key) => self.on_key(key),
            Event::Paste(text) => self.on_paste(&text),
            Event::Resize(_, _) => {}
            Event::Tick => self.on_tick(),
            Event::Task(target, result) => self.on_task(target, result),
            Event::QuitSignal => self.request_quit(true),
        }
        if self.quit_pending {
            match self.busy_label() {
                None => self.quit = true,
                // Still waiting: the popup names what for, now.
                Some(label) => {
                    if let Some(Popup::Quitting { label: shown }) = &mut self.popup {
                        *shown = label;
                    }
                }
            }
        }
    }

    fn on_tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        if self.last_refresh.elapsed() >= Duration::from_secs(CONSOLE_POLL_SECONDS) {
            self.reload();
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.request_quit(true);
            return;
        }
        if self.popup.is_some() {
            self.on_popup_key(key);
            return;
        }
        if self.panel.is_some() {
            self.with_panel(|panel, ctx| panel.handle_key(key, ctx));
            return;
        }
        self.on_dashboard_key(key);
    }

    fn on_paste(&mut self, text: &str) {
        if self.popup.is_some() {
            return;
        }
        if self.panel.is_some() {
            self.with_panel(|panel, ctx| panel.handle_paste(text, ctx));
        }
    }

    fn on_task(&mut self, target: Target, result: TaskResult) {
        match target {
            Target::Panel(id) => {
                if self.panel.as_ref().is_some_and(|p| p.id == id) {
                    self.with_panel(|panel, ctx| panel.on_task(result, ctx));
                }
            }
            Target::List(seq) => self.on_list_result(seq, result),
            Target::Details(seq) => self.on_details_result(seq, result),
            Target::Action(id) => self.on_action_result(Some(id), result),
            Target::Launch => self.on_action_result(None, result),
        }
    }

    fn on_list_result(&mut self, seq: u64, result: TaskResult) {
        let fresh = self.list_load.land(seq);
        match result {
            TaskResult::Loaded {
                vms,
                templates,
                vm_err,
                tpl_err,
            } if fresh => self.apply_lists(seq, vms, templates, vm_err, tpl_err),
            // The listing itself failed (it panicked, or got no thread).
            TaskResult::Failed { what, err } if fresh => {
                self.loading = false;
                self.vm_list_err = Some(failure_text(&what, &err));
            }
            _ => {}
        }
        if self.list_load.take_dirty() {
            self.reload_lists();
        }
    }

    fn apply_lists(
        &mut self,
        seq: u64,
        vms: Vec<VmEntry>,
        templates: Vec<TemplateEntry>,
        vm_err: Option<String>,
        tpl_err: Option<String>,
    ) {
        self.loading = false;
        self.vm_list_err = vm_err;
        self.tpl_list_err = tpl_err;
        let prev_cfg = self.selected_vm().map(|v| v.cfg.clone());
        let prev_vm = prev_cfg.as_ref().map(|c| c.name.clone());
        let prev_tpl = self.selected_template().map(|t| t.tpl.name.clone());
        self.vms = vms;
        self.templates = templates;
        // A listing taken before the request may not have the new name yet.
        let select = match &self.pending_select {
            Some((_, after)) if seq > *after => self.pending_select.take().map(|(sel, _)| sel),
            _ => None,
        };
        let (want_vm, want_tpl) = match select {
            Some(Select::Vm(n)) => (Some(n), prev_tpl),
            Some(Select::Template(n)) => {
                self.focus = Focus::Templates;
                (prev_vm.clone(), Some(n))
            }
            None => (prev_vm.clone(), prev_tpl),
        };
        if let Some(i) = want_vm.and_then(|n| self.vms.iter().position(|v| v.cfg.name == n)) {
            self.vm_cursor.index = i;
        }
        if let Some(i) = want_tpl.and_then(|n| self.templates.iter().position(|t| t.tpl.name == n))
        {
            self.tpl_cursor.index = i;
        }
        self.vm_cursor.clamp(self.vms.len());
        self.tpl_cursor.clamp(self.templates.len());
        // The same VM keeps its details, which reload() is fetching anew
        // already; another VM under the cursor needs its own, and so does a
        // config edited since (that pass used the old one).
        let now_cfg = self.selected_vm().map(|v| &v.cfg);
        if now_cfg.map(|c| &c.name) != prev_vm.as_ref() {
            self.selection_changed();
        } else if now_cfg != prev_cfg.as_ref() {
            self.reload_details();
        }
    }

    fn on_details_result(&mut self, seq: u64, result: TaskResult) {
        let fresh = self.details_load.land(seq);
        match result {
            TaskResult::Details(d) if fresh => {
                if self.selected_vm().is_some_and(|v| v.cfg.name == d.name) {
                    // The console tail moves into the view, the rest is kept.
                    let mut d = *d;
                    self.console.set_lines(std::mem::take(&mut d.console));
                    self.details = Some(d);
                }
            }
            // Say so, and leave the retry to the next refresh: retrying at
            // once would spin on a pass that fails every time.
            TaskResult::Failed { what, err } if fresh => {
                self.set_notice(Notice::Err(failure_text(&what, &err)));
            }
            _ => {}
        }
        if self.details_load.take_dirty() {
            self.reload_details();
        }
    }

    /// The result of a start, stop or delete (`id`), or of a VNC launch
    /// (`None`). The failure of the action a quit waits for calls the quit
    /// off, so the error is seen.
    fn on_action_result(&mut self, id: Option<u64>, result: TaskResult) {
        let awaited = id.is_some() && id == self.busy_id;
        if awaited {
            self.busy = None;
            self.busy_id = None;
        }
        match result {
            TaskResult::Done { what } => self.set_notice(Notice::Ok(what)),
            TaskResult::Failed { what, err } => {
                if awaited {
                    self.cancel_quit();
                }
                self.set_notice(Notice::Err(failure_text(&what, &err)))
            }
            _ => {}
        }
        // A launched viewer changes nothing the dashboard shows.
        if id.is_some() {
            self.reload();
        }
    }

    // -----------------------------------------------------------------------
    // Loading
    // -----------------------------------------------------------------------

    /// Reloads the lists and the selected VM's details in the background.
    pub fn reload(&mut self) {
        self.last_refresh = Instant::now();
        self.reload_lists();
        self.reload_details();
    }

    /// Reloads the VM and template lists in the background, or, with a
    /// reload already running, once that one is back.
    fn reload_lists(&mut self) {
        if let Some(seq) = self.list_load.begin() {
            let mgr = self.mgr.clone();
            self.tasks
                .spawn(Target::List(seq), move || actions::load_all(&mgr));
        }
    }

    /// Refreshes the selected VM's details and console in the background,
    /// or, with a refresh already running, once that one is back.
    pub fn reload_details(&mut self) {
        let Some(cfg) = self.selected_vm().map(|v| v.cfg.clone()) else {
            return;
        };
        if let Some(seq) = self.details_load.begin() {
            let storage = self.storage().to_path_buf();
            self.tasks.spawn(Target::Details(seq), move || {
                actions::load_details(&storage, &cfg)
            });
        }
    }

    /// After the VM cursor moved: forget the old VM's details and fetch the new one's.
    fn selection_changed(&mut self) {
        self.details = None;
        self.console.set_lines(Vec::new());
        self.console.goto_bottom();
        self.reload_details();
    }

    /// Runs a start, stop or delete in the background with `label` in the
    /// status bar until its own result comes back.
    fn spawn_action<F>(&mut self, label: String, f: F)
    where
        F: FnOnce() -> TaskResult + Send + 'static,
    {
        self.next_action += 1;
        let id = self.next_action;
        self.busy = Some(label);
        self.busy_id = Some(id);
        self.tasks.spawn(Target::Action(id), f);
    }

    /// q, Ctrl-c, or a quit signal. With a start, stop or delete, or a
    /// panel's work, in flight, quitting now would cut it short (a
    /// half-deleted or half-renamed VM): wait under the Quitting popup, and
    /// quit as soon as it is done. Asking again with `force` (Ctrl-c on the
    /// popup, a second signal) quits anyway.
    fn request_quit(&mut self, force: bool) {
        match self.busy_label() {
            None => self.quit = true,
            Some(_) if force && self.quit_pending => self.quit = true,
            Some(label) => {
                self.quit_pending = true;
                self.popup = Some(Popup::quitting(label));
            }
        }
    }

    /// Calls a pending quit off, Quitting popup and all: the work it waited
    /// for failed, or the user chose to stay. Quitting can be asked again.
    fn cancel_quit(&mut self) {
        self.quit_pending = false;
        if matches!(self.popup, Some(Popup::Quitting { .. })) {
            self.popup = None;
        }
    }

    // -----------------------------------------------------------------------
    // Dashboard keys
    // -----------------------------------------------------------------------

    fn on_dashboard_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('q') if !ctrl => {
                self.request_quit(false);
                return;
            }
            KeyCode::Char('?') => {
                self.popup = Some(Popup::help());
                return;
            }
            KeyCode::Tab => {
                self.cycle_focus(1);
                return;
            }
            KeyCode::BackTab => {
                self.cycle_focus(-1);
                return;
            }
            KeyCode::Esc => {
                // Esc backs out of the templates or console pane to the VM
                // list (Go's `q/h/Esc: back`); on the VM list it clears the
                // notice.
                if self.focus != Focus::Vms {
                    self.focus = Focus::Vms;
                } else {
                    self.notice = None;
                }
                return;
            }
            KeyCode::Char('r') if !ctrl => {
                self.notice = None;
                self.reload();
                return;
            }
            KeyCode::Char('T') => {
                self.focus = Focus::Templates;
                return;
            }
            KeyCode::Char('n') if !ctrl && self.focus != Focus::Templates => {
                self.open_panel(Box::new(CreateForm::new()));
                return;
            }
            _ => {}
        }

        match self.focus {
            Focus::Vms => {
                let before = self.vm_cursor.index;
                if self.vm_cursor.handle_key(key, self.vms.len(), self.vm_rows) {
                    if self.vm_cursor.index != before {
                        self.selection_changed();
                    }
                    return;
                }
                match key.code {
                    KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                        if self.selected_vm().is_some() {
                            self.focus = Focus::Console;
                        }
                    }
                    _ => self.vm_action_key(key),
                }
            }
            Focus::Templates => {
                if self
                    .tpl_cursor
                    .handle_key(key, self.templates.len(), self.tpl_rows)
                {
                    return;
                }
                match key.code {
                    KeyCode::Enter | KeyCode::Char('l') | KeyCode::Char('n') | KeyCode::Right => {
                        match self.selected_template() {
                            Some(t) => {
                                let name = t.tpl.name.clone();
                                self.open_from_template(&name);
                            }
                            // No templates yet: the plain create form, as
                            // the card's "Press n to create one" promises.
                            None => self.open_panel(Box::new(CreateForm::new())),
                        }
                    }
                    // One dashboard action at a time, like s and x.
                    KeyCode::Char('d') if !ctrl && self.busy.is_none() => {
                        if let Some(t) = self.selected_template() {
                            let name = t.tpl.name.clone();
                            self.popup = Some(Popup::confirm(
                                format!(
                                    "Delete template {name:?}? VMs made from it are not affected."
                                ),
                                ConfirmAction::DeleteTemplate(name),
                            ));
                        }
                    }
                    KeyCode::Char('h') | KeyCode::Left => self.focus = Focus::Vms,
                    _ => {}
                }
            }
            Focus::Console => {
                if self.console.handle_key(key) {
                    return;
                }
                match key.code {
                    KeyCode::Char('h') | KeyCode::Left => self.focus = Focus::Vms,
                    _ => self.vm_action_key(key),
                }
            }
        }
    }

    /// The keys that act on the selected VM, shared by the VM and console panes.
    fn vm_action_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return;
        }
        let Some(entry) = self.selected_vm().cloned() else {
            return;
        };
        let name = entry.cfg.name.clone();
        let status = self.selected_status();
        let running = status.running();
        match key.code {
            KeyCode::Char('s') => {
                if running {
                    self.set_notice(Notice::Err(format!(
                        "VM {name:?} is already running (PID {})",
                        status.pid
                    )));
                } else if self.busy.is_none() {
                    // The config as it is now, not as of the last refresh:
                    // a hand edit made just before s counts.
                    let Some(cfg) = self.fresh_config(&name) else {
                        return;
                    };
                    let storage = self.storage().to_path_buf();
                    self.spawn_action(format!("starting {name}…"), move || {
                        actions::start_vm(&storage, &cfg)
                    });
                }
            }
            KeyCode::Char('x') => {
                if !running {
                    self.set_notice(Notice::Err(format!("{name} is not running")));
                } else if self.busy.is_none() {
                    let storage = self.storage().to_path_buf();
                    let label = format!("stopping {name}…");
                    self.spawn_action(label, move || actions::stop_vm(&storage, &name));
                }
            }
            KeyCode::Char('d') => {
                // One dashboard action at a time, like s and x.
                if self.busy.is_none() {
                    self.popup = Some(Popup::confirm(
                        format!("Delete {name:?} and all of its disks?"),
                        ConfirmAction::DeleteVm(name),
                    ));
                }
            }
            // The forms and dialogs start from the VM's state right now,
            // like Go's constructors (vm.Status), not from the last refresh.
            KeyCode::Char('e') => {
                if let Some(cfg) = self.fresh_config(&name) {
                    let running = self.fresh_running(&name);
                    self.open_panel(Box::new(EditForm::new(cfg, running)));
                }
            }
            KeyCode::Char('u') => {
                if let Some(cfg) = self.fresh_config(&name) {
                    let running = self.fresh_running(&name);
                    self.open_panel(Box::new(UsbDialog::new(cfg, running)));
                }
            }
            KeyCode::Char('i') => {
                if let Some(cfg) = self.fresh_config(&name) {
                    let running = self.fresh_running(&name);
                    self.open_panel(Box::new(IsoDialog::new(cfg, running)));
                }
            }
            KeyCode::Char('t') => {
                if let Some(cfg) = self.fresh_config(&name) {
                    let running = self.fresh_running(&name);
                    self.open_panel(Box::new(SaveTemplateForm::new(cfg, running)));
                }
            }
            KeyCode::Char('c') => {
                match actions::serial_console_command(self.storage(), &name, running) {
                    Ok(cmd) => {
                        self.suspend = Some(Suspend {
                            cmd,
                            done: "disconnected from serial console".to_string(),
                        });
                    }
                    Err(err) => self.set_notice(Notice::Err(err)),
                }
            }
            KeyCode::Char('v') => {
                let cfg = entry.cfg.clone();
                self.tasks.spawn(Target::Launch, move || {
                    actions::launch_vnc_viewer(&cfg, running)
                });
            }
            _ => {}
        }
    }

    /// Whether the VM is running right now, from its PID file.
    fn fresh_running(&self, name: &str) -> bool {
        vm::status(self.storage(), name).is_ok_and(|s| s.running())
    }

    /// Re-reads a VM's `vm.yaml` right before a form opens on it, so the form
    /// starts from what is on disk. A VM that is gone reports an error.
    fn fresh_config(&mut self, name: &str) -> Option<vm::VmConfig> {
        match vm::load_config(self.storage(), name) {
            Ok(cfg) => Some(cfg),
            Err(err) => {
                self.set_notice(Notice::Err(format!("load {name}: {err:#}")));
                self.reload();
                None
            }
        }
    }

    fn open_from_template(&mut self, name: &str) {
        match vm::load_template(self.storage(), name) {
            Ok(tpl) => self.open_panel(Box::new(FromTemplateForm::new(tpl))),
            Err(err) => {
                self.set_notice(Notice::Err(format!("load template {name}: {err:#}")));
                self.reload();
            }
        }
    }

    fn cycle_focus(&mut self, delta: i32) {
        let order = [Focus::Vms, Focus::Templates, Focus::Console];
        let i = order.iter().position(|f| *f == self.focus).unwrap_or(0) as i32;
        let n = order.len() as i32;
        self.focus = order[(((i + delta) % n + n) % n) as usize];
    }

    // -----------------------------------------------------------------------
    // Panels
    // -----------------------------------------------------------------------

    /// Opens a panel in the right-hand column and runs its `init`.
    pub fn open_panel(&mut self, panel: Box<dyn Panel>) {
        let id = PanelId(self.next_panel);
        self.next_panel += 1;
        self.notice = None;
        self.panel = Some(OpenPanel { id, panel });
        self.with_panel(|panel, ctx| panel.init(ctx));
    }

    /// Closes the open panel and refreshes.
    fn close_panel(&mut self) {
        self.panel = None;
        self.reload();
    }

    /// Runs `f` on the open panel with a fresh context, then applies the
    /// actions it pushed. When that ends the panel's work in an error, a
    /// quit waiting for the work is called off first.
    fn with_panel<F>(&mut self, f: F)
    where
        F: FnOnce(&mut dyn Panel, &mut Ctx),
    {
        let Some(mut op) = self.panel.take() else {
            return;
        };
        let was_busy = op.panel.busy().is_some();
        let size = Rect::new(0, 0, 0, 0);
        let actions = {
            let mut ctx = Ctx::new(&self.mgr, &self.home, size, self.tick, &self.tasks, op.id);
            f(op.panel.as_mut(), &mut ctx);
            std::mem::take(&mut ctx.actions)
        };
        let failed = was_busy && op.panel.busy().is_none() && op.panel.failed();
        // Put the panel back unless an action closes it.
        self.panel = Some(op);
        if failed {
            self.cancel_quit();
        }
        for action in actions {
            self.apply_action(action);
        }
    }

    fn apply_action(&mut self, action: Action) {
        match action {
            Action::Close => self.close_panel(),
            Action::CloseSelectVm(name) => {
                self.pending_select = Some((Select::Vm(name), self.list_load.started));
                self.focus = Focus::Vms;
                self.close_panel();
            }
            Action::CloseSelectTemplate(name) => {
                self.pending_select = Some((Select::Template(name), self.list_load.started));
                self.close_panel();
            }
            Action::Notice(n) => self.set_notice(n),
            Action::Refresh => self.reload(),
            Action::Quit => self.request_quit(false),
        }
    }

    // -----------------------------------------------------------------------
    // Popups and notices
    // -----------------------------------------------------------------------

    /// Shows a notice in the status bar; a multi-line error also opens a
    /// popup with the whole text, since the bar has one line. While a quit
    /// waits the Quitting popup stays up and only the bar has the error:
    /// taking the popup's place would leave the quit pending unseen.
    pub fn set_notice(&mut self, notice: Notice) {
        if let Notice::Err(text) = &notice {
            if text.lines().count() > 1 && !self.quit_pending {
                self.popup = Some(Popup::message("Error", text.clone()));
            }
        }
        self.notice = Some(notice);
    }

    fn on_popup_key(&mut self, key: KeyEvent) {
        let Some(popup) = self.popup.take() else {
            return;
        };
        match popups::handle_key(popup, key) {
            popups::KeyOutcome::Keep(p) => self.popup = Some(p),
            popups::KeyOutcome::Close => {}
            popups::KeyOutcome::Confirm(action) => self.run_confirmed(action),
            popups::KeyOutcome::Stay => {
                self.cancel_quit();
                self.set_notice(Notice::Ok("quit cancelled".to_string()));
            }
        }
    }

    fn run_confirmed(&mut self, action: ConfirmAction) {
        // One dashboard action at a time, like s and x.
        if self.busy.is_some() {
            return;
        }
        let mgr = self.mgr.clone();
        match action {
            ConfirmAction::DeleteVm(name) => {
                let label = format!("deleting {name}…");
                self.spawn_action(label, move || actions::delete_vm(&mgr, &name));
            }
            ConfirmAction::DeleteTemplate(name) => {
                let label = format!("deleting template {name}…");
                self.spawn_action(label, move || actions::delete_template(&mgr, &name));
            }
        }
    }

    // -----------------------------------------------------------------------
    // Suspending for an interactive command
    // -----------------------------------------------------------------------

    fn suspend_and_run(&mut self, terminal: &mut DefaultTerminal, mut s: Suspend) -> Result<()> {
        default_job_signals_in_child(&mut s.cmd);
        let (status, back) = {
            // Like system(3): while socat has the terminal, Ctrl-C and
            // friends are for it (and the guest), not for Ostrich.
            let _ignored = SignalGuard::set(&JOB_SIGNALS, SigHandler::SigIgn);
            let status = leave_tui(terminal).and_then(|()| s.cmd.status());
            (status, enter_tui(terminal))
        };
        match status {
            // socat exits with status 1 on a normal Ctrl-] disconnect.
            Ok(_) => self.set_notice(Notice::Ok(s.done)),
            Err(err) => {
                self.set_notice(Notice::Err(format!("run {:?}: {err}", s.cmd.get_program())))
            }
        }
        self.reload();
        back
    }

    // -----------------------------------------------------------------------
    // Drawing
    // -----------------------------------------------------------------------

    /// Draws the whole screen.
    pub fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
            frame.render_widget(
                Paragraph::new("terminal too small")
                    .style(self.theme.help)
                    .alignment(Alignment::Center),
                area,
            );
            return;
        }
        let [main, status, hints] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(area);
        let left_w = left_width(area.width);
        let [left, right] =
            Layout::horizontal([Constraint::Length(left_w), Constraint::Fill(1)]).areas(main);

        // Left column: VMs on top, templates below, sized to the template count.
        let tpl_h = (self.templates.len() as u16 + 2)
            .clamp(TPL_PANE_MIN, (left.height / 2).max(TPL_PANE_MIN));
        let [vm_area, tpl_area] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(tpl_h)]).areas(left);
        self.vm_rows = dashboard::render_vm_list(self, frame, vm_area);
        self.tpl_rows = dashboard::render_templates(self, frame, tpl_area);

        // Right column: the open panel, or details over the console.
        if let Some(mut op) = self.panel.take() {
            op.panel.render(frame, right, &self.theme, self.tick);
            self.panel = Some(op);
        } else {
            let (title, lines) =
                if self.focus == Focus::Templates && self.selected_template().is_some() {
                    dashboard::template_details(self)
                } else {
                    dashboard::vm_details(self)
                };
            let details_h = details_height(lines.len() as u16 + 2, right.height);
            let [d_area, c_area] =
                Layout::vertical([Constraint::Length(details_h), Constraint::Fill(1)]).areas(right);
            let focused_details = self.focus != Focus::Console;
            dashboard::render_details(self, frame, d_area, title, lines, focused_details);
            let console_title = dashboard::console_title_fit(self, c_area.width);
            let focused = self.focus == Focus::Console;
            let theme = self.theme.clone();
            self.console
                .render(frame, c_area, &theme, console_title, focused);
        }

        dashboard::render_status_bar(self, frame, status);
        dashboard::render_hints(self, frame, hints);

        if let Some(popup) = &mut self.popup {
            popups::render(popup, frame, &self.theme);
        }
    }
}

/// Leaves the dashboard for an interactive command: cooked mode, the main
/// screen, the cursor shown (ratatui hides it on every frame).
fn leave_tui(terminal: &mut DefaultTerminal) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(stdout(), DisableBracketedPaste, LeaveAlternateScreen)?;
    terminal.show_cursor()
}

/// Back to the dashboard after an interactive command. Every step is tried
/// even when one fails, so a command that never started still gets the
/// alternate screen, bracketed paste, raw mode and the hidden cursor back.
fn enter_tui(terminal: &mut DefaultTerminal) -> Result<()> {
    let screen = execute!(stdout(), EnterAlternateScreen, EnableBracketedPaste);
    let raw = enable_raw_mode();
    let cursor = terminal.hide_cursor();
    let clear = terminal.clear();
    screen.and(raw).and(cursor).and(clear)?;
    Ok(())
}

/// The left column's width: a third of a wide terminal (30–54 cells); below
/// 90 columns 40 % (24–30 cells), so the right column stays usable down to
/// 60 columns.
fn left_width(width: u16) -> u16 {
    if width < 90 {
        (width * 40 / 100).clamp(24, 30)
    } else {
        (width * 34 / 100).clamp(30, 54)
    }
}

/// The details card's height: the rows it wants, up to 70 % of the right
/// column, at least 3.
fn details_height(wanted: u16, right_height: u16) -> u16 {
    wanted.min(right_height * 70 / 100).max(3)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::AtomicUsize;

    use crossterm::event::KeyCode;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::tui::testutil::*;
    use crate::vm::{save_config, vm_dir, VmConfig, VmStatus};

    /// A dashboard over a scratch storage directory. Nothing is loaded:
    /// tests feed it the results its background tasks would deliver.
    fn test_app() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = AppConfig {
            vm_storage_path: dir.path().to_string_lossy().into_owned(),
            recent_isos: Vec::new(),
        };
        (dir, App::new(cfg))
    }

    fn running(pid: i32) -> ProcessInfo {
        ProcessInfo {
            pid,
            status: VmStatus::Running,
        }
    }

    fn vm_entry(name: &str, is_running: bool) -> VmEntry {
        VmEntry {
            cfg: VmConfig {
                name: name.to_string(),
                cpu: 2,
                ram: 2048,
                disk_size: 20,
                ..VmConfig::default()
            },
            status: if is_running {
                running(4242)
            } else {
                ProcessInfo::default()
            },
        }
    }

    fn tpl_entry(name: &str) -> TemplateEntry {
        TemplateEntry {
            tpl: vm::Template {
                name: name.to_string(),
                description: String::new(),
                source_vm: "src".to_string(),
                cpu: 1,
                ram: 512,
                disk_size: 8,
                arch: "x86_64".to_string(),
                firmware: vm::FirmwareType::Bios,
                secure_boot: false,
                tpm: false,
                network: vm::NetworkType::User,
                vnc: false,
                created_at: "2026-10-09T13:33:00Z".parse().expect("time"),
            },
            disk_usage: 0,
        }
    }

    fn loaded(names: &[&str]) -> TaskResult {
        TaskResult::Loaded {
            vms: names.iter().map(|n| vm_entry(n, false)).collect(),
            templates: Vec::new(),
            vm_err: None,
            tpl_err: None,
        }
    }

    fn details_for(name: &str, status: ProcessInfo) -> TaskResult {
        TaskResult::Details(Box::new(Details {
            name: name.to_string(),
            status,
            console: vec![format!("{name} says hi")],
            ..Details::default()
        }))
    }

    fn failed(what: &str, err: &str) -> TaskResult {
        TaskResult::Failed {
            what: what.to_string(),
            err: err.to_string(),
        }
    }

    fn done(what: &str) -> TaskResult {
        TaskResult::Done {
            what: what.to_string(),
        }
    }

    fn deliver(app: &mut App, target: Target, result: TaskResult) {
        app.on_event(Event::Task(target, result));
    }

    fn names(app: &App) -> Vec<String> {
        app.vms.iter().map(|v| v.cfg.name.clone()).collect()
    }

    fn selected(app: &App) -> Option<String> {
        app.selected_vm().map(|v| v.cfg.name.clone())
    }

    /// Makes the next tick due for the 2 s refresh.
    fn refresh_due(app: &mut App) {
        app.last_refresh = Instant::now()
            .checked_sub(Duration::from_secs(CONSOLE_POLL_SECONDS + 1))
            .expect("a monotonic clock past a few seconds");
    }

    fn draw(app: &mut App, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal.draw(|f| app.draw(f)).expect("draw");
        buffer_lines(terminal.backend().buffer())
    }

    /// Waits for the result of a real background task addressed to
    /// `target`, dropping the others.
    fn wait_for(app: &App, target: Target) -> TaskResult {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok((t, r)) = app.rx.recv_timeout(Duration::from_millis(100)) {
                if t == target {
                    return r;
                }
            }
        }
        panic!("no result for {target:?}");
    }

    // --- refresh ------------------------------------------------------------

    #[test]
    fn a_list_result_older_than_the_applied_one_is_dropped() {
        let (_dir, mut app) = test_app();
        app.reload();
        assert_eq!(app.list_load.in_flight, Some(1));
        deliver(&mut app, Target::List(1), loaded(&["a"]));
        app.reload();
        deliver(&mut app, Target::List(2), loaded(&["a", "b"]));
        // A late copy of the older listing changes nothing.
        deliver(&mut app, Target::List(1), loaded(&["a"]));
        assert_eq!(names(&app), ["a", "b"]);
        assert_eq!(app.list_load.in_flight, None);
    }

    #[test]
    fn a_refresh_while_one_runs_waits_for_it_and_runs_once() {
        let (_dir, mut app) = test_app();
        app.reload();
        for _ in 0..3 {
            refresh_due(&mut app);
            app.on_event(Event::Tick);
        }
        assert_eq!(
            app.list_load.started, 1,
            "no listing on top of the running one"
        );
        assert!(app.list_load.dirty);
        deliver(&mut app, Target::List(1), loaded(&["a"]));
        assert_eq!(app.list_load.started, 2, "one more once it is back");
        assert_eq!(app.list_load.in_flight, Some(2));
        deliver(&mut app, Target::List(2), loaded(&["a"]));
        assert_eq!(app.list_load.started, 2, "and no more after that");
        assert_eq!(app.list_load.in_flight, None);
    }

    #[test]
    fn a_selection_waits_for_a_listing_started_after_the_request() {
        let (_dir, mut app) = test_app();
        app.reload();
        deliver(&mut app, Target::List(1), loaded(&["a", "c"]));
        // A tick's listing is under way when the create form closes on "b".
        app.reload();
        app.apply_action(Action::CloseSelectVm("b".into()));
        // That listing predates "b": it must not use up the request.
        deliver(&mut app, Target::List(2), loaded(&["a", "c"]));
        assert!(app.pending_select.is_some());
        assert_eq!(selected(&app).as_deref(), Some("a"));
        assert_eq!(
            app.list_load.in_flight,
            Some(3),
            "the close's reload runs once the tick's is back"
        );
        deliver(&mut app, Target::List(3), loaded(&["a", "b", "c"]));
        assert_eq!(selected(&app).as_deref(), Some("b"));
        assert!(app.pending_select.is_none());
    }

    #[test]
    fn a_template_selection_waits_the_same_way() {
        let (_dir, mut app) = test_app();
        app.reload();
        app.apply_action(Action::CloseSelectTemplate("base".into()));
        // The listing started before the request does not apply it.
        deliver(&mut app, Target::List(1), loaded(&[]));
        assert!(app.pending_select.is_some());
        assert_eq!(app.focus, Focus::Vms);
        assert_eq!(app.list_load.in_flight, Some(2));
        deliver(&mut app, Target::List(2), loaded(&[]));
        assert!(app.pending_select.is_none());
        assert_eq!(app.focus, Focus::Templates);
    }

    #[test]
    fn a_listing_does_not_fetch_the_details_a_second_time() {
        let (_dir, mut app) = test_app();
        app.reload();
        deliver(&mut app, Target::List(1), loaded(&["a"]));
        // The first listing put a VM under the cursor: its details are due.
        assert_eq!(app.details_load.started, 1);
        deliver(&mut app, Target::Details(1), details_for("a", running(7)));
        app.reload();
        assert_eq!(app.details_load.started, 2);
        deliver(&mut app, Target::List(2), loaded(&["a"]));
        assert_eq!(
            app.details_load.started, 2,
            "the same VM: reload() asked already"
        );
        assert!(!app.details_load.dirty);
        assert!(app.details.is_some(), "kept until the fresh ones land");
    }

    #[test]
    fn an_edited_config_fetches_the_details_again_once() {
        let (_dir, mut app) = test_app();
        app.reload();
        deliver(&mut app, Target::List(1), loaded(&["a"]));
        deliver(&mut app, Target::Details(1), details_for("a", running(7)));
        // The edit form closes: reload() fetches details with the old config.
        app.reload();
        assert_eq!(app.details_load.in_flight, Some(2));
        let mut edited = vm_entry("a", false);
        edited.cfg.cpu = 4;
        deliver(
            &mut app,
            Target::List(2),
            TaskResult::Loaded {
                vms: vec![edited],
                templates: Vec::new(),
                vm_err: None,
                tpl_err: None,
            },
        );
        assert!(app.details_load.dirty, "once more, with the new config");
        deliver(&mut app, Target::Details(2), details_for("a", running(7)));
        assert_eq!(app.details_load.in_flight, Some(3));
        assert!(!app.details_load.dirty);
    }

    #[test]
    fn details_of_a_vm_no_longer_selected_are_dropped_and_the_new_ones_fetched() {
        let (_dir, mut app) = test_app();
        app.reload();
        deliver(&mut app, Target::List(1), loaded(&["a", "b"]));
        assert_eq!(app.details_load.in_flight, Some(1));
        app.on_event(Event::Key(ch('j')));
        assert_eq!(selected(&app).as_deref(), Some("b"));
        assert_eq!(app.details_load.started, 1, "waits for a's pass");
        deliver(&mut app, Target::Details(1), details_for("a", running(7)));
        assert!(app.details.is_none(), "a's details are not b's");
        assert_eq!(app.details_load.in_flight, Some(2));
        deliver(&mut app, Target::Details(2), details_for("b", running(8)));
        assert_eq!(app.details.as_ref().map(|d| d.status.pid), Some(8));
        assert_eq!(app.console.lines(), ["b says hi"]);
    }

    #[test]
    fn a_details_result_older_than_the_applied_one_is_dropped() {
        let (_dir, mut app) = test_app();
        app.reload();
        deliver(&mut app, Target::List(1), loaded(&["a"]));
        deliver(&mut app, Target::Details(1), details_for("a", running(7)));
        app.reload();
        deliver(&mut app, Target::List(2), loaded(&["a"]));
        deliver(
            &mut app,
            Target::Details(2),
            details_for("a", ProcessInfo::default()),
        );
        // The pass from before the stop arrives late.
        deliver(&mut app, Target::Details(1), details_for("a", running(7)));
        assert!(!app.selected_running());
    }

    #[test]
    fn a_failing_details_pass_is_not_retried_at_once() {
        let (_dir, mut app) = test_app();
        app.reload();
        deliver(&mut app, Target::List(1), loaded(&["a"]));
        let lists = app.list_load.started;
        deliver(
            &mut app,
            Target::Details(1),
            failed("background task", "internal error: boom"),
        );
        assert_eq!(app.details_load.started, 1);
        assert_eq!(app.details_load.in_flight, None);
        assert_eq!(app.list_load.started, lists);
        assert_eq!(
            app.notice,
            Some(Notice::Err("background task: internal error: boom".into()))
        );
        // The next refresh tries again.
        refresh_due(&mut app);
        app.on_event(Event::Tick);
        assert_eq!(app.details_load.started, 2);
    }

    #[test]
    fn a_failing_listing_is_shown_in_place_of_the_list() {
        let (_dir, mut app) = test_app();
        app.reload();
        deliver(
            &mut app,
            Target::List(1),
            failed(
                "background task",
                "spawn thread: Resource temporarily unavailable",
            ),
        );
        assert!(!app.loading);
        assert_eq!(
            app.vm_list_err.as_deref(),
            Some("background task: spawn thread: Resource temporarily unavailable")
        );
        assert_eq!(app.list_load.started, 1, "no retry before the next tick");
        assert_eq!(app.list_load.in_flight, None);
    }

    // --- busy ---------------------------------------------------------------

    #[test]
    fn only_its_own_result_clears_the_busy_label() {
        let (_dir, mut app) = test_app();
        app.vms = vec![vm_entry("a", false), vm_entry("b", true)];
        app.spawn_action("starting a…".into(), || done("started a"));
        let id = app.busy_id.expect("an action id");
        // A VNC launch comes back while the start runs.
        deliver(
            &mut app,
            Target::Launch,
            done("launched vncviewer → port 5901"),
        );
        assert_eq!(app.busy.as_deref(), Some("starting a…"));
        // So s on a is still refused.
        app.on_event(Event::Key(ch('s')));
        assert_eq!(app.next_action, id, "no second start");
        // Another action's result leaves it alone too.
        deliver(&mut app, Target::Action(id + 1), done("stopped b"));
        assert_eq!(app.busy.as_deref(), Some("starting a…"));
        deliver(&mut app, Target::Action(id), done("started a"));
        assert_eq!(app.busy, None);
        assert_eq!(app.notice, Some(Notice::Ok("started a".into())));
    }

    /// s starts the VM as its vm.yaml says now, not as the last refresh
    /// listed it.
    #[test]
    fn start_reads_the_config_afresh() {
        let (_dir, mut app) = test_app();
        app.vms = vec![vm_entry("a", false)];
        write_vm(
            &app,
            &VmConfig {
                cdrom_path: "/nonexistent/edited-by-hand.iso".into(),
                ..app.vms[0].cfg.clone()
            },
        );
        app.on_event(Event::Key(ch('s')));
        let id = app.busy_id.expect("a start");
        match wait_for(&app, Target::Action(id)) {
            TaskResult::Failed { err, .. } => {
                assert!(err.contains("edited-by-hand.iso"), "{err}")
            }
            other => panic!("expected the edited ISO to fail the start: {other:?}"),
        }
    }

    #[test]
    fn a_vnc_launch_never_touches_busy() {
        let (_dir, mut app) = test_app();
        app.vms = vec![vm_entry("a", true)];
        app.spawn_action("stopping a…".into(), || done("stopped a"));
        // vnc_port 0: the launcher fails straight away, no viewer started.
        app.on_event(Event::Key(ch('v')));
        let result = wait_for(&app, Target::Launch);
        deliver(&mut app, Target::Launch, result);
        assert_eq!(app.busy.as_deref(), Some("stopping a…"));
        match &app.notice {
            Some(Notice::Err(text)) => assert!(
                text.ends_with("VNC is not enabled for this VM (vnc_port: 0)"),
                "{text}"
            ),
            other => panic!("unexpected notice {other:?}"),
        }
        assert_eq!(app.list_load.started, 0, "a launch reloads nothing");
    }

    #[test]
    fn delete_waits_for_the_running_action() {
        let (_dir, mut app) = test_app();
        app.vms = vec![vm_entry("a", false)];
        app.spawn_action("starting a…".into(), || done("started a"));
        app.on_event(Event::Key(ch('d')));
        assert!(app.popup.is_none(), "no delete while the start runs");
        // A confirmation already on screen starts nothing either.
        app.popup = Some(Popup::confirm(
            "Delete \"a\" and all of its disks?",
            ConfirmAction::DeleteVm("a".into()),
        ));
        app.on_event(Event::Key(ch('y')));
        assert_eq!(app.busy.as_deref(), Some("starting a…"));
        assert_eq!(app.next_action, 1);
        // The templates pane's d waits too.
        app.templates = vec![tpl_entry("base")];
        app.focus = Focus::Templates;
        app.on_event(Event::Key(ch('d')));
        assert!(
            app.popup.is_none(),
            "no template delete while the start runs"
        );
        // Once the start is back, d asks again.
        deliver(&mut app, Target::Action(1), done("started a"));
        app.on_event(Event::Key(ch('d')));
        assert!(matches!(app.popup, Some(Popup::Confirm { .. })));
        app.popup = None;
        app.focus = Focus::Vms;
        app.vms = vec![vm_entry("a", false)];
        app.on_event(Event::Key(ch('d')));
        assert!(matches!(app.popup, Some(Popup::Confirm { .. })));
    }

    #[test]
    fn s_on_a_running_vm_says_so_like_the_backend() {
        let (_dir, mut app) = test_app();
        app.vms = vec![vm_entry("deb", true)];
        app.on_event(Event::Key(ch('s')));
        assert_eq!(
            app.notice,
            Some(Notice::Err(
                "VM \"deb\" is already running (PID 4242)".into()
            ))
        );
        assert!(app.busy.is_none());
    }

    // --- quitting -----------------------------------------------------------

    #[test]
    fn q_quits_at_once_when_nothing_runs() {
        let (_dir, mut app) = test_app();
        app.on_event(Event::Key(ch('q')));
        assert!(app.quit);
    }

    #[test]
    fn q_waits_for_a_running_action_and_quits_when_it_is_done() {
        let (_dir, mut app) = test_app();
        app.spawn_action("deleting a…".into(), || done("deleted a"));
        let id = app.busy_id.expect("an action id");
        app.on_event(Event::Key(ch('q')));
        assert!(!app.quit);
        assert!(
            matches!(&app.popup, Some(Popup::Quitting { label }) if label == "deleting a…"),
            "{:?}",
            app.popup
        );
        // Other results do not end the wait.
        deliver(
            &mut app,
            Target::Launch,
            done("launched vncviewer → port 5901"),
        );
        assert!(!app.quit);
        deliver(&mut app, Target::Action(id), done("deleted a"));
        assert!(app.quit);
    }

    #[test]
    fn ctrl_c_twice_quits_without_waiting() {
        let (_dir, mut app) = test_app();
        app.spawn_action("starting a…".into(), || done("started a"));
        app.on_event(Event::Key(ctrl('c')));
        assert!(!app.quit);
        app.on_event(Event::Key(ctrl('c')));
        assert!(app.quit);
    }

    /// A panel that is busy until its task comes back.
    struct BusyPanel {
        busy: bool,
    }

    impl Panel for BusyPanel {
        fn title(&self) -> String {
            "Edit VM: deb".into()
        }
        fn handle_key(&mut self, _key: KeyEvent, _ctx: &mut Ctx) {}
        fn on_task(&mut self, _result: TaskResult, _ctx: &mut Ctx) {
            self.busy = false;
        }
        fn render(&mut self, _frame: &mut Frame, _area: Rect, _theme: &Theme, _tick: u64) {}
        fn key_hints(&self) -> Vec<(String, String)> {
            Vec::new()
        }
        fn busy(&self) -> Option<String> {
            self.busy.then(|| "saving deb…".to_string())
        }
    }

    #[test]
    fn ctrl_c_waits_for_a_busy_panel() {
        let (_dir, mut app) = test_app();
        app.open_panel(Box::new(BusyPanel { busy: true }));
        let id = app.panel.as_ref().expect("a panel").id;
        app.on_event(Event::Key(ctrl('c')));
        assert!(!app.quit);
        assert!(
            matches!(&app.popup, Some(Popup::Quitting { label }) if label == "saving deb…"),
            "{:?}",
            app.popup
        );
        deliver(&mut app, Target::Panel(id), done("saved deb"));
        assert!(app.quit);
    }

    /// Quit waits for a start and a panel's save; once the start is done
    /// the Quitting popup names the save.
    #[test]
    fn the_quitting_popup_names_what_is_still_in_flight() {
        let (_dir, mut app) = test_app();
        app.spawn_action("starting a…".into(), || done("started a"));
        let action = app.busy_id.expect("an action id");
        app.open_panel(Box::new(BusyPanel { busy: true }));
        let panel = app.panel.as_ref().expect("a panel").id;
        app.on_event(Event::Key(ctrl('c')));
        let label = |app: &App| match &app.popup {
            Some(Popup::Quitting { label }) => label.clone(),
            other => panic!("unexpected popup {other:?}"),
        };
        assert_eq!(label(&app), "starting a…");
        deliver(&mut app, Target::Action(action), done("started a"));
        assert!(!app.quit);
        assert_eq!(label(&app), "saving deb…");
        deliver(&mut app, Target::Panel(panel), done("saved deb"));
        assert!(app.quit);
    }

    #[test]
    fn a_quit_signal_quits_the_normal_way() {
        let (_dir, mut app) = test_app();
        app.on_event(Event::QuitSignal);
        assert!(app.quit);

        let (_dir, mut app) = test_app();
        app.spawn_action("deleting a…".into(), || done("deleted a"));
        app.on_event(Event::QuitSignal);
        assert!(!app.quit, "the first one waits for the delete");
        app.on_event(Event::QuitSignal);
        assert!(app.quit, "a second one quits anyway");
    }

    /// The Quitting popup's text and keys, as drawn.
    #[test]
    fn the_quitting_popup_says_what_it_waits_for() {
        let (_dir, mut app) = test_app();
        app.loading = false;
        app.vms = vec![vm_entry("a", false)];
        app.spawn_action("deleting a…".into(), || done("deleted a"));
        app.on_event(Event::Key(ch('q')));
        let s = draw(&mut app, 80, 24);
        let y = s
            .iter()
            .position(|l| l.contains("│ Still deleting a… "))
            .unwrap_or_else(|| panic!("{}", s.join("\n")));
        assert_eq!(s[y + 1].trim_matches(|c| c == ' ' || c == '│'), "");
        assert!(
            s[y + 2].contains("│ Ostrich quits as soon as that is done. "),
            "{}",
            s.join("\n")
        );
        assert!(
            screen_contains(&s, " Esc stay  Ctrl-c quit now ╯"),
            "{}",
            s.join("\n")
        );
        assert_eq!(
            app.key_hints(),
            [
                ("Esc".to_string(), "stay".to_string()),
                ("Ctrl-c".to_string(), "quit now".to_string())
            ]
        );
    }

    /// Esc or Enter on the Quitting popup stay: the quit is off, and a form
    /// filled in meanwhile is not quit under when the work lands.
    #[test]
    fn esc_or_enter_on_the_quitting_popup_cancel_the_quit() {
        for k in [key(KeyCode::Esc), key(KeyCode::Enter)] {
            let (_dir, mut app) = test_app();
            app.loading = false;
            app.vms = vec![vm_entry("a", false), vm_entry("b", false)];
            app.spawn_action("deleting a…".into(), || done("deleted a"));
            let id = app.busy_id.expect("an action id");
            app.on_event(Event::Key(ch('q')));
            app.on_event(Event::Key(k));
            assert!(app.popup.is_none(), "{k:?}");
            assert_eq!(app.notice, Some(Notice::Ok("quit cancelled".into())));
            app.on_event(Event::Key(ch('n')));
            assert_eq!(app.panel_title().as_deref(), Some("Create VM"));
            for c in "my-new-vm".chars() {
                app.on_event(Event::Key(ch(c)));
            }
            deliver(&mut app, Target::Action(id), done("deleted a"));
            assert!(!app.quit, "{k:?}: quit under the form");
            assert_eq!(app.panel_title().as_deref(), Some("Create VM"));
        }
    }

    /// q on the Quitting popup keeps waiting; Ctrl-c there quits now.
    #[test]
    fn q_on_the_quitting_popup_keeps_waiting_and_ctrl_c_quits_now() {
        let (_dir, mut app) = test_app();
        app.spawn_action("deleting a…".into(), || done("deleted a"));
        let id = app.busy_id.expect("an action id");
        app.on_event(Event::Key(ch('q')));
        let shown = draw(&mut app, 80, 24);
        app.on_event(Event::Key(ch('q')));
        assert!(!app.quit);
        assert_eq!(draw(&mut app, 80, 24), shown, "q changes nothing");
        deliver(&mut app, Target::Action(id), done("deleted a"));
        assert!(app.quit, "still waiting, so the delete ends it");

        let (_dir, mut app) = test_app();
        app.spawn_action("deleting a…".into(), || done("deleted a"));
        app.on_event(Event::Key(ch('q')));
        app.on_event(Event::Key(ctrl('c')));
        assert!(app.quit);
    }

    /// The awaited start fails: Ostrich stays and shows why, and q then
    /// quits at once.
    #[test]
    fn a_failed_action_cancels_the_pending_quit() {
        let (_dir, mut app) = test_app();
        app.vms = vec![vm_entry("a", false)];
        app.spawn_action("starting a…".into(), || done("started a"));
        let id = app.busy_id.expect("an action id");
        app.on_event(Event::Key(ch('q')));
        let err = "QEMU exited immediately (exit status: 1):\nqemu-system-x86_64: Could not open disk.qcow2";
        deliver(&mut app, Target::Action(id), failed("", err));
        assert!(!app.quit, "the failure is shown, not quit over");
        assert!(
            matches!(&app.popup, Some(Popup::Message { title, text, .. }) if title == "Error" && text == err),
            "{:?}",
            app.popup
        );
        assert_eq!(app.notice, Some(Notice::Err(err.into())));
        app.on_event(Event::Key(key(KeyCode::Esc)));
        assert!(app.popup.is_none() && !app.quit);
        app.on_event(Event::Key(ch('q')));
        assert!(app.quit, "q quits again, with nothing to wait for");

        // A one-line error: the Quitting popup goes, the bar says why.
        let (_dir, mut app) = test_app();
        app.vms = vec![vm_entry("a", true)];
        app.spawn_action("stopping a…".into(), || done("stopped a"));
        let id = app.busy_id.expect("an action id");
        app.on_event(Event::Key(ctrl('c')));
        deliver(&mut app, Target::Action(id), failed("", "a did not stop"));
        assert!(!app.quit);
        assert!(app.popup.is_none(), "{:?}", app.popup);
        assert_eq!(app.notice, Some(Notice::Err("a did not stop".into())));
        app.on_event(Event::Key(ctrl('c')));
        assert!(app.quit);
    }

    /// Only the awaited work's failure cancels: a VNC launch failing
    /// meanwhile, or a refresh, does not.
    #[test]
    fn other_failures_leave_the_pending_quit_alone() {
        let (_dir, mut app) = test_app();
        app.reload();
        deliver(&mut app, Target::List(1), loaded(&["a"]));
        app.spawn_action("deleting a…".into(), || done("deleted a"));
        let id = app.busy_id.expect("an action id");
        app.on_event(Event::Key(ch('q')));
        deliver(&mut app, Target::Launch, failed("", "VM is not running"));
        deliver(
            &mut app,
            Target::Details(1),
            failed("background task", "internal error:\nboom"),
        );
        assert!(!app.quit);
        assert_eq!(
            app.key_hints()[0],
            ("Esc".to_string(), "stay".to_string()),
            "the Quitting popup stays up"
        );
        deliver(&mut app, Target::Action(id), done("deleted a"));
        assert!(app.quit);
    }

    /// Writes a stopped VM's `vm.yaml` into the app's storage.
    fn write_vm(app: &App, cfg: &VmConfig) {
        fs::create_dir_all(vm_dir(app.storage(), &cfg.name)).unwrap();
        save_config(app.storage(), cfg).unwrap();
    }

    /// Waits for the result of panel `id`'s work, dropping its scans and
    /// everything else.
    fn wait_for_work(app: &App, id: PanelId) -> TaskResult {
        loop {
            match wait_for(app, Target::Panel(id)) {
                TaskResult::IsoScanned { .. } | TaskResult::UsbScanned { .. } => {}
                r => return r,
            }
        }
    }

    /// Each panel's work, started from the panel, with quit waiting for it.
    /// A failure keeps Ostrich on the panel, which shows the error: the
    /// Quitting popup goes, and Ctrl-c then quits at once. A success quits.
    #[test]
    fn a_failed_panel_job_cancels_the_pending_quit() {
        fn press(app: &mut App, k: KeyEvent) {
            app.on_event(Event::Key(k));
        }
        fn tabs(app: &mut App, n: usize) {
            for _ in 0..n {
                press(app, key(KeyCode::Tab));
            }
        }
        /// What, how it is started, how it fails, how it succeeds.
        type Case = (&'static str, fn(&mut App), TaskResult, TaskResult);
        let cases: [Case; 6] = [
            (
                "edit",
                |app| {
                    press(app, ch('e'));
                    press(app, ctrl('s'));
                },
                TaskResult::VmUpdateFailed {
                    err: "save VM config: No space left on device".into(),
                },
                TaskResult::VmUpdated { name: "deb".into() },
            ),
            (
                "save template",
                |app| {
                    press(app, ch('t'));
                    press(app, ctrl('s'));
                },
                TaskResult::TemplateSaveFailed {
                    err: "copy disk image: No space left on device".into(),
                },
                TaskResult::TemplateSaved { name: "deb".into() },
            ),
            (
                "ISO",
                |app| {
                    press(app, ch('i'));
                    press(app, ch('e'));
                },
                TaskResult::IsoApplied {
                    cfg: None,
                    notice: String::new(),
                    err: Some("save VM config: Permission denied".into()),
                },
                TaskResult::IsoApplied {
                    cfg: None,
                    notice: "ejected the boot ISO — takes effect on next start".into(),
                    err: None,
                },
            ),
            (
                "USB",
                |app| {
                    press(app, ch('u'));
                    press(app, ch('a'));
                    for c in "1234:5678".chars() {
                        press(app, ch(c));
                    }
                    press(app, key(KeyCode::Enter));
                },
                TaskResult::UsbApplied {
                    cfg: None,
                    notice: String::new(),
                    err: Some("save VM config: Permission denied".into()),
                },
                TaskResult::UsbApplied {
                    cfg: None,
                    notice: "attached 1234:5678 — takes effect on next start".into(),
                    err: None,
                },
            ),
            (
                "create",
                |app| {
                    press(app, ch('n'));
                    tabs(app, 10);
                    press(app, key(KeyCode::Enter));
                },
                TaskResult::VmCreateFailed {
                    err: "create disk image: qemu-img: Permission denied".into(),
                },
                TaskResult::VmCreated {
                    name: "my-vm".into(),
                },
            ),
            (
                "from template",
                |app| {
                    press(app, ch('T'));
                    press(app, key(KeyCode::Enter));
                    tabs(app, 5);
                    press(app, key(KeyCode::Enter));
                },
                TaskResult::VmCreateFailed {
                    err: "copy disk image: No space left on device".into(),
                },
                TaskResult::VmCreated {
                    name: "base-1".into(),
                },
            ),
        ];
        for (what, start, failure, success) in cases {
            for (fails, result) in [(true, failure), (false, success)] {
                let (dir, mut app) = test_app();
                app.loading = false;
                let iso = dir.path().join("debian.iso");
                fs::write(&iso, [0u8; 512]).unwrap();
                let cfg = VmConfig {
                    cdrom_path: iso.to_string_lossy().into_owned(),
                    ..vm_entry("deb", false).cfg
                };
                write_vm(&app, &cfg);
                let tpl = tpl_entry("base");
                fs::create_dir_all(vm::template_dir(app.storage(), "base")).unwrap();
                vm::save_template(app.storage(), &tpl.tpl).unwrap();
                app.vms = vec![VmEntry {
                    cfg,
                    status: ProcessInfo::default(),
                }];
                app.templates = vec![tpl];

                start(&mut app);
                let title = app.panel_title();
                assert!(title.is_some(), "{what}: no panel");
                assert!(
                    app.busy_label().is_some(),
                    "{what}: nothing for quit to wait for:\n{}",
                    draw(&mut app, 120, 40).join("\n")
                );
                let id = app.panel.as_ref().expect("a panel").id;
                // The real work's own result is dropped (a hand-made one
                // stands in for it), but waited for, so it is done with the
                // scratch directory before that goes.
                let real = wait_for_work(&app, id);
                assert!(
                    !matches!(real, TaskResult::Failed { .. }),
                    "{what}: {real:?}"
                );
                press(&mut app, ctrl('c'));
                assert!(!app.quit && app.popup.is_some(), "{what}");
                deliver(&mut app, Target::Panel(id), result);
                if !fails {
                    assert!(app.quit, "{what}: done, so Ostrich quits");
                    continue;
                }
                assert!(!app.quit, "{what}: quit over the failure");
                assert!(app.popup.is_none(), "{what}: {:?}", app.popup);
                assert_eq!(app.panel_title(), title, "{what}: the panel stays");
                assert!(app.busy_label().is_none(), "{what}");
                press(&mut app, ctrl('c'));
                assert!(app.quit, "{what}: Ctrl-c quits at once afterwards");
            }
        }
    }

    // --- keys and panels ----------------------------------------------------

    #[test]
    fn n_on_an_empty_templates_pane_opens_the_create_form() {
        for k in [ch('n'), key(KeyCode::Enter), ch('l')] {
            let (_dir, mut app) = test_app();
            app.loading = false;
            app.focus = Focus::Templates;
            app.on_event(Event::Key(k));
            assert_eq!(app.panel_title().as_deref(), Some("Create VM"), "{k:?}");
        }
    }

    #[test]
    fn panels_open_with_the_vm_state_read_at_that_moment() {
        let (_dir, mut app) = test_app();
        app.loading = false;
        let cfg = VmConfig {
            name: "deb".into(),
            cpu: 1,
            ram: 512,
            disk_size: 8,
            ..VmConfig::default()
        };
        fs::create_dir_all(vm_dir(app.storage(), "deb")).unwrap();
        save_config(app.storage(), &cfg).unwrap();

        // The last refresh says running, but QEMU has gone since.
        app.vms = vec![VmEntry {
            cfg: cfg.clone(),
            status: running(4242),
        }];
        app.on_event(Event::Key(ch('t')));
        let s = draw(&mut app, 140, 40);
        assert!(screen_contains(&s, "can be copied"), "{}", s.join("\n"));
        assert!(!screen_contains(&s, "stop the VM first"));

        // And the other way round: started since the last refresh.
        app.panel = None;
        let qemu = crate::vm::process::testutil::FakeQemu::spawn();
        qemu.run_as(app.storage(), "deb");
        app.vms[0].status = ProcessInfo::default();
        app.on_event(Event::Key(ch('t')));
        let s = draw(&mut app, 140, 40);
        assert!(screen_contains(&s, "stop the VM first"), "{}", s.join("\n"));
    }

    // --- layout -------------------------------------------------------------

    #[test]
    fn the_left_column_shrinks_below_90_columns() {
        assert_eq!(left_width(40), 24);
        assert_eq!(left_width(60), 24);
        assert_eq!(left_width(70), 28);
        assert_eq!(left_width(80), 30);
        assert_eq!(left_width(89), 30);
        assert_eq!(left_width(90), 30);
        assert_eq!(left_width(140), 47);
        assert_eq!(left_width(200), 54);

        let (_dir, mut app) = test_app();
        app.loading = false;
        app.vms = vec![vm_entry("debian-12", true)];
        let s = draw(&mut app, 60, 20);
        let top: Vec<char> = s[0].chars().collect();
        assert_eq!(top[23], '╮', "{}", s.join("\n"));
        assert_eq!(top[24], '╭', "{}", s.join("\n"));
    }

    #[test]
    fn the_details_card_gets_up_to_70_percent_of_the_column() {
        assert_eq!(details_height(19, 22), 15);
        assert_eq!(details_height(10, 22), 10);
        assert_eq!(details_height(2, 22), 3);
        assert_eq!(details_height(19, 40), 19);

        let (_dir, mut app) = test_app();
        app.loading = false;
        app.vms = vec![vm_entry("debian-12", false)];
        let rows = dashboard::vm_details(&app).1.len() as u16;
        // 80x24: the right column is 22 rows; 60 % used to leave 11 for
        // the body.
        assert!(rows > 11 && rows <= 13, "{rows} rows");
        let s = draw(&mut app, 80, 24);
        assert!(screen_contains(&s, "Created"), "{}", s.join("\n"));
    }

    /// The smallest dashboard drawn, 9 rows: the VM list, the templates
    /// pane, the details card and the console each show a row. One row less
    /// is too small, at any width.
    #[test]
    fn the_smallest_dashboard_shows_a_row_in_every_pane() {
        let (_dir, mut app) = test_app();
        app.loading = false;
        app.vms = vec![vm_entry("debian-12", false)];
        app.templates = vec![tpl_entry("base")];
        deliver(
            &mut app,
            Target::Details(1),
            TaskResult::Details(Box::new(Details {
                name: "debian-12".into(),
                console: vec!["login:".into()],
                ..Details::default()
            })),
        );
        for w in [40, 80] {
            let s = draw(&mut app, w, 9);
            let text = s.join("\n");
            assert!(!screen_contains(&s, "terminal too small"), "{w}:\n{text}");
            assert!(s[1].contains("▸ debian"), "{w}: VM list:\n{text}");
            let tpl_top = s
                .iter()
                .position(|l| l.contains("Templates (1)"))
                .unwrap_or_else(|| panic!("{w}:\n{text}"));
            assert!(s[tpl_top + 1].contains("base"), "{w}: templates:\n{text}");
            assert!(s[1].contains("Status"), "{w}: details:\n{text}");
            let console_top = s
                .iter()
                .position(|l| l.contains("╭ Serial"))
                .unwrap_or_else(|| panic!("{w}:\n{text}"));
            assert!(
                s[console_top + 1].contains("login:"),
                "{w}: console:\n{text}"
            );

            assert!(
                screen_contains(&draw(&mut app, w, 8), "terminal too small"),
                "{w}x8"
            );
        }
    }

    // --- terminal and signals -----------------------------------------------

    #[test]
    fn the_panic_hook_restores_the_terminal_only_for_the_ui_thread() {
        static RESTORED: AtomicUsize = AtomicUsize::new(0);
        static PRINTED: AtomicUsize = AtomicUsize::new(0);
        fn restore() {
            RESTORED.fetch_add(1, Ordering::SeqCst);
        }
        let prev: PanicHook = Box::new(|_| {
            PRINTED.fetch_add(1, Ordering::SeqCst);
        });
        let original = panic::take_hook();
        panic::set_hook(ui_panic_hook(thread::current().id(), restore, prev));
        // A task's panic, caught on its own thread the way Tasks::spawn does.
        let task = thread::spawn(|| panic::catch_unwind(|| panic!("task")).is_err())
            .join()
            .expect("join");
        let after_task = (
            RESTORED.load(Ordering::SeqCst),
            PRINTED.load(Ordering::SeqCst),
        );
        let ui = panic::catch_unwind(|| panic!("ui")).is_err();
        panic::set_hook(original);
        assert!(task && ui);
        assert_eq!(
            after_task,
            (0, 0),
            "a task's panic leaves the terminal alone"
        );
        assert_eq!(RESTORED.load(Ordering::SeqCst), 1);
        assert_eq!(PRINTED.load(Ordering::SeqCst), 1);
    }

    /// Closing the terminal must still end Ostrich (see [`QUIT_SIGNALS`]).
    #[test]
    fn a_hangup_is_left_to_its_default_action() {
        assert!(!QUIT_SIGNALS.contains(&Signal::SIGHUP));
    }

    #[test]
    fn quit_signals_are_counted_for_the_event_loop() {
        let guard = SignalGuard::set(&[Signal::SIGHUP], SigHandler::Handler(note_quit_signal));
        take_quit_signals();
        nix::sys::signal::raise(Signal::SIGHUP).expect("raise");
        nix::sys::signal::raise(Signal::SIGHUP).expect("raise");
        let caught = take_quit_signals();
        drop(guard);
        assert_eq!(caught, 2);
        assert_eq!(take_quit_signals(), 0);
    }

    /// The SigIgn mask from a `/proc/<pid>/status` text.
    fn ignored_mask(status: &str) -> u64 {
        let line = status
            .lines()
            .find(|l| l.starts_with("SigIgn:"))
            .expect("SigIgn line");
        u64::from_str_radix(line.split_whitespace().nth(1).expect("mask"), 16).expect("hex")
    }

    #[test]
    fn the_serial_console_runs_with_job_signals_ignored_here_and_default_in_the_child() {
        let bits: u64 = JOB_SIGNALS.iter().map(|s| 1u64 << (*s as i32 - 1)).sum();
        let own = || ignored_mask(&fs::read_to_string("/proc/self/status").expect("status"));
        let child = |cmd: &mut Command| {
            let out = cmd.arg("/proc/self/status").output().expect("cat");
            ignored_mask(&String::from_utf8_lossy(&out.stdout))
        };
        let before = own() & bits;
        let (inside, plain, console) = {
            let _ignored = SignalGuard::set(&JOB_SIGNALS, SigHandler::SigIgn);
            let inside = own() & bits;
            let plain = child(&mut Command::new("cat")) & bits;
            let mut cmd = Command::new("cat");
            default_job_signals_in_child(&mut cmd);
            (inside, plain, child(&mut cmd) & bits)
        };
        assert_eq!(inside, bits, "Ostrich ignores them meanwhile");
        assert_eq!(plain, bits, "an ignored signal is inherited across exec");
        assert_eq!(console, 0, "socat gets the defaults back");
        assert_eq!(own() & bits, before, "and the old dispositions return");
    }
}
