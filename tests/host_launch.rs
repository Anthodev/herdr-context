use std::error::Error;
use std::path::{Path, PathBuf};

use herdr_context::host::launch::{DockLauncher, ToggleOutcome};
use herdr_context::host::{
    DockIdentity, DockWidth, HostClient, HostError, HostPane, LaunchContext, OpenDockRequest,
    PaneId, TabId,
};
use tempfile::TempDir;

struct FakeHost {
    panes: Vec<HostPane>,
    opened_panes: Vec<HostPane>,
    operations: Vec<String>,
    fail_open: bool,
}

impl FakeHost {
    fn new(panes: Vec<HostPane>) -> Self {
        Self {
            panes,
            opened_panes: vec![dock_pane("dock", false)],
            operations: Vec::new(),
            fail_open: false,
        }
    }

    fn with_opened_panes(mut self, panes: Vec<HostPane>) -> Self {
        self.opened_panes = panes;
        self
    }
}

impl HostClient for FakeHost {
    fn pane(&self, pane_id: &PaneId) -> Result<Option<HostPane>, HostError> {
        Ok(self
            .panes
            .iter()
            .find(|pane| pane.pane_id() == pane_id)
            .cloned())
    }

    fn panes_in_tab(
        &self,
        _workspace_id: &herdr_context::host::WorkspaceId,
        _tab_id: &TabId,
    ) -> Result<Vec<HostPane>, HostError> {
        Ok(self.panes.clone())
    }

    fn live_sessions(&self) -> Result<Vec<herdr_context::host::HostAgentSession>, HostError> {
        Ok(Vec::new())
    }

    fn send_text(&self, _pane_id: &PaneId, _text: &str) -> Result<(), HostError> {
        Ok(())
    }

    fn focus_origin_pane(
        &self,
        _dock_pane_id: &PaneId,
        _origin_pane_id: &PaneId,
    ) -> Result<(), HostError> {
        Ok(())
    }

    fn verified_dock_identity(
        &mut self,
        pane: &HostPane,
    ) -> Result<Option<DockIdentity>, HostError> {
        self.operations
            .push(format!("verify:{}", pane.pane_id().as_str()));
        Ok(pane.dock_identity())
    }

    fn open_dock(&mut self, request: &OpenDockRequest) -> Result<PaneId, HostError> {
        self.operations.push(format!(
            "open:{}:{}:{}",
            request.origin_pane_id().as_str(),
            request.cwd().display(),
            request.width().columns()
        ));
        if self.fail_open {
            return Err(HostError::new(
                herdr_context::host::HostErrorKind::OperationFailed,
                "open failed",
            ));
        }
        let opened_id = self.opened_panes[0].pane_id().clone();
        self.panes.extend(self.opened_panes.clone());
        Ok(opened_id)
    }

    fn focus_pane(&mut self, pane_id: &PaneId) -> Result<(), HostError> {
        self.operations.push(format!("focus:{}", pane_id.as_str()));
        Ok(())
    }

    fn close_pane(&mut self, pane_id: &PaneId) -> Result<(), HostError> {
        self.operations.push(format!("close:{}", pane_id.as_str()));
        self.panes.retain(|pane| pane.pane_id() != pane_id);
        Ok(())
    }

    fn move_to_right_edge(&mut self, pane_id: &PaneId) -> Result<(), HostError> {
        self.operations.push(format!("move:{}", pane_id.as_str()));
        Ok(())
    }

    fn resize_pane(&mut self, pane_id: &PaneId, width: DockWidth) -> Result<(), HostError> {
        self.operations
            .push(format!("resize:{}:{}", pane_id.as_str(), width.columns()));
        Ok(())
    }
}

fn context() -> Result<LaunchContext, Box<dyn std::error::Error>> {
    Ok(LaunchContext::from_vars([(
        "HERDR_PLUGIN_CONTEXT_JSON",
        r#"{"workspace_id":"workspace","tab_id":"tab","pane_id":"origin","cwd":"/project","foreground_cwd":"/project/foreground"}"#,
    )])?)
}

fn pane(id: &str, focused: bool) -> HostPane {
    HostPane::new(
        PaneId::new(id).expect("valid test pane id"),
        TabId::new("tab").expect("valid test tab id"),
        Some(PathBuf::from("/project")),
        None,
        focused,
    )
}

fn pane_with_foreground(id: &str, focused: bool, foreground_cwd: &str) -> HostPane {
    HostPane::new(
        PaneId::new(id).expect("valid test pane id"),
        TabId::new("tab").expect("valid test tab id"),
        Some(PathBuf::from("/project")),
        Some(PathBuf::from(foreground_cwd)),
        focused,
    )
}

fn dock_pane(id: &str, focused: bool) -> HostPane {
    pane(id, focused).with_dock_identity(DockIdentity::PluginMetadata)
}

fn launcher(state_dir: &Path) -> DockLauncher {
    DockLauncher::new(state_dir.to_path_buf())
}

#[test]
fn absent_dock_opens_from_captured_origin_then_places_sizes_and_focuses()
-> Result<(), Box<dyn std::error::Error>> {
    let state = TempDir::new()?;
    let mut host = FakeHost::new(vec![pane_with_foreground("origin", true, "/live/project")]);

    let outcome = launcher(state.path()).toggle(&context()?, &mut host)?;

    assert_eq!(outcome, ToggleOutcome::Opened);
    assert_eq!(
        host.operations,
        [
            "open:origin:/live/project:40",
            "verify:dock",
            "move:dock",
            "resize:dock:40",
            "focus:dock",
        ]
    );
    Ok(())
}
#[test]
fn configured_width_is_used_for_open_and_resize() -> Result<(), Box<dyn std::error::Error>> {
    let state = TempDir::new()?;
    let mut host = FakeHost::new(vec![pane("origin", true)]);

    let outcome = launcher(state.path())
        .with_width(DockWidth::clamped(52))
        .toggle(&context()?, &mut host)?;

    assert_eq!(outcome, ToggleOutcome::Opened);
    assert_eq!(host.operations[0], "open:origin:/project/foreground:52");
    assert!(
        host.operations
            .iter()
            .any(|operation| operation == "resize:dock:52")
    );
    Ok(())
}

#[test]
fn moved_origin_falls_back_to_a_pane_still_in_the_locked_tab()
-> Result<(), Box<dyn std::error::Error>> {
    let state = TempDir::new()?;
    let mut host = FakeHost::new(vec![pane("fallback", true)]);

    let outcome = launcher(state.path()).toggle(&context()?, &mut host)?;

    assert_eq!(outcome, ToggleOutcome::Opened);
    assert_eq!(host.operations[0], "open:fallback:/project/foreground:40");
    Ok(())
}

#[test]
fn unfocused_dock_is_focused_without_reopening() -> Result<(), Box<dyn std::error::Error>> {
    let state = TempDir::new()?;
    let mut host = FakeHost::new(vec![pane("origin", true), dock_pane("dock", false)]);

    let outcome = launcher(state.path()).toggle(&context()?, &mut host)?;

    assert_eq!(outcome, ToggleOutcome::Focused);
    assert_eq!(host.operations, ["verify:dock", "focus:dock"]);
    Ok(())
}

#[test]
fn focused_dock_is_closed() -> Result<(), Box<dyn std::error::Error>> {
    let state = TempDir::new()?;
    let mut host = FakeHost::new(vec![pane("origin", false), dock_pane("dock", true)]);

    let outcome = launcher(state.path()).toggle(&context()?, &mut host)?;

    assert_eq!(outcome, ToggleOutcome::Closed);
    assert_eq!(host.operations, ["verify:dock", "focus:dock", "close:dock"]);
    Ok(())
}

#[test]
fn duplicate_docks_after_open_keep_lexicographically_first()
-> Result<(), Box<dyn std::error::Error>> {
    let state = TempDir::new()?;
    let mut host = FakeHost::new(vec![pane("origin", true)])
        .with_opened_panes(vec![dock_pane("dock-b", false), dock_pane("dock-a", false)]);

    let outcome = launcher(state.path()).toggle(&context()?, &mut host)?;

    assert_eq!(outcome, ToggleOutcome::Opened);
    assert_eq!(
        host.operations,
        [
            "open:origin:/project/foreground:40",
            "verify:dock-b",
            "verify:dock-a",
            "close:dock-b",
            "move:dock-a",
            "resize:dock-a:40",
            "focus:dock-a",
        ]
    );
    Ok(())
}

#[test]
fn plugin_metadata_wins_over_lexically_earlier_osc_match() -> Result<(), Box<dyn std::error::Error>>
{
    let state = TempDir::new()?;
    let osc = pane("dock-a", false).with_dock_identity(DockIdentity::OscTitle);
    let metadata = dock_pane("dock-z", false);
    let mut host = FakeHost::new(vec![pane("origin", true), osc, metadata]);

    let outcome = launcher(state.path()).toggle(&context()?, &mut host)?;

    assert_eq!(outcome, ToggleOutcome::Focused);
    assert_eq!(
        host.operations,
        [
            "verify:dock-a",
            "verify:dock-z",
            "close:dock-a",
            "focus:dock-z"
        ]
    );
    Ok(())
}

#[test]
fn host_failures_remain_structured_errors() -> Result<(), Box<dyn std::error::Error>> {
    let state = TempDir::new()?;
    let mut host = FakeHost::new(vec![pane("origin", true)]);
    host.fail_open = true;

    let error = launcher(state.path())
        .toggle(&context()?, &mut host)
        .expect_err("open failure must propagate");

    assert!(error.source().is_some());
    assert!(error.to_string().contains("open failed"));
    Ok(())
}

#[test]
fn dock_width_is_clamped_to_supported_bounds() {
    assert_eq!(DockWidth::clamped(1).columns(), 24);
    assert_eq!(DockWidth::clamped(40).columns(), 40);
    assert_eq!(DockWidth::clamped(u16::MAX).columns(), 60);
}
