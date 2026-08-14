mod support;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use herdr_context::conversations::sources::{
    ConversationSource, ConversationSourceErrorKind, DiscoveryLimit, MetadataBudget,
    OpenCodeSource, ProjectEvidenceKind,
};
use herdr_context::conversations::{
    Conversation, ConversationState, ResumeCapability, ResumeReference,
};
use herdr_context::project::ProjectIdentity;
use rusqlite::{Connection, params};
use support::conversation_source::{SourceConformanceCase, assert_source_conforms};
use tempfile::TempDir;

const FIXTURE: &str = "tests/fixtures/conversations/opencode/opencode.db";
const FIXTURE_SESSION_ID: &str = "ses_ffffffffffffffffffffffffff";
const METADATA_BUDGET: usize = 16 * 1024;

fn make_project() -> (TempDir, ProjectIdentity) {
    let directory = TempDir::new().expect("project directory");
    let identity = ProjectIdentity::from_canonical_root(directory.path().to_path_buf())
        .expect("canonical project");
    (directory, identity)
}

fn database_path(home: &Path) -> PathBuf {
    home.join(".local/share/opencode/opencode.db")
}

fn install_fixture(home: &Path, project: &ProjectIdentity) -> PathBuf {
    let destination = database_path(home);
    fs::create_dir_all(destination.parent().expect("database parent")).expect("database directory");
    fs::copy(FIXTURE, &destination).expect("copy fixture");
    let root = project.root().to_str().expect("UTF-8 fixture path");
    let connection = Connection::open(&destination).expect("open fixture copy");
    connection
        .execute(
            "UPDATE project SET worktree = ?1 WHERE id = 'fixture-project'",
            [root],
        )
        .expect("update project worktree");
    connection
        .execute(
            "UPDATE project_directory SET directory = ?1 WHERE project_id = 'fixture-project'",
            [root],
        )
        .expect("update project directory");
    connection
        .execute(
            "UPDATE session SET directory = ?1 WHERE project_id = 'fixture-project'",
            [root],
        )
        .expect("update session directory");
    connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .expect("checkpoint fixture copy");
    drop(connection);
    destination
}

fn source(project: &ProjectIdentity, database: &Path) -> OpenCodeSource {
    OpenCodeSource::new(project.clone(), database.to_path_buf()).expect("OpenCode source")
}

fn discover_batch(
    source: &OpenCodeSource,
    project: &ProjectIdentity,
    after: Option<&herdr_context::conversations::sources::SourceWatermark>,
    limit: usize,
) -> herdr_context::conversations::sources::DiscoveryBatch {
    source
        .discover(
            project,
            after,
            DiscoveryLimit::new(limit).expect("non-zero limit"),
        )
        .expect("OpenCode discovery")
}

fn extract(
    source: &OpenCodeSource,
    batch: &herdr_context::conversations::sources::DiscoveryBatch,
) -> Conversation {
    source
        .extract_metadata(
            &batch.candidates()[0],
            MetadataBudget::new(METADATA_BUDGET).expect("non-zero metadata budget"),
        )
        .expect("OpenCode metadata")
}

fn millis(value: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_millis(value)
}

#[test]
fn fixture_passes_source_conformance_and_extracts_approved_metadata() {
    let (_project_directory, project) = make_project();
    let (_foreign_directory, foreign) = make_project();
    let home = TempDir::new().expect("home");
    let database = install_fixture(home.path(), &project);
    let source = source(&project, &database);
    let expected_resume = ResumeCapability::Supported(
        ResumeReference::new(FIXTURE_SESSION_ID).expect("resume reference"),
    );

    assert_source_conforms(SourceConformanceCase {
        source: &source,
        project: &project,
        foreign_project: &foreign,
        metadata_budget: MetadataBudget::new(METADATA_BUDGET).expect("metadata budget"),
        expected_resume: &expected_resume,
        expected_evidence_kind: ProjectEvidenceKind::CanonicalWorkingDirectory,
        small_budget_error: ConversationSourceErrorKind::InvalidData,
    });

    let batch = discover_batch(&source, &project, None, 1);
    let conversation = extract(&source, &batch);
    assert_eq!(conversation.tool().as_str(), "opencode");
    assert_eq!(conversation.session_reference().id(), FIXTURE_SESSION_ID);
    assert_eq!(conversation.title(), Some("Sanitized OpenCode fixture"));
    assert_eq!(conversation.created_at(), Some(millis(1_767_323_045_000)));
    assert_eq!(conversation.updated_at(), millis(1_767_323_050_000));
    assert_eq!(conversation.archived_at(), None);
    assert_eq!(conversation.state(), ConversationState::Live);
    assert_eq!(
        conversation.provenance()[0].path(),
        Some(database.as_path())
    );

    let evidence = source
        .project_evidence(&batch.candidates()[0], &project)
        .expect("project evidence");
    assert!(
        evidence
            .iter()
            .any(|item| item.kind() == ProjectEvidenceKind::CanonicalWorkingDirectory)
    );
    assert!(
        evidence
            .iter()
            .any(|item| item.kind() == ProjectEvidenceKind::CanonicalWorkspaceRoot)
    );
}

#[test]
fn archived_timestamp_is_preserved() {
    let (_project_directory, project) = make_project();
    let home = TempDir::new().expect("home");
    let database = install_fixture(home.path(), &project);
    let connection = Connection::open(&database).expect("fixture writer");
    connection
        .execute(
            "UPDATE session SET time_archived = ?1, time_updated = ?1 WHERE id = ?2",
            params![1_767_323_060_000_i64, FIXTURE_SESSION_ID],
        )
        .expect("archive session");
    connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .expect("checkpoint archive");
    drop(connection);

    let source = source(&project, &database);
    let batch = discover_batch(&source, &project, None, 1);
    let conversation = extract(&source, &batch);
    assert_eq!(conversation.archived_at(), Some(millis(1_767_323_060_000)));
    assert_eq!(conversation.state(), ConversationState::Archived);
}

#[test]
fn unsupported_schema_generations_are_explicitly_rejected() {
    let (_project_directory, project) = make_project();
    let home = TempDir::new().expect("home");
    let database = install_fixture(home.path(), &project);
    let connection = Connection::open(&database).expect("fixture writer");
    connection
        .execute(
            "DELETE FROM migration WHERE id = '20260622202450_simplify_session_input'",
            [],
        )
        .expect("remove migration marker");
    connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .expect("checkpoint migration change");
    drop(connection);

    let source = source(&project, &database);
    let error = source
        .discover(&project, None, DiscoveryLimit::new(1).expect("limit"))
        .expect_err("unsupported schema must fail");
    assert_eq!(error.kind(), ConversationSourceErrorKind::UnsupportedFormat);
    assert!(error.to_string().contains("OpenCode 1.18.18"));
}
#[test]
fn unsupported_session_versions_are_explicitly_rejected() {
    let (_project_directory, project) = make_project();
    let home = TempDir::new().expect("home");
    let database = install_fixture(home.path(), &project);
    let connection = Connection::open(&database).expect("fixture writer");
    connection
        .execute("UPDATE session SET version = '1.18.19'", [])
        .expect("change session version");
    connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .expect("checkpoint version change");
    drop(connection);

    let source = source(&project, &database);
    let error = source
        .discover(&project, None, DiscoveryLimit::new(1).expect("limit"))
        .expect_err("unsupported session version must fail");
    assert_eq!(error.kind(), ConversationSourceErrorKind::UnsupportedFormat);
}

#[test]
fn duplicate_unconstrained_migration_journals_are_rejected() {
    let (_project_directory, project) = make_project();
    let home = TempDir::new().expect("home");
    let database = install_fixture(home.path(), &project);
    let connection = Connection::open(&database).expect("fixture writer");
    connection
        .execute_batch(
            "ALTER TABLE migration RENAME TO migration_old;
             CREATE TABLE migration (id TEXT, time_completed INTEGER NOT NULL);
             INSERT INTO migration SELECT id, time_completed FROM migration_old;
             INSERT INTO migration
             SELECT id, time_completed FROM migration_old ORDER BY id LIMIT 1;
             DROP TABLE migration_old;
             PRAGMA wal_checkpoint(TRUNCATE);",
        )
        .expect("replace migration journal");
    drop(connection);

    let source = source(&project, &database);
    let error = source
        .discover(&project, None, DiscoveryLimit::new(1).expect("limit"))
        .expect_err("duplicate journal must fail");
    assert_eq!(error.kind(), ConversationSourceErrorKind::UnsupportedFormat);
}

#[test]
fn canonical_session_and_project_evidence_are_both_required() {
    let (_project_directory, project) = make_project();
    let foreign_directory = TempDir::new().expect("foreign directory");
    let foreign = fs::canonicalize(foreign_directory.path()).expect("canonical foreign path");
    let home = TempDir::new().expect("home");
    let database = install_fixture(home.path(), &project);
    let connection = Connection::open(&database).expect("fixture writer");
    connection
        .execute(
            "UPDATE project SET worktree = ?1",
            [foreign.to_str().expect("UTF-8 foreign path")],
        )
        .expect("change worktree");
    connection
        .execute("DELETE FROM project_directory", [])
        .expect("remove directory evidence");
    connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .expect("checkpoint evidence change");
    drop(connection);

    let source = source(&project, &database);
    let batch = discover_batch(&source, &project, None, 2);
    assert!(batch.candidates().is_empty());
}

#[test]
fn discovery_is_recent_first_paginated_and_incremental() {
    let (_project_directory, project) = make_project();
    let home = TempDir::new().expect("home");
    let database = install_fixture(home.path(), &project);
    let second_id = format!("ses_{}", "e".repeat(26));
    let connection = Connection::open(&database).expect("fixture writer");
    connection
        .execute(
            "INSERT INTO session (id, project_id, slug, directory, title, version, time_created, time_updated) \
             VALUES (?1, 'fixture-project', 'newer', ?2, 'Newer fixture', '1.18.18', ?3, ?3)",
            params![
                second_id,
                project.root().to_str().expect("UTF-8 fixture path"),
                1_767_323_070_000_i64
            ],
        )
        .expect("insert newer session");
    connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .expect("checkpoint second session");
    drop(connection);

    let source = source(&project, &database);
    let first = discover_batch(&source, &project, None, 1);
    assert_eq!(first.candidates()[0].source_reference(), second_id);
    assert!(first.has_more());
    let second = discover_batch(&source, &project, first.next_watermark(), 1);
    assert_eq!(
        second.candidates()[0].source_reference(),
        FIXTURE_SESSION_ID
    );
    assert!(!second.has_more());
    let unchanged = discover_batch(&source, &project, second.next_watermark(), 1);
    assert!(unchanged.candidates().is_empty());

    let writer = Connection::open(&database).expect("fixture writer");
    writer
        .execute(
            "UPDATE session SET title = 'Updated fixture', time_updated = ?1 WHERE id = ?2",
            params![1_767_323_080_000_i64, FIXTURE_SESSION_ID],
        )
        .expect("update session");
    writer
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .expect("checkpoint update");
    drop(writer);
    let changed = discover_batch(&source, &project, unchanged.next_watermark(), 1);
    assert_eq!(changed.candidates().len(), 1);
    assert_eq!(extract(&source, &changed).title(), Some("Updated fixture"));
}
#[test]
fn removals_are_emitted_incrementally() {
    let (_project_directory, project) = make_project();
    let home = TempDir::new().expect("home");
    let database = install_fixture(home.path(), &project);
    let source = source(&project, &database);
    let first = discover_batch(&source, &project, None, 1);

    let writer = Connection::open(&database).expect("fixture writer");
    writer
        .execute("DELETE FROM session WHERE id = ?1", [FIXTURE_SESSION_ID])
        .expect("delete session");
    writer
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .expect("checkpoint deletion");
    drop(writer);

    let removed = discover_batch(&source, &project, first.next_watermark(), 1);
    assert!(removed.candidates().is_empty());
    assert_eq!(removed.removals().len(), 1);
    assert_eq!(
        removed.removals()[0].session_reference().id(),
        FIXTURE_SESSION_ID
    );
}

#[test]
fn replacement_between_snapshots_is_source_scoped() {
    let (_project_directory, project) = make_project();
    let home = TempDir::new().expect("home");
    let database = install_fixture(home.path(), &project);
    let source = source(&project, &database);
    let first = discover_batch(&source, &project, None, 1);

    let replacement_home = TempDir::new().expect("replacement home");
    let replacement = install_fixture(replacement_home.path(), &project);
    fs::rename(replacement, &database).expect("replace database");

    let error = source
        .discover(
            &project,
            first.next_watermark(),
            DiscoveryLimit::new(1).expect("limit"),
        )
        .expect_err("replacement must fail");
    assert_eq!(error.kind(), ConversationSourceErrorKind::Io);
    assert!(error.to_string().contains("replaced"));
}

#[test]
fn active_wal_writer_does_not_block_a_consistent_reader() {
    let (_project_directory, project) = make_project();
    let home = TempDir::new().expect("home");
    let database = install_fixture(home.path(), &project);
    let writer = Connection::open(&database).expect("fixture writer");
    writer
        .execute_batch("BEGIN IMMEDIATE")
        .expect("active write transaction");
    writer
        .execute(
            "UPDATE session SET title = 'Uncommitted private title' WHERE id = ?1",
            [FIXTURE_SESSION_ID],
        )
        .expect("uncommitted update");

    let source = source(&project, &database);
    let started = std::time::Instant::now();
    let batch = discover_batch(&source, &project, None, 1);
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(
        extract(&source, &batch).title(),
        Some("Sanitized OpenCode fixture")
    );
    writer.execute_batch("ROLLBACK").expect("rollback writer");
}

#[test]
fn discovery_does_not_create_or_modify_database_files() {
    let (_project_directory, project) = make_project();
    let home = TempDir::new().expect("home");
    let database = install_fixture(home.path(), &project);
    let parent = database.parent().expect("database parent");
    for suffix in ["-wal", "-shm"] {
        let sidecar = PathBuf::from(format!("{}{}", database.display(), suffix));
        if sidecar.exists() {
            fs::remove_file(sidecar).expect("remove stale sidecar");
        }
    }
    let before = fs::metadata(&database).expect("database metadata");
    let before_entries = fs::read_dir(parent)
        .expect("database directory")
        .map(|entry| entry.expect("entry").file_name())
        .collect::<BTreeSet<_>>();

    let source = source(&project, &database);
    let batch = discover_batch(&source, &project, None, 1);
    let _ = extract(&source, &batch);

    let after = fs::metadata(&database).expect("database metadata");
    let after_entries = fs::read_dir(parent)
        .expect("database directory")
        .map(|entry| entry.expect("entry").file_name())
        .collect::<BTreeSet<_>>();
    assert_eq!(before.len(), after.len());
    assert_eq!(
        before.modified().expect("mtime"),
        after.modified().expect("mtime")
    );
    assert_eq!(before_entries, after_entries);
}

#[cfg(unix)]
#[test]
fn symlink_non_regular_and_corrupt_databases_fail_source_locally() {
    use std::os::unix::fs::symlink;
    use std::os::unix::net::UnixListener;

    let (_project_directory, project) = make_project();
    let home = TempDir::new().expect("home");
    let valid = install_fixture(home.path(), &project);
    let alternate = home.path().join("alternate.db");
    fs::copy(&valid, &alternate).expect("alternate database");
    fs::remove_file(&valid).expect("remove database");
    symlink(&alternate, &valid).expect("database symlink");
    let symlink_source = source(&project, &valid);
    let symlink_error = symlink_source.probe().expect_err("symlink must fail");
    assert_eq!(
        symlink_error.kind(),
        ConversationSourceErrorKind::PermissionDenied
    );

    fs::remove_file(&valid).expect("remove symlink");
    let _socket = UnixListener::bind(&valid).expect("database socket");
    let socket_source = source(&project, &valid);
    let socket_error = socket_source.probe().expect_err("socket must fail");
    assert_eq!(
        socket_error.kind(),
        ConversationSourceErrorKind::InvalidData
    );
    drop(_socket);
    fs::remove_file(&valid).expect("remove socket");

    fs::write(&valid, b"not a sqlite database").expect("corrupt database");
    let corrupt_source = source(&project, &valid);
    let corrupt_error = corrupt_source
        .discover(&project, None, DiscoveryLimit::new(1).expect("limit"))
        .expect_err("corrupt database must fail");
    assert_eq!(
        corrupt_error.kind(),
        ConversationSourceErrorKind::MalformedData
    );
}
