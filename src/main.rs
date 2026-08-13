use std::env;
use std::error::Error;
use std::ffi::OsString;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use herdr_context::app::App;
use herdr_context::host::LaunchContext;
use herdr_context::host::client::{CommandHostClient, DOCK_TITLE};
use herdr_context::host::launch::DockLauncher;

fn main() -> ExitCode {
    match run(env::args_os().nth(1)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("herdr-context: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(mode: Option<OsString>) -> Result<(), Box<dyn Error>> {
    match mode.as_deref().and_then(|value| value.to_str()) {
        Some("toggle") => toggle(),
        Some("dock") => run_dock(),
        None => run_default(),
        Some(mode) => Err(format!("unknown mode {mode:?}; expected toggle or dock").into()),
    }
}

fn toggle() -> Result<(), Box<dyn Error>> {
    // Capture the invoking terminal before any Herdr operation can change focus.
    let context = LaunchContext::from_env()?;
    let state_dir = env::var_os("HERDR_PLUGIN_STATE_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or("missing required variable HERDR_PLUGIN_STATE_DIR")?;
    let mut host = CommandHostClient::from_env()?;
    DockLauncher::new(state_dir).toggle(&context, &mut host)?;
    Ok(())
}

fn run_default() -> Result<(), Box<dyn Error>> {
    let context = LaunchContext::from_env()?;
    if io::stdout().is_terminal() {
        run_terminal(context)?;
    }
    Ok(())
}

fn run_dock() -> Result<(), Box<dyn Error>> {
    let context = LaunchContext::from_env()?;
    let mut stdout = io::stdout().lock();
    write!(stdout, "\u{1b}]2;{DOCK_TITLE}\u{7}")?;
    stdout.flush()?;
    drop(stdout);
    if io::stdout().is_terminal() {
        run_terminal(context)?;
    }
    Ok(())
}

fn run_terminal(context: LaunchContext) -> Result<(), Box<dyn Error>> {
    let mut app = App::new(context);
    ratatui::run(|terminal| app.run(terminal))?;
    Ok(())
}
