use std::env;
use std::error::Error;
use std::fmt;
use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

use super::dock_state::{
    DockRecord, DockStateError, load as load_dock_state, remove_record, replace_socket_records,
    upsert_record,
};
use super::{
    DEFAULT_DOCK_WIDTH, DockIdentity, DockWidth, HostClient, HostError, HostErrorKind, HostPane,
    LaunchContext, OpenDockRequest, PaneId, TabId, WorkspaceId,
};

/// Current dock visibility relative to focused pane.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DockState {
    Absent,
    Present {
        dock_pane_id: PaneId,
        focused_pane_id: PaneId,
    },
}

/// Side-effect-free action selected by dock toggle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToggleDecision {
    Open,
    Focus { pane_id: PaneId },
    Close { pane_id: PaneId },
}

/// Decides future toggle behavior without calling Herdr.
///
/// Invariant: absent docks open, unfocused docks focus, and focused docks close.
#[must_use]
pub fn decide_toggle(state: DockState) -> ToggleDecision {
    match state {
        DockState::Absent => ToggleDecision::Open,
        DockState::Present {
            dock_pane_id,
            focused_pane_id,
        } if dock_pane_id == focused_pane_id => ToggleDecision::Close {
            pane_id: dock_pane_id,
        },
        DockState::Present { dock_pane_id, .. } => ToggleDecision::Focus {
            pane_id: dock_pane_id,
        },
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToggleOutcome {
    Opened,
    Focused,
    Closed,
}

/// Race-safe launcher for one dock in a workspace tab.
#[derive(Clone, Debug)]
pub struct DockLauncher {
    state_dir: PathBuf,
    lock_timeout: Duration,
    width: DockWidth,
    socket: Option<String>,
}

impl DockLauncher {
    #[must_use]
    pub fn new(state_dir: PathBuf) -> Self {
        Self {
            state_dir,
            lock_timeout: Duration::from_secs(2),
            width: DockWidth::clamped(DEFAULT_DOCK_WIDTH),
            socket: None,
        }
    }

    #[must_use]
    pub const fn with_lock_timeout(mut self, timeout: Duration) -> Self {
        self.lock_timeout = timeout;
        self
    }
    #[must_use]
    pub const fn with_width(mut self, width: DockWidth) -> Self {
        self.width = width;
        self
    }

    /// Configures best-effort dock persistence for the server behind `socket`.
    ///
    /// Without a socket the toggle still opens, focuses, and closes docks; it
    /// just leaves nothing for a restart restore to re-open.
    #[must_use]
    pub fn with_socket(mut self, socket: Option<String>) -> Self {
        self.socket = socket;
        self
    }

    pub fn toggle(
        &self,
        context: &LaunchContext,
        host: &mut impl HostClient,
    ) -> Result<ToggleOutcome, LauncherError> {
        let origin = host.pane(context.focused_pane_id())?;
        let captured_cwd = origin
            .as_ref()
            .and_then(HostPane::foreground_cwd)
            .or_else(|| context.foreground_cwd())
            .or_else(|| origin.as_ref().and_then(HostPane::cwd))
            .unwrap_or_else(|| context.cwd())
            .to_path_buf();
        let _lock = TabLock::acquire(
            &self.state_dir,
            context.workspace_id(),
            context.tab_id(),
            self.lock_timeout,
        )?;

        let panes = host.panes_in_tab(context.workspace_id(), context.tab_id())?;
        let open_target_pane_id = panes
            .iter()
            .find(|pane| pane.pane_id() == context.focused_pane_id())
            .or_else(|| panes.iter().find(|pane| pane.is_focused()))
            .or_else(|| panes.first())
            .map(|pane| pane.pane_id().clone());
        let dock = reconcile_docks(host, &panes)?;
        let focused_pane_id = panes
            .iter()
            .find(|pane| pane.is_focused())
            .map(|pane| pane.pane_id().clone())
            .unwrap_or_else(|| context.focused_pane_id().clone());
        let state = dock.map_or(DockState::Absent, |pane| DockState::Present {
            dock_pane_id: pane.pane_id().clone(),
            focused_pane_id,
        });

        match decide_toggle(state) {
            ToggleDecision::Open => {
                let target_pane_id = open_target_pane_id.ok_or_else(|| {
                    LauncherError::Invariant("target tab has no pane to split".to_owned())
                })?;
                let request = OpenDockRequest::new(
                    target_pane_id,
                    context.tab_id().clone(),
                    captured_cwd,
                    self.width,
                );
                let opened_pane_id = host.open_dock(&request)?;
                let panes = host.panes_in_tab(context.workspace_id(), context.tab_id())?;
                let dock = reconcile_docks(host, &panes)?.ok_or_else(|| {
                    LauncherError::Invariant(format!(
                        "opened dock {} was absent from the post-open pane query",
                        opened_pane_id.as_str()
                    ))
                })?;
                let pane_id = dock.pane_id();

                // Persist before placement: a dock that exists but fails
                // placement must still restore later. Lock order stays
                // tab lock first, then docks.lock.
                self.persist_open(context, pane_id, request.width());
                host.move_to_right_edge(pane_id)?;
                host.resize_pane(pane_id, request.width())?;
                host.focus_pane(pane_id)?;
                Ok(ToggleOutcome::Opened)
            }
            ToggleDecision::Focus { pane_id } => {
                host.focus_pane(&pane_id)?;
                Ok(ToggleOutcome::Focused)
            }
            ToggleDecision::Close { pane_id } => {
                host.close_pane(&pane_id)?;
                self.persist_close(context);
                Ok(ToggleOutcome::Closed)
            }
        }
    }

    /// Re-opens docks persisted for `socket` after a Herdr server restart.
    ///
    /// Best-effort per record: vanished tabs prune their record, transient
    /// failures keep it for the next restart, and the pane holding focus when
    /// the hook ran keeps it — docks open with an explicit no-focus request
    /// and the captured focused pane is re-focused after identity probes.
    pub fn restore(&self, socket: &str, host: &mut impl HostClient) -> Result<(), LauncherError> {
        let records = load_dock_state(&self.state_dir)
            .records_for(socket)
            .to_vec();
        if records.is_empty() {
            return Ok(());
        }
        let mut kept = Vec::with_capacity(records.len());
        for record in records {
            match self.restore_record(host, &record) {
                RecordOutcome::Kept(record) => kept.push(record),
                RecordOutcome::Dropped => {}
            }
        }
        replace_socket_records(&self.state_dir, socket, kept)?;
        Ok(())
    }

    fn restore_record(&self, host: &mut impl HostClient, record: &DockRecord) -> RecordOutcome {
        let Some((workspace_id, tab_id)) = record_ids(record) else {
            return RecordOutcome::Dropped;
        };
        let _lock =
            match TabLock::acquire(&self.state_dir, &workspace_id, &tab_id, self.lock_timeout) {
                Ok(lock) => lock,
                Err(error) => {
                    note_restore_deferred(&workspace_id, &tab_id, &error);
                    return RecordOutcome::Kept(record.clone());
                }
            };
        let panes = match host.panes_in_tab(&workspace_id, &tab_id) {
            Ok(panes) => panes,
            Err(error) if error.kind() == HostErrorKind::NotFound => {
                return RecordOutcome::Dropped;
            }
            Err(error) => {
                note_restore_deferred(&workspace_id, &tab_id, &error);
                return RecordOutcome::Kept(record.clone());
            }
        };
        if panes.is_empty() {
            return RecordOutcome::Dropped;
        }

        // Capture focus before reconcile: the identity probes below focus
        // candidate panes as a side effect of verifying them.
        let focused_pane_id = panes
            .iter()
            .find(|pane| pane.is_focused())
            .map(|pane| pane.pane_id().clone());
        // The identity probes focus candidate panes as a side effect of
        // verifying them, and a partially opened dock leaves them focused, so
        // every post-capture path pins focus back to the pane the server had.
        let focused = focused_pane_id.as_ref();
        let keeper = match reconcile_docks(host, &panes) {
            Ok(Some(dock)) => dock.pane_id().clone(),
            Ok(None) => {
                match self.open_restored_dock(host, &workspace_id, &tab_id, &panes, record) {
                    Ok(keeper) => keeper,
                    Err(error) => {
                        return self.defer_record(
                            host,
                            &workspace_id,
                            &tab_id,
                            focused,
                            &error,
                            record,
                        );
                    }
                }
            }
            Err(error) => {
                return self.defer_record(host, &workspace_id, &tab_id, focused, &error, record);
            }
        };
        refocus_focused_pane(host, &workspace_id, &tab_id, focused);
        RecordOutcome::Kept(DockRecord {
            workspace_id: record.workspace_id.clone(),
            tab_id: record.tab_id.clone(),
            dock_pane_id: keeper.as_str().to_owned(),
            width: record.width,
        })
    }

    /// Keeps a record for the next restart after a transient failure: pins
    /// focus back to the pane the server had, notes the deferral.
    fn defer_record(
        &self,
        host: &impl HostClient,
        workspace_id: &WorkspaceId,
        tab_id: &TabId,
        focused: Option<&PaneId>,
        error: impl fmt::Display,
        record: &DockRecord,
    ) -> RecordOutcome {
        refocus_focused_pane(host, workspace_id, tab_id, focused);
        note_restore_deferred(workspace_id, tab_id, &error);
        RecordOutcome::Kept(record.clone())
    }

    /// Opens a restored dock with an explicit no-focus request, mirroring
    /// `toggle`'s open invariants minus the focus handoff. Placement (edge
    /// move and resize) is best-effort and never fails the open: a dock that
    /// exists but sits at an imperfect width must still own the record, or
    /// every restart would open another dock beside the leftovers.
    fn open_restored_dock(
        &self,
        host: &mut impl HostClient,
        workspace_id: &WorkspaceId,
        tab_id: &TabId,
        panes: &[HostPane],
        record: &DockRecord,
    ) -> Result<PaneId, LauncherError> {
        let target = panes
            .iter()
            .find(|pane| pane.is_focused())
            .or_else(|| panes.first())
            .ok_or_else(|| {
                LauncherError::Invariant("target tab has no pane to split".to_owned())
            })?;
        let cwd = target
            .cwd()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| env::current_dir().unwrap_or_default());
        let request = OpenDockRequest::new_unfocused(
            target.pane_id().clone(),
            tab_id.clone(),
            cwd,
            DockWidth::clamped(record.width),
        );
        let opened_pane_id = host.open_dock(&request)?;
        let panes = host.panes_in_tab(workspace_id, tab_id)?;
        let dock = reconcile_docks(host, &panes)?.ok_or_else(|| {
            LauncherError::Invariant(format!(
                "opened dock {} was absent from the post-open pane query",
                opened_pane_id.as_str()
            ))
        })?;
        let pane_id = dock.pane_id();
        if let Err(error) = host.move_to_right_edge(pane_id) {
            eprintln!(
                "herdr-context: restore could not move {}: {error}",
                pane_id.as_str()
            );
        }
        if let Err(error) = host.resize_pane(pane_id, request.width()) {
            eprintln!(
                "herdr-context: restore could not resize {} to {} columns: {error}",
                pane_id.as_str(),
                request.width().columns()
            );
        }
        Ok(pane_id.clone())
    }

    /// Records an opened dock for `restore`. Persistence is best-effort:
    /// failures degrade to "not restored" and never fail the finished toggle.
    fn persist_open(&self, context: &LaunchContext, dock_pane_id: &PaneId, width: DockWidth) {
        let Some(socket) = self.socket.as_deref() else {
            return;
        };
        let record = DockRecord {
            workspace_id: context.workspace_id().as_str().to_owned(),
            tab_id: context.tab_id().as_str().to_owned(),
            dock_pane_id: dock_pane_id.as_str().to_owned(),
            width: width.columns(),
        };
        if let Err(error) = upsert_record(&self.state_dir, socket, record) {
            eprintln!("herdr-context: could not record the open dock: {error}");
        }
    }

    /// Drops the record for a closed dock. Best-effort like `persist_open`.
    fn persist_close(&self, context: &LaunchContext) {
        let Some(socket) = self.socket.as_deref() else {
            return;
        };
        if let Err(error) = remove_record(
            &self.state_dir,
            socket,
            context.workspace_id().as_str(),
            context.tab_id().as_str(),
        ) {
            eprintln!("herdr-context: could not drop the closed dock record: {error}");
        }
    }
}

/// Fate of one persisted record after a restore pass.
enum RecordOutcome {
    /// Keep the (possibly refreshed) record for the next restart.
    Kept(DockRecord),
    /// The workspace or tab is gone; prune the record.
    Dropped,
}

fn record_ids(record: &DockRecord) -> Option<(WorkspaceId, TabId)> {
    Some((
        WorkspaceId::new(record.workspace_id.as_str()).ok()?,
        TabId::new(record.tab_id.as_str()).ok()?,
    ))
}

fn note_restore_deferred(workspace_id: &WorkspaceId, tab_id: &TabId, error: impl fmt::Display) {
    eprintln!(
        "herdr-context: restore deferred {}/{}: {error}",
        workspace_id.as_str(),
        tab_id.as_str()
    );
}

/// Re-focuses the pane that held focus when the restore hook ran, undoing the
/// focus side effects of the identity probes and any partially opened dock.
fn refocus_focused_pane(
    host: &impl HostClient,
    workspace_id: &WorkspaceId,
    tab_id: &TabId,
    focused_pane_id: Option<&PaneId>,
) {
    let Some(target) = focused_pane_id else {
        return;
    };
    // The saved focus holder is usually not a plugin pane, so `plugin pane
    // focus` cannot reach it; walk from the currently focused pane instead.
    let current = host
        .panes_in_tab(workspace_id, tab_id)
        .ok()
        .and_then(|panes| {
            panes
                .iter()
                .find(|pane| pane.is_focused())
                .map(|pane| pane.pane_id().clone())
        });
    let Some(current) = current else {
        return;
    };
    if current == *target {
        return;
    }
    if let Err(error) = host.focus_origin_pane(&current, target) {
        eprintln!(
            "herdr-context: restore could not refocus {}: {error}",
            target.as_str()
        );
    }
}

fn reconcile_docks(
    host: &mut impl HostClient,
    panes: &[HostPane],
) -> Result<Option<HostPane>, HostError> {
    let mut candidates = panes
        .iter()
        .filter(|pane| pane.is_dock())
        .collect::<Vec<_>>();
    candidates.sort_unstable_by_key(|pane| !pane.is_focused());
    let mut docks = Vec::new();
    let mut originally_focused_dock = None;
    for pane in candidates {
        match host.verified_dock_identity(pane) {
            Ok(Some(identity)) => {
                docks.push(pane.clone().with_dock_identity(identity));
                if pane.is_focused() {
                    originally_focused_dock = Some(pane.pane_id().clone());
                }
            }
            Ok(None) => {}
            Err(error) => {
                if let Some(pane_id) = &originally_focused_dock {
                    let _ = host.focus_pane(pane_id);
                }
                return Err(error);
            }
        }
    }
    if let Some(pane_id) = &originally_focused_dock {
        host.focus_pane(pane_id)?;
    }
    docks.sort_unstable_by(|left, right| {
        dock_identity_rank(left)
            .cmp(&dock_identity_rank(right))
            .then_with(|| left.pane_id().as_str().cmp(right.pane_id().as_str()))
    });
    let Some(keeper) = docks.first().cloned() else {
        return Ok(None);
    };
    for duplicate in &docks[1..] {
        host.close_pane(duplicate.pane_id())?;
    }
    Ok(Some(keeper))
}

const fn dock_identity_rank(pane: &HostPane) -> u8 {
    match pane.dock_identity() {
        Some(DockIdentity::PluginMetadata) => 0,
        Some(DockIdentity::OscTitle) => 1,
        None => 2,
    }
}

/// Held file descriptor for a workspace/tab advisory lock.
#[derive(Debug)]
pub struct TabLock {
    _file: File,
}

impl TabLock {
    pub fn acquire(
        state_dir: impl AsRef<Path>,
        workspace_id: &WorkspaceId,
        tab_id: &TabId,
        timeout: Duration,
    ) -> Result<Self, LockError> {
        let lock_dir = state_dir.as_ref().join("locks");
        ensure_private_directory(state_dir.as_ref())?;
        ensure_private_directory(&lock_dir)?;
        let path = lock_dir.join(Self::file_name(workspace_id, tab_id));
        let file = open_private_lock_file(&path)?;
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(LockError::InvalidTimeout(timeout))?;

        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(TryLockError::WouldBlock) => {
                    return Err(LockError::Timeout { path, timeout });
                }
                Err(TryLockError::Error(error)) => {
                    return Err(LockError::Io {
                        operation: "lock",
                        path,
                        source: error,
                    });
                }
            }
        }
    }

    #[must_use]
    pub fn file_name(workspace_id: &WorkspaceId, tab_id: &TabId) -> String {
        let first = lock_hash(0xcbf2_9ce4_8422_2325, workspace_id, tab_id);
        let second = lock_hash(0x8422_2325_cbf2_9ce4, workspace_id, tab_id);
        format!("tab-{first:016x}{second:016x}.lock")
    }
}

fn lock_hash(seed: u64, workspace_id: &WorkspaceId, tab_id: &TabId) -> u64 {
    let mut hash = seed;
    for value in [workspace_id.as_str().as_bytes(), tab_id.as_str().as_bytes()] {
        for byte in (value.len() as u64).to_le_bytes().iter().chain(value) {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}

pub(super) fn ensure_private_directory(path: &Path) -> Result<(), LockError> {
    std::fs::create_dir_all(path).map_err(|source| LockError::Io {
        operation: "create directory",
        path: path.to_path_buf(),
        source,
    })?;
    let metadata = std::fs::symlink_metadata(path).map_err(|source| LockError::Io {
        operation: "inspect directory",
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.file_type().is_dir() {
        return Err(LockError::UnsafePath(path.to_path_buf()));
    }
    #[cfg(unix)]
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(|source| {
        LockError::Io {
            operation: "secure directory",
            path: path.to_path_buf(),
            source,
        }
    })?;
    Ok(())
}

pub(super) fn open_private_lock_file(path: &Path) -> Result<File, LockError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    let file = options.open(path).map_err(|source| LockError::Io {
        operation: "open lock file",
        path: path.to_path_buf(),
        source,
    })?;
    let metadata = file.metadata().map_err(|source| LockError::Io {
        operation: "inspect lock file",
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.file_type().is_file() {
        return Err(LockError::UnsafeLockFile(path.to_path_buf()));
    }
    #[cfg(unix)]
    {
        let Some(parent) = path.parent() else {
            return Err(LockError::UnsafeLockFile(path.to_path_buf()));
        };
        let parent_metadata = std::fs::metadata(parent).map_err(|source| LockError::Io {
            operation: "inspect lock directory",
            path: parent.to_path_buf(),
            source,
        })?;
        if metadata.uid() != parent_metadata.uid() || metadata.nlink() != 1 {
            return Err(LockError::UnsafeLockFile(path.to_path_buf()));
        }
    }
    #[cfg(unix)]
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|source| LockError::Io {
            operation: "secure lock file",
            path: path.to_path_buf(),
            source,
        })?;
    Ok(file)
}

#[derive(Debug)]
pub enum LockError {
    Timeout {
        path: PathBuf,
        timeout: Duration,
    },
    UnsafePath(PathBuf),
    UnsafeLockFile(PathBuf),
    InvalidTimeout(Duration),
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
}

impl fmt::Display for LockError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout { path, timeout } => write!(
                formatter,
                "timed out after {} ms acquiring lock {}",
                timeout.as_millis(),
                path.display()
            ),
            Self::UnsafePath(path) => {
                write!(
                    formatter,
                    "lock directory is not a directory: {}",
                    path.display()
                )
            }
            Self::UnsafeLockFile(path) => {
                write!(
                    formatter,
                    "lock file is not a private regular file: {}",
                    path.display()
                )
            }
            Self::InvalidTimeout(timeout) => {
                write!(formatter, "lock timeout is too large: {timeout:?}")
            }
            Self::Io {
                operation,
                path,
                source,
            } => write!(
                formatter,
                "failed to {operation} {}: {source}",
                path.display()
            ),
        }
    }
}

impl Error for LockError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Timeout { .. }
            | Self::UnsafePath(_)
            | Self::UnsafeLockFile(_)
            | Self::InvalidTimeout(_) => None,
        }
    }
}

#[derive(Debug)]
pub enum LauncherError {
    Lock(LockError),
    Host(HostError),
    State(DockStateError),
    Invariant(String),
}

impl fmt::Display for LauncherError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lock(error) => write!(formatter, "dock lock failed: {error}"),
            Self::Host(error) => write!(formatter, "Herdr operation failed: {error}"),
            Self::Invariant(message) => write!(formatter, "dock invariant failed: {message}"),
            Self::State(error) => write!(formatter, "dock state failed: {error}"),
        }
    }
}

impl Error for LauncherError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Lock(error) => Some(error),
            Self::Host(error) => Some(error),
            Self::Invariant(_) => None,
            Self::State(error) => Some(error),
        }
    }
}

impl From<LockError> for LauncherError {
    fn from(error: LockError) -> Self {
        Self::Lock(error)
    }
}

impl From<HostError> for LauncherError {
    fn from(error: HostError) -> Self {
        Self::Host(error)
    }
}

impl From<DockStateError> for LauncherError {
    fn from(error: DockStateError) -> Self {
        Self::State(error)
    }
}

#[cfg(test)]
mod tests {
    use super::{DockState, ToggleDecision, decide_toggle};
    use crate::host::PaneId;

    #[test]
    fn absent_dock_opens() {
        assert_eq!(decide_toggle(DockState::Absent), ToggleDecision::Open);
    }

    #[test]
    fn unfocused_dock_receives_focus() -> Result<(), crate::host::LaunchContextError> {
        let dock = PaneId::new("dock")?;
        let terminal = PaneId::new("terminal")?;

        assert_eq!(
            decide_toggle(DockState::Present {
                dock_pane_id: dock.clone(),
                focused_pane_id: terminal,
            }),
            ToggleDecision::Focus { pane_id: dock }
        );
        Ok(())
    }

    #[test]
    fn focused_dock_closes() -> Result<(), crate::host::LaunchContextError> {
        let dock = PaneId::new("dock")?;

        assert_eq!(
            decide_toggle(DockState::Present {
                dock_pane_id: dock.clone(),
                focused_pane_id: dock.clone(),
            }),
            ToggleDecision::Close { pane_id: dock }
        );
        Ok(())
    }
}
