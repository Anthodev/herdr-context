use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::hash::Hash;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::host::{HostAgentSession, HostSessionReference};
use crate::project::{ProjectIdentity, path_is_within};

use super::{
    Conversation, ConversationProvenance, ConversationState, ProvenanceKind, ResumeCapability,
    SessionReference, SourceId, ToolIdentity,
};

const MAX_NATIVE_ID_BYTES: usize = 256;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct TranscriptIdentity {
    canonical_path: PathBuf,
    length: u64,
    modified_at: SystemTime,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl TranscriptIdentity {
    fn from_path(path: &Path) -> Option<Self> {
        let canonical_path = fs::canonicalize(path).ok()?;
        let metadata = fs::metadata(&canonical_path).ok()?;
        metadata.is_file().then_some(Self {
            canonical_path,
            length: metadata.len(),
            modified_at: metadata.modified().ok()?,
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
        })
    }
}

#[derive(Clone, Copy)]
struct ToolBinding {
    source: &'static str,
    agent: &'static str,
    tool: &'static str,
    path_identity: Option<PathIdentity>,
}

#[derive(Clone, Copy)]
enum PathIdentity {
    ExactUuid { version: u8 },
    SuffixedUuid { separator: u8, version: u8 },
    CodexRollout,
}

const TOOL_BINDINGS: [ToolBinding; 5] = [
    ToolBinding {
        source: "herdr:claude",
        agent: "claude",
        tool: "claude-code",
        path_identity: Some(PathIdentity::ExactUuid { version: 4 }),
    },
    ToolBinding {
        source: "herdr:codex",
        agent: "codex",
        tool: "codex-cli",
        path_identity: Some(PathIdentity::CodexRollout),
    },
    ToolBinding {
        source: "herdr:pi",
        agent: "pi",
        tool: "pi",
        path_identity: Some(PathIdentity::SuffixedUuid {
            separator: b'_',
            version: 7,
        }),
    },
    ToolBinding {
        source: "herdr:omp",
        agent: "omp",
        tool: "omp",
        path_identity: Some(PathIdentity::SuffixedUuid {
            separator: b'_',
            version: 7,
        }),
    },
    ToolBinding {
        source: "herdr:opencode",
        agent: "opencode",
        tool: "opencode",
        path_identity: None,
    },
];

#[derive(Clone)]
struct NormalizedLiveSession {
    tool: ToolIdentity,
    stable_reference: SessionReference,
    native_identity: Option<SessionReference>,
    transcript_identity: Option<TranscriptIdentity>,
    documented_identity: Option<SessionReference>,
    provenance: ConversationProvenance,
    title: Option<String>,
    pane_id: String,
}

pub(crate) struct FilesystemConversationSnapshot {
    conversations: Vec<Conversation>,
    native: HashMap<SessionReference, Option<usize>>,
    transcript: HashMap<TranscriptIdentity, Option<usize>>,
    documented: HashMap<SessionReference, Option<usize>>,
}

impl FilesystemConversationSnapshot {
    #[must_use]
    pub(crate) fn conversations(&self) -> &[Conversation] {
        &self.conversations
    }
}

pub(crate) fn merge_filesystem_snapshots(
    previous: &FilesystemConversationSnapshot,
    fresh: FilesystemConversationSnapshot,
) -> FilesystemConversationSnapshot {
    let mut merged = previous.conversations.clone();
    let mut indexes = merged
        .iter()
        .enumerate()
        .map(|(index, conversation)| (conversation.session_reference().clone(), index))
        .collect::<HashMap<_, _>>();
    for conversation in fresh.conversations {
        match indexes.get(conversation.session_reference()).copied() {
            Some(index) => merged[index] = conversation,
            None => {
                indexes.insert(conversation.session_reference().clone(), merged.len());
                merged.push(conversation);
            }
        }
    }
    sort_recent_first(&mut merged);
    prepare_filesystem_conversations(merged)
}

#[derive(Clone)]
pub(crate) struct LiveConversationSnapshot(Vec<NormalizedLiveSession>);

/// Merges bounded live Herdr metadata into filesystem-owned conversations.
///
/// Matching precedence is native tool/session identity, exact canonical transcript
/// identity with a stable metadata fingerprint, then a verified provider path identity.
#[must_use]
pub fn merge_live_sessions(
    filesystem: Vec<Conversation>,
    live: Vec<HostAgentSession>,
    project: &ProjectIdentity,
    observed_at: SystemTime,
) -> Vec<Conversation> {
    let filesystem = prepare_filesystem_conversations(filesystem);
    let live = prepare_live_conversations(live, project);
    merge_prepared_live_sessions(&filesystem, &live, project, observed_at)
}

pub(crate) fn prepare_filesystem_conversations(
    conversations: Vec<Conversation>,
) -> FilesystemConversationSnapshot {
    let mut native = HashMap::new();
    let mut transcript = HashMap::new();
    let mut documented = HashMap::new();
    for (index, conversation) in conversations.iter().enumerate() {
        insert_unique(&mut native, conversation.session_reference().clone(), index);
        insert_unique(
            &mut documented,
            conversation.session_reference().clone(),
            index,
        );
        for provenance in conversation.provenance() {
            if let Some(identity) = provenance.path().and_then(TranscriptIdentity::from_path) {
                insert_unique(&mut transcript, identity, index);
            }
        }
    }
    FilesystemConversationSnapshot {
        conversations,
        native,
        transcript,
        documented,
    }
}

pub(crate) fn prepare_live_conversations(
    live: Vec<HostAgentSession>,
    project: &ProjectIdentity,
) -> LiveConversationSnapshot {
    let mut normalized = live
        .into_iter()
        .filter_map(|session| normalize_live_session(session, project))
        .collect::<Vec<_>>();
    normalized.sort_unstable_by(|left, right| {
        left.tool
            .as_str()
            .cmp(right.tool.as_str())
            .then_with(|| left.stable_reference.id().cmp(right.stable_reference.id()))
            .then_with(|| left.pane_id.cmp(&right.pane_id))
    });
    LiveConversationSnapshot(normalized)
}

pub(crate) fn merge_prepared_live_sessions(
    filesystem: &FilesystemConversationSnapshot,
    live: &LiveConversationSnapshot,
    project: &ProjectIdentity,
    observed_at: SystemTime,
) -> Vec<Conversation> {
    let mut merged = filesystem.conversations.clone();
    let mut live_only = BTreeMap::<(String, String), Conversation>::new();
    for session in &live.0 {
        let matched = session
            .native_identity
            .as_ref()
            .and_then(|identity| unique_index(&filesystem.native, identity))
            .or_else(|| {
                session
                    .transcript_identity
                    .as_ref()
                    .and_then(|identity| unique_index(&filesystem.transcript, identity))
            })
            .or_else(|| {
                session
                    .documented_identity
                    .as_ref()
                    .and_then(|identity| unique_index(&filesystem.documented, identity))
            });
        if let Some(index) = matched {
            merged[index] = enrich(&merged[index], session, observed_at);
            continue;
        }

        let key = (
            session.tool.as_str().to_owned(),
            session.stable_reference.id().to_owned(),
        );
        live_only
            .entry(key)
            .and_modify(|conversation| {
                *conversation = enrich(conversation, session, observed_at);
            })
            .or_insert_with(|| live_only_conversation(session, project, observed_at));
    }
    merged.extend(live_only.into_values());
    sort_recent_first(&mut merged);
    merged
}

fn normalize_live_session(
    session: HostAgentSession,
    project: &ProjectIdentity,
) -> Option<NormalizedLiveSession> {
    if !session_belongs_to_project(&session, project) {
        return None;
    }
    let binding = TOOL_BINDINGS
        .iter()
        .find(|binding| binding.source == session.source() && binding.agent == session.agent());
    let tool_name = binding.map_or_else(|| session.agent(), |binding| binding.tool);
    if binding.is_none()
        && session
            .source()
            .strip_prefix("herdr:")
            .is_none_or(|source_agent| source_agent != session.agent())
    {
        return None;
    }
    let tool = ToolIdentity::new(tool_name).ok()?;
    let source = SourceId::new(session.source()).ok()?;
    let pane_id = session.pane_id().as_str().to_owned();
    let title = session.title().map(str::to_owned);

    let (stable_reference, native_identity, transcript_identity, documented_identity, path) =
        match session.reference() {
            HostSessionReference::NativeId(id) => {
                if !valid_native_id(id) {
                    return None;
                }
                let identity = SessionReference::new(tool.as_str(), id).ok()?;
                (identity.clone(), Some(identity), None, None, None)
            }
            HostSessionReference::TranscriptPath(path) => {
                if !path.is_absolute() {
                    return None;
                }
                let documented_identity = binding
                    .and_then(|binding| documented_path_id(binding, path))
                    .and_then(|id| SessionReference::new(tool.as_str(), id).ok());
                let fallback = format!("{}:path:{}", session.source(), path.to_string_lossy());
                let stable_reference = documented_identity
                    .clone()
                    .or_else(|| SessionReference::new(tool.as_str(), fallback).ok())?;
                (
                    stable_reference,
                    None,
                    TranscriptIdentity::from_path(path),
                    documented_identity,
                    Some(path.to_path_buf()),
                )
            }
        };
    Some(NormalizedLiveSession {
        tool,
        stable_reference,
        native_identity,
        transcript_identity,
        documented_identity,
        provenance: ConversationProvenance::new(source, ProvenanceKind::HostRuntime, path),
        title,
        pane_id,
    })
}

fn session_belongs_to_project(session: &HostAgentSession, project: &ProjectIdentity) -> bool {
    session
        .foreground_cwd()
        .or_else(|| session.cwd())
        .and_then(|path| fs::canonicalize(path).ok())
        .is_some_and(|path| path_is_within(project.root(), &path))
}

fn documented_path_id(binding: &ToolBinding, path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    if path.extension()?.to_str()? != "jsonl" {
        return None;
    }
    let identity = binding.path_identity?;
    let candidate = match identity {
        PathIdentity::ExactUuid { version } => valid_uuid(stem, version).then_some(stem)?,
        PathIdentity::SuffixedUuid { separator, version } => {
            let candidate = stem.get(stem.len().checked_sub(36)?..)?;
            let separator_index = stem.len().checked_sub(37)?;
            (stem.as_bytes().get(separator_index) == Some(&separator)
                && valid_uuid(candidate, version))
            .then_some(candidate)?
        }
        PathIdentity::CodexRollout => {
            let candidate = stem.get(stem.len().checked_sub(36)?..)?;
            let prefix = stem.get(..stem.len().checked_sub(36)?)?;
            (prefix.starts_with("rollout-") && prefix.ends_with('-') && valid_uuid(candidate, 7))
                .then_some(candidate)?
        }
    };
    Some(candidate.to_owned())
}

fn valid_native_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_NATIVE_ID_BYTES
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn valid_uuid(value: &str, version: u8) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && bytes[8] == b'-'
        && bytes[13] == b'-'
        && bytes[18] == b'-'
        && bytes[23] == b'-'
        && bytes[14] == b'0' + version
        && matches!(bytes[19], b'8'..=b'b')
        && bytes.iter().enumerate().all(|(index, byte)| {
            matches!(index, 8 | 13 | 18 | 23)
                || byte.is_ascii_digit()
                || matches!(byte, b'a'..=b'f')
        })
}

fn enrich(
    filesystem: &Conversation,
    live: &NormalizedLiveSession,
    observed_at: SystemTime,
) -> Conversation {
    let mut provenance = filesystem.provenance().to_vec();
    if !provenance.contains(&live.provenance) {
        provenance.push(live.provenance.clone());
    }
    Conversation::new(
        filesystem.tool().clone(),
        filesystem.session_reference().clone(),
        filesystem.project_identity().clone(),
        filesystem
            .title()
            .map(str::to_owned)
            .or_else(|| live.title.clone()),
        filesystem.created_at(),
        filesystem.archived_at(),
        filesystem.updated_at().max(observed_at),
        ConversationState::Live,
        provenance,
        filesystem.resume_capability().clone(),
    )
    .expect("existing and normalized live conversation metadata is valid")
}

fn live_only_conversation(
    live: &NormalizedLiveSession,
    project: &ProjectIdentity,
    observed_at: SystemTime,
) -> Conversation {
    Conversation::new(
        live.tool.clone(),
        live.stable_reference.clone(),
        project.clone(),
        live.title.clone(),
        None,
        None,
        observed_at,
        ConversationState::Live,
        vec![live.provenance.clone()],
        ResumeCapability::Unsupported,
    )
    .expect("normalized live conversation metadata is valid")
}

fn insert_unique<K: Eq + Hash>(map: &mut HashMap<K, Option<usize>>, key: K, index: usize) {
    map.entry(key)
        .and_modify(|existing| *existing = None)
        .or_insert(Some(index));
}

fn unique_index<K: Eq + Hash>(map: &HashMap<K, Option<usize>>, key: &K) -> Option<usize> {
    map.get(key).copied().flatten()
}

fn sort_recent_first(conversations: &mut [Conversation]) {
    conversations.sort_unstable_by(|left, right| {
        right
            .updated_at()
            .cmp(&left.updated_at())
            .then_with(|| left.tool().as_str().cmp(right.tool().as_str()))
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
    });
}
