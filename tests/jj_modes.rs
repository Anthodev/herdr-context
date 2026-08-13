#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

use herdr_context::vcs::jj::{JjService, JujutsuMode};
use herdr_context::vcs::{VcsBackendMetadata, VcsErrorKind, VcsService, VcsWorkspace};
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
        VcsBackendMetadata::new("jj", "Jujutsu", true).expect("metadata"),
    )
    .expect("workspace")
}

#[test]
fn fresh_is_default_and_mode_can_be_switched() {
    let mut service = JjService::default();
    assert_eq!(service.mode(), JujutsuMode::Fresh);
    assert!(service.set_mode(JujutsuMode::Passive));
    assert_eq!(service.mode(), JujutsuMode::Passive);
    assert!(!service.set_mode(JujutsuMode::Passive));
}

#[test]
fn fresh_and_passive_use_fixed_single_command_argv_and_stale_metadata() {
    for (mode, expects_ignore, expects_stale) in [
        (JujutsuMode::Fresh, false, false),
        (JujutsuMode::Passive, true, true),
    ] {
        let temp = TempDir::new().expect("tempdir");
        let script = temp.path().join("fake-jj");
        let args = temp.path().join("args");
        let calls = temp.path().join("calls");
        executable(
            &script,
            &format!(
                "#!/bin/sh\nprintf 'call\\n' >> '{}'\nprintf '%s\\n' \"$@\" > '{}'\nprintf 'M\\000tracked\\000tracked\\000false\\000false\\000file\\000file\\000'\n",
                calls.display(),
                args.display()
            ),
        );
        let mut service = JjService::with_executable(script, mode, Duration::from_secs(1));

        let snapshot = service
            .refresh_status(&workspace(temp.path()))
            .expect("status");
        let arguments = fs::read_to_string(args).expect("arguments");
        let arguments: Vec<_> = arguments.lines().collect();

        for required in [
            "--color=never",
            "--no-pager",
            "--quiet",
            "-R",
            "diff",
            "-r",
            "@",
            "--template",
        ] {
            assert!(
                arguments.contains(&required),
                "missing {required}: {arguments:?}"
            );
        }
        assert_eq!(
            arguments.contains(&"--ignore-working-copy"),
            expects_ignore,
            "unexpected mode argv: {arguments:?}"
        );
        assert_eq!(snapshot.is_stale(), expects_stale);
        assert_eq!(
            fs::read_to_string(calls).expect("calls").lines().count(),
            1,
            "status must use one command per workspace"
        );
    }
}

#[test]
fn root_detection_is_passive_and_confined_to_the_reported_ancestor() {
    let temp = TempDir::new().expect("tempdir");
    let nested = temp.path().join("nested");
    fs::create_dir(&nested).expect("nested");
    let script = temp.path().join("fake-jj");
    let args = temp.path().join("args");
    executable(
        &script,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nprintf '%s\\n' '{}'\n",
            args.display(),
            temp.path().display()
        ),
    );
    let service = JjService::with_executable(script, JujutsuMode::Fresh, Duration::from_secs(1));

    let detected = service.detect(&nested).expect("detect").expect("workspace");
    assert_eq!(detected.root(), temp.path().canonicalize().expect("root"));
    let arguments = fs::read_to_string(args).expect("arguments");
    assert!(arguments.lines().any(|argument| argument == "root"));
    assert!(
        arguments
            .lines()
            .any(|argument| argument == "--ignore-working-copy"),
        "detection must never snapshot"
    );
}

#[test]
fn status_timeout_terminates_descendants_and_preserves_a_typed_error() {
    let temp = TempDir::new().expect("tempdir");
    let script = temp.path().join("slow-jj");
    executable(&script, "#!/bin/sh\n(while :; do :; done) &\nexit 0\n");
    let mut service =
        JjService::with_executable(script, JujutsuMode::Fresh, Duration::from_millis(50));

    let started = Instant::now();
    let error = service
        .refresh_status(&workspace(temp.path()))
        .expect_err("timeout");

    assert_eq!(error.kind(), VcsErrorKind::CommandFailed);
    assert!(started.elapsed() < Duration::from_secs(1));
}
