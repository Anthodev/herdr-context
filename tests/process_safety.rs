#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use herdr_context::vcs::git::GitService;
use herdr_context::vcs::{
    VcsBackendMetadata, VcsErrorKind, VcsService, VcsStatusKind, VcsWorkspace,
};
use tempfile::TempDir;

fn executable(path: &Path, contents: &str) {
    fs::write(path, contents).expect("script");
    let mut permissions = fs::metadata(path).expect("metadata").permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(path, permissions).expect("permissions");
}

fn workspace(root: &Path) -> VcsWorkspace {
    VcsWorkspace::new(
        root.to_path_buf(),
        VcsBackendMetadata::new("git", "Git", false).expect("metadata"),
    )
    .expect("workspace")
}

#[test]
fn status_uses_fixed_argv_and_clears_redirecting_environment() {
    let temp = TempDir::new().expect("tempdir");
    let script = temp.path().join("fake-git");
    let args = temp.path().join("args");
    let environment = temp.path().join("environment");
    executable(
        &script,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nprintf '%s\\n' \"${{GIT_DIR-unset}}\" \"${{GIT_WORK_TREE-unset}}\" \"${{GIT_CONFIG_SYSTEM-unset}}\" \"${{GIT_TERMINAL_PROMPT-unset}}\" > '{}'\nprintf '? safe path\\0'\n",
            args.display(),
            environment.display()
        ),
    );

    let status = Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", "status_process_child"])
        .env("HDC_STATUS_CHILD", "1")
        .env("HDC_GIT_EXECUTABLE", &script)
        .env("HDC_WORKSPACE", temp.path())
        .env("GIT_DIR", "/redirected/repository")
        .env("GIT_WORK_TREE", "/redirected/worktree")
        .env("GIT_CONFIG_SYSTEM", "/redirected/config")
        .status()
        .expect("run child test");
    assert!(status.success());

    let arguments = fs::read_to_string(args).expect("args");
    for required in [
        "--no-optional-locks",
        "core.hooksPath=/dev/null",
        "core.fsmonitor=false",
        "pager.status=false",
        "diff.external=",
        "status",
        "--porcelain=v2",
        "-z",
        "--untracked-files=all",
    ] {
        assert!(
            arguments.lines().any(|argument| argument == required),
            "missing {required}"
        );
    }
    assert_eq!(
        fs::read_to_string(environment).expect("environment"),
        "unset\nunset\nunset\n0\n"
    );
}

#[test]
fn status_process_child() {
    if std::env::var_os("HDC_STATUS_CHILD").is_none() {
        return;
    }
    let executable = std::env::var_os("HDC_GIT_EXECUTABLE").expect("child executable");
    let root = std::env::var_os("HDC_WORKSPACE").expect("child workspace");
    let mut service = GitService::with_executable(executable.into(), Duration::from_secs(1));
    let snapshot = service
        .refresh_status(&workspace(Path::new(&root)))
        .expect("hardened status");
    assert_eq!(snapshot.entries()[0].kind(), VcsStatusKind::Untracked);
}

#[test]
fn timeout_kills_descendants_that_keep_status_pipes_open() {
    let temp = TempDir::new().expect("tempdir");
    let script = temp.path().join("slow-git");
    executable(&script, "#!/bin/sh\n(while :; do :; done) &\nexit 0\n");
    let mut service = GitService::with_executable(script, Duration::from_millis(50));

    let started = Instant::now();
    let error = service
        .refresh_status(&workspace(temp.path()))
        .expect_err("timeout");

    assert_eq!(
        error.kind(),
        VcsErrorKind::CommandFailed,
        "unexpected error: {error}"
    );
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn successful_status_kills_background_process_group_members() {
    let temp = TempDir::new().expect("tempdir");
    let script = temp.path().join("background-git");
    let marker = temp.path().join("descendant-finished");
    executable(
        &script,
        &format!(
            "#!/bin/sh\ncase \" $* \" in *\" config \"*) exit 0;; esac\n(sleep 0.2; touch '{}') </dev/null >/dev/null 2>&1 &\nprintf '? safe\\0'\n",
            marker.display()
        ),
    );
    let mut service = GitService::with_executable(script, Duration::from_secs(1));

    service
        .refresh_status(&workspace(temp.path()))
        .expect("successful status");
    std::thread::sleep(Duration::from_millis(400));

    assert!(
        !marker.exists(),
        "Git descendant survived successful status"
    );
}

#[test]
fn malformed_output_is_rejected_without_adapter_records_escaping() {
    let temp = TempDir::new().expect("tempdir");
    let script = temp.path().join("bad-git");
    executable(&script, "#!/bin/sh\nprintf '2 R. truncated\\0'\n");
    let mut service = GitService::with_executable(script, Duration::from_secs(1));

    let error = service
        .refresh_status(&workspace(temp.path()))
        .expect_err("malformed output");
    assert_eq!(error.kind(), VcsErrorKind::InvalidData);
}
