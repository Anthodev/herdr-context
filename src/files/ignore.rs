use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use cap_std::fs::{Dir, Metadata};
use ignore::gitignore::{Gitignore, GitignoreBuilder};

const MAX_IGNORE_FILE_BYTES: usize = 1_048_576;
const MAX_SEARCH_MATCHER_COMPONENTS: usize = 4_096;
const MAX_SEARCH_IGNORE_BYTES: usize = 4 * 1_048_576;
const MAX_SEARCH_IGNORE_RULES: usize = 100_000;

struct EnumerationBounds<'a> {
    max_entries: usize,
    max_examined: usize,
    cancelled: &'a AtomicBool,
    page_cancelled: &'a AtomicBool,
}

struct MatcherBudget<'a> {
    remaining_components: usize,
    remaining_bytes: usize,
    remaining_rules: usize,
    cancelled: &'a AtomicBool,
    page_cancelled: &'a AtomicBool,
}

impl<'a> MatcherBudget<'a> {
    const fn new(bounds: &'a EnumerationBounds<'a>) -> Self {
        Self {
            remaining_components: MAX_SEARCH_MATCHER_COMPONENTS,
            remaining_bytes: MAX_SEARCH_IGNORE_BYTES,
            remaining_rules: MAX_SEARCH_IGNORE_RULES,
            cancelled: bounds.cancelled,
            page_cancelled: bounds.page_cancelled,
        }
    }

    fn checkpoint(&self) -> io::Result<()> {
        if self.cancelled.load(Ordering::Relaxed) || self.page_cancelled.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "file search was cancelled",
            ));
        }
        Ok(())
    }

    fn consume_component(&mut self) -> io::Result<()> {
        self.checkpoint()?;
        if self.remaining_components == 0 {
            return Err(search_budget_exhausted());
        }
        self.remaining_components = self.remaining_components.saturating_sub(1);
        Ok(())
    }

    fn consume_matcher(&mut self, bytes: usize, rules: usize) -> io::Result<()> {
        self.checkpoint()?;
        if bytes > self.remaining_bytes || rules > self.remaining_rules {
            return Err(search_budget_exhausted());
        }
        self.remaining_bytes = self.remaining_bytes.saturating_sub(bytes);
        self.remaining_rules = self.remaining_rules.saturating_sub(rules);
        Ok(())
    }
}

/// Decides whether a root-relative filesystem entry is shown.
///
/// Implementations must be side-effect free in production. The seam is kept
/// independent from configuration so roadmap #11 can replace the fixed default.
pub trait VisibilityPolicy: fmt::Debug + Send + Sync {
    fn is_visible(&self, relative_path: &Path) -> bool;
}

/// Fixed HDC-10 default: dot-prefixed entries are hidden.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultVisibilityPolicy;

impl VisibilityPolicy for DefaultVisibilityPolicy {
    fn is_visible(&self, relative_path: &Path) -> bool {
        relative_path.components().all(|component| {
            let std::path::Component::Normal(name) = component else {
                return false;
            };
            !name.as_encoded_bytes().starts_with(b".")
        })
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfiguredVisibilityPolicy {
    show_hidden: bool,
    exclusions: Vec<PathBuf>,
}

impl ConfiguredVisibilityPolicy {
    #[must_use]
    pub const fn new(show_hidden: bool, exclusions: Vec<PathBuf>) -> Self {
        Self {
            show_hidden,
            exclusions,
        }
    }
}

impl VisibilityPolicy for ConfiguredVisibilityPolicy {
    fn is_visible(&self, relative_path: &Path) -> bool {
        let normal = relative_path.components().all(|component| {
            let std::path::Component::Normal(name) = component else {
                return false;
            };
            self.show_hidden || !name.as_encoded_bytes().starts_with(b".")
        });
        normal
            && !self
                .exclusions
                .iter()
                .any(|excluded| relative_path.starts_with(excluded))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VisibleEntryKind {
    Directory,
    File,
    Symlink,
}

#[derive(Debug)]
pub(crate) struct VisibleEntry {
    pub(crate) path: PathBuf,
    pub(crate) kind: VisibleEntryKind,
    pub(crate) ignored: bool,
}

pub(crate) struct VisibleEntriesBatch {
    pub(crate) entries: Vec<VisibleEntry>,
    pub(crate) examined: usize,
    pub(crate) truncated: bool,
}

#[derive(Clone, Debug)]
pub struct IgnorePolicy {
    root_path: PathBuf,
    root: Arc<Dir>,
    ignore_root_path: PathBuf,
    ignore_root: Arc<Dir>,
    files_prefix: PathBuf,
    visibility: Arc<dyn VisibilityPolicy>,
    gitignore_enabled: bool,
    show_ignored: bool,
}

impl IgnorePolicy {
    pub fn new(root: PathBuf) -> io::Result<Self> {
        Self::with_visibility_policy(root, Arc::new(DefaultVisibilityPolicy), false)
    }

    pub fn with_visibility_policy(
        root: PathBuf,
        visibility: Arc<dyn VisibilityPolicy>,
        show_ignored: bool,
    ) -> io::Result<Self> {
        Self::with_workspace_visibility_policy(root.clone(), root, visibility, show_ignored)
    }

    pub(crate) fn for_workspace(root: PathBuf, workspace_root: PathBuf) -> io::Result<Self> {
        Self::with_workspace_visibility_policy(
            root,
            workspace_root,
            Arc::new(DefaultVisibilityPolicy),
            false,
        )
    }
    pub(crate) fn for_workspace_with_visibility(
        root: PathBuf,
        workspace_root: PathBuf,
        visibility: Arc<dyn VisibilityPolicy>,
        show_ignored: bool,
    ) -> io::Result<Self> {
        Self::with_workspace_visibility_policy(root, workspace_root, visibility, show_ignored)
    }

    fn with_workspace_visibility_policy(
        root: PathBuf,
        workspace_root: PathBuf,
        visibility: Arc<dyn VisibilityPolicy>,
        show_ignored: bool,
    ) -> io::Result<Self> {
        if !root.is_absolute() || !workspace_root.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "root and workspace root must be absolute directories",
            ));
        }
        let root_path = fs::canonicalize(root)?;
        let ignore_root_path = fs::canonicalize(workspace_root)?;
        let files_prefix = root_path
            .strip_prefix(&ignore_root_path)
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "root must be inside the VCS workspace",
                )
            })?
            .to_path_buf();
        let root = Arc::new(open_ambient_directory_nofollow(&root_path)?);
        let ignore_root = if root_path == ignore_root_path {
            Arc::new(root.try_clone()?)
        } else {
            Arc::new(open_ambient_directory_nofollow(&ignore_root_path)?)
        };
        let gitignore_enabled = [".jj", ".git"].into_iter().any(|marker| {
            ignore_root
                .symlink_metadata(marker)
                .is_ok_and(|metadata| !metadata.is_symlink())
        });
        Ok(Self {
            root_path,
            root,
            ignore_root_path,
            ignore_root,
            files_prefix,
            visibility,
            gitignore_enabled,
            show_ignored,
        })
    }

    /// Clones the policy with the ignored-visibility flag flipped; no I/O.
    #[must_use]
    pub(crate) fn rescoped(&self, show_ignored: bool) -> Self {
        let mut rescoped = self.clone();
        rescoped.show_ignored = show_ignored;
        rescoped
    }

    #[must_use]
    pub fn is_visible(&self, relative_path: &Path) -> bool {
        self.visibility.is_visible(relative_path)
    }

    pub fn visible_children(&self, relative_directory: &Path) -> io::Result<Vec<PathBuf>> {
        self.visible_entries(relative_directory)
            .map(|entries| entries.into_iter().map(|entry| entry.path).collect())
    }

    pub(crate) fn visible_entries(
        &self,
        relative_directory: &Path,
    ) -> io::Result<Vec<VisibleEntry>> {
        self.visible_entries_with_bounds(relative_directory, None)
            .map(|batch| batch.entries)
    }

    pub(crate) fn visible_entries_bounded(
        &self,
        relative_directory: &Path,
        max_entries: usize,
        max_examined: usize,
        cancelled: &AtomicBool,
        page_cancelled: &AtomicBool,
    ) -> io::Result<VisibleEntriesBatch> {
        self.visible_entries_with_bounds(
            relative_directory,
            Some(EnumerationBounds {
                max_entries: max_entries.max(1),
                max_examined: max_examined.max(1),
                cancelled,
                page_cancelled,
            }),
        )
    }

    fn visible_entries_with_bounds(
        &self,
        relative_directory: &Path,
        bounds: Option<EnumerationBounds<'_>>,
    ) -> io::Result<VisibleEntriesBatch> {
        let mut matcher_budget = bounds.as_ref().map(MatcherBudget::new);
        let (directory, matchers, ancestor_ignored) =
            self.open_directory_with_matchers(relative_directory, matcher_budget.as_mut())?;
        if ancestor_ignored && !self.show_ignored {
            return Ok(VisibleEntriesBatch {
                entries: Vec::new(),
                examined: 0,
                truncated: false,
            });
        }
        let mut entries = Vec::new();
        let mut examined = 0_usize;
        let mut truncated = false;
        for result in directory.entries()? {
            if bounds
                .as_ref()
                .is_some_and(|bounds| examined >= bounds.max_examined)
            {
                truncated = true;
                break;
            }
            if bounds.as_ref().is_some_and(|bounds| {
                bounds.cancelled.load(Ordering::Relaxed)
                    || bounds.page_cancelled.load(Ordering::Relaxed)
            }) {
                break;
            }
            examined = examined.saturating_add(1);
            let entry = match result {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            let file_name = entry.file_name();
            let relative = relative_directory.join(&file_name);
            if !self.is_visible(&relative) {
                continue;
            }
            let metadata = match directory.symlink_metadata(&file_name) {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            let kind = if metadata.is_symlink() {
                VisibleEntryKind::Symlink
            } else if metadata.is_dir() {
                VisibleEntryKind::Directory
            } else {
                VisibleEntryKind::File
            };
            let ignored = is_ignored(
                &matchers,
                &self.root_path.join(&relative),
                kind == VisibleEntryKind::Directory,
            );
            if ignored && !self.show_ignored {
                continue;
            }
            if bounds
                .as_ref()
                .is_some_and(|bounds| entries.len() >= bounds.max_entries)
            {
                truncated = true;
                break;
            }
            entries.push(VisibleEntry {
                path: relative,
                kind,
                ignored,
            });
        }
        entries.sort_unstable_by(|left, right| left.path.cmp(&right.path));
        Ok(VisibleEntriesBatch {
            entries,
            examined,
            truncated,
        })
    }

    pub(crate) fn symlink_metadata(&self, relative_path: &Path) -> io::Result<Metadata> {
        validate_relative_path(relative_path)?;
        let parent = relative_path.parent().unwrap_or_else(|| Path::new(""));
        let name = relative_path
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing file name"))?;
        self.open_directory(parent)?.symlink_metadata(name)
    }

    fn open_directory(&self, relative_directory: &Path) -> io::Result<Dir> {
        self.open_directory_bounded(relative_directory, None)
    }

    fn open_directory_bounded(
        &self,
        relative_directory: &Path,
        mut budget: Option<&mut MatcherBudget<'_>>,
    ) -> io::Result<Dir> {
        validate_relative_directory(relative_directory)?;
        if let Some(budget) = budget.as_deref_mut() {
            budget.checkpoint()?;
        }
        let mut directory = self.root.try_clone()?;
        for component in relative_directory.components() {
            let std::path::Component::Normal(name) = component else {
                return Err(invalid_relative_path());
            };
            if let Some(budget) = budget.as_deref_mut() {
                budget.consume_component()?;
            }
            directory = open_child_directory_nofollow(&directory, name)?;
        }
        Ok(directory)
    }

    fn open_directory_with_matchers(
        &self,
        relative_directory: &Path,
        mut budget: Option<&mut MatcherBudget<'_>>,
    ) -> io::Result<(Dir, Vec<Gitignore>, bool)> {
        let directory = self.open_directory_bounded(relative_directory, budget.as_deref_mut())?;
        let (matchers, ancestor_ignored) = self.ignore_matchers(relative_directory, budget)?;
        Ok((directory, matchers, ancestor_ignored))
    }

    fn ignore_matchers(
        &self,
        relative_directory: &Path,
        mut budget: Option<&mut MatcherBudget<'_>>,
    ) -> io::Result<(Vec<Gitignore>, bool)> {
        validate_relative_directory(relative_directory)?;
        if let Some(budget) = budget.as_deref_mut() {
            budget.checkpoint()?;
        }
        let mut directory = self.ignore_root.try_clone()?;
        let mut prefix = PathBuf::new();
        let mut matchers = Vec::new();
        let mut ancestor_ignored = false;
        if self.gitignore_enabled {
            if let Some(exclude) = self.git_exclude(budget.as_deref_mut())? {
                matchers.push(exclude);
            }
            if let Some(matcher) = self.gitignore(&directory, &prefix, budget.as_deref_mut())? {
                matchers.push(matcher);
            }
        }
        let target = self.files_prefix.join(relative_directory);
        for component in target.components() {
            let std::path::Component::Normal(name) = component else {
                return Err(invalid_relative_path());
            };
            if let Some(budget) = budget.as_deref_mut() {
                budget.consume_component()?;
            }
            prefix.push(name);
            if is_ignored(&matchers, &self.ignore_root_path.join(&prefix), true) {
                ancestor_ignored = true;
                break;
            }
            directory = open_child_directory_nofollow(&directory, name)?;
            if self.gitignore_enabled
                && let Some(matcher) = self.gitignore(&directory, &prefix, budget.as_deref_mut())?
            {
                matchers.push(matcher);
            }
        }
        Ok((matchers, ancestor_ignored))
    }

    fn gitignore(
        &self,
        directory: &Dir,
        prefix: &Path,
        budget: Option<&mut MatcherBudget<'_>>,
    ) -> io::Result<Option<Gitignore>> {
        let matcher_root = self.ignore_root_path.join(prefix);
        self.matcher_from_file(
            directory,
            OsStr::new(".gitignore"),
            matcher_root.clone(),
            matcher_root.join(".gitignore"),
            budget,
        )
    }

    fn git_exclude(
        &self,
        mut budget: Option<&mut MatcherBudget<'_>>,
    ) -> io::Result<Option<Gitignore>> {
        if let Some(budget) = budget.as_deref_mut() {
            budget.consume_component()?;
        }
        let git = match open_child_directory_nofollow(&self.ignore_root, OsStr::new(".git")) {
            Ok(git) => git,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        if let Some(budget) = budget.as_deref_mut() {
            budget.consume_component()?;
        }
        let info = match open_child_directory_nofollow(&git, OsStr::new("info")) {
            Ok(info) => info,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        self.matcher_from_file(
            &info,
            OsStr::new("exclude"),
            self.ignore_root_path.clone(),
            self.ignore_root_path.join(".git/info/exclude"),
            budget,
        )
    }

    fn matcher_from_file(
        &self,
        directory: &Dir,
        name: &OsStr,
        matcher_root: PathBuf,
        source: PathBuf,
        mut budget: Option<&mut MatcherBudget<'_>>,
    ) -> io::Result<Option<Gitignore>> {
        if let Some(budget) = budget.as_deref_mut() {
            budget.checkpoint()?;
        }
        let metadata = match directory.symlink_metadata(name) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if metadata.is_symlink() || !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "ignore source must be a regular file",
            ));
        }
        if metadata.len() > MAX_IGNORE_FILE_BYTES as u64 {
            return Err(search_budget_exhausted());
        }
        let file = open_file_nofollow(directory, name)?;
        let mut reader = file.take((MAX_IGNORE_FILE_BYTES as u64).saturating_add(1));
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 8_192];
        loop {
            if let Some(budget) = budget.as_deref_mut() {
                budget.checkpoint()?;
            }
            let read = reader.read(&mut chunk)?;
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..read]);
            if bytes.len() > MAX_IGNORE_FILE_BYTES {
                return Err(search_budget_exhausted());
            }
        }
        let contents = String::from_utf8(bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let rules = contents.lines().count();
        if let Some(budget) = budget {
            budget.consume_matcher(contents.len(), rules)?;
        }
        let mut builder = GitignoreBuilder::new(matcher_root);
        for (index, line) in contents.lines().enumerate() {
            let line = if index == 0 {
                line.strip_prefix('\u{feff}').unwrap_or(line)
            } else {
                line
            };
            builder
                .add_line(Some(source.clone()), line)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        }
        builder
            .build()
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }
}

fn search_budget_exhausted() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "file search ignore budget was exhausted",
    )
}

fn is_ignored(matchers: &[Gitignore], path: &Path, is_directory: bool) -> bool {
    let mut ignored = false;
    for matcher in matchers {
        let matched = matcher.matched_path_or_any_parents(path, is_directory);
        if matched.is_ignore() {
            ignored = true;
        } else if matched.is_whitelist() {
            ignored = false;
        }
    }
    ignored
}

fn validate_relative_directory(path: &Path) -> io::Result<()> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
    {
        return Ok(());
    }
    Err(invalid_relative_path())
}

fn validate_relative_path(path: &Path) -> io::Result<()> {
    if !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
    {
        return Ok(());
    }
    Err(invalid_relative_path())
}

fn invalid_relative_path() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "tree path must be root-relative and normalized",
    )
}

#[cfg(unix)]
fn open_ambient_directory_nofollow(path: &Path) -> io::Result<Dir> {
    use std::os::unix::fs::OpenOptionsExt;

    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open("/")?;
    let mut directory = Dir::from_std_file(file);
    for component in path.components() {
        match component {
            std::path::Component::RootDir => {}
            std::path::Component::Normal(name) => {
                directory = open_child_directory_nofollow(&directory, name)?;
            }
            _ => return Err(invalid_relative_path()),
        }
    }
    Ok(directory)
}

#[cfg(not(unix))]
fn open_ambient_directory_nofollow(path: &Path) -> io::Result<Dir> {
    Dir::open_ambient_dir(path, cap_std::ambient_authority())
}

#[cfg(unix)]
fn open_child_directory_nofollow(parent: &Dir, name: &OsStr) -> io::Result<Dir> {
    use cap_std::fs::{OpenOptions, OpenOptionsExt};

    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW);
    let file = parent.open_with(name, &options)?;
    Ok(Dir::from_std_file(file.into_std()))
}

#[cfg(not(unix))]
fn open_child_directory_nofollow(parent: &Dir, name: &OsStr) -> io::Result<Dir> {
    if parent.symlink_metadata(name)?.is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "tree directory must not contain symlinked components",
        ));
    }
    parent.open_dir(name)
}

#[cfg(unix)]
fn open_file_nofollow(parent: &Dir, name: &OsStr) -> io::Result<cap_std::fs::File> {
    use cap_std::fs::{OpenOptions, OpenOptionsExt};

    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK);
    parent.open_with(name, &options)
}

#[cfg(not(unix))]
fn open_file_nofollow(parent: &Dir, name: &OsStr) -> io::Result<cap_std::fs::File> {
    if parent.symlink_metadata(name)?.is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "tree file must not be a symlink",
        ));
    }
    parent.open(name)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use super::{IgnorePolicy, MAX_IGNORE_FILE_BYTES};

    fn touch(path: impl AsRef<Path>) {
        fs::write(path, []).expect("write fixture");
    }

    #[test]
    fn honors_root_anchored_directory_glob_and_negated_rules_lazily() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join(".git")).expect("git marker");
        fs::create_dir(temp.path().join("build")).expect("build dir");
        fs::write(
            temp.path().join(".gitignore"),
            "/anchored.txt\n*.log\nbuild/*\n!build/keep.txt\n",
        )
        .expect("root ignore");
        touch(temp.path().join("anchored.txt"));
        touch(temp.path().join("visible.txt"));
        touch(temp.path().join("debug.log"));
        touch(temp.path().join("build/drop.txt"));
        touch(temp.path().join("build/keep.txt"));

        let policy = IgnorePolicy::new(temp.path().to_path_buf()).expect("policy");
        let root = policy
            .visible_children(Path::new(""))
            .expect("root children");
        assert!(root.contains(&PathBuf::from("build")));
        assert!(root.contains(&PathBuf::from("visible.txt")));
        assert!(!root.contains(&PathBuf::from("anchored.txt")));
        assert!(!root.contains(&PathBuf::from("debug.log")));

        assert_eq!(
            policy
                .visible_children(Path::new("build"))
                .expect("build children"),
            vec![PathBuf::from("build/keep.txt")]
        );
    }
    #[test]
    fn strips_utf8_bom_from_the_first_gitignore_rule() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join(".git")).expect("git marker");
        fs::write(temp.path().join(".gitignore"), "\u{feff}ignored\n").expect("root ignore");
        touch(temp.path().join("ignored"));
        touch(temp.path().join("visible"));
        let policy = IgnorePolicy::new(temp.path().to_path_buf()).expect("policy");

        assert_eq!(
            policy.visible_children(Path::new("")).expect("children"),
            vec![PathBuf::from("visible")]
        );
    }

    #[test]
    fn nested_rules_override_parent_rules() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join(".git")).expect("git marker");
        fs::create_dir(temp.path().join("nested")).expect("nested dir");
        fs::write(temp.path().join(".gitignore"), "*.tmp\n").expect("root ignore");
        fs::write(
            temp.path().join("nested/.gitignore"),
            "!important.tmp\n/local-only\n",
        )
        .expect("nested ignore");
        touch(temp.path().join("nested/drop.tmp"));
        touch(temp.path().join("nested/important.tmp"));
        touch(temp.path().join("nested/local-only"));

        let policy = IgnorePolicy::new(temp.path().to_path_buf()).expect("policy");
        let children = policy
            .visible_children(Path::new("nested"))
            .expect("children");
        assert!(children.contains(&PathBuf::from("nested/important.tmp")));
        assert!(!children.contains(&PathBuf::from("nested/drop.tmp")));
        assert!(!children.contains(&PathBuf::from("nested/local-only")));
    }

    #[test]
    fn does_not_read_gitignore_above_the_repository_root() {
        let temp = TempDir::new().expect("tempdir");
        fs::write(temp.path().join(".gitignore"), "*.txt\n").expect("parent ignore");
        let repository = temp.path().join("repository");
        fs::create_dir(&repository).expect("repository");
        fs::create_dir(repository.join(".git")).expect("git marker");
        touch(repository.join("visible.txt"));

        let policy = IgnorePolicy::new(repository).expect("policy");
        assert_eq!(
            policy.visible_children(Path::new("")).expect("children"),
            vec![PathBuf::from("visible.txt")]
        );
    }
    #[test]
    fn honors_repository_exclude_without_leaving_the_root_capability() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir_all(temp.path().join(".git/info")).expect("git info");
        fs::write(temp.path().join(".git/info/exclude"), "excluded\n").expect("exclude file");
        touch(temp.path().join("excluded"));
        touch(temp.path().join("visible"));
        let policy = IgnorePolicy::new(temp.path().to_path_buf()).expect("policy");

        assert_eq!(
            policy.visible_children(Path::new("")).expect("children"),
            vec![PathBuf::from("visible")]
        );
    }

    #[test]
    fn rejects_paths_outside_the_lazy_tree_root() {
        let temp = TempDir::new().expect("tempdir");
        let policy = IgnorePolicy::new(temp.path().to_path_buf()).expect("policy");
        assert!(policy.visible_children(Path::new("../outside")).is_err());
    }

    #[test]
    fn rejects_oversized_gitignore_files_without_unbounded_reads() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join(".git")).expect("git marker");
        let mut contents = b"visible\n".to_vec();
        contents.resize(MAX_IGNORE_FILE_BYTES.saturating_add(1), b'#');
        fs::write(temp.path().join(".gitignore"), contents).expect("oversized ignore");
        touch(temp.path().join("visible"));
        let policy = IgnorePolicy::new(temp.path().to_path_buf()).expect("policy");

        assert!(policy.visible_children(Path::new("")).is_err());
    }
    #[test]
    fn rejects_non_regular_gitignore_sources() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join(".git")).expect("git marker");
        fs::create_dir(temp.path().join(".gitignore")).expect("non-regular ignore");
        touch(temp.path().join("visible"));
        let policy = IgnorePolicy::new(temp.path().to_path_buf()).expect("policy");

        assert!(policy.visible_children(Path::new("")).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn rejects_directories_reached_through_an_intermediate_symlink() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("tempdir");
        let outside = TempDir::new().expect("outside");
        fs::create_dir_all(temp.path().join("a/b")).expect("inside directory");
        fs::create_dir(outside.path().join("b")).expect("outside directory");
        touch(outside.path().join("b/secret"));
        let policy = IgnorePolicy::new(temp.path().to_path_buf()).expect("policy");
        fs::remove_dir_all(temp.path().join("a")).expect("remove inside directory");
        symlink(outside.path(), temp.path().join("a")).expect("replace with symlink");

        assert!(policy.visible_children(Path::new("a/b")).is_err());
    }

    #[test]
    fn show_ignored_flags_entries_and_enters_ignored_directories() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join(".git")).expect("git marker");
        fs::write(
            temp.path().join(".gitignore"),
            b"skipped.txt\nskipped-dir/\n",
        )
        .expect("gitignore");
        touch(temp.path().join("kept"));
        touch(temp.path().join("skipped.txt"));
        fs::create_dir(temp.path().join("skipped-dir")).expect("ignored directory");
        touch(temp.path().join("skipped-dir/nested"));

        let hidden =
            IgnorePolicy::for_workspace(temp.path().to_path_buf(), temp.path().to_path_buf())
                .expect("policy");
        let shown = hidden.rescoped(true);

        assert_eq!(
            hidden.visible_children(Path::new("")).expect("children"),
            vec![PathBuf::from("kept")]
        );
        assert!(
            hidden
                .visible_children(Path::new("skipped-dir"))
                .expect("children")
                .is_empty()
        );

        let entries = shown.visible_entries(Path::new("")).expect("entries");
        let mut shown_children: Vec<PathBuf> =
            entries.iter().map(|entry| entry.path.clone()).collect();
        shown_children.sort_unstable();
        assert_eq!(
            shown_children,
            [
                PathBuf::from("kept"),
                PathBuf::from("skipped-dir"),
                PathBuf::from("skipped.txt"),
            ]
        );
        let ignored_flag = |path: &str| {
            entries
                .iter()
                .find(|entry| entry.path == Path::new(path))
                .map(|entry| entry.ignored)
        };
        assert_eq!(ignored_flag("skipped.txt"), Some(true));
        assert_eq!(ignored_flag("kept"), Some(false));
        assert_eq!(
            shown
                .visible_children(Path::new("skipped-dir"))
                .expect("children"),
            vec![PathBuf::from("skipped-dir/nested")]
        );
    }
}
