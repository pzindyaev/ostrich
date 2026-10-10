//! The contract between the dashboard and the screens that take over its
//! right-hand column: the create and edit forms, the template forms, the
//! USB and ISO dialogs.
//!
//! A panel gets every key while it is open, draws itself into the area it is
//! given (and may draw a popup anywhere on the frame), runs blocking work
//! through [`Ctx::spawn`], and asks the dashboard for things — close me, show
//! a notice, select a VM — by pushing [`Action`]s onto the context.

use std::path::{Path, PathBuf};

use crossterm::event::KeyEvent;
use ratatui::prelude::*;

use super::events::{Notice, PanelId, Target, TaskResult, Tasks};
use super::theme::Theme;
use crate::vm::Manager;

/// What a panel asks the dashboard to do.
#[derive(Debug)]
pub enum Action {
    /// Close the panel and return to the dashboard (lists are refreshed).
    Close,
    /// Close, refresh, and put the VM cursor on this VM.
    CloseSelectVm(String),
    /// Close, refresh, focus the templates pane and put its cursor on this template.
    CloseSelectTemplate(String),
    /// Show a message in the status bar (a multi-line error opens a popup).
    Notice(Notice),
    /// Reload the lists and details without closing.
    Refresh,
    /// Quit the program.
    Quit,
}

/// What a panel can see and do while handling an event.
pub struct Ctx<'a> {
    pub mgr: &'a Manager,
    /// The user's home directory (`$HOME`), for `~` expansion.
    pub home: &'a Path,
    /// The whole terminal.
    pub size: Rect,
    /// Spinner tick count.
    pub tick: u64,
    tasks: &'a Tasks,
    panel: PanelId,
    pub actions: Vec<Action>,
}

impl<'a> Ctx<'a> {
    /// A context for one event delivered to the panel `panel`.
    pub fn new(
        mgr: &'a Manager,
        home: &'a Path,
        size: Rect,
        tick: u64,
        tasks: &'a Tasks,
        panel: PanelId,
    ) -> Self {
        Ctx {
            mgr,
            home,
            size,
            tick,
            tasks,
            panel,
            actions: Vec::new(),
        }
    }

    /// The VM storage directory.
    pub fn storage(&self) -> &'a Path {
        self.mgr.storage()
    }

    /// An owned copy of the storage path, for moving into a task closure.
    pub fn storage_buf(&self) -> PathBuf {
        self.mgr.storage().to_path_buf()
    }

    /// Runs `f` on a background thread; its result comes back to this panel
    /// through [`Panel::on_task`] (and is dropped if the panel has closed).
    pub fn spawn<F>(&self, f: F)
    where
        F: FnOnce() -> TaskResult + Send + 'static,
    {
        self.tasks.spawn(Target::Panel(self.panel), f);
    }

    pub fn close(&mut self) {
        self.actions.push(Action::Close);
    }

    pub fn close_select_vm(&mut self, name: impl Into<String>) {
        self.actions.push(Action::CloseSelectVm(name.into()));
    }

    pub fn close_select_template(&mut self, name: impl Into<String>) {
        self.actions.push(Action::CloseSelectTemplate(name.into()));
    }

    pub fn notice_ok(&mut self, text: impl Into<String>) {
        self.actions.push(Action::Notice(Notice::Ok(text.into())));
    }

    pub fn notice_err(&mut self, text: impl Into<String>) {
        self.actions.push(Action::Notice(Notice::Err(text.into())));
    }

    pub fn refresh(&mut self) {
        self.actions.push(Action::Refresh);
    }

    pub fn quit(&mut self) {
        self.actions.push(Action::Quit);
    }
}

/// A screen that takes over the dashboard's right-hand column.
pub trait Panel {
    /// The pane title, e.g. `Create VM` or `Edit VM: debian-12`.
    fn title(&self) -> String;

    /// Called once right after the panel is opened, with a context it can
    /// spawn its initial scan from.
    fn init(&mut self, _ctx: &mut Ctx) {}

    /// A key press (Ctrl-c is handled before this: it quits, or, while the
    /// panel is busy, waits for that work first).
    fn handle_key(&mut self, key: KeyEvent, ctx: &mut Ctx);

    /// Pasted text (bracketed paste); goes into the focused input, if any.
    fn handle_paste(&mut self, _text: &str, _ctx: &mut Ctx) {}

    /// A background task's result, addressed to this panel.
    fn on_task(&mut self, result: TaskResult, ctx: &mut Ctx);

    /// Draws the panel into `area` (the right-hand column, borders
    /// included: draw your own bordered block with [`Panel::title`]). A
    /// panel that shows a modal dialog draws it centred on `frame.area()`.
    fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme, tick: u64);

    /// The key hints for the bottom bar, as (key, description) pairs.
    fn key_hints(&self) -> Vec<(String, String)>;

    /// What the panel is waiting for, if a long operation is in flight. The
    /// panel shows its own progress (the status bar's spinner is for the
    /// dashboard's start, stop and delete); quitting waits for this work to
    /// finish, under a `Still <label>` popup, and the panel ignores its
    /// close keys meanwhile.
    fn busy(&self) -> Option<String> {
        None
    }

    /// Whether the work [`Panel::busy`] named ended in an error the panel
    /// shows. Asked as soon as `busy` is back to `None`: a quit waiting for
    /// that work is then called off, so the error is seen and the user can
    /// quit again. A panel clears its error when it starts work.
    fn failed(&self) -> bool {
        false
    }
}
