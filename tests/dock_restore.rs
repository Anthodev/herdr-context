use std::cell::RefCell;
use std::path::{Path, PathBuf};

use herdr_context::host::dock_state::{self, DockRecord};
use herdr_context::host::launch::DockLauncher;
use herdr_context::host::{
    DockIdentity, DockWidth, HostClient, HostError, HostErrorKind, HostPane, OpenDockRequest,
    PaneId, TabId, WorkspaceId,
};
use tempfile::TempDir;

const SOCKET: &str = "sock-main";
const OTHER_SOCKET: &str = "sock-other";
const WORKSPACE: &str = "workspace";
const TAB: &str = "tab";

struct FakeHost {
    panes: Vec<HostPane>,
    opened_panes: Vec<HostPane>,
    operations: RefCell<Vec<String>>,
    gone_tabs: Vec<String>,
    failing_tabs: Vec<String>,
    fail_open: bool,
    fail_resize: bool,
}

impl FakeHost {
    fn new(panes: Vec<HostPane>) -> Self {
        Self {
            panes,
            opened_panes: vec![dock_pane("dock", false)],
            operations: RefCell::new(Vec::new()),
            gone_tabs: Vec::new(),
            failing_tabs: Vec::new(),
            fail_open: false,
            fail_resize: false,
        }
    }

    fn with_opened_panes(mut self, panes: Vec<HostPane>) -> Self {
        self.opened_panes = panes;
        self
    }

    /// Rebuilds the pane list with exactly one focused pane, mirroring herdr.
    fn focus_model(&mut self, pane_id: &str) {
        for pane in &mut self.panes {
            let focused = pane.pane_id().as_str() == pane_id;
            let updated = HostPane::new(
                pane.pane_id().clone(),
                pane.tab_id().clone(),
                pane.cwd().map(Path::to_path_buf),
                pane.foreground_cwd().map(Path::to_path_buf),
                focused,
            );
            *pane = match pane.dock_identity() {
                Some(identity) => updated.with_dock_identity(identity),
                None => updated,
            };
        }
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
        _workspace_id: &WorkspaceId,
        tab_id: &TabId,
    ) -> Result<Vec<HostPane>, HostError> {
        let tab = tab_id.as_str();
        if self.gone_tabs.iter().any(|gone| gone == tab) {
            return Err(HostError::new(HostErrorKind::NotFound, "tab is gone"));
        }
        if self.failing_tabs.iter().any(|failing| failing == tab) {
            return Err(HostError::new(
                HostErrorKind::Unavailable,
                "herdr is unavailable",
            ));
        }
        Ok(self
            .panes
            .iter()
            .filter(|pane| pane.tab_id().as_str() == tab)
            .cloned()
            .collect())
    }

    fn live_sessions(&self) -> Result<Vec<herdr_context::host::HostAgentSession>, HostError> {
        Ok(Vec::new())
    }

    fn send_text(&self, _pane_id: &PaneId, _text: &str) -> Result<(), HostError> {
        Ok(())
    }

    fn focus_origin_pane(
        &self,
        dock_pane_id: &PaneId,
        origin_pane_id: &PaneId,
    ) -> Result<(), HostError> {
        if dock_pane_id == origin_pane_id {
            return Ok(());
        }
        self.operations.borrow_mut().push(format!(
            "refocus:{}:{}",
            dock_pane_id.as_str(),
            origin_pane_id.as_str()
        ));
        Ok(())
    }

    fn verified_dock_identity(
        &mut self,
        pane: &HostPane,
    ) -> Result<Option<DockIdentity>, HostError> {
        self.operations
            .borrow_mut()
            .push(format!("verify:{}", pane.pane_id().as_str()));
        // Mirrors herdr: the ownership probe focuses the candidate pane.
        if pane.dock_identity().is_some() {
            self.focus_model(pane.pane_id().as_str());
        }
        Ok(pane.dock_identity())
    }

    fn open_dock(&mut self, request: &OpenDockRequest) -> Result<PaneId, HostError> {
        self.operations.borrow_mut().push(format!(
            "open:{}:{}:{}",
            request.origin_pane_id().as_str(),
            request.cwd().display(),
            request.width().columns()
        ));
        if self.fail_open {
            return Err(HostError::new(
                HostErrorKind::OperationFailed,
                "open failed",
            ));
        }
        let opened_id = self.opened_panes[0].pane_id().clone();
        self.panes.extend(self.opened_panes.clone());
        Ok(opened_id)
    }

    fn focus_pane(&mut self, pane_id: &PaneId) -> Result<(), HostError> {
        self.operations
            .borrow_mut()
            .push(format!("focus:{}", pane_id.as_str()));
        self.focus_model(pane_id.as_str());
        Ok(())
    }

    fn close_pane(&mut self, pane_id: &PaneId) -> Result<(), HostError> {
        self.operations
            .borrow_mut()
            .push(format!("close:{}", pane_id.as_str()));
        self.panes.retain(|pane| pane.pane_id() != pane_id);
        Ok(())
    }

    fn move_to_right_edge(&mut self, pane_id: &PaneId) -> Result<(), HostError> {
        self.operations
            .borrow_mut()
            .push(format!("move:{}", pane_id.as_str()));
        Ok(())
    }

    fn resize_pane(&mut self, pane_id: &PaneId, width: DockWidth) -> Result<(), HostError> {
        self.operations.borrow_mut().push(format!(
            "resize:{}:{}",
            pane_id.as_str(),
            width.columns()
        ));
        if self.fail_resize {
            return Err(HostError::new(
                HostErrorKind::OperationFailed,
                "could not resize pane",
            ));
        }
        Ok(())
    }
}

fn record(pane: &str, width: u16) -> DockRecord {
    record_in(WORKSPACE, TAB, pane, width)
}

fn record_in(workspace: &str, tab: &str, pane: &str, width: u16) -> DockRecord {
    DockRecord {
        workspace_id: workspace.to_owned(),
        tab_id: tab.to_owned(),
        dock_pane_id: pane.to_owned(),
        width,
    }
}

fn pane(id: &str, focused: bool) -> HostPane {
    HostPane::new(
        PaneId::new(id).expect("valid test pane id"),
        TabId::new(TAB).expect("valid test tab id"),
        Some(PathBuf::from("/project")),
        None,
        focused,
    )
}

fn pane_without_cwd(id: &str, focused: bool) -> HostPane {
    HostPane::new(
        PaneId::new(id).expect("valid test pane id"),
        TabId::new(TAB).expect("valid test tab id"),
        None,
        None,
        focused,
    )
}

fn dock_pane(id: &str, focused: bool) -> HostPane {
    pane(id, focused).with_dock_identity(DockIdentity::PluginMetadata)
}

fn launcher(state: &Path) -> DockLauncher {
    DockLauncher::new(state.to_path_buf())
}

fn seed(state: &Path, socket: &str, record: DockRecord) {
    dock_state::upsert_record(state, socket, record).expect("seed dock record");
}

fn stored(state: &Path, socket: &str) -> Vec<DockRecord> {
    dock_state::load(state).records_for(socket).to_vec()
}

#[test]
fn restore_opens_the_saved_dock_without_stealing_focus_and_refreshes_the_record()
-> Result<(), Box<dyn std::error::Error>> {
    let state = TempDir::new()?;
    seed(state.path(), SOCKET, record("dock-old", 48));
    let mut host =
        FakeHost::new(vec![pane("origin", true)]).with_opened_panes(vec![dock_pane("dock", false)]);

    launcher(state.path()).restore(SOCKET, &mut host)?;

    assert_eq!(
        *host.operations.borrow(),
        [
            "open:origin:/project:48",
            "verify:dock",
            "move:dock",
            "resize:dock:48",
            "refocus:dock:origin",
        ]
    );
    assert_eq!(stored(state.path(), SOCKET), [record("dock", 48)]);
    Ok(())
}

#[test]
fn restore_falls_back_to_the_process_cwd_when_the_pane_has_none()
-> Result<(), Box<dyn std::error::Error>> {
    let state = TempDir::new()?;
    seed(state.path(), SOCKET, record("dock-old", 40));
    let mut host = FakeHost::new(vec![pane_without_cwd("origin", true)]);

    launcher(state.path()).restore(SOCKET, &mut host)?;

    assert_eq!(
        host.operations.borrow()[0],
        format!("open:origin:{}:40", std::env::current_dir()?.display())
    );
    Ok(())
}

#[test]
fn restore_prunes_the_record_when_the_tab_is_gone() -> Result<(), Box<dyn std::error::Error>> {
    let state = TempDir::new()?;
    seed(state.path(), SOCKET, record("dock-old", 40));
    let mut host = FakeHost::new(vec![pane("origin", true)]);
    host.gone_tabs = vec![TAB.to_owned()];

    launcher(state.path()).restore(SOCKET, &mut host)?;

    assert!(host.operations.borrow().is_empty());
    assert!(stored(state.path(), SOCKET).is_empty());
    Ok(())
}

#[test]
fn restore_prunes_the_record_when_the_tab_has_no_panes() -> Result<(), Box<dyn std::error::Error>> {
    let state = TempDir::new()?;
    seed(state.path(), SOCKET, record("dock-old", 40));
    let mut host = FakeHost::new(Vec::new());

    launcher(state.path()).restore(SOCKET, &mut host)?;

    assert!(host.operations.borrow().is_empty());
    assert!(stored(state.path(), SOCKET).is_empty());
    Ok(())
}

#[test]
fn restore_refreshes_the_record_when_a_dock_is_already_open()
-> Result<(), Box<dyn std::error::Error>> {
    let state = TempDir::new()?;
    seed(state.path(), SOCKET, record("dock-old", 40));
    let mut host = FakeHost::new(vec![pane("origin", true), dock_pane("dock-live", false)]);

    launcher(state.path()).restore(SOCKET, &mut host)?;

    assert_eq!(
        *host.operations.borrow(),
        ["verify:dock-live", "refocus:dock-live:origin"]
    );
    assert_eq!(stored(state.path(), SOCKET), [record("dock-live", 40)]);
    Ok(())
}

#[test]
fn restore_touches_only_the_restored_socket() -> Result<(), Box<dyn std::error::Error>> {
    let state = TempDir::new()?;
    seed(state.path(), SOCKET, record("dock-old", 40));
    seed(
        state.path(),
        OTHER_SOCKET,
        record_in("ws2", "tab2", "kept", 52),
    );
    let mut host = FakeHost::new(vec![pane("origin", true)]);
    host.gone_tabs = vec![TAB.to_owned()];

    launcher(state.path()).restore(SOCKET, &mut host)?;

    assert!(stored(state.path(), SOCKET).is_empty());
    assert_eq!(
        stored(state.path(), OTHER_SOCKET),
        [record_in("ws2", "tab2", "kept", 52)]
    );
    Ok(())
}

#[test]
fn restore_keeps_the_record_when_the_host_is_unavailable() -> Result<(), Box<dyn std::error::Error>>
{
    let state = TempDir::new()?;
    seed(state.path(), SOCKET, record("dock-old", 40));
    let mut host = FakeHost::new(vec![pane("origin", true)]);
    host.failing_tabs = vec![TAB.to_owned()];

    launcher(state.path()).restore(SOCKET, &mut host)?;

    assert!(host.operations.borrow().is_empty());
    assert_eq!(stored(state.path(), SOCKET), [record("dock-old", 40)]);
    Ok(())
}

#[test]
fn restore_keeps_the_record_when_the_open_fails() -> Result<(), Box<dyn std::error::Error>> {
    let state = TempDir::new()?;
    seed(state.path(), SOCKET, record("dock-old", 40));
    let mut host = FakeHost::new(vec![pane("origin", true)]);
    host.fail_open = true;

    launcher(state.path()).restore(SOCKET, &mut host)?;

    assert_eq!(*host.operations.borrow(), ["open:origin:/project:40"]);
    assert_eq!(stored(state.path(), SOCKET), [record("dock-old", 40)]);
    Ok(())
}

#[test]
fn restore_gives_the_opened_dock_the_record_when_placement_is_imperfect()
-> Result<(), Box<dyn std::error::Error>> {
    let state = TempDir::new()?;
    seed(state.path(), SOCKET, record("dock-old", 24));
    let mut host = FakeHost::new(vec![pane("origin", true)]);
    host.fail_resize = true;

    launcher(state.path()).restore(SOCKET, &mut host)?;

    assert_eq!(
        *host.operations.borrow(),
        [
            "open:origin:/project:24",
            "verify:dock",
            "move:dock",
            "resize:dock:24",
            "refocus:dock:origin",
        ]
    );
    // The opened dock owns the record even at an imperfect width: a stale
    // record would open yet another dock on every restart.
    assert_eq!(stored(state.path(), SOCKET), [record("dock", 24)]);
    Ok(())
}

#[test]
fn restore_without_records_is_a_no_op() -> Result<(), Box<dyn std::error::Error>> {
    let state = TempDir::new()?;
    let mut host = FakeHost::new(vec![pane("origin", true)]);

    launcher(state.path()).restore(SOCKET, &mut host)?;

    assert!(host.operations.borrow().is_empty());
    assert!(!state.path().join("docks.json").exists());
    Ok(())
}
