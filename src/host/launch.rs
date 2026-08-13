use std::error::Error;
use std::fmt;
use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

use super::{
    DEFAULT_DOCK_WIDTH, DockIdentity, DockWidth, HostClient, HostError, HostPane, LaunchContext,
    OpenDockRequest, PaneId, TabId, WorkspaceId,
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
}

impl DockLauncher {
    #[must_use]
    pub const fn new(state_dir: PathBuf) -> Self {
        Self {
            state_dir,
            lock_timeout: Duration::from_secs(2),
        }
    }

    #[must_use]
    pub const fn with_lock_timeout(mut self, timeout: Duration) -> Self {
        self.lock_timeout = timeout;
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
                    DockWidth::clamped(DEFAULT_DOCK_WIDTH),
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
                Ok(ToggleOutcome::Closed)
            }
        }
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

fn ensure_private_directory(path: &Path) -> Result<(), LockError> {
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

fn open_private_lock_file(path: &Path) -> Result<File, LockError> {
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
    Invariant(String),
}

impl fmt::Display for LauncherError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lock(error) => write!(formatter, "dock lock failed: {error}"),
            Self::Host(error) => write!(formatter, "Herdr operation failed: {error}"),
            Self::Invariant(message) => write!(formatter, "dock invariant failed: {message}"),
        }
    }
}

impl Error for LauncherError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Lock(error) => Some(error),
            Self::Host(error) => Some(error),
            Self::Invariant(_) => None,
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
