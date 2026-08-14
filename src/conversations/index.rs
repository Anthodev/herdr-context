use std::collections::{BTreeMap, HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::conversations::discovery::discover_conversations_cancellable;
use crate::conversations::sources::{
    ConversationSourceError, ConversationSourceErrorKind, DiscoveryLimit, MetadataBudget, SourceId,
    SourceRegistry, SourceWatermark,
};
use crate::conversations::{
    Conversation, ConversationProvenance, ConversationState, ProvenanceKind, ResumeCapability,
    ResumeReference, SessionReference, ToolIdentity,
};
use crate::project::ProjectIdentity;

const SCHEMA_VERSION: u32 = 3;
const MAX_CACHE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_INDEX_ENTRIES: usize = 4_096;
const MAX_WATERMARK_BYTES: usize = 4 * 1024 * 1024;
const MAX_TOTAL_WATERMARK_BYTES: usize = 8 * 1024 * 1024;
const MAX_SOURCE_WATERMARKS: usize = 32;
const MAX_TITLE_BYTES: usize = 256;
const MAX_SESSION_ID_BYTES: usize = 256;
const MAX_PATH_BYTES: usize = 4_096;
const MAX_CACHE_DIRECTORY_ENTRIES: usize = 64;
static CACHE_NONCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IndexStatus {
    Loaded,
    RebuiltMissing,
    RebuiltCorrupt,
    RebuiltIncompatible,
}

#[derive(Clone, Debug)]
pub struct ConversationIndex {
    project: ProjectIdentity,
    project_dir: PathBuf,
    generation: u64,
    watermarks: HashMap<SourceId, SourceWatermark>,
    entries: BTreeMap<String, CachedConversation>,
    status: IndexStatus,
    scan_has_more: bool,
    max_entries: NonZeroUsize,
}

impl ConversationIndex {
    pub fn open(
        state_dir: impl AsRef<Path>,
        project: ProjectIdentity,
    ) -> Result<Self, ConversationIndexError> {
        Self::open_with_max_entries(
            state_dir,
            project,
            NonZeroUsize::new(MAX_INDEX_ENTRIES).expect("non-zero index limit"),
        )
    }

    pub fn open_with_max_entries(
        state_dir: impl AsRef<Path>,
        project: ProjectIdentity,
        max_entries: NonZeroUsize,
    ) -> Result<Self, ConversationIndexError> {
        if !cfg!(unix) {
            return Err(ConversationIndexError::PrivatePermissionsUnsupported);
        }
        let max_entries = NonZeroUsize::new(max_entries.get().min(MAX_INDEX_ENTRIES))
            .expect("bounded index limit is non-zero");
        let state_dir = state_dir.as_ref();
        ensure_private_directory(state_dir)?;
        let conversations_dir = state_dir.join("conversations");
        ensure_private_directory(&conversations_dir)?;
        let project_dir = conversations_dir.join(project_cache_key(project.root()));
        ensure_private_directory(&project_dir)?;

        let Some((cache_path, cached_generation)) = latest_cache_file(&project_dir)? else {
            return Ok(Self::empty(
                project,
                project_dir,
                0,
                IndexStatus::RebuiltMissing,
                max_entries,
            ));
        };
        let bytes = read_private_cache(&cache_path)?;
        let disk = match serde_json::from_slice::<DiskIndex>(&bytes) {
            Ok(disk) => disk,
            Err(_) => {
                return Ok(Self::empty(
                    project,
                    project_dir,
                    cached_generation,
                    IndexStatus::RebuiltCorrupt,
                    max_entries,
                ));
            }
        };
        if disk.generation != cached_generation {
            return Ok(Self::empty(
                project,
                project_dir,
                cached_generation,
                IndexStatus::RebuiltCorrupt,
                max_entries,
            ));
        }
        match Self::from_disk(project.clone(), project_dir.clone(), disk, max_entries) {
            Ok(index) => Ok(index),
            Err(LoadFailure::Corrupt) => Ok(Self::empty(
                project,
                project_dir,
                cached_generation,
                IndexStatus::RebuiltCorrupt,
                max_entries,
            )),
            Err(LoadFailure::Incompatible) => Ok(Self::empty(
                project,
                project_dir,
                cached_generation,
                IndexStatus::RebuiltIncompatible,
                max_entries,
            )),
        }
    }

    fn empty(
        project: ProjectIdentity,
        project_dir: PathBuf,
        generation: u64,
        status: IndexStatus,
        max_entries: NonZeroUsize,
    ) -> Self {
        Self {
            project,
            project_dir,
            generation,
            watermarks: HashMap::new(),
            entries: BTreeMap::new(),
            status,
            max_entries,
            scan_has_more: false,
        }
    }

    fn from_disk(
        project: ProjectIdentity,
        project_dir: PathBuf,
        disk: DiskIndex,
        max_entries: NonZeroUsize,
    ) -> Result<Self, LoadFailure> {
        if !(1..=MAX_INDEX_ENTRIES).contains(&disk.max_entries) {
            return Err(LoadFailure::Corrupt);
        }
        let cache_limit_increased = max_entries.get() > disk.max_entries;
        let disk_root = disk.project_root.to_path()?;
        if disk.schema_version != SCHEMA_VERSION || disk_root != project.root() {
            return Err(LoadFailure::Incompatible);
        }
        if disk.entries.len() > MAX_INDEX_ENTRIES || disk.watermarks.len() > MAX_SOURCE_WATERMARKS {
            return Err(LoadFailure::Corrupt);
        }
        let mut watermarks = HashMap::new();
        for watermark in disk.watermarks {
            validate_source(&watermark.source_id, None)?;
            if watermark.token.is_empty() || watermark.token.len() > MAX_WATERMARK_BYTES {
                return Err(LoadFailure::Corrupt);
            }
            let source_id = SourceId::new(watermark.source_id).map_err(|_| LoadFailure::Corrupt)?;
            let value = SourceWatermark::new(source_id.clone(), watermark.token)
                .map_err(|_| LoadFailure::Corrupt)?;
            if watermarks.insert(source_id, value).is_some() {
                return Err(LoadFailure::Corrupt);
            }
        }
        let mut entries = BTreeMap::new();
        for entry in disk.entries {
            entry.validate()?;
            let key = entry.key();
            if entries.insert(key, entry).is_some() {
                return Err(LoadFailure::Corrupt);
            }
        }
        Ok(Self {
            project,
            project_dir,
            generation: disk.generation,
            watermarks: if cache_limit_increased {
                HashMap::new()
            } else {
                watermarks
            },
            entries,
            status: IndexStatus::Loaded,
            max_entries,
            scan_has_more: false,
        })
    }

    pub fn refresh_page(
        &mut self,
        registry: &SourceRegistry,
        limit: DiscoveryLimit,
        metadata_budget: MetadataBudget,
    ) -> Result<IndexRefresh, ConversationIndexError> {
        self.refresh_page_cancellable(registry, limit, metadata_budget, &AtomicBool::new(false))
    }

    pub fn refresh_page_cancellable(
        &mut self,
        registry: &SourceRegistry,
        limit: DiscoveryLimit,
        metadata_budget: MetadataBudget,
        cancelled: &AtomicBool,
    ) -> Result<IndexRefresh, ConversationIndexError> {
        let discovery = discover_conversations_cancellable(
            registry,
            &self.project,
            &self.watermarks,
            limit,
            metadata_budget,
            cancelled,
        );
        let cancelled_result = || {
            Ok(IndexRefresh {
                added_or_updated: 0,
                has_more: false,
                cancelled: true,
                errors: discovery.errors().to_vec(),
            })
        };
        if cancelled.load(Ordering::Relaxed) {
            return cancelled_result();
        }
        let mut refresh_errors = discovery.errors().to_vec();

        let mut staged = self.clone();
        let desired_source_ids = registry
            .desired_source_ids()
            .map(SourceId::as_str)
            .collect::<HashSet<_>>();
        staged
            .entries
            .retain(|_, entry| desired_source_ids.contains(entry.source_id.as_str()));
        for source_id in discovery.purged_sources() {
            if cancelled.load(Ordering::Relaxed) {
                return cancelled_result();
            }
            staged
                .entries
                .retain(|_, entry| entry.source_id != source_id.as_str());
        }
        for removal in discovery.removals() {
            if cancelled.load(Ordering::Relaxed) {
                return cancelled_result();
            }
            let key = format!(
                "{}\0{}\0{}",
                removal.source_id().as_str(),
                removal.session_reference().namespace(),
                removal.session_reference().id()
            );
            if staged
                .entries
                .get(&key)
                .is_some_and(|entry| entry.source_id == removal.source_id().as_str())
            {
                staged.entries.remove(&key);
            }
        }
        let mut added_or_updated = 0;
        for conversation in discovery.conversations() {
            if cancelled.load(Ordering::Relaxed) {
                return cancelled_result();
            }
            let cached = CachedConversation::from_conversation(conversation)?;
            let key = cached.key();
            if staged.entries.get(&key) != Some(&cached) {
                staged.entries.insert(key, cached);
                added_or_updated += 1;
            }
        }
        if staged.entries.len() > staged.max_entries.get() {
            staged.trim_oldest()?;
        }
        let mut ordered_watermarks = discovery.watermarks().values().cloned().collect::<Vec<_>>();
        ordered_watermarks.sort_unstable_by(|left, right| {
            left.source_id().as_str().cmp(right.source_id().as_str())
        });
        let mut watermark_bytes = 0_usize;
        let mut bounded_watermarks = HashMap::new();
        let mut dropped_continuation = false;
        for watermark in ordered_watermarks {
            let token_bytes = watermark.token().len();
            let next_bytes = watermark_bytes.saturating_add(token_bytes);
            if token_bytes > MAX_WATERMARK_BYTES || next_bytes > MAX_TOTAL_WATERMARK_BYTES {
                refresh_errors.push(ConversationSourceError::new(
                    watermark.source_id().clone(),
                    ConversationSourceErrorKind::InvalidData,
                    "source watermark exceeds the metadata cache budget",
                ));
                dropped_continuation = true;
                continue;
            }
            watermark_bytes = next_bytes;
            bounded_watermarks.insert(watermark.source_id().clone(), watermark);
        }
        staged.watermarks = bounded_watermarks;
        staged.scan_has_more = discovery.has_more() && !dropped_continuation;
        if cancelled.load(Ordering::Relaxed) {
            return cancelled_result();
        }
        staged.generation = staged.generation.saturating_add(1);
        if !staged.persist_cancellable(cancelled)? {
            return cancelled_result();
        }
        staged.status = IndexStatus::Loaded;
        let has_more = staged.scan_has_more;
        *self = staged;
        Ok(IndexRefresh {
            added_or_updated,
            has_more,
            cancelled: false,
            errors: refresh_errors,
        })
    }

    fn trim_oldest(&mut self) -> Result<(), ConversationIndexError> {
        let mut ordered = self
            .entries
            .iter()
            .map(|(key, entry)| entry.updated_at().map(|updated| (updated, key.clone())))
            .collect::<Result<Vec<_>, _>>()?;
        ordered.sort_unstable_by(|left, right| {
            right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1))
        });
        let keep = ordered
            .into_iter()
            .take(self.max_entries.get())
            .map(|(_, key)| key)
            .collect::<HashSet<_>>();
        self.entries.retain(|key, _| keep.contains(key));
        Ok(())
    }

    fn persist_cancellable(&self, cancelled: &AtomicBool) -> Result<bool, ConversationIndexError> {
        let disk = DiskIndex {
            schema_version: SCHEMA_VERSION,
            project_root: StoredPath::from_path(self.project.root())?,
            generation: self.generation,
            max_entries: self.max_entries.get(),
            watermarks: self
                .watermarks
                .values()
                .map(|watermark| DiskWatermark {
                    source_id: watermark.source_id().as_str().to_owned(),
                    token: watermark.token().to_owned(),
                })
                .collect(),
            entries: self.entries.values().cloned().collect(),
        };
        let bytes = serde_json::to_vec(&disk).map_err(|_| {
            ConversationIndexError::InvalidCache("metadata index cannot be encoded")
        })?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_CACHE_BYTES {
            return Err(ConversationIndexError::InvalidCache(
                "metadata index exceeds the cache byte limit",
            ));
        }
        if cancelled.load(Ordering::Relaxed) {
            return Ok(false);
        }

        let nonce = CACHE_NONCE.fetch_add(1, Ordering::Relaxed);
        let process = std::process::id();
        let temporary = self.project_dir.join(format!(
            ".cache-{:020}-{process:08x}-{nonce:016x}.tmp",
            self.generation
        ));
        let published = self.project_dir.join(format!(
            "cache-{:020}-{process:08x}-{nonce:016x}.json",
            self.generation
        ));
        let mut published_generation = false;
        let result = write_private_file(&temporary, &bytes).and_then(|()| {
            if cancelled.load(Ordering::Relaxed) {
                fs::remove_file(&temporary).map_err(|source| ConversationIndexError::Io {
                    operation: "remove cancelled metadata index",
                    path: temporary.clone(),
                    source,
                })?;
                return Ok(());
            }
            fs::rename(&temporary, &published).map_err(|source| ConversationIndexError::Io {
                operation: "publish metadata index",
                path: published.clone(),
                source,
            })?;
            published_generation = true;
            Ok(())
        });
        if let Err(error) = result {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        if !published_generation {
            return Ok(false);
        }
        sync_directory(&self.project_dir)?;
        publish_current_pointer(&self.project_dir, &published, process, nonce)?;
        remove_older_generations(&self.project_dir, self.generation, &published)?;
        Ok(true)
    }

    pub fn page(&self, offset: usize, limit: usize) -> IndexPage {
        if limit == 0 {
            return IndexPage {
                conversations: Vec::new(),
                has_more: self.scan_has_more || offset < self.entries.len(),
            };
        }
        let mut conversations = self
            .entries
            .values()
            .map(|entry| {
                entry
                    .to_conversation(&self.project)
                    .expect("validated cached metadata remains constructible")
            })
            .collect::<Vec<_>>();
        conversations.sort_unstable_by(|left, right| {
            right
                .updated_at()
                .cmp(&left.updated_at())
                .then_with(|| {
                    left.session_reference()
                        .namespace()
                        .cmp(right.session_reference().namespace())
                })
                .then_with(|| {
                    left.session_reference()
                        .id()
                        .cmp(right.session_reference().id())
                })
                .then_with(|| {
                    left.provenance()[0]
                        .source_id()
                        .as_str()
                        .cmp(right.provenance()[0].source_id().as_str())
                })
        });
        let mut seen = HashSet::new();
        conversations.retain(|conversation| {
            seen.insert((
                conversation.session_reference().namespace().to_owned(),
                conversation.session_reference().id().to_owned(),
            ))
        });
        let total = conversations.len();
        let conversations = conversations
            .into_iter()
            .skip(offset)
            .take(limit)
            .collect::<Vec<_>>();
        IndexPage {
            has_more: self.scan_has_more || offset.saturating_add(conversations.len()) < total,
            conversations,
        }
    }

    #[must_use]
    pub const fn status(&self) -> IndexStatus {
        self.status
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[derive(Clone, Debug)]
pub struct IndexRefresh {
    added_or_updated: usize,
    has_more: bool,
    cancelled: bool,
    errors: Vec<ConversationSourceError>,
}

impl IndexRefresh {
    #[must_use]
    pub const fn added_or_updated(&self) -> usize {
        self.added_or_updated
    }

    #[must_use]
    pub const fn has_more(&self) -> bool {
        self.has_more
    }
    #[must_use]
    pub const fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    #[must_use]
    pub fn errors(&self) -> &[ConversationSourceError] {
        &self.errors
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexPage {
    conversations: Vec<Conversation>,
    has_more: bool,
}

impl IndexPage {
    #[must_use]
    pub fn conversations(&self) -> &[Conversation] {
        &self.conversations
    }

    #[must_use]
    pub const fn has_more(&self) -> bool {
        self.has_more
    }

    pub fn into_conversations(self) -> Vec<Conversation> {
        self.conversations
    }
}

#[derive(Serialize, Deserialize)]
struct DiskIndex {
    schema_version: u32,
    project_root: StoredPath,
    generation: u64,
    #[serde(default = "default_disk_max_entries")]
    max_entries: usize,
    watermarks: Vec<DiskWatermark>,
    entries: Vec<CachedConversation>,
}

const fn default_disk_max_entries() -> usize {
    MAX_INDEX_ENTRIES
}

#[derive(Serialize, Deserialize)]
struct DiskWatermark {
    source_id: String,
    token: String,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct StoredPath {
    encoding: String,
    bytes: Vec<u8>,
}

impl StoredPath {
    fn from_path(path: &Path) -> Result<Self, ConversationIndexError> {
        validate_path(Some(path))?;
        Ok(Self {
            encoding: if cfg!(unix) {
                "unix-bytes".to_owned()
            } else {
                "utf8".to_owned()
            },
            bytes: path.as_os_str().as_encoded_bytes().to_vec(),
        })
    }

    #[cfg(unix)]
    fn to_path(&self) -> Result<PathBuf, LoadFailure> {
        use std::os::unix::ffi::OsStringExt;

        if self.encoding != "unix-bytes"
            || self.bytes.is_empty()
            || self.bytes.len() > MAX_PATH_BYTES
            || self.bytes.contains(&0)
        {
            return Err(LoadFailure::Corrupt);
        }
        Ok(PathBuf::from(std::ffi::OsString::from_vec(
            self.bytes.clone(),
        )))
    }

    #[cfg(not(unix))]
    fn to_path(&self) -> Result<PathBuf, LoadFailure> {
        if self.encoding != "utf8" || self.bytes.is_empty() || self.bytes.len() > MAX_PATH_BYTES {
            return Err(LoadFailure::Corrupt);
        }
        String::from_utf8(self.bytes.clone())
            .map(PathBuf::from)
            .map_err(|_| LoadFailure::Corrupt)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct CachedConversation {
    source_id: String,
    tool: String,
    namespace: String,
    session_id: String,
    #[serde(default)]
    title: Option<String>,
    created_at: Option<TimeParts>,
    #[serde(default)]
    archived_at: Option<TimeParts>,
    updated_at: TimeParts,
    state: String,
    provenance_kind: String,
    source_path: Option<StoredPath>,
    resume_reference: Option<String>,
}

impl CachedConversation {
    fn from_conversation(conversation: &Conversation) -> Result<Self, ConversationIndexError> {
        let [provenance] = conversation.provenance() else {
            return Err(ConversationIndexError::InvalidCache(
                "cache accepts exactly one approved provenance record",
            ));
        };
        let source_id = provenance.source_id().as_str();
        validate_source(source_id, Some(conversation.tool().as_str()))
            .map_err(|_| ConversationIndexError::InvalidCache("unapproved metadata source"))?;
        if conversation.session_reference().namespace() != conversation.tool().as_str() {
            return Err(ConversationIndexError::InvalidCache(
                "session namespace does not match its approved tool",
            ));
        }
        let session_id = conversation.session_reference().id();
        validate_identifier(session_id)?;
        if conversation
            .title()
            .is_some_and(|title| title.len() > MAX_TITLE_BYTES)
        {
            return Err(ConversationIndexError::InvalidCache(
                "metadata title is outside the allowlist",
            ));
        }
        let source_path = provenance.path().map(StoredPath::from_path).transpose()?;
        let state = match conversation.state() {
            ConversationState::Live => "live",
            ConversationState::Archived => "archived",
            ConversationState::Unknown => "unknown",
        };
        let provenance_kind = match provenance.kind() {
            ProvenanceKind::ProjectLocal => "project-local",
            ProvenanceKind::ExternalLocal => "external-local",
            ProvenanceKind::HostRuntime => "host-runtime",
        };
        let resume_reference = match conversation.resume_capability() {
            ResumeCapability::Unsupported => None,
            ResumeCapability::Supported(reference) => {
                validate_identifier(reference.as_str())?;
                Some(reference.as_str().to_owned())
            }
        };
        Ok(Self {
            source_id: source_id.to_owned(),
            tool: conversation.tool().as_str().to_owned(),
            namespace: conversation.session_reference().namespace().to_owned(),
            session_id: session_id.to_owned(),
            title: conversation.title().map(str::to_owned),
            created_at: conversation.created_at().map(TimeParts::from_system_time),
            archived_at: conversation.archived_at().map(TimeParts::from_system_time),
            updated_at: TimeParts::from_system_time(conversation.updated_at()),
            state: state.to_owned(),
            provenance_kind: provenance_kind.to_owned(),
            source_path,
            resume_reference,
        })
    }

    fn validate(&self) -> Result<(), LoadFailure> {
        validate_source(&self.source_id, Some(&self.tool))?;
        if self.namespace != self.tool {
            return Err(LoadFailure::Corrupt);
        }
        validate_identifier_load(&self.session_id)?;
        if self
            .title
            .as_ref()
            .is_some_and(|title| title.is_empty() || title.len() > MAX_TITLE_BYTES)
        {
            return Err(LoadFailure::Corrupt);
        }
        let source_path = self
            .source_path
            .as_ref()
            .map(StoredPath::to_path)
            .transpose()?;
        validate_path_load(source_path.as_deref())?;
        self.updated_at.to_system_time()?;
        if let Some(created) = self.created_at {
            created.to_system_time()?;
        }
        if let Some(archived) = self.archived_at {
            archived.to_system_time()?;
        }
        if !matches!(self.state.as_str(), "live" | "archived" | "unknown")
            || !matches!(
                self.provenance_kind.as_str(),
                "project-local" | "external-local" | "host-runtime"
            )
        {
            return Err(LoadFailure::Corrupt);
        }
        if let Some(reference) = &self.resume_reference {
            validate_identifier_load(reference)?;
        }
        Ok(())
    }

    fn key(&self) -> String {
        format!(
            "{}\0{}\0{}",
            self.source_id, self.namespace, self.session_id
        )
    }

    fn updated_at(&self) -> Result<SystemTime, ConversationIndexError> {
        self.updated_at
            .to_system_time()
            .map_err(|_| ConversationIndexError::InvalidCache("cached timestamp is invalid"))
    }

    fn to_conversation(
        &self,
        project: &ProjectIdentity,
    ) -> Result<Conversation, ConversationIndexError> {
        let source_id = SourceId::new(&self.source_id)
            .map_err(|_| ConversationIndexError::InvalidCache("cached source ID is invalid"))?;
        let tool = ToolIdentity::new(&self.tool)
            .map_err(|_| ConversationIndexError::InvalidCache("cached tool is invalid"))?;
        let session = SessionReference::new(&self.namespace, &self.session_id).map_err(|_| {
            ConversationIndexError::InvalidCache("cached session reference is invalid")
        })?;
        let provenance_kind = match self.provenance_kind.as_str() {
            "project-local" => ProvenanceKind::ProjectLocal,
            "external-local" => ProvenanceKind::ExternalLocal,
            "host-runtime" => ProvenanceKind::HostRuntime,
            _ => {
                return Err(ConversationIndexError::InvalidCache(
                    "cached provenance is invalid",
                ));
            }
        };
        let state = match self.state.as_str() {
            "live" => ConversationState::Live,
            "archived" => ConversationState::Archived,
            "unknown" => ConversationState::Unknown,
            _ => {
                return Err(ConversationIndexError::InvalidCache(
                    "cached state is invalid",
                ));
            }
        };
        let resume = self.resume_reference.as_ref().map_or_else(
            || Ok(ResumeCapability::Unsupported),
            |reference| {
                ResumeReference::new(reference)
                    .map(ResumeCapability::Supported)
                    .map_err(|_| {
                        ConversationIndexError::InvalidCache("cached resume reference is invalid")
                    })
            },
        )?;
        let source_path = self
            .source_path
            .as_ref()
            .map(StoredPath::to_path)
            .transpose()
            .map_err(|_| ConversationIndexError::InvalidCache("cached path is invalid"))?;
        Conversation::new(
            tool,
            session,
            project.clone(),
            self.title.clone(),
            self.created_at
                .map(TimeParts::to_system_time)
                .transpose()
                .map_err(|_| ConversationIndexError::InvalidCache("cached timestamp is invalid"))?,
            self.archived_at
                .map(TimeParts::to_system_time)
                .transpose()
                .map_err(|_| ConversationIndexError::InvalidCache("cached timestamp is invalid"))?,
            self.updated_at()?,
            state,
            vec![ConversationProvenance::new(
                source_id,
                provenance_kind,
                source_path,
            )],
            resume,
        )
        .map_err(|_| ConversationIndexError::InvalidCache("cached conversation is invalid"))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct TimeParts {
    after_epoch: bool,
    seconds: u64,
    nanoseconds: u32,
}

impl TimeParts {
    fn from_system_time(value: SystemTime) -> Self {
        match value.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(duration) => Self {
                after_epoch: true,
                seconds: duration.as_secs(),
                nanoseconds: duration.subsec_nanos(),
            },
            Err(error) => {
                let duration = error.duration();
                Self {
                    after_epoch: false,
                    seconds: duration.as_secs(),
                    nanoseconds: duration.subsec_nanos(),
                }
            }
        }
    }

    fn to_system_time(self) -> Result<SystemTime, LoadFailure> {
        if self.nanoseconds >= 1_000_000_000 {
            return Err(LoadFailure::Corrupt);
        }
        let duration = Duration::new(self.seconds, self.nanoseconds);
        if self.after_epoch {
            SystemTime::UNIX_EPOCH
                .checked_add(duration)
                .ok_or(LoadFailure::Corrupt)
        } else {
            SystemTime::UNIX_EPOCH
                .checked_sub(duration)
                .ok_or(LoadFailure::Corrupt)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LoadFailure {
    Corrupt,
    Incompatible,
}

fn validate_source(source: &str, tool: Option<&str>) -> Result<(), LoadFailure> {
    let expected_tool = if source == "project-local-generic-jsonl" {
        "generic-jsonl"
    } else {
        ["claude-code", "codex-cli", "opencode", "omp", "pi"]
            .into_iter()
            .find(|candidate| {
                source == *candidate
                    || source
                        .strip_prefix(*candidate)
                        .and_then(|suffix| suffix.strip_prefix(":extra:"))
                        .is_some_and(|fingerprint| {
                            fingerprint.len() == 16
                                && fingerprint.bytes().all(|byte| {
                                    byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()
                                })
                        })
            })
            .ok_or(LoadFailure::Incompatible)?
    };
    if tool.is_some_and(|tool| tool != expected_tool) {
        return Err(LoadFailure::Corrupt);
    }
    Ok(())
}

fn validate_identifier(value: &str) -> Result<(), ConversationIndexError> {
    if value.is_empty()
        || value.len() > MAX_SESSION_ID_BYTES
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(ConversationIndexError::InvalidCache(
            "metadata identifier is outside the allowlist",
        ));
    }
    Ok(())
}

fn validate_identifier_load(value: &str) -> Result<(), LoadFailure> {
    validate_identifier(value).map_err(|_| LoadFailure::Corrupt)
}

fn validate_path(value: Option<&Path>) -> Result<(), ConversationIndexError> {
    if value.is_some_and(|path| {
        !path.is_absolute() || path.as_os_str().as_encoded_bytes().len() > MAX_PATH_BYTES
    }) {
        return Err(ConversationIndexError::InvalidCache(
            "metadata path is outside the allowlist",
        ));
    }
    Ok(())
}

fn validate_path_load(value: Option<&Path>) -> Result<(), LoadFailure> {
    validate_path(value).map_err(|_| LoadFailure::Corrupt)
}

fn project_cache_key(root: &Path) -> String {
    const OFFSET: u64 = 14_695_981_039_346_656_037;
    let bytes = root.as_os_str().as_encoded_bytes();
    let first = fnv1a(bytes, OFFSET);
    let second = fnv1a(bytes, OFFSET ^ 0x9e37_79b9_7f4a_7c15);
    format!("project-{first:016x}{second:016x}")
}

fn fnv1a(bytes: &[u8], seed: u64) -> u64 {
    bytes.iter().fold(seed, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(1_099_511_628_211)
    })
}

fn ensure_private_directory(path: &Path) -> Result<(), ConversationIndexError> {
    fs::create_dir_all(path).map_err(|source| ConversationIndexError::Io {
        operation: "create metadata index directory",
        path: path.to_path_buf(),
        source,
    })?;
    let metadata = fs::symlink_metadata(path).map_err(|source| ConversationIndexError::Io {
        operation: "inspect metadata index directory",
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.file_type().is_dir() {
        return Err(ConversationIndexError::UnsafePath(path.to_path_buf()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|source| {
            ConversationIndexError::Io {
                operation: "secure metadata index directory",
                path: path.to_path_buf(),
                source,
            }
        })?;
    }
    Ok(())
}

fn latest_cache_file(project_dir: &Path) -> Result<Option<(PathBuf, u64)>, ConversationIndexError> {
    cleanup_stale_temporaries(project_dir)?;
    let pointer = project_dir.join("current");
    match fs::symlink_metadata(&pointer) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ConversationIndexError::Io {
                operation: "inspect metadata index pointer",
                path: pointer,
                source,
            });
        }
    }
    let bytes = read_private_cache(&pointer)?;
    let Ok(name) = std::str::from_utf8(&bytes) else {
        return Ok(None);
    };
    if Path::new(name).components().count() != 1 {
        return Ok(None);
    }
    let path = project_dir.join(name);
    let Some(generation) = cache_generation(&path) else {
        return Ok(None);
    };
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(Some((path, generation))),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ConversationIndexError::Io {
            operation: "inspect pointed metadata index",
            path,
            source,
        }),
    }
}

fn cleanup_stale_temporaries(project_dir: &Path) -> Result<(), ConversationIndexError> {
    for entry in fs::read_dir(project_dir)
        .map_err(|source| ConversationIndexError::Io {
            operation: "list metadata index directory",
            path: project_dir.to_path_buf(),
            source,
        })?
        .take(MAX_CACHE_DIRECTORY_ENTRIES)
    {
        let entry = entry.map_err(|source| ConversationIndexError::Io {
            operation: "read metadata index entry",
            path: project_dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if is_temporary_cache_file(&path) || is_temporary_pointer_file(&path) {
            fs::remove_file(&path).map_err(|source| ConversationIndexError::Io {
                operation: "remove stale metadata index temporary",
                path,
                source,
            })?;
        }
    }
    Ok(())
}

fn cache_generation(path: &Path) -> Option<u64> {
    let name = path.file_name()?.to_str()?;
    if name.len() != 57
        || !name.starts_with("cache-")
        || !name.ends_with(".json")
        || name.as_bytes().get(26) != Some(&b'-')
        || name.as_bytes().get(35) != Some(&b'-')
        || !name.as_bytes()[27..35].iter().all(u8::is_ascii_hexdigit)
        || !name.as_bytes()[36..52].iter().all(u8::is_ascii_hexdigit)
    {
        return None;
    }
    name[6..26].parse().ok()
}

fn is_temporary_cache_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
        return false;
    };
    name.len() == 57
        && name.starts_with(".cache-")
        && name.ends_with(".tmp")
        && name.as_bytes().get(27) == Some(&b'-')
        && name.as_bytes().get(36) == Some(&b'-')
        && name.as_bytes()[7..27].iter().all(u8::is_ascii_digit)
        && name.as_bytes()[28..36].iter().all(u8::is_ascii_hexdigit)
        && name.as_bytes()[37..53].iter().all(u8::is_ascii_hexdigit)
}

fn is_temporary_pointer_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
        return false;
    };
    name.len() == 38
        && name.starts_with(".current-")
        && name.ends_with(".tmp")
        && name.as_bytes().get(17) == Some(&b'-')
        && name.as_bytes()[9..17].iter().all(u8::is_ascii_hexdigit)
        && name.as_bytes()[18..34].iter().all(u8::is_ascii_hexdigit)
}

fn read_private_cache(path: &Path) -> Result<Vec<u8>, ConversationIndexError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options
        .open(path)
        .map_err(|source| ConversationIndexError::Io {
            operation: "open metadata index",
            path: path.to_path_buf(),
            source,
        })?;
    validate_private_file(&file, path)?;
    let length = file
        .metadata()
        .map_err(|source| ConversationIndexError::Io {
            operation: "inspect metadata index",
            path: path.to_path_buf(),
            source,
        })?
        .len();
    if length > MAX_CACHE_BYTES {
        return Ok(Vec::new());
    }
    let mut bytes = Vec::with_capacity(usize::try_from(length).unwrap_or(0));
    (&mut file)
        .take(length)
        .read_to_end(&mut bytes)
        .map_err(|source| ConversationIndexError::Io {
            operation: "read metadata index",
            path: path.to_path_buf(),
            source,
        })?;
    Ok(bytes)
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), ConversationIndexError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(path)
        .map_err(|source| ConversationIndexError::Io {
            operation: "create metadata index",
            path: path.to_path_buf(),
            source,
        })?;
    validate_private_file(&file, path)?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|source| ConversationIndexError::Io {
            operation: "write metadata index",
            path: path.to_path_buf(),
            source,
        })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|source| ConversationIndexError::Io {
                operation: "secure metadata index",
                path: path.to_path_buf(),
                source,
            })?;
    }
    Ok(())
}

fn validate_private_file(file: &File, path: &Path) -> Result<(), ConversationIndexError> {
    let metadata = file
        .metadata()
        .map_err(|source| ConversationIndexError::Io {
            operation: "inspect metadata index",
            path: path.to_path_buf(),
            source,
        })?;
    if !metadata.file_type().is_file() {
        return Err(ConversationIndexError::UnsafePath(path.to_path_buf()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let parent = path
            .parent()
            .ok_or_else(|| ConversationIndexError::UnsafePath(path.to_path_buf()))?;
        let parent_metadata =
            fs::metadata(parent).map_err(|source| ConversationIndexError::Io {
                operation: "inspect metadata index directory",
                path: parent.to_path_buf(),
                source,
            })?;
        if metadata.uid() != parent_metadata.uid()
            || metadata.nlink() != 1
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(ConversationIndexError::UnsafePath(path.to_path_buf()));
        }
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), ConversationIndexError> {
    #[cfg(unix)]
    {
        File::open(path)
            .and_then(|directory| directory.sync_all())
            .map_err(|source| ConversationIndexError::Io {
                operation: "sync metadata index directory",
                path: path.to_path_buf(),
                source,
            })?;
    }
    Ok(())
}

fn publish_current_pointer(
    project_dir: &Path,
    published: &Path,
    process: u32,
    nonce: u64,
) -> Result<(), ConversationIndexError> {
    let name = published
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or(ConversationIndexError::InvalidCache(
            "published metadata index name is invalid",
        ))?;
    let temporary = project_dir.join(format!(".current-{process:08x}-{nonce:016x}.tmp"));
    let current = project_dir.join("current");
    let result = write_private_file(&temporary, name.as_bytes()).and_then(|()| {
        fs::rename(&temporary, &current).map_err(|source| ConversationIndexError::Io {
            operation: "publish metadata index pointer",
            path: current,
            source,
        })
    });
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    sync_directory(project_dir)
}

fn remove_older_generations(
    project_dir: &Path,
    generation: u64,
    published: &Path,
) -> Result<(), ConversationIndexError> {
    for entry in fs::read_dir(project_dir)
        .map_err(|source| ConversationIndexError::Io {
            operation: "list old metadata indexes",
            path: project_dir.to_path_buf(),
            source,
        })?
        .take(MAX_CACHE_DIRECTORY_ENTRIES)
    {
        let entry = entry.map_err(|source| ConversationIndexError::Io {
            operation: "read old metadata index entry",
            path: project_dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if path == published {
            continue;
        }
        let Some(value) = cache_generation(&path) else {
            continue;
        };
        if value < generation {
            fs::remove_file(&path).map_err(|source| ConversationIndexError::Io {
                operation: "remove old metadata index",
                path,
                source,
            })?;
        }
    }
    Ok(())
}

#[derive(Debug)]
pub enum ConversationIndexError {
    UnsafePath(PathBuf),
    PrivatePermissionsUnsupported,
    InvalidCache(&'static str),
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
}

impl fmt::Display for ConversationIndexError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsafePath(path) => {
                write!(
                    formatter,
                    "metadata index path is not private: {}",
                    path.display()
                )
            }
            Self::PrivatePermissionsUnsupported => formatter
                .write_str("private metadata index permissions are unsupported on this platform"),
            Self::InvalidCache(message) => formatter.write_str(message),
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

impl Error for ConversationIndexError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::UnsafePath(_) | Self::InvalidCache(_) | Self::PrivatePermissionsUnsupported => {
                None
            }
        }
    }
}
