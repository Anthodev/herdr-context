use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::de::IgnoredAny;
use serde::{Deserialize, Deserializer};

use super::known_stores::{
    EntryKind, FormatFailure, KnownFormat, KnownJsonlSource, KnownStore, ParsedMetadata,
    canonical_cwd, parse_rfc3339, push_listing_error, push_shape_error, validate_uuid,
};
use super::{
    ConversationCandidate, ConversationSource, ConversationSourceError,
    ConversationSourceErrorKind, DiscoveryBatch, DiscoveryLimit, MetadataBudget,
    ProjectAssociationEvidence, SourceId, SourceWatermark, StorageProbe,
};
use crate::conversations::{Conversation, ResumeCapability};
use crate::project::{CanonicalPath, ProjectIdentity};

const SOURCE_ID: &str = "omp";
const SCHEMA_VERSION: u64 = 3;
const TITLE_SLOT_VERSION: u64 = 1;
const TITLE_SLOT_BYTES: usize = 256;
const MAX_TITLE_BYTES: usize = TITLE_SLOT_BYTES;
const MAX_OMP_RECORDS: u64 = 32_768;

#[derive(Debug)]
pub struct OmpSource {
    inner: KnownJsonlSource<OmpFormat>,
}

impl OmpSource {
    pub fn new(
        project: ProjectIdentity,
        store_root: PathBuf,
    ) -> Result<Self, ConversationSourceError> {
        let format = OmpFormat::new(&store_root)?;
        Ok(Self {
            inner: KnownJsonlSource::new(project, store_root, format)?,
        })
    }

    fn metadata_budget_error(&self, candidate: &ConversationCandidate) -> ConversationSourceError {
        let error = ConversationSourceError::new(
            self.source_id().clone(),
            ConversationSourceErrorKind::InvalidData,
            "OMP metadata exceeds the bounded extraction budget",
        );
        match candidate.source_path() {
            Some(path) => error.with_path(path.to_path_buf()),
            None => error,
        }
    }
}

impl ConversationSource for OmpSource {
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
        let conversation = self.inner.extract_metadata_raw(candidate, budget)?;
        if omp_metadata_bytes(&conversation) > budget.max_bytes() {
            return Err(self.metadata_budget_error(candidate));
        }
        Ok(conversation)
    }

    fn project_evidence_raw(
        &self,
        candidate: &ConversationCandidate,
        project: &ProjectIdentity,
    ) -> Result<Vec<ProjectAssociationEvidence>, ConversationSourceError> {
        self.inner.project_evidence_raw(candidate, project)
    }
}

fn omp_metadata_bytes(conversation: &Conversation) -> usize {
    let mut bytes = conversation.tool().as_str().len();
    bytes = bytes.saturating_add(conversation.session_reference().namespace().len());
    bytes = bytes.saturating_add(conversation.session_reference().id().len());
    bytes = bytes.saturating_add(
        conversation
            .project_identity()
            .root()
            .as_os_str()
            .as_encoded_bytes()
            .len(),
    );
    bytes = bytes.saturating_add(conversation.title().map_or(0, str::len));
    bytes = bytes.saturating_add(2 * size_of::<std::time::SystemTime>());
    for provenance in conversation.provenance() {
        bytes = bytes.saturating_add(provenance.source_id().as_str().len());
        bytes = bytes.saturating_add(
            provenance
                .path()
                .map_or(0, |path| path.as_os_str().as_encoded_bytes().len()),
        );
    }
    if let ResumeCapability::Supported(reference) = conversation.resume_capability() {
        bytes = bytes.saturating_add(reference.as_str().len());
    }
    bytes
}

#[derive(Debug)]
struct OmpFormat {
    home: CanonicalPath,
    temp: PathBuf,
}

impl OmpFormat {
    fn new(store_root: &Path) -> Result<Self, ConversationSourceError> {
        let home = documented_omp_home(store_root).ok_or_else(|| {
            source_error(
                ConversationSourceErrorKind::InvalidData,
                "OMP store root must use the documented .omp/agent/sessions layout",
                store_root,
            )
        })?;
        let home = CanonicalPath::new(home.to_path_buf()).map_err(|_| {
            source_error(
                ConversationSourceErrorKind::InvalidData,
                "OMP home directory cannot be canonicalized",
                home,
            )
        })?;
        let temp_path = std::env::temp_dir();
        let temp = CanonicalPath::new(temp_path.clone())
            .map_or(temp_path, |path| path.as_path().to_path_buf());
        Ok(Self { home, temp })
    }
}

impl KnownFormat for OmpFormat {
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
        let directory = PathBuf::from(omp_project_directory(
            project.root(),
            self.home.as_path(),
            self.temp.as_path(),
        ));
        let entries = match store.list_directory(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(error) => {
                push_listing_error(
                    errors,
                    SOURCE_ID,
                    store.absolute(&directory),
                    "OMP project store cannot be listed",
                    &error,
                );
                return Vec::new();
            }
        };
        let mut files = Vec::new();
        for (name, kind) in entries {
            if cancelled.load(Ordering::Relaxed) {
                break;
            }
            let relative = directory.join(&name);
            match kind {
                EntryKind::File
                    if Path::new(&name)
                        .extension()
                        .is_some_and(|value| value == "jsonl") =>
                {
                    files.push(relative);
                }
                EntryKind::Directory => {
                    // OMP stores child-run artifacts beside the parent JSONL.
                    // They are intentionally opaque and never traversed.
                }
                EntryKind::File | EntryKind::Symlink | EntryKind::Other => push_shape_error(
                    errors,
                    SOURCE_ID,
                    store.absolute(&relative),
                    "OMP store entry is outside the verified flat JSONL layout",
                ),
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
            title,
            cwd,
            created_at,
            mut updated_at,
            mut chain_updated_at,
            mut previous_id,
            append_start,
            mut record_count,
        ) = if let Some(previous) = previous {
            (
                previous.session_id.clone(),
                previous.title.clone(),
                previous.cwd.clone(),
                previous.created_at,
                previous.updated_at,
                previous.chain_updated_at,
                previous.chain_tail.clone(),
                0,
                previous.record_count,
            )
        } else {
            if records.len() < 2 {
                return Err(FormatFailure::unsupported(
                    "OMP JSONL is missing the fixed-width title slot or session header",
                ));
            }
            let slot = parse_title_slot(records[0])?;
            let header: OmpHeader = serde_json::from_slice(records[1]).map_err(|_| {
                FormatFailure::unsupported("OMP header does not match the verified JSON shape")
            })?;
            if header.kind != "session" || header.version != SCHEMA_VERSION {
                return Err(FormatFailure::unsupported(
                    "OMP JSONL does not start with the verified schema-v3 session header",
                ));
            }
            validate_uuid(&header.id, 7)?;
            let cwd = canonical_cwd(&header.cwd, project)?;
            let created_at = parse_rfc3339(&header.timestamp)?;
            let slot_updated_at = parse_rfc3339(&slot.updated_at)?;
            if slot_updated_at < created_at {
                return Err(FormatFailure::unsupported(
                    "OMP title slot timestamp predates the session header",
                ));
            }
            validate_omp_path(
                relative,
                &header.id,
                &header.timestamp,
                project,
                self.home.as_path(),
                self.temp.as_path(),
            )?;
            let title = (!slot.title.trim().is_empty()).then_some(slot.title);
            (
                header.id,
                title,
                cwd,
                created_at,
                slot_updated_at,
                created_at,
                None,
                2,
                2,
            )
        };

        for bytes in &records[append_start..] {
            if cancelled.load(Ordering::Relaxed) {
                return Err(FormatFailure::cancelled());
            }
            let record: OmpRecord = serde_json::from_slice(bytes).map_err(|_| {
                FormatFailure::unsupported("OMP append record has an unverified JSON shape")
            })?;
            if !record.has_valid_payload() {
                return Err(FormatFailure::unsupported(
                    "OMP append record type is outside the verified current session set",
                ));
            }
            validate_entry_id(&record.id)?;
            if let Some(parent_id) = record.parent_id.as_deref() {
                if parent_id == record.id {
                    return Err(FormatFailure::unsupported(
                        "OMP entry cannot reference itself as its parent",
                    ));
                }
                validate_entry_id(parent_id)?;
            }
            let timestamp = parse_rfc3339(&record.timestamp)?;
            if timestamp < chain_updated_at {
                return Err(FormatFailure::unsupported(
                    "OMP append timestamps are not monotonic",
                ));
            }
            chain_updated_at = timestamp;
            updated_at = updated_at.max(timestamp);
            previous_id = Some(record.id);
            record_count = record_count.saturating_add(1);
            if record_count > MAX_OMP_RECORDS {
                return Err(FormatFailure::unsupported(
                    "OMP JSONL exceeds the verified total record limit",
                ));
            }
        }
        Ok(ParsedMetadata {
            session_id,
            title,
            created_at,
            updated_at,
            chain_updated_at,
            cwd,
            chain_tail: previous_id,
            record_count,
        })
    }
}

fn documented_omp_home(store_root: &Path) -> Option<&Path> {
    if store_root.file_name()? != OsStr::new("sessions") {
        return None;
    }
    let agent = store_root.parent()?;
    if agent.file_name()? != OsStr::new("agent") {
        return None;
    }
    let omp = agent.parent()?;
    if omp.file_name()? != OsStr::new(".omp") {
        return None;
    }
    omp.parent()
}

fn source_error(
    kind: ConversationSourceErrorKind,
    message: &'static str,
    path: &Path,
) -> ConversationSourceError {
    ConversationSourceError::new(
        SourceId::new(SOURCE_ID).expect("static OMP source ID"),
        kind,
        message,
    )
    .with_path(path.to_path_buf())
}

fn validate_omp_path(
    relative: &Path,
    id: &str,
    timestamp: &str,
    project: &ProjectIdentity,
    home: &Path,
    temp: &Path,
) -> Result<(), FormatFailure> {
    let expected_directory = omp_project_directory(project.root(), home, temp);
    if relative.parent().and_then(Path::file_name) != Some(expected_directory.as_os_str()) {
        return Err(FormatFailure::project_mismatch(
            "OMP encoded project directory conflicts with canonical cwd",
        ));
    }
    let name = relative
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| FormatFailure::unsupported("OMP session filename is invalid"))?;
    let expected_name = format!("{}_{id}.jsonl", timestamp.replace([':', '.'], "-"));
    if name != expected_name {
        return Err(FormatFailure::unsupported(
            "OMP filename hints conflict with native session metadata",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn omp_project_directory(path: &Path, home: &Path, temp: &Path) -> OsString {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    fn relative(prefix: &[u8], path: &Path) -> OsString {
        let bytes = path.as_os_str().as_bytes();
        let mut encoded = Vec::with_capacity(prefix.len().saturating_add(bytes.len() + 1));
        encoded.extend_from_slice(prefix);
        if !bytes.is_empty() && !prefix.ends_with(b"-") {
            encoded.push(b'-');
        }
        encoded.extend(bytes.iter().map(|byte| match byte {
            b'/' | b'\\' | b':' => b'-',
            byte => *byte,
        }));
        OsString::from_vec(encoded)
    }

    if let Ok(path) = path.strip_prefix(home) {
        return relative(b"-", path);
    }
    if let Ok(path) = path.strip_prefix(temp) {
        return relative(b"-tmp", path);
    }
    let bytes = path.as_os_str().as_bytes();
    let bytes = bytes.strip_prefix(b"/").unwrap_or(bytes);
    let mut encoded = Vec::with_capacity(bytes.len().saturating_add(4));
    encoded.extend_from_slice(b"--");
    encoded.extend(bytes.iter().map(|byte| match byte {
        b'/' | b'\\' | b':' => b'-',
        byte => *byte,
    }));
    encoded.extend_from_slice(b"--");
    OsString::from_vec(encoded)
}

#[cfg(not(unix))]
fn omp_project_directory(path: &Path, home: &Path, temp: &Path) -> OsString {
    fn relative(prefix: &str, path: &Path) -> OsString {
        let encoded = path.to_string_lossy().replace(['/', '\\', ':'], "-");
        if encoded.is_empty() || prefix.ends_with('-') {
            OsString::from(format!("{prefix}{encoded}"))
        } else {
            OsString::from(format!("{prefix}-{encoded}"))
        }
    }

    if let Ok(path) = path.strip_prefix(home) {
        return relative("-", path);
    }
    if let Ok(path) = path.strip_prefix(temp) {
        return relative("-tmp", path);
    }
    let encoded = path.to_string_lossy().replace(['/', '\\', ':'], "-");
    OsString::from(format!("--{}--", encoded.trim_start_matches('-')))
}

fn parse_title_slot(bytes: &[u8]) -> Result<OmpTitleSlot, FormatFailure> {
    if bytes.len().saturating_add(1) != TITLE_SLOT_BYTES {
        return Err(FormatFailure::unsupported(
            "OMP title slot is not exactly 256 UTF-8 bytes",
        ));
    }
    let slot: OmpTitleSlot = serde_json::from_slice(bytes).map_err(|_| {
        FormatFailure::unsupported("OMP title slot does not match the verified JSON shape")
    })?;
    if slot.kind != "title"
        || slot.version != TITLE_SLOT_VERSION
        || slot.title.len() > MAX_TITLE_BYTES
        || !slot.pad.bytes().all(|byte| byte == b' ')
    {
        return Err(FormatFailure::unsupported(
            "OMP title slot is outside the verified fixed-width layout",
        ));
    }
    Ok(slot)
}

fn validate_entry_id(value: &str) -> Result<(), FormatFailure> {
    if value.len() != 8
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(FormatFailure::unsupported(
            "OMP native entry identifier is outside the verified lowercase-hex shape",
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum OmpTitleSource {
    Auto,
    User,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OmpTitleSlot {
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "v")]
    version: u64,
    title: String,
    #[serde(rename = "source")]
    _source: Option<OmpTitleSource>,
    #[serde(rename = "updatedAt")]
    updated_at: String,
    pad: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OmpHeader {
    #[serde(rename = "type")]
    kind: String,
    version: u64,
    id: String,
    timestamp: String,
    cwd: String,
    #[serde(rename = "title")]
    _title: Option<String>,
    #[serde(rename = "titleSource")]
    _title_source: Option<OmpTitleSource>,
    #[serde(rename = "additionalDirectories")]
    _additional_directories: Option<Vec<String>>,
    #[serde(rename = "previousSessionFiles")]
    _previous_session_files: Option<Vec<String>>,
    #[serde(rename = "providerPromptCacheKey")]
    _provider_prompt_cache_key: Option<String>,
    #[serde(rename = "parentSession")]
    _parent_session: Option<String>,
}

#[derive(Default)]
enum Presence {
    #[default]
    Missing,
    Present,
}

impl Presence {
    const fn is_present(&self) -> bool {
        matches!(self, Self::Present)
    }
}

impl<'de> Deserialize<'de> for Presence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        IgnoredAny::deserialize(deserializer)?;
        Ok(Self::Present)
    }
}

#[derive(Deserialize)]
struct OmpMessage {
    role: String,
    timestamp: i64,
    #[serde(default)]
    content: Presence,
    #[serde(default, rename = "toolCallId")]
    tool_call_id: Presence,
    #[serde(default, rename = "toolName")]
    tool_name: Presence,
    #[serde(default, rename = "isError")]
    is_error: Presence,
    #[serde(default)]
    command: Presence,
    #[serde(default)]
    output: Presence,
    #[serde(default, rename = "exitCode")]
    _exit_code: Presence,
    #[serde(default)]
    cancelled: Presence,
    #[serde(default)]
    truncated: Presence,
    #[serde(default)]
    code: Presence,
    #[serde(default, rename = "customType")]
    custom_type: Presence,
    #[serde(default)]
    display: Presence,
    #[serde(default)]
    summary: Presence,
    #[serde(default, rename = "fromId")]
    from_id: Presence,
    #[serde(default, rename = "tokensBefore")]
    tokens_before: Presence,
    #[serde(default)]
    files: Presence,
}

impl OmpMessage {
    fn has_valid_shape(&self) -> bool {
        if self.timestamp <= 0 {
            return false;
        }
        match self.role.as_str() {
            "user" | "developer" | "assistant" => self.content.is_present(),
            "toolResult" => {
                self.content.is_present()
                    && self.tool_call_id.is_present()
                    && self.tool_name.is_present()
                    && self.is_error.is_present()
            }
            "bashExecution" => {
                self.command.is_present()
                    && self.output.is_present()
                    && self.cancelled.is_present()
                    && self.truncated.is_present()
            }
            "pythonExecution" => {
                self.code.is_present()
                    && self.output.is_present()
                    && self.cancelled.is_present()
                    && self.truncated.is_present()
            }
            "custom" | "hookMessage" => {
                self.custom_type.is_present()
                    && self.content.is_present()
                    && self.display.is_present()
            }
            "branchSummary" => self.summary.is_present() && self.from_id.is_present(),
            "compactionSummary" => self.summary.is_present() && self.tokens_before.is_present(),
            "fileMention" => self.files.is_present(),
            _ => false,
        }
    }
}

mod payload {
    pub const MESSAGE: u64 = 1 << 0;
    pub const THINKING_LEVEL: u64 = 1 << 1;
    pub const CONFIGURED: u64 = 1 << 2;
    pub const MODEL: u64 = 1 << 3;
    pub const ROLE: u64 = 1 << 4;
    pub const RESOLVED_FALLBACK: u64 = 1 << 5;
    pub const SERVICE_TIER: u64 = 1 << 6;
    pub const SUMMARY: u64 = 1 << 7;
    pub const SHORT_SUMMARY: u64 = 1 << 8;
    pub const FIRST_KEPT: u64 = 1 << 9;
    pub const TOKENS_BEFORE: u64 = 1 << 10;
    pub const DETAILS: u64 = 1 << 11;
    pub const PRESERVE_DATA: u64 = 1 << 12;
    pub const FROM_EXTENSION: u64 = 1 << 13;
    pub const WARNING: u64 = 1 << 14;
    pub const FROM_ID: u64 = 1 << 15;
    pub const CUSTOM_TYPE: u64 = 1 << 16;
    pub const DATA: u64 = 1 << 17;
    pub const CONTENT: u64 = 1 << 18;
    pub const DISPLAY: u64 = 1 << 19;
    pub const ATTRIBUTION: u64 = 1 << 20;
    pub const TARGET_ID: u64 = 1 << 21;
    pub const LABEL: u64 = 1 << 22;
    pub const TITLE: u64 = 1 << 23;
    pub const PREVIOUS_TITLE: u64 = 1 << 24;
    pub const SOURCE: u64 = 1 << 25;
    pub const TRIGGER: u64 = 1 << 26;
    pub const INJECTED_RULES: u64 = 1 << 27;
    pub const PROVIDER: u64 = 1 << 28;
    pub const HASH: u64 = 1 << 29;
    pub const SYSTEM_PROMPT: u64 = 1 << 30;
    pub const TASK: u64 = 1 << 31;
    pub const TOOLS: u64 = 1 << 32;
    pub const AGENT: u64 = 1 << 33;
    pub const MODEL_ROLE: u64 = 1 << 34;
    pub const RESOLVED_MODEL: u64 = 1 << 35;
    pub const READ_ONLY: u64 = 1 << 36;
    pub const OUTPUT_SCHEMA: u64 = 1 << 37;
    pub const OUTPUT_SCHEMA_MODE: u64 = 1 << 38;
    pub const RESTRICT_TOOL_NAMES: u64 = 1 << 39;
    pub const SPAWNS: u64 = 1 << 40;
    pub const READ_SUMMARIZE: u64 = 1 << 41;
    pub const ADVISOR: u64 = 1 << 42;
    pub const MODE: u64 = 1 << 43;
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OmpRecord {
    #[serde(rename = "type")]
    kind: String,
    id: String,
    #[serde(rename = "parentId")]
    parent_id: Option<String>,
    timestamp: String,
    message: Option<OmpMessage>,
    #[serde(default, rename = "thinkingLevel")]
    thinking_level: Presence,
    #[serde(default)]
    configured: Presence,
    model: Option<String>,
    role: Option<String>,
    #[serde(default, rename = "resolvedModelIsFallback")]
    resolved_fallback: Presence,
    #[serde(default, rename = "serviceTier")]
    service_tier: Presence,
    #[serde(default)]
    summary: Presence,
    #[serde(default, rename = "shortSummary")]
    short_summary: Presence,
    #[serde(default, rename = "firstKeptEntryId")]
    first_kept: Presence,
    #[serde(default, rename = "tokensBefore")]
    tokens_before: Presence,
    #[serde(default)]
    details: Presence,
    #[serde(default, rename = "preserveData")]
    preserve_data: Presence,
    #[serde(default, rename = "fromExtension")]
    from_extension: Presence,
    #[serde(default)]
    warning: Presence,
    #[serde(default, rename = "fromId")]
    from_id: Presence,
    #[serde(rename = "customType")]
    custom_type: Option<String>,
    #[serde(default)]
    data: Presence,
    #[serde(default)]
    content: Presence,
    #[serde(default)]
    display: Presence,
    #[serde(default)]
    attribution: Presence,
    #[serde(rename = "targetId")]
    target_id: Option<String>,
    #[serde(default)]
    label: Presence,
    title: Option<String>,
    #[serde(rename = "previousTitle")]
    previous_title: Option<String>,
    source: Option<OmpTitleSource>,
    trigger: Option<String>,
    #[serde(default, rename = "injectedRules")]
    injected_rules: Presence,
    provider: Option<String>,
    hash: Option<String>,
    #[serde(default, rename = "systemPrompt")]
    system_prompt: Presence,
    #[serde(default)]
    task: Presence,
    #[serde(default)]
    tools: Presence,
    #[serde(default)]
    agent: Presence,
    #[serde(default, rename = "modelRole")]
    model_role: Presence,
    #[serde(default, rename = "resolvedModel")]
    resolved_model: Presence,
    #[serde(default, rename = "readOnly")]
    read_only: Presence,
    #[serde(default, rename = "outputSchema")]
    output_schema: Presence,
    #[serde(default, rename = "outputSchemaMode")]
    output_schema_mode: Presence,
    #[serde(default, rename = "restrictToolNames")]
    restrict_tool_names: Presence,
    #[serde(default)]
    spawns: Presence,
    #[serde(default, rename = "readSummarize")]
    read_summarize: Presence,
    #[serde(default)]
    advisor: Presence,
    mode: Option<String>,
}

impl OmpRecord {
    fn payload_bits(&self) -> u64 {
        use payload::*;

        let bit = |present: bool, value: u64| u64::from(present) * value;
        bit(self.message.is_some(), MESSAGE)
            | bit(self.thinking_level.is_present(), THINKING_LEVEL)
            | bit(self.configured.is_present(), CONFIGURED)
            | bit(self.model.is_some(), MODEL)
            | bit(self.role.is_some(), ROLE)
            | bit(self.resolved_fallback.is_present(), RESOLVED_FALLBACK)
            | bit(self.service_tier.is_present(), SERVICE_TIER)
            | bit(self.summary.is_present(), SUMMARY)
            | bit(self.short_summary.is_present(), SHORT_SUMMARY)
            | bit(self.first_kept.is_present(), FIRST_KEPT)
            | bit(self.tokens_before.is_present(), TOKENS_BEFORE)
            | bit(self.details.is_present(), DETAILS)
            | bit(self.preserve_data.is_present(), PRESERVE_DATA)
            | bit(self.from_extension.is_present(), FROM_EXTENSION)
            | bit(self.warning.is_present(), WARNING)
            | bit(self.from_id.is_present(), FROM_ID)
            | bit(self.custom_type.is_some(), CUSTOM_TYPE)
            | bit(self.data.is_present(), DATA)
            | bit(self.content.is_present(), CONTENT)
            | bit(self.display.is_present(), DISPLAY)
            | bit(self.attribution.is_present(), ATTRIBUTION)
            | bit(self.target_id.is_some(), TARGET_ID)
            | bit(self.label.is_present(), LABEL)
            | bit(self.title.is_some(), TITLE)
            | bit(self.previous_title.is_some(), PREVIOUS_TITLE)
            | bit(self.source.is_some(), SOURCE)
            | bit(self.trigger.is_some(), TRIGGER)
            | bit(self.injected_rules.is_present(), INJECTED_RULES)
            | bit(self.provider.is_some(), PROVIDER)
            | bit(self.hash.is_some(), HASH)
            | bit(self.system_prompt.is_present(), SYSTEM_PROMPT)
            | bit(self.task.is_present(), TASK)
            | bit(self.tools.is_present(), TOOLS)
            | bit(self.agent.is_present(), AGENT)
            | bit(self.model_role.is_present(), MODEL_ROLE)
            | bit(self.resolved_model.is_present(), RESOLVED_MODEL)
            | bit(self.read_only.is_present(), READ_ONLY)
            | bit(self.output_schema.is_present(), OUTPUT_SCHEMA)
            | bit(self.output_schema_mode.is_present(), OUTPUT_SCHEMA_MODE)
            | bit(self.restrict_tool_names.is_present(), RESTRICT_TOOL_NAMES)
            | bit(self.spawns.is_present(), SPAWNS)
            | bit(self.read_summarize.is_present(), READ_SUMMARIZE)
            | bit(self.advisor.is_present(), ADVISOR)
            | bit(self.mode.is_some(), MODE)
    }

    fn has_valid_payload(&self) -> bool {
        use payload::*;

        let bits = self.payload_bits();
        match self.kind.as_str() {
            "message" => {
                bits == MESSAGE
                    && self
                        .message
                        .as_ref()
                        .is_some_and(OmpMessage::has_valid_shape)
            }
            "thinking_level_change" => {
                bits & THINKING_LEVEL != 0 && bits & !(THINKING_LEVEL | CONFIGURED) == 0
            }
            "model_change" => bits & MODEL != 0 && bits & !(MODEL | ROLE | RESOLVED_FALLBACK) == 0,
            "service_tier_change" => bits == SERVICE_TIER,
            "compaction" => {
                let required = SUMMARY | FIRST_KEPT | TOKENS_BEFORE;
                let allowed =
                    required | SHORT_SUMMARY | DETAILS | PRESERVE_DATA | FROM_EXTENSION | WARNING;
                bits & required == required && bits & !allowed == 0
            }
            "branch_summary" => {
                let required = FROM_ID | SUMMARY;
                bits & required == required && bits & !(required | DETAILS | FROM_EXTENSION) == 0
            }
            "reset_boundary" => bits == 0,
            "custom" => bits & CUSTOM_TYPE != 0 && bits & !(CUSTOM_TYPE | DATA) == 0,
            "custom_message" => {
                let required = CUSTOM_TYPE | CONTENT | DISPLAY;
                bits & required == required && bits & !(required | DETAILS | ATTRIBUTION) == 0
            }
            "label" => bits & TARGET_ID != 0 && bits & !(TARGET_ID | LABEL) == 0,
            "title_change" => {
                let required = TITLE | SOURCE;
                bits & required == required && bits & !(required | PREVIOUS_TITLE | TRIGGER) == 0
            }
            "ttsr_injection" => bits == INJECTED_RULES,
            "credential_pin" => bits == PROVIDER | HASH,
            "session_init" => {
                let required = SYSTEM_PROMPT | TASK | TOOLS;
                let allowed = required
                    | AGENT
                    | MODEL_ROLE
                    | RESOLVED_MODEL
                    | READ_ONLY
                    | OUTPUT_SCHEMA
                    | OUTPUT_SCHEMA_MODE
                    | RESTRICT_TOOL_NAMES
                    | SPAWNS
                    | READ_SUMMARIZE
                    | ADVISOR;
                bits & required == required && bits & !allowed == 0
            }
            "mode_change" => bits & MODE != 0 && bits & !(MODE | DATA) == 0,
            _ => false,
        }
    }
}
