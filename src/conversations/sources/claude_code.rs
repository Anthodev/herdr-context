use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Deserialize;
use serde::de::IgnoredAny;

use super::known_stores::{
    EntryKind, FormatFailure, KnownFormat, KnownJsonlSource, KnownStore, ParsedMetadata,
    canonical_cwd, claude_project_directory, parse_rfc3339, push_listing_error, push_shape_error,
    validate_uuid,
};
use super::{
    ConversationCandidate, ConversationSource, ConversationSourceError, DiscoveryBatch,
    DiscoveryLimit, MetadataBudget, ProjectAssociationEvidence, SourceId, SourceWatermark,
    StorageProbe,
};
use crate::conversations::Conversation;
use crate::project::ProjectIdentity;

const SOURCE_ID: &str = "claude-code";
const VERSION: &str = "2.1.232";

#[derive(Debug)]
pub struct ClaudeCodeSource {
    inner: KnownJsonlSource<ClaudeCodeFormat>,
}

impl ClaudeCodeSource {
    pub fn new(
        project: ProjectIdentity,
        store_root: PathBuf,
    ) -> Result<Self, ConversationSourceError> {
        Ok(Self {
            inner: KnownJsonlSource::new(project, store_root, ClaudeCodeFormat)?,
        })
    }
}

impl ConversationSource for ClaudeCodeSource {
    fn source_id(&self) -> &SourceId {
        self.inner.source_id()
    }

    fn probe(&self) -> Result<StorageProbe, ConversationSourceError> {
        self.inner.probe()
    }

    fn discover_raw(
        &self,
        project: &ProjectIdentity,
        after: Option<&SourceWatermark>,
        limit: DiscoveryLimit,
    ) -> Result<DiscoveryBatch, ConversationSourceError> {
        self.inner.discover_raw(project, after, limit)
    }
    fn discover_raw_cancellable(
        &self,
        project: &ProjectIdentity,
        after: Option<&SourceWatermark>,
        limit: DiscoveryLimit,
        cancelled: &AtomicBool,
    ) -> Result<DiscoveryBatch, ConversationSourceError> {
        self.inner
            .discover_raw_cancellable(project, after, limit, cancelled)
    }

    fn extract_metadata_raw(
        &self,
        candidate: &ConversationCandidate,
        budget: MetadataBudget,
    ) -> Result<Conversation, ConversationSourceError> {
        self.inner.extract_metadata_raw(candidate, budget)
    }

    fn project_evidence_raw(
        &self,
        candidate: &ConversationCandidate,
        project: &ProjectIdentity,
    ) -> Result<Vec<ProjectAssociationEvidence>, ConversationSourceError> {
        self.inner.project_evidence_raw(candidate, project)
    }
}

#[derive(Debug)]
struct ClaudeCodeFormat;

impl KnownFormat for ClaudeCodeFormat {
    fn source_id(&self) -> &'static str {
        SOURCE_ID
    }

    fn tool_id(&self) -> &'static str {
        SOURCE_ID
    }

    fn list_candidates(
        &self,
        store: &KnownStore,
        project: &ProjectIdentity,
        errors: &mut Vec<ConversationSourceError>,
        cancelled: &AtomicBool,
    ) -> Vec<PathBuf> {
        let directory = PathBuf::from(claude_project_directory(project.root()));
        let entries = match store.list_directory(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(error) => {
                push_listing_error(
                    errors,
                    SOURCE_ID,
                    store.absolute(&directory),
                    "Claude project store cannot be listed",
                    &error,
                );
                return Vec::new();
            }
        };
        let mut files = Vec::new();
        for (name, kind) in entries {
            let relative = directory.join(&name);
            if cancelled.load(Ordering::Relaxed) {
                break;
            }
            match kind {
                EntryKind::File
                    if Path::new(&name)
                        .extension()
                        .is_some_and(|value| value == "jsonl") =>
                {
                    files.push(relative);
                }
                EntryKind::File | EntryKind::Directory | EntryKind::Symlink | EntryKind::Other => {
                    push_shape_error(
                        errors,
                        SOURCE_ID,
                        store.absolute(&relative),
                        "Claude store entry is outside the verified flat JSONL layout",
                    )
                }
            }
        }
        files
    }

    fn parse(
        &self,
        records: &[&[u8]],
        relative: &Path,
        project: &ProjectIdentity,
        cancelled: &AtomicBool,
        previous: Option<&ParsedMetadata>,
    ) -> Result<ParsedMetadata, FormatFailure> {
        let expected_directory = claude_project_directory(project.root());
        if relative.parent().and_then(Path::file_name) != Some(expected_directory.as_os_str()) {
            return Err(FormatFailure::project_mismatch(
                "Claude encoded project directory conflicts with canonical cwd",
            ));
        }
        let expected_id = relative
            .file_stem()
            .and_then(|value| value.to_str())
            .ok_or_else(|| FormatFailure::unsupported("Claude session filename is invalid"))?;
        validate_uuid(expected_id, 4)?;

        let mut session_id = previous.map(|metadata| metadata.session_id.clone());
        let mut created_at = previous.map(|metadata| metadata.created_at);
        let mut updated_at = previous.map(|metadata| metadata.updated_at);
        let mut previous_uuid = previous.and_then(|metadata| metadata.chain_tail.clone());
        let mut canonical = previous.map(|metadata| metadata.cwd.clone());
        let mut record_count = previous.map_or(0, |metadata| metadata.record_count);
        if session_id.as_deref().is_some_and(|id| id != expected_id) {
            return Err(FormatFailure::unsupported(
                "Claude incremental state conflicts with the native session identifier",
            ));
        }

        for bytes in records {
            if cancelled.load(Ordering::Relaxed) {
                return Err(FormatFailure::cancelled());
            }
            let record: ClaudeRecord = serde_json::from_slice(bytes).map_err(|_| {
                FormatFailure::unsupported("Claude record does not match the current JSON shape")
            })?;
            if record.kind.is_empty() {
                return Err(FormatFailure::unsupported(
                    "Claude record type must be non-empty",
                ));
            }

            let carries_identity = match record.kind.as_str() {
                "user" | "assistant" => true,
                "attachment" => false,
                _ => {
                    return Err(FormatFailure::unsupported(
                        "Claude record type is outside the verified current set",
                    ));
                }
            };
            if carries_identity {
                if record.message.is_none() || record.attachment.is_some() {
                    return Err(FormatFailure::unsupported(
                        "Claude transcript record has an invalid message payload shape",
                    ));
                }
                let native_id = record.session_id.as_deref().ok_or_else(|| {
                    FormatFailure::unsupported(
                        "Claude transcript record is missing its native session identifier",
                    )
                })?;
                validate_uuid(native_id, 4)?;
                if native_id != expected_id {
                    return Err(FormatFailure::unsupported(
                        "Claude filename and native session identifier conflict",
                    ));
                }
                if record.version.as_deref() != Some(VERSION) || record.is_sidechain != Some(false)
                {
                    return Err(FormatFailure::unsupported(
                        "Claude record version or sidechain shape is unsupported",
                    ));
                }
                if session_id.as_deref().is_some_and(|id| id != native_id) {
                    return Err(FormatFailure::unsupported(
                        "Claude JSONL mixes native session identifiers",
                    ));
                }

                let cwd = canonical_cwd(
                    record.cwd.as_deref().ok_or_else(|| {
                        FormatFailure::project_mismatch(
                            "Claude transcript record is missing canonical cwd",
                        )
                    })?,
                    project,
                )?;
                if canonical
                    .as_ref()
                    .is_some_and(|current: &crate::project::CanonicalPath| {
                        current.as_path() != cwd.as_path()
                    })
                {
                    return Err(FormatFailure::project_mismatch(
                        "Claude JSONL mixes canonical cwd evidence",
                    ));
                }

                let uuid = record.uuid.as_deref().ok_or_else(|| {
                    FormatFailure::unsupported(
                        "Claude transcript record is missing its native entry identifier",
                    )
                })?;
                validate_uuid(uuid, 4)?;
                match (previous_uuid.is_some(), record.parent_uuid.as_deref()) {
                    (false, None) => {}
                    (true, Some(parent_uuid)) if parent_uuid != uuid => {
                        validate_uuid(parent_uuid, 4)?;
                    }
                    _ => {
                        return Err(FormatFailure::unsupported(
                            "Claude root/parent shape does not match the current transcript tree",
                        ));
                    }
                }

                let timestamp = parse_rfc3339(record.timestamp.as_deref().ok_or_else(|| {
                    FormatFailure::unsupported("Claude transcript record is missing its timestamp")
                })?)?;
                created_at = Some(created_at.map_or(timestamp, |current| current.min(timestamp)));
                updated_at = Some(updated_at.map_or(timestamp, |current| current.max(timestamp)));
                session_id = Some(native_id.to_owned());
                previous_uuid = Some(uuid.to_owned());
                canonical = Some(cwd);
            } else {
                if record.attachment.is_none() || record.message.is_some() {
                    return Err(FormatFailure::unsupported(
                        "Claude attachment record has an invalid payload shape",
                    ));
                }
                if record.version.is_some()
                    || record.cwd.is_some()
                    || record.session_id.is_some()
                    || record.is_sidechain.is_some()
                    || record.parent_uuid.is_some()
                {
                    return Err(FormatFailure::unsupported(
                        "Claude attachment unexpectedly contains transcript identity fields",
                    ));
                }
                let uuid = record.uuid.as_deref().ok_or_else(|| {
                    FormatFailure::unsupported(
                        "Claude attachment is missing its native entry identifier",
                    )
                })?;
                validate_uuid(uuid, 4)?;
                let timestamp = parse_rfc3339(record.timestamp.as_deref().ok_or_else(|| {
                    FormatFailure::unsupported("Claude attachment is missing its timestamp")
                })?)?;
                created_at = Some(created_at.map_or(timestamp, |current| current.min(timestamp)));
                updated_at = Some(updated_at.map_or(timestamp, |current| current.max(timestamp)));
            }
            record_count = record_count.saturating_add(1);
        }

        Ok(ParsedMetadata {
            session_id: session_id.ok_or_else(|| {
                FormatFailure::unsupported("Claude JSONL contains no current transcript record")
            })?,
            created_at: created_at.ok_or_else(|| {
                FormatFailure::unsupported("Claude JSONL contains no current timestamp")
            })?,
            updated_at: updated_at.ok_or_else(|| {
                FormatFailure::unsupported("Claude JSONL contains no current timestamp")
            })?,
            cwd: canonical.ok_or_else(|| {
                FormatFailure::project_mismatch(
                    "Claude JSONL contains no canonical project evidence",
                )
            })?,
            chain_tail: previous_uuid,
            record_count,
        })
    }
}

#[derive(Deserialize)]
struct ClaudeRecord {
    #[serde(rename = "parentUuid")]
    parent_uuid: Option<String>,
    #[serde(rename = "isSidechain")]
    is_sidechain: Option<bool>,
    message: Option<IgnoredAny>,
    attachment: Option<IgnoredAny>,
    cwd: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    version: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    uuid: Option<String>,
    timestamp: Option<String>,
}
