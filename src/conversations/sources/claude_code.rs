use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Deserialize;
use serde::de::IgnoredAny;

use super::known_stores::{
    EntryKind, FormatFailure, KnownFormat, KnownJsonlSource, KnownStore, MAX_CANDIDATE_PATHS,
    ParseOutcome, ParsedMetadata, PendingMetadata, canonical_cwd, claude_project_directory,
    normalize_metadata_title, parse_rfc3339, push_inventory_error, push_listing_error,
    push_shape_error, validate_tool_version, validate_uuid,
};
use super::{
    ConversationCandidate, ConversationSource, ConversationSourceError, DiscoveryBatch,
    DiscoveryLimit, MetadataBudget, ProjectAssociationEvidence, SourceId, SourceWatermark,
    StorageProbe,
};
use crate::conversations::Conversation;
use crate::project::ProjectIdentity;

const SOURCE_ID: &str = "claude-code";
const MAX_PROJECT_DIRECTORIES: usize = 2_000;
const MAX_VISITED_ENTRIES: usize = MAX_CANDIDATE_PATHS + MAX_PROJECT_DIRECTORIES;

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
    pub(crate) fn new_with_source_id(
        project: ProjectIdentity,
        store_root: PathBuf,
        source_id: SourceId,
    ) -> Result<Self, ConversationSourceError> {
        Ok(Self {
            inner: KnownJsonlSource::new_with_source_id(
                project,
                store_root,
                ClaudeCodeFormat,
                source_id,
            )?,
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

    fn report_project_mismatch(&self) -> bool {
        false
    }

    fn adapter_revision(&self) -> u32 {
        // 4: Claude titles are now extracted, so cached title-less metadata
        // from revision 3 must be revisited.
        4
    }

    fn list_candidates(
        &self,
        store: &KnownStore,
        project: &ProjectIdentity,
        errors: &mut Vec<ConversationSourceError>,
        cancelled: &AtomicBool,
    ) -> Vec<PathBuf> {
        let expected = PathBuf::from(claude_project_directory(project.root()));
        let entries = match store.list_directory(Path::new("")) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(error) => {
                push_listing_error(
                    errors,
                    SOURCE_ID,
                    store.absolute(Path::new(".")),
                    "Claude projects store cannot be listed",
                    &error,
                );
                return Vec::new();
            }
        };
        let mut directories = Vec::new();
        let mut visited_entries = 0_usize;
        for (name, kind) in entries {
            if cancelled.load(Ordering::Relaxed) {
                return Vec::new();
            }
            visited_entries = visited_entries.saturating_add(1);
            let relative = PathBuf::from(&name);
            if kind == EntryKind::Directory {
                directories.push(relative);
            } else {
                push_shape_error(
                    errors,
                    SOURCE_ID,
                    store.absolute(&relative),
                    "Claude projects store contains a non-directory entry",
                );
            }
        }
        directories.sort_unstable_by(|left, right| {
            (left != &expected)
                .cmp(&(right != &expected))
                .then_with(|| left.cmp(right))
        });
        if directories.len() > MAX_PROJECT_DIRECTORIES {
            push_inventory_error(
                errors,
                SOURCE_ID,
                store.absolute(Path::new(".")),
                "Claude projects store exceeds the bounded directory inventory",
            );
            directories.truncate(MAX_PROJECT_DIRECTORIES);
        }

        let mut files = Vec::new();
        for directory in directories {
            if cancelled.load(Ordering::Relaxed) {
                break;
            }
            let entries = match store.list_directory(&directory) {
                Ok(entries) => entries,
                Err(error) => {
                    push_listing_error(
                        errors,
                        SOURCE_ID,
                        store.absolute(&directory),
                        "Claude project store cannot be listed",
                        &error,
                    );
                    continue;
                }
            };
            for (name, kind) in entries {
                if cancelled.load(Ordering::Relaxed) {
                    return files;
                }
                if visited_entries == MAX_VISITED_ENTRIES {
                    push_inventory_error(
                        errors,
                        SOURCE_ID,
                        store.absolute(Path::new(".")),
                        "Claude projects store exceeds the bounded traversal budget",
                    );
                    return files;
                }
                visited_entries += 1;
                let relative = directory.join(&name);
                match kind {
                    EntryKind::File
                        if Path::new(&name)
                            .extension()
                            .is_some_and(|value| value == "jsonl") =>
                    {
                        if files.len() == MAX_CANDIDATE_PATHS {
                            push_inventory_error(
                                errors,
                                SOURCE_ID,
                                store.absolute(Path::new(".")),
                                "Claude projects store exceeds the bounded session inventory",
                            );
                            return files;
                        }
                        files.push(relative);
                    }
                    // Claude Code writes per-session `<uuid>/tool-results` and
                    // per-project `memory` directories next to the transcripts.
                    EntryKind::Directory => {}
                    _ => {
                        push_shape_error(
                            errors,
                            SOURCE_ID,
                            store.absolute(&relative),
                            "Claude store entry is outside the verified flat JSONL layout",
                        );
                    }
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
        previous_pending: Option<&PendingMetadata>,
    ) -> Result<ParseOutcome, FormatFailure> {
        let encoded_directory_matches = relative.parent().and_then(Path::file_name)
            == Some(claude_project_directory(project.root()).as_os_str());
        let expected_id = relative
            .file_stem()
            .and_then(|value| value.to_str())
            .ok_or_else(|| FormatFailure::unsupported("Claude session filename is invalid"))?;
        validate_uuid(expected_id, 4)?;

        let mut session_id = previous.map(|metadata| metadata.session_id.clone());
        let mut title = previous
            .and_then(|metadata| metadata.title.clone())
            .or_else(|| previous_pending.and_then(|metadata| metadata.title.clone()));
        let mut created_at = previous
            .map(|metadata| metadata.created_at)
            .or_else(|| previous_pending.and_then(|metadata| metadata.created_at));
        let mut updated_at = previous
            .map(|metadata| metadata.updated_at)
            .or_else(|| previous_pending.and_then(|metadata| metadata.updated_at));
        let mut previous_uuid = previous.and_then(|metadata| metadata.chain_tail.clone());
        let mut canonical = previous.map(|metadata| metadata.cwd.clone());
        let mut record_count = previous.map_or_else(
            || previous_pending.map_or(0, |metadata| metadata.record_count),
            |metadata| metadata.record_count,
        );
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
            let canonical_record_cwd = record
                .cwd
                .as_deref()
                .map(|cwd| canonical_cwd(cwd, project))
                .transpose()?;

            let carries_identity = match record.kind.as_str() {
                "user" | "assistant" | "system" => true,
                "attachment" => {
                    record.session_id.is_some()
                        || record.cwd.is_some()
                        || record.version.is_some()
                        || record.is_sidechain.is_some()
                }
                "last-prompt"
                | "permission-mode"
                | "ai-title"
                | "file-history-snapshot"
                | "queue-operation"
                | "mode"
                | "custom-title"
                | "agent-name"
                | "agent-setting" => false,
                // Auxiliary metadata is open-ended in Claude Code. Unknown
                // records cannot establish any session metadata below.
                _ => false,
            };
            if carries_identity {
                let payload_shape_valid = match record.kind.as_str() {
                    "user" | "assistant" => record.message.is_some() && record.attachment.is_none(),
                    "attachment" => record.attachment.is_some() && record.message.is_none(),
                    "system" => {
                        record
                            .subtype
                            .as_deref()
                            .is_some_and(|value| !value.is_empty())
                            && record.message.is_none()
                            && record.attachment.is_none()
                    }
                    _ => false,
                };
                if !payload_shape_valid {
                    return Err(FormatFailure::unsupported(
                        "Claude transcript record has an invalid payload shape",
                    ));
                }
                let native_id = validate_claude_session(&record, expected_id)?;
                validate_tool_version(record.version.as_deref())?;
                if record.is_sidechain != Some(false) {
                    return Err(FormatFailure::unsupported(
                        "Claude sidechain transcript records are unsupported",
                    ));
                }
                if session_id.as_deref().is_some_and(|id| id != native_id) {
                    return Err(FormatFailure::unsupported(
                        "Claude JSONL mixes native session identifiers",
                    ));
                }

                let cwd = match canonical_record_cwd {
                    Some(cwd) => cwd,
                    None if encoded_directory_matches => crate::project::CanonicalPath::new(
                        project.root().to_path_buf(),
                    )
                    .map_err(|_| {
                        FormatFailure::project_mismatch(
                            "Claude encoded project directory cannot be canonicalized",
                        )
                    })?,
                    None => {
                        return Err(FormatFailure::project_mismatch(
                            "Claude record has neither canonical cwd nor matching project directory",
                        ));
                    }
                };

                let uuid = record.uuid.as_deref().ok_or_else(|| {
                    FormatFailure::unsupported(
                        "Claude transcript record is missing its native entry identifier",
                    )
                })?;
                validate_uuid(uuid, 4)?;
                let compact_boundary = record.kind == "system"
                    && record.subtype.as_deref() == Some("compact_boundary");
                match (previous_uuid.is_some(), record.parent_uuid.as_deref()) {
                    (false, None) => {}
                    (true, Some(parent_uuid)) if parent_uuid != uuid => {
                        validate_uuid(parent_uuid, 4)?;
                    }
                    // A compaction boundary deliberately restarts the transcript
                    // chain: the pre-compact tail moves into `logicalParentUuid`.
                    (true, None) if compact_boundary => {
                        let logical_parent =
                            record.logical_parent_uuid.as_deref().ok_or_else(|| {
                                FormatFailure::unsupported(
                                    "Claude compact boundary is missing its logical parent identifier",
                                )
                            })?;
                        validate_uuid(logical_parent, 4)?;
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
                canonical.get_or_insert(cwd);
            } else {
                match record.kind.as_str() {
                    "attachment" => {
                        if record.attachment.is_none()
                            || record.message.is_some()
                            || record.version.is_some()
                            || record.cwd.is_some()
                            || record.session_id.is_some()
                            || record.is_sidechain.is_some()
                            || record.parent_uuid.is_some()
                        {
                            return Err(FormatFailure::unsupported(
                                "Claude attachment record has an invalid payload shape",
                            ));
                        }
                        validate_uuid(
                            record.uuid.as_deref().ok_or_else(|| {
                                FormatFailure::unsupported(
                                    "Claude attachment is missing its native entry identifier",
                                )
                            })?,
                            4,
                        )?;
                        let timestamp =
                            parse_rfc3339(record.timestamp.as_deref().ok_or_else(|| {
                                FormatFailure::unsupported(
                                    "Claude attachment is missing its timestamp",
                                )
                            })?)?;
                        created_at =
                            Some(created_at.map_or(timestamp, |current| current.min(timestamp)));
                        updated_at =
                            Some(updated_at.map_or(timestamp, |current| current.max(timestamp)));
                    }
                    "last-prompt" => {
                        validate_claude_session(&record, expected_id)?;
                        validate_uuid(
                            record.leaf_uuid.as_deref().ok_or_else(|| {
                                FormatFailure::unsupported(
                                    "Claude last-prompt record is missing its leaf UUID",
                                )
                            })?,
                            4,
                        )?;
                    }
                    "permission-mode" => {
                        validate_claude_session(&record, expected_id)?;
                        if record.permission_mode.is_none() {
                            return Err(FormatFailure::unsupported(
                                "Claude permission-mode record is missing its mode",
                            ));
                        }
                    }
                    "ai-title" => {
                        validate_claude_session(&record, expected_id)?;
                        if title.is_none() {
                            title = record
                                .ai_title
                                .as_ref()
                                .and_then(serde_json::Value::as_str)
                                .and_then(normalize_metadata_title);
                        }
                    }
                    "queue-operation" => {
                        validate_claude_session(&record, expected_id)?;
                        if record.operation.is_none() {
                            return Err(FormatFailure::unsupported(
                                "Claude queue-operation record is missing its operation",
                            ));
                        }
                        let timestamp =
                            parse_rfc3339(record.timestamp.as_deref().ok_or_else(|| {
                                FormatFailure::unsupported(
                                    "Claude queue-operation record is missing its timestamp",
                                )
                            })?)?;
                        created_at =
                            Some(created_at.map_or(timestamp, |current| current.min(timestamp)));
                        updated_at =
                            Some(updated_at.map_or(timestamp, |current| current.max(timestamp)));
                    }
                    "custom-title" => {
                        validate_claude_session(&record, expected_id)?;
                        if let Some(native_title) = record
                            .custom_title
                            .as_ref()
                            .and_then(serde_json::Value::as_str)
                            .and_then(normalize_metadata_title)
                        {
                            title = Some(native_title);
                        }
                    }
                    "mode" | "agent-name" | "agent-setting" => {
                        validate_claude_session(&record, expected_id)?;
                        let (present, message) = match record.kind.as_str() {
                            "mode" => (
                                record.mode.is_some(),
                                "Claude mode record is missing its mode",
                            ),
                            "agent-name" => (
                                record.agent_name.is_some(),
                                "Claude agent-name record is missing its name",
                            ),
                            "agent-setting" => (
                                record.agent_setting.is_some(),
                                "Claude agent-setting record is missing its setting",
                            ),
                            _ => unreachable!("record kind matched above"),
                        };
                        if !present {
                            return Err(FormatFailure::unsupported(message));
                        }
                    }
                    "file-history-snapshot" => {
                        validate_uuid(
                            record.message_id.as_deref().ok_or_else(|| {
                                FormatFailure::unsupported(
                                    "Claude file-history snapshot is missing its message UUID",
                                )
                            })?,
                            4,
                        )?;
                        if record.snapshot.is_none() || record.is_snapshot_update.is_none() {
                            return Err(FormatFailure::unsupported(
                                "Claude file-history snapshot has an invalid payload shape",
                            ));
                        }
                    }
                    _ => {}
                }
            }
            record_count = record_count.saturating_add(1);
        }

        let Some(session_id) = session_id else {
            return Ok(ParseOutcome::IdentityPending(PendingMetadata {
                title,
                created_at,
                updated_at,
                record_count,
            }));
        };
        Ok(ParseOutcome::Metadata(ParsedMetadata {
            session_id,
            title,
            created_at: created_at.ok_or_else(|| {
                FormatFailure::unsupported("Claude JSONL contains no current timestamp")
            })?,
            updated_at: updated_at.ok_or_else(|| {
                FormatFailure::unsupported("Claude JSONL contains no current timestamp")
            })?,
            chain_updated_at: updated_at.ok_or_else(|| {
                FormatFailure::unsupported("Claude JSONL contains no current timestamp")
            })?,
            cwd: canonical.ok_or_else(|| {
                FormatFailure::project_mismatch(
                    "Claude JSONL contains no canonical project evidence",
                )
            })?,
            chain_tail: previous_uuid,
            record_count,
        }))
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
    subtype: Option<String>,
    #[serde(rename = "leafUuid")]
    leaf_uuid: Option<String>,
    #[serde(rename = "logicalParentUuid")]
    logical_parent_uuid: Option<String>,
    #[serde(rename = "permissionMode")]
    permission_mode: Option<IgnoredAny>,
    #[serde(rename = "aiTitle")]
    ai_title: Option<serde_json::Value>,
    mode: Option<IgnoredAny>,
    #[serde(rename = "customTitle")]
    custom_title: Option<serde_json::Value>,
    #[serde(rename = "agentName")]
    agent_name: Option<IgnoredAny>,
    #[serde(rename = "agentSetting")]
    agent_setting: Option<IgnoredAny>,
    #[serde(rename = "messageId")]
    message_id: Option<String>,
    snapshot: Option<IgnoredAny>,
    #[serde(rename = "isSnapshotUpdate")]
    is_snapshot_update: Option<bool>,
    operation: Option<IgnoredAny>,
}

fn validate_claude_session<'a>(
    record: &'a ClaudeRecord,
    expected_id: &str,
) -> Result<&'a str, FormatFailure> {
    let native_id = record.session_id.as_deref().ok_or_else(|| {
        FormatFailure::unsupported("Claude record is missing its native session identifier")
    })?;
    validate_uuid(native_id, 4)?;
    if native_id != expected_id {
        return Err(FormatFailure::unsupported(
            "Claude filename and native session identifier conflict",
        ));
    }
    Ok(native_id)
}
