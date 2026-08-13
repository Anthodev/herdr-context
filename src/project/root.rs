use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::{CanonicalPath, DetectedVcs, ProjectContext, ProjectIdentity, VcsBackendIdentity};

/// Resolves project identity without running VCS commands or scanning descendants.
///
/// Files remain rooted at `opening_directory`. Conversation identity uses the
/// canonical nearest valid Jujutsu workspace, then nearest valid Git worktree,
/// then canonical opening directory.
pub fn resolve_project_context(
    opening_directory: impl AsRef<Path>,
) -> Result<ProjectContext, ProjectResolutionError> {
    let files_root = opening_directory.as_ref().to_path_buf();
    if !files_root.is_absolute() {
        return Err(ProjectResolutionError::NonAbsoluteFilesRoot(files_root));
    }
    let canonical_opening = CanonicalPath::new(files_root.clone())?;
    if !canonical_opening.as_path().is_dir() {
        return Err(ProjectResolutionError::NotDirectory(
            canonical_opening.as_path().to_path_buf(),
        ));
    }

    let mut nearest_jj = None;
    let mut nearest_git = None;
    for ancestor in canonical_opening.as_path().ancestors() {
        if valid_jj_marker(ancestor)? {
            nearest_jj = Some(CanonicalPath::from_canonicalized(ancestor.to_path_buf()));
            break;
        }
        if nearest_git.is_none() && valid_git_marker(ancestor)? {
            nearest_git = Some(CanonicalPath::from_canonicalized(ancestor.to_path_buf()));
        }
    }

    let selected = nearest_jj
        .map(|root| ("jj", root))
        .or_else(|| nearest_git.map(|root| ("git", root)));

    let (identity_root, vcs) = match selected {
        Some((backend, root)) => {
            let backend = VcsBackendIdentity::new(backend)?;
            (root.clone(), Some(DetectedVcs::new(backend, root)))
        }
        None => (canonical_opening, None),
    };
    let conversation_identity = ProjectIdentity::from_canonical_path(identity_root);

    Ok(ProjectContext::new(files_root, conversation_identity, vcs))
}

fn valid_jj_marker(root: &Path) -> Result<bool, ProjectResolutionError> {
    let marker = root.join(".jj");
    let Some(metadata) = marker_metadata(&marker)? else {
        return Ok(false);
    };
    if !metadata.is_dir() {
        return Ok(false);
    }
    Ok(marker.join("repo").is_dir() && marker.join("working_copy").is_dir())
}

fn valid_git_marker(root: &Path) -> Result<bool, ProjectResolutionError> {
    let marker = root.join(".git");
    let Some(metadata) = marker_metadata(&marker)? else {
        return Ok(false);
    };
    if metadata.is_dir() {
        return Ok(valid_git_admin_dir(&marker));
    }
    if !metadata.is_file() {
        return Ok(false);
    }

    let contents =
        fs::read_to_string(&marker).map_err(|source| ProjectResolutionError::InspectMarker {
            path: marker.clone(),
            source: IoError::from(source),
        })?;
    let Some(target) = contents.trim().strip_prefix("gitdir:") else {
        return Ok(false);
    };
    let target = PathBuf::from(target.trim());
    if target.as_os_str().is_empty() {
        return Ok(false);
    }
    let target = if target.is_absolute() {
        target
    } else {
        root.join(target)
    };
    match fs::canonicalize(target) {
        Ok(path) => Ok(valid_git_admin_dir(&path)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(ProjectResolutionError::InspectMarker {
            path: marker,
            source: IoError::from(source),
        }),
    }
}

fn valid_git_admin_dir(path: &Path) -> bool {
    path.join("HEAD").is_file()
        && (path.join("objects").is_dir() || path.join("commondir").is_file())
}

fn marker_metadata(path: &Path) -> Result<Option<fs::Metadata>, ProjectResolutionError> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ProjectResolutionError::InspectMarker {
            path: path.to_path_buf(),
            source: IoError::from(source),
        }),
    }
}

/// Component-aware ownership check. Both paths should already be normalized.
#[must_use]
pub fn path_is_within(root: &Path, candidate: &Path) -> bool {
    candidate.starts_with(root)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProjectResolutionError {
    Canonicalize { path: PathBuf, source: IoError },
    InspectMarker { path: PathBuf, source: IoError },
    NotDirectory(PathBuf),
    NonAbsoluteFilesRoot(PathBuf),
    NonAbsoluteIdentity(PathBuf),
    EmptyBackendIdentity,
}

impl fmt::Display for ProjectResolutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Canonicalize { path, source } => {
                write!(
                    formatter,
                    "cannot canonicalize {}: {source}",
                    path.display()
                )
            }
            Self::InspectMarker { path, source } => {
                write!(formatter, "cannot inspect {}: {source}", path.display())
            }
            Self::NotDirectory(path) => write!(formatter, "{} is not a directory", path.display()),
            Self::NonAbsoluteFilesRoot(path) => {
                write!(formatter, "files root {} is not absolute", path.display())
            }
            Self::NonAbsoluteIdentity(path) => {
                write!(
                    formatter,
                    "project identity {} is not absolute",
                    path.display()
                )
            }
            Self::EmptyBackendIdentity => write!(formatter, "VCS backend identity is empty"),
        }
    }
}

impl Error for ProjectResolutionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Canonicalize { source, .. } | Self::InspectMarker { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Cloneable I/O detail suitable for structured domain errors.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IoError {
    kind: io::ErrorKind,
    message: String,
}

impl IoError {
    #[must_use]
    pub const fn kind(&self) -> io::ErrorKind {
        self.kind
    }
}

impl From<io::Error> for IoError {
    fn from(error: io::Error) -> Self {
        Self {
            kind: error.kind(),
            message: error.to_string(),
        }
    }
}

impl fmt::Display for IoError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

impl Error for IoError {}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::path_is_within;

    #[test]
    fn ownership_is_component_aware() {
        assert!(path_is_within(
            Path::new("/srv/app"),
            Path::new("/srv/app/src")
        ));
        assert!(!path_is_within(
            Path::new("/srv/app"),
            Path::new("/srv/application")
        ));
    }
}
