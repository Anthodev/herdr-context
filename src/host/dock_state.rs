//! Persisted "dock open in (workspace, tab)" records for one plugin state dir.
//!
//! Toggle records where a dock is open, a `restore` startup hook re-opens those
//! docks after a Herdr server restart, and a `pane.exited` event hook prunes
//! records whose dock pane died out-of-band. State is keyed by the server's
//! socket path because named sessions run separate servers against one global
//! state dir; without the key, one server's restore could steal another
//! server's records.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use serde::{Deserialize, Serialize};

use super::launch::{LockError, ensure_private_directory, open_private_lock_file};

const STATE_FILE_VERSION: u8 = 1;
const STATE_FILE_NAME: &str = "docks.json";
const LOCK_FILE_NAME: &str = "docks.lock";
const TMP_FILE_NAME: &str = "docks.json.tmp";
const LOCK_TIMEOUT: Duration = Duration::from_secs(2);
/// Above the worst legitimate content (8 servers x 256 short records) so a
/// full file still loads while hostile or corrupted files stay bounded.
const MAX_STATE_BYTES: u64 = 512 * 1024;
const MAX_SERVERS: usize = 8;
const MAX_RECORDS_PER_SERVER: usize = 256;

/// One persisted dock: the workspace tab it docks into, its pane, and width.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DockRecord {
    pub workspace_id: String,
    pub tab_id: String,
    pub dock_pane_id: String,
    pub width: u16,
}

/// Docks open per Herdr server, keyed by the server's socket path.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DockStateFile {
    version: u8,
    #[serde(default)]
    servers: BTreeMap<String, Vec<DockRecord>>,
}

impl Default for DockStateFile {
    fn default() -> Self {
        Self {
            version: STATE_FILE_VERSION,
            servers: BTreeMap::new(),
        }
    }
}

impl DockStateFile {
    /// Records persisted for one server socket; empty when unknown.
    #[must_use]
    pub fn records_for(&self, socket: &str) -> &[DockRecord] {
        self.servers.get(socket).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Inserts or replaces the record for the same (workspace, tab) pair.
    ///
    /// Replacement always succeeds; new inserts are refused beyond
    /// [`MAX_SERVERS`] sockets or [`MAX_RECORDS_PER_SERVER`] records per socket.
    pub fn upsert(&mut self, socket: &str, record: DockRecord) -> Result<(), DockStateError> {
        let Some(records) = self.servers.get_mut(socket) else {
            if self.servers.len() >= MAX_SERVERS {
                return Err(DockStateError::ServerCapReached { cap: MAX_SERVERS });
            }
            self.servers.insert(socket.to_owned(), vec![record]);
            return Ok(());
        };
        if let Some(existing) = records.iter_mut().find(|existing| {
            existing.workspace_id == record.workspace_id && existing.tab_id == record.tab_id
        }) {
            *existing = record;
            return Ok(());
        }
        if records.len() >= MAX_RECORDS_PER_SERVER {
            return Err(DockStateError::RecordCapReached {
                socket: socket.to_owned(),
                cap: MAX_RECORDS_PER_SERVER,
            });
        }
        records.push(record);
        Ok(())
    }

    /// Removes the record for a (workspace, tab) pair; reports whether anything changed.
    pub fn remove(&mut self, socket: &str, workspace_id: &str, tab_id: &str) -> bool {
        let Some(records) = self.servers.get_mut(socket) else {
            return false;
        };
        let before = records.len();
        records.retain(|record| !(record.workspace_id == workspace_id && record.tab_id == tab_id));
        let removed = records.len() != before;
        if records.is_empty() {
            self.servers.remove(socket);
        }
        removed
    }

    /// Removes records whose dock pane exited; reports whether anything changed.
    pub fn remove_by_pane(&mut self, socket: &str, pane_id: &str) -> bool {
        let Some(records) = self.servers.get_mut(socket) else {
            return false;
        };
        let before = records.len();
        records.retain(|record| record.dock_pane_id != pane_id);
        let removed = records.len() != before;
        if records.is_empty() {
            self.servers.remove(socket);
        }
        removed
    }

    /// Replaces one socket's records; an empty vector removes the section.
    fn set_records(&mut self, socket: &str, records: Vec<DockRecord>) {
        if records.is_empty() {
            self.servers.remove(socket);
        } else {
            self.servers.insert(socket.to_owned(), records);
        }
    }

    /// Bounds a file read back from disk; over-cap content only appears through
    /// external tampering, so it is truncated instead of trusted.
    fn clamped(mut self) -> Self {
        for records in self.servers.values_mut() {
            records.truncate(MAX_RECORDS_PER_SERVER);
        }
        while self.servers.len() > MAX_SERVERS {
            self.servers.pop_last();
        }
        self
    }
}

/// Loads the dock state file, falling back to an empty state on any problem.
///
/// Missing files, corrupt JSON, unknown versions, and over-size files all
/// degrade to an empty state; loading never fails.
#[must_use]
pub fn load(state_dir: &Path) -> DockStateFile {
    let path = state_dir.join(STATE_FILE_NAME);
    let bytes = match read_bounded_regular_file(&path) {
        Ok(Some(bytes)) => bytes,
        Ok(None) | Err(_) => return DockStateFile::default(),
    };
    let Ok(state) = serde_json::from_slice::<DockStateFile>(&bytes) else {
        return DockStateFile::default();
    };
    if state.version != STATE_FILE_VERSION {
        return DockStateFile::default();
    }
    state.clamped()
}

/// Persists one record under the state lock, re-reading the file before writing
/// so a concurrent toggle in another process is never clobbered.
pub fn upsert_record(
    state_dir: &Path,
    socket: &str,
    record: DockRecord,
) -> Result<(), DockStateError> {
    with_locked_state(state_dir, |state| {
        state.upsert(socket, record).map(|()| ((), true))
    })
}

/// Drops the record for a (workspace, tab) pair under the state lock; reports
/// whether anything changed. Nothing is written when nothing changed.
pub fn remove_record(
    state_dir: &Path,
    socket: &str,
    workspace_id: &str,
    tab_id: &str,
) -> Result<bool, DockStateError> {
    with_locked_state(state_dir, |state| {
        let removed = state.remove(socket, workspace_id, tab_id);
        Ok((removed, removed))
    })
}

/// Drops records whose dock pane exited under the state lock; reports whether
/// anything changed. Nothing is written when nothing changed.
pub fn remove_by_pane(
    state_dir: &Path,
    socket: &str,
    pane_id: &str,
) -> Result<bool, DockStateError> {
    with_locked_state(state_dir, |state| {
        let removed = state.remove_by_pane(socket, pane_id);
        Ok((removed, removed))
    })
}

/// Overlays one socket's processed records under the state lock, leaving other
/// sockets untouched; an empty vector prunes the socket's section.
pub fn replace_socket_records(
    state_dir: &Path,
    socket: &str,
    records: Vec<DockRecord>,
) -> Result<(), DockStateError> {
    with_locked_state(state_dir, |state| {
        state.set_records(socket, records);
        Ok(((), true))
    })
}

/// Mutates the state file under its lock: the file is re-read after the lock is
/// held, so the closure always sees the latest content, and the write is
/// skipped when the closure leaves the state unchanged.
///
/// Callers must already hold any narrower lock (a tab lock) before calling:
/// the lock order is always tab lock first, then this state lock.
fn with_locked_state<T>(
    state_dir: &Path,
    apply: impl FnOnce(&mut DockStateFile) -> Result<(T, bool), DockStateError>,
) -> Result<T, DockStateError> {
    ensure_private_directory(state_dir).map_err(DockStateError::Lock)?;
    let _lock = acquire_lock(&state_dir.join(LOCK_FILE_NAME))?;
    let mut state = load(state_dir);
    let (result, dirty) = apply(&mut state)?;
    if dirty {
        write_state_file(state_dir, &state)?;
    }
    Ok(result)
}

fn acquire_lock(path: &Path) -> Result<File, DockStateError> {
    let file = open_private_lock_file(path).map_err(DockStateError::Lock)?;
    let deadline = Instant::now()
        .checked_add(LOCK_TIMEOUT)
        .ok_or(DockStateError::Lock(LockError::InvalidTimeout(
            LOCK_TIMEOUT,
        )))?;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(TryLockError::WouldBlock) => {
                return Err(DockStateError::Lock(LockError::Timeout {
                    path: path.to_path_buf(),
                    timeout: LOCK_TIMEOUT,
                }));
            }
            Err(TryLockError::Error(error)) => {
                return Err(DockStateError::Lock(LockError::Io {
                    operation: "lock",
                    path: path.to_path_buf(),
                    source: error,
                }));
            }
        }
    }
}

fn write_state_file(state_dir: &Path, state: &DockStateFile) -> Result<(), DockStateError> {
    let bytes = serde_json::to_vec(state).map_err(DockStateError::Encoding)?;
    let tmp_path = state_dir.join(TMP_FILE_NAME);
    let mut tmp = open_private_lock_file(&tmp_path).map_err(DockStateError::Lock)?;
    tmp.set_len(0).map_err(|source| DockStateError::Io {
        operation: "truncate state file",
        path: tmp_path.clone(),
        source,
    })?;
    tmp.write_all(&bytes).map_err(|source| DockStateError::Io {
        operation: "write state file",
        path: tmp_path.clone(),
        source,
    })?;
    std::fs::rename(&tmp_path, state_dir.join(STATE_FILE_NAME)).map_err(|source| {
        DockStateError::Io {
            operation: "publish state file",
            path: tmp_path,
            source,
        }
    })?;
    Ok(())
}

fn read_bounded_regular_file(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_file() || metadata.len() > MAX_STATE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "dock state file is not a bounded regular file",
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let mut file: File = options.open(path)?;
    let opened = file.metadata()?;
    if !opened.is_file() || opened.len() > MAX_STATE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "dock state file changed during open",
        ));
    }
    let capacity = usize::try_from(opened.len()).unwrap_or(0);
    let mut bytes = Vec::with_capacity(capacity);
    (&mut file)
        .take(MAX_STATE_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_STATE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "dock state file exceeds the byte limit",
        ));
    }
    Ok(Some(bytes))
}

#[derive(Debug)]
pub enum DockStateError {
    Lock(LockError),
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    Encoding(serde_json::Error),
    ServerCapReached {
        cap: usize,
    },
    RecordCapReached {
        socket: String,
        cap: usize,
    },
}

impl fmt::Display for DockStateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lock(error) => write!(formatter, "dock state lock failed: {error}"),
            Self::Io {
                operation,
                path,
                source,
            } => write!(
                formatter,
                "could not {operation} {}: {source}",
                path.display()
            ),
            Self::Encoding(error) => write!(formatter, "could not encode dock state: {error}"),
            Self::ServerCapReached { cap } => write!(
                formatter,
                "dock state already tracks {cap} servers; refusing to track another"
            ),
            Self::RecordCapReached { socket, cap } => write!(
                formatter,
                "dock state already tracks {cap} docks for {socket}; refusing to track another"
            ),
        }
    }
}

impl Error for DockStateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Lock(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            Self::Encoding(error) => Some(error),
            Self::ServerCapReached { .. } | Self::RecordCapReached { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(workspace: &str, tab: &str, pane: &str) -> DockRecord {
        DockRecord {
            workspace_id: workspace.to_owned(),
            tab_id: tab.to_owned(),
            dock_pane_id: pane.to_owned(),
            width: 40,
        }
    }

    #[test]
    fn upsert_replaces_by_workspace_and_tab_pair() {
        let mut state = DockStateFile::default();
        state
            .upsert("sock", record("ws", "tab", "pane-a"))
            .expect("insert");
        state
            .upsert("sock", record("ws", "tab", "pane-b"))
            .expect("replace");

        assert_eq!(state.records_for("sock"), &[record("ws", "tab", "pane-b")]);
    }

    #[test]
    fn records_are_isolated_per_socket() {
        let mut state = DockStateFile::default();
        state
            .upsert("sock-a", record("ws", "tab", "pane-a"))
            .expect("insert");
        state
            .upsert("sock-b", record("ws", "tab", "pane-b"))
            .expect("insert");

        assert_eq!(
            state.records_for("sock-a"),
            &[record("ws", "tab", "pane-a")]
        );
        assert_eq!(
            state.records_for("sock-b"),
            &[record("ws", "tab", "pane-b")]
        );
        assert!(state.records_for("sock-c").is_empty());
    }

    #[test]
    fn remove_targets_the_pair_and_prunes_empty_sections() {
        let mut state = DockStateFile::default();
        state
            .upsert("sock", record("ws", "tab", "pane-a"))
            .expect("insert");
        state
            .upsert("sock", record("ws", "other", "pane-b"))
            .expect("insert");

        assert!(state.remove("sock", "ws", "tab"));
        assert_eq!(
            state.records_for("sock"),
            &[record("ws", "other", "pane-b")]
        );
        assert!(state.remove("sock", "ws", "other"));
        assert!(!state.servers.contains_key("sock"));
        assert!(!state.remove("sock", "ws", "tab"));
    }

    #[test]
    fn remove_by_pane_matches_only_the_pane_id() {
        let mut state = DockStateFile::default();
        state
            .upsert("sock", record("ws", "tab", "pane-a"))
            .expect("insert");

        assert!(!state.remove_by_pane("sock", "ws"));
        assert!(!state.remove_by_pane("other", "pane-a"));
        assert!(state.remove_by_pane("sock", "pane-a"));
        assert!(state.records_for("sock").is_empty());
    }

    #[test]
    fn caps_refuse_new_inserts_but_allow_replacement() {
        let mut state = DockStateFile::default();
        for index in 0..MAX_SERVERS {
            state
                .upsert(&format!("sock-{index}"), record("ws", "tab", "pane"))
                .expect("server insert");
        }
        assert!(matches!(
            state.upsert("sock-extra", record("ws", "tab", "pane")),
            Err(DockStateError::ServerCapReached { .. })
        ));

        // "sock-0" already holds one record from the server loop above.
        for index in 1..MAX_RECORDS_PER_SERVER {
            state
                .upsert("sock-0", record("ws", &format!("tab-{index}"), "pane"))
                .expect("record insert");
        }
        assert!(matches!(
            state.upsert("sock-0", record("ws", "tab-new", "pane")),
            Err(DockStateError::RecordCapReached { .. })
        ));
        state
            .upsert("sock-0", record("ws", "tab-1", "replaced"))
            .expect("replacement passes the cap");
    }

    #[test]
    fn store_and_load_roundtrip_per_socket() {
        let state_dir = tempfile::tempdir().expect("tempdir");
        upsert_record(state_dir.path(), "sock-a", record("ws", "tab", "pane-a")).expect("store");
        upsert_record(state_dir.path(), "sock-b", record("ws2", "tab2", "pane-b")).expect("store");

        let state = load(state_dir.path());
        assert_eq!(
            state.records_for("sock-a"),
            &[record("ws", "tab", "pane-a")]
        );
        assert_eq!(
            state.records_for("sock-b"),
            &[record("ws2", "tab2", "pane-b")]
        );
    }

    #[test]
    fn remove_record_writes_only_when_something_changed() {
        let state_dir = tempfile::tempdir().expect("tempdir");
        upsert_record(state_dir.path(), "sock", record("ws", "tab", "pane")).expect("store");

        assert!(remove_record(state_dir.path(), "sock", "ws", "tab").expect("remove"));
        assert!(!remove_record(state_dir.path(), "sock", "ws", "tab").expect("remove"));
        assert_eq!(load(state_dir.path()), DockStateFile::default());

        let untouched = tempfile::tempdir().expect("tempdir");
        assert!(!remove_record(untouched.path(), "sock", "ws", "tab").expect("remove"));
        assert!(!untouched.path().join(STATE_FILE_NAME).exists());
    }

    #[test]
    fn replace_socket_records_overlays_only_one_socket() {
        let state_dir = tempfile::tempdir().expect("tempdir");
        upsert_record(state_dir.path(), "sock-a", record("ws", "tab", "pane-a")).expect("store");

        replace_socket_records(
            state_dir.path(),
            "sock-b",
            vec![record("ws2", "tab2", "pane-b")],
        )
        .expect("overlay");
        let state = load(state_dir.path());
        assert_eq!(
            state.records_for("sock-a"),
            &[record("ws", "tab", "pane-a")]
        );
        assert_eq!(
            state.records_for("sock-b"),
            &[record("ws2", "tab2", "pane-b")]
        );

        replace_socket_records(state_dir.path(), "sock-a", Vec::new()).expect("prune");
        let state = load(state_dir.path());
        assert!(state.records_for("sock-a").is_empty());
        assert_eq!(
            state.records_for("sock-b"),
            &[record("ws2", "tab2", "pane-b")]
        );
    }

    #[test]
    fn load_falls_back_to_empty_on_corrupt_wrong_version_or_oversize_files() {
        let state_dir = tempfile::tempdir().expect("tempdir");
        let path = state_dir.path().join(STATE_FILE_NAME);

        std::fs::write(&path, b"{not json").expect("corrupt file");
        assert_eq!(load(state_dir.path()), DockStateFile::default());

        std::fs::write(&path, br#"{"version":9,"servers":{}}"#).expect("future version");
        assert_eq!(load(state_dir.path()), DockStateFile::default());

        std::fs::write(&path, br#"{"servers":{}}"#).expect("missing version");
        assert_eq!(load(state_dir.path()), DockStateFile::default());

        std::fs::write(&path, vec![b'x'; (MAX_STATE_BYTES + 1) as usize]).expect("oversize file");
        assert_eq!(load(state_dir.path()), DockStateFile::default());

        let missing = tempfile::tempdir().expect("tempdir");
        assert_eq!(load(missing.path()), DockStateFile::default());
    }

    #[test]
    fn load_clamps_oversized_sections_from_tampered_files() {
        let state_dir = tempfile::tempdir().expect("tempdir");
        let mut servers = BTreeMap::new();
        for server in 0..(MAX_SERVERS + 1) {
            servers.insert(
                format!("sock-{server}"),
                (0..(MAX_RECORDS_PER_SERVER + 1))
                    .map(|index| record("ws", &format!("tab-{index}"), "pane"))
                    .collect::<Vec<_>>(),
            );
        }
        let tampered = serde_json::to_vec(&serde_json::json!({
            "version": STATE_FILE_VERSION,
            "servers": servers,
        }))
        .expect("encode tampered state");
        std::fs::write(state_dir.path().join(STATE_FILE_NAME), tampered).expect("write");

        let state = load(state_dir.path());
        assert_eq!(state.servers.len(), MAX_SERVERS);
        assert!(
            state
                .servers
                .values()
                .all(|records| records.len() == MAX_RECORDS_PER_SERVER)
        );
    }
}
