//! Conversation source boundary.
mod claude_code;
mod codex_cli;
mod generic_jsonl;
mod known_stores;
mod omp;
mod opencode;
mod pi;
mod project_local;

pub use claude_code::ClaudeCodeSource;
pub use codex_cli::CodexCliSource;
pub use generic_jsonl::GenericJsonlSource;
pub use known_stores::KnownStoreRoots;
pub use omp::OmpSource;
pub use opencode::OpenCodeSource;
pub use pi::PiSource;
pub use project_local::{ProjectLocalLocation, ProjectLocalLocationError};

use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::SystemTime;

pub use crate::conversations::SourceId;
use crate::conversations::{Conversation, SessionReference};
use crate::project::{CanonicalPath, ProjectIdentity};
impl KnownStoreRoots {
    pub fn sources(
        &self,
        project: ProjectIdentity,
    ) -> Result<Vec<Box<dyn ConversationSource>>, ConversationSourceError> {
        Ok(vec![
            Box::new(ClaudeCodeSource::new(
                project.clone(),
                self.claude_code().to_path_buf(),
            )?),
            Box::new(CodexCliSource::new(
                project.clone(),
                self.codex_cli().to_path_buf(),
            )?),
            Box::new(OpenCodeSource::new(
                project.clone(),
                self.opencode().to_path_buf(),
            )?),
            Box::new(OmpSource::new(project.clone(), self.omp().to_path_buf())?),
            Box::new(PiSource::new(project, self.pi().to_path_buf())?),
        ])
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StorageProbe {
    Available,
    Unavailable { reason: String },
}

/// Opaque, source-scoped cursor for incremental discovery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceWatermark {
    source_id: SourceId,
    token: String,
}

impl SourceWatermark {
    pub fn new(
        source_id: SourceId,
        token: impl Into<String>,
    ) -> Result<Self, ConversationSourceError> {
        let token = token.into();
        if token.is_empty() {
            return Err(ConversationSourceError::new(
                source_id,
                ConversationSourceErrorKind::InvalidData,
                "watermark token must be non-empty",
            ));
        }
        Ok(Self { source_id, token })
    }

    #[must_use]
    pub const fn source_id(&self) -> &SourceId {
        &self.source_id
    }

    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DiscoveryLimit(NonZeroUsize);

impl DiscoveryLimit {
    pub fn new(value: usize) -> Option<Self> {
        NonZeroUsize::new(value).map(Self)
    }

    #[must_use]
    pub const fn get(self) -> usize {
        self.0.get()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MetadataBudget(NonZeroUsize);

impl MetadataBudget {
    pub fn new(max_bytes: usize) -> Option<Self> {
        NonZeroUsize::new(max_bytes).map(Self)
    }

    #[must_use]
    pub const fn max_bytes(self) -> usize {
        self.0.get()
    }
}

/// Cheap source-owned reference discovered before transcript metadata is parsed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationCandidate {
    source_id: SourceId,
    project_identity: ProjectIdentity,
    source_reference: String,
    source_path: Option<PathBuf>,
    observed_size: Option<u64>,
    modified_at: Option<SystemTime>,
    fingerprint: Option<String>,
}

impl ConversationCandidate {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        source_id: SourceId,
        project_identity: ProjectIdentity,
        source_reference: impl Into<String>,
        source_path: Option<PathBuf>,
        observed_size: Option<u64>,
        modified_at: Option<SystemTime>,
        fingerprint: Option<String>,
    ) -> Result<Self, ConversationSourceError> {
        let source_reference = source_reference.into();
        if source_reference.trim().is_empty() {
            return Err(ConversationSourceError::new(
                source_id,
                ConversationSourceErrorKind::InvalidData,
                "candidate reference must be non-empty",
            ));
        }
        Ok(Self {
            source_id,
            project_identity,
            source_reference,
            source_path,
            observed_size,
            modified_at,
            fingerprint,
        })
    }

    #[must_use]
    pub const fn source_id(&self) -> &SourceId {
        &self.source_id
    }

    #[must_use]
    pub const fn project_identity(&self) -> &ProjectIdentity {
        &self.project_identity
    }

    #[must_use]
    pub fn source_reference(&self) -> &str {
        &self.source_reference
    }

    #[must_use]
    pub fn source_path(&self) -> Option<&Path> {
        self.source_path.as_deref()
    }

    #[must_use]
    pub const fn observed_size(&self) -> Option<u64> {
        self.observed_size
    }

    #[must_use]
    pub const fn modified_at(&self) -> Option<SystemTime> {
        self.modified_at
    }

    #[must_use]
    pub fn fingerprint(&self) -> Option<&str> {
        self.fingerprint.as_deref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationRemoval {
    source_id: SourceId,
    session_reference: SessionReference,
}

impl ConversationRemoval {
    #[must_use]
    pub const fn new(source_id: SourceId, session_reference: SessionReference) -> Self {
        Self {
            source_id,
            session_reference,
        }
    }

    #[must_use]
    pub const fn source_id(&self) -> &SourceId {
        &self.source_id
    }

    #[must_use]
    pub const fn session_reference(&self) -> &SessionReference {
        &self.session_reference
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveryBatch {
    candidates: Vec<ConversationCandidate>,
    next_watermark: Option<SourceWatermark>,
    removals: Vec<ConversationRemoval>,
    errors: Vec<ConversationSourceError>,
    has_more: bool,
}

impl DiscoveryBatch {
    pub fn new(
        source_id: &SourceId,
        project: &ProjectIdentity,
        candidates: Vec<ConversationCandidate>,
        next_watermark: Option<SourceWatermark>,
        removals: Vec<ConversationRemoval>,
        has_more: bool,
        errors: Vec<ConversationSourceError>,
    ) -> Result<Self, ConversationSourceError> {
        let sources_match = candidates
            .iter()
            .all(|candidate| candidate.source_id() == source_id)
            && next_watermark
                .as_ref()
                .is_none_or(|watermark| watermark.source_id() == source_id)
            && removals
                .iter()
                .all(|removal| removal.source_id() == source_id)
            && errors.iter().all(|error| error.source_id() == source_id);
        if !sources_match {
            return Err(ConversationSourceError::new(
                source_id.clone(),
                ConversationSourceErrorKind::SourceMismatch,
                "discovery batch contains data owned by another source",
            ));
        }
        if candidates
            .iter()
            .any(|candidate| candidate.project_identity() != project)
        {
            return Err(ConversationSourceError::new(
                source_id.clone(),
                ConversationSourceErrorKind::ProjectMismatch,
                "discovery batch contains data owned by another project",
            ));
        }
        Ok(Self {
            candidates,
            next_watermark,
            removals,
            errors,
            has_more,
        })
    }

    #[must_use]
    pub fn candidates(&self) -> &[ConversationCandidate] {
        &self.candidates
    }

    #[must_use]
    pub const fn next_watermark(&self) -> Option<&SourceWatermark> {
        self.next_watermark.as_ref()
    }
    #[must_use]
    pub fn removals(&self) -> &[ConversationRemoval] {
        &self.removals
    }

    /// Per-entry errors are non-fatal and do not discard valid candidates.
    #[must_use]
    pub fn errors(&self) -> &[ConversationSourceError] {
        &self.errors
    }

    #[must_use]
    pub const fn has_more(&self) -> bool {
        self.has_more
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectEvidenceKind {
    RecognizedProjectLocalPath,
    CanonicalWorkingDirectory,
    CanonicalWorkspaceRoot,
    AdapterValidatedEncoding,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectAssociationEvidence {
    kind: ProjectEvidenceKind,
    canonical_path: CanonicalPath,
    detail: Option<String>,
}

impl ProjectAssociationEvidence {
    #[must_use]
    pub const fn new(
        kind: ProjectEvidenceKind,
        canonical_path: CanonicalPath,
        detail: Option<String>,
    ) -> Self {
        Self {
            kind,
            canonical_path,
            detail,
        }
    }

    #[must_use]
    pub const fn kind(&self) -> ProjectEvidenceKind {
        self.kind
    }

    #[must_use]
    pub fn canonical_path(&self) -> &Path {
        self.canonical_path.as_path()
    }

    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }
}

/// Adapter contract. Every potentially expensive operation is explicit and bounded.
///
/// Checked default methods enforce source and project ownership before and after
/// calling adapter-specific implementations.
pub trait ConversationSource: Send + Sync {
    fn source_id(&self) -> &SourceId;
    fn probe(&self) -> Result<StorageProbe, ConversationSourceError>;

    fn discover_raw(
        &self,
        project: &ProjectIdentity,
        after: Option<&SourceWatermark>,
        limit: DiscoveryLimit,
    ) -> Result<DiscoveryBatch, ConversationSourceError>;
    fn discover_raw_cancellable(
        &self,
        project: &ProjectIdentity,
        after: Option<&SourceWatermark>,
        limit: DiscoveryLimit,
        _cancelled: &AtomicBool,
    ) -> Result<DiscoveryBatch, ConversationSourceError> {
        self.discover_raw(project, after, limit)
    }

    fn discover(
        &self,
        project: &ProjectIdentity,
        after: Option<&SourceWatermark>,
        limit: DiscoveryLimit,
    ) -> Result<DiscoveryBatch, ConversationSourceError> {
        let cancelled = AtomicBool::new(false);
        self.discover_cancellable(project, after, limit, &cancelled)
    }

    fn discover_cancellable(
        &self,
        project: &ProjectIdentity,
        after: Option<&SourceWatermark>,
        limit: DiscoveryLimit,
        cancelled: &AtomicBool,
    ) -> Result<DiscoveryBatch, ConversationSourceError> {
        self.ensure_watermark(after)?;
        let batch = self.discover_raw_cancellable(project, after, limit, cancelled)?;
        DiscoveryBatch::new(
            self.source_id(),
            project,
            batch.candidates,
            batch.next_watermark,
            batch.removals,
            batch.has_more,
            batch.errors,
        )
    }

    fn extract_metadata_raw(
        &self,
        candidate: &ConversationCandidate,
        budget: MetadataBudget,
    ) -> Result<Conversation, ConversationSourceError>;

    fn extract_metadata(
        &self,
        candidate: &ConversationCandidate,
        budget: MetadataBudget,
    ) -> Result<Conversation, ConversationSourceError> {
        self.ensure_candidate(candidate)?;
        let conversation = self.extract_metadata_raw(candidate, budget)?;
        if conversation.project_identity() != candidate.project_identity() {
            return Err(self.source_mismatch("adapter changed candidate project identity"));
        }
        Ok(conversation)
    }

    fn project_evidence_raw(
        &self,
        candidate: &ConversationCandidate,
        project: &ProjectIdentity,
    ) -> Result<Vec<ProjectAssociationEvidence>, ConversationSourceError>;

    fn project_evidence(
        &self,
        candidate: &ConversationCandidate,
        project: &ProjectIdentity,
    ) -> Result<Vec<ProjectAssociationEvidence>, ConversationSourceError> {
        self.ensure_candidate(candidate)?;
        if candidate.project_identity() != project {
            return Err(self.source_mismatch("candidate belongs to another project"));
        }
        self.project_evidence_raw(candidate, project)
    }

    fn ensure_watermark(
        &self,
        watermark: Option<&SourceWatermark>,
    ) -> Result<(), ConversationSourceError> {
        if watermark.is_some_and(|value| value.source_id() != self.source_id()) {
            return Err(self.source_mismatch("watermark belongs to another source"));
        }
        Ok(())
    }

    fn ensure_candidate(
        &self,
        candidate: &ConversationCandidate,
    ) -> Result<(), ConversationSourceError> {
        if candidate.source_id() != self.source_id() {
            return Err(self.source_mismatch("candidate belongs to another source"));
        }
        Ok(())
    }

    fn source_mismatch(&self, message: &'static str) -> ConversationSourceError {
        ConversationSourceError::new(
            self.source_id().clone(),
            ConversationSourceErrorKind::SourceMismatch,
            message,
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConversationSourceErrorKind {
    Unavailable,
    PermissionDenied,
    MalformedData,
    UnsupportedFormat,
    InvalidData,
    SourceMismatch,
    ProjectMismatch,
    Io,
}

/// Structured adapter failure. Callers isolate it to one source or candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationSourceError {
    source_id: SourceId,
    kind: ConversationSourceErrorKind,
    message: String,
    path: Option<PathBuf>,
}

impl ConversationSourceError {
    pub fn new(
        source_id: SourceId,
        kind: ConversationSourceErrorKind,
        message: impl Into<String>,
    ) -> Self {
        Self {
            source_id,
            kind,
            message: message.into(),
            path: None,
        }
    }

    #[must_use]
    pub fn unavailable(source_id: SourceId, message: impl Into<String>) -> Self {
        Self::new(source_id, ConversationSourceErrorKind::Unavailable, message)
    }

    #[must_use]
    pub fn with_path(mut self, path: PathBuf) -> Self {
        self.path = Some(path);
        self
    }

    #[must_use]
    pub const fn source_id(&self) -> &SourceId {
        &self.source_id
    }

    #[must_use]
    pub const fn kind(&self) -> ConversationSourceErrorKind {
        self.kind
    }

    #[must_use]
    pub(crate) fn message(&self) -> &str {
        &self.message
    }

    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

impl fmt::Display for ConversationSourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "conversation source {}: {}",
            self.source_id.as_str(),
            self.message
        )
    }
}

impl Error for ConversationSourceError {}

/// Deterministic set of conversation sources.
///
/// Sources are ordered lexicographically by their open `SourceId` namespace.
/// Registration fails rather than selecting an arbitrary implementation when
/// two sources claim the same ID.
pub struct SourceRegistry {
    sources: Vec<Box<dyn ConversationSource>>,
    desired_source_ids: HashSet<SourceId>,
}

impl SourceRegistry {
    pub fn new(sources: Vec<Box<dyn ConversationSource>>) -> Result<Self, SourceRegistryError> {
        Self::new_with_desired_source_ids(sources, Vec::new())
    }

    pub(crate) fn new_with_desired_source_ids(
        mut sources: Vec<Box<dyn ConversationSource>>,
        desired_source_ids: Vec<SourceId>,
    ) -> Result<Self, SourceRegistryError> {
        sources.sort_unstable_by(|left, right| {
            left.source_id().as_str().cmp(right.source_id().as_str())
        });
        if let Some(source_id) = sources.windows(2).find_map(|pair| {
            (pair[0].source_id() == pair[1].source_id()).then(|| pair[0].source_id().clone())
        }) {
            return Err(SourceRegistryError { source_id });
        }
        let mut desired_source_ids = desired_source_ids.into_iter().collect::<HashSet<_>>();
        desired_source_ids.extend(sources.iter().map(|source| source.source_id().clone()));
        Ok(Self {
            sources,
            desired_source_ids,
        })
    }

    #[must_use]
    pub fn iter(
        &self,
    ) -> impl ExactSizeIterator<Item = &dyn ConversationSource> + DoubleEndedIterator {
        self.sources.iter().map(Box::as_ref)
    }

    #[must_use]
    pub(crate) fn retains(&self, source_id: &SourceId) -> bool {
        self.desired_source_ids.contains(source_id)
    }

    pub(crate) fn desired_source_ids(&self) -> impl Iterator<Item = &SourceId> {
        self.desired_source_ids.iter()
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.sources.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }
}

impl fmt::Debug for SourceRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SourceRegistry")
            .field("source_count", &self.sources.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceRegistryError {
    source_id: SourceId,
}

impl SourceRegistryError {
    #[must_use]
    pub const fn source_id(&self) -> &SourceId {
        &self.source_id
    }
}

impl fmt::Display for SourceRegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "duplicate conversation source ID: {}",
            self.source_id.as_str()
        )
    }
}

impl Error for SourceRegistryError {}
