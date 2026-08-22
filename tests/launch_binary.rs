use std::process::{Command, Output};

fn run_binary(mode: Option<&str>, context: Option<&str>) -> std::io::Result<Output> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_herdr-context"));
    command.args(mode);
    command.env_remove("HERDR_PLUGIN_CONTEXT_JSON");
    command.env_remove("HERDR_PLUGIN_STATE_DIR");
    command.env_remove("HERDR_BIN_PATH");
    command.env_remove("HERDR_WORKSPACE_ID");
    command.env_remove("HERDR_TAB_ID");
    command.env_remove("HERDR_PANE_ID");
    if let Some(context) = context {
        command.env("HERDR_PLUGIN_CONTEXT_JSON", context);
    }
    command.output()
}

const VALID_CONTEXT: &str =
    r#"{"workspace_id":"workspace","tab_id":"tab","pane_id":"pane","cwd":"/project"}"#;

#[test]
fn dock_mode_sets_stable_osc_title() -> Result<(), Box<dyn std::error::Error>> {
    let output = run_binary(Some("dock"), Some(VALID_CONTEXT))?;

    assert!(output.status.success());
    assert_eq!(output.stdout, b"\x1b]2;herdr-context\x07");
    assert!(output.stderr.is_empty());
    Ok(())
}

#[test]
fn default_mode_preserves_silent_non_terminal_startup() -> Result<(), Box<dyn std::error::Error>> {
    let output = run_binary(None, Some(VALID_CONTEXT))?;

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    Ok(())
}

#[test]
fn toggle_without_context_exits_with_startup_error() -> Result<(), Box<dyn std::error::Error>> {
    let output = run_binary(Some("toggle"), None)?;

    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("missing required variable HERDR_PLUGIN_CONTEXT_JSON")
    );
    Ok(())
}

#[test]
fn malformed_toggle_context_exits_with_startup_error() -> Result<(), Box<dyn std::error::Error>> {
    let output = run_binary(Some("toggle"), Some("{"))?;

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("malformed Herdr context"));
    Ok(())
}

#[test]
fn unknown_mode_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
    let output = run_binary(Some("unknown"), None)?;

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("expected toggle or dock"));
    Ok(())
}
