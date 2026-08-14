#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use herdr_context::host::client::CommandHostClient;
use herdr_context::host::{
    DockIdentity, DockWidth, HostAgentStatus, HostClient, HostErrorKind, HostSessionReference,
    OpenDockRequest, PaneId, TabId, WorkspaceId,
};
use tempfile::TempDir;

#[test]
fn argv_client_queries_identity_and_executes_dock_operations()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let log = temp.path().join("argv.log");
    let resized = temp.path().join("resized");
    let script = temp.path().join("fake-herdr");
    fs::write(
        &script,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$*" in
  "pane list --workspace workspace")
    printf '%s\n' '{{"id":"test","result":{{"type":"pane_list","panes":[{{"pane_id":"dock-primary","tab_id":"tab","cwd":"/project","focused":false,"label":"herdr-context dock"}},{{"pane_id":"dock-secondary","tab_id":"tab","cwd":"/project","focused":false,"terminal_title_stripped":"herdr-context dock"}},{{"pane_id":"false-label","tab_id":"tab","cwd":"/project","focused":false,"label":"herdr-context dock"}},{{"pane_id":"cwd-unavailable","tab_id":"tab","cwd":null,"focused":false}},{{"pane_id":"other-tab","tab_id":"other","cwd":null,"focused":true}}]}}}}'
    ;;
  "pane get dock-primary")
    printf '%s\n' '{{"id":"test","result":{{"type":"pane_info","pane":{{"pane_id":"dock-primary","tab_id":"tab","cwd":"/project","focused":false,"label":"herdr-context dock"}}}}}}'
    ;;
  plugin\ pane\ open*)
    printf '%s\n' '{{"id":"test","result":{{"type":"plugin_pane_opened","plugin_pane":{{"plugin_id":"herdr-context","entrypoint":"dock","pane":{{"pane_id":"opened","tab_id":"tab","cwd":"/project with space","focused":true}}}}}}}}'
    ;;
  "plugin pane focus dock-primary"|"plugin pane focus dock-secondary")
    printf '%s\n' '{{"id":"test","result":{{"type":"plugin_pane_focused","plugin_pane":{{"plugin_id":"herdr-context","entrypoint":"dock","pane":{{"pane_id":"verified","tab_id":"tab","cwd":"/project","focused":true}}}}}}}}'
    ;;
  "plugin pane focus false-label")
    printf '%s\n' '{{"error":{{"code":"plugin_pane_not_found","message":"plugin pane not found"}},"id":"test"}}' >&2
    exit 1
    ;;
  "plugin pane focus opened")
    printf '%s\n' '{{"id":"test","result":{{"type":"plugin_pane_focused","plugin_pane":{{"plugin_id":"herdr-context","entrypoint":"dock","pane":{{"pane_id":"opened","tab_id":"tab","cwd":"/project","focused":true}}}}}}}}'
    ;;
  "plugin pane close opened")
    printf '%s\n' '{{"id":"test","result":{{"type":"plugin_pane_closed","pane_id":"opened"}}}}'
    ;;
  "pane swap --direction right --pane opened")
    printf '%s\n' '{{"id":"test","result":{{"type":"pane_swap","swap":{{"changed":false}}}}}}'
    ;;
  "pane layout --pane opened")
    if [ -f '{}' ]; then width=40; else width=80; fi
    printf '%s\n' "{{\"id\":\"test\",\"result\":{{\"type\":\"pane_layout\",\"layout\":{{\"area\":{{\"width\":120}},\"panes\":[{{\"pane_id\":\"opened\",\"rect\":{{\"width\":$width}}}}]}}}}}}"
    ;;
  pane\ resize*)
    : > '{}'
    printf '%s\n' '{{"id":"test","result":{{"type":"pane_resize","resize":{{"changed":true}}}}}}'
    ;;
  *)
    printf '%s\n' '{{"error":{{"code":"operation_failed","message":"unexpected argv"}},"id":"test"}}'
    exit 1
    ;;
esac
"#,
            log.display(),
            resized.display(),
            resized.display()
        ),
    )?;
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700))?;

    let workspace = WorkspaceId::new("workspace")?;
    let tab = TabId::new("tab")?;
    let mut client = CommandHostClient::new(script);
    let panes = client.panes_in_tab(&workspace, &tab)?;
    assert_eq!(panes.len(), 4);
    assert_eq!(panes[0].dock_identity(), Some(DockIdentity::PluginMetadata));
    assert_eq!(panes[1].dock_identity(), Some(DockIdentity::OscTitle));
    assert_eq!(panes[3].cwd(), None);
    assert_eq!(
        client.verified_dock_identity(&panes[0])?,
        Some(DockIdentity::PluginMetadata)
    );
    assert_eq!(
        client.verified_dock_identity(&panes[1])?,
        Some(DockIdentity::PluginMetadata)
    );
    assert_eq!(client.verified_dock_identity(&panes[2])?, None);
    assert!(client.pane(&PaneId::new("dock-primary")?)?.is_some());

    let request = OpenDockRequest::new(
        PaneId::new("origin")?,
        tab,
        PathBuf::from("/project with space"),
        DockWidth::clamped(40),
    );
    let opened = client.open_dock(&request)?;
    assert_eq!(opened.as_str(), "opened");
    client.move_to_right_edge(&opened)?;
    client.resize_pane(&opened, DockWidth::clamped(40))?;
    client.focus_pane(&opened)?;
    client.close_pane(&opened)?;

    let argv = fs::read_to_string(log)?;
    assert!(argv.contains("pane list --workspace workspace"));
    assert!(argv.contains("plugin pane open --plugin herdr-context --entrypoint dock --placement split --target-pane origin --direction right --cwd /project with space --focus"));
    assert!(argv.contains("pane swap --direction right --pane opened"));
    assert!(argv.contains("pane resize --direction right"));
    assert!(argv.contains("plugin pane focus opened"));
    assert!(argv.contains("plugin pane close opened"));
    Ok(())
}

#[test]
fn argv_client_normalizes_bounded_live_agent_sessions() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let script = temp.path().join("fake-herdr-live");
    let transcript = temp.path().join("session.jsonl");
    let mut agents = (0..300)
        .map(|index| {
            serde_json::json!({
                "agent": "shell",
                "agent_status": "unknown",
                "cwd": temp.path(),
                "pane_id": format!("pane-shell-{index}"),
            })
        })
        .collect::<Vec<_>>();
    agents.push(serde_json::json!({
        "agent": "omp",
        "agent_session": {
            "source": "herdr:omp",
            "agent": "omp",
            "kind": "path",
            "value": transcript,
        },
        "agent_status": "working",
        "cwd": temp.path(),
        "foreground_cwd": temp.path(),
        "pane_id": "pane-live",
        "title": "live title",
    }));
    let response = serde_json::json!({
        "id": "test",
        "result": {
            "type": "agent_list",
            "agents": agents,
        },
    });
    fs::write(
        &script,
        format!("#!/bin/sh\nprintf '%s\\n' '{}'\n", response),
    )?;
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700))?;

    let sessions = CommandHostClient::new(script.clone()).live_sessions()?;

    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].source(), "herdr:omp");
    assert_eq!(sessions[0].agent(), "omp");
    assert_eq!(sessions[0].pane_id().as_str(), "pane-live");
    assert_eq!(sessions[0].title(), Some("live title"));
    assert_eq!(sessions[0].status(), HostAgentStatus::Working);
    assert!(matches!(
        sessions[0].reference(),
        HostSessionReference::TranscriptPath(path) if path == &transcript
    ));

    let agents = (0..257)
        .map(|index| {
            serde_json::json!({
                "agent": "omp",
                "agent_session": {
                    "source": "herdr:omp",
                    "agent": "omp",
                    "kind": "id",
                    "value": format!("session-{index}"),
                },
                "agent_status": "working",
                "cwd": temp.path(),
                "pane_id": format!("pane-live-{index}"),
            })
        })
        .collect::<Vec<_>>();
    let response = serde_json::json!({
        "id": "test",
        "result": {
            "type": "agent_list",
            "agents": agents,
        },
    });
    fs::write(
        &script,
        format!("#!/bin/sh\nprintf '%s\\n' '{}'\n", response),
    )?;
    let error = CommandHostClient::new(script)
        .live_sessions()
        .expect_err("live session limit");
    assert_eq!(error.kind(), HostErrorKind::InvalidResponse);
    Ok(())
}

#[test]
fn missing_binary_is_a_structured_host_error() {
    let client = CommandHostClient::new(PathBuf::from("/definitely/missing/herdr"));
    let error = client
        .pane(&PaneId::new("pane").expect("valid pane id"))
        .expect_err("spawn must fail");
    assert_eq!(
        error.kind(),
        herdr_context::host::HostErrorKind::Unavailable
    );
}

#[test]
fn transiently_busy_executable_is_retried() -> Result<(), Box<dyn std::error::Error>> {
    use std::fs::OpenOptions;
    use std::thread;
    use std::time::Duration;

    let temp = TempDir::new()?;
    let script = temp.path().join("busy-herdr");
    fs::write(
        &script,
        "#!/bin/sh\nprintf '%s\\n' '{\"id\":\"test\",\"result\":{\"type\":\"pane_info\",\"pane\":{\"pane_id\":\"pane\",\"tab_id\":\"tab\",\"cwd\":\"/project\",\"focused\":false}}}'\n",
    )?;
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700))?;
    let writer = OpenOptions::new().write(true).open(&script)?;
    let release = thread::spawn(move || {
        thread::sleep(Duration::from_millis(30));
        drop(writer);
    });

    let client = CommandHostClient::new(script);
    assert!(client.pane(&PaneId::new("pane")?)?.is_some());
    release.join().expect("release writer");
    Ok(())
}

#[test]
fn stalled_command_returns_a_bounded_structured_error() -> Result<(), Box<dyn std::error::Error>> {
    use std::time::{Duration, Instant};

    let temp = TempDir::new()?;
    let script = temp.path().join("slow-herdr");
    fs::write(&script, "#!/bin/sh\nsleep 5\n")?;
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700))?;
    let client = CommandHostClient::new(script).with_timeout(Duration::from_millis(50));

    let started = Instant::now();
    let error = client
        .pane(&PaneId::new("pane")?)
        .expect_err("slow command must time out");

    assert_eq!(
        error.kind(),
        herdr_context::host::HostErrorKind::OperationFailed
    );
    assert!(error.to_string().contains("timed out"));
    assert!(started.elapsed() < Duration::from_secs(1));
    Ok(())
}
