use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Deserialize;
use serde::de::IgnoredAny;

use super::known_stores::{
    EntryKind, FormatFailure, KnownFormat, KnownJsonlSource, KnownStore, MAX_CANDIDATE_PATHS,
    ParsedMetadata, canonical_cwd, parse_rfc3339, push_inventory_error, push_listing_error,
    push_shape_error, validate_uuid,
};
use super::{
    ConversationCandidate, ConversationSource, ConversationSourceError, DiscoveryBatch,
    DiscoveryLimit, MetadataBudget, ProjectAssociationEvidence, SourceId, SourceWatermark,
    StorageProbe,
};
use crate::conversations::Conversation;
use crate::project::ProjectIdentity;

const SOURCE_ID: &str = "codex-cli";
const VERSION: &str = "0.147.0";
const MAX_LAYOUT_DIRECTORIES: usize = 2_000;
const MAX_VISITED_ENTRIES: usize = MAX_CANDIDATE_PATHS + 3 * MAX_LAYOUT_DIRECTORIES;

#[derive(Debug)]
pub struct CodexCliSource {
    inner: KnownJsonlSource<CodexCliFormat>,
}

impl CodexCliSource {
    pub fn new(
        project: ProjectIdentity,
        store_root: PathBuf,
    ) -> Result<Self, ConversationSourceError> {
        Ok(Self {
            inner: KnownJsonlSource::new(project, store_root, CodexCliFormat)?,
        })
    }
}

impl ConversationSource for CodexCliSource {
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
struct CodexCliFormat;

impl KnownFormat for CodexCliFormat {
    fn source_id(&self) -> &'static str {
        SOURCE_ID
    }

    fn tool_id(&self) -> &'static str {
        SOURCE_ID
    }

    fn list_candidates(
        &self,
        store: &KnownStore,
        _project: &ProjectIdentity,
        errors: &mut Vec<ConversationSourceError>,
        cancelled: &AtomicBool,
    ) -> Vec<PathBuf> {
        let mut directories = vec![PathBuf::new()];
        let mut files = Vec::new();
        let mut visited_entries = 0_usize;
        for depth in 0..3 {
            let mut children = Vec::new();
            for directory in directories {
                if cancelled.load(Ordering::Relaxed) {
                    return files;
                }
                let entries = match store.list_directory(&directory) {
                    Ok(entries) => entries,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => {
                        push_listing_error(
                            errors,
                            SOURCE_ID,
                            store.absolute(&directory),
                            "Codex date directory cannot be listed",
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
                            "Codex store exceeds the bounded traversal budget",
                        );
                        return files;
                    }
                    visited_entries += 1;
                    let relative = directory.join(&name);
                    if kind == EntryKind::Directory && valid_date_component(depth, &name) {
                        children.push(relative);
                    } else {
                        push_shape_error(
                            errors,
                            SOURCE_ID,
                            store.absolute(&relative),
                            "Codex store entry is outside the verified YYYY/MM/DD layout",
                        );
                    }
                }
            }
            children.sort_unstable_by(|left, right| right.cmp(left));
            if children.len() > MAX_LAYOUT_DIRECTORIES {
                push_inventory_error(
                    errors,
                    SOURCE_ID,
                    store.absolute(Path::new(".")),
                    "Codex store exceeds the bounded date-directory inventory",
                );
                children.truncate(MAX_LAYOUT_DIRECTORIES);
            }
            directories = children;
        }
        for directory in directories {
            if cancelled.load(Ordering::Relaxed) {
                return files;
            }
            let entries = match store.list_directory(&directory) {
                Ok(entries) => entries,
                Err(error) => {
                    push_listing_error(
                        errors,
                        SOURCE_ID,
                        store.absolute(&directory),
                        "Codex session directory cannot be listed",
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
                        "Codex store exceeds the bounded traversal budget",
                    );
                    return files;
                }
                visited_entries += 1;
                let relative = directory.join(&name);
                if kind == EntryKind::File && codex_file_name(Path::new(&name)).is_some() {
                    if files.len() == MAX_CANDIDATE_PATHS {
                        push_inventory_error(
                            errors,
                            SOURCE_ID,
                            store.absolute(Path::new(".")),
                            "Codex store exceeds the bounded session inventory",
                        );
                        return files;
                    }
                    files.push(relative);
                } else {
                    push_shape_error(
                        errors,
                        SOURCE_ID,
                        store.absolute(&relative),
                        "Codex session entry is outside the verified rollout JSONL layout",
                    );
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
        let (id, cwd, created_at, mut updated_at, append_start, mut record_count, mut history) =
            if let Some(previous) = previous {
                let history = CodexHistory::from_watermark(previous.chain_tail.as_deref())?;
                (
                    previous.session_id.clone(),
                    previous.cwd.clone(),
                    previous.created_at,
                    previous.updated_at,
                    0,
                    previous.record_count,
                    history,
                )
            } else {
                let header: CodexRecord = serde_json::from_slice(records[0]).map_err(|_| {
                    FormatFailure::unsupported(
                        "Codex header does not match the verified JSON shape",
                    )
                })?;
                if header.kind != "session_meta" {
                    return Err(FormatFailure::unsupported(
                        "Codex JSONL does not start with session_meta",
                    ));
                }
                let id = header.payload.id.as_deref().ok_or_else(|| {
                    FormatFailure::unsupported(
                        "Codex session_meta is missing its native session ID",
                    )
                })?;
                validate_uuid(id, 7)?;
                if header.payload.session_id.as_deref() != Some(id) {
                    return Err(FormatFailure::unsupported(
                        "Codex session_meta thread and session identifiers conflict",
                    ));
                }
                if header.payload.cli_version.as_deref() != Some(VERSION) {
                    return Err(FormatFailure::unsupported(
                        "Codex CLI version is outside the committed inventory",
                    ));
                }
                let history = CodexHistory::from_header(
                    header.payload.history_mode.as_deref(),
                    header.ordinal,
                )?;
                let cwd = canonical_cwd(
                    header.payload.cwd.as_deref().ok_or_else(|| {
                        FormatFailure::project_mismatch(
                            "Codex session_meta is missing canonical cwd",
                        )
                    })?,
                    project,
                )?;
                let created_text = header.payload.timestamp.as_deref().ok_or_else(|| {
                    FormatFailure::unsupported("Codex session_meta is missing its start timestamp")
                })?;
                let created_at = parse_rfc3339(created_text)?;
                let header_written_at = parse_rfc3339(&header.timestamp)?;
                if header_written_at < created_at {
                    return Err(FormatFailure::unsupported(
                        "Codex record-write timestamp precedes session start",
                    ));
                }
                validate_codex_path(relative, id)?;
                (
                    id.to_owned(),
                    cwd,
                    created_at,
                    header_written_at,
                    1,
                    1,
                    history,
                )
            };

        for bytes in &records[append_start..] {
            if cancelled.load(Ordering::Relaxed) {
                return Err(FormatFailure::cancelled());
            }
            let record: CodexRecord = serde_json::from_slice(bytes).map_err(|_| {
                FormatFailure::unsupported("Codex append record has an unverified JSON shape")
            })?;
            if !record.payload.is_valid_append(record.kind.as_str()) {
                return Err(FormatFailure::unsupported(
                    "Codex append record is outside the verified current rollout set",
                ));
            }
            history.advance(record.ordinal)?;
            let timestamp = parse_rfc3339(&record.timestamp)?;
            if timestamp < updated_at {
                return Err(FormatFailure::unsupported(
                    "Codex append timestamps are not monotonic",
                ));
            }
            updated_at = timestamp;
            record_count = record_count.saturating_add(1);
        }
        Ok(ParsedMetadata {
            session_id: id,
            title: None,
            created_at,
            updated_at,
            chain_updated_at: updated_at,
            cwd,
            chain_tail: Some(history.watermark()),
            record_count,
        })
    }
}

#[derive(Clone, Copy)]
enum CodexHistory {
    Legacy,
    Paginated(u64),
}

impl CodexHistory {
    fn from_header(mode: Option<&str>, ordinal: Option<u64>) -> Result<Self, FormatFailure> {
        match (mode, ordinal) {
            (Some("legacy"), None) => Ok(Self::Legacy),
            (Some("paginated"), Some(0)) => Ok(Self::Paginated(0)),
            _ => Err(FormatFailure::unsupported(
                "Codex history mode and rollout ordinal conflict",
            )),
        }
    }

    fn from_watermark(value: Option<&str>) -> Result<Self, FormatFailure> {
        match value {
            Some("legacy") => Ok(Self::Legacy),
            Some(value) => value
                .strip_prefix("paginated:")
                .and_then(|ordinal| ordinal.parse::<u64>().ok())
                .map(Self::Paginated)
                .ok_or_else(|| {
                    FormatFailure::unsupported(
                        "Codex incremental state has an invalid history cursor",
                    )
                }),
            None => Err(FormatFailure::unsupported(
                "Codex incremental state is missing its history cursor",
            )),
        }
    }

    fn advance(&mut self, ordinal: Option<u64>) -> Result<(), FormatFailure> {
        match self {
            Self::Legacy if ordinal.is_none() => Ok(()),
            Self::Paginated(current) => {
                let expected = current.checked_add(1).ok_or_else(|| {
                    FormatFailure::unsupported("Codex rollout ordinal exceeds the supported range")
                })?;
                if ordinal != Some(expected) {
                    return Err(FormatFailure::unsupported(
                        "Codex paginated rollout ordinals are not contiguous",
                    ));
                }
                *current = expected;
                Ok(())
            }
            Self::Legacy => Err(FormatFailure::unsupported(
                "Codex legacy rollout unexpectedly contains an ordinal",
            )),
        }
    }

    fn watermark(self) -> String {
        match self {
            Self::Legacy => "legacy".to_owned(),
            Self::Paginated(ordinal) => format!("paginated:{ordinal}"),
        }
    }
}

fn valid_date_component(depth: usize, value: &std::ffi::OsStr) -> bool {
    let Some(value) = value.to_str() else {
        return false;
    };
    let parsed = value.parse::<u8>().ok();
    match depth {
        0 => value.len() == 4 && value.bytes().all(|byte| byte.is_ascii_digit()),
        1 => value.len() == 2 && parsed.is_some_and(|value| (1..=12).contains(&value)),
        2 => value.len() == 2 && parsed.is_some_and(|value| (1..=31).contains(&value)),
        _ => false,
    }
}

fn codex_file_name(path: &Path) -> Option<&str> {
    let name = path.file_name()?.to_str()?;
    (name.starts_with("rollout-") && name.ends_with(".jsonl")).then_some(name)
}

fn validate_codex_path(relative: &Path, id: &str) -> Result<(), FormatFailure> {
    let components = relative
        .components()
        .map(|component| component.as_os_str().to_str())
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| FormatFailure::unsupported("Codex path is not valid UTF-8"))?;
    if components.len() != 4
        || !components[..3]
            .iter()
            .enumerate()
            .all(|(depth, value)| valid_date_component(depth, std::ffi::OsStr::new(value)))
    {
        return Err(FormatFailure::unsupported(
            "Codex path is outside the verified date layout",
        ));
    }
    let prefix = format!(
        "rollout-{}-{}-{}T",
        components[0], components[1], components[2]
    );
    let suffix = format!("-{id}.jsonl");
    let local_time = components[3]
        .strip_prefix(&prefix)
        .and_then(|value| value.strip_suffix(&suffix))
        .ok_or_else(|| {
            FormatFailure::unsupported(
                "Codex rollout filename does not repeat its date and native session ID",
            )
        })?;
    let parts = local_time.split('-').collect::<Vec<_>>();
    if parts.len() != 3
        || parts
            .iter()
            .any(|value| value.len() != 2 || !value.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return Err(FormatFailure::unsupported(
            "Codex rollout local time is invalid",
        ));
    }
    let values = parts
        .iter()
        .map(|value| value.parse::<u8>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| FormatFailure::unsupported("Codex rollout local time is invalid"))?;
    if values[0] > 23 || values[1] > 59 || values[2] > 59 {
        return Err(FormatFailure::unsupported(
            "Codex rollout local time is invalid",
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
struct CodexRecord {
    timestamp: String,
    ordinal: Option<u64>,
    #[serde(rename = "type")]
    kind: String,
    payload: CodexPayload,
}

#[derive(Deserialize)]
struct CodexPayload {
    id: Option<String>,
    session_id: Option<String>,
    timestamp: Option<String>,
    cwd: Option<String>,
    cli_version: Option<String>,
    history_mode: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    full: Option<bool>,
    state: Option<IgnoredAny>,
    trigger_turn: Option<bool>,
    author: Option<IgnoredAny>,
    recipient: Option<IgnoredAny>,
    content: Option<IgnoredAny>,
    message: Option<IgnoredAny>,
    model: Option<IgnoredAny>,
    role: Option<IgnoredAny>,
    tools: Option<IgnoredAny>,
    summary: Option<IgnoredAny>,
    encrypted_content: Option<IgnoredAny>,
    call_id: Option<IgnoredAny>,
    status: Option<IgnoredAny>,
    action: Option<IgnoredAny>,
    name: Option<IgnoredAny>,
    arguments: Option<IgnoredAny>,
    execution: Option<IgnoredAny>,
    output: Option<IgnoredAny>,
    input: Option<IgnoredAny>,
    result: Option<IgnoredAny>,
    approval_policy: Option<IgnoredAny>,
    sandbox_policy: Option<IgnoredAny>,
}

impl CodexPayload {
    fn is_valid_append(&self, kind: &str) -> bool {
        match kind {
            "response_item" => self.is_valid_response_item(),
            "event_msg" => self.kind.as_deref().is_some_and(|kind| {
                is_known_event_kind(kind) && (kind != "user_message" || self.message.is_some())
            }),
            "inter_agent_communication" => {
                self.author.is_some() && self.recipient.is_some() && self.content.is_some()
            }
            "inter_agent_communication_metadata" => self.trigger_turn.is_some(),
            "compacted" => self.message.is_some(),
            "turn_context" => {
                self.cwd.is_some()
                    && self.model.is_some()
                    && self.approval_policy.is_some()
                    && self.sandbox_policy.is_some()
            }
            "world_state" => self.full.is_some() && self.state.is_some(),
            _ => false,
        }
    }

    fn is_valid_response_item(&self) -> bool {
        match self.kind.as_deref() {
            Some("additional_tools") => self.role.is_some() && self.tools.is_some(),
            Some("message") => self.role.is_some() && self.content.is_some(),
            Some("agent_message") => {
                self.author.is_some() && self.recipient.is_some() && self.content.is_some()
            }
            Some("reasoning") => self.summary.is_some(),
            Some("local_shell_call") => self.status.is_some() && self.action.is_some(),
            Some("function_call") => {
                self.name.is_some() && self.arguments.is_some() && self.call_id.is_some()
            }
            Some("tool_search_call") => self.execution.is_some() && self.arguments.is_some(),
            Some("function_call_output" | "custom_tool_call_output") => {
                self.call_id.is_some() && self.output.is_some()
            }
            Some("custom_tool_call") => {
                self.call_id.is_some() && self.name.is_some() && self.input.is_some()
            }
            Some("tool_search_output") => {
                self.status.is_some() && self.execution.is_some() && self.tools.is_some()
            }
            Some("image_generation_call") => self.status.is_some() && self.result.is_some(),
            Some("compaction" | "compaction_summary") => self.encrypted_content.is_some(),
            Some("web_search_call" | "compaction_trigger" | "context_compaction") => true,
            Some(_) | None => false,
        }
    }
}

fn is_known_event_kind(kind: &str) -> bool {
    matches!(
        kind,
        "error"
            | "warning"
            | "guardian_warning"
            | "realtime_conversation_started"
            | "realtime_conversation_realtime"
            | "realtime_conversation_closed"
            | "realtime_conversation_sdp"
            | "model_reroute"
            | "model_verification"
            | "turn_moderation_metadata"
            | "safety_buffering"
            | "context_compacted"
            | "thread_rolled_back"
            | "task_started"
            | "turn_started"
            | "thread_settings_applied"
            | "task_complete"
            | "turn_complete"
            | "token_count"
            | "agent_message"
            | "user_message"
            | "agent_reasoning"
            | "agent_reasoning_raw_content"
            | "agent_reasoning_section_break"
            | "session_configured"
            | "environment_connected"
            | "environment_disconnected"
            | "thread_goal_updated"
            | "mcp_startup_update"
            | "mcp_startup_complete"
            | "mcp_tool_call_begin"
            | "mcp_tool_call_end"
            | "web_search_begin"
            | "web_search_end"
            | "image_generation_begin"
            | "image_generation_end"
            | "exec_command_begin"
            | "exec_command_output_delta"
            | "terminal_interaction"
            | "exec_command_end"
            | "view_image_tool_call"
            | "exec_approval_request"
            | "request_permissions"
            | "request_user_input"
            | "dynamic_tool_call_request"
            | "dynamic_tool_call_response"
            | "elicitation_request"
            | "apply_patch_approval_request"
            | "guardian_assessment"
            | "deprecation_notice"
            | "stream_error"
            | "patch_apply_begin"
            | "patch_apply_updated"
            | "patch_apply_end"
            | "turn_diff"
            | "realtime_conversation_list_voices_response"
            | "plan_update"
            | "turn_aborted"
            | "shutdown_complete"
            | "entered_review_mode"
            | "exited_review_mode"
            | "raw_response_item"
            | "raw_response_completed"
            | "item_started"
            | "item_completed"
            | "hook_started"
            | "hook_completed"
            | "agent_message_content_delta"
            | "plan_delta"
            | "reasoning_content_delta"
            | "reasoning_raw_content_delta"
            | "collab_agent_spawn_begin"
            | "collab_agent_spawn_end"
            | "collab_agent_interaction_begin"
            | "collab_agent_interaction_end"
            | "collab_waiting_begin"
            | "collab_waiting_end"
            | "collab_close_begin"
            | "collab_close_end"
            | "collab_resume_begin"
            | "collab_resume_end"
            | "sub_agent_activity"
    )
}
