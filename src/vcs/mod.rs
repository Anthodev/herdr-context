//! Backend-neutral VCS contracts.

pub mod status;

use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

pub use status::{VcsEntryStatus, VcsEntryStatusError, VcsStatusKind, VcsStatusSnapshot};

/// Open backend identifier and capabilities. No adapter record escapes here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VcsBackendMetadata {
    id: String,
    display_name: String,
    supports_stale_status: bool,
}

impl VcsBackendMetadata {
    pub fn new(
        id: impl Into<String>,
        display_name: impl Into<String>,
        supports_stale_status: bool,
    ) -> Result<Self, VcsError> {
        let (Some(id), Some(display_name)) = (
            crate::normalize_nonempty(id),
            crate::normalize_nonempty(display_name),
        ) else {
            return Err(VcsError::new(
                VcsErrorKind::InvalidData,
                "backend id and display name must be non-empty",
            ));
        };
        Ok(Self {
            id,
            display_name,
            supports_stale_status,
        })
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    #[must_use]
    pub const fn supports_stale_status(&self) -> bool {
        self.supports_stale_status
    }
}

/// Result of backend detection, expressed only through normalized metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VcsWorkspace {
    root: PathBuf,
    backend: VcsBackendMetadata,
}

impl VcsWorkspace {
    pub fn new(root: PathBuf, backend: VcsBackendMetadata) -> Result<Self, VcsError> {
        if !root.is_absolute() {
            return Err(VcsError::new(
                VcsErrorKind::InvalidData,
                "workspace root must be absolute",
            ));
        }
        Ok(Self { root, backend })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub const fn backend(&self) -> &VcsBackendMetadata {
        &self.backend
    }
}

/// Boundary implemented by Git, Jujutsu, or future VCS adapters.
///
/// Detection returns normalized workspace values. Status refresh explicitly
/// names that workspace, preventing service state from drifting from detection.
pub trait VcsService: Send {
    fn detect(&self, start: &Path) -> Result<Option<VcsWorkspace>, VcsError>;
    fn refresh_status(&mut self, workspace: &VcsWorkspace) -> Result<VcsStatusSnapshot, VcsError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VcsErrorKind {
    Unavailable,
    PermissionDenied,
    InvalidData,
    CommandFailed,
    Io,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VcsError {
    kind: VcsErrorKind,
    message: String,
}

impl VcsError {
    pub fn new(kind: VcsErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> VcsErrorKind {
        self.kind
    }
}

impl fmt::Display for VcsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

impl Error for VcsError {}
