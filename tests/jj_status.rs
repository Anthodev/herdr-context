#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use herdr_context::files::FilesModel;
use herdr_context::files::refresh::RefreshResult;
use herdr_context::files::tree::{FilesTree, TreeNodeKind};
use herdr_context::vcs::git::GitService;
use herdr_context::vcs::jj::{JjService, JujutsuMode};
use herdr_context::vcs::{VcsErrorKind, VcsService, VcsStatusKind};
use tempfile::TempDir;

fn executable(path: &Path, contents: &str) {
    fs::write(path, contents).expect("script");
    let mut permissions = fs::metadata(path).expect("metadata").permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(path, permissions).expect("permissions");
}

fn command_available(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

fn jj(root: &Path, arguments: &[&str]) {
    let output = Command::new("jj")
        .args(arguments)
        .current_dir(root)
        .output()
        .expect("run Jujutsu fixture command");
    assert!(
        output.status.success(),
        "jj {arguments:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn supports_tree_diff_entry_status(root: &Path) -> bool {
    Command::new("jj")
        .args([
            "--ignore-working-copy",
            "diff",
            "-r",
            "@",
            "--template",
            "self.status_char()",
        ])
        .current_dir(root)
        .output()
        .is_ok_and(|output| output.status.success())
}

#[test]
fn templated_diff_maps_statuses_types_conflicts_copies_and_renames() {
    let temp = TempDir::new().expect("tempdir");
    let script = temp.path().join("fake-jj");
    executable(
        &script,
        "#!/bin/sh\nprintf 'M\\000modified\\000modified\\000false\\000false\\000file\\000file\\000A\\000added\\000added\\000false\\000false\\000\\000file\\000D\\000deleted\\000deleted\\000false\\000false\\000file\\000\\000R\\000old-name\\000new-name\\000false\\000false\\000file\\000file\\000C\\000source-copy\\000target-copy\\000false\\000false\\000file\\000file\\000M\\000typed\\000typed\\000false\\000false\\000file\\000symlink\\000M\\000conflicted\\000conflicted\\000true\\000false\\000conflict\\000file\\000'\n",
    );
    let mut service =
        JjService::with_executable(script, JujutsuMode::Fresh, Duration::from_secs(1));
    let workspace = herdr_context::vcs::VcsWorkspace::new(
        temp.path().to_path_buf(),
        herdr_context::vcs::VcsBackendMetadata::new("jj", "Jujutsu", true).expect("metadata"),
    )
    .expect("workspace");

    let snapshot = service.refresh_status(&workspace).expect("status");
    let find = |path: &str| {
        snapshot
            .entries()
            .iter()
            .find(|entry| entry.path() == Path::new(path))
            .unwrap_or_else(|| panic!("missing status for {path}"))
    };

    assert_eq!(find("modified").kind(), VcsStatusKind::Modified);
    assert_eq!(find("added").kind(), VcsStatusKind::Added);
    assert_eq!(find("deleted").kind(), VcsStatusKind::Deleted);
    assert_eq!(find("new-name").kind(), VcsStatusKind::Renamed);
    assert_eq!(find("new-name").source_path(), Some(Path::new("old-name")));
    assert_eq!(find("target-copy").kind(), VcsStatusKind::Copied);
    assert_eq!(
        find("target-copy").source_path(),
        Some(Path::new("source-copy"))
    );
    assert_eq!(find("typed").kind(), VcsStatusKind::TypeChanged);
    assert_eq!(find("conflicted").kind(), VcsStatusKind::Conflicted);
    assert!(!snapshot.is_stale());
}

#[test]
fn deleted_descendant_marks_present_directories_modified() {
    let temp = TempDir::new().expect("tempdir");
    fs::create_dir_all(temp.path().join("src/nested")).expect("directories");
    let script = temp.path().join("fake-jj");
    executable(
        &script,
        "#!/bin/sh\nprintf 'D\\000src/nested/deleted.rs\\000src/nested/deleted.rs\\000false\\000false\\000file\\000\\000'\n",
    );
    let mut service =
        JjService::with_executable(script, JujutsuMode::Fresh, Duration::from_secs(1));
    let workspace = herdr_context::vcs::VcsWorkspace::new(
        temp.path().to_path_buf(),
        herdr_context::vcs::VcsBackendMetadata::new("jj", "Jujutsu", true).expect("metadata"),
    )
    .expect("workspace");
    let snapshot = service.refresh_status(&workspace).expect("status");
    let mut tree = FilesTree::new(temp.path().to_path_buf()).expect("tree");
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
fn mixed_non_conflicted_descendants_aggregate_as_modified() {
    let temp = TempDir::new().expect("tempdir");
    fs::create_dir_all(temp.path().join("src/nested")).expect("directories");
    for name in [
        "src/old.rs",
        "src/base.rs",
        "src/nested/renamed.rs",
        "src/nested/copied.rs",
        "src/nested/added.rs",
    ] {
        fs::write(temp.path().join(name), []).expect("fixture file");
    }
    let script = temp.path().join("fake-jj");
    executable(
        &script,
        "#!/bin/sh\nprintf 'A\\000\\000src/nested/added.rs\\000false\\000false\\000\\000file\\000R\\000src/old.rs\\000src/nested/renamed.rs\\000false\\000false\\000file\\000file\\000C\\000src/base.rs\\000src/nested/copied.rs\\000false\\000false\\000file\\000file\\000'\n",
    );
    let mut service =
        JjService::with_executable(script, JujutsuMode::Fresh, Duration::from_secs(1));
    let workspace = herdr_context::vcs::VcsWorkspace::new(
        temp.path().to_path_buf(),
        herdr_context::vcs::VcsBackendMetadata::new("jj", "Jujutsu", true).expect("metadata"),
    )
    .expect("workspace");
    let snapshot = service.refresh_status(&workspace).expect("status");
    let mut tree = FilesTree::new(temp.path().to_path_buf()).expect("tree");
    tree.load_directory(Path::new("")).expect("root");
    tree.load_directory(Path::new("src")).expect("src");
    tree.load_directory(Path::new("src/nested"))
        .expect("nested");
    tree.merge_status(&snapshot).expect("status overlay");

    assert_eq!(
        tree.node(Path::new("src")).expect("src").status(),
        Some(VcsStatusKind::Modified)
    );
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
    assert_eq!(
        tree.node(Path::new("src/nested/renamed.rs"))
            .expect("renamed row")
            .status(),
        Some(VcsStatusKind::Renamed)
    );
    assert_eq!(
        tree.node(Path::new("src/nested/copied.rs"))
            .expect("copied row")
            .status(),
        Some(VcsStatusKind::Copied)
    );
}

#[test]
fn malformed_templated_output_is_rejected() {
    let temp = TempDir::new().expect("tempdir");
    let script = temp.path().join("fake-jj");
    executable(
        &script,
        "#!/bin/sh\nprintf 'M\\000path\\000path\\000maybe\\000false\\000file\\000file\\000'\n",
    );
    let mut service =
        JjService::with_executable(script, JujutsuMode::Fresh, Duration::from_secs(1));
    let workspace = herdr_context::vcs::VcsWorkspace::new(
        temp.path().to_path_buf(),
        herdr_context::vcs::VcsBackendMetadata::new("jj", "Jujutsu", true).expect("metadata"),
    )
    .expect("workspace");

    let error = service
        .refresh_status(&workspace)
        .expect_err("malformed output");
    assert_eq!(error.kind(), VcsErrorKind::InvalidData);
}

#[test]
fn native_and_colocated_workspaces_are_detected_as_jujutsu() {
    if !command_available("jj") {
        return;
    }

    for colocated in [false, true] {
        let temp = TempDir::new().expect("tempdir");
        if colocated {
            jj(temp.path(), &["git", "init", "--colocate"]);
        } else {
            jj(temp.path(), &["git", "init", "--no-colocate"]);
        }
        if !supports_tree_diff_entry_status(temp.path()) {
            return;
        }
        jj(
            temp.path(),
            &[
                "config",
                "set",
                "--repo",
                "template-aliases.status_char",
                r#""M""#,
            ],
        );
        fs::write(temp.path().join("added"), "contents").expect("added fixture");

        let mut service = JjService::new(JujutsuMode::Fresh, Duration::from_secs(5));
        let workspace = service
            .detect(temp.path())
            .expect("detect")
            .expect("Jujutsu workspace");
        assert_eq!(workspace.backend().id(), "jj");
        assert_eq!(workspace.root(), temp.path().canonicalize().expect("root"));
        assert_eq!(
            service
                .refresh_status(&workspace)
                .expect("status")
                .entries()[0]
                .kind(),
            VcsStatusKind::Added
        );

        jj(
            temp.path(),
            &[
                "--config",
                "user.name=Test",
                "--config",
                "user.email=test@example.invalid",
                "commit",
                "-m",
                "base",
            ],
        );
        fs::remove_file(temp.path().join("added")).expect("delete fixture");
        let deleted = service.refresh_status(&workspace).expect("deleted status");
        assert_eq!(deleted.entries()[0].kind(), VcsStatusKind::Deleted);

        let mut files = FilesModel::new(temp.path().to_path_buf()).expect("files");
        files.load_directory(Path::new("")).expect("root listing");
        files.request_refresh();
        let generation = files.begin_refresh().expect("generation");
        assert!(files.complete_refresh(RefreshResult::new(generation, Ok(deleted))));
        assert_eq!(
            files
                .tree()
                .node(Path::new("added"))
                .expect("virtual deleted row")
                .kind(),
            TreeNodeKind::Virtual
        );

        if colocated {
            assert!(
                GitService::new(Duration::from_secs(5))
                    .detect(temp.path())
                    .expect("Git detection")
                    .is_none(),
                "colocated Git must not become authoritative"
            );
        }
    }
}
