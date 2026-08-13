use std::fs;
use std::path::{Path, PathBuf};

use herdr_context::conversations::sources::{
    ConversationSource, DiscoveryLimit, GenericJsonlSource, ProjectLocalLocation,
};
use herdr_context::project::ProjectIdentity;
use tempfile::TempDir;

fn record(project: &ProjectIdentity, session: &str) -> String {
    serde_json::json!({
        "session_id": session,
        "cwd": project.root(),
        "timestamp": "2026-01-02T03:04:05Z",
        "role": "user",
        "message": "fixture body",
    })
    .to_string()
}

fn source(
    temp: &TempDir,
    locations: impl IntoIterator<Item = PathBuf>,
) -> (ProjectIdentity, GenericJsonlSource) {
    let project = ProjectIdentity::from_canonical_root(temp.path().to_path_buf()).expect("project");
    let locations = locations
        .into_iter()
        .map(|path| ProjectLocalLocation::new(path).expect("location"));
    let source = GenericJsonlSource::new(project.clone(), locations).expect("source");
    (project, source)
}

#[test]
fn discovery_is_shallow_and_only_visits_registered_locations() {
    let temp = TempDir::new().expect("tempdir");
    let registered = temp.path().join("history");
    fs::create_dir_all(registered.join("nested")).expect("directories");
    let (project, source) = source(&temp, [PathBuf::from("history")]);
    fs::write(registered.join("direct.jsonl"), record(&project, "direct")).expect("direct");
    fs::write(
        registered.join("nested/hidden.jsonl"),
        record(&project, "nested"),
    )
    .expect("nested");
    fs::write(
        temp.path().join("unknown.jsonl"),
        record(&project, "unknown"),
    )
    .expect("unknown");

    let batch = source
        .discover(&project, None, DiscoveryLimit::new(8).expect("limit"))
        .expect("discovery");
    assert_eq!(batch.candidates().len(), 1);
    assert_eq!(batch.candidates()[0].source_reference(), "direct");
}

#[test]
fn an_exact_registered_file_is_discoverable() {
    let temp = TempDir::new().expect("tempdir");
    let (project, source) = source(&temp, [PathBuf::from("history.jsonl")]);
    fs::write(
        temp.path().join("history.jsonl"),
        record(&project, "exact-file"),
    )
    .expect("fixture");

    let batch = source
        .discover(&project, None, DiscoveryLimit::new(1).expect("limit"))
        .expect("discovery");
    assert_eq!(batch.candidates().len(), 1);
    assert_eq!(batch.candidates()[0].source_reference(), "exact-file");
}

#[test]
fn missing_registered_locations_are_non_fatal() {
    let temp = TempDir::new().expect("tempdir");
    let (project, source) = source(&temp, [PathBuf::from("missing")]);

    let probe = source.probe().expect("probe");
    assert!(matches!(
        probe,
        herdr_context::conversations::sources::StorageProbe::Unavailable { .. }
    ));
    let batch = source
        .discover(&project, None, DiscoveryLimit::new(4).expect("limit"))
        .expect("missing location discovery");
    assert!(batch.candidates().is_empty());
    assert!(batch.errors().is_empty());
}

#[test]
fn location_paths_must_be_normalized_project_relative_components() {
    for invalid in [
        PathBuf::from(""),
        PathBuf::from("../history"),
        PathBuf::from("history/../outside"),
        PathBuf::from("/absolute/history"),
    ] {
        assert!(
            ProjectLocalLocation::new(invalid).is_err(),
            "invalid path accepted"
        );
    }
    assert!(ProjectLocalLocation::new("history/sessions").is_ok());
}

#[test]
fn an_overfull_location_is_rejected_without_selecting_an_arbitrary_subset() {
    let temp = TempDir::new().expect("tempdir");
    fs::create_dir(temp.path().join("history")).expect("history");
    let (project, source) = source(&temp, [PathBuf::from("history")]);
    for index in 0..129 {
        fs::write(
            temp.path().join(format!("history/{index:03}.jsonl")),
            record(&project, &format!("session-{index:03}")),
        )
        .expect("fixture");
    }

    let batch = source
        .discover(&project, None, DiscoveryLimit::new(128).expect("limit"))
        .expect("bounded discovery");
    assert!(batch.candidates().is_empty());
    assert_eq!(batch.errors().len(), 1);
}

#[cfg(unix)]
#[test]
fn symlinked_registered_paths_and_files_cannot_escape_the_project() {
    use std::os::unix::fs::symlink;

    let project_dir = TempDir::new().expect("project tempdir");
    let outside = TempDir::new().expect("outside tempdir");
    let (project, source) = source(
        &project_dir,
        [PathBuf::from("escaped-dir"), PathBuf::from("history")],
    );
    fs::create_dir(project_dir.path().join("history")).expect("history");
    fs::write(
        outside.path().join("outside.jsonl"),
        record(&project, "outside"),
    )
    .expect("outside fixture");
    symlink(outside.path(), project_dir.path().join("escaped-dir")).expect("directory symlink");
    symlink(
        outside.path().join("outside.jsonl"),
        project_dir.path().join("history/escaped.jsonl"),
    )
    .expect("file symlink");
    assert_eq!(
        source.probe().expect("healthy registered location"),
        herdr_context::conversations::sources::StorageProbe::Available
    );

    let batch = source
        .discover(&project, None, DiscoveryLimit::new(8).expect("limit"))
        .expect("isolated discovery");
    assert!(batch.candidates().is_empty());
    assert_eq!(batch.errors().len(), 2);
    assert!(batch.errors().iter().all(|error| {
        error
            .path()
            .is_some_and(|path| path.starts_with(project.root()))
    }));
}

#[test]
fn discovery_limit_pages_changed_files_without_losing_them() {
    let temp = TempDir::new().expect("tempdir");
    fs::create_dir(temp.path().join("history")).expect("history");
    let (project, source) = source(&temp, [PathBuf::from("history")]);
    for (name, session) in [("a.jsonl", "a"), ("b.jsonl", "b"), ("c.jsonl", "c")] {
        fs::write(
            temp.path().join("history").join(name),
            record(&project, session),
        )
        .expect("fixture");
    }

    let first = source
        .discover(&project, None, DiscoveryLimit::new(1).expect("limit"))
        .expect("first page");
    let second = source
        .discover(
            &project,
            first.next_watermark(),
            DiscoveryLimit::new(1).expect("limit"),
        )
        .expect("second page");
    let third = source
        .discover(
            &project,
            second.next_watermark(),
            DiscoveryLimit::new(1).expect("limit"),
        )
        .expect("third page");

    assert_eq!(first.candidates()[0].source_reference(), "a");
    assert_eq!(second.candidates()[0].source_reference(), "b");
    assert_eq!(third.candidates()[0].source_reference(), "c");
    let done = source
        .discover(
            &project,
            third.next_watermark(),
            DiscoveryLimit::new(1).expect("limit"),
        )
        .expect("completed page");
    assert!(done.candidates().is_empty());
}

#[cfg(unix)]
#[test]
fn unreadable_entries_are_isolated_from_readable_entries() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new().expect("tempdir");
    fs::create_dir(temp.path().join("history")).expect("history");
    let (project, source) = source(&temp, [PathBuf::from("history")]);
    let readable = temp.path().join("history/readable.jsonl");
    let unreadable = temp.path().join("history/unreadable.jsonl");
    fs::write(&readable, record(&project, "readable")).expect("readable");
    fs::write(&unreadable, record(&project, "unreadable")).expect("unreadable");
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).expect("permissions");

    let batch = source
        .discover(&project, None, DiscoveryLimit::new(8).expect("limit"))
        .expect("isolated discovery");
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o600)).expect("restore");

    assert_eq!(batch.candidates().len(), 1);
    assert_eq!(batch.candidates()[0].source_reference(), "readable");
    assert_eq!(batch.errors().len(), 1);
    assert_eq!(batch.errors()[0].path(), Some(unreadable.as_path()));
}

#[test]
fn component_prefixes_are_not_treated_as_project_membership() {
    let temp = TempDir::new().expect("tempdir");
    let sibling = TempDir::new_in(temp.path().parent().expect("parent")).expect("sibling");
    let (project, source) = source(&temp, [PathBuf::from("history")]);
    fs::create_dir(temp.path().join("history")).expect("history");
    let misleading = format!("{}-suffix", project.root().display());
    assert!(!Path::new(&misleading).starts_with(project.root()));
    fs::write(
        temp.path().join("history/prefix.jsonl"),
        serde_json::json!({
            "session_id": "prefix",
            "cwd": sibling.path(),
            "timestamp": "2026-01-02T03:04:05Z",
            "role": "user",
            "message": "not this project",
        })
        .to_string(),
    )
    .expect("fixture");

    let batch = source
        .discover(&project, None, DiscoveryLimit::new(8).expect("limit"))
        .expect("discovery");
    assert!(batch.candidates().is_empty());
    assert_eq!(batch.errors().len(), 1);
}
