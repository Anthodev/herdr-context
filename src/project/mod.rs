//! Project identity and root normalization.

mod root;

pub use root::{IoError, ProjectResolutionError, path_is_within, resolve_project_context};

use std::path::{Path, PathBuf};

/// Canonical, absolute path used at domain boundaries.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CanonicalPath(PathBuf);

impl CanonicalPath {
    /// Canonicalizes an absolute existing path.
    pub fn new(path: PathBuf) -> Result<Self, ProjectResolutionError> {
        if !path.is_absolute() {
            return Err(ProjectResolutionError::NonAbsoluteIdentity(path));
        }
        let canonical = std::fs::canonicalize(&path).map_err(|source| {
            ProjectResolutionError::Canonicalize {
                path,
                source: IoError::from(source),
            }
        })?;
        Ok(Self(canonical))
    }

    /// Wraps a path already derived from a canonical path.
    pub(crate) const fn from_canonicalized(path: PathBuf) -> Self {
        Self(path)
    }

    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

/// Canonical project key used for conversation association.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ProjectIdentity {
    root: CanonicalPath,
}

impl ProjectIdentity {
    /// Builds an identity while enforcing canonical, absolute project paths.
    pub fn from_canonical_root(root: PathBuf) -> Result<Self, ProjectResolutionError> {
        Ok(Self {
            root: CanonicalPath::new(root)?,
        })
    }

    pub(crate) const fn from_canonical_path(root: CanonicalPath) -> Self {
        Self { root }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        self.root.as_path()
    }
}

/// Opaque backend key. Adapters choose values; consumers must not branch on a
/// closed Git/Jujutsu enum.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct VcsBackendIdentity(String);

impl VcsBackendIdentity {
    pub fn new(value: impl Into<String>) -> Result<Self, ProjectResolutionError> {
        crate::normalize_nonempty(value)
            .map(Self)
            .ok_or(ProjectResolutionError::EmptyBackendIdentity)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Supported VCS workspace selected for project identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DetectedVcs {
    backend: VcsBackendIdentity,
    workspace_root: CanonicalPath,
}

impl DetectedVcs {
    pub(crate) const fn new(backend: VcsBackendIdentity, workspace_root: CanonicalPath) -> Self {
        Self {
            backend,
            workspace_root,
        }
    }

    #[must_use]
    pub const fn backend(&self) -> &VcsBackendIdentity {
        &self.backend
    }

    #[must_use]
    pub fn workspace_root(&self) -> &Path {
        self.workspace_root.as_path()
    }
}

/// Opening directory and canonical conversation identity kept intentionally separate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectContext {
    files_root: PathBuf,
    conversation_identity: ProjectIdentity,
    vcs: Option<DetectedVcs>,
}

impl ProjectContext {
    pub(crate) const fn new(
        files_root: PathBuf,
        conversation_identity: ProjectIdentity,
        vcs: Option<DetectedVcs>,
    ) -> Self {
        Self {
            files_root,
            conversation_identity,
            vcs,
        }
    }

    /// Exact absolute path supplied by opening pane. It is not replaced by VCS root.
    #[must_use]
    pub fn files_root(&self) -> &Path {
        &self.files_root
    }

    #[must_use]
    pub const fn conversation_identity(&self) -> &ProjectIdentity {
        &self.conversation_identity
    }

    #[must_use]
    pub const fn vcs(&self) -> Option<&DetectedVcs> {
        self.vcs.as_ref()
    }
}
