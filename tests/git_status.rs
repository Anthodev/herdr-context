use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
#[cfg(unix)]
use std::thread;
use std::time::Duration;

use herdr_context::files::tree::FilesTree;
use herdr_context::vcs::git::GitService;
use herdr_context::vcs::{VcsService, VcsStatusKind};
use tempfile::TempDir;

fn git(root: &Path, arguments: &[&str]) {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(root)
        .output()
        .expect("run Git fixture command");
    assert!(
        output.status.success(),
        "git {arguments:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn repository() -> TempDir {
    let temp = TempDir::new().expect("tempdir");
    git(temp.path(), &["init", "--quiet"]);
    git(temp.path(), &["config", "user.name", "Test"]);
    git(
        temp.path(),
        &["config", "user.email", "test@example.invalid"],
    );
    temp
}

#[test]
fn real_worktree_produces_normalized_modified_deleted_renamed_and_untracked_entries() {
    let repository = repository();
    fs::write(repository.path().join("modified.txt"), "before").expect("modified fixture");
    fs::write(repository.path().join("deleted.txt"), "before").expect("deleted fixture");
    fs::write(repository.path().join("old name.txt"), "before").expect("rename fixture");
    git(repository.path(), &["add", "."]);
    git(repository.path(), &["commit", "--quiet", "-m", "fixture"]);

    fs::write(repository.path().join("modified.txt"), "after").expect("modify");
    fs::remove_file(repository.path().join("deleted.txt")).expect("delete");
    git(
        repository.path(),
        &["mv", "old name.txt", "renommé file.txt"],
    );
    fs::write(repository.path().join("untracked space.txt"), "new").expect("untracked");

    let mut service = GitService::new(Duration::from_secs(5));
    let workspace = service
        .detect(repository.path())
        .expect("detect")
        .expect("Git workspace");
    let snapshot = service.refresh_status(&workspace).expect("status");

    let find = |path: &str| {
        snapshot
            .entries()
            .iter()
            .find(|entry| entry.path() == Path::new(path))
            .unwrap_or_else(|| panic!("missing status for {path}"))
    };
    assert_eq!(find("modified.txt").kind(), VcsStatusKind::Modified);
    assert_eq!(find("deleted.txt").kind(), VcsStatusKind::Deleted);
    let renamed = find("renommé file.txt");
    assert_eq!(renamed.kind(), VcsStatusKind::Renamed);
    assert_eq!(renamed.source_path(), Some(Path::new("old name.txt")));
    assert_eq!(find("untracked space.txt").kind(), VcsStatusKind::Untracked);
}

#[test]
fn deleted_descendant_marks_present_directories_modified() {
    let repository = repository();
    fs::create_dir_all(repository.path().join("src/nested")).expect("directories");
    fs::write(repository.path().join("src/nested/deleted.rs"), "before").expect("fixture");
    git(repository.path(), &["add", "."]);
    git(repository.path(), &["commit", "--quiet", "-m", "fixture"]);
    fs::remove_file(repository.path().join("src/nested/deleted.rs")).expect("delete");

    let mut service = GitService::new(Duration::from_secs(5));
    let workspace = service
        .detect(repository.path())
        .expect("detect")
        .expect("Git workspace");
    let snapshot = service.refresh_status(&workspace).expect("status");
    let mut tree = FilesTree::new(repository.path().to_path_buf()).expect("tree");
    tree.load_directory(Path::new("")).expect("root");
    tree.merge_status(&snapshot).expect("status overlay");

    assert_eq!(
        tree.node(Path::new("src")).expect("src").status(),
        Some(VcsStatusKind::Modified)
    );
    tree.load_directory(Path::new("src")).expect("src");
    assert_eq!(
        tree.node(Path::new("src/nested")).expect("nested").status(),
        Some(VcsStatusKind::Modified)
    );
    assert_eq!(
        tree.node(Path::new("src/nested/deleted.rs"))
            .expect("deleted")
            .status(),
        Some(VcsStatusKind::Deleted)
    );
}

#[test]
fn added_descendant_marks_present_directories_modified_not_added() {
    let repository = repository();
    fs::create_dir_all(repository.path().join("src/nested")).expect("directories");
    fs::write(repository.path().join("src/nested/tracked.rs"), "before").expect("fixture");
    git(repository.path(), &["add", "."]);
    git(repository.path(), &["commit", "--quiet", "-m", "fixture"]);
    fs::write(repository.path().join("src/nested/added.rs"), "new").expect("added file");
    git(repository.path(), &["add", "src/nested/added.rs"]);

    let mut service = GitService::new(Duration::from_secs(5));
    let workspace = service
        .detect(repository.path())
        .expect("detect")
        .expect("Git workspace");
    let snapshot = service.refresh_status(&workspace).expect("status");
    let mut tree = FilesTree::new(repository.path().to_path_buf()).expect("tree");
    tree.load_directory(Path::new("")).expect("root");
    tree.merge_status(&snapshot).expect("status overlay");

    assert_eq!(
        tree.node(Path::new("src")).expect("src").status(),
        Some(VcsStatusKind::Modified)
    );
    tree.load_directory(Path::new("src")).expect("src");
    tree.load_directory(Path::new("src/nested"))
        .expect("nested");
    assert_eq!(
        tree.node(Path::new("src/nested")).expect("nested").status(),
        Some(VcsStatusKind::Modified)
    );
    assert_eq!(
        tree.node(Path::new("src/nested/added.rs"))
            .expect("added row")
            .status(),
        Some(VcsStatusKind::Added)
    );
}

#[test]
fn mixed_non_conflicted_descendants_aggregate_as_modified() {
    let repository = repository();
    fs::create_dir_all(repository.path().join("src/nested/untracked-dir")).expect("directories");
    for name in ["src/modified.rs", "src/nested/moved-source.rs"] {
        fs::write(repository.path().join(name), "before").expect("tracked fixture");
    }
    git(repository.path(), &["add", "."]);
    git(repository.path(), &["commit", "--quiet", "-m", "fixture"]);

    fs::write(repository.path().join("src/modified.rs"), "after").expect("modify");
    git(
        repository.path(),
        &["mv", "src/nested/moved-source.rs", "src/nested/renamed.rs"],
    );
    fs::write(
        repository.path().join("src/nested/untracked-dir/fresh.rs"),
        "new",
    )
    .expect("untracked file");

    let mut service = GitService::new(Duration::from_secs(5));
    let workspace = service
        .detect(repository.path())
        .expect("detect")
        .expect("Git workspace");
    let snapshot = service.refresh_status(&workspace).expect("status");
    let mut tree = FilesTree::new(repository.path().to_path_buf()).expect("tree");
    tree.load_directory(Path::new("")).expect("root");
    tree.merge_status(&snapshot).expect("status overlay");

    tree.load_directory(Path::new("src")).expect("src");
    tree.load_directory(Path::new("src/nested"))
        .expect("nested");
    tree.load_directory(Path::new("src/nested/untracked-dir"))
        .expect("untracked directory");
    for directory in ["src", "src/nested", "src/nested/untracked-dir"] {
        assert_eq!(
            tree.node(Path::new(directory))
                .unwrap_or_else(|| panic!("{directory} node"))
                .status(),
            Some(VcsStatusKind::Modified),
            "{directory} aggregates as Modified"
        );
    }
    assert_eq!(
        tree.node(Path::new("src/modified.rs"))
            .expect("modified row")
            .status(),
        Some(VcsStatusKind::Modified)
    );
    assert_eq!(
        tree.node(Path::new("src/nested/renamed.rs"))
            .expect("renamed row")
            .status(),
        Some(VcsStatusKind::Renamed)
    );
    assert_eq!(
        tree.node(Path::new("src/nested/untracked-dir/fresh.rs"))
            .expect("untracked row")
            .status(),
        Some(VcsStatusKind::Untracked)
    );
}

#[test]
fn jujutsu_marker_prevents_git_from_becoming_authoritative() {
    let repository = repository();
    fs::create_dir_all(repository.path().join(".jj/repo")).expect("jj repo");
    fs::create_dir_all(repository.path().join(".jj/working_copy")).expect("jj working copy");

    let service = GitService::new(Duration::from_secs(5));
    assert!(service.detect(repository.path()).expect("detect").is_none());
}

#[test]
fn repository_core_worktree_cannot_redirect_status() {
    let repository = repository();
    let other = TempDir::new().expect("other worktree");
    fs::write(repository.path().join("inside"), "tracked").expect("inside");
    git(repository.path(), &["add", "inside"]);
    git(repository.path(), &["commit", "--quiet", "-m", "fixture"]);
    fs::write(other.path().join("outside"), "untracked").expect("outside");
    git(
        repository.path(),
        &[
            "config",
            "core.worktree",
            other.path().to_str().expect("UTF-8 temp path"),
        ],
    );

    let mut service = GitService::new(Duration::from_secs(5));
    let workspace = service
        .detect(repository.path())
        .expect("detect")
        .expect("Git workspace");
    let snapshot = service.refresh_status(&workspace).expect("status");

    assert!(snapshot.entries().is_empty());
}

#[cfg(unix)]
#[test]
fn repository_content_filters_are_disabled_during_status() {
    let repository = repository();
    let tracked = repository.path().join("tracked");
    fs::write(&tracked, "same").expect("tracked");
    git(repository.path(), &["add", "tracked"]);
    git(repository.path(), &["commit", "--quiet", "-m", "fixture"]);

    let marker = repository.path().join("filter-ran");
    let filter = repository.path().join("filter.sh");
    fs::write(
        &filter,
        format!("#!/bin/sh\ntouch '{}'\ncat\n", marker.display()),
    )
    .expect("filter script");
    let mut permissions = fs::metadata(&filter).expect("metadata").permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&filter, permissions).expect("permissions");
    fs::write(
        repository.path().join(".gitattributes"),
        "tracked filter=review\n",
    )
    .expect("attributes");
    git(
        repository.path(),
        &[
            "config",
            "filter.review.clean",
            filter.to_str().expect("UTF-8 temp path"),
        ],
    );
    thread::sleep(Duration::from_millis(20));
    fs::write(&tracked, "same").expect("refresh tracked mtime");

    let mut service = GitService::new(Duration::from_secs(5));
    let workspace = service
        .detect(repository.path())
        .expect("detect")
        .expect("Git workspace");
    service.refresh_status(&workspace).expect("status");

    assert!(!marker.exists(), "content filter executed");
}
