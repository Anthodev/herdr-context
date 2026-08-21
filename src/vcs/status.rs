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
        let path = normalize_relative_path(path, "path")?;
        let source_path = source_path
            .map(|path| normalize_relative_path(path, "source_path"))
            .transpose()?;
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

fn normalize_relative_path(
    path: PathBuf,
    field: &'static str,
) -> Result<PathBuf, VcsEntryStatusError> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        let Component::Normal(component) = component else {
            return Err(VcsEntryStatusError::NonNormalizedPath { field, path });
        };
        normalized.push(component);
    }
    if normalized.as_os_str().is_empty() {
        return Err(VcsEntryStatusError::EmptyPath(field));
    }
    if normalized.as_os_str() == path.as_os_str() {
        Ok(path)
    } else {
        Ok(normalized)
    }
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

/// Aggregate line-level totals of the workspace diff (tracked changes only).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VcsDiffStats {
    insertions: u64,
    deletions: u64,
}

impl VcsDiffStats {
    #[must_use]
    pub const fn new(insertions: u64, deletions: u64) -> Self {
        Self {
            insertions,
            deletions,
        }
    }

    #[must_use]
    pub const fn insertions(&self) -> u64 {
        self.insertions
    }

    #[must_use]
    pub const fn deletions(&self) -> u64 {
        self.deletions
    }
}

/// Parses the git-style `--shortstat` summary line ("…, 2 insertions(+), 1
/// deletion(-)"). Empty or whitespace-only output means "no changes" and maps
/// to zeroed stats; non-empty output without a parsable summary is unknown.
#[must_use]
pub(crate) fn parse_shortstat_summary(output: &[u8]) -> Option<VcsDiffStats> {
    let text = String::from_utf8_lossy(output);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Some(VcsDiffStats::new(0, 0));
    }
    let summary = trimmed
        .lines()
        .rev()
        .find(|line| line.contains("changed"))?;
    let insertions = number_before(summary, "insertion").unwrap_or(0);
    let deletions = number_before(summary, "deletion").unwrap_or(0);
    Some(VcsDiffStats::new(insertions, deletions))
}

fn number_before(text: &str, marker: &str) -> Option<u64> {
    let end = text.find(marker)?;
    let prefix = text[..end].trim_end();
    let start = prefix
        .char_indices()
        .rev()
        .find(|(_, character)| !character.is_ascii_digit())
        .map_or(0, |(index, _)| index + 1);
    prefix.get(start..)?.parse().ok()
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

    use super::{
        VcsDiffStats, VcsEntryStatus, VcsEntryStatusError, VcsStatusKind, parse_shortstat_summary,
    };

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

    #[test]
    fn normalizes_equivalent_relative_path_spellings() {
        let status = VcsEntryStatus::new(
            PathBuf::from("src//./lib.rs/"),
            None,
            VcsStatusKind::Modified,
            None,
            Some(VcsStatusKind::Modified),
        )
        .expect("normalized status");
        assert_eq!(status.path(), PathBuf::from("src/lib.rs"));
    }

    #[test]
    fn parses_git_style_shortstat_summaries() {
        assert_eq!(parse_shortstat_summary(b""), Some(VcsDiffStats::new(0, 0)));
        assert_eq!(
            parse_shortstat_summary(b"\n"),
            Some(VcsDiffStats::new(0, 0))
        );
        assert_eq!(
            parse_shortstat_summary(b" 3 files changed, 12 insertions(+), 4 deletions(-)\n"),
            Some(VcsDiffStats::new(12, 4))
        );
        assert_eq!(
            parse_shortstat_summary(b" 1 file changed, 1 insertion(+)\n"),
            Some(VcsDiffStats::new(1, 0))
        );
        // The jj diffstat body precedes the same summary line.
        assert_eq!(
            parse_shortstat_summary(
                b" src/main.rs | 10 ++++++---\n 1 file changed, 7 insertions(+), 3 deletions(-)\n"
            ),
            Some(VcsDiffStats::new(7, 3))
        );
        assert_eq!(parse_shortstat_summary(b"garbage"), None);
    }
}
