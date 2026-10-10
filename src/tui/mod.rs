//! The ratatui front end: a dashboard with the VMs and templates on the
//! left and the selected VM's details and serial console on the right.
//! Forms and device dialogs take over the right-hand column; confirmations,
//! long errors and the key help are centred popups.

pub mod actions;
pub mod app;
pub mod console;
pub mod dashboard;
pub mod dialogs;
pub mod events;
pub mod forms;
pub mod panel;
pub mod popups;
pub mod setup;
pub mod theme;
pub mod widgets;

pub use app::{run, App, Focus};

/// Helpers for the TUI's own tests: key events, a context over a scratch
/// storage directory, and collecting the results of spawned tasks.
#[cfg(test)]
pub mod testutil {
    use std::path::PathBuf;
    use std::sync::mpsc::{Receiver, RecvTimeoutError};
    use std::time::Duration;

    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
    use ratatui::backend::TestBackend;
    use ratatui::prelude::*;

    use super::events::{PanelId, Target, TaskResult, Tasks};
    use super::panel::{Action, Ctx, Panel};
    use super::theme::Theme;
    use crate::vm::Manager;

    /// A key press without modifiers.
    pub fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    /// A character key.
    pub fn ch(c: char) -> KeyEvent {
        key(KeyCode::Char(c))
    }

    /// A Ctrl-<c> key.
    pub fn ctrl(c: char) -> KeyEvent {
        KeyEvent {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    /// Shift-Tab.
    pub fn backtab() -> KeyEvent {
        KeyEvent {
            code: KeyCode::BackTab,
            modifiers: KeyModifiers::SHIFT,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    /// Types every char of `s`.
    pub fn type_str(panel: &mut dyn Panel, h: &Harness, s: &str) -> Vec<Action> {
        let mut acts = Vec::new();
        for c in s.chars() {
            acts.extend(h.press(panel, ch(c)));
        }
        acts
    }

    /// A manager over a scratch directory, a task runner and the receiver
    /// its results land on.
    pub struct Harness {
        pub dir: tempfile::TempDir,
        pub mgr: Manager,
        pub home: PathBuf,
        pub tasks: Tasks,
        pub rx: Receiver<(Target, TaskResult)>,
        pub theme: Theme,
        pub panel_id: PanelId,
    }

    impl Harness {
        pub fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let storage = dir.path().join("vms");
            std::fs::create_dir_all(&storage).expect("storage dir");
            let home = dir.path().join("home");
            std::fs::create_dir_all(&home).expect("home dir");
            let (tasks, rx) = Tasks::new();
            Harness {
                mgr: Manager::new(storage),
                dir,
                home,
                tasks,
                rx,
                theme: Theme::default(),
                panel_id: PanelId(1),
            }
        }

        /// A context for one event.
        pub fn ctx(&self) -> Ctx<'_> {
            Ctx::new(
                &self.mgr,
                &self.home,
                Rect::new(0, 0, 120, 40),
                0,
                &self.tasks,
                self.panel_id,
            )
        }

        /// Delivers a key and returns the actions the panel pushed.
        pub fn press(&self, panel: &mut dyn Panel, k: KeyEvent) -> Vec<Action> {
            let mut ctx = self.ctx();
            panel.handle_key(k, &mut ctx);
            ctx.actions
        }

        /// Runs the panel's `init`.
        pub fn init(&self, panel: &mut dyn Panel) -> Vec<Action> {
            let mut ctx = self.ctx();
            panel.init(&mut ctx);
            ctx.actions
        }

        /// Waits up to `timeout` for the next task result.
        pub fn next_result(&self, timeout: Duration) -> Option<TaskResult> {
            match self.rx.recv_timeout(timeout) {
                Ok((_, r)) => Some(r),
                Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => None,
            }
        }

        /// Waits for the next task result and feeds it to the panel,
        /// returning the actions it pushed. Panics when none arrives.
        pub fn deliver_next(&self, panel: &mut dyn Panel) -> Vec<Action> {
            let r = self
                .next_result(Duration::from_secs(10))
                .expect("a task result");
            let mut ctx = self.ctx();
            panel.on_task(r, &mut ctx);
            ctx.actions
        }

        /// Renders the panel into a `width` × `height` test terminal and
        /// returns the screen as text lines.
        pub fn render(&self, panel: &mut dyn Panel, width: u16, height: u16) -> Vec<String> {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
            terminal
                .draw(|f| {
                    let area = f.area();
                    panel.render(f, area, &self.theme, 0);
                })
                .expect("draw");
            buffer_lines(terminal.backend().buffer())
        }
    }

    impl Default for Harness {
        fn default() -> Self {
            Self::new()
        }
    }

    /// The rows of a buffer as plain strings (trailing spaces trimmed).
    pub fn buffer_lines(buf: &Buffer) -> Vec<String> {
        let area = buf.area();
        (0..area.height)
            .map(|y| {
                let line: String = (0..area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect();
                line.trim_end().to_string()
            })
            .collect()
    }

    /// Whether any screen line contains `needle`.
    pub fn screen_contains(lines: &[String], needle: &str) -> bool {
        lines.iter().any(|l| l.contains(needle))
    }
}
