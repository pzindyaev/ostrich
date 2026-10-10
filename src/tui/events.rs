//! What flows through the event loop: terminal events, the 100 ms tick, and
//! the results of background tasks. Every operation that may block — listing
//! VMs, starting QEMU, copying a disk, scanning USB — runs on a thread and
//! reports back as a [`TaskResult`] addressed to the dashboard or to one
//! panel.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use crossterm::event::KeyEvent;

use crate::vm::{HostUsbDevice, ImageState, ProcessInfo, Template, UsbState, VmConfig};

/// Who a task result is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Target {
    /// The dashboard's list reload with this sequence number; a result older
    /// than the newest one applied is dropped.
    List(u64),
    /// The dashboard's details reload with this sequence number.
    Details(u64),
    /// The dashboard action (start, stop, delete) with this id: only its own
    /// result clears the busy label it set.
    Action(u64),
    /// A dashboard launch that never sets the busy label (the VNC viewer).
    Launch,
    /// The panel that was open when the task was spawned; results for a
    /// panel that has closed since are dropped.
    Panel(PanelId),
}

/// Identifies one opening of a panel. A new id per opening keeps late
/// results from a closed form away from the one opened after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PanelId(pub u64);

/// One row of the VM list.
#[derive(Debug, Clone)]
pub struct VmEntry {
    pub cfg: VmConfig,
    pub status: ProcessInfo,
}

/// One row of the templates list.
#[derive(Debug, Clone)]
pub struct TemplateEntry {
    pub tpl: Template,
    /// The disk image's size on the host, 0 when unknown.
    pub disk_usage: u64,
}

/// Everything the details pane and the console show for the selected VM,
/// gathered in one background pass every 2 s.
#[derive(Debug, Clone, Default)]
pub struct Details {
    pub name: String,
    pub status: ProcessInfo,
    pub guest_ip: Option<String>,
    pub usb: Vec<UsbState>,
    /// The boot ISO; meaningless when none is configured.
    pub cdrom: ImageState,
    pub images: Vec<ImageState>,
    /// The additional disks' images.
    pub disks: Vec<ImageState>,
    /// The console tail, already sanitised. The dashboard moves it into
    /// its console view, so `App::details` keeps this empty.
    pub console: Vec<String>,
}

/// What a background task reports back.
#[derive(Debug, Clone)]
pub enum TaskResult {
    // --- dashboard ---
    /// The VM and template lists, reloaded, with each listing's error: a
    /// listing that failed comes back empty, the other one is still shown.
    Loaded {
        vms: Vec<VmEntry>,
        templates: Vec<TemplateEntry>,
        vm_err: Option<String>,
        tpl_err: Option<String>,
    },
    /// The selected VM's details and console tail.
    Details(Box<Details>),
    /// A VM action finished: `what` is past tense, e.g. `started debian-12`.
    Done {
        what: String,
    },
    /// A task failed: `what` is what was attempted (`background task`), `err`
    /// the error text. The dashboard's own actions leave `what` empty, so the
    /// status bar shows the bare error, as Go did (see [`failure_text`]).
    Failed {
        what: String,
        err: String,
    },

    // --- forms ---
    VmCreated {
        name: String,
    },
    VmCreateFailed {
        err: String,
    },
    VmUpdated {
        name: String,
    },
    /// The edit form stays open on this; the config may already be saved
    /// (the text says so).
    VmUpdateFailed {
        err: String,
    },
    TemplateSaved {
        name: String,
    },
    TemplateSaveFailed {
        err: String,
    },

    // --- dialogs ---
    UsbScanned {
        devs: Vec<HostUsbDevice>,
        err: Option<String>,
        status: ProcessInfo,
    },
    /// An attach/detach was applied; `cfg` is the saved config, or `None`
    /// when nothing was written.
    UsbApplied {
        cfg: Option<Box<VmConfig>>,
        notice: String,
        err: Option<String>,
    },
    /// Copying the udev command to the clipboard finished.
    Clipboard {
        err: Option<String>,
    },
    IsoScanned {
        cdrom: ImageState,
        states: Vec<ImageState>,
        status: ProcessInfo,
    },
    IsoApplied {
        cfg: Option<Box<VmConfig>>,
        notice: String,
        err: Option<String>,
    },
}

/// A terminal event or a message from a background task.
#[derive(Debug)]
pub enum Event {
    Key(KeyEvent),
    Paste(String),
    Resize(u16, u16),
    /// Fires about every 100 ms; drives spinners and the 2 s refresh.
    Tick,
    Task(Target, TaskResult),
    /// SIGINT or SIGTERM arrived: quit the normal way, with the terminal
    /// restored.
    QuitSignal,
}

/// How a [`TaskResult::Failed`] reads in the status bar: `<what>: <err>`,
/// or `err` alone when `what` is empty.
pub fn failure_text(what: &str, err: &str) -> String {
    if what.is_empty() {
        err.to_string()
    } else {
        format!("{what}: {err}")
    }
}

/// Spawns background tasks and delivers their results to the event loop.
#[derive(Debug, Clone)]
pub struct Tasks {
    tx: Sender<(Target, TaskResult)>,
}

impl Tasks {
    /// A task runner and the receiving end the event loop drains.
    pub fn new() -> (Tasks, Receiver<(Target, TaskResult)>) {
        let (tx, rx) = mpsc::channel();
        (Tasks { tx }, rx)
    }

    /// Runs `f` on a new thread and delivers its result to `target`. A task
    /// that panics is reported as [`TaskResult::Failed`] instead of taking
    /// the program down, and so is a thread the system refuses to create.
    pub fn spawn<F>(&self, target: Target, f: F)
    where
        F: FnOnce() -> TaskResult + Send + 'static,
    {
        self.spawn_on(
            thread::Builder::new().name("ostrich-task".into()),
            target,
            f,
        );
    }

    /// [`Tasks::spawn`] with an explicit thread builder (tests use one the
    /// system refuses).
    fn spawn_on<F>(&self, builder: thread::Builder, target: Target, f: F)
    where
        F: FnOnce() -> TaskResult + Send + 'static,
    {
        let tx = self.tx.clone();
        let spawned = builder.spawn(move || {
            let result = match catch_unwind(AssertUnwindSafe(f)) {
                Ok(r) => r,
                Err(p) => {
                    let msg = p
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                        .unwrap_or_else(|| "unknown panic".to_string());
                    TaskResult::Failed {
                        what: TASK.to_string(),
                        err: format!("internal error: {msg}"),
                    }
                }
            };
            let _ = tx.send((target, result));
        });
        // Out of threads (a pids limit, RLIMIT_NPROC): the task never ran,
        // which its target hears about like any other failure.
        if let Err(err) = spawned {
            let _ = self.tx.send((
                target,
                TaskResult::Failed {
                    what: TASK.to_string(),
                    err: format!("spawn thread: {err}"),
                },
            ));
        }
    }
}

/// What a task that panicked or never started is called in its failure.
const TASK: &str = "background task";

/// The text of an error the way the UI shows it: the whole context chain.
pub fn err_text(err: &anyhow::Error) -> String {
    format!("{err:#}")
}

/// `✓ what` / `✗ what: err` material for the status bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    Ok(String),
    Err(String),
}

impl Notice {
    /// The text without the glyph.
    pub fn text(&self) -> &str {
        match self {
            Notice::Ok(s) | Notice::Err(s) => s,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn next(rx: &Receiver<(Target, TaskResult)>) -> (Target, TaskResult) {
        rx.recv_timeout(Duration::from_secs(10))
            .expect("a task result")
    }

    #[test]
    fn spawn_delivers_the_result_to_its_target() {
        let (tasks, rx) = Tasks::new();
        tasks.spawn(Target::Action(7), || TaskResult::Done {
            what: "started a".into(),
        });
        let (target, result) = next(&rx);
        assert_eq!(target, Target::Action(7));
        assert!(matches!(result, TaskResult::Done { what } if what == "started a"));
    }

    #[test]
    fn a_panicking_task_is_reported_as_failed() {
        let (tasks, rx) = Tasks::new();
        tasks.spawn(Target::Details(3), || panic!("boom"));
        let (target, result) = next(&rx);
        assert_eq!(target, Target::Details(3));
        match result {
            TaskResult::Failed { what, err } => {
                assert_eq!(what, "background task");
                assert_eq!(err, "internal error: boom");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_thread_the_system_refuses_is_reported_as_failed() {
        let (tasks, rx) = Tasks::new();
        // A stack larger than the address space: thread creation fails the
        // way it does under an exhausted pids limit, with an error, not a
        // panic on the calling (UI) thread.
        let builder = thread::Builder::new().stack_size(1 << 47);
        tasks.spawn_on(builder, Target::List(5), || TaskResult::Done {
            what: "never".into(),
        });
        let (target, result) = next(&rx);
        assert_eq!(target, Target::List(5));
        match result {
            TaskResult::Failed { what, err } => {
                assert_eq!(what, "background task");
                assert!(err.starts_with("spawn thread: "), "{err}");
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
