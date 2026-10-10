use std::ffi::OsStr;
use std::process::ExitCode;

use ostrich::tui::app::InitError;

/// What the command line asks for. Only the first argument matters, and
/// anything but version or help opens the dashboard, as in the Go program;
/// so does an argument that is not UTF-8.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    Version,
    Help,
    Run,
}

fn command(arg: Option<&OsStr>) -> Command {
    match arg.and_then(OsStr::to_str) {
        Some("version" | "--version" | "-v") => Command::Version,
        Some("help" | "--help" | "-h") => Command::Help,
        _ => Command::Run,
    }
}

/// The line a fatal error is reported with: `error initializing: <err>`
/// when the config could not be read (before any UI), `error: <err>`
/// otherwise.
fn error_line(err: &anyhow::Error) -> String {
    match err.downcast_ref::<InitError>() {
        Some(init) => format!("error initializing: {init}"),
        None => format!("error: {err:#}"),
    }
}

fn main() -> ExitCode {
    let arg = std::env::args_os().nth(1);
    match command(arg.as_deref()) {
        Command::Version => {
            println!(
                "ostrich {} (commit {}, built {})",
                ostrich::VERSION,
                ostrich::COMMIT,
                ostrich::BUILD_DATE
            );
            return ExitCode::SUCCESS;
        }
        Command::Help => {
            println!("Usage: ostrich [--version]\n\nA TUI for managing QEMU virtual machines. Run without arguments to open the interface.");
            return ExitCode::SUCCESS;
        }
        Command::Run => {}
    }

    match ostrich::tui::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{}", error_line(&err));
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStrExt;

    use super::*;

    #[test]
    fn only_version_and_help_are_commands() {
        for arg in ["version", "--version", "-v"] {
            assert_eq!(command(Some(OsStr::new(arg))), Command::Version, "{arg}");
        }
        for arg in ["help", "--help", "-h"] {
            assert_eq!(command(Some(OsStr::new(arg))), Command::Help, "{arg}");
        }
        // Go ignores everything else and opens the dashboard.
        for arg in ["foo", "--bogus", "/some/path", "", "-V"] {
            assert_eq!(command(Some(OsStr::new(arg))), Command::Run, "{arg}");
        }
        assert_eq!(command(None), Command::Run);
        // An argument that is not UTF-8 too, rather than a panic.
        assert_eq!(
            command(Some(OsStr::from_bytes(b"\xffversion"))),
            Command::Run
        );
    }

    #[test]
    fn a_config_error_is_reported_as_an_initialization_error() {
        let parse = anyhow::anyhow!("expected value at line 1 column 1")
            .context("parse /home/u/.config/ostrich/config.json");
        assert_eq!(
            error_line(&anyhow::Error::new(InitError(parse))),
            "error initializing: parse /home/u/.config/ostrich/config.json: expected value at line 1 column 1"
        );
        let other = anyhow::anyhow!("No such device or address (os error 6)")
            .context("initialize terminal");
        assert_eq!(
            error_line(&other),
            "error: initialize terminal: No such device or address (os error 6)"
        );
    }
}
