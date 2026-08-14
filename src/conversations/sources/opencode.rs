use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsStr;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

#[cfg(unix)]
use cap_std::fs::MetadataExt;
use cap_std::fs::{Dir, Metadata};
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::{Connection, ErrorCode, OpenFlags, params};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::fd::AsRawFd;

use super::known_stores::KnownStore;
use super::{
    ConversationCandidate, ConversationRemoval, ConversationSource, ConversationSourceError,
    ConversationSourceErrorKind, DiscoveryBatch, DiscoveryLimit, MetadataBudget,
    ProjectAssociationEvidence, ProjectEvidenceKind, SourceId, SourceWatermark, StorageProbe,
};
use crate::conversations::{
    Conversation, ConversationProvenance, ConversationState, ProvenanceKind, ResumeCapability,
    ResumeReference, SessionReference, ToolIdentity,
};
use crate::project::{CanonicalPath, ProjectIdentity};

const SOURCE_ID: &str = "opencode";
const TOOL_ID: &str = "opencode";
const DATABASE_NAME: &str = "opencode.db";
const VERIFIED_VERSION: &str = "1.18.18";
const WATERMARK_VERSION: u8 = 1;
const MAX_ROWS: usize = 4_096;
const MAX_WATERMARK_BYTES: usize = 512 * 1024;
const MAX_ID_BYTES: usize = 128;
const MAX_TITLE_BYTES: usize = 256;
const MAX_VERSION_BYTES: usize = 32;
const QUERY_DEADLINE: Duration = Duration::from_millis(250);
const PROGRESS_OPS: i32 = 1_000;
const FNV_OFFSET: u64 = 14_695_981_039_346_656_037;
const FNV_PRIME: u64 = 1_099_511_628_211;

const MIGRATIONS: [&str; 38] = [
    "20260127222353_familiar_lady_ursula",
    "20260211171708_add_project_commands",
    "20260213144116_wakeful_the_professor",
    "20260225215848_workspace",
    "20260227213759_add_session_workspace_id",
    "20260228203230_blue_harpoon",
    "20260303231226_add_workspace_fields",
    "20260309230000_move_org_to_state",
    "20260312043431_session_message_cursor",
    "20260323234822_events",
    "20260410174513_workspace-name",
    "20260413175956_chief_energizer",
    "20260423070820_add_icon_url_override",
    "20260427172553_slow_nightmare",
    "20260428004200_add_session_path",
    "20260501142318_next_venus",
    "20260504145000_add_sync_owner",
    "20260507164347_add_workspace_time",
    "20260510033149_session_usage",
    "20260511000411_data_migration_state",
    "20260511173437_session-metadata",
    "20260601010001_normalize_storage_paths",
    "20260601202201_amazing_prowler",
    "20260602002951_lowly_union_jack",
    "20260602182828_add_project_directories",
    "20260603001617_session_message_projection_indexes",
    "20260603040000_session_message_projection_order",
    "20260603141458_session_input_inbox",
    "20260603160727_jittery_ezekiel_stane",
    "20260604172448_event_sourced_session_input",
    "20260605003541_add_session_context_snapshot",
    "20260605042240_add_context_epoch_agent",
    "20260611035744_credential",
    "20260611192811_lush_chimera",
    "20260612174303_project_dir_strategy",
    "20260622142730_simplify_session_context_epoch",
    "20260622170816_reset_v2_session_state",
    "20260622202450_simplify_session_input",
];

const MIGRATION_COLUMNS: [Column; 2] = [
    Column::new("id", "TEXT", false, 1),
    Column::new("time_completed", "INTEGER", true, 0),
];
const PROJECT_COLUMNS: [Column; 2] = [
    Column::new("id", "TEXT", false, 1),
    Column::new("worktree", "TEXT", true, 0),
];
const PROJECT_DIRECTORY_COLUMNS: [Column; 3] = [
    Column::new("project_id", "TEXT", true, 1),
    Column::new("directory", "TEXT", true, 2),
    Column::new("time_created", "INTEGER", true, 0),
];
const SESSION_COLUMNS: [Column; 9] = [
    Column::new("id", "TEXT", false, 1),
    Column::new("project_id", "TEXT", true, 0),
    Column::new("directory", "TEXT", true, 0),
    Column::new("title", "TEXT", true, 0),
    Column::new("version", "TEXT", true, 0),
    Column::new("time_created", "INTEGER", true, 0),
    Column::new("time_updated", "INTEGER", true, 0),
    Column::new("time_archived", "INTEGER", false, 0),
    Column::new("slug", "TEXT", true, 0),
];

const METADATA_QUERY: &str = "
SELECT
    substr(s.id, 1, 129), length(s.id),
    substr(s.title, 1, 257), length(s.title),
    substr(s.version, 1, 33), length(s.version),
    s.time_created, s.time_updated, s.time_archived,
    p.worktree = ?1,
    pd.directory IS NOT NULL
FROM session AS s
JOIN project AS p ON p.id = s.project_id
LEFT JOIN project_directory AS pd
  ON pd.project_id = s.project_id AND pd.directory = ?1
WHERE s.directory = ?1
  AND (p.worktree = ?1 OR pd.directory IS NOT NULL)
ORDER BY s.time_updated DESC, s.id ASC
LIMIT ?2
";

#[derive(Debug)]
pub struct OpenCodeSource {
    id: SourceId,
    project: ProjectIdentity,
    database_path: PathBuf,
    store: KnownStore,
    snapshots: Mutex<HashMap<String, ValidatedSession>>,
}

impl OpenCodeSource {
    pub fn new(
        project: ProjectIdentity,
        database_path: PathBuf,
    ) -> Result<Self, ConversationSourceError> {
        Self::new_with_source_id(project, database_path, source_id())
    }

    pub(crate) fn new_with_source_id(
        project: ProjectIdentity,
        database_path: PathBuf,
        id: SourceId,
    ) -> Result<Self, ConversationSourceError> {
        if !cfg!(unix) {
            return Err(ConversationSourceError::new(
                id,
                ConversationSourceErrorKind::PermissionDenied,
                "private OpenCode indexing is unsupported on this platform",
            ));
        }
        if !database_path.is_absolute() || database_path.file_name() != Some(DATABASE_NAME.as_ref())
        {
            return Err(ConversationSourceError::new(
                id,
                ConversationSourceErrorKind::InvalidData,
                "OpenCode database path must be absolute and end in opencode.db",
            )
            .with_path(database_path));
        }
        let parent = database_path
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| {
                ConversationSourceError::new(
                    id.clone(),
                    ConversationSourceErrorKind::InvalidData,
                    "OpenCode database path has no parent directory",
                )
                .with_path(database_path.clone())
            })?;
        Ok(Self {
            id,
            project,
            database_path,
            store: KnownStore::new(parent),
            snapshots: Mutex::new(HashMap::new()),
        })
    }

    fn error(
        &self,
        kind: ConversationSourceErrorKind,
        message: impl Into<String>,
    ) -> ConversationSourceError {
        ConversationSourceError::new(self.id.clone(), kind, message)
            .with_path(self.database_path.clone())
    }

    fn io_error(&self, message: &'static str, error: &io::Error) -> ConversationSourceError {
        let kind = if error.kind() == io::ErrorKind::PermissionDenied
            || error.raw_os_error() == Some(libc::ELOOP)
        {
            ConversationSourceErrorKind::PermissionDenied
        } else if error.kind() == io::ErrorKind::InvalidInput
            || error.raw_os_error() == Some(libc::ENXIO)
        {
            ConversationSourceErrorKind::InvalidData
        } else {
            ConversationSourceErrorKind::Io
        };
        self.error(kind, message)
    }

    fn sqlite_error(
        &self,
        message: &'static str,
        error: &rusqlite::Error,
    ) -> ConversationSourceError {
        let kind = match error {
            rusqlite::Error::SqliteFailure(code, _) => match code.code {
                ErrorCode::PermissionDenied => ConversationSourceErrorKind::PermissionDenied,
                ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase => {
                    ConversationSourceErrorKind::MalformedData
                }
                ErrorCode::AuthorizationForStatementDenied
                | ErrorCode::TooBig
                | ErrorCode::TypeMismatch => ConversationSourceErrorKind::InvalidData,
                ErrorCode::DatabaseBusy
                | ErrorCode::DatabaseLocked
                | ErrorCode::OperationInterrupted
                | ErrorCode::SchemaChanged
                | ErrorCode::ReadOnly
                | ErrorCode::CannotOpen
                | ErrorCode::SystemIoFailure
                | ErrorCode::FileLockingProtocolFailed => ConversationSourceErrorKind::Io,
                _ => ConversationSourceErrorKind::MalformedData,
            },
            rusqlite::Error::InvalidColumnType(..)
            | rusqlite::Error::InvalidColumnIndex(..)
            | rusqlite::Error::InvalidColumnName(..) => ConversationSourceErrorKind::MalformedData,
            _ => ConversationSourceErrorKind::Io,
        };
        self.error(kind, message)
    }

    fn open_snapshot(
        &self,
        cancelled: &AtomicBool,
    ) -> Result<OpenSnapshot, ConversationSourceError> {
        let directory = self
            .store
            .open_root_directory()
            .map_err(|error| self.io_error("OpenCode data directory cannot be confined", &error))?;
        let before = self.capture_database(&directory)?;
        if cancelled.load(Ordering::Relaxed) {
            return Err(self.error(
                ConversationSourceErrorKind::Io,
                "OpenCode discovery was cancelled",
            ));
        }
        let database_path = descriptor_database_path(&directory).map_err(|error| {
            self.io_error(
                "OpenCode data directory cannot be addressed by descriptor",
                &error,
            )
        })?;
        herdr_sqlite_vfs::register().map_err(|error| {
            self.io_error("OpenCode confined SQLite VFS is unavailable", &error)
        })?;
        let mut flags = OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW
            | OpenFlags::SQLITE_OPEN_EXRESCODE;
        let open_path = if before.state.wal.is_none() {
            flags |= OpenFlags::SQLITE_OPEN_URI;
            PathBuf::from(immutable_uri(&database_path)?)
        } else {
            database_path
        };
        let connection =
            Connection::open_with_flags_and_vfs(open_path, flags, herdr_sqlite_vfs::NAME).map_err(
                |error| self.sqlite_error("OpenCode database cannot be opened read-only", &error),
            )?;
        connection.busy_timeout(Duration::ZERO).map_err(|error| {
            self.sqlite_error("OpenCode database busy policy cannot be configured", &error)
        })?;
        connection
            .pragma_update(None, "query_only", true)
            .map_err(|error| {
                self.sqlite_error("OpenCode database cannot be restricted to queries", &error)
            })?;
        connection
            .pragma_update(None, "trusted_schema", false)
            .map_err(|error| {
                self.sqlite_error("OpenCode database schema cannot be sandboxed", &error)
            })?;
        let deadline = Instant::now() + QUERY_DEADLINE;
        connection
            .progress_handler(PROGRESS_OPS, Some(move || Instant::now() >= deadline))
            .map_err(|error| {
                self.sqlite_error("OpenCode query deadline cannot be configured", &error)
            })?;
        connection
            .authorizer(Some(authorize_metadata_query))
            .map_err(|error| {
                self.sqlite_error("OpenCode metadata allowlist cannot be configured", &error)
            })?;
        Ok(OpenSnapshot {
            connection,
            directory,
            captured: before,
        })
    }

    fn capture_database(
        &self,
        directory: &Dir,
    ) -> Result<CapturedDatabase, ConversationSourceError> {
        let (mut file, metadata) = KnownStore::open_file_in(directory, OsStr::new(DATABASE_NAME))
            .map_err(|error| {
            self.io_error("OpenCode database is not a confined regular file", &error)
        })?;
        let mut header = [0_u8; 20];
        file.read_exact(&mut header).map_err(|_| {
            self.error(
                ConversationSourceErrorKind::MalformedData,
                "OpenCode database header is truncated",
            )
        })?;
        if &header[..16] != b"SQLite format 3\0" {
            return Err(self.error(
                ConversationSourceErrorKind::MalformedData,
                "OpenCode database header is malformed",
            ));
        }
        if header[18] != 2 || header[19] != 2 {
            return Err(self.unsupported_schema());
        }
        let main = FileMark::from_metadata(&metadata)
            .map_err(|error| self.io_error("OpenCode database metadata is unavailable", &error))?;
        let wal = self.capture_sidecar(directory, "opencode.db-wal")?;
        let shm = self.capture_sidecar(directory, "opencode.db-shm")?;
        if wal.is_some() && shm.is_none() {
            return Err(self.error(
                ConversationSourceErrorKind::Io,
                "OpenCode WAL exists without its shared-memory index",
            ));
        }
        Ok(CapturedDatabase {
            main_len: metadata.len(),
            main_modified: metadata
                .modified()
                .map_err(|error| {
                    self.io_error("OpenCode database modification time is unavailable", &error)
                })?
                .into_std(),
            state: DatabaseState { main, wal },
        })
    }

    fn capture_sidecar(
        &self,
        directory: &Dir,
        name: &'static str,
    ) -> Result<Option<FileMark>, ConversationSourceError> {
        match KnownStore::open_file_in(directory, OsStr::new(name)) {
            Ok((_, metadata)) => FileMark::from_metadata(&metadata)
                .map(Some)
                .map_err(|error| self.io_error("OpenCode sidecar metadata is unavailable", &error)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => {
                Err(self.io_error("OpenCode sidecar is not a confined regular file", &error))
            }
        }
    }

    fn query_sessions(
        &self,
        connection: &Connection,
        project: &ProjectIdentity,
        cancelled: &AtomicBool,
    ) -> Result<Vec<ValidatedSession>, ConversationSourceError> {
        self.validate_schema(connection)?;
        if cancelled.load(Ordering::Relaxed) {
            return Err(self.error(
                ConversationSourceErrorKind::Io,
                "OpenCode discovery was cancelled",
            ));
        }
        let root = project.root().to_str().ok_or_else(|| {
            self.error(
                ConversationSourceErrorKind::InvalidData,
                "canonical project path cannot be represented in the OpenCode text schema",
            )
        })?;
        let row_limit = i64::try_from(MAX_ROWS + 1).expect("row bound fits i64");
        let mut statement = connection.prepare(METADATA_QUERY).map_err(|error| {
            self.sqlite_error("OpenCode metadata query cannot be prepared", &error)
        })?;
        if !statement.readonly() {
            return Err(self.error(
                ConversationSourceErrorKind::InvalidData,
                "OpenCode metadata query is not read-only",
            ));
        }
        let mut rows = statement
            .query(params![root, row_limit])
            .map_err(|error| self.sqlite_error("OpenCode metadata query cannot start", &error))?;
        let canonical_root = CanonicalPath::new(project.root().to_path_buf()).map_err(|_| {
            self.error(
                ConversationSourceErrorKind::InvalidData,
                "canonical project evidence is unavailable",
            )
        })?;
        let mut sessions = Vec::new();
        while let Some(row) = rows
            .next()
            .map_err(|error| self.sqlite_error("OpenCode metadata query failed", &error))?
        {
            if sessions.len() >= MAX_ROWS {
                return Err(self.error(
                    ConversationSourceErrorKind::InvalidData,
                    "OpenCode project session count exceeds the row limit",
                ));
            }
            if cancelled.load(Ordering::Relaxed) {
                return Err(self.error(
                    ConversationSourceErrorKind::Io,
                    "OpenCode discovery was cancelled",
                ));
            }
            let id: String = row.get(0).map_err(|error| {
                self.sqlite_error("OpenCode session identifier is malformed", &error)
            })?;
            let id_chars: i64 = row.get(1).map_err(|error| {
                self.sqlite_error("OpenCode session identifier length is malformed", &error)
            })?;
            let title: String = row.get(2).map_err(|error| {
                self.sqlite_error("OpenCode session title is malformed", &error)
            })?;
            let title_chars: i64 = row.get(3).map_err(|error| {
                self.sqlite_error("OpenCode session title length is malformed", &error)
            })?;
            let version: String = row.get(4).map_err(|error| {
                self.sqlite_error("OpenCode session version is malformed", &error)
            })?;
            let version_chars: i64 = row.get(5).map_err(|error| {
                self.sqlite_error("OpenCode session version length is malformed", &error)
            })?;
            if version != VERIFIED_VERSION {
                return Err(self.unsupported_schema());
            }
            let created_ms: i64 = row.get(6).map_err(|error| {
                self.sqlite_error("OpenCode creation timestamp is malformed", &error)
            })?;
            let updated_ms: i64 = row.get(7).map_err(|error| {
                self.sqlite_error("OpenCode update timestamp is malformed", &error)
            })?;
            let archived_ms: Option<i64> = row.get(8).map_err(|error| {
                self.sqlite_error("OpenCode archive timestamp is malformed", &error)
            })?;
            let worktree_match: bool = row.get(9).map_err(|error| {
                self.sqlite_error("OpenCode worktree evidence is malformed", &error)
            })?;
            let directory_match: bool = row.get(10).map_err(|error| {
                self.sqlite_error("OpenCode project-directory evidence is malformed", &error)
            })?;
            sessions.push(
                ValidatedSession::new(
                    id,
                    id_chars,
                    title,
                    title_chars,
                    version,
                    version_chars,
                    created_ms,
                    updated_ms,
                    archived_ms,
                    canonical_root.clone(),
                    worktree_match,
                    directory_match,
                )
                .map_err(|message| self.error(ConversationSourceErrorKind::InvalidData, message))?,
            );
        }
        Ok(sessions)
    }

    fn validate_schema(&self, connection: &Connection) -> Result<(), ConversationSourceError> {
        validate_columns(connection, "migration", &MIGRATION_COLUMNS)
            .map_err(|_| self.unsupported_schema())?;
        validate_columns(connection, "project", &PROJECT_COLUMNS)
            .map_err(|_| self.unsupported_schema())?;
        validate_columns(connection, "project_directory", &PROJECT_DIRECTORY_COLUMNS)
            .map_err(|_| self.unsupported_schema())?;
        validate_columns(connection, "session", &SESSION_COLUMNS)
            .map_err(|_| self.unsupported_schema())?;
        validate_foreign_key(connection, "project_directory")
            .map_err(|_| self.unsupported_schema())?;
        validate_foreign_key(connection, "session").map_err(|_| self.unsupported_schema())?;

        let mut statement = connection
            .prepare("SELECT id FROM migration ORDER BY id")
            .map_err(|_| self.unsupported_schema())?;
        let completed = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|_| self.unsupported_schema())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| self.unsupported_schema())?;
        let expected = MIGRATIONS
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if completed != expected {
            return Err(self.unsupported_schema());
        }
        Ok(())
    }

    fn unsupported_schema(&self) -> ConversationSourceError {
        self.error(
            ConversationSourceErrorKind::UnsupportedFormat,
            format!(
                "unsupported OpenCode database generation; expected fixture-backed OpenCode {VERIFIED_VERSION} schema"
            ),
        )
    }

    fn encode_watermark(
        &self,
        watermark: &Watermark,
    ) -> Result<SourceWatermark, ConversationSourceError> {
        let token = serde_json::to_string(watermark).map_err(|_| {
            self.error(
                ConversationSourceErrorKind::InvalidData,
                "OpenCode watermark cannot be encoded",
            )
        })?;
        if token.len() > MAX_WATERMARK_BYTES {
            return Err(self.error(
                ConversationSourceErrorKind::InvalidData,
                "OpenCode watermark exceeds the byte limit",
            ));
        }
        SourceWatermark::new(self.id.clone(), token)
    }

    fn decode_watermark(
        &self,
        after: Option<&SourceWatermark>,
    ) -> Result<Watermark, ConversationSourceError> {
        let Some(after) = after else {
            return Ok(Watermark::default());
        };
        if after.token().len() > MAX_WATERMARK_BYTES {
            return Err(self.error(
                ConversationSourceErrorKind::InvalidData,
                "OpenCode watermark exceeds the byte limit",
            ));
        }
        let watermark: Watermark = serde_json::from_str(after.token()).map_err(|_| {
            self.error(
                ConversationSourceErrorKind::InvalidData,
                "OpenCode watermark is malformed",
            )
        })?;
        if watermark.version != WATERMARK_VERSION || watermark.sessions.len() > MAX_ROWS {
            return Err(self.error(
                ConversationSourceErrorKind::InvalidData,
                "OpenCode watermark is incompatible",
            ));
        }
        Ok(watermark)
    }

    fn validate_candidate(
        &self,
        candidate: &ConversationCandidate,
    ) -> Result<ValidatedSession, ConversationSourceError> {
        if candidate.source_path() != Some(self.database_path.as_path()) {
            return Err(self.error(
                ConversationSourceErrorKind::SourceMismatch,
                "OpenCode candidate path does not match the configured database",
            ));
        }
        {
            let snapshots = self.snapshots.lock().map_err(|_| {
                self.error(
                    ConversationSourceErrorKind::Io,
                    "OpenCode metadata snapshot state is unavailable",
                )
            })?;
            snapshots
                .get(candidate.source_reference())
                .filter(|snapshot| candidate.fingerprint() == Some(snapshot.fingerprint.as_str()))
                .cloned()
                .ok_or_else(|| {
                    self.error(
                        ConversationSourceErrorKind::InvalidData,
                        "OpenCode candidate has no matching validated metadata snapshot",
                    )
                })
        }
    }
}

impl ConversationSource for OpenCodeSource {
    fn source_id(&self) -> &SourceId {
        &self.id
    }

    fn probe(&self) -> Result<StorageProbe, ConversationSourceError> {
        match self.store.probe() {
            Ok(false) => Ok(StorageProbe::Unavailable {
                reason: "OpenCode data directory is absent".to_owned(),
            }),
            Ok(true) => match self.store.open_file(Path::new(DATABASE_NAME)) {
                Ok(_) => Ok(StorageProbe::Available),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    Ok(StorageProbe::Unavailable {
                        reason: "OpenCode database is absent".to_owned(),
                    })
                }
                Err(error) => {
                    Err(self.io_error("OpenCode database is not a confined regular file", &error))
                }
            },
            Err(error) => Err(self.io_error("OpenCode data directory is unavailable", &error)),
        }
    }

    fn discover_raw(
        &self,
        project: &ProjectIdentity,
        after: Option<&SourceWatermark>,
        limit: DiscoveryLimit,
    ) -> Result<DiscoveryBatch, ConversationSourceError> {
        self.discover_raw_cancellable(project, after, limit, &AtomicBool::new(false))
    }

    fn discover_raw_cancellable(
        &self,
        project: &ProjectIdentity,
        after: Option<&SourceWatermark>,
        limit: DiscoveryLimit,
        cancelled: &AtomicBool,
    ) -> Result<DiscoveryBatch, ConversationSourceError> {
        if project != &self.project {
            return Err(self.error(
                ConversationSourceErrorKind::ProjectMismatch,
                "OpenCode source belongs to another project",
            ));
        }
        let previous = self.decode_watermark(after)?;
        if matches!(self.probe()?, StorageProbe::Unavailable { .. }) {
            let next = after
                .cloned()
                .map_or_else(|| self.encode_watermark(&Watermark::default()), Ok)?;
            return DiscoveryBatch::new(
                &self.id,
                project,
                Vec::new(),
                Some(next),
                Vec::new(),
                false,
                Vec::new(),
            );
        }
        let OpenSnapshot {
            connection,
            directory,
            captured,
        } = self.open_snapshot(cancelled)?;
        if previous
            .database
            .as_ref()
            .is_some_and(|database| database.main.identity() != captured.state.main.identity())
        {
            return Err(self.error(
                ConversationSourceErrorKind::Io,
                "OpenCode database was replaced since the previous snapshot",
            ));
        }
        if !previous.pending && previous.database.as_ref() == Some(&captured.state) {
            let next = self.encode_watermark(&previous)?;
            return DiscoveryBatch::new(
                &self.id,
                project,
                Vec::new(),
                Some(next),
                Vec::new(),
                false,
                Vec::new(),
            );
        }
        connection
            .execute_batch("BEGIN DEFERRED")
            .map_err(|error| self.sqlite_error("OpenCode read snapshot cannot start", &error))?;
        let sessions = self.query_sessions(&connection, project, cancelled)?;
        connection
            .execute_batch("COMMIT")
            .map_err(|error| self.sqlite_error("OpenCode read snapshot cannot finish", &error))?;
        let after_capture = self.capture_database(&directory)?;
        if captured.state.main.identity() != after_capture.state.main.identity() {
            return Err(self.error(
                ConversationSourceErrorKind::Io,
                "OpenCode database was replaced during discovery",
            ));
        }

        let current_ids = sessions
            .iter()
            .map(|session| session.id.clone())
            .collect::<BTreeSet<_>>();
        let mut next_sessions = BTreeMap::new();
        let mut candidates = Vec::new();
        let mut removals = Vec::new();
        let mut snapshots = self.snapshots.lock().map_err(|_| {
            self.error(
                ConversationSourceErrorKind::Io,
                "OpenCode metadata snapshot state is unavailable",
            )
        })?;
        snapshots.retain(|id, _| current_ids.contains(id));
        let mut has_more = false;
        for session in sessions {
            let unchanged = previous.sessions.get(&session.id) == Some(&session.fingerprint);
            if unchanged {
                next_sessions.insert(session.id.clone(), session.fingerprint.clone());
                snapshots.insert(session.id.clone(), session);
                continue;
            }
            if candidates.len() + removals.len() >= limit.get() {
                has_more = true;
                if let Some(fingerprint) = previous.sessions.get(&session.id) {
                    next_sessions.insert(session.id.clone(), fingerprint.clone());
                }
                continue;
            }
            let candidate = ConversationCandidate::new(
                self.id.clone(),
                project.clone(),
                session.id.clone(),
                Some(self.database_path.clone()),
                Some(captured.main_len),
                Some(captured.main_modified),
                Some(session.fingerprint.clone()),
            )?;
            next_sessions.insert(session.id.clone(), session.fingerprint.clone());
            snapshots.insert(session.id.clone(), session);
            candidates.push(candidate);
        }
        for session_id in previous.sessions.keys() {
            if current_ids.contains(session_id) {
                continue;
            }
            if candidates.len() + removals.len() >= limit.get() {
                has_more = true;
                next_sessions.insert(session_id.clone(), previous.sessions[session_id].clone());
                continue;
            }
            let reference = SessionReference::new(TOOL_ID, session_id).map_err(|_| {
                self.error(
                    ConversationSourceErrorKind::InvalidData,
                    "OpenCode watermark contains an invalid session identifier",
                )
            })?;
            removals.push(ConversationRemoval::new(self.id.clone(), reference));
        }
        drop(snapshots);
        let watermark = Watermark {
            version: WATERMARK_VERSION,
            database: Some(captured.state),
            pending: has_more,
            sessions: next_sessions,
        };
        let next = self.encode_watermark(&watermark)?;
        DiscoveryBatch::new(
            &self.id,
            project,
            candidates,
            Some(next),
            removals,
            has_more,
            Vec::new(),
        )
    }

    fn extract_metadata_raw(
        &self,
        candidate: &ConversationCandidate,
        budget: MetadataBudget,
    ) -> Result<Conversation, ConversationSourceError> {
        let metadata = self.validate_candidate(candidate)?;
        if metadata.metadata_bytes(&self.database_path) > budget.max_bytes() {
            return Err(self.error(
                ConversationSourceErrorKind::InvalidData,
                "OpenCode metadata exceeds the extraction budget",
            ));
        }
        let tool = ToolIdentity::new(TOOL_ID).expect("static tool ID is valid");
        let session = SessionReference::new(TOOL_ID, &metadata.id).map_err(|_| {
            self.error(
                ConversationSourceErrorKind::InvalidData,
                "OpenCode session reference is invalid",
            )
        })?;
        let resume = ResumeReference::new(&metadata.id).map_err(|_| {
            self.error(
                ConversationSourceErrorKind::InvalidData,
                "OpenCode resume reference is invalid",
            )
        })?;
        Conversation::new(
            tool,
            session,
            candidate.project_identity().clone(),
            Some(metadata.title),
            Some(metadata.created_at),
            metadata.archived_at,
            metadata.updated_at,
            if metadata.archived_at.is_some() {
                ConversationState::Archived
            } else {
                ConversationState::Live
            },
            vec![ConversationProvenance::new(
                self.id.clone(),
                ProvenanceKind::ExternalLocal,
                Some(self.database_path.clone()),
            )],
            ResumeCapability::Supported(resume),
        )
        .map_err(|_| {
            self.error(
                ConversationSourceErrorKind::InvalidData,
                "OpenCode conversation metadata is invalid",
            )
        })
    }

    fn project_evidence_raw(
        &self,
        candidate: &ConversationCandidate,
        _project: &ProjectIdentity,
    ) -> Result<Vec<ProjectAssociationEvidence>, ConversationSourceError> {
        let metadata = self.validate_candidate(candidate)?;
        let mut evidence = vec![ProjectAssociationEvidence::new(
            ProjectEvidenceKind::CanonicalWorkingDirectory,
            metadata.canonical_root.clone(),
            Some("OpenCode session.directory equals the canonical project root".to_owned()),
        )];
        let detail = if metadata.worktree_match && metadata.directory_match {
            "OpenCode project.worktree and project_directory both equal the canonical project root"
        } else if metadata.worktree_match {
            "OpenCode project.worktree equals the canonical project root"
        } else {
            "OpenCode project_directory equals the canonical project root"
        };
        evidence.push(ProjectAssociationEvidence::new(
            ProjectEvidenceKind::CanonicalWorkspaceRoot,
            metadata.canonical_root,
            Some(detail.to_owned()),
        ));
        Ok(evidence)
    }
}

#[derive(Clone, Copy)]
struct Column {
    name: &'static str,
    declared_type: &'static str,
    not_null: bool,
    primary_key: i64,
}

impl Column {
    const fn new(
        name: &'static str,
        declared_type: &'static str,
        not_null: bool,
        primary_key: i64,
    ) -> Self {
        Self {
            name,
            declared_type,
            not_null,
            primary_key,
        }
    }
}

fn validate_columns(
    connection: &Connection,
    table: &'static str,
    expected: &[Column],
) -> rusqlite::Result<()> {
    let sql = format!("PRAGMA table_info({table})");
    let mut statement = connection.prepare(&sql)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                (
                    row.get::<_, String>(2)?,
                    row.get::<_, bool>(3)?,
                    row.get::<_, i64>(5)?,
                ),
            ))
        })?
        .collect::<Result<HashMap<_, _>, _>>()?;
    for column in expected {
        let Some((declared_type, not_null, primary_key)) = rows.get(column.name) else {
            return Err(rusqlite::Error::InvalidColumnName(column.name.to_owned()));
        };
        if !declared_type.eq_ignore_ascii_case(column.declared_type)
            || *not_null != column.not_null
            || *primary_key != column.primary_key
        {
            return Err(rusqlite::Error::InvalidColumnType(
                0,
                column.name.to_owned(),
                rusqlite::types::Type::Text,
            ));
        }
    }
    Ok(())
}

fn validate_foreign_key(connection: &Connection, table: &'static str) -> rusqlite::Result<()> {
    let sql = format!("PRAGMA foreign_key_list({table})");
    let mut statement = connection.prepare(&sql)?;
    let valid = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(6)?,
            ))
        })?
        .filter_map(Result::ok)
        .any(|(target, from, to, on_delete)| {
            target == "project"
                && from == "project_id"
                && to == "id"
                && on_delete.eq_ignore_ascii_case("cascade")
        });
    if valid {
        Ok(())
    } else {
        Err(rusqlite::Error::InvalidQuery)
    }
}

fn authorize_metadata_query(context: AuthContext<'_>) -> Authorization {
    match context.action {
        AuthAction::Select | AuthAction::Transaction { .. } | AuthAction::Function { .. } => {
            Authorization::Allow
        }
        AuthAction::Read {
            table_name:
                "sqlite_master" | "sqlite_schema" | "migration" | "project" | "project_directory"
                | "session",
            ..
        }
        | AuthAction::Pragma {
            pragma_name: "journal_mode" | "table_info" | "foreign_key_list",
            ..
        } => Authorization::Allow,
        _ => Authorization::Deny,
    }
}

#[derive(Clone, Debug)]
struct ValidatedSession {
    id: String,
    title: String,
    version: String,
    created_at: SystemTime,
    updated_at: SystemTime,
    archived_at: Option<SystemTime>,
    canonical_root: CanonicalPath,
    worktree_match: bool,
    directory_match: bool,
    fingerprint: String,
}

impl ValidatedSession {
    #[allow(clippy::too_many_arguments)]
    fn new(
        id: String,
        id_chars: i64,
        title: String,
        title_chars: i64,
        version: String,
        version_chars: i64,
        created_ms: i64,
        updated_ms: i64,
        archived_ms: Option<i64>,
        canonical_root: CanonicalPath,
        worktree_match: bool,
        directory_match: bool,
    ) -> Result<Self, &'static str> {
        if id_chars <= 0
            || usize::try_from(id_chars)
                .ok()
                .is_none_or(|length| length > MAX_ID_BYTES)
            || id.len() > MAX_ID_BYTES
            || !valid_session_id(&id)
        {
            return Err("OpenCode session identifier is outside the fixture-backed bound");
        }
        if title_chars <= 0
            || usize::try_from(title_chars)
                .ok()
                .is_none_or(|length| length > MAX_TITLE_BYTES)
            || title.is_empty()
            || title.len() > MAX_TITLE_BYTES
        {
            return Err("OpenCode session title is outside the metadata bound");
        }
        if version_chars <= 0
            || usize::try_from(version_chars)
                .ok()
                .is_none_or(|length| length > MAX_VERSION_BYTES)
            || version.is_empty()
            || version.len() > MAX_VERSION_BYTES
        {
            return Err("OpenCode session version is outside the metadata bound");
        }
        if !worktree_match && !directory_match {
            return Err("OpenCode session lacks canonical project evidence");
        }
        let created_at = timestamp_from_millis(created_ms)?;
        let updated_at = timestamp_from_millis(updated_ms)?;
        let archived_at = archived_ms.map(timestamp_from_millis).transpose()?;
        if updated_at < created_at {
            return Err("OpenCode session timestamps are inconsistent");
        }
        let fingerprint = session_fingerprint(
            &id,
            &title,
            &version,
            (created_ms, updated_ms, archived_ms),
            (worktree_match, directory_match),
        );
        Ok(Self {
            id,
            title,
            version,
            created_at,
            updated_at,
            archived_at,
            canonical_root,
            worktree_match,
            directory_match,
            fingerprint,
        })
    }

    fn metadata_bytes(&self, database_path: &Path) -> usize {
        self.id
            .len()
            .saturating_add(self.title.len())
            .saturating_add(self.version.len())
            .saturating_add(TOOL_ID.len().saturating_mul(3))
            .saturating_add(self.canonical_root.as_path().as_os_str().len())
            .saturating_add(database_path.as_os_str().len())
    }
}

fn valid_session_id(id: &str) -> bool {
    id.len() == 30
        && id.starts_with("ses_")
        && id[4..].bytes().all(|byte| byte.is_ascii_alphanumeric())
}

fn timestamp_from_millis(value: i64) -> Result<SystemTime, &'static str> {
    let millis = u64::try_from(value).map_err(|_| "OpenCode timestamp is negative")?;
    SystemTime::UNIX_EPOCH
        .checked_add(Duration::from_millis(millis))
        .ok_or("OpenCode timestamp is outside the supported range")
}

fn session_fingerprint(
    id: &str,
    title: &str,
    version: &str,
    times: (i64, i64, Option<i64>),
    evidence: (bool, bool),
) -> String {
    let mut hash = FNV_OFFSET;
    for bytes in [id.as_bytes(), title.as_bytes(), version.as_bytes()] {
        hash = hash_bytes(hash, bytes);
        hash = hash_bytes(hash, &[0]);
    }
    hash = hash_bytes(hash, &times.0.to_le_bytes());
    hash = hash_bytes(hash, &times.1.to_le_bytes());
    hash = hash_bytes(hash, &times.2.unwrap_or(-1).to_le_bytes());
    hash = hash_bytes(hash, &[u8::from(evidence.0), u8::from(evidence.1)]);
    format!("{hash:016x}")
}

fn hash_bytes(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileMark {
    len: u64,
    modified_secs: u64,
    modified_nanos: u32,
    modified_before_epoch: bool,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl FileMark {
    fn from_metadata(metadata: &Metadata) -> io::Result<Self> {
        let modified = metadata.modified()?.into_std();
        let (duration, modified_before_epoch) =
            modified.duration_since(SystemTime::UNIX_EPOCH).map_or_else(
                |error| (error.duration(), true),
                |duration| (duration, false),
            );
        Ok(Self {
            len: metadata.len(),
            modified_secs: duration.as_secs(),
            modified_nanos: duration.subsec_nanos(),
            modified_before_epoch,
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
        })
    }

    #[cfg(unix)]
    const fn identity(&self) -> (u64, u64) {
        (self.device, self.inode)
    }

    #[cfg(not(unix))]
    const fn identity(&self) -> (u64, u64) {
        (self.len, self.modified_secs)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DatabaseState {
    main: FileMark,
    wal: Option<FileMark>,
}

struct CapturedDatabase {
    state: DatabaseState,
    main_len: u64,
    main_modified: SystemTime,
}
struct OpenSnapshot {
    connection: Connection,
    directory: Dir,
    captured: CapturedDatabase,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Watermark {
    version: u8,
    database: Option<DatabaseState>,
    pending: bool,
    sessions: BTreeMap<String, String>,
}

impl Default for Watermark {
    fn default() -> Self {
        Self {
            version: WATERMARK_VERSION,
            database: None,
            pending: false,
            sessions: BTreeMap::new(),
        }
    }
}

#[cfg(unix)]
fn descriptor_database_path(directory: &Dir) -> io::Result<PathBuf> {
    #[cfg(any(target_os = "android", target_os = "linux"))]
    const DESCRIPTOR_DIRECTORY: &str = "/proc/self/fd";
    #[cfg(not(any(target_os = "android", target_os = "linux")))]
    const DESCRIPTOR_DIRECTORY: &str = "/dev/fd";

    Ok(PathBuf::from(DESCRIPTOR_DIRECTORY)
        .join(directory.as_raw_fd().to_string())
        .join(DATABASE_NAME))
}

#[cfg(not(unix))]
fn descriptor_database_path(_directory: &Dir) -> io::Result<PathBuf> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "descriptor-relative SQLite paths are unavailable on this platform",
    ))
}

#[cfg(unix)]
fn immutable_uri(path: &Path) -> Result<String, ConversationSourceError> {
    use std::os::unix::ffi::OsStrExt;

    let mut uri =
        String::with_capacity(path.as_os_str().len().saturating_mul(3).saturating_add(25));
    uri.push_str("file:");
    for byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'/' | b'-' | b'.' | b'_' | b'~') {
            uri.push(char::from(*byte));
        } else {
            const HEX: &[u8; 16] = b"0123456789ABCDEF";
            uri.push('%');
            uri.push(char::from(HEX[usize::from(byte >> 4)]));
            uri.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    uri.push_str("?immutable=1&mode=ro");
    Ok(uri)
}

#[cfg(not(unix))]
fn immutable_uri(path: &Path) -> Result<String, ConversationSourceError> {
    path.to_str()
        .map(|path| format!("file:{path}?immutable=1&mode=ro"))
        .ok_or_else(|| {
            ConversationSourceError::new(
                source_id(),
                ConversationSourceErrorKind::InvalidData,
                "OpenCode database path cannot be represented as a SQLite URI",
            )
        })
}

fn source_id() -> SourceId {
    SourceId::new(SOURCE_ID).expect("static source ID is valid")
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::{DATABASE_NAME, KnownStore, descriptor_database_path};

    #[test]
    fn descriptor_path_survives_parent_replacement() {
        let temporary = TempDir::new().expect("temporary directory");
        let live = temporary.path().join("opencode");
        let moved = temporary.path().join("moved");
        fs::create_dir(&live).expect("live directory");
        fs::write(live.join(DATABASE_NAME), b"original").expect("original database");

        let directory = KnownStore::new(live.clone())
            .open_root_directory()
            .expect("confined directory");
        fs::rename(&live, &moved).expect("move original directory");
        fs::create_dir(&live).expect("replacement directory");
        fs::write(live.join(DATABASE_NAME), b"replacement").expect("replacement database");

        let descriptor_path =
            descriptor_database_path(&directory).expect("descriptor database path");
        assert_eq!(
            fs::read(descriptor_path).expect("descriptor database"),
            b"original"
        );
    }
}
