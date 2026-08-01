//! Provider-neutral conversation model.

pub mod sources;

use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::project::ProjectIdentity;

fn normalized_identifier(
    value: impl Into<String>,
    field: &'static str,
) -> Result<String, ConversationError> {
    crate::normalize_nonempty(value).ok_or(ConversationError::EmptyIdentifier(field))
}

/// Open source namespace shared by provenance, watermarks, and adapters.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SourceId(String);

impl SourceId {
    pub fn new(value: impl Into<String>) -> Result<Self, ConversationError> {
        normalized_identifier(value, "source id").map(Self)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Open tool namespace. Adding a tool does not change domain enums.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ToolIdentity(String);

impl ToolIdentity {
    pub fn new(value: impl Into<String>) -> Result<Self, ConversationError> {
        normalized_identifier(value, "tool").map(Self)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SessionReference {
    namespace: String,
    id: String,
}

impl SessionReference {
    pub fn new(
        namespace: impl Into<String>,
        id: impl Into<String>,
    ) -> Result<Self, ConversationError> {
        Ok(Self {
            namespace: normalized_identifier(namespace, "session namespace")?,
            id: normalized_identifier(id, "session id")?,
        })
    }

    #[must_use]
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConversationState {
    Live,
    Archived,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProvenanceKind {
    ProjectLocal,
    ExternalLocal,
    HostRuntime,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationProvenance {
    source_id: SourceId,
    kind: ProvenanceKind,
    path: Option<PathBuf>,
}

impl ConversationProvenance {
    #[must_use]
    pub const fn new(source_id: SourceId, kind: ProvenanceKind, path: Option<PathBuf>) -> Self {
        Self {
            source_id,
            kind,
            path,
        }
    }

    #[must_use]
    pub const fn source_id(&self) -> &SourceId {
        &self.source_id
    }

    #[must_use]
    pub const fn kind(&self) -> ProvenanceKind {
        self.kind
    }

    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResumeReference(String);

impl ResumeReference {
    pub fn new(value: impl Into<String>) -> Result<Self, ConversationError> {
        normalized_identifier(value, "resume reference").map(Self)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResumeCapability {
    Unsupported,
    Supported(ResumeReference),
}

/// Display-safe metadata shared across source adapters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Conversation {
    tool: ToolIdentity,
    session_reference: SessionReference,
    project_identity: ProjectIdentity,
    title: Option<String>,
    created_at: Option<SystemTime>,
    updated_at: SystemTime,
    state: ConversationState,
    provenance: Vec<ConversationProvenance>,
    resume: ResumeCapability,
}

impl Conversation {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tool: ToolIdentity,
        session_reference: SessionReference,
        project_identity: ProjectIdentity,
        title: Option<String>,
        created_at: Option<SystemTime>,
        updated_at: SystemTime,
        state: ConversationState,
        provenance: Vec<ConversationProvenance>,
        resume: ResumeCapability,
    ) -> Result<Self, ConversationError> {
        if provenance.is_empty() {
            return Err(ConversationError::MissingProvenance);
        }
        let title = title.and_then(crate::normalize_nonempty);
        Ok(Self {
            tool,
            session_reference,
            project_identity,
            title,
            created_at,
            updated_at,
            state,
            provenance,
            resume,
        })
    }

    #[must_use]
    pub const fn tool(&self) -> &ToolIdentity {
        &self.tool
    }

    #[must_use]
    pub const fn session_reference(&self) -> &SessionReference {
        &self.session_reference
    }

    #[must_use]
    pub const fn project_identity(&self) -> &ProjectIdentity {
        &self.project_identity
    }

    #[must_use]
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    #[must_use]
    pub const fn created_at(&self) -> Option<SystemTime> {
        self.created_at
    }

    #[must_use]
    pub const fn updated_at(&self) -> SystemTime {
        self.updated_at
    }

    #[must_use]
    pub const fn state(&self) -> ConversationState {
        self.state
    }

    #[must_use]
    pub fn provenance(&self) -> &[ConversationProvenance] {
        &self.provenance
    }

    #[must_use]
    pub const fn resume_capability(&self) -> &ResumeCapability {
        &self.resume
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConversationError {
    EmptyIdentifier(&'static str),
    MissingProvenance,
}

impl fmt::Display for ConversationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyIdentifier(field) => write!(formatter, "{field} must be non-empty"),
            Self::MissingProvenance => write!(formatter, "conversation must preserve provenance"),
        }
    }
}

impl Error for ConversationError {}
