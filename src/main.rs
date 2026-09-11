use std::env;
use std::error::Error;
use std::ffi::OsString;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use herdr_context::app::App;
use herdr_context::config::PluginConfig;
use herdr_context::host::client::{CommandHostClient, DOCK_TITLE};
use herdr_context::host::dock_state;
use herdr_context::host::launch::DockLauncher;
use herdr_context::host::{DockWidth, LaunchContext};

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
        Some("restore") => restore_command(),
        Some("on-event") => on_event_command(),
        None => run_default(),
        Some(mode) => Err(format!(
            "unknown mode {mode:?}; expected toggle, dock, restore, or on-event"
        )
        .into()),
    }
}

fn toggle() -> Result<(), Box<dyn Error>> {
    // Capture the invoking terminal before any Herdr operation can change focus.
    let context = LaunchContext::from_env()?;
    let state_dir = state_dir_env().ok_or("missing required variable HERDR_PLUGIN_STATE_DIR")?;
    let mut host = CommandHostClient::from_env()?;
    let config = PluginConfig::load_from_env().into_config();
    DockLauncher::new(state_dir)
        .with_width(DockWidth::clamped(config.dock().initial_width()))
        .with_socket(socket_path_env())
        .toggle(&context, &mut host)?;
    Ok(())
}

/// Startup hook: re-opens docks persisted before the server stopped.
///
/// Best-effort by design: missing hook context and per-record failures are
/// noted on stderr (herdr logs plugin hook output) and the exit code stays 0,
/// so restore can never break session startup.
fn restore_command() -> Result<(), Box<dyn Error>> {
    let Some(state_dir) = state_dir_env() else {
        return Ok(());
    };
    let Some(socket) = socket_path_env() else {
        return Ok(());
    };
    let config = PluginConfig::load_from_env().into_config();
    if !config.dock().restore_on_startup() {
        return Ok(());
    }
    let mut host = match CommandHostClient::from_env() {
        Ok(host) => host,
        Err(error) => {
            eprintln!("herdr-context: restore skipped: {error}");
            return Ok(());
        }
    };
    if let Err(error) = DockLauncher::new(state_dir).restore(&socket, &mut host) {
        eprintln!("herdr-context: restore incomplete: {error}");
    }
    Ok(())
}

/// Event hook: prunes persisted docks whose pane exited out-of-band so a later
/// restart does not resurrect them. Toggle-close already removed the record,
/// which makes the event a no-op for that path.
fn on_event_command() -> Result<(), Box<dyn Error>> {
    let (Some(state_dir), Some(socket)) = (state_dir_env(), socket_path_env()) else {
        return Ok(());
    };
    let Ok(event) = env::var("HERDR_PLUGIN_EVENT_JSON") else {
        return Ok(());
    };
    let Ok(event) = serde_json::from_str::<serde_json::Value>(&event) else {
        return Ok(());
    };
    if event.get("event").and_then(serde_json::Value::as_str) != Some("pane.exited") {
        return Ok(());
    }
    let Some(pane_id) = event
        .pointer("/data/pane_id")
        .and_then(serde_json::Value::as_str)
        .filter(|pane_id| !pane_id.is_empty())
    else {
        return Ok(());
    };
    if let Err(error) = dock_state::remove_by_pane(&state_dir, &socket, pane_id) {
        eprintln!("herdr-context: prune failed: {error}");
    }
    Ok(())
}

fn state_dir_env() -> Option<PathBuf> {
    env::var_os("HERDR_PLUGIN_STATE_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn socket_path_env() -> Option<String> {
    env::var_os("HERDR_SOCKET_PATH")
        .filter(|value| !value.is_empty())
        .and_then(|value| value.into_string().ok())
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
