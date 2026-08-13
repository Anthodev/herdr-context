use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use herdr_context::files::FilesModel;
use herdr_context::files::refresh::RefreshResult;
use herdr_context::files::tree::TreeNodeKind;
use herdr_context::vcs::VcsService;
use herdr_context::vcs::git::GitService;
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

#[test]
fn nested_ignore_tree_and_git_status_merge_end_to_end() {
    let temp = TempDir::new().expect("tempdir");
    git(temp.path(), &["init", "--quiet"]);
    git(temp.path(), &["config", "user.name", "Test"]);
    git(
        temp.path(),
        &["config", "user.email", "test@example.invalid"],
    );
    fs::create_dir(temp.path().join("build")).expect("build");
    fs::create_dir(temp.path().join("nested")).expect("nested");
    fs::write(
        temp.path().join(".gitignore"),
        "*.log\nbuild/*\n!build/keep.txt\n",
    )
    .expect("root ignore");
    fs::write(
        temp.path().join("nested/.gitignore"),
        "*.tmp\n!important.tmp\n",
    )
    .expect("nested ignore");
    fs::write(temp.path().join("tracked-delete.txt"), "tracked").expect("tracked");
    git(
        temp.path(),
        &[
            "add",
            ".gitignore",
            "nested/.gitignore",
            "tracked-delete.txt",
        ],
    );
    git(temp.path(), &["commit", "--quiet", "-m", "fixture"]);

    fs::remove_file(temp.path().join("tracked-delete.txt")).expect("delete");
    fs::write(temp.path().join("ignored.log"), []).expect("ignored");
    fs::write(temp.path().join("build/drop.txt"), []).expect("drop");
    fs::write(temp.path().join("build/keep.txt"), []).expect("keep");
    fs::write(temp.path().join("nested/drop.tmp"), []).expect("nested drop");
    fs::write(temp.path().join("nested/important.tmp"), []).expect("nested keep");

    let mut files = FilesModel::new(temp.path().to_path_buf()).expect("files");
    files.load_directory(Path::new("")).expect("root listing");
    files
        .load_directory(Path::new("build"))
        .expect("build listing");
    files
        .load_directory(Path::new("nested"))
        .expect("nested listing");
    assert!(files.tree().node(Path::new("ignored.log")).is_none());
    assert!(files.tree().node(Path::new("build/drop.txt")).is_none());
    assert!(files.tree().node(Path::new("build/keep.txt")).is_some());
    assert!(files.tree().node(Path::new("nested/drop.tmp")).is_none());
    assert!(
        files
            .tree()
            .node(Path::new("nested/important.tmp"))
            .is_some()
    );

    let mut git = GitService::new(Duration::from_secs(5));
    let workspace = git.detect(temp.path()).expect("detect").expect("workspace");
    files.request_refresh();
    let generation = files.begin_refresh().expect("generation");
    let result = RefreshResult::run(generation, &mut git, &workspace);
    assert!(files.complete_refresh(result));

    let deleted = files
        .tree()
        .node(Path::new("tracked-delete.txt"))
        .expect("virtual deleted row");
    assert_eq!(deleted.kind(), TreeNodeKind::Virtual);
    assert!(!deleted.is_expandable());
    assert!(files.failure_notice().is_none());
}
