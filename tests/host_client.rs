#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use herdr_context::host::client::CommandHostClient;
use herdr_context::host::{
    AgentHarness, DockIdentity, DockWidth, HostAgentStatus, HostClient, HostErrorKind,
    HostSessionReference, OpenDockRequest, PaneId, ResumeConversationRequest, TabId, WorkspaceId,
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
    printf '%s\n' '{{"id":"test","result":{{"type":"pane_list","panes":[{{"pane_id":"dock-primary","tab_id":"tab","cwd":"/project","focused":false,"label":"herdr-context"}},{{"pane_id":"dock-secondary","tab_id":"tab","cwd":"/project","focused":false,"terminal_title_stripped":"herdr-context"}},{{"pane_id":"false-label","tab_id":"tab","cwd":"/project","focused":false,"label":"herdr-context"}},{{"pane_id":"cwd-unavailable","tab_id":"tab","cwd":null,"focused":false}},{{"pane_id":"other-tab","tab_id":"other","cwd":null,"focused":true}}]}}}}'
    ;;
  "pane get dock-primary")
    printf '%s\n' '{{"id":"test","result":{{"type":"pane_info","pane":{{"pane_id":"dock-primary","tab_id":"tab","cwd":"/project","focused":false,"label":"herdr-context"}}}}}}'
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
  "pane send-text origin @src/file.tmp ")
    ;;
  "pane focus --direction left --pane opened")
    printf '%s\n' '{{"id":"test","result":{{"type":"pane_focus_direction","focus":{{"changed":true,"focused_pane_id":"middle","source_pane_id":"opened"}}}}}}'
    ;;
  "pane focus --direction left --pane middle")
    printf '%s\n' '{{"id":"test","result":{{"type":"pane_focus_direction","focus":{{"changed":true,"focused_pane_id":"origin","source_pane_id":"middle"}}}}}}'
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
    let mut client = CommandHostClient::new(script).with_plugin_root(PathBuf::from("/plugin root"));
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
    client.send_text(&PaneId::new("origin")?, "@src/file.tmp ")?;
    client.focus_origin_pane(&opened, &PaneId::new("origin")?)?;

    let argv = fs::read_to_string(log)?;
    assert!(argv.contains("pane list --workspace workspace"));
    assert!(argv.contains("plugin pane open --plugin herdr-context --entrypoint dock --placement split --target-pane origin --direction right --cwd /plugin root --env HERDR_CONTEXT_ORIGIN_CWD=/project with space --env HERDR_CONTEXT_ORIGIN_PANE_ID=origin --focus"));
    assert!(argv.contains("pane swap --direction right --pane opened"));
    assert!(argv.contains("pane resize --direction right"));
    assert!(argv.contains("plugin pane focus opened"));
    assert!(argv.contains("plugin pane close opened"));
    assert!(argv.contains("pane send-text origin @src/file.tmp \n"));
    assert!(argv.contains("pane focus --direction left --pane opened"));
    assert!(argv.contains("pane focus --direction left --pane middle"));
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
fn argv_client_resumes_every_supported_harness_in_a_new_focused_tab()
-> Result<(), Box<dyn std::error::Error>> {
    use std::time::Duration;

    let temp = TempDir::new()?;
    let log = temp.path().join("argv.log");
    let script = temp.path().join("fake-herdr-resume");
    fs::write(
        &script,
        format!(
            r#"#!/bin/sh
for arg in "$@"; do printf '<%s>' "$arg" >> '{}'; done
printf '\n' >> '{}'
case "$*" in
  "tab create --workspace workspace --cwd /project with space --no-focus")
    printf '%s\n' '{{"id":"test","result":{{"type":"tab_created","tab":{{"tab_id":"created-tab"}},"root_pane":{{"pane_id":"created-pane"}}}}}}'
    ;;
  agent\ start*|"tab focus created-tab")
    ;;
  *)
    printf '%s\n' '{{"error":{{"code":"operation_failed","message":"unexpected argv"}},"id":"test"}}'
    exit 1
    ;;
esac
"#,
            log.display(),
            log.display(),
        ),
    )?;
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700))?;
    let client = CommandHostClient::new(script)
        .with_timeout(Duration::from_millis(20))
        .with_agent_ready_timeout(Duration::from_millis(5));
    let workspace = WorkspaceId::new("workspace")?;

    let cases = [
        (
            "claude-code",
            AgentHarness::Claude,
            "<agent><start><claude><--kind><claude><--pane><created-pane><--timeout><5><--><--resume><session-id>",
        ),
        (
            "codex-cli",
            AgentHarness::Codex,
            "<agent><start><codex><--kind><codex><--pane><created-pane><--timeout><5><--><resume><session-id>",
        ),
        (
            "opencode",
            AgentHarness::OpenCode,
            "<agent><start><opencode><--kind><opencode><--pane><created-pane><--timeout><5><--><--session><session-id>",
        ),
        (
            "omp",
            AgentHarness::Omp,
            "<agent><start><omp><--kind><omp><--pane><created-pane><--timeout><5><--><--resume><session-id>",
        ),
        (
            "pi",
            AgentHarness::Pi,
            "<agent><start><pi><--kind><pi><--pane><created-pane><--timeout><5><--><--session><session-id>",
        ),
    ];
    for (tool, harness, expected_start) in cases {
        assert_eq!(AgentHarness::from_tool(tool), Some(harness));
        let request = ResumeConversationRequest::new(
            workspace.clone(),
            PathBuf::from("/project with space"),
            harness,
            "session-id",
        )?;
        client.resume_conversation(&request)?;
        assert!(
            fs::read_to_string(&log)?
                .lines()
                .any(|line| line == expected_start)
        );
    }
    assert_eq!(AgentHarness::from_tool("generic-jsonl"), None);

    let argv = fs::read_to_string(log)?;
    assert_eq!(
        argv.matches(
            "<tab><create><--workspace><workspace><--cwd></project with space><--no-focus>"
        )
        .count(),
        5
    );
    assert_eq!(argv.matches("<tab><focus><created-tab>").count(), 5);
    Ok(())
}

#[test]
fn argv_client_closes_created_tab_when_harness_start_fails()
-> Result<(), Box<dyn std::error::Error>> {
    use std::time::Duration;

    let temp = TempDir::new()?;
    let log = temp.path().join("argv.log");
    let script = temp.path().join("fake-herdr-resume-failure");
    fs::write(
        &script,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$*" in
  tab\ create*)
    printf '%s\n' '{{"id":"test","result":{{"type":"tab_created","tab":{{"tab_id":"created-tab"}},"root_pane":{{"pane_id":"created-pane"}}}}}}'
    ;;
  agent\ start*)
    printf '%s\n' '{{"error":{{"code":"operation_failed","message":"harness failed"}},"id":"test"}}'
    exit 1
    ;;
  "tab close created-tab")
    ;;
  *)
    exit 1
    ;;
esac
"#,
            log.display(),
        ),
    )?;
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700))?;
    let client = CommandHostClient::new(script)
        .with_timeout(Duration::from_millis(20))
        .with_agent_ready_timeout(Duration::from_millis(5));
    let request = ResumeConversationRequest::new(
        WorkspaceId::new("workspace")?,
        PathBuf::from("/project"),
        AgentHarness::Omp,
        "session-id",
    )?;

    let error = client
        .resume_conversation(&request)
        .expect_err("harness start must fail");

    assert!(error.to_string().contains("harness failed"));
    let argv = fs::read_to_string(log)?;
    assert!(argv.contains("tab close created-tab\n"));
    assert!(!argv.contains("tab focus"));
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
