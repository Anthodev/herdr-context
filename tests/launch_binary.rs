use std::process::{Command, Output};

fn run_binary(context: Option<&str>) -> std::io::Result<Output> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_herdr-context"));
    command.env_remove("HERDR_PLUGIN_CONTEXT_JSON");
    command.env_remove("HERDR_WORKSPACE_ID");
    command.env_remove("HERDR_TAB_ID");
    command.env_remove("HERDR_PANE_ID");
    if let Some(context) = context {
        command.env("HERDR_PLUGIN_CONTEXT_JSON", context);
    }
    command.output()
}

#[test]
fn valid_environment_exits_successfully() -> Result<(), Box<dyn std::error::Error>> {
    let output = run_binary(Some(
        r#"{"workspace_id":"workspace","tab_id":"tab","pane_id":"pane","cwd":"/project"}"#,
    ))?;

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    Ok(())
}

#[test]
fn missing_context_exits_with_startup_error() -> Result<(), Box<dyn std::error::Error>> {
    let output = run_binary(None)?;

    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("missing required variable HERDR_PLUGIN_CONTEXT_JSON")
    );
    Ok(())
}

#[test]
fn malformed_context_exits_with_startup_error() -> Result<(), Box<dyn std::error::Error>> {
    let output = run_binary(Some("{"))?;

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("malformed Herdr context"));
    Ok(())
}
