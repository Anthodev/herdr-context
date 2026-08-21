use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use herdr_context::files::ignore::{DefaultVisibilityPolicy, VisibilityPolicy};
use herdr_context::files::tree::{FilesTree, TreeNodeKind};
use herdr_context::vcs::{VcsEntryStatus, VcsStatusKind, VcsStatusSnapshot};
use tempfile::TempDir;

#[derive(Debug)]
struct IncludeAll;

impl VisibilityPolicy for IncludeAll {
    fn is_visible(&self, _relative_path: &Path) -> bool {
        true
    }
}

#[derive(Debug)]
struct RemoveDuringScan {
    victim: PathBuf,
}

impl VisibilityPolicy for RemoveDuringScan {
    fn is_visible(&self, relative_path: &Path) -> bool {
        if relative_path == Path::new("vanishes") {
            let _ = fs::remove_file(&self.victim);
        }
        true
    }
}
#[cfg(unix)]
#[derive(Debug)]
struct SwapRootDuringScan {
    root: PathBuf,
    moved: PathBuf,
    outside: PathBuf,
    swapped: std::sync::atomic::AtomicBool,
}

#[cfg(unix)]
impl VisibilityPolicy for SwapRootDuringScan {
    fn is_visible(&self, _relative_path: &Path) -> bool {
        use std::os::unix::fs::symlink;
        use std::sync::atomic::Ordering;

        if !self.swapped.swap(true, Ordering::AcqRel) {
            fs::rename(&self.root, &self.moved).expect("move root during scan");
            symlink(&self.outside, &self.root).expect("replace root with symlink");
        }
        true
    }
}

fn child_paths(tree: &FilesTree, directory: &Path) -> Vec<PathBuf> {
    tree.children(directory)
        .into_iter()
        .map(|node| node.path().to_path_buf())
        .collect()
}

#[test]
fn default_visibility_excludes_hidden_entries() {
    let temp = TempDir::new().expect("tempdir");
    fs::write(temp.path().join(".hidden"), []).expect("hidden fixture");
    fs::write(temp.path().join("visible"), []).expect("visible fixture");
    let mut tree = FilesTree::new(temp.path().to_path_buf()).expect("tree");

    tree.load_directory(Path::new("")).expect("root");

    assert_eq!(
        child_paths(&tree, Path::new("")),
        vec![PathBuf::from("visible")]
    );
}
#[test]
fn default_visibility_excludes_hidden_path_components() {
    let policy = DefaultVisibilityPolicy;

    for path in [
        Path::new(".hidden/child"),
        Path::new("parent/.hidden/child"),
    ] {
        assert!(!policy.is_visible(path));
    }
    assert!(policy.is_visible(Path::new("parent/visible")));
}

#[test]
fn default_visibility_excludes_hidden_virtual_status_rows() {
    let temp = TempDir::new().expect("tempdir");
    let mut tree = FilesTree::new(temp.path().to_path_buf()).expect("tree");
    let entries = [".hidden/deleted", "visible-deleted"]
        .into_iter()
        .map(|path| {
            VcsEntryStatus::new(
                PathBuf::from(path),
                None,
                VcsStatusKind::Deleted,
                None,
                Some(VcsStatusKind::Deleted),
            )
            .expect("status")
        })
        .collect();

    tree.merge_status(&VcsStatusSnapshot::new(entries, false))
        .expect("merge status");

    assert!(tree.node(Path::new(".hidden/deleted")).is_none());
    assert!(tree.node(Path::new("visible-deleted")).is_some());
}

#[test]
fn injected_visibility_policy_can_include_hidden_entries() {
    let temp = TempDir::new().expect("tempdir");
    fs::write(temp.path().join(".hidden"), []).expect("hidden fixture");
    let mut tree =
        FilesTree::with_visibility_policy(temp.path().to_path_buf(), Arc::new(IncludeAll), false)
            .expect("tree");

    tree.load_directory(Path::new("")).expect("root");

    assert_eq!(
        child_paths(&tree, Path::new("")),
        vec![PathBuf::from(".hidden")]
    );
}

#[test]
fn nested_tree_stays_lazy_root_relative_and_directory_first() {
    let temp = TempDir::new().expect("tempdir");
    fs::create_dir(temp.path().join("β-directory")).expect("directory");
    fs::write(temp.path().join("β-directory/é-child"), []).expect("nested file");
    fs::write(temp.path().join("a-file"), []).expect("root file");
    let mut tree = FilesTree::new(temp.path().to_path_buf()).expect("tree");

    tree.load_directory(Path::new("")).expect("root");

    assert_eq!(
        child_paths(&tree, Path::new("")),
        vec![PathBuf::from("β-directory"), PathBuf::from("a-file")]
    );
    assert!(tree.node(Path::new("β-directory/é-child")).is_none());

    tree.load_directory(Path::new("β-directory"))
        .expect("expanded directory");

    let child = tree
        .node(Path::new("β-directory/é-child"))
        .expect("nested child");
    assert_eq!(child.path(), Path::new("β-directory/é-child"));
    assert!(!child.path().is_absolute());
}

#[test]
fn disappearing_child_does_not_discard_readable_siblings() {
    let temp = TempDir::new().expect("tempdir");
    let victim = temp.path().join("vanishes");
    fs::write(&victim, []).expect("vanishing fixture");
    fs::write(temp.path().join("stable"), []).expect("stable fixture");
    let mut tree = FilesTree::with_visibility_policy(
        temp.path().to_path_buf(),
        Arc::new(RemoveDuringScan { victim }),
        false,
    )
    .expect("tree");

    tree.load_directory(Path::new(""))
        .expect("disappearing child is non-fatal");

    assert_eq!(
        child_paths(&tree, Path::new("")),
        vec![PathBuf::from("stable")]
    );
}

#[cfg(unix)]
#[test]
fn root_swap_cannot_redirect_entry_metadata_outside_the_tree() {
    let temp = TempDir::new().expect("tempdir");
    let outside = TempDir::new().expect("outside");
    let root = temp.path().join("root");
    let moved = temp.path().join("moved-root");
    fs::create_dir(&root).expect("root");
    fs::write(root.join("same-name"), []).expect("inside file");
    fs::create_dir(outside.path().join("same-name")).expect("outside directory");
    let mut tree = FilesTree::with_visibility_policy(
        root.clone(),
        Arc::new(SwapRootDuringScan {
            root,
            moved,
            outside: outside.path().to_path_buf(),
            swapped: std::sync::atomic::AtomicBool::new(false),
        }),
        false,
    )
    .expect("tree");

    tree.load_directory(Path::new("")).expect("root snapshot");

    assert_eq!(
        tree.node(Path::new("same-name"))
            .expect("inside row")
            .kind(),
        TreeNodeKind::File
    );
}

#[cfg(unix)]
#[test]
fn inaccessible_virtual_candidate_does_not_abort_other_vcs_statuses() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().expect("tempdir");
    let outside = TempDir::new().expect("outside");
    fs::write(temp.path().join("visible"), []).expect("visible file");
    symlink(outside.path(), temp.path().join("escape")).expect("escape symlink");
    let mut tree = FilesTree::new(temp.path().to_path_buf()).expect("tree");
    tree.load_directory(Path::new("")).expect("root");
    let entries = vec![
        VcsEntryStatus::new(
            PathBuf::from("escape/missing"),
            None,
            VcsStatusKind::Deleted,
            None,
            Some(VcsStatusKind::Deleted),
        )
        .expect("deleted status"),
        VcsEntryStatus::new(
            PathBuf::from("visible"),
            None,
            VcsStatusKind::Modified,
            None,
            Some(VcsStatusKind::Modified),
        )
        .expect("modified status"),
    ];

    tree.merge_status(&VcsStatusSnapshot::new(entries, false))
        .expect("entry-local failure is non-fatal");

    assert_eq!(
        tree.node(Path::new("visible"))
            .expect("visible row")
            .status(),
        Some(VcsStatusKind::Modified)
    );
    assert!(tree.node(Path::new("escape/missing")).is_none());
}

#[cfg(unix)]
#[test]
fn symlink_cycle_and_escape_are_visible_but_never_expandable() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().expect("tempdir");
    let outside = TempDir::new().expect("outside");
    fs::create_dir(temp.path().join("directory")).expect("directory");
    symlink(".", temp.path().join("directory/cycle")).expect("cycle symlink");
    symlink(outside.path(), temp.path().join("escape")).expect("escape symlink");
    let mut tree = FilesTree::new(temp.path().to_path_buf()).expect("tree");

    tree.load_directory(Path::new("")).expect("root");
    tree.load_directory(Path::new("directory"))
        .expect("directory children");

    for path in [Path::new("escape"), Path::new("directory/cycle")] {
        let node = tree.node(path).expect("symlink row");
        assert_eq!(node.kind(), TreeNodeKind::Symlink);
        assert!(!node.is_expandable());
        assert!(tree.load_directory(path).is_err());
    }
}
