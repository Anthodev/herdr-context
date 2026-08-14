#![cfg(feature = "perf-harness")]

#[path = "support/perf_fixtures.rs"]
mod perf_fixtures;
use herdr_context::conversations::sources::{
    ConversationSource, DiscoveryLimit, GenericJsonlSource, ProjectLocalLocation,
};
use herdr_context::project::ProjectIdentity;

use perf_fixtures::{
    EXTERNAL_SESSION_COUNT, LOCAL_SESSION_COUNT, MAX_FIXTURE_BYTES, MONOREPO_IGNORED_FILE_COUNT,
    MONOREPO_VISIBLE_FILE_COUNT, PerformanceFixtures,
};
use tempfile::TempDir;

#[test]
fn generated_workloads_are_bounded_synthetic_and_complete() {
    let temp = TempDir::new().expect("fixture root");
    let fixtures =
        PerformanceFixtures::create_for_tests(temp.path()).expect("performance fixtures");
    let manifest = fixtures.validate().expect("fixture manifest");

    assert_eq!(manifest.external_sessions(), EXTERNAL_SESSION_COUNT);
    assert_eq!(manifest.local_sessions(), LOCAL_SESSION_COUNT);
    assert_eq!(
        manifest.monorepo_visible_files(),
        MONOREPO_VISIBLE_FILE_COUNT
    );
    assert_eq!(
        manifest.monorepo_ignored_files(),
        MONOREPO_IGNORED_FILE_COUNT
    );
    assert!(manifest.total_payload_bytes() <= MAX_FIXTURE_BYTES);
    assert!(fixtures.no_vcs().join("plain-000.txt").is_file());
    assert!(fixtures.small_git().join(".git").is_dir());
    assert!(fixtures.native_jj().join(".jj").is_dir());
    assert!(fixtures.colocated_jj().join(".jj").is_dir());
    assert!(fixtures.colocated_jj().join(".git").exists());
    assert!(fixtures.append_transcript().is_file());
}

#[test]
fn generation_replaces_only_its_owned_fixture_directory() {
    let temp = TempDir::new().expect("fixture root");
    let unrelated = temp.path().join("keep.txt");
    std::fs::write(&unrelated, "keep").expect("unrelated fixture sibling");

    let first = PerformanceFixtures::create_for_tests(temp.path()).expect("first generation");
    std::fs::write(first.root().join("stale.txt"), "stale").expect("stale owned file");
    let second = PerformanceFixtures::create_for_tests(temp.path()).expect("second generation");

    assert!(unrelated.is_file());
    assert!(!second.root().join("stale.txt").exists());
}

#[test]
fn validation_rejects_completed_fixture_root_above_bound() {
    let temp = TempDir::new().expect("fixture root");
    let fixtures =
        PerformanceFixtures::create_for_tests(temp.path()).expect("performance fixtures");
    let oversized = std::fs::File::create(fixtures.root().join("oversized.bin"))
        .expect("oversized fixture file");
    oversized
        .set_len(MAX_FIXTURE_BYTES + 1)
        .expect("sparse oversized fixture");

    let error = fixtures.validate().expect_err("oversized fixture rejected");

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn local_history_fixture_is_discoverable_in_bounded_pages() {
    let temp = TempDir::new().expect("fixture root");
    let fixtures =
        PerformanceFixtures::create_for_tests(temp.path()).expect("performance fixtures");
    let root = std::fs::canonicalize(fixtures.local_project()).expect("canonical project");
    let project = ProjectIdentity::from_canonical_root(root).expect("project identity");
    let source = GenericJsonlSource::new(
        project.clone(),
        [ProjectLocalLocation::new(".herdr/conversations").expect("location")],
    )
    .expect("generic source");

    let batch = source
        .discover(
            &project,
            None,
            DiscoveryLimit::new(64).expect("bounded page"),
        )
        .expect("local fixture discovery");

    assert_eq!(batch.candidates().len(), 64, "{:?}", batch.errors());
    assert!(!batch.has_more());
}
