use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

#[cfg(unix)]
use cap_std::fs::MetadataExt;
use cap_std::fs::{File, Metadata};
use serde::de::{self, Visitor};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::project_local::{ProjectLocalFile, ProjectLocalFiles};
use super::{
    ConversationCandidate, ConversationRemoval, ConversationSource, ConversationSourceError,
    ConversationSourceErrorKind, DiscoveryBatch, DiscoveryLimit, MetadataBudget,
    ProjectAssociationEvidence, ProjectEvidenceKind, ProjectLocalLocation, SourceId,
    SourceWatermark, StorageProbe,
};
use crate::conversations::{
    Conversation, ConversationProvenance, ConversationState, ProvenanceKind, ResumeCapability,
    ResumeReference, SessionReference, ToolIdentity,
};
use crate::project::{CanonicalPath, ProjectIdentity};

const SOURCE_ID: &str = "project-local-generic-jsonl";
const TOOL_ID: &str = "generic-jsonl";
const MAX_FILE_BYTES: u64 = 256 * 1024;
const MAX_RECORD_BYTES: usize = 64 * 1024;
const MAX_RECORDS: usize = 256;
const MAX_SESSION_ID_BYTES: usize = 256;
const MAX_WATERMARK_ENTRIES: usize = 256;
const MAX_WATERMARK_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug)]
pub struct GenericJsonlSource {
    id: SourceId,
    project: ProjectIdentity,
    files: ProjectLocalFiles,
}

impl GenericJsonlSource {
    pub fn new(
        project: ProjectIdentity,
        locations: impl IntoIterator<Item = ProjectLocalLocation>,
    ) -> Result<Self, ConversationSourceError> {
        let id = SourceId::new(SOURCE_ID).expect("static generic source ID is valid");
        let files = ProjectLocalFiles::new(&id, project.clone(), locations)?;
        Ok(Self { id, project, files })
    }

    pub fn for_project(project: ProjectIdentity) -> Result<Self, ConversationSourceError> {
        let locations = [
            ".herdr/conversations",
            ".herdr/conversations.jsonl",
            ".herdr/conversations.json",
        ]
        .into_iter()
        .map(|path| {
            ProjectLocalLocation::new(path).map_err(|_| {
                ConversationSourceError::new(
                    SourceId::new(SOURCE_ID).expect("static generic source ID is valid"),
                    ConversationSourceErrorKind::InvalidData,
                    "built-in project-local location is invalid",
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
        Self::new(project, locations)
    }

    fn error(
        &self,
        kind: ConversationSourceErrorKind,
        message: &'static str,
        path: &Path,
    ) -> ConversationSourceError {
        ConversationSourceError::new(self.id.clone(), kind, message).with_path(path.to_path_buf())
    }

    fn discover_file(
        &self,
        file: &ProjectLocalFile,
    ) -> Result<(ParsedSummary, FileState), ConversationSourceError> {
        let (opened, metadata) = self
            .files
            .open_registered_file(&self.id, file.absolute_path())?;
        self.read_stable_summary(opened, metadata, file.absolute_path(), MAX_FILE_BYTES)
    }

    fn read_stable_summary(
        &self,
        mut file: File,
        before: Metadata,
        path: &Path,
        byte_budget: u64,
    ) -> Result<(ParsedSummary, FileState), ConversationSourceError> {
        let before_state = FileState::from_metadata(&before).map_err(|_| {
            self.error(
                ConversationSourceErrorKind::Io,
                "conversation file timestamp is unavailable",
                path,
            )
        })?;
        if before_state.len > byte_budget.min(MAX_FILE_BYTES) {
            return Err(self.error(
                ConversationSourceErrorKind::InvalidData,
                "conversation file exceeds the bounded metadata budget",
                path,
            ));
        }
        let summary = {
            let snapshot = (&mut file).take(before_state.len);
            match path.extension().and_then(|extension| extension.to_str()) {
                Some("jsonl") => self.parse_jsonl(snapshot, path)?,
                Some("json") => self.parse_json(snapshot, path)?,
                _ => {
                    return Err(self.error(
                        ConversationSourceErrorKind::UnsupportedFormat,
                        "conversation file extension is unsupported",
                        path,
                    ));
                }
            }
        };
        let after = file.metadata().map_err(|_| {
            self.error(
                ConversationSourceErrorKind::Io,
                "conversation file metadata changed unexpectedly",
                path,
            )
        })?;
        let after_state = FileState::from_metadata(&after).map_err(|_| {
            self.error(
                ConversationSourceErrorKind::Io,
                "conversation file timestamp is unavailable",
                path,
            )
        })?;
        if before_state != after_state {
            return Err(self.error(
                ConversationSourceErrorKind::InvalidData,
                "conversation file changed during bounded metadata extraction",
                path,
            ));
        }
        Ok((summary, after_state))
    }

    fn parse_jsonl(
        &self,
        snapshot: impl Read,
        path: &Path,
    ) -> Result<ParsedSummary, ConversationSourceError> {
        let mut reader = BufReader::new(snapshot);
        let mut line = Vec::new();
        let mut summary = None;
        let mut count = 0_usize;
        loop {
            line.clear();
            let read = reader.read_until(b'\n', &mut line).map_err(|_| {
                self.error(
                    ConversationSourceErrorKind::Io,
                    "conversation JSONL cannot be read",
                    path,
                )
            })?;
            if read == 0 {
                break;
            }
            let terminated = line.last() == Some(&b'\n');
            if terminated {
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
            }
            if line.len() > MAX_RECORD_BYTES {
                return Err(self.error(
                    ConversationSourceErrorKind::InvalidData,
                    "conversation JSONL record exceeds the byte limit",
                    path,
                ));
            }
            if line.is_empty() {
                return Err(self.error(
                    ConversationSourceErrorKind::MalformedData,
                    "conversation JSONL contains an empty record",
                    path,
                ));
            }
            let record = match serde_json::from_slice::<GenericRecord>(&line) {
                Ok(record) => record,
                Err(_) if !terminated => break,
                Err(error) => {
                    return Err(self.json_error(error, path));
                }
            };
            count = count.saturating_add(1);
            if count > MAX_RECORDS {
                return Err(self.error(
                    ConversationSourceErrorKind::InvalidData,
                    "conversation JSONL exceeds the record limit",
                    path,
                ));
            }
            self.merge_record(&mut summary, record, path)?;
        }
        summary.ok_or_else(|| {
            self.error(
                ConversationSourceErrorKind::InvalidData,
                "conversation JSONL has no complete records",
                path,
            )
        })
    }

    fn parse_json(
        &self,
        snapshot: impl Read,
        path: &Path,
    ) -> Result<ParsedSummary, ConversationSourceError> {
        let record = serde_json::from_reader::<_, GenericRecord>(BufReader::new(snapshot))
            .map_err(|error| self.json_error(error, path))?;
        let mut summary = None;
        self.merge_record(&mut summary, record, path)?;
        Ok(summary.expect("one parsed record always creates a summary"))
    }

    fn json_error(&self, error: serde_json::Error, path: &Path) -> ConversationSourceError {
        let kind = if error.is_syntax() || error.is_eof() {
            ConversationSourceErrorKind::MalformedData
        } else {
            ConversationSourceErrorKind::InvalidData
        };
        self.error(
            kind,
            "conversation JSON record does not match the generic schema",
            path,
        )
    }

    fn merge_record(
        &self,
        summary: &mut Option<ParsedSummary>,
        record: GenericRecord,
        path: &Path,
    ) -> Result<(), ConversationSourceError> {
        validate_session_id(&record.session_id).map_err(|message| {
            self.error(ConversationSourceErrorKind::InvalidData, message, path)
        })?;
        if record.cwd.len() > 4_096 || Path::new(&record.cwd) != self.project.root() {
            return Err(self.error(
                ConversationSourceErrorKind::InvalidData,
                "conversation cwd is not the canonical project root",
                path,
            ));
        }
        if !matches!(
            record.role.as_str(),
            "user" | "assistant" | "system" | "tool"
        ) {
            return Err(self.error(
                ConversationSourceErrorKind::InvalidData,
                "conversation role is unsupported",
                path,
            ));
        }
        let timestamp = parse_timestamp(&record.timestamp).ok_or_else(|| {
            self.error(
                ConversationSourceErrorKind::InvalidData,
                "conversation timestamp must be RFC3339",
                path,
            )
        })?;
        match summary {
            Some(summary) => {
                if summary.session_id != record.session_id {
                    return Err(self.error(
                        ConversationSourceErrorKind::InvalidData,
                        "conversation JSONL mixes session identifiers",
                        path,
                    ));
                }
                summary.created_at = summary.created_at.min(timestamp);
                summary.updated_at = summary.updated_at.max(timestamp);
            }
            None => {
                *summary = Some(ParsedSummary {
                    session_id: record.session_id,
                    created_at: timestamp,
                    updated_at: timestamp,
                });
            }
        }
        Ok(())
    }
}

impl ConversationSource for GenericJsonlSource {
    fn source_id(&self) -> &SourceId {
        &self.id
    }

    fn probe(&self) -> Result<StorageProbe, ConversationSourceError> {
        self.files.probe(&self.id)
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
                "generic project-local source belongs to another project",
            ));
        }
        let previous = decode_watermark(self, after)?;
        let listing = self.files.list_files(&self.id, is_json_file);
        if cancelled.load(Ordering::Relaxed) {
            return Err(ConversationSourceError::new(
                self.id.clone(),
                ConversationSourceErrorKind::Io,
                "generic conversation discovery was cancelled",
            ));
        }
        let mut errors = listing.errors;
        let mut inventory_incomplete = errors.iter().any(|error| {
            matches!(
                error.kind(),
                ConversationSourceErrorKind::Io | ConversationSourceErrorKind::PermissionDenied
            )
        });
        let mut candidate_slots = Vec::new();
        let mut sessions = BTreeMap::<String, SessionOccurrence>::new();
        let mut next = BTreeMap::new();
        let mut current_keys = BTreeSet::new();
        let mut has_more = false;

        for file in listing.files {
            if cancelled.load(Ordering::Relaxed) {
                return Err(ConversationSourceError::new(
                    self.id.clone(),
                    ConversationSourceErrorKind::Io,
                    "generic conversation discovery was cancelled",
                ));
            }
            let key = watermark_key(file.relative_path());
            if !current_keys.insert(key.clone()) {
                errors.push(self.error(
                    ConversationSourceErrorKind::InvalidData,
                    "conversation paths produced a duplicate bounded watermark key",
                    file.absolute_path(),
                ));
                continue;
            }
            match self.discover_file(&file) {
                Ok((summary, state)) => {
                    let session_id = summary.session_id.clone();
                    if let Some(first) = sessions.get_mut(&session_id) {
                        if !first.duplicate {
                            first.duplicate = true;
                            next.remove(&first.key);
                            if let Some(index) = first.candidate_slot.take() {
                                candidate_slots[index] = None;
                            }
                            errors.push(self.error(
                                ConversationSourceErrorKind::InvalidData,
                                "generic session identifier appears in multiple files",
                                &first.path,
                            ));
                        }
                        errors.push(self.error(
                            ConversationSourceErrorKind::InvalidData,
                            "generic session identifier appears in multiple files",
                            file.absolute_path(),
                        ));
                        continue;
                    }
                    sessions.insert(
                        session_id.clone(),
                        SessionOccurrence {
                            key: key.clone(),
                            path: file.absolute_path().to_path_buf(),
                            candidate_slot: None,
                            duplicate: false,
                        },
                    );

                    let fingerprint = snapshot_fingerprint(&summary, &state);
                    let watermark_entry = GenericWatermarkEntry {
                        fingerprint: fingerprint.clone(),
                        session_id: session_id.clone(),
                    };
                    let changed = previous
                        .get(&key)
                        .is_none_or(|entry| entry.fingerprint != fingerprint);
                    if !changed {
                        next.insert(key, watermark_entry);
                        continue;
                    }
                    if candidate_slots.iter().flatten().count() >= limit.get() {
                        has_more = true;
                        if let Some(previous) = previous.get(&key) {
                            next.insert(key, previous.clone());
                        }
                        continue;
                    }
                    match ConversationCandidate::new(
                        self.id.clone(),
                        project.clone(),
                        summary.session_id,
                        Some(file.absolute_path().to_path_buf()),
                        Some(state.len),
                        Some(state.modified),
                        Some(fingerprint),
                    ) {
                        Ok(candidate) => {
                            if let Some(occurrence) = sessions.get_mut(&session_id) {
                                let index = candidate_slots.len();
                                candidate_slots.push(Some(candidate));
                                occurrence.candidate_slot = Some(index);
                                next.insert(key, watermark_entry);
                            } else {
                                errors.push(self.error(
                                    ConversationSourceErrorKind::InvalidData,
                                    "generic session occurrence was not retained",
                                    file.absolute_path(),
                                ));
                            }
                        }
                        Err(error) => errors.push(error),
                    }
                }
                Err(error) => {
                    inventory_incomplete |= matches!(
                        error.kind(),
                        ConversationSourceErrorKind::Io
                            | ConversationSourceErrorKind::PermissionDenied
                    );
                    if let Some(previous) = previous.get(&key) {
                        next.insert(key, previous.clone());
                    }
                    errors.push(error);
                }
            }
        }
        if inventory_incomplete {
            for (key, entry) in &previous {
                next.entry(key.clone()).or_insert_with(|| entry.clone());
            }
        }
        if !inventory_incomplete {
            next.retain(|key, _| current_keys.contains(key));
        }
        let mut removed_session_ids = BTreeSet::new();
        for (key, previous_entry) in &previous {
            if next
                .get(key)
                .is_none_or(|entry| entry.session_id != previous_entry.session_id)
            {
                removed_session_ids.insert(previous_entry.session_id.clone());
            }
        }
        let removals = removed_session_ids
            .into_iter()
            .map(|session_id| {
                SessionReference::new(TOOL_ID, session_id)
                    .map(|reference| ConversationRemoval::new(self.id.clone(), reference))
                    .map_err(|_| {
                        ConversationSourceError::new(
                            self.id.clone(),
                            ConversationSourceErrorKind::InvalidData,
                            "generic watermark contains an invalid session identifier",
                        )
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let token = serde_json::to_string(&next).map_err(|_| {
            ConversationSourceError::new(
                self.id.clone(),
                ConversationSourceErrorKind::InvalidData,
                "generic source watermark cannot be encoded",
            )
        })?;
        if token.len() > MAX_WATERMARK_BYTES {
            return Err(ConversationSourceError::new(
                self.id.clone(),
                ConversationSourceErrorKind::InvalidData,
                "generic source watermark exceeds the byte limit",
            ));
        }
        let watermark = SourceWatermark::new(self.id.clone(), token)?;
        let candidates = candidate_slots.into_iter().flatten().collect();
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
        let path = candidate.source_path().ok_or_else(|| {
            ConversationSourceError::new(
                self.id.clone(),
                ConversationSourceErrorKind::InvalidData,
                "generic conversation candidate has no source path",
            )
        })?;
        let (file, metadata) = self.files.open_registered_file(&self.id, path)?;
        let (summary, state) = self.read_stable_summary(
            file,
            metadata,
            path,
            u64::try_from(budget.max_bytes()).unwrap_or(u64::MAX),
        )?;
        let fingerprint = snapshot_fingerprint(&summary, &state);
        if summary.session_id != candidate.source_reference()
            || candidate.observed_size() != Some(state.len)
            || candidate.modified_at() != Some(state.modified)
            || candidate.fingerprint() != Some(fingerprint.as_str())
        {
            return Err(self.error(
                ConversationSourceErrorKind::InvalidData,
                "generic conversation candidate no longer matches its discovered snapshot",
                path,
            ));
        }
        let provenance = ConversationProvenance::new(
            self.id.clone(),
            ProvenanceKind::ProjectLocal,
            Some(path.to_path_buf()),
        );
        let tool = ToolIdentity::new(TOOL_ID).map_err(|_| {
            self.error(
                ConversationSourceErrorKind::InvalidData,
                "generic tool identity is invalid",
                path,
            )
        })?;
        let session = SessionReference::new(TOOL_ID, &summary.session_id).map_err(|_| {
            self.error(
                ConversationSourceErrorKind::InvalidData,
                "generic session reference is invalid",
                path,
            )
        })?;
        let resume = ResumeReference::new(&summary.session_id).map_err(|_| {
            self.error(
                ConversationSourceErrorKind::InvalidData,
                "generic resume reference is invalid",
                path,
            )
        })?;
        Conversation::new(
            tool,
            session,
            candidate.project_identity().clone(),
            None,
            Some(summary.created_at),
            summary.updated_at,
            ConversationState::Unknown,
            vec![provenance],
            ResumeCapability::Supported(resume),
        )
        .map_err(|_| {
            self.error(
                ConversationSourceErrorKind::InvalidData,
                "generic conversation metadata is invalid",
                path,
            )
        })
    }

    fn project_evidence_raw(
        &self,
        candidate: &ConversationCandidate,
        project: &ProjectIdentity,
    ) -> Result<Vec<ProjectAssociationEvidence>, ConversationSourceError> {
        let path = candidate.source_path().ok_or_else(|| {
            ConversationSourceError::new(
                self.id.clone(),
                ConversationSourceErrorKind::InvalidData,
                "generic conversation candidate has no source path",
            )
        })?;
        self.files.open_registered_file(&self.id, path)?;
        let canonical_root = CanonicalPath::new(project.root().to_path_buf()).map_err(|_| {
            self.error(
                ConversationSourceErrorKind::InvalidData,
                "canonical project evidence is unavailable",
                path,
            )
        })?;
        Ok(vec![
            ProjectAssociationEvidence::new(
                ProjectEvidenceKind::RecognizedProjectLocalPath,
                canonical_root.clone(),
                Some("registered shallow generic JSON location".to_owned()),
            ),
            ProjectAssociationEvidence::new(
                ProjectEvidenceKind::CanonicalWorkingDirectory,
                canonical_root,
                Some("generic JSON cwd equals the canonical project root".to_owned()),
            ),
        ])
    }
}

#[derive(Deserialize)]
struct GenericRecord {
    session_id: String,
    cwd: String,
    timestamp: String,
    role: String,
    #[serde(rename = "message")]
    _message: MessageMarker,
}

struct MessageMarker;

impl<'de> Deserialize<'de> for MessageMarker {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_str(MessageVisitor)
    }
}

struct MessageVisitor;

impl Visitor<'_> for MessageVisitor {
    type Value = MessageMarker;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON string message")
    }

    fn visit_str<E>(self, _value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(MessageMarker)
    }

    fn visit_string<E>(self, _value: String) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(MessageMarker)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct GenericWatermarkEntry {
    fingerprint: String,
    session_id: String,
}

struct SessionOccurrence {
    key: String,
    path: PathBuf,
    candidate_slot: Option<usize>,
    duplicate: bool,
}

#[derive(Debug)]
struct ParsedSummary {
    session_id: String,
    created_at: SystemTime,
    updated_at: SystemTime,
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
    fn from_metadata(metadata: &Metadata) -> std::io::Result<Self> {
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

fn snapshot_fingerprint(summary: &ParsedSummary, state: &FileState) -> String {
    const OFFSET: u64 = 14_695_981_039_346_656_037;
    let first = summary_hash(summary, OFFSET);
    let second = summary_hash(summary, OFFSET ^ 0x9e37_79b9_7f4a_7c15);
    format!("{}:{first:016x}{second:016x}", state.fingerprint())
}

fn summary_hash(summary: &ParsedSummary, seed: u64) -> u64 {
    let hash = fnv1a(summary.session_id.as_bytes(), seed);
    let hash = hash_system_time(summary.created_at, hash);
    hash_system_time(summary.updated_at, hash)
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

fn validate_session_id(session_id: &str) -> Result<(), &'static str> {
    if session_id.is_empty()
        || session_id.len() > MAX_SESSION_ID_BYTES
        || session_id.trim() != session_id
        || session_id.chars().any(char::is_control)
    {
        return Err("conversation session identifier is invalid");
    }
    Ok(())
}

fn parse_timestamp(value: &str) -> Option<SystemTime> {
    let timestamp = OffsetDateTime::parse(value, &Rfc3339).ok()?;
    let nanos = timestamp.unix_timestamp_nanos();
    if nanos >= 0 {
        let nanos = u128::try_from(nanos).ok()?;
        let seconds = u64::try_from(nanos / 1_000_000_000).ok()?;
        let subsecond = u32::try_from(nanos % 1_000_000_000).ok()?;
        SystemTime::UNIX_EPOCH.checked_add(Duration::new(seconds, subsecond))
    } else {
        let nanos = nanos.unsigned_abs();
        let seconds = u64::try_from(nanos / 1_000_000_000).ok()?;
        let subsecond = u32::try_from(nanos % 1_000_000_000).ok()?;
        SystemTime::UNIX_EPOCH.checked_sub(Duration::new(seconds, subsecond))
    }
}

fn is_json_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("json" | "jsonl")
    )
}

fn watermark_key(path: &Path) -> String {
    const OFFSET: u64 = 14_695_981_039_346_656_037;
    let bytes = path.as_os_str().as_encoded_bytes();
    let first = fnv1a(bytes, OFFSET);
    let second = fnv1a(bytes, OFFSET ^ 0x9e37_79b9_7f4a_7c15);
    format!("{first:016x}{second:016x}")
}

fn fnv1a(bytes: &[u8], seed: u64) -> u64 {
    bytes.iter().fold(seed, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(1_099_511_628_211)
    })
}

fn decode_watermark(
    source: &GenericJsonlSource,
    after: Option<&SourceWatermark>,
) -> Result<BTreeMap<String, GenericWatermarkEntry>, ConversationSourceError> {
    let Some(after) = after else {
        return Ok(BTreeMap::new());
    };
    let invalid = || {
        ConversationSourceError::new(
            source.id.clone(),
            ConversationSourceErrorKind::InvalidData,
            "generic source watermark is malformed",
        )
    };
    if after.token().len() > MAX_WATERMARK_BYTES {
        return Err(ConversationSourceError::new(
            source.id.clone(),
            ConversationSourceErrorKind::InvalidData,
            "generic source watermark exceeds the byte limit",
        ));
    }
    let entries = serde_json::from_str::<BTreeMap<String, GenericWatermarkEntry>>(after.token())
        .map_err(|_| invalid())?;
    if entries.len() > MAX_WATERMARK_ENTRIES
        || entries.iter().any(|(key, entry)| {
            key.len() != 32
                || !key.bytes().all(|byte| byte.is_ascii_hexdigit())
                || entry.fingerprint.is_empty()
                || entry.fingerprint.len() > 512
                || validate_session_id(&entry.session_id).is_err()
        })
    {
        return Err(invalid());
    }
    Ok(entries)
}
