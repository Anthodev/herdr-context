//! Lazy filesystem tree, ignore policy, and VCS refresh coordination.

pub mod ignore;
pub mod refresh;
pub mod tree;
use std::hash::{DefaultHasher, Hash, Hasher};

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::files::ignore::VisibilityPolicy;
use crate::vcs::{VcsError, VcsErrorKind, VcsStatusSnapshot};
use refresh::{RefreshCoordinator, RefreshResult};
use tree::FilesTree;

#[derive(Debug)]
pub(crate) struct StatusMergeInput {
    pub(crate) tree: FilesTree,
    pub(crate) workspace_prefix: PathBuf,
    pub(crate) tree_revision: u64,
}

#[derive(Debug)]
pub(crate) struct PreparedRefreshResult {
    generation: u64,
    tree_revision: u64,
    tree: Result<(FilesTree, bool), VcsError>,
    status_fingerprint: Option<u64>,
}

impl PreparedRefreshResult {
    pub(crate) fn prepare(
        generation: u64,
        input: StatusMergeInput,
        snapshot: Result<VcsStatusSnapshot, VcsError>,
    ) -> Self {
        let status_fingerprint = snapshot.as_ref().ok().map(snapshot_fingerprint);
        let tree = snapshot.and_then(|snapshot| {
            let stale = snapshot.is_stale();
            let mut tree = input.tree;
            tree.merge_workspace_status(&snapshot, &input.workspace_prefix)
                .map(|()| (tree, stale))
                .map_err(|error| {
                    VcsError::new(
                        VcsErrorKind::Io,
                        format!("cannot merge VCS status: {error}"),
                    )
                })
        });
        Self {
            generation,
            tree_revision: input.tree_revision,
            tree,
            status_fingerprint,
        }
    }

    pub(crate) const fn status_fingerprint(&self) -> Option<u64> {
        self.status_fingerprint
    }
}

fn snapshot_fingerprint(snapshot: &VcsStatusSnapshot) -> u64 {
    let mut hasher = DefaultHasher::new();
    snapshot.is_stale().hash(&mut hasher);
    for entry in snapshot.entries() {
        entry.path().hash(&mut hasher);
        entry.source_path().hash(&mut hasher);
        (entry.kind() as u8).hash(&mut hasher);
        entry.index_state().map(|kind| kind as u8).hash(&mut hasher);
        entry
            .worktree_state()
            .map(|kind| kind as u8)
            .hash(&mut hasher);
    }
    hasher.finish()
}

/// Files view state. Failed VCS refreshes never replace the current tree.
#[derive(Debug)]
pub struct FilesModel {
    tree: FilesTree,
    refresh: RefreshCoordinator,
    workspace_prefix: PathBuf,
    failure_notice: Option<String>,
    status_is_stale: bool,
    tree_revision: u64,
}

impl FilesModel {
    pub fn new(root: PathBuf) -> io::Result<Self> {
        Self::for_workspace(root.clone(), root)
    }

    pub fn with_visibility_policy(
        root: PathBuf,
        visibility: Arc<dyn VisibilityPolicy>,
    ) -> io::Result<Self> {
        Self::for_workspace_with_visibility(root.clone(), root, visibility)
    }

    pub fn for_workspace(files_root: PathBuf, workspace_root: PathBuf) -> io::Result<Self> {
        Self::build(files_root, workspace_root, None)
    }

    pub fn for_workspace_with_visibility(
        files_root: PathBuf,
        workspace_root: PathBuf,
        visibility: Arc<dyn VisibilityPolicy>,
    ) -> io::Result<Self> {
        Self::build(files_root, workspace_root, Some(visibility))
    }

    fn build(
        files_root: PathBuf,
        workspace_root: PathBuf,
        visibility: Option<Arc<dyn VisibilityPolicy>>,
    ) -> io::Result<Self> {
        let files_root = std::fs::canonicalize(files_root)?;
        let workspace_root = std::fs::canonicalize(workspace_root)?;
        let workspace_prefix = files_root
            .strip_prefix(&workspace_root)
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "files root must be inside the VCS workspace",
                )
            })?
            .to_path_buf();
        let tree = match visibility {
            Some(visibility) => {
                FilesTree::for_workspace_with_visibility(files_root, workspace_root, visibility)?
            }
            None => FilesTree::for_workspace(files_root, workspace_root)?,
        };
        Ok(Self {
            tree,
            refresh: RefreshCoordinator::new(),
            workspace_prefix,
            failure_notice: None,
            status_is_stale: false,
            tree_revision: 0,
        })
    }

    #[must_use]
    pub const fn tree(&self) -> &FilesTree {
        &self.tree
    }

    #[cfg(test)]
    pub(crate) const fn tree_mut(&mut self) -> &mut FilesTree {
        &mut self.tree
    }

    pub(crate) fn select(&mut self, path: &Path) -> bool {
        self.tree.select(path)
    }

    pub(crate) fn apply_directory(&mut self, snapshot: tree::DirectorySnapshot) {
        self.tree.apply_directory(snapshot);
        self.tree_revision = self.tree_revision.saturating_add(1);
    }

    #[must_use]
    pub fn failure_notice(&self) -> Option<&str> {
        self.failure_notice.as_deref()
    }

    #[must_use]
    pub const fn status_is_stale(&self) -> bool {
        self.status_is_stale
    }

    pub(crate) const fn mark_status_stale(&mut self) -> bool {
        if self.status_is_stale {
            return false;
        }
        self.status_is_stale = true;
        true
    }

    pub const fn request_refresh(&mut self) -> u64 {
        self.refresh.request()
    }

    /// Returns work only when this workspace has no status command in flight.
    pub const fn begin_refresh(&mut self) -> Option<u64> {
        self.refresh.start_next()
    }

    pub(crate) fn cancel_refresh_start(&mut self, generation: u64) -> bool {
        self.refresh.cancel_start(generation)
    }

    #[must_use]
    pub(crate) const fn refresh_is_running(&self) -> bool {
        self.refresh.is_running()
    }

    pub(crate) fn status_merge_input(&self) -> StatusMergeInput {
        StatusMergeInput {
            tree: self.tree.clone(),
            workspace_prefix: self.workspace_prefix.clone(),
            tree_revision: self.tree_revision,
        }
    }

    pub(crate) fn complete_prepared_refresh(&mut self, result: PreparedRefreshResult) -> bool {
        if !self.refresh.finish(result.generation) {
            return false;
        }
        if result.tree_revision != self.tree_revision {
            self.refresh.request();
            return false;
        }
        match result.tree {
            Ok((mut tree, stale)) => {
                let selected = self.tree.selection().map(Path::to_path_buf);
                tree.restore_selection_from(selected.as_deref());
                self.tree = tree;
                self.failure_notice = None;
                self.status_is_stale = stale;
                true
            }
            Err(error) => {
                self.failure_notice = Some(error.to_string());
                false
            }
        }
    }

    /// Applies a current successful result. Failures preserve all existing rows.
    pub fn complete_refresh(&mut self, result: RefreshResult) -> bool {
        if !self.refresh.finish(result.generation()) {
            return false;
        }
        match result.into_snapshot() {
            Ok(snapshot) => {
                let stale = snapshot.is_stale();
                match self
                    .tree
                    .merge_workspace_status(&snapshot, &self.workspace_prefix)
                {
                    Ok(()) => {
                        self.tree_revision = self.tree_revision.saturating_add(1);
                        self.failure_notice = None;
                        self.status_is_stale = stale;
                        true
                    }
                    Err(error) => {
                        self.failure_notice = Some(error.to_string());
                        false
                    }
                }
            }
            Err(error) => {
                self.failure_notice = Some(error.to_string());
                false
            }
        }
    }

    pub fn load_directory(&mut self, directory: &Path) -> io::Result<()> {
        self.tree.load_directory(directory)?;
        self.tree_revision = self.tree_revision.saturating_add(1);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use super::refresh::RefreshResult;
    use super::{FilesModel, PreparedRefreshResult};
    use crate::vcs::{VcsEntryStatus, VcsError, VcsErrorKind, VcsStatusKind, VcsStatusSnapshot};

    #[test]
    fn failed_and_superseded_refreshes_preserve_the_visible_tree() {
        let temp = TempDir::new().expect("tempdir");
        fs::write(temp.path().join("visible"), []).expect("file");
        let mut files = FilesModel::new(temp.path().to_path_buf()).expect("model");
        files.load_directory(Path::new("")).expect("root");

        files.request_refresh();
        let generation = files.begin_refresh().expect("generation");
        let deleted = VcsEntryStatus::new(
            PathBuf::from("missing"),
            None,
            VcsStatusKind::Deleted,
            Some(VcsStatusKind::Deleted),
            None,
        )
        .expect("status");
        assert!(files.complete_refresh(RefreshResult::new(
            generation,
            Ok(VcsStatusSnapshot::new(vec![deleted], false)),
        )));
        assert!(files.tree().node(Path::new("missing")).is_some());

        files.request_refresh();
        let generation = files.begin_refresh().expect("next generation");
        assert!(!files.complete_refresh(RefreshResult::new(
            generation,
            Err(VcsError::new(VcsErrorKind::CommandFailed, "Git failed")),
        )));
        assert!(files.tree().node(Path::new("visible")).is_some());
        assert!(files.tree().node(Path::new("missing")).is_some());
        assert_eq!(files.failure_notice(), Some("Git failed"));

        files.request_refresh();
        let generation = files.begin_refresh().expect("running generation");
        assert!(!files.complete_refresh(RefreshResult::new(
            generation + 1,
            Ok(VcsStatusSnapshot::new(Vec::new(), false)),
        )));
        assert!(files.tree().node(Path::new("missing")).is_some());
    }

    #[test]
    fn prepared_status_is_retried_after_a_concurrent_tree_change() {
        let temp = TempDir::new().expect("tempdir");
        fs::write(temp.path().join("initial"), []).expect("initial");
        let mut files = FilesModel::new(temp.path().to_path_buf()).expect("model");
        files.load_directory(Path::new("")).expect("root");
        files.request_refresh();
        let generation = files.begin_refresh().expect("generation");
        let input = files.status_merge_input();
        let deleted = VcsEntryStatus::new(
            PathBuf::from("missing"),
            None,
            VcsStatusKind::Deleted,
            Some(VcsStatusKind::Deleted),
            None,
        )
        .expect("status");
        let prepared = PreparedRefreshResult::prepare(
            generation,
            input,
            Ok(VcsStatusSnapshot::new(vec![deleted], false)),
        );
        fs::write(temp.path().join("added"), []).expect("added");
        files.load_directory(Path::new("")).expect("newer tree");

        assert!(!files.complete_prepared_refresh(prepared));
        assert!(files.tree().node(Path::new("missing")).is_none());
        assert!(files.begin_refresh().is_some(), "merge was not retried");
    }

    #[test]
    fn rebases_workspace_status_to_the_open_files_subtree() {
        let temp = TempDir::new().expect("tempdir");
        let opened = temp.path().join("src");
        fs::create_dir(&opened).expect("opened subtree");
        let mut files =
            FilesModel::for_workspace(opened, temp.path().to_path_buf()).expect("model");
        files.request_refresh();
        let generation = files.begin_refresh().expect("generation");
        let entries = vec![
            VcsEntryStatus::new(
                PathBuf::from("src/deleted.rs"),
                None,
                VcsStatusKind::Deleted,
                Some(VcsStatusKind::Deleted),
                None,
            )
            .expect("inside status"),
            VcsEntryStatus::new(
                PathBuf::from("outside.rs"),
                None,
                VcsStatusKind::Deleted,
                Some(VcsStatusKind::Deleted),
                None,
            )
            .expect("outside status"),
            VcsEntryStatus::new(
                PathBuf::from("src/renamed.rs"),
                Some(PathBuf::from("outside-old.rs")),
                VcsStatusKind::Renamed,
                Some(VcsStatusKind::Renamed),
                None,
            )
            .expect("rename status"),
        ];

        assert!(files.complete_refresh(RefreshResult::new(
            generation,
            Ok(VcsStatusSnapshot::new(entries, false)),
        )));

        assert!(files.tree().node(Path::new("deleted.rs")).is_some());
        assert!(files.tree().node(Path::new("outside.rs")).is_none());
        assert!(files.tree().node(Path::new("outside-old.rs")).is_none());
    }

    #[test]
    fn snapshot_marked_stale_replaces_status_and_marks_the_metadata() {
        let temp = TempDir::new().expect("tempdir");
        let mut files = FilesModel::new(temp.path().to_path_buf()).expect("model");
        files.request_refresh();
        let generation = files.begin_refresh().expect("generation");
        let deleted = VcsEntryStatus::new(
            PathBuf::from("missing"),
            None,
            VcsStatusKind::Deleted,
            Some(VcsStatusKind::Deleted),
            None,
        )
        .expect("status");

        assert!(files.complete_refresh(RefreshResult::new(
            generation,
            Ok(VcsStatusSnapshot::new(vec![deleted], true)),
        )));

        assert!(files.tree().node(Path::new("missing")).is_some());
        assert!(files.status_is_stale());
    }
}
