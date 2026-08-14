use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::vcs::{VcsStatusKind, VcsStatusSnapshot};

use super::ignore::{DefaultVisibilityPolicy, IgnorePolicy, VisibilityPolicy, VisibleEntryKind};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TreeNodeKind {
    Directory,
    File,
    Symlink,
    Virtual,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeNode {
    path: PathBuf,
    kind: TreeNodeKind,
    status: Option<VcsStatusKind>,
}

impl TreeNode {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn kind(&self) -> TreeNodeKind {
        self.kind
    }

    #[must_use]
    pub const fn status(&self) -> Option<VcsStatusKind> {
        self.status
    }

    #[must_use]
    pub fn is_expandable(&self) -> bool {
        self.kind == TreeNodeKind::Directory
    }
}

#[derive(Clone, Debug)]
pub(crate) struct DirectoryLoader {
    ignore: IgnorePolicy,
}

impl DirectoryLoader {
    pub(crate) fn load(&self, directory: PathBuf) -> io::Result<DirectorySnapshot> {
        let entries = self.ignore.visible_entries(&directory)?;
        let nodes = entries
            .into_iter()
            .map(|entry| TreeNode {
                path: entry.path,
                kind: match entry.kind {
                    VisibleEntryKind::Directory => TreeNodeKind::Directory,
                    VisibleEntryKind::File => TreeNodeKind::File,
                    VisibleEntryKind::Symlink => TreeNodeKind::Symlink,
                },
                status: None,
            })
            .collect();
        Ok(DirectorySnapshot { directory, nodes })
    }
}

#[derive(Debug)]
pub(crate) struct DirectorySnapshot {
    directory: PathBuf,
    nodes: Vec<TreeNode>,
}

#[derive(Clone, Debug)]
pub struct FilesTree {
    loader: DirectoryLoader,
    nodes: Arc<BTreeMap<PathBuf, TreeNode>>,
    statuses: Arc<BTreeMap<PathBuf, VcsStatusKind>>,
    children: Arc<BTreeMap<PathBuf, Vec<PathBuf>>>,
    selection: Option<PathBuf>,
}

impl FilesTree {
    pub fn new(root: PathBuf) -> io::Result<Self> {
        Self::with_visibility_policy(root, Arc::new(DefaultVisibilityPolicy))
    }

    pub(crate) fn for_workspace(root: PathBuf, workspace_root: PathBuf) -> io::Result<Self> {
        let ignore = IgnorePolicy::for_workspace(root, workspace_root)?;
        Ok(Self {
            loader: DirectoryLoader { ignore },
            nodes: Arc::new(BTreeMap::new()),
            statuses: Arc::new(BTreeMap::new()),
            children: Arc::new(BTreeMap::new()),
            selection: None,
        })
    }
    pub(crate) fn for_workspace_with_visibility(
        root: PathBuf,
        workspace_root: PathBuf,
        visibility: Arc<dyn VisibilityPolicy>,
    ) -> io::Result<Self> {
        let ignore = IgnorePolicy::for_workspace_with_visibility(root, workspace_root, visibility)?;
        Ok(Self {
            loader: DirectoryLoader { ignore },
            nodes: Arc::new(BTreeMap::new()),
            statuses: Arc::new(BTreeMap::new()),
            children: Arc::new(BTreeMap::new()),
            selection: None,
        })
    }

    pub fn with_visibility_policy(
        root: PathBuf,
        visibility: Arc<dyn VisibilityPolicy>,
    ) -> io::Result<Self> {
        let ignore = IgnorePolicy::with_visibility_policy(root, visibility)?;
        Ok(Self {
            loader: DirectoryLoader { ignore },
            nodes: Arc::new(BTreeMap::new()),
            statuses: Arc::new(BTreeMap::new()),
            children: Arc::new(BTreeMap::new()),
            selection: None,
        })
    }

    pub fn load_directory(&mut self, directory: &Path) -> io::Result<()> {
        let snapshot = self.loader.load(directory.to_path_buf())?;
        self.apply_directory(snapshot);
        Ok(())
    }

    pub(crate) fn directory_loader(&self) -> DirectoryLoader {
        self.loader.clone()
    }

    pub(crate) fn apply_directory(&mut self, mut snapshot: DirectorySnapshot) {
        for node in &mut snapshot.nodes {
            node.status = self.statuses.get(&node.path).copied();
        }
        let loaded_paths = snapshot
            .nodes
            .iter()
            .map(|node| node.path.clone())
            .collect::<BTreeSet<_>>();
        let removed = self
            .nodes
            .values()
            .filter(|node| {
                node.kind != TreeNodeKind::Virtual
                    && parent_path(&node.path) == snapshot.directory
                    && !loaded_paths.contains(&node.path)
            })
            .map(|node| node.path.clone())
            .collect::<BTreeSet<_>>();
        let replaced_directories = snapshot
            .nodes
            .iter()
            .filter(|node| {
                node.kind != TreeNodeKind::Directory
                    && self
                        .nodes
                        .get(&node.path)
                        .is_some_and(|prior| prior.kind == TreeNodeKind::Directory)
            })
            .map(|node| node.path.clone())
            .collect::<BTreeSet<_>>();
        let nodes = Arc::make_mut(&mut self.nodes);
        nodes.retain(|path, node| {
            if node.kind == TreeNodeKind::Virtual {
                return true;
            }
            !path.ancestors().skip(1).any(|ancestor| {
                removed.contains(ancestor) || replaced_directories.contains(ancestor)
            }) && (parent_path(path) != snapshot.directory || loaded_paths.contains(path))
        });
        nodes.extend(
            snapshot
                .nodes
                .into_iter()
                .map(|node| (node.path.clone(), node)),
        );
        self.rebuild_children();
        self.restore_selection();
    }

    pub fn merge_status(&mut self, snapshot: &VcsStatusSnapshot) -> io::Result<()> {
        self.merge_workspace_status(snapshot, Path::new(""))
    }

    pub fn merge_workspace_status(
        &mut self,
        snapshot: &VcsStatusSnapshot,
        files_prefix: &Path,
    ) -> io::Result<()> {
        let capacity = snapshot.entries().len().saturating_mul(2);
        let mut statuses = BTreeMap::new();
        let mut virtual_candidates = BTreeMap::new();
        for entry in snapshot.entries() {
            if let Ok(path) = entry.path().strip_prefix(files_prefix)
                && self.loader.ignore.is_visible(path)
            {
                let path = path.to_path_buf();
                insert_preferred_status(&mut statuses, path.clone(), entry.kind());
                if entry.kind() == VcsStatusKind::Deleted
                    || entry.index_state() == Some(VcsStatusKind::Deleted)
                    || entry.worktree_state() == Some(VcsStatusKind::Deleted)
                {
                    insert_preferred_status(&mut virtual_candidates, path, entry.kind());
                }
            }
            if let Some(source) = entry.source_path()
                && let Ok(path) = source.strip_prefix(files_prefix)
                && self.loader.ignore.is_visible(path)
            {
                insert_preferred_status(&mut virtual_candidates, path.to_path_buf(), entry.kind());
            }
        }

        let mut virtual_nodes = Vec::with_capacity(capacity.min(virtual_candidates.len()));
        for (path, status) in virtual_candidates {
            match self.loader.ignore.symlink_metadata(&path) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    virtual_nodes.push(TreeNode {
                        path,
                        kind: TreeNodeKind::Virtual,
                        status: Some(status),
                    });
                }
                Err(_) => continue,
            }
        }

        let nodes = Arc::make_mut(&mut self.nodes);
        nodes.retain(|_, node| node.kind != TreeNodeKind::Virtual);
        for node in nodes.values_mut() {
            node.status = statuses.get(&node.path).copied();
        }
        nodes.extend(
            virtual_nodes
                .into_iter()
                .map(|node| (node.path.clone(), node)),
        );
        self.statuses = Arc::new(statuses);
        self.rebuild_children();
        self.restore_selection();
        Ok(())
    }

    #[must_use]
    pub fn node(&self, path: &Path) -> Option<&TreeNode> {
        self.nodes.get(path)
    }

    pub fn select(&mut self, path: &Path) -> bool {
        if self.nodes.contains_key(path) {
            self.selection = Some(path.to_path_buf());
            true
        } else {
            false
        }
    }

    #[must_use]
    pub fn selection(&self) -> Option<&Path> {
        self.selection.as_deref()
    }

    pub(crate) fn restore_selection_from(&mut self, selected: Option<&Path>) {
        self.selection = selected.map(Path::to_path_buf);
        self.restore_selection();
    }

    #[must_use]
    pub fn children(&self, directory: &Path) -> Vec<&TreeNode> {
        self.children
            .get(directory)
            .into_iter()
            .flatten()
            .filter_map(|path| self.nodes.get(path))
            .collect()
    }

    #[must_use]
    pub(crate) fn display_parent_of(&self, path: &Path) -> Option<&Path> {
        self.nodes.get(path).map(|node| self.display_parent(node))
    }

    fn display_parent<'a>(&'a self, node: &'a TreeNode) -> &'a Path {
        if node.kind != TreeNodeKind::Virtual {
            return parent_path(&node.path);
        }
        node.path
            .ancestors()
            .skip(1)
            .find(|ancestor| {
                ancestor.as_os_str().is_empty()
                    || self
                        .nodes
                        .get(*ancestor)
                        .is_some_and(|parent| parent.kind == TreeNodeKind::Directory)
            })
            .unwrap_or_else(|| Path::new(""))
    }

    fn rebuild_children(&mut self) {
        let mut children = BTreeMap::<PathBuf, Vec<PathBuf>>::new();
        for node in self.nodes.values() {
            children
                .entry(self.display_parent(node).to_path_buf())
                .or_default()
                .push(node.path.clone());
        }
        for paths in children.values_mut() {
            paths.sort_unstable_by(|left, right| {
                let left = self.nodes.get(left).expect("indexed tree node");
                let right = self.nodes.get(right).expect("indexed tree node");
                node_order(left)
                    .cmp(&node_order(right))
                    .then_with(|| left.path.cmp(&right.path))
            });
        }
        self.children = Arc::new(children);
    }

    fn restore_selection(&mut self) {
        let Some(selected) = self.selection.clone() else {
            self.selection = self.ordered_first_path();
            return;
        };
        if self.nodes.contains_key(&selected) {
            return;
        }
        self.selection = selected
            .ancestors()
            .skip(1)
            .find(|ancestor| self.nodes.contains_key(*ancestor))
            .map(Path::to_path_buf)
            .or_else(|| self.ordered_first_path());
    }

    fn ordered_first_path(&self) -> Option<PathBuf> {
        self.children
            .get(Path::new(""))
            .and_then(|children| children.first())
            .cloned()
    }
}

fn parent_path(path: &Path) -> &Path {
    path.parent().unwrap_or_else(|| Path::new(""))
}

fn insert_preferred_status(
    statuses: &mut BTreeMap<PathBuf, VcsStatusKind>,
    path: PathBuf,
    status: VcsStatusKind,
) {
    statuses
        .entry(path)
        .and_modify(|current| {
            if status_order(status) < status_order(*current) {
                *current = status;
            }
        })
        .or_insert(status);
}

const fn status_order(status: VcsStatusKind) -> u8 {
    match status {
        VcsStatusKind::Conflicted => 0,
        VcsStatusKind::Renamed => 1,
        VcsStatusKind::Copied => 2,
        VcsStatusKind::TypeChanged => 3,
        VcsStatusKind::Deleted => 4,
        VcsStatusKind::Added => 5,
        VcsStatusKind::Modified => 6,
        VcsStatusKind::Untracked => 7,
    }
}

const fn node_order(node: &TreeNode) -> u8 {
    if matches!(node.kind, TreeNodeKind::Directory) {
        0
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use super::{FilesTree, TreeNodeKind};
    use crate::vcs::{VcsEntryStatus, VcsStatusKind, VcsStatusSnapshot};

    fn status(path: &str, source: Option<&str>, kind: VcsStatusKind) -> VcsEntryStatus {
        VcsEntryStatus::new(
            PathBuf::from(path),
            source.map(PathBuf::from),
            kind,
            None,
            Some(kind),
        )
        .expect("status")
    }

    #[test]
    fn injects_non_expandable_missing_and_moved_source_nodes() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join("src")).expect("src");
        fs::write(temp.path().join("src/current.rs"), []).expect("current");
        fs::write(temp.path().join("src/copy.rs"), []).expect("copy");
        let mut tree = FilesTree::new(temp.path().to_path_buf()).expect("tree");
        tree.load_directory(Path::new("")).expect("root");
        tree.load_directory(Path::new("src")).expect("src");

        tree.merge_status(&VcsStatusSnapshot::new(
            vec![
                status("src/deleted.rs", None, VcsStatusKind::Deleted),
                status("src/current.rs", Some("src/old.rs"), VcsStatusKind::Renamed),
                status(
                    "src/copy.rs",
                    Some("src/missing-copy-source.rs"),
                    VcsStatusKind::Copied,
                ),
            ],
            false,
        ))
        .expect("merge status");

        let deleted = tree
            .node(Path::new("src/deleted.rs"))
            .expect("deleted node");
        assert_eq!(deleted.kind(), TreeNodeKind::Virtual);
        assert!(!deleted.is_expandable());
        assert_eq!(deleted.status(), Some(VcsStatusKind::Deleted));
        let source = tree.node(Path::new("src/old.rs")).expect("rename source");
        assert_eq!(source.kind(), TreeNodeKind::Virtual);
        assert_eq!(source.status(), Some(VcsStatusKind::Renamed));
        let copy_source = tree
            .node(Path::new("src/missing-copy-source.rs"))
            .expect("copy source");
        assert_eq!(copy_source.kind(), TreeNodeKind::Virtual);
        assert_eq!(copy_source.status(), Some(VcsStatusKind::Copied));
        assert_eq!(
            tree.node(Path::new("src/current.rs"))
                .expect("target")
                .status(),
            Some(VcsStatusKind::Renamed)
        );
    }

    #[test]
    fn keeps_selection_by_stable_path_and_orders_directories_first() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join("z-dir")).expect("dir");
        fs::write(temp.path().join("a-file"), []).expect("file");
        let mut tree = FilesTree::new(temp.path().to_path_buf()).expect("tree");
        tree.load_directory(Path::new("")).expect("root");
        assert!(tree.select(Path::new("a-file")));

        tree.merge_status(&VcsStatusSnapshot::new(
            vec![status("missing", None, VcsStatusKind::Deleted)],
            false,
        ))
        .expect("merge status");

        assert_eq!(tree.selection(), Some(Path::new("a-file")));
        assert_eq!(
            tree.children(Path::new(""))
                .iter()
                .map(|node| node.path())
                .collect::<Vec<_>>(),
            vec![
                Path::new("z-dir"),
                Path::new("a-file"),
                Path::new("missing")
            ]
        );
    }

    #[test]
    fn removes_loaded_descendants_when_their_parent_disappears() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join("removed")).expect("directory");
        fs::write(temp.path().join("removed/child"), []).expect("child");
        let mut tree = FilesTree::new(temp.path().to_path_buf()).expect("tree");
        tree.load_directory(Path::new("")).expect("root");
        tree.load_directory(Path::new("removed"))
            .expect("expanded directory");
        fs::remove_dir_all(temp.path().join("removed")).expect("remove directory");

        tree.load_directory(Path::new("")).expect("reload root");

        assert!(tree.node(Path::new("removed")).is_none());
        assert!(tree.node(Path::new("removed/child")).is_none());
    }

    #[test]
    fn non_missing_metadata_errors_skip_virtual_nodes() {
        let temp = TempDir::new().expect("tempdir");
        fs::write(temp.path().join("blocked"), []).expect("blocking file");
        fs::write(temp.path().join("visible"), []).expect("visible");
        let mut tree = FilesTree::new(temp.path().to_path_buf()).expect("tree");
        tree.load_directory(Path::new("")).expect("root");

        let result = tree.merge_status(&VcsStatusSnapshot::new(
            vec![status("blocked/child", None, VcsStatusKind::Deleted)],
            false,
        ));

        assert!(result.is_ok());
        assert!(tree.node(Path::new("blocked/child")).is_none());
        assert!(tree.node(Path::new("visible")).is_some());
    }

    #[test]
    fn replaces_prior_virtual_nodes_deterministically() {
        let temp = TempDir::new().expect("tempdir");
        let mut tree = FilesTree::new(temp.path().to_path_buf()).expect("tree");
        tree.merge_status(&VcsStatusSnapshot::new(
            vec![status("old-missing", None, VcsStatusKind::Deleted)],
            false,
        ))
        .expect("merge old status");
        assert!(tree.select(Path::new("old-missing")));

        tree.merge_status(&VcsStatusSnapshot::new(
            vec![status("new-missing", None, VcsStatusKind::Deleted)],
            false,
        ))
        .expect("merge new status");

        assert!(tree.node(Path::new("old-missing")).is_none());
        assert!(tree.node(Path::new("new-missing")).is_some());
        assert_eq!(tree.selection(), Some(Path::new("new-missing")));
    }

    #[test]
    fn applies_cached_status_when_a_directory_is_loaded_later() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join("src")).expect("src");
        fs::write(temp.path().join("src/modified.rs"), []).expect("modified");
        let mut tree = FilesTree::new(temp.path().to_path_buf()).expect("tree");
        tree.load_directory(Path::new("")).expect("root");
        tree.merge_status(&VcsStatusSnapshot::new(
            vec![status("src/modified.rs", None, VcsStatusKind::Modified)],
            false,
        ))
        .expect("merge status");

        tree.load_directory(Path::new("src")).expect("load src");

        assert_eq!(
            tree.node(Path::new("src/modified.rs"))
                .expect("modified node")
                .status(),
            Some(VcsStatusKind::Modified)
        );
    }

    #[test]
    fn keeps_virtual_nodes_across_identical_snapshots() {
        let temp = TempDir::new().expect("tempdir");
        let mut tree = FilesTree::new(temp.path().to_path_buf()).expect("tree");
        let snapshot =
            VcsStatusSnapshot::new(vec![status("missing", None, VcsStatusKind::Deleted)], false);

        tree.merge_status(&snapshot).expect("first merge");
        tree.merge_status(&snapshot).expect("second merge");

        let missing = tree.node(Path::new("missing")).expect("virtual node");
        assert_eq!(missing.kind(), TreeNodeKind::Virtual);
        assert_eq!(missing.status(), Some(VcsStatusKind::Deleted));
    }

    #[test]
    fn does_not_mark_existing_copy_or_rename_sources_as_changed() {
        let temp = TempDir::new().expect("tempdir");
        fs::write(temp.path().join("copy-source"), []).expect("copy source");
        fs::write(temp.path().join("copy-target"), []).expect("copy target");
        fs::write(temp.path().join("recreated-old-name"), []).expect("recreated source");
        fs::write(temp.path().join("renamed-target"), []).expect("rename target");
        let mut tree = FilesTree::new(temp.path().to_path_buf()).expect("tree");
        tree.load_directory(Path::new("")).expect("root");

        tree.merge_status(&VcsStatusSnapshot::new(
            vec![
                status("copy-target", Some("copy-source"), VcsStatusKind::Copied),
                status(
                    "renamed-target",
                    Some("recreated-old-name"),
                    VcsStatusKind::Renamed,
                ),
            ],
            false,
        ))
        .expect("merge status");

        assert_eq!(
            tree.node(Path::new("copy-source"))
                .expect("copy source")
                .status(),
            None
        );
        assert_eq!(
            tree.node(Path::new("recreated-old-name"))
                .expect("rename source")
                .status(),
            None
        );
        assert_eq!(
            tree.node(Path::new("copy-target"))
                .expect("copy target")
                .status(),
            Some(VcsStatusKind::Copied)
        );
        assert_eq!(
            tree.node(Path::new("renamed-target"))
                .expect("rename target")
                .status(),
            Some(VcsStatusKind::Renamed)
        );
    }

    #[test]
    fn exposes_deep_virtual_nodes_at_the_nearest_loaded_ancestor() {
        let temp = TempDir::new().expect("tempdir");
        let mut tree = FilesTree::new(temp.path().to_path_buf()).expect("tree");

        tree.merge_status(&VcsStatusSnapshot::new(
            vec![status(
                "removed/nested/file.rs",
                None,
                VcsStatusKind::Deleted,
            )],
            false,
        ))
        .expect("merge status");

        assert_eq!(
            tree.children(Path::new(""))
                .iter()
                .map(|node| (node.path(), node.kind()))
                .collect::<Vec<_>>(),
            vec![(Path::new("removed/nested/file.rs"), TreeNodeKind::Virtual)]
        );
        assert!(
            !tree
                .node(Path::new("removed/nested/file.rs"))
                .expect("virtual row")
                .is_expandable()
        );
    }

    #[test]
    fn reparents_virtual_nodes_when_a_closer_ancestor_is_loaded() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir_all(temp.path().join("src/nested")).expect("nested");
        let mut tree = FilesTree::new(temp.path().to_path_buf()).expect("tree");
        tree.load_directory(Path::new("")).expect("root");
        tree.merge_status(&VcsStatusSnapshot::new(
            vec![status(
                "src/nested/missing.rs",
                None,
                VcsStatusKind::Deleted,
            )],
            false,
        ))
        .expect("merge status");

        assert_eq!(
            tree.children(Path::new("src"))
                .iter()
                .map(|node| node.path())
                .collect::<Vec<_>>(),
            vec![Path::new("src/nested/missing.rs")]
        );

        tree.load_directory(Path::new("src")).expect("load src");

        assert_eq!(
            tree.children(Path::new("src"))
                .iter()
                .map(|node| node.path())
                .collect::<Vec<_>>(),
            vec![Path::new("src/nested")]
        );
        assert_eq!(
            tree.children(Path::new("src/nested"))
                .iter()
                .map(|node| node.path())
                .collect::<Vec<_>>(),
            vec![Path::new("src/nested/missing.rs")]
        );
    }

    #[test]
    fn reparents_virtual_nodes_when_their_loaded_ancestor_disappears() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join("removed")).expect("removed");
        let mut tree = FilesTree::new(temp.path().to_path_buf()).expect("tree");
        tree.load_directory(Path::new("")).expect("root");
        tree.merge_status(&VcsStatusSnapshot::new(
            vec![status("removed/missing.rs", None, VcsStatusKind::Deleted)],
            false,
        ))
        .expect("merge status");
        assert_eq!(
            tree.children(Path::new("removed"))
                .iter()
                .map(|node| node.path())
                .collect::<Vec<_>>(),
            vec![Path::new("removed/missing.rs")]
        );

        fs::remove_dir(temp.path().join("removed")).expect("remove ancestor");
        tree.load_directory(Path::new("")).expect("reload root");

        assert_eq!(
            tree.children(Path::new(""))
                .iter()
                .map(|node| node.path())
                .collect::<Vec<_>>(),
            vec![Path::new("removed/missing.rs")]
        );
    }

    #[test]
    fn coalesces_duplicate_paths_with_stable_status_precedence() {
        let temp = TempDir::new().expect("tempdir");
        fs::write(temp.path().join("recreated"), []).expect("file");
        let mut tree = FilesTree::new(temp.path().to_path_buf()).expect("tree");
        tree.load_directory(Path::new("")).expect("root");

        for entries in [
            vec![
                status("recreated", None, VcsStatusKind::Deleted),
                status("recreated", None, VcsStatusKind::Untracked),
            ],
            vec![
                status("recreated", None, VcsStatusKind::Untracked),
                status("recreated", None, VcsStatusKind::Deleted),
            ],
        ] {
            tree.merge_status(&VcsStatusSnapshot::new(entries, false))
                .expect("merge duplicate statuses");
            assert_eq!(
                tree.node(Path::new("recreated")).expect("node").status(),
                Some(VcsStatusKind::Deleted)
            );
        }
    }

    #[test]
    fn replaces_a_cached_physical_node_with_a_virtual_deleted_row() {
        let temp = TempDir::new().expect("tempdir");
        fs::write(temp.path().join("tracked"), []).expect("tracked file");
        let mut tree = FilesTree::new(temp.path().to_path_buf()).expect("tree");
        tree.load_directory(Path::new("")).expect("root");
        fs::remove_file(temp.path().join("tracked")).expect("remove tracked file");

        tree.merge_status(&VcsStatusSnapshot::new(
            vec![status("tracked", None, VcsStatusKind::Deleted)],
            false,
        ))
        .expect("merge deletion");

        assert_eq!(
            tree.node(Path::new("tracked")).expect("virtual row").kind(),
            TreeNodeKind::Virtual
        );
        tree.load_directory(Path::new("")).expect("reload root");
        assert_eq!(
            tree.node(Path::new("tracked"))
                .expect("retained row")
                .kind(),
            TreeNodeKind::Virtual
        );
    }

    #[test]
    fn virtualizes_missing_targets_with_detailed_deleted_states() {
        let temp = TempDir::new().expect("tempdir");
        fs::write(temp.path().join("source"), []).expect("rename source");
        let mut tree = FilesTree::new(temp.path().to_path_buf()).expect("tree");
        tree.load_directory(Path::new("")).expect("root");
        let renamed = VcsEntryStatus::new(
            PathBuf::from("target"),
            Some(PathBuf::from("source")),
            VcsStatusKind::Renamed,
            Some(VcsStatusKind::Renamed),
            Some(VcsStatusKind::Deleted),
        )
        .expect("renamed status");
        let conflicted = VcsEntryStatus::new(
            PathBuf::from("conflicted"),
            None,
            VcsStatusKind::Conflicted,
            Some(VcsStatusKind::Deleted),
            Some(VcsStatusKind::Modified),
        )
        .expect("conflicted status");

        tree.merge_status(&VcsStatusSnapshot::new(vec![renamed, conflicted], false))
            .expect("merge status");

        for (path, status) in [
            ("target", VcsStatusKind::Renamed),
            ("conflicted", VcsStatusKind::Conflicted),
        ] {
            let node = tree.node(Path::new(path)).expect("virtual target");
            assert_eq!(node.kind(), TreeNodeKind::Virtual);
            assert_eq!(node.status(), Some(status));
        }
    }

    #[test]
    fn removes_loaded_descendants_when_a_directory_becomes_a_file() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join("changed")).expect("directory");
        fs::write(temp.path().join("changed/child"), []).expect("child");
        let mut tree = FilesTree::new(temp.path().to_path_buf()).expect("tree");
        tree.load_directory(Path::new("")).expect("root");
        tree.load_directory(Path::new("changed"))
            .expect("directory");
        fs::remove_dir_all(temp.path().join("changed")).expect("remove directory");
        fs::write(temp.path().join("changed"), []).expect("replacement file");

        tree.load_directory(Path::new("")).expect("reload root");

        assert_eq!(
            tree.node(Path::new("changed")).expect("replacement").kind(),
            TreeNodeKind::File
        );
        assert!(tree.node(Path::new("changed/child")).is_none());
    }
}
