use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use cap_fs_ext::{OpenOptionsFollowExt, OpenOptionsMaybeDirExt};
use cap_primitives::fs::FollowSymlinks;
#[cfg(unix)]
use cap_std::fs::MetadataExt;
use cap_std::fs::{Dir, File, Metadata};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

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

const MAX_STORE_FILES: usize = 4_096;
const MAX_DIRECTORY_ENTRIES: usize = 8_192;
pub(super) const MAX_CANDIDATE_PATHS: usize = MAX_DIRECTORY_ENTRIES;
const MAX_FILE_BYTES: u64 = 512 * 1024;
const MAX_RECORD_BYTES: usize = 256 * 1024;
const PREFIX_GUARD_BYTES: u64 = 4 * 1024;
const FNV_OFFSET: u64 = 14_695_981_039_346_656_037;
const MAX_RECORDS: usize = 32_768;
const MAX_WATERMARK_BYTES: usize = 4 * 1024 * 1024;
const MAX_METADATA_TITLE_BYTES: usize = 256;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KnownStoreRoots {
    claude_code: PathBuf,
    codex_cli: PathBuf,
    pi: PathBuf,
    omp: PathBuf,
    opencode: PathBuf,
}

impl KnownStoreRoots {
    #[must_use]
    pub fn under_home(home: impl AsRef<Path>) -> Self {
        Self::with_overrides(home, None, None)
    }

    #[must_use]
    pub fn with_overrides(
        home: impl AsRef<Path>,
        codex_home: Option<&Path>,
        claude_config_dir: Option<&Path>,
    ) -> Self {
        let home = home.as_ref();
        Self {
            claude_code: claude_config_dir
                .map_or_else(|| home.join(".claude"), Path::to_path_buf)
                .join("projects"),
            codex_cli: codex_home
                .map_or_else(|| home.join(".codex"), Path::to_path_buf)
                .join("sessions"),
            pi: home.join(".pi/agent/sessions"),
            omp: home.join(".omp/agent/sessions"),
            opencode: home.join(".local/share/opencode/opencode.db"),
        }
    }

    #[must_use]
    pub fn new(
        claude_code: impl Into<PathBuf>,
        codex_cli: impl Into<PathBuf>,
        pi: impl Into<PathBuf>,
        omp: impl Into<PathBuf>,
        opencode: impl Into<PathBuf>,
    ) -> Self {
        Self {
            claude_code: claude_code.into(),
            codex_cli: codex_cli.into(),
            pi: pi.into(),
            omp: omp.into(),
            opencode: opencode.into(),
        }
    }

    #[must_use]
    pub fn claude_code(&self) -> &Path {
        &self.claude_code
    }

    #[must_use]
    pub fn codex_cli(&self) -> &Path {
        &self.codex_cli
    }

    #[must_use]
    pub fn omp(&self) -> &Path {
        &self.omp
    }
    #[must_use]
    pub fn opencode(&self) -> &Path {
        &self.opencode
    }

    #[must_use]
    pub fn pi(&self) -> &Path {
        &self.pi
    }
}

#[derive(Clone, Debug)]
pub(super) enum ParseOutcome {
    Metadata(ParsedMetadata),
    IdentityPending(PendingMetadata),
}

pub(super) trait KnownFormat: std::fmt::Debug + Send + Sync + 'static {
    fn source_id(&self) -> &'static str;
    fn tool_id(&self) -> &'static str;

    fn report_project_mismatch(&self) -> bool {
        true
    }

    /// Increment when a format starts accepting records previously cached as rejected.
    fn adapter_revision(&self) -> u32 {
        0
    }

    fn list_candidates(
        &self,
        store: &KnownStore,
        project: &ProjectIdentity,
        errors: &mut Vec<ConversationSourceError>,
        cancelled: &AtomicBool,
    ) -> Vec<PathBuf>;

    fn parse(
        &self,
        records: &[&[u8]],
        relative: &Path,
        project: &ProjectIdentity,
        cancelled: &AtomicBool,
        previous: Option<&ParsedMetadata>,
        previous_pending: Option<&PendingMetadata>,
    ) -> Result<ParseOutcome, FormatFailure>;
}

#[derive(Debug)]
pub(super) struct KnownJsonlSource<F> {
    id: SourceId,
    project: ProjectIdentity,
    store: KnownStore,
    format: F,
    snapshots: Mutex<HashMap<PathBuf, ValidatedSnapshot>>,
}

impl<F: KnownFormat> KnownJsonlSource<F> {
    pub(super) fn new(
        project: ProjectIdentity,
        store_root: PathBuf,
        format: F,
    ) -> Result<Self, ConversationSourceError> {
        let id = SourceId::new(format.source_id()).expect("static source ID is valid");
        Self::new_with_source_id(project, store_root, format, id)
    }

    pub(super) fn new_with_source_id(
        project: ProjectIdentity,
        store_root: PathBuf,
        format: F,
        id: SourceId,
    ) -> Result<Self, ConversationSourceError> {
        if !cfg!(unix) {
            return Err(ConversationSourceError::new(
                id,
                ConversationSourceErrorKind::PermissionDenied,
                "private known-store indexing is unsupported on this platform",
            ));
        }
        if !store_root.is_absolute() {
            return Err(ConversationSourceError::new(
                id,
                ConversationSourceErrorKind::InvalidData,
                "known conversation store root must be absolute",
            )
            .with_path(store_root));
        }
        Ok(Self {
            id,
            project,
            store: KnownStore::new(store_root),
            format,
            snapshots: Mutex::new(HashMap::new()),
        })
    }

    fn error(
        &self,
        kind: ConversationSourceErrorKind,
        message: impl Into<String>,
        relative: &Path,
    ) -> ConversationSourceError {
        ConversationSourceError::new(self.id.clone(), kind, message)
            .with_path(self.store.absolute(relative))
    }

    fn scan_file(
        &self,
        relative: &Path,
        previous: Option<&WatermarkEntry>,
        byte_budget: u64,
        cancelled: &AtomicBool,
    ) -> Result<ScannedFile, ConversationSourceError> {
        let (mut file, before) = self.store.open_file(relative).map_err(|error| {
            store_io_error(
                &self.id,
                self.store.absolute(relative),
                "known conversation file is unreadable",
                &error,
            )
        })?;
        let before_state = FileState::from_metadata(&before).map_err(|error| {
            store_io_error(
                &self.id,
                self.store.absolute(relative),
                "known conversation file metadata is unavailable",
                &error,
            )
        })?;
        let identity = before_state.identity();
        let resume = previous.and_then(|entry| {
            let summary = entry
                .summary
                .as_ref()
                .and_then(|summary| summary.to_parsed(&self.project));
            let pending = entry
                .pending
                .as_ref()
                .and_then(StoredPendingMetadata::to_pending);
            let session_matches = match (&entry.session_id, &summary, &pending) {
                (Some(session_id), Some(summary), None) => session_id == &summary.session_id,
                (None, None, Some(_)) => true,
                _ => false,
            };
            let structurally_resumable = entry.identity == identity
                && entry.safe_offset < before_state.len
                && session_matches;
            if !structurally_resumable
                || entry.prefix_hash.is_none()
                || read_prefix_hash(&mut file, entry.safe_offset).ok() != entry.prefix_hash
            {
                return None;
            }
            Some((
                entry.safe_offset,
                entry.content_hash,
                summary,
                pending,
                entry.prefix_hash,
            ))
        });
        let (start, mut content_hash, previous_metadata, previous_pending, prefix_hash) = resume
            .map_or(
                (0, FNV_OFFSET, None, None, None),
                |(offset, hash, metadata, pending, prefix_hash)| {
                    (offset, hash, metadata, pending, prefix_hash)
                },
            );
        let remaining = before_state.len.saturating_sub(start);
        let read_len = remaining.min(byte_budget).min(MAX_FILE_BYTES);
        if read_len == 0 {
            return Err(self.error(
                ConversationSourceErrorKind::InvalidData,
                "known conversation scan has no metadata byte budget",
                relative,
            ));
        }
        let expected_len = usize::try_from(read_len).map_err(|_| {
            self.error(
                ConversationSourceErrorKind::InvalidData,
                "known conversation scan length is unsupported",
                relative,
            )
        })?;
        file.seek(SeekFrom::Start(start)).map_err(|error| {
            store_io_error(
                &self.id,
                self.store.absolute(relative),
                "known conversation file cannot be positioned",
                &error,
            )
        })?;
        if cancelled.load(Ordering::Relaxed) {
            return Err(self.error(
                ConversationSourceErrorKind::Io,
                "known conversation scan was cancelled",
                relative,
            ));
        }
        let mut snapshot = Vec::with_capacity(expected_len);
        (&mut file)
            .take(read_len)
            .read_to_end(&mut snapshot)
            .map_err(|error| {
                store_io_error(
                    &self.id,
                    self.store.absolute(relative),
                    "known conversation file cannot be read",
                    &error,
                )
            })?;
        if snapshot.len() != expected_len {
            return Err(self.error(
                ConversationSourceErrorKind::Io,
                "known conversation file was truncated during metadata extraction",
                relative,
            ));
        }
        if cancelled.load(Ordering::Relaxed) {
            return Err(self.error(
                ConversationSourceErrorKind::Io,
                "known conversation scan was cancelled",
                relative,
            ));
        }
        let safe_len = snapshot
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index.saturating_add(1));
        let parked = safe_len == 0 && previous_metadata.is_some() && read_len < remaining;
        if parked
            && read_final_byte(&mut file, before_state.len).map_err(|error| {
                store_io_error(
                    &self.id,
                    self.store.absolute(relative),
                    "known conversation partial tail cannot be inspected",
                    &error,
                )
            })? == Some(b'\n')
        {
            return Err(self.error(
                ConversationSourceErrorKind::UnsupportedFormat,
                "known JSONL record exceeds the byte limit",
                relative,
            ));
        }
        if safe_len == 0 && previous_metadata.is_none() {
            let message = if read_len < remaining || snapshot.len() > MAX_RECORD_BYTES {
                "known JSONL record exceeds the byte limit"
            } else {
                "known JSONL has no complete newline-terminated record"
            };
            return Err(self.error(
                ConversationSourceErrorKind::UnsupportedFormat,
                message,
                relative,
            ));
        }
        let outcome = if safe_len == 0 {
            ParseOutcome::Metadata(previous_metadata.expect("incremental scan has prior metadata"))
        } else {
            let records = complete_records(&snapshot[..safe_len])
                .map_err(|failure| self.error(failure.kind, failure.message, relative))?;
            let outcome = self
                .format
                .parse(
                    &records,
                    relative,
                    &self.project,
                    cancelled,
                    previous_metadata.as_ref(),
                    previous_pending.as_ref(),
                )
                .map_err(|failure| self.error(failure.kind, failure.message, relative))?;
            content_hash = fnv1a(&snapshot[..safe_len], content_hash);
            outcome
        };
        let after = file.metadata().map_err(|error| {
            store_io_error(
                &self.id,
                self.store.absolute(relative),
                "known conversation file metadata changed unexpectedly",
                &error,
            )
        })?;
        let after_state = FileState::from_metadata(&after).map_err(|error| {
            store_io_error(
                &self.id,
                self.store.absolute(relative),
                "known conversation file metadata is unavailable",
                &error,
            )
        })?;
        if before_state != after_state {
            return Err(self.error(
                ConversationSourceErrorKind::Io,
                "known conversation file changed during bounded metadata extraction",
                relative,
            ));
        }
        let safe_offset = start.saturating_add(u64::try_from(safe_len).unwrap_or(u64::MAX));
        let complete = start.saturating_add(read_len) >= before_state.len || parked;
        if complete && matches!(outcome, ParseOutcome::IdentityPending(_)) {
            return Err(self.error(
                ConversationSourceErrorKind::UnsupportedFormat,
                "known JSONL contains no current transcript record",
                relative,
            ));
        }
        let prefix_hash = prefix_hash.or_else(|| {
            let prefix_len =
                safe_len.min(usize::try_from(PREFIX_GUARD_BYTES).unwrap_or(usize::MAX));
            (prefix_len > 0).then(|| fnv1a(&snapshot[..prefix_len], FNV_OFFSET))
        });
        let (metadata, pending) = match outcome {
            ParseOutcome::Metadata(metadata) => (Some(metadata), None),
            ParseOutcome::IdentityPending(pending) => {
                (None, Some(StoredPendingMetadata::from_pending(&pending)))
            }
        };
        let fingerprint = complete.then(|| {
            snapshot_fingerprint(
                metadata
                    .as_ref()
                    .expect("complete scan has parsed metadata"),
                &after_state,
                content_hash,
            )
        });
        let session_id = metadata
            .as_ref()
            .map(|metadata| metadata.session_id.clone());
        let summary = metadata.as_ref().map(StoredMetadata::from_parsed);
        Ok(ScannedFile {
            metadata,
            state: after_state,
            watermark: WatermarkEntry {
                state: before_state.fingerprint(),
                fingerprint,
                session_id,
                safe_offset,
                complete,
                rejection: None,
                rejected: false,
                duplicate: false,
                prefix_hash,
                content_hash,
                summary,
                pending,
                identity,
                adapter_revision: self.format.adapter_revision(),
            },
        })
    }

    fn validate_candidate(
        &self,
        candidate: &ConversationCandidate,
        _byte_budget: u64,
    ) -> Result<(ParsedMetadata, FileState), ConversationSourceError> {
        let path = candidate.source_path().ok_or_else(|| {
            ConversationSourceError::new(
                self.id.clone(),
                ConversationSourceErrorKind::InvalidData,
                "known conversation candidate has no source path",
            )
        })?;
        let relative = self.store.relative(path).map_err(|_| {
            self.error(
                ConversationSourceErrorKind::InvalidData,
                "known conversation candidate is outside its registered store",
                Path::new("."),
            )
        })?;
        let snapshot = self
            .snapshots
            .lock()
            .map_err(|_| {
                self.error(
                    ConversationSourceErrorKind::Io,
                    "known conversation snapshot state is unavailable",
                    relative,
                )
            })?
            .get(relative)
            .cloned()
            .ok_or_else(|| {
                self.error(
                    ConversationSourceErrorKind::InvalidData,
                    "known conversation candidate has no validated discovery snapshot",
                    relative,
                )
            })?;
        let state = self.store.file_state(relative).map_err(|error| {
            store_io_error(
                &self.id,
                self.store.absolute(relative),
                "known conversation file metadata is unavailable",
                &error,
            )
        })?;
        if state != snapshot.state
            || snapshot.metadata.session_id != candidate.source_reference()
            || candidate.observed_size() != Some(state.len)
            || candidate.modified_at() != Some(state.modified)
            || candidate.fingerprint() != Some(snapshot.fingerprint.as_str())
        {
            return Err(self.error(
                ConversationSourceErrorKind::InvalidData,
                "known conversation candidate no longer matches its discovered snapshot",
                relative,
            ));
        }
        Ok((snapshot.metadata, state))
    }
}

impl<F: KnownFormat> ConversationSource for KnownJsonlSource<F> {
    fn source_id(&self) -> &SourceId {
        &self.id
    }

    fn probe(&self) -> Result<StorageProbe, ConversationSourceError> {
        match self.store.probe() {
            Ok(true) => Ok(StorageProbe::Available),
            Ok(false) => Ok(StorageProbe::Unavailable {
                reason: "known conversation store does not exist".to_owned(),
            }),
            Err(error) => Err(store_io_error(
                &self.id,
                self.store.root.clone(),
                "known conversation store is unavailable",
                &error,
            )),
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
            return Err(ConversationSourceError::new(
                self.id.clone(),
                ConversationSourceErrorKind::ProjectMismatch,
                "known conversation source belongs to another project",
            ));
        }
        if cancelled.load(Ordering::Relaxed) {
            return Err(ConversationSourceError::new(
                self.id.clone(),
                ConversationSourceErrorKind::Io,
                "known conversation discovery was cancelled",
            ));
        }
        if matches!(self.probe()?, StorageProbe::Unavailable { .. }) {
            let watermark = SourceWatermark::new(self.id.clone(), "{}")?;
            return DiscoveryBatch::new(
                &self.id,
                project,
                Vec::new(),
                Some(watermark),
                Vec::new(),
                false,
                Vec::new(),
            );
        }

        let previous = decode_watermark(self, after)?;
        self.snapshots
            .lock()
            .map_err(|_| {
                ConversationSourceError::new(
                    self.id.clone(),
                    ConversationSourceErrorKind::Io,
                    "known conversation snapshot state is unavailable",
                )
            })?
            .clear();
        let mut errors = Vec::new();
        let mut paths = self
            .format
            .list_candidates(&self.store, project, &mut errors, cancelled);
        let mut inventory_incomplete = errors.iter().any(|error| {
            matches!(
                error.kind(),
                ConversationSourceErrorKind::Io
                    | ConversationSourceErrorKind::PermissionDenied
                    | ConversationSourceErrorKind::InvalidData
            )
        });
        if cancelled.load(Ordering::Relaxed) {
            return Err(ConversationSourceError::new(
                self.id.clone(),
                ConversationSourceErrorKind::Io,
                "known conversation discovery was cancelled",
            ));
        }
        paths.sort_unstable();
        paths.dedup();
        if paths.len() > MAX_CANDIDATE_PATHS {
            inventory_incomplete = true;
            paths.sort_unstable_by(|left, right| right.cmp(left));
            paths.truncate(MAX_CANDIDATE_PATHS);
            errors.push(ConversationSourceError::new(
                self.id.clone(),
                ConversationSourceErrorKind::InvalidData,
                "known conversation candidate inventory exceeds the traversal limit",
            ));
        }

        let mut files = Vec::with_capacity(paths.len());
        for relative in paths {
            if cancelled.load(Ordering::Relaxed) {
                return Err(ConversationSourceError::new(
                    self.id.clone(),
                    ConversationSourceErrorKind::Io,
                    "known conversation discovery was cancelled",
                ));
            }
            match self.store.file_state(&relative) {
                Ok(state) => files.push((relative, state)),
                Err(error) => {
                    inventory_incomplete = true;
                    errors.push(store_io_error(
                        &self.id,
                        self.store.absolute(&relative),
                        "known conversation file metadata is unavailable",
                        &error,
                    ));
                }
            }
        }
        if files.len() > MAX_STORE_FILES {
            inventory_incomplete = true;
            files.sort_unstable_by(|left, right| {
                right
                    .1
                    .modified
                    .cmp(&left.1.modified)
                    .then_with(|| right.0.cmp(&left.0))
            });
            files.truncate(MAX_STORE_FILES);
            errors.push(ConversationSourceError::new(
                self.id.clone(),
                ConversationSourceErrorKind::InvalidData,
                "known conversation store exceeds the total file limit",
            ));
        }
        files.sort_unstable_by(|left, right| {
            right
                .1
                .modified
                .cmp(&left.1.modified)
                .then_with(|| right.0.cmp(&left.0))
        });

        let mut prepared = Vec::with_capacity(files.len());
        let mut current_keys = BTreeSet::new();
        let mut inspected_changed = 0_usize;
        let mut has_more = false;
        for (relative, state) in files {
            if cancelled.load(Ordering::Relaxed) {
                return Err(ConversationSourceError::new(
                    self.id.clone(),
                    ConversationSourceErrorKind::Io,
                    "known conversation discovery was cancelled",
                ));
            }
            let key = watermark_key(&relative);
            if !current_keys.insert(key.clone()) {
                errors.push(self.error(
                    ConversationSourceErrorKind::InvalidData,
                    "known conversation paths produced a duplicate watermark key",
                    &relative,
                ));
                continue;
            }
            let state_fingerprint = state.fingerprint();
            if let Some(entry) = previous.get(&key)
                && (entry.complete || entry.rejected)
                && entry.state == state_fingerprint
            {
                let error = entry.rejection.as_ref().map(|rejection| {
                    self.error(rejection.kind(), rejection.message.clone(), &relative)
                });
                prepared.push(PreparedFile {
                    relative,
                    key,
                    state,
                    session_id: entry.session_id.clone(),
                    watermark: entry.clone(),
                    changed: false,
                    error,
                });
                continue;
            }
            if inspected_changed >= limit.get() {
                has_more = true;
                if let Some(entry) = previous.get(&key) {
                    prepared.push(PreparedFile {
                        relative,
                        key,
                        state,
                        session_id: entry.session_id.clone(),
                        watermark: entry.clone(),
                        changed: false,
                        error: None,
                    });
                }
                continue;
            }
            inspected_changed = inspected_changed.saturating_add(1);
            match self.scan_file(&relative, previous.get(&key), MAX_FILE_BYTES, cancelled) {
                Ok(scanned) => {
                    let session_id = scanned
                        .metadata
                        .as_ref()
                        .map(|metadata| metadata.session_id.clone());
                    let changed = scanned.watermark.complete
                        && previous
                            .get(&key)
                            .and_then(|entry| entry.fingerprint.as_ref())
                            != scanned.watermark.fingerprint.as_ref();
                    has_more |= !scanned.watermark.complete;
                    prepared.push(PreparedFile {
                        relative,
                        key,
                        state: scanned.state,
                        session_id,
                        watermark: scanned.watermark,
                        changed,
                        error: None,
                    });
                }
                Err(error) => {
                    if matches!(
                        error.kind(),
                        ConversationSourceErrorKind::Io
                            | ConversationSourceErrorKind::PermissionDenied
                    ) && let Some(previous_entry) = previous.get(&key)
                    {
                        prepared.push(PreparedFile {
                            relative,
                            key,
                            state: state.clone(),
                            session_id: previous_entry.session_id.clone(),
                            watermark: previous_entry.clone(),
                            changed: false,
                            error: Some(error),
                        });
                    } else {
                        let rejection = StoredRejection::from_error(&error);
                        prepared.push(PreparedFile {
                            relative,
                            key,
                            state: state.clone(),
                            session_id: None,
                            watermark: WatermarkEntry {
                                state: state_fingerprint,
                                fingerprint: None,
                                session_id: None,
                                safe_offset: 0,
                                complete: false,
                                rejected: rejection.is_some(),
                                rejection,
                                duplicate: false,
                                prefix_hash: None,
                                content_hash: FNV_OFFSET,
                                summary: None,
                                pending: None,
                                identity: state.identity(),
                                adapter_revision: self.format.adapter_revision(),
                            },
                            changed: false,
                            error: Some(error),
                        });
                    }
                }
            }
        }

        let mut occurrences = HashMap::<String, usize>::new();
        let mut duplicate = BTreeSet::new();
        for file in &prepared {
            if let Some(session_id) = &file.session_id
                && occurrences.insert(session_id.clone(), 1).is_some()
            {
                duplicate.insert(session_id.clone());
            }
        }

        let mut next = BTreeMap::new();
        let mut candidates = Vec::new();
        for file in prepared {
            let mut file = file;
            if let Some(error) = file.error {
                if error.kind() != ConversationSourceErrorKind::ProjectMismatch
                    || self.format.report_project_mismatch()
                {
                    errors.push(error);
                }
                next.insert(file.key, file.watermark);
                continue;
            }
            let Some(session_id) = file.session_id else {
                next.insert(file.key, file.watermark);
                continue;
            };
            if duplicate.contains(&session_id) {
                errors.push(self.error(
                    ConversationSourceErrorKind::InvalidData,
                    "native session identifier appears in multiple known-store files",
                    &file.relative,
                ));
                file.watermark.duplicate = true;
                next.insert(file.key, file.watermark);
                continue;
            }
            if file.watermark.duplicate {
                file.changed = true;
                file.watermark.duplicate = false;
            }
            if !file.changed {
                if file.watermark.complete {
                    let fingerprint = file
                        .watermark
                        .fingerprint
                        .clone()
                        .expect("unchanged complete metadata has a fingerprint");
                    let metadata = file
                        .watermark
                        .summary
                        .as_ref()
                        .and_then(|summary| summary.to_parsed(project))
                        .expect("validated watermark has metadata");
                    self.snapshots
                        .lock()
                        .map_err(|_| {
                            self.error(
                                ConversationSourceErrorKind::Io,
                                "known conversation snapshot state is unavailable",
                                &file.relative,
                            )
                        })?
                        .insert(
                            file.relative.clone(),
                            ValidatedSnapshot {
                                metadata,
                                state: file.state.clone(),
                                fingerprint,
                            },
                        );
                }
                next.insert(file.key, file.watermark);
                continue;
            }
            debug_assert!(candidates.len() < limit.get());
            let fingerprint = file
                .watermark
                .fingerprint
                .clone()
                .expect("complete parsed metadata has a fingerprint");
            let metadata = file
                .watermark
                .summary
                .as_ref()
                .and_then(|summary| summary.to_parsed(project))
                .ok_or_else(|| {
                    self.error(
                        ConversationSourceErrorKind::InvalidData,
                        "known conversation watermark metadata is invalid",
                        &file.relative,
                    )
                })?;
            match ConversationCandidate::new(
                self.id.clone(),
                project.clone(),
                session_id,
                Some(self.store.absolute(&file.relative)),
                Some(file.state.len),
                Some(file.state.modified),
                Some(fingerprint.clone()),
            ) {
                Ok(candidate) => {
                    self.snapshots
                        .lock()
                        .map_err(|_| {
                            self.error(
                                ConversationSourceErrorKind::Io,
                                "known conversation snapshot state is unavailable",
                                &file.relative,
                            )
                        })?
                        .insert(
                            file.relative.clone(),
                            ValidatedSnapshot {
                                metadata,
                                state: file.state.clone(),
                                fingerprint,
                            },
                        );
                    candidates.push(candidate);
                    next.insert(file.key, file.watermark);
                }
                Err(error) => errors.push(error),
            }
        }
        next.retain(|key, _| current_keys.contains(key));
        if inventory_incomplete {
            for (key, entry) in &previous {
                if next.len() >= MAX_STORE_FILES {
                    break;
                }
                next.entry(key.clone()).or_insert_with(|| entry.clone());
            }
        }
        let mut removed_session_ids = BTreeSet::new();
        for (key, previous_entry) in &previous {
            if inventory_incomplete && !current_keys.contains(key) {
                continue;
            }
            let Some(previous_session_id) = &previous_entry.session_id else {
                continue;
            };
            let current_session_id = next.get(key).and_then(|entry| entry.session_id.as_ref());
            if current_session_id != Some(previous_session_id)
                || duplicate.contains(previous_session_id)
            {
                removed_session_ids.insert(previous_session_id.clone());
            }
        }
        let removals = removed_session_ids
            .into_iter()
            .map(|session_id| {
                SessionReference::new(self.format.tool_id(), session_id)
                    .map(|reference| ConversationRemoval::new(self.id.clone(), reference))
                    .map_err(|_| {
                        ConversationSourceError::new(
                            self.id.clone(),
                            ConversationSourceErrorKind::InvalidData,
                            "known source watermark contains an invalid session identifier",
                        )
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let token = serde_json::to_string(&next).map_err(|_| {
            ConversationSourceError::new(
                self.id.clone(),
                ConversationSourceErrorKind::InvalidData,
                "known source watermark cannot be encoded",
            )
        })?;
        if token.len() > MAX_WATERMARK_BYTES {
            return Err(ConversationSourceError::new(
                self.id.clone(),
                ConversationSourceErrorKind::InvalidData,
                "known source watermark exceeds the byte limit",
            ));
        }
        let watermark = SourceWatermark::new(self.id.clone(), token)?;
        DiscoveryBatch::new(
            &self.id,
            project,
            candidates,
            Some(watermark),
            removals,
            has_more,
            errors,
        )
    }

    fn extract_metadata_raw(
        &self,
        candidate: &ConversationCandidate,
        budget: MetadataBudget,
    ) -> Result<Conversation, ConversationSourceError> {
        let (metadata, _) = self.validate_candidate(
            candidate,
            u64::try_from(budget.max_bytes()).unwrap_or(u64::MAX),
        )?;
        let path = candidate.source_path().expect("validated candidate path");
        let tool = ToolIdentity::new(self.format.tool_id()).map_err(|_| {
            self.error(
                ConversationSourceErrorKind::InvalidData,
                "known tool identity is invalid",
                Path::new("."),
            )
        })?;
        let session =
            SessionReference::new(self.format.tool_id(), &metadata.session_id).map_err(|_| {
                self.error(
                    ConversationSourceErrorKind::InvalidData,
                    "known session reference is invalid",
                    Path::new("."),
                )
            })?;
        let resume = ResumeReference::new(&metadata.session_id).map_err(|_| {
            self.error(
                ConversationSourceErrorKind::InvalidData,
                "known resume reference is invalid",
                Path::new("."),
            )
        })?;
        Conversation::new(
            tool,
            session,
            candidate.project_identity().clone(),
            metadata.title.clone(),
            Some(metadata.created_at),
            None,
            metadata.updated_at,
            ConversationState::Unknown,
            vec![ConversationProvenance::new(
                self.id.clone(),
                ProvenanceKind::ExternalLocal,
                Some(path.to_path_buf()),
            )],
            ResumeCapability::Supported(resume),
        )
        .map_err(|_| {
            self.error(
                ConversationSourceErrorKind::InvalidData,
                "known conversation metadata is invalid",
                Path::new("."),
            )
        })
    }
    fn project_evidence_raw(
        &self,
        candidate: &ConversationCandidate,
        _project: &ProjectIdentity,
    ) -> Result<Vec<ProjectAssociationEvidence>, ConversationSourceError> {
        let (metadata, _) = self.validate_candidate(candidate, MAX_FILE_BYTES)?;
        Ok(vec![
            ProjectAssociationEvidence::new(
                ProjectEvidenceKind::CanonicalWorkingDirectory,
                metadata.cwd.clone(),
                Some("native session cwd equals the canonical project root".to_owned()),
            ),
            ProjectAssociationEvidence::new(
                ProjectEvidenceKind::AdapterValidatedEncoding,
                metadata.cwd,
                Some("known store path hints agree with native session metadata".to_owned()),
            ),
        ])
    }
}

#[derive(Clone, Debug)]
struct PreparedFile {
    relative: PathBuf,
    key: String,
    state: FileState,
    session_id: Option<String>,
    watermark: WatermarkEntry,
    changed: bool,
    error: Option<ConversationSourceError>,
}
#[derive(Clone, Debug)]
struct ScannedFile {
    metadata: Option<ParsedMetadata>,
    state: FileState,
    watermark: WatermarkEntry,
}

#[derive(Clone, Debug)]
struct ValidatedSnapshot {
    metadata: ParsedMetadata,
    state: FileState,
    fingerprint: String,
}

#[derive(Clone, Debug)]
pub(super) struct ParsedMetadata {
    pub(super) session_id: String,
    pub(super) title: Option<String>,
    pub(super) created_at: SystemTime,
    pub(super) updated_at: SystemTime,
    pub(super) chain_updated_at: SystemTime,
    pub(super) cwd: CanonicalPath,
    pub(super) chain_tail: Option<String>,
    pub(super) record_count: u64,
}

#[derive(Clone, Debug)]
pub(super) struct PendingMetadata {
    pub(super) created_at: Option<SystemTime>,
    pub(super) updated_at: Option<SystemTime>,
    pub(super) record_count: u64,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct FormatFailure {
    pub(super) kind: ConversationSourceErrorKind,
    pub(super) message: &'static str,
}

impl FormatFailure {
    pub(super) const fn unsupported(message: &'static str) -> Self {
        Self {
            kind: ConversationSourceErrorKind::UnsupportedFormat,
            message,
        }
    }

    pub(super) const fn project_mismatch(message: &'static str) -> Self {
        Self {
            kind: ConversationSourceErrorKind::ProjectMismatch,
            message,
        }
    }
    pub(super) const fn cancelled() -> Self {
        Self {
            kind: ConversationSourceErrorKind::Io,
            message: "known conversation scan was cancelled",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct WatermarkEntry {
    state: String,
    fingerprint: Option<String>,
    session_id: Option<String>,
    safe_offset: u64,
    complete: bool,
    #[serde(default)]
    rejected: bool,
    #[serde(default)]
    rejection: Option<StoredRejection>,
    #[serde(default)]
    duplicate: bool,
    #[serde(default)]
    prefix_hash: Option<u64>,
    content_hash: u64,
    summary: Option<StoredMetadata>,
    #[serde(default)]
    pending: Option<StoredPendingMetadata>,
    identity: String,
    #[serde(default)]
    adapter_revision: u32,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum StoredRejectionKind {
    UnsupportedFormat,
    ProjectMismatch,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct StoredRejection {
    kind: StoredRejectionKind,
    message: String,
}

impl StoredRejection {
    fn from_error(error: &ConversationSourceError) -> Option<Self> {
        let kind = match error.kind() {
            ConversationSourceErrorKind::UnsupportedFormat => {
                StoredRejectionKind::UnsupportedFormat
            }
            ConversationSourceErrorKind::ProjectMismatch => StoredRejectionKind::ProjectMismatch,
            _ => return None,
        };
        Some(Self {
            kind,
            message: error.message().to_owned(),
        })
    }

    const fn kind(&self) -> ConversationSourceErrorKind {
        match self.kind {
            StoredRejectionKind::UnsupportedFormat => {
                ConversationSourceErrorKind::UnsupportedFormat
            }
            StoredRejectionKind::ProjectMismatch => ConversationSourceErrorKind::ProjectMismatch,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct StoredPendingMetadata {
    created_at: Option<(bool, u64, u32)>,
    updated_at: Option<(bool, u64, u32)>,
    record_count: u64,
}

impl StoredPendingMetadata {
    fn from_pending(metadata: &PendingMetadata) -> Self {
        Self {
            created_at: metadata.created_at.map(system_time_parts),
            updated_at: metadata.updated_at.map(system_time_parts),
            record_count: metadata.record_count,
        }
    }

    fn to_pending(&self) -> Option<PendingMetadata> {
        if self.record_count == 0
            || self
                .created_at
                .is_some_and(|(_, _, nanos)| nanos >= 1_000_000_000)
            || self
                .updated_at
                .is_some_and(|(_, _, nanos)| nanos >= 1_000_000_000)
        {
            return None;
        }
        let created_at = self.created_at.and_then(system_time_from_parts);
        let updated_at = self.updated_at.and_then(system_time_from_parts);
        if created_at.is_some() != updated_at.is_some()
            || created_at
                .zip(updated_at)
                .is_some_and(|(created_at, updated_at)| updated_at < created_at)
        {
            return None;
        }
        Some(PendingMetadata {
            created_at,
            updated_at,
            record_count: self.record_count,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct StoredMetadata {
    session_id: String,
    #[serde(default)]
    title: Option<String>,
    created_at: (bool, u64, u32),
    updated_at: (bool, u64, u32),
    #[serde(default)]
    chain_updated_at: Option<(bool, u64, u32)>,
    chain_tail: Option<String>,
    record_count: u64,
    #[serde(default)]
    cwd: Option<PathBuf>,
}

impl StoredMetadata {
    fn from_parsed(metadata: &ParsedMetadata) -> Self {
        Self {
            session_id: metadata.session_id.clone(),
            title: metadata.title.clone(),
            created_at: system_time_parts(metadata.created_at),
            updated_at: system_time_parts(metadata.updated_at),
            chain_updated_at: Some(system_time_parts(metadata.chain_updated_at)),
            chain_tail: metadata.chain_tail.clone(),
            cwd: Some(metadata.cwd.as_path().to_path_buf()),
            record_count: metadata.record_count,
        }
    }

    fn to_parsed(&self, project: &ProjectIdentity) -> Option<ParsedMetadata> {
        let chain_updated_at = self.chain_updated_at.unwrap_or(self.updated_at);
        if self.created_at.2 >= 1_000_000_000
            || self.updated_at.2 >= 1_000_000_000
            || chain_updated_at.2 >= 1_000_000_000
            || self
                .title
                .as_ref()
                .is_some_and(|title| title.len() > MAX_METADATA_TITLE_BYTES)
        {
            return None;
        }
        let cwd = self
            .cwd
            .as_ref()
            .and_then(|cwd| CanonicalPath::new(cwd.clone()).ok())
            .filter(|cwd| crate::project::path_is_within(project.root(), cwd.as_path()))
            .or_else(|| CanonicalPath::new(project.root().to_path_buf()).ok())?;
        Some(ParsedMetadata {
            session_id: self.session_id.clone(),
            title: self.title.clone(),
            created_at: system_time_from_parts(self.created_at)?,
            updated_at: system_time_from_parts(self.updated_at)?,
            chain_updated_at: system_time_from_parts(chain_updated_at)?,
            cwd,
            chain_tail: self.chain_tail.clone(),
            record_count: self.record_count,
        })
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
struct FileState {
    len: u64,
    modified: SystemTime,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    changed_seconds: i64,
    #[cfg(unix)]
    changed_nanoseconds: i64,
}

impl FileState {
    fn from_metadata(metadata: &Metadata) -> io::Result<Self> {
        Ok(Self {
            len: metadata.len(),
            modified: metadata.modified()?.into_std(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
            #[cfg(unix)]
            changed_seconds: metadata.ctime(),
            #[cfg(unix)]
            changed_nanoseconds: metadata.ctime_nsec(),
        })
    }
    fn identity(&self) -> String {
        #[cfg(unix)]
        {
            format!("{}:{}", self.device, self.inode)
        }
        #[cfg(not(unix))]
        {
            self.fingerprint()
        }
    }

    fn fingerprint(&self) -> String {
        let (sign, duration) = match self.modified.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(duration) => ('+', duration),
            Err(error) => ('-', error.duration()),
        };
        #[cfg(unix)]
        {
            format!(
                "{}:{sign}:{}:{}:{}:{}:{}:{}",
                self.len,
                duration.as_secs(),
                duration.subsec_nanos(),
                self.device,
                self.inode,
                self.changed_seconds,
                self.changed_nanoseconds,
            )
        }
        #[cfg(not(unix))]
        {
            format!(
                "{}:{sign}:{}:{}",
                self.len,
                duration.as_secs(),
                duration.subsec_nanos()
            )
        }
    }
}

#[derive(Debug)]
pub(super) struct KnownStore {
    root: PathBuf,
}

impl KnownStore {
    pub(super) const fn new(root: PathBuf) -> Self {
        Self { root }
    }
    pub(super) fn probe(&self) -> io::Result<bool> {
        match std::fs::symlink_metadata(&self.root) {
            Ok(metadata) if metadata.file_type().is_dir() => Ok(true),
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "known store root is not a directory",
            )),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
    pub(super) fn open_root_directory(&self) -> io::Result<Dir> {
        self.open_directory(Path::new(""))
    }

    pub(super) fn open_file_in(directory: &Dir, name: &OsStr) -> io::Result<(File, Metadata)> {
        let file = open_file_nofollow(directory, name)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "known conversation candidate is not a regular file",
            ));
        }
        Ok((file, metadata))
    }

    pub(super) fn list_directory(&self, relative: &Path) -> io::Result<Vec<(OsString, EntryKind)>> {
        let directory = self.open_directory(relative)?;
        let mut entries = Vec::new();
        for result in directory.entries()?.take(MAX_DIRECTORY_ENTRIES + 1) {
            let entry = result?;
            let name = entry.file_name();
            let metadata = directory.symlink_metadata(&name)?;
            let kind = if metadata.is_symlink() {
                EntryKind::Symlink
            } else if metadata.is_dir() {
                EntryKind::Directory
            } else if metadata.is_file() {
                EntryKind::File
            } else {
                EntryKind::Other
            };
            entries.push((name, kind));
        }
        if entries.len() > MAX_DIRECTORY_ENTRIES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "known store directory exceeds the entry limit",
            ));
        }
        entries.sort_unstable_by(|left, right| right.0.cmp(&left.0));
        Ok(entries)
    }

    fn file_state(&self, relative: &Path) -> io::Result<FileState> {
        let (_, metadata) = self.open_file(relative)?;
        FileState::from_metadata(&metadata)
    }
    pub(super) fn open_file(&self, relative: &Path) -> io::Result<(File, Metadata)> {
        let parent = relative.parent().unwrap_or_else(|| Path::new(""));
        let name = relative
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing file name"))?;
        let directory = self.open_directory(parent)?;
        Self::open_file_in(&directory, name)
    }

    fn open_directory(&self, relative: &Path) -> io::Result<Dir> {
        if !relative.as_os_str().is_empty() && !is_normal_relative_path(relative) {
            return Err(invalid_relative_path());
        }
        let mut directory = open_ambient_directory_nofollow(&self.root)?;
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(invalid_relative_path());
            };
            directory = open_child_directory_nofollow(&directory, name)?;
        }
        Ok(directory)
    }

    fn relative<'a>(&self, absolute: &'a Path) -> io::Result<&'a Path> {
        let relative = absolute
            .strip_prefix(&self.root)
            .map_err(|_| invalid_relative_path())?;
        if !is_normal_relative_path(relative) {
            return Err(invalid_relative_path());
        }
        Ok(relative)
    }

    pub(super) fn absolute(&self, relative: &Path) -> PathBuf {
        self.root.join(relative)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum EntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

pub(super) fn push_listing_error(
    errors: &mut Vec<ConversationSourceError>,
    source_id: &'static str,
    path: PathBuf,
    message: &'static str,
    error: &io::Error,
) {
    let id = SourceId::new(source_id).expect("static source ID is valid");
    errors.push(store_io_error(&id, path, message, error));
}

pub(super) fn push_shape_error(
    errors: &mut Vec<ConversationSourceError>,
    source_id: &'static str,
    path: PathBuf,
    message: &'static str,
) {
    let id = SourceId::new(source_id).expect("static source ID is valid");
    errors.push(
        ConversationSourceError::new(id, ConversationSourceErrorKind::UnsupportedFormat, message)
            .with_path(path),
    );
}

pub(super) fn push_inventory_error(
    errors: &mut Vec<ConversationSourceError>,
    source_id: &'static str,
    path: PathBuf,
    message: &'static str,
) {
    let id = SourceId::new(source_id).expect("static source ID is valid");
    errors.push(
        ConversationSourceError::new(id, ConversationSourceErrorKind::InvalidData, message)
            .with_path(path),
    );
}
pub(super) fn parse_rfc3339(value: &str) -> Result<SystemTime, FormatFailure> {
    let timestamp = OffsetDateTime::parse(value, &Rfc3339)
        .map_err(|_| FormatFailure::unsupported("native timestamp is not RFC 3339"))?;
    let nanos = timestamp.unix_timestamp_nanos();
    if nanos >= 0 {
        let nanos = u128::try_from(nanos)
            .map_err(|_| FormatFailure::unsupported("native timestamp is out of range"))?;
        let seconds = u64::try_from(nanos / 1_000_000_000)
            .map_err(|_| FormatFailure::unsupported("native timestamp is out of range"))?;
        let subsecond = u32::try_from(nanos % 1_000_000_000)
            .map_err(|_| FormatFailure::unsupported("native timestamp is out of range"))?;
        SystemTime::UNIX_EPOCH
            .checked_add(Duration::new(seconds, subsecond))
            .ok_or_else(|| FormatFailure::unsupported("native timestamp is out of range"))
    } else {
        let nanos = nanos.unsigned_abs();
        let seconds = u64::try_from(nanos / 1_000_000_000)
            .map_err(|_| FormatFailure::unsupported("native timestamp is out of range"))?;
        let subsecond = u32::try_from(nanos % 1_000_000_000)
            .map_err(|_| FormatFailure::unsupported("native timestamp is out of range"))?;
        SystemTime::UNIX_EPOCH
            .checked_sub(Duration::new(seconds, subsecond))
            .ok_or_else(|| FormatFailure::unsupported("native timestamp is out of range"))
    }
}

pub(super) fn validate_tool_version(value: Option<&str>) -> Result<(), FormatFailure> {
    let Some(value) = value else {
        return Err(FormatFailure::unsupported(
            "tool version is missing from the validated record shape",
        ));
    };
    let mut components = value.split('.');
    if (0..3).any(|_| {
        components
            .next()
            .is_none_or(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
    }) || components.next().is_some()
    {
        return Err(FormatFailure::unsupported(
            "tool version is not a dotted numeric release",
        ));
    }
    Ok(())
}

pub(super) fn canonical_cwd(
    value: &str,
    project: &ProjectIdentity,
) -> Result<CanonicalPath, FormatFailure> {
    let cwd = CanonicalPath::new(PathBuf::from(value)).map_err(|_| {
        FormatFailure::project_mismatch("native cwd is missing or cannot be canonicalized")
    })?;
    if !crate::project::path_is_within(project.root(), cwd.as_path()) {
        return Err(FormatFailure::project_mismatch(
            "native cwd is outside the canonical project root",
        ));
    }
    Ok(cwd)
}

pub(super) fn validate_uuid(value: &str, version: u8) -> Result<(), FormatFailure> {
    let bytes = value.as_bytes();
    if bytes.len() != 36
        || bytes[8] != b'-'
        || bytes[13] != b'-'
        || bytes[18] != b'-'
        || bytes[23] != b'-'
        || bytes[14] != b'0' + version
        || !matches!(bytes[19], b'8'..=b'b')
        || bytes.iter().enumerate().any(|(index, byte)| {
            !matches!(index, 8 | 13 | 18 | 23)
                && !(byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        })
    {
        return Err(FormatFailure::unsupported(
            "native session identifier is not the verified UUID variant",
        ));
    }
    Ok(())
}

pub(super) fn claude_project_directory(path: &Path) -> OsString {
    const PREFIX_LEN: usize = 200;

    let cwd = path.to_string_lossy();
    let mut encoded = Vec::with_capacity(cwd.len().min(PREFIX_LEN + 16));
    for unit in cwd.encode_utf16() {
        encoded.push(
            u8::try_from(unit)
                .ok()
                .filter(u8::is_ascii_alphanumeric)
                .unwrap_or(b'-'),
        );
    }
    if encoded.len() > PREFIX_LEN {
        let hash = cwd.encode_utf16().fold(0_i32, |hash, unit| {
            hash.wrapping_mul(31).wrapping_add(i32::from(unit))
        });
        encoded.truncate(PREFIX_LEN);
        encoded.push(b'-');
        push_base36(&mut encoded, i64::from(hash).unsigned_abs());
    }
    OsString::from(String::from_utf8(encoded).expect("Claude project key is ASCII"))
}

fn push_base36(output: &mut Vec<u8>, mut value: u64) {
    if value == 0 {
        output.push(b'0');
        return;
    }
    let mut reversed = [0_u8; 13];
    let mut len = 0;
    while value != 0 {
        let digit = u8::try_from(value % 36).expect("base-36 digit");
        reversed[len] = if digit < 10 {
            b'0' + digit
        } else {
            b'a' + digit - 10
        };
        len += 1;
        value /= 36;
    }
    output.extend(reversed[..len].iter().rev());
}

pub(super) fn pi_project_directory(path: &Path) -> OsString {
    encode_pi_project_path(path)
}

#[cfg(unix)]
fn encode_pi_project_path(path: &Path) -> OsString {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    let mut encoded = Vec::from(b"--");
    let bytes = path.as_os_str().as_bytes();
    let bytes = bytes.strip_prefix(b"/").unwrap_or(bytes);
    encoded.extend(
        bytes
            .iter()
            .map(|byte| if *byte == b'/' { b'-' } else { *byte }),
    );
    encoded.extend_from_slice(b"--");
    OsString::from_vec(encoded)
}

#[cfg(not(unix))]
fn encode_pi_project_path(path: &Path) -> OsString {
    let encoded = path.to_string_lossy().replace(['/', '\\'], "-");
    OsString::from(format!("--{}--", encoded.trim_start_matches('-')))
}

fn complete_records(snapshot: &[u8]) -> Result<Vec<&[u8]>, FormatFailure> {
    let safe = if snapshot.last() == Some(&b'\n') {
        snapshot
    } else if let Some(last_newline) = snapshot.iter().rposition(|byte| *byte == b'\n') {
        &snapshot[..=last_newline]
    } else {
        return Err(FormatFailure::unsupported(
            "known JSONL has no complete newline-terminated record",
        ));
    };
    let safe = safe.strip_suffix(b"\n").unwrap_or(safe);
    let mut records = Vec::new();
    for line in safe.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            return Err(FormatFailure::unsupported(
                "known JSONL contains an empty record",
            ));
        }
        if line.len() > MAX_RECORD_BYTES {
            return Err(FormatFailure::unsupported(
                "known JSONL record exceeds the byte limit",
            ));
        }
        records.push(line);
        if records.len() > MAX_RECORDS {
            return Err(FormatFailure::unsupported(
                "known JSONL exceeds the record limit",
            ));
        }
    }
    if records.is_empty() {
        return Err(FormatFailure::unsupported(
            "known JSONL has no complete records",
        ));
    }
    Ok(records)
}

fn snapshot_fingerprint(metadata: &ParsedMetadata, state: &FileState, content_hash: u64) -> String {
    let first = metadata_hash(metadata, FNV_OFFSET);
    let second = metadata_hash(metadata, FNV_OFFSET ^ 0x9e37_79b9_7f4a_7c15);
    format!(
        "{}:{content_hash:016x}:{first:016x}{second:016x}",
        state.fingerprint()
    )
}

fn metadata_hash(metadata: &ParsedMetadata, seed: u64) -> u64 {
    let hash = fnv1a(metadata.session_id.as_bytes(), seed);
    let hash = fnv1a(&[u8::from(metadata.title.is_some())], hash);
    let hash = metadata
        .title
        .as_ref()
        .map_or(hash, |title| fnv1a(title.as_bytes(), hash));
    let hash = hash_system_time(metadata.created_at, hash);
    let hash = hash_system_time(metadata.updated_at, hash);
    hash_system_time(metadata.chain_updated_at, hash)
}

fn hash_system_time(value: SystemTime, seed: u64) -> u64 {
    let (sign, duration) = match value.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(duration) => (b'+', duration),
        Err(error) => (b'-', error.duration()),
    };
    let hash = fnv1a(&[sign], seed);
    let hash = fnv1a(&duration.as_secs().to_le_bytes(), hash);
    fnv1a(&duration.subsec_nanos().to_le_bytes(), hash)
}
fn system_time_parts(value: SystemTime) -> (bool, u64, u32) {
    match value.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(duration) => (true, duration.as_secs(), duration.subsec_nanos()),
        Err(error) => {
            let duration = error.duration();
            (false, duration.as_secs(), duration.subsec_nanos())
        }
    }
}

fn system_time_from_parts(parts: (bool, u64, u32)) -> Option<SystemTime> {
    let duration = Duration::new(parts.1, parts.2);
    if parts.0 {
        SystemTime::UNIX_EPOCH.checked_add(duration)
    } else {
        SystemTime::UNIX_EPOCH.checked_sub(duration)
    }
}

fn watermark_key(path: &Path) -> String {
    const OFFSET: u64 = 14_695_981_039_346_656_037;
    let bytes = path.as_os_str().as_encoded_bytes();
    let first = fnv1a(bytes, OFFSET);
    let second = fnv1a(bytes, OFFSET ^ 0x9e37_79b9_7f4a_7c15);
    format!("{first:016x}{second:016x}")
}

pub(super) fn fnv1a(bytes: &[u8], seed: u64) -> u64 {
    bytes.iter().fold(seed, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(1_099_511_628_211)
    })
}

fn decode_watermark<F: KnownFormat>(
    source: &KnownJsonlSource<F>,
    after: Option<&SourceWatermark>,
) -> Result<BTreeMap<String, WatermarkEntry>, ConversationSourceError> {
    let Some(after) = after else {
        return Ok(BTreeMap::new());
    };
    let invalid = || {
        ConversationSourceError::new(
            source.id.clone(),
            ConversationSourceErrorKind::InvalidData,
            "known source watermark is malformed",
        )
    };
    if after.token().len() > MAX_WATERMARK_BYTES {
        return Err(ConversationSourceError::new(
            source.id.clone(),
            ConversationSourceErrorKind::InvalidData,
            "known source watermark exceeds the byte limit",
        ));
    }
    let mut entries = serde_json::from_str::<BTreeMap<String, WatermarkEntry>>(after.token())
        .map_err(|_| invalid())?;
    if entries.len() > MAX_STORE_FILES {
        return Err(invalid());
    }
    for (key, entry) in &entries {
        let file_len = entry
            .state
            .split(':')
            .next()
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(&invalid)?;
        let key_is_valid = key.len() == 32 && key.bytes().all(|byte| byte.is_ascii_hexdigit());
        let scalar_bounds_hold = !entry.state.is_empty()
            && entry.state.len() <= 256
            && !entry.identity.is_empty()
            && entry.identity.len() <= 128
            && entry
                .fingerprint
                .as_ref()
                .is_none_or(|value| value.len() <= 512)
            && entry.safe_offset <= file_len;
        let summary_is_valid = match (&entry.summary, &entry.session_id, &entry.pending) {
            (Some(summary), Some(session_id), None) => {
                summary.session_id == *session_id
                    && summary.record_count > 0
                    && summary.to_parsed(&source.project).is_some()
                    && SessionReference::new(source.format.tool_id(), session_id).is_ok()
            }
            (None, None, Some(pending)) => {
                !entry.rejected
                    && !entry.complete
                    && entry.safe_offset > 0
                    && entry.fingerprint.is_none()
                    && pending.to_pending().is_some()
            }
            (None, None, None) => {
                entry.rejected
                    && !entry.complete
                    && entry.safe_offset == 0
                    && entry.fingerprint.is_none()
            }
            _ => false,
        };
        let completion_is_valid = if entry.rejected {
            !entry.complete && entry.fingerprint.is_none() && entry.summary.is_none()
        } else {
            !entry.complete || (entry.fingerprint.is_some() && entry.summary.is_some())
        };
        let prefix_is_valid = (entry.safe_offset == 0 && entry.prefix_hash.is_none())
            || (entry.safe_offset > 0 && entry.prefix_hash.is_some());
        let duplicate_is_valid =
            !entry.duplicate || (!entry.rejected && entry.complete && entry.summary.is_some());
        let rejection_is_valid = match (&entry.rejection, entry.rejected) {
            (Some(rejection), true) => {
                !rejection.message.is_empty()
                    && rejection.message.len() <= 256
                    && !rejection.message.chars().any(char::is_control)
            }
            (None, false) => true,
            _ => false,
        };
        if !key_is_valid
            || !scalar_bounds_hold
            || !summary_is_valid
            || !completion_is_valid
            || !prefix_is_valid
            || !duplicate_is_valid
            || !rejection_is_valid
        {
            return Err(invalid());
        }
    }
    entries.retain(|_, entry| entry.adapter_revision == source.format.adapter_revision());
    Ok(entries)
}

fn read_prefix_hash(file: &mut File, safe_offset: u64) -> io::Result<u64> {
    let length = safe_offset.min(PREFIX_GUARD_BYTES);
    let expected = usize::try_from(length)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "prefix length is invalid"))?;
    file.seek(SeekFrom::Start(0))?;
    let mut prefix = vec![0_u8; expected];
    file.read_exact(&mut prefix)?;
    Ok(fnv1a(&prefix, FNV_OFFSET))
}

fn read_final_byte(file: &mut File, length: u64) -> io::Result<Option<u8>> {
    let Some(offset) = length.checked_sub(1) else {
        return Ok(None);
    };
    file.seek(SeekFrom::Start(offset))?;
    let mut byte = [0_u8; 1];
    file.read_exact(&mut byte)?;
    Ok(Some(byte[0]))
}

fn store_io_error(
    source_id: &SourceId,
    path: PathBuf,
    message: &'static str,
    error: &io::Error,
) -> ConversationSourceError {
    let kind = if error.kind() == io::ErrorKind::PermissionDenied {
        ConversationSourceErrorKind::PermissionDenied
    } else if matches!(
        error.kind(),
        io::ErrorKind::InvalidData | io::ErrorKind::InvalidInput
    ) {
        ConversationSourceErrorKind::InvalidData
    } else {
        ConversationSourceErrorKind::Io
    };
    ConversationSourceError::new(source_id.clone(), kind, message).with_path(path)
}

fn is_normal_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn invalid_relative_path() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "path must contain only normalized relative components",
    )
}

#[cfg(unix)]
fn open_ambient_directory_nofollow(path: &Path) -> io::Result<Dir> {
    use std::fs;
    use std::os::unix::fs::OpenOptionsExt;

    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open("/")?;
    let mut directory = Dir::from_std_file(file);
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                directory = open_child_directory_nofollow(&directory, name)?;
            }
            _ => return Err(invalid_relative_path()),
        }
    }
    Ok(directory)
}

#[cfg(not(unix))]
fn open_ambient_directory_nofollow(path: &Path) -> io::Result<Dir> {
    let mut ambient_root = PathBuf::new();
    let mut has_root = false;
    for component in path.components() {
        match component {
            Component::Prefix(prefix) if !has_root => ambient_root.push(prefix.as_os_str()),
            Component::RootDir if !has_root => {
                ambient_root.push(component.as_os_str());
                has_root = true;
            }
            Component::Normal(_) => break,
            _ => return Err(invalid_relative_path()),
        }
    }
    if !has_root {
        return Err(invalid_relative_path());
    }
    let mut directory = Dir::open_ambient_dir(ambient_root, cap_std::ambient_authority())?;
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => {}
            Component::Normal(name) => {
                directory = open_child_directory_nofollow(&directory, name)?;
            }
            _ => return Err(invalid_relative_path()),
        }
    }
    Ok(directory)
}

fn open_child_directory_nofollow(parent: &Dir, name: &OsStr) -> io::Result<Dir> {
    use cap_std::fs::{OpenOptions, OpenOptionsExt};

    let mut options = OpenOptions::new();
    options.read(true);
    OpenOptionsFollowExt::follow(&mut options, FollowSymlinks::No);
    OpenOptionsMaybeDirExt::maybe_dir(&mut options, true);
    #[cfg(unix)]
    options.custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW);
    let file = parent.open_with(name, &options)?;
    if !file.metadata()?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path component is not a directory",
        ));
    }
    Ok(Dir::from_std_file(file.into_std()))
}

fn open_file_nofollow(parent: &Dir, name: &OsStr) -> io::Result<File> {
    use cap_std::fs::{OpenOptions, OpenOptionsExt};

    let mut options = OpenOptions::new();
    options.read(true);
    OpenOptionsFollowExt::follow(&mut options, FollowSymlinks::No);
    #[cfg(unix)]
    options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK);
    parent.open_with(name, &options)
}
