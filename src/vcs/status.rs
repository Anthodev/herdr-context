use std::error::Error;
use std::fmt;
use std::path::{Component, Path, PathBuf};

/// Normalized status vocabulary shared by every VCS adapter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VcsStatusKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    Untracked,
    Conflicted,
    TypeChanged,
}

/// Root-relative normalized status entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VcsEntryStatus {
    path: PathBuf,
    source_path: Option<PathBuf>,
    kind: VcsStatusKind,
    index_state: Option<VcsStatusKind>,
    worktree_state: Option<VcsStatusKind>,
}

impl VcsEntryStatus {
    pub fn new(
        path: PathBuf,
        source_path: Option<PathBuf>,
        kind: VcsStatusKind,
        index_state: Option<VcsStatusKind>,
        worktree_state: Option<VcsStatusKind>,
    ) -> Result<Self, VcsEntryStatusError> {
        validate_relative_path(&path, "path")?;
        if let Some(source_path) = &source_path {
            validate_relative_path(source_path, "source_path")?;
        }
        if matches!(kind, VcsStatusKind::Renamed | VcsStatusKind::Copied) && source_path.is_none() {
            return Err(VcsEntryStatusError::MissingSourcePath(kind));
        }
        Ok(Self {
            path,
            source_path,
            kind,
            index_state,
            worktree_state,
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn source_path(&self) -> Option<&Path> {
        self.source_path.as_deref()
    }

    #[must_use]
    pub const fn kind(&self) -> VcsStatusKind {
        self.kind
    }

    #[must_use]
    pub const fn index_state(&self) -> Option<VcsStatusKind> {
        self.index_state
    }

    #[must_use]
    pub const fn worktree_state(&self) -> Option<VcsStatusKind> {
        self.worktree_state
    }
}

fn validate_relative_path(path: &Path, field: &'static str) -> Result<(), VcsEntryStatusError> {
    let mut components = path.components();
    let Some(first) = components.next() else {
        return Err(VcsEntryStatusError::EmptyPath(field));
    };
    if first != Component::Normal(first.as_os_str())
        || components.any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(VcsEntryStatusError::NonNormalizedPath {
            field,
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VcsStatusSnapshot {
    entries: Vec<VcsEntryStatus>,
    stale: bool,
}

impl VcsStatusSnapshot {
    #[must_use]
    pub const fn new(entries: Vec<VcsEntryStatus>, stale: bool) -> Self {
        Self { entries, stale }
    }

    #[must_use]
    pub fn entries(&self) -> &[VcsEntryStatus] {
        &self.entries
    }

    #[must_use]
    pub const fn is_stale(&self) -> bool {
        self.stale
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VcsEntryStatusError {
    EmptyPath(&'static str),
    NonNormalizedPath { field: &'static str, path: PathBuf },
    MissingSourcePath(VcsStatusKind),
}

impl fmt::Display for VcsEntryStatusError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPath(field) => write!(formatter, "{field} must be non-empty"),
            Self::NonNormalizedPath { field, path } => {
                write!(
                    formatter,
                    "{field} {} must be root-relative and normalized",
                    path.display()
                )
            }
            Self::MissingSourcePath(kind) => {
                write!(formatter, "{kind:?} status requires source_path")
            }
        }
    }
}

impl Error for VcsEntryStatusError {}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{VcsEntryStatus, VcsEntryStatusError, VcsStatusKind};

    #[test]
    fn accepts_normalized_relative_entry() -> Result<(), Box<dyn std::error::Error>> {
        let status = VcsEntryStatus::new(
            PathBuf::from("src/lib.rs"),
            None,
            VcsStatusKind::Modified,
            None,
            Some(VcsStatusKind::Modified),
        )?;
        assert_eq!(status.path(), PathBuf::from("src/lib.rs"));
        Ok(())
    }

    #[test]
    fn rejects_parent_traversal() {
        let error = VcsEntryStatus::new(
            PathBuf::from("../outside"),
            None,
            VcsStatusKind::Modified,
            None,
            None,
        );
        assert!(matches!(
            error,
            Err(VcsEntryStatusError::NonNormalizedPath { .. })
        ));
    }

    #[test]
    fn rename_requires_source_path() {
        let error = VcsEntryStatus::new(
            PathBuf::from("new-name"),
            None,
            VcsStatusKind::Renamed,
            None,
            None,
        );
        assert_eq!(
            error,
            Err(VcsEntryStatusError::MissingSourcePath(
                VcsStatusKind::Renamed
            ))
        );
    }
}
