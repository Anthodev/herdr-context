use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Deserialize;
use serde::de::IgnoredAny;

use super::known_stores::{
    EntryKind, FormatFailure, KnownFormat, KnownJsonlSource, KnownStore, ParsedMetadata,
    canonical_cwd, parse_rfc3339, pi_project_directory, push_listing_error, push_shape_error,
    validate_uuid,
};
use super::{
    ConversationCandidate, ConversationSource, ConversationSourceError, DiscoveryBatch,
    DiscoveryLimit, MetadataBudget, ProjectAssociationEvidence, SourceId, SourceWatermark,
    StorageProbe,
};
use crate::conversations::Conversation;
use crate::project::ProjectIdentity;

const SOURCE_ID: &str = "pi";
const SCHEMA_VERSION: u64 = 3;

#[derive(Debug)]
pub struct PiSource {
    inner: KnownJsonlSource<PiFormat>,
}

impl PiSource {
    pub fn new(
        project: ProjectIdentity,
        store_root: PathBuf,
    ) -> Result<Self, ConversationSourceError> {
        Ok(Self {
            inner: KnownJsonlSource::new(project, store_root, PiFormat)?,
        })
    }
}

impl ConversationSource for PiSource {
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
struct PiFormat;

impl KnownFormat for PiFormat {
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
        let directory = PathBuf::from(pi_project_directory(project.root()));
        let entries = match store.list_directory(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(error) => {
                push_listing_error(
                    errors,
                    SOURCE_ID,
                    store.absolute(&directory),
                    "Pi project store cannot be listed",
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
                        "Pi store entry is outside the verified flat JSONL layout",
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
        let (
            session_id,
            cwd,
            created_at,
            mut updated_at,
            mut previous_id,
            append_start,
            mut record_count,
        ) = if let Some(previous) = previous {
            (
                previous.session_id.clone(),
                previous.cwd.clone(),
                previous.created_at,
                previous.updated_at,
                previous.chain_tail.clone(),
                0,
                previous.record_count,
            )
        } else {
            let header: PiRecord = serde_json::from_slice(records[0]).map_err(|_| {
                FormatFailure::unsupported("Pi header does not match the verified JSON shape")
            })?;
            if header.kind != "session" || header.version != Some(SCHEMA_VERSION) {
                return Err(FormatFailure::unsupported(
                    "Pi JSONL does not start with the verified schema-v3 session header",
                ));
            }
            validate_uuid(&header.id, 7)?;
            let cwd = canonical_cwd(
                header.cwd.as_deref().ok_or_else(|| {
                    FormatFailure::project_mismatch("Pi session header is missing canonical cwd")
                })?,
                project,
            )?;
            let created_at = parse_rfc3339(&header.timestamp)?;
            validate_pi_path(relative, &header.id, &header.timestamp, project)?;
            (header.id, cwd, created_at, created_at, None, 1, 1)
        };

        for bytes in &records[append_start..] {
            if cancelled.load(Ordering::Relaxed) {
                return Err(FormatFailure::cancelled());
            }
            let record: PiRecord = serde_json::from_slice(bytes).map_err(|_| {
                FormatFailure::unsupported("Pi append record has an unverified JSON shape")
            })?;
            if record.version.is_some() || record.cwd.is_some() || !record.has_valid_payload() {
                return Err(FormatFailure::unsupported(
                    "Pi append record type is outside the verified current session set",
                ));
            }
            validate_entry_id(&record.id)?;
            match (previous_id.is_some(), record.parent_id.as_deref()) {
                (false, None) => {}
                (true, Some(parent_id)) if parent_id != record.id => validate_entry_id(parent_id)?,
                _ => {
                    return Err(FormatFailure::unsupported(
                        "Pi entry root/parent shape does not match the current session tree",
                    ));
                }
            }
            if record.kind == "message" {
                let message = record.message.ok_or_else(|| {
                    FormatFailure::unsupported(
                        "Pi message record is missing native message metadata",
                    )
                })?;
                if !matches!(
                    message.role.as_str(),
                    "user"
                        | "assistant"
                        | "toolResult"
                        | "bashExecution"
                        | "custom"
                        | "compactionSummary"
                        | "branchSummary"
                ) || message.timestamp <= 0
                {
                    return Err(FormatFailure::unsupported(
                        "Pi message metadata is outside the verified current session set",
                    ));
                }
            } else if record.message.is_some() {
                return Err(FormatFailure::unsupported(
                    "Pi non-message entry unexpectedly contains message metadata",
                ));
            }
            let timestamp = parse_rfc3339(&record.timestamp)?;
            if timestamp < updated_at {
                return Err(FormatFailure::unsupported(
                    "Pi append timestamps are not monotonic",
                ));
            }
            updated_at = timestamp;
            previous_id = Some(record.id);
            record_count = record_count.saturating_add(1);
        }
        Ok(ParsedMetadata {
            session_id,
            created_at,
            updated_at,
            cwd,
            chain_tail: previous_id,
            record_count,
        })
    }
}

fn validate_entry_id(value: &str) -> Result<(), FormatFailure> {
    if value.len() != 8
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(FormatFailure::unsupported(
            "Pi native entry identifier is outside the verified lowercase-hex shape",
        ));
    }
    Ok(())
}

fn validate_pi_path(
    relative: &Path,
    id: &str,
    timestamp: &str,
    project: &ProjectIdentity,
) -> Result<(), FormatFailure> {
    let expected_directory = pi_project_directory(project.root());
    if relative.parent().and_then(Path::file_name) != Some(expected_directory.as_os_str()) {
        return Err(FormatFailure::project_mismatch(
            "Pi encoded project directory conflicts with canonical cwd",
        ));
    }
    let name = relative
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| FormatFailure::unsupported("Pi session filename is invalid"))?;
    let expected_name = format!("{}_{id}.jsonl", timestamp.replace([':', '.'], "-"));
    if name != expected_name {
        return Err(FormatFailure::unsupported(
            "Pi filename hints conflict with native session metadata",
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
struct PiRecord {
    #[serde(rename = "type")]
    kind: String,
    version: Option<u64>,
    id: String,
    #[serde(rename = "parentId")]
    parent_id: Option<String>,
    timestamp: String,
    cwd: Option<String>,
    message: Option<PiMessage>,
    #[serde(rename = "thinkingLevel")]
    thinking_level: Option<IgnoredAny>,
    provider: Option<IgnoredAny>,
    #[serde(rename = "modelId")]
    model_id: Option<IgnoredAny>,
    summary: Option<IgnoredAny>,
    #[serde(rename = "firstKeptEntryId")]
    first_kept_entry_id: Option<IgnoredAny>,
    #[serde(rename = "tokensBefore")]
    tokens_before: Option<IgnoredAny>,
    #[serde(rename = "fromId")]
    from_id: Option<IgnoredAny>,
    #[serde(rename = "customType")]
    custom_type: Option<IgnoredAny>,
    content: Option<IgnoredAny>,
    display: Option<IgnoredAny>,
    #[serde(rename = "targetId")]
    target_id: Option<IgnoredAny>,
}

impl PiRecord {
    fn has_valid_payload(&self) -> bool {
        match self.kind.as_str() {
            "message" => self.message.is_some(),
            "thinking_level_change" => self.thinking_level.is_some(),
            "model_change" => self.provider.is_some() && self.model_id.is_some(),
            "compaction" => {
                self.summary.is_some()
                    && self.first_kept_entry_id.is_some()
                    && self.tokens_before.is_some()
            }
            "branch_summary" => self.from_id.is_some() && self.summary.is_some(),
            "custom" => self.custom_type.is_some(),
            "custom_message" => {
                self.custom_type.is_some() && self.content.is_some() && self.display.is_some()
            }
            "label" => self.target_id.is_some(),
            "session_info" => true,
            _ => false,
        }
    }
}

#[derive(Deserialize)]
struct PiMessage {
    role: String,
    #[serde(rename = "content")]
    _content: IgnoredAny,
    timestamp: i64,
}
