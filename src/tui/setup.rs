//! The first-run wizard: asks for the VM storage directory, creates it and
//! saves the app config.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Paragraph};
use ratatui::DefaultTerminal;

use super::theme::Theme;
use super::widgets::{centered_rect, hints_line, wrap_text, TextInput};
use crate::config::{self, AppConfig};

/// The texts of the screen, verbatim from the Go wizard.
const TITLE: &str = "Ostrich — QEMU Manager";
const SUBTITLE: &str = "First-run setup";
const LABEL: &str = "VM Storage Directory";
const HELP: &str = "Each VM will get its own sub-folder here containing its disk and config.";
const PLACEHOLDER: &str = "e.g. ~/VMs";
/// The key hints, in the dashboard's vocabulary and covering every key
/// [`key_action`] acts on: `Enter confirm  Esc/Ctrl-c quit`.
const HINTS: [(&str, &str); 2] = [("Enter", "confirm"), ("Esc/Ctrl-c", "quit")];
/// The longest path the input takes.
const CHAR_LIMIT: usize = 256;
/// The widest the centred box grows.
const BOX_WIDTH: u16 = 78;

/// The directory the input starts with: `$HOME/VMs`, already expanded.
pub(crate) fn default_storage(home: &Path) -> PathBuf {
    home.join("VMs")
}

/// Expands a leading `~/` to the home directory. Only that prefix: a bare
/// `~` or `~user/…` is left as typed, like the Go wizard did.
pub(crate) fn expand_home(val: &str, home: &Path) -> PathBuf {
    match val.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(val),
    }
}

/// What Enter does: trims the input, rejects an empty path (`path cannot be
/// empty`), expands `~/`, creates the directory (`cannot create directory:
/// <error>`) and saves the config to `config_path` (`cannot save config:
/// <error>`). The error text is what the screen shows under the input. The
/// image paths a config without a storage path already remembers are kept.
pub(crate) fn submit_at(
    input: &str,
    home: &Path,
    config_path: &Path,
) -> std::result::Result<AppConfig, String> {
    let val = input.trim();
    if val.is_empty() {
        return Err("path cannot be empty".to_string());
    }
    let dir = expand_home(val, home);
    if let Err(err) = fs::create_dir_all(&dir) {
        return Err(format!("cannot create directory: {err}"));
    }
    let cfg = AppConfig {
        vm_storage_path: dir.to_string_lossy().into_owned(),
        recent_isos: config::recent_isos_at(config_path),
    };
    if let Err(err) = config::save_at(config_path, &cfg) {
        return Err(format!("cannot save config: {err:#}"));
    }
    Ok(cfg)
}

/// Draws the wizard centred on the frame: title, subtitle, the field label
/// and its help, the input (with the terminal cursor), the error if any,
/// and the key hints.
pub(crate) fn draw_setup(frame: &mut Frame, input: &mut TextInput, err: &str, theme: &Theme) {
    let area = frame.area();
    if area.width < 4 || area.height < 3 {
        return;
    }
    let width = BOX_WIDTH.min(area.width);
    let text_w = width.saturating_sub(4) as usize; // borders and a one-cell margin
    let help_lines = wrap_text(HELP, text_w);
    let err_lines = if err.is_empty() {
        Vec::new()
    } else {
        wrap_text(&format!("✗ {err}"), text_w)
    };

    let mut top: Vec<Line> = vec![
        Line::styled(TITLE, theme.title),
        Line::styled(SUBTITLE, theme.subtitle),
        Line::raw(""),
        Line::styled(LABEL, theme.label),
    ];
    top.extend(help_lines.into_iter().map(|l| Line::styled(l, theme.help)));
    top.push(Line::raw(""));

    let mut bottom: Vec<Line> = vec![Line::raw("")];
    if !err_lines.is_empty() {
        bottom.extend(err_lines.into_iter().map(|l| Line::styled(l, theme.error)));
        bottom.push(Line::raw(""));
    }
    bottom.push(hints_line(&HINTS, theme));

    let height = (top.len() + 1 + bottom.len()) as u16 + 2;
    let rect = centered_rect(area, width, height);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme.border_focused);
    let inner = block.inner(rect).inner(Margin::new(1, 0));
    frame.render_widget(block, rect);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let [top_area, input_area, bottom_area] = Layout::vertical([
        Constraint::Length(top.len() as u16),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(inner);
    frame.render_widget(Paragraph::new(top), top_area);
    if input_area.height > 0 && input_area.width > 2 {
        let [marker, field] =
            Layout::horizontal([Constraint::Length(2), Constraint::Fill(1)]).areas(input_area);
        frame.render_widget(Span::styled("▸ ", theme.label), marker);
        input.render(frame, field, theme);
    }
    frame.render_widget(Paragraph::new(bottom), bottom_area);
}

/// What a key press does on the wizard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyAction {
    /// Esc or Ctrl-c: leave without saving.
    Quit,
    /// Enter: validate, create the directory and save.
    Confirm,
    /// Anything else goes to the text input.
    Edit,
}

/// Maps a key press to what the wizard does with it.
fn key_action(key: &KeyEvent) -> KeyAction {
    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => KeyAction::Quit,
        KeyCode::Esc => KeyAction::Quit,
        KeyCode::Enter => KeyAction::Confirm,
        _ => KeyAction::Edit,
    }
}

/// Runs the wizard on the terminal until the user confirms a directory
/// (returns the saved config) or quits with Ctrl-c / Esc (returns `None`).
/// `quit_signal` is asked every 100 ms while no key comes in; when it says
/// a quit signal (SIGTERM, SIGINT) arrived, the wizard returns `None`
/// as for Esc, so the caller restores the terminal.
///
/// The screen, centred: title `Ostrich — QEMU Manager`, subtitle `First-run
/// setup`, label `VM Storage Directory`, the help line `Each VM will get its
/// own sub-folder here containing its disk and config.`, a text input
/// pre-filled with `~/VMs` expanded (`$HOME/VMs`), the error (if any) as
/// `✗ …`, and the hints `Enter confirm  Esc/Ctrl-c quit`. Enter: an empty
/// path is `path cannot be empty`; `~/` is expanded; the directory is created
/// (`cannot create directory: <error>`) and the config saved
/// (`cannot save config: <error>`).
pub fn run(
    terminal: &mut DefaultTerminal,
    quit_signal: impl FnMut() -> bool,
) -> Result<Option<AppConfig>> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let next_event = || -> std::io::Result<Option<Event>> {
        Ok(if event::poll(POLL)? {
            Some(event::read()?)
        } else {
            None
        })
    };
    run_with(
        terminal,
        &home,
        &config::config_path(),
        next_event,
        quit_signal,
    )
}

/// How long the wizard waits for a key before it looks for a quit signal.
const POLL: Duration = Duration::from_millis(100);

/// [`run`] over any backend, with the home directory, the config file and
/// the event source given: `next_event` returns `None` when no event came
/// within its wait.
fn run_with<B>(
    terminal: &mut Terminal<B>,
    home: &Path,
    config_path: &Path,
    mut next_event: impl FnMut() -> std::io::Result<Option<Event>>,
    mut quit_signal: impl FnMut() -> bool,
) -> Result<Option<AppConfig>>
where
    B: Backend,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let theme = Theme::default();
    let mut input = TextInput::new()
        .with_placeholder(PLACEHOLDER)
        .with_char_limit(CHAR_LIMIT)
        .with_value(&default_storage(home).to_string_lossy())
        .focused();
    let mut err = String::new();
    let mut redraw = true;
    loop {
        if redraw {
            terminal.draw(|f| draw_setup(f, &mut input, &err, &theme))?;
        }
        if quit_signal() {
            return Ok(None);
        }
        redraw = true;
        match next_event()? {
            Some(Event::Key(key)) if key.kind == KeyEventKind::Press => match key_action(&key) {
                KeyAction::Quit => return Ok(None),
                KeyAction::Confirm => match submit_at(&input.value(), home, config_path) {
                    Ok(cfg) => return Ok(Some(cfg)),
                    Err(e) => err = e,
                },
                KeyAction::Edit => {
                    input.handle_key(key);
                }
            },
            Some(Event::Paste(text)) => input.insert_str(&text),
            Some(_) => {}
            None => redraw = false,
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::tui::testutil::*;

    fn scratch() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let config_path = home.join(".config").join("ostrich").join("config.json");
        (dir, home, config_path)
    }

    #[test]
    fn default_is_home_vms_expanded() {
        assert_eq!(
            default_storage(Path::new("/home/user")),
            PathBuf::from("/home/user/VMs")
        );
    }

    #[test]
    fn only_a_tilde_slash_prefix_is_expanded() {
        let home = Path::new("/home/user");
        assert_eq!(expand_home("~/VMs", home), PathBuf::from("/home/user/VMs"));
        assert_eq!(expand_home("~/", home), PathBuf::from("/home/user/"));
        assert_eq!(expand_home("~", home), PathBuf::from("~"));
        assert_eq!(expand_home("~bob/VMs", home), PathBuf::from("~bob/VMs"));
        assert_eq!(expand_home("/srv/vms", home), PathBuf::from("/srv/vms"));
        assert_eq!(
            expand_home("relative/dir", home),
            PathBuf::from("relative/dir")
        );
    }

    #[test]
    fn submit_rejects_an_empty_path() {
        let (_dir, home, cfg_path) = scratch();
        assert_eq!(
            submit_at("", &home, &cfg_path),
            Err("path cannot be empty".to_string())
        );
        assert_eq!(
            submit_at("   ", &home, &cfg_path),
            Err("path cannot be empty".to_string())
        );
        assert!(!cfg_path.exists());
    }

    #[test]
    fn submit_expands_creates_and_saves() {
        let (_dir, home, cfg_path) = scratch();
        let cfg = submit_at(" ~/VMs ", &home, &cfg_path).expect("submit");
        let want = home.join("VMs");
        assert_eq!(cfg.vm_storage_path, want.to_string_lossy());
        assert!(cfg.recent_isos.is_empty());
        assert!(want.is_dir(), "the storage directory is created");
        assert_eq!(config::load_at(&cfg_path).unwrap(), Some(cfg));
        // An existing directory is fine too.
        assert!(submit_at("~/VMs", &home, &cfg_path).is_ok());
        // Nested paths are created all the way down.
        let deep = submit_at("~/a/b/c", &home, &cfg_path).unwrap();
        assert!(home.join("a/b/c").is_dir());
        assert_eq!(config::load_at(&cfg_path).unwrap().unwrap(), deep);
    }

    #[test]
    fn submit_keeps_the_images_a_blank_config_remembers() {
        let (_dir, home, cfg_path) = scratch();
        fs::create_dir_all(cfg_path.parent().unwrap()).unwrap();
        fs::write(
            &cfg_path,
            br#"{"vm_storage_path": "", "recent_isos": ["/a.iso"]}"#,
        )
        .unwrap();
        assert_eq!(config::load_at(&cfg_path).unwrap(), None);
        let cfg = submit_at("~/VMs", &home, &cfg_path).expect("submit");
        assert_eq!(cfg.recent_isos, ["/a.iso".to_string()]);
        let saved = config::load_at(&cfg_path).unwrap().expect("saved");
        assert_eq!(saved.vm_storage_path, home.join("VMs").to_string_lossy());
        assert_eq!(saved.recent_isos, ["/a.iso".to_string()]);
    }

    #[test]
    fn submit_reports_a_directory_it_cannot_create() {
        let (_dir, home, cfg_path) = scratch();
        fs::write(home.join("file"), b"x").unwrap();
        let err = submit_at("~/file/sub", &home, &cfg_path).unwrap_err();
        assert!(err.starts_with("cannot create directory: "), "{err}");
        assert!(!cfg_path.exists());
    }

    #[test]
    fn submit_reports_a_config_it_cannot_save() {
        let (_dir, home, _) = scratch();
        fs::write(home.join("blocker"), b"x").unwrap();
        let bad_cfg = home.join("blocker").join("config.json");
        let err = submit_at("~/VMs", &home, &bad_cfg).unwrap_err();
        assert!(err.starts_with("cannot save config: "), "{err}");
        assert!(
            home.join("VMs").is_dir(),
            "the directory was made before the save failed"
        );
    }

    #[test]
    fn screen_shows_the_go_texts() {
        let theme = Theme::default();
        let mut input = TextInput::new()
            .with_placeholder(PLACEHOLDER)
            .with_char_limit(CHAR_LIMIT)
            .with_value("/home/user/VMs")
            .focused();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|f| draw_setup(f, &mut input, "", &theme))
            .unwrap();
        let s = buffer_lines(terminal.backend().buffer());
        for want in [TITLE, SUBTITLE, LABEL, HELP, "▸ /home/user/VMs", WANT_HINTS] {
            assert!(
                screen_contains(&s, want),
                "setup screen missing {want:?}:\n{}",
                s.join("\n")
            );
        }
        assert!(!screen_contains(&s, "✗"));
        // The box is centred, not stuck in a corner.
        assert!(s[0].is_empty() && s[29].is_empty(), "{}", s.join("\n"));
        let cursor = terminal.get_cursor_position().unwrap();
        let row = line_index(&s, "▸ /home/user/VMs");
        assert_eq!(
            cursor.y as usize, row,
            "the terminal cursor sits on the input"
        );

        terminal
            .draw(|f| draw_setup(f, &mut input, "path cannot be empty", &theme))
            .unwrap();
        let s = buffer_lines(terminal.backend().buffer());
        assert!(
            screen_contains(&s, "✗ path cannot be empty"),
            "{}",
            s.join("\n")
        );
        assert!(screen_contains(&s, WANT_HINTS));

        // The empty input shows its placeholder.
        input.set_value("");
        terminal
            .draw(|f| draw_setup(f, &mut input, "", &theme))
            .unwrap();
        let s = buffer_lines(terminal.backend().buffer());
        assert!(screen_contains(&s, PLACEHOLDER), "{}", s.join("\n"));

        // Small terminals do not panic.
        for (w, h) in [(1, 1), (3, 2), (10, 5), (40, 8), (79, 12)] {
            let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
            t.draw(|f| draw_setup(f, &mut input, "some error", &theme))
                .unwrap();
        }
    }

    /// The hint line as the screen shows it.
    const WANT_HINTS: &str = "Enter confirm  Esc/Ctrl-c quit";

    #[test]
    fn hints_speak_the_dashboard_vocabulary_and_cover_every_key() {
        let theme = Theme::default();
        let line = hints_line(&HINTS, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, WANT_HINTS);
        assert!(!text.contains('—') && !text.contains("Ctrl+C"), "{text}");
        // Every key the wizard acts on is in the hints, and does what they say.
        assert_eq!(key_action(&key(KeyCode::Enter)), KeyAction::Confirm);
        assert_eq!(key_action(&key(KeyCode::Esc)), KeyAction::Quit);
        assert_eq!(key_action(&ctrl('c')), KeyAction::Quit);
        // Plain letters, `q` included, are typed into the path.
        for k in [
            ch('c'),
            ch('q'),
            key(KeyCode::Backspace),
            key(KeyCode::Left),
        ] {
            assert_eq!(key_action(&k), KeyAction::Edit, "{k:?}");
        }
    }

    #[test]
    fn input_edits_like_the_go_field_was_meant_to() {
        let mut input = TextInput::new()
            .with_char_limit(CHAR_LIMIT)
            .with_value("/home/user/VMs")
            .focused();
        assert!(input.handle_key(key(KeyCode::Backspace)));
        assert!(input.handle_key(ch('x')));
        assert_eq!(input.value(), "/home/user/VMx");
        assert!(
            !input.handle_key(key(KeyCode::Enter)),
            "Enter is the wizard's to handle"
        );
        input.set_value(&"a".repeat(300));
        assert_eq!(input.value().len(), CHAR_LIMIT);
    }

    /// Feeds `events` to the wizard one per wait, then reports waits with
    /// nothing in them.
    fn events_from(events: Vec<Event>) -> impl FnMut() -> std::io::Result<Option<Event>> {
        let mut events = events.into_iter();
        move || Ok(events.next())
    }

    #[test]
    fn a_quit_signal_ends_the_wizard_without_saving() {
        let (_dir, home, cfg_path) = scratch();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut asked = 0;
        let quit = || {
            asked += 1;
            asked > 3
        };
        let got = run_with(&mut terminal, &home, &cfg_path, events_from(vec![]), quit).unwrap();
        assert_eq!(got, None);
        assert_eq!(asked, 4, "asked once per wait until it said yes");
        assert!(!cfg_path.exists() && !home.join("VMs").exists());
    }

    #[test]
    fn keys_drive_the_wizard_to_a_saved_config() {
        let (_dir, home, cfg_path) = scratch();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let events = vec![
            Event::Key(ch('x')),
            Event::Paste("/y".into()),
            Event::Key(key(KeyCode::Enter)),
        ];
        let cfg = run_with(&mut terminal, &home, &cfg_path, events_from(events), || {
            false
        })
        .unwrap()
        .expect("a saved config");
        let want = home.join("VMsx/y");
        assert_eq!(cfg.vm_storage_path, want.to_string_lossy());
        assert!(want.is_dir());
        assert_eq!(config::load_at(&cfg_path).unwrap(), Some(cfg));

        // Esc quits, saving nothing.
        let (_dir, home, cfg_path) = scratch();
        let events = vec![Event::Key(key(KeyCode::Esc))];
        let got = run_with(&mut terminal, &home, &cfg_path, events_from(events), || {
            false
        });
        assert_eq!(got.unwrap(), None);
        assert!(!cfg_path.exists());
    }

    fn line_index(lines: &[String], needle: &str) -> usize {
        lines
            .iter()
            .position(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("no line contains {needle:?}"))
    }
}
