use std::fs;
use std::path::Path;

use herdr_context::files::FilesModel;
use tempfile::TempDir;

fn jj_marker(root: &Path) {
    fs::create_dir_all(root.join(".jj/repo")).expect("repo marker");
    fs::create_dir_all(root.join(".jj/working_copy")).expect("working-copy marker");
}

#[test]
fn native_jujutsu_uses_workspace_and_nested_gitignores_lazily_with_negation() {
    let temp = TempDir::new().expect("tempdir");
    let workspace = temp.path();
    let opened = workspace.join("project");
    jj_marker(workspace);
    fs::create_dir_all(opened.join("build")).expect("build");
    fs::create_dir_all(opened.join("nested")).expect("nested");
    fs::write(
        workspace.join(".gitignore"),
        "project/*.log\nproject/build/*\n!project/build/keep.txt\n",
    )
    .expect("workspace ignore");
    fs::write(opened.join("nested/.gitignore"), "*.tmp\n!important.tmp\n").expect("nested ignore");
    fs::write(opened.join("ignored.log"), []).expect("ignored");
    fs::write(opened.join("visible.txt"), []).expect("visible");
    fs::write(opened.join("build/drop.txt"), []).expect("drop");
    fs::write(opened.join("build/keep.txt"), []).expect("keep");
    fs::write(opened.join("nested/drop.tmp"), []).expect("nested drop");
    fs::write(opened.join("nested/important.tmp"), []).expect("nested keep");

    let mut files = FilesModel::for_workspace(opened, workspace.to_path_buf()).expect("files");
    files.load_directory(Path::new("")).expect("root");
    files
        .load_directory(Path::new("build"))
        .expect("build listing");
    files
        .load_directory(Path::new("nested"))
        .expect("nested listing");

    assert!(files.tree().node(Path::new("ignored.log")).is_none());
    assert!(files.tree().node(Path::new("visible.txt")).is_some());
    assert!(files.tree().node(Path::new("build/drop.txt")).is_none());
    assert!(files.tree().node(Path::new("build/keep.txt")).is_some());
    assert!(files.tree().node(Path::new("nested/drop.tmp")).is_none());
    assert!(
        files
            .tree()
            .node(Path::new("nested/important.tmp"))
            .is_some()
    );
}

#[test]
fn same_file_negation_cannot_reinclude_files_below_an_ignored_workspace_directory() {
    let temp = TempDir::new().expect("tempdir");
    let workspace = temp.path();
    let opened = workspace.join("project");
    jj_marker(workspace);
    fs::create_dir(&opened).expect("project");
    fs::write(
        workspace.join(".gitignore"),
        "project/\n!project/keep.txt\n",
    )
    .expect("workspace ignore");
    fs::write(opened.join("keep.txt"), []).expect("ignored fixture");

    let mut files = FilesModel::for_workspace(opened, workspace.to_path_buf()).expect("files");
    files.load_directory(Path::new("")).expect("root");

    assert!(files.tree().node(Path::new("keep.txt")).is_none());
}

#[test]
fn colocated_jujutsu_honors_git_info_exclude() {
    let temp = TempDir::new().expect("tempdir");
    jj_marker(temp.path());
    fs::create_dir_all(temp.path().join(".git/info")).expect("Git info");
    fs::write(temp.path().join(".git/info/exclude"), "excluded.txt\n").expect("exclude");
    fs::write(temp.path().join("excluded.txt"), []).expect("excluded fixture");
    fs::write(temp.path().join("visible.txt"), []).expect("visible fixture");

    let mut files = FilesModel::new(temp.path().to_path_buf()).expect("files");
    files.load_directory(Path::new("")).expect("root listing");

    assert!(files.tree().node(Path::new("excluded.txt")).is_none());
    assert!(files.tree().node(Path::new("visible.txt")).is_some());
}
