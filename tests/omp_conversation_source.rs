mod support;

use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use herdr_context::conversations::index::ConversationIndex;
use herdr_context::conversations::sources::{
    ConversationSource, ConversationSourceErrorKind, DiscoveryLimit, MetadataBudget, OmpSource,
    ProjectEvidenceKind, SourceRegistry, SourceWatermark, StorageProbe,
};
use herdr_context::conversations::{ProvenanceKind, ResumeCapability, ResumeReference};
use herdr_context::project::ProjectIdentity;
use support::conversation_source::{SourceConformanceCase, assert_source_conforms};
use tempfile::TempDir;

const FIXTURE_ROOT: &str = "tests/fixtures/conversations";
const VALID_FIXTURE: &str =
    "omp/--workspace-project--/2026-01-04T05-06-07-000Z_019b8721-4a18-7000-8005-000000000005.jsonl";
const PARTIAL_FIXTURE: &str =
    "omp/--workspace-project--/2026-01-05T06-07-08-000Z_019b8c49-5e80-7000-8006-000000000006.jsonl";
const VALID_ID: &str = "019b8721-4a18-7000-8005-000000000005";
const PARTIAL_ID: &str = "019b8c49-5e80-7000-8006-000000000006";

fn fixture(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(FIXTURE_ROOT)
        .join(relative)
}

fn make_project() -> (TempDir, ProjectIdentity) {
    let directory = TempDir::new().expect("project tempdir");
    let identity = ProjectIdentity::from_canonical_root(directory.path().to_path_buf())
        .expect("canonical project");
    (directory, identity)
}

fn omp_root(home: &Path) -> PathBuf {
    home.join(".omp/agent/sessions")
}

fn encode_relative(prefix: &str, relative: &Path) -> String {
    let encoded = relative.to_string_lossy().replace(['/', '\\', ':'], "-");
    if encoded.is_empty() {
        prefix.to_owned()
    } else if prefix.ends_with('-') {
        format!("{prefix}{encoded}")
    } else {
        format!("{prefix}-{encoded}")
    }
}

fn omp_directory(project: &ProjectIdentity, home: &Path) -> String {
    let home = fs::canonicalize(home).expect("canonical home");
    let temp = fs::canonicalize(std::env::temp_dir()).expect("canonical temp root");
    if let Ok(relative) = project.root().strip_prefix(&home) {
        return encode_relative("-", relative);
    }
    if let Ok(relative) = project.root().strip_prefix(&temp) {
        return encode_relative("-tmp", relative);
    }
    format!(
        "--{}--",
        project
            .root()
            .to_string_lossy()
            .trim_start_matches(['/', '\\'])
            .replace(['/', '\\', ':'], "-")
    )
}

fn fixture_text(relative: &str, project: &ProjectIdentity) -> String {
    fs::read_to_string(fixture(relative))
        .expect("fixture")
        .replace(
            "/workspace/project",
            project.root().to_str().expect("UTF-8 test project"),
        )
}

fn title_slot_line(title: &str, updated_at: &str) -> String {
    let mut slot = serde_json::json!({
        "type": "title",
        "v": 1,
        "title": title,
        "source": "user",
        "updatedAt": updated_at,
        "pad": "",
    });
    let unpadded = serde_json::to_string(&slot).expect("unpadded title slot");
    let padding = 255usize
        .checked_sub(unpadded.len())
        .expect("title fits fixed slot");
    slot["pad"] = serde_json::Value::String(" ".repeat(padding));
    let line = serde_json::to_string(&slot).expect("padded title slot");
    assert_eq!(line.len(), 255, "title slot payload width");
    format!("{line}\n")
}

fn install_fixture(home: &Path, project: &ProjectIdentity, relative: &str) -> PathBuf {
    let destination = omp_root(home)
        .join(omp_directory(project, home))
        .join(fixture(relative).file_name().expect("fixture filename"));
    fs::create_dir_all(destination.parent().expect("fixture parent")).expect("OMP store");
    fs::write(&destination, fixture_text(relative, project)).expect("installed OMP fixture");
    destination
}

fn source(home: &Path, project: &ProjectIdentity) -> OmpSource {
    OmpSource::new(project.clone(), omp_root(home)).expect("OMP source")
}

fn discover_one(
    source: &dyn ConversationSource,
    project: &ProjectIdentity,
) -> herdr_context::conversations::Conversation {
    let batch = source
        .discover(
            project,
            None,
            DiscoveryLimit::new(8).expect("discovery limit"),
        )
        .expect("OMP discovery");
    assert!(batch.errors().is_empty(), "{:?}", batch.errors());
    assert_eq!(batch.candidates().len(), 1);
    source
        .extract_metadata(
            &batch.candidates()[0],
            MetadataBudget::new(512 * 1024).expect("metadata budget"),
        )
        .expect("OMP metadata")
}

fn cache_file(state_dir: &Path) -> PathBuf {
    let conversations = state_dir.join("conversations");
    let project_dir = fs::read_dir(conversations)
        .expect("index root")
        .next()
        .expect("project cache")
        .expect("project cache entry")
        .path();
    fs::read_dir(project_dir)
        .expect("project cache directory")
        .map(|entry| entry.expect("cache entry").path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .expect("cache generation")
}

#[test]
fn omp_source_conforms_and_extracts_only_approved_metadata() {
    let (_project_dir, project) = make_project();
    let (_foreign_dir, foreign_project) = make_project();
    let home = TempDir::new().expect("home");
    install_fixture(home.path(), &project, VALID_FIXTURE);
    let source = source(home.path(), &project);
    let expected_resume = ResumeCapability::Supported(
        ResumeReference::new(VALID_ID).expect("valid native resume reference"),
    );

    assert_source_conforms(SourceConformanceCase {
        source: &source,
        project: &project,
        foreign_project: &foreign_project,
        metadata_budget: MetadataBudget::new(512 * 1024).expect("metadata budget"),
        expected_resume: &expected_resume,
        expected_evidence_kind: ProjectEvidenceKind::AdapterValidatedEncoding,
        small_budget_error: ConversationSourceErrorKind::InvalidData,
    });

    let conversation = discover_one(&source, &project);
    assert_eq!(conversation.tool().as_str(), "omp");
    assert_eq!(conversation.session_reference().id(), VALID_ID);
    assert_eq!(conversation.title(), Some("Synthetic OMP session"));
    assert!(conversation.created_at().is_some());
    assert_eq!(conversation.provenance().len(), 1);
    assert_eq!(
        conversation.provenance()[0].kind(),
        ProvenanceKind::ExternalLocal
    );
}

#[test]
fn schema_three_watermark_without_chain_timestamp_remains_compatible() {
    let (_project_dir, project) = make_project();
    let home = TempDir::new().expect("home");
    install_fixture(home.path(), &project, VALID_FIXTURE);
    let source = source(home.path(), &project);
    let initial = source
        .discover(
            &project,
            None,
            DiscoveryLimit::new(8).expect("discovery limit"),
        )
        .expect("initial OMP discovery");
    let mut token: serde_json::Value =
        serde_json::from_str(initial.next_watermark().expect("OMP watermark").token())
            .expect("watermark JSON");
    for entry in token
        .as_object_mut()
        .expect("watermark object")
        .values_mut()
    {
        let summary = entry
            .get_mut("summary")
            .and_then(serde_json::Value::as_object_mut)
            .expect("stored metadata");
        assert!(summary.remove("chain_updated_at").is_some());
    }
    let legacy = SourceWatermark::new(
        source.source_id().clone(),
        serde_json::to_string(&token).expect("legacy watermark JSON"),
    )
    .expect("legacy watermark");

    let resumed = source
        .discover(
            &project,
            Some(&legacy),
            DiscoveryLimit::new(8).expect("discovery limit"),
        )
        .expect("legacy watermark discovery");
    assert!(resumed.errors().is_empty(), "{:?}", resumed.errors());
    assert!(resumed.candidates().is_empty());
}

#[test]
fn later_root_after_leaf_reset_is_supported() {
    let (_project_dir, project) = make_project();
    let home = TempDir::new().expect("home");
    let path = install_fixture(home.path(), &project, VALID_FIXTURE);
    let contents = fs::read_to_string(&path)
        .expect("OMP fixture")
        .replace("\"parentId\":\"0c1d2e3f\"", "\"parentId\":null");
    fs::write(path, contents).expect("OMP reset-leaf fixture");

    let conversation = discover_one(&source(home.path(), &project), &project);
    assert_eq!(conversation.session_reference().id(), VALID_ID);
}

#[test]
fn execution_message_without_optional_exit_code_is_supported() {
    let (_project_dir, project) = make_project();
    let home = TempDir::new().expect("home");
    let path = install_fixture(home.path(), &project, VALID_FIXTURE);
    let contents = fs::read_to_string(&path).expect("OMP fixture").replace(
        "{\"role\":\"user\",\"content\":\"sanitized OMP user message\",\"timestamp\":1767503169000}",
        "{\"role\":\"bashExecution\",\"command\":\"synthetic command\",\"output\":\"synthetic output\",\"cancelled\":false,\"truncated\":false,\"timestamp\":1767503169000}",
    );
    fs::write(path, contents).expect("OMP execution-message fixture");

    let conversation = discover_one(&source(home.path(), &project), &project);
    assert_eq!(conversation.session_reference().id(), VALID_ID);
}

#[test]
fn developer_message_is_supported() {
    let (_project_dir, project) = make_project();
    let home = TempDir::new().expect("home");
    let path = install_fixture(home.path(), &project, VALID_FIXTURE);
    let contents = fs::read_to_string(&path)
        .expect("OMP fixture")
        .replace("\"role\":\"user\"", "\"role\":\"developer\"");
    fs::write(path, contents).expect("OMP developer-message fixture");

    let conversation = discover_one(&source(home.path(), &project), &project);
    assert_eq!(conversation.session_reference().id(), VALID_ID);
}

#[test]
fn partial_tail_is_ignored_then_completed_append_is_discovered_incrementally() {
    let (_project_dir, project) = make_project();
    let home = TempDir::new().expect("home");
    let path = install_fixture(home.path(), &project, PARTIAL_FIXTURE);
    let source = source(home.path(), &project);

    let initial = source
        .discover(
            &project,
            None,
            DiscoveryLimit::new(8).expect("discovery limit"),
        )
        .expect("initial discovery");
    assert!(initial.errors().is_empty(), "{:?}", initial.errors());
    assert_eq!(initial.candidates().len(), 1);
    assert_eq!(initial.candidates()[0].source_reference(), PARTIAL_ID);
    let watermark = initial.next_watermark().expect("OMP watermark").clone();

    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(path)
        .expect("partial OMP session");
    writeln!(
        file,
        "{{\"role\":\"user\",\"content\":\"sanitized appended OMP message\",\"timestamp\":1767593229000}}}}"
    )
    .expect("complete appended OMP entry");

    let appended = source
        .discover(
            &project,
            Some(&watermark),
            DiscoveryLimit::new(8).expect("discovery limit"),
        )
        .expect("incremental OMP discovery");
    assert!(appended.errors().is_empty(), "{:?}", appended.errors());
    assert_eq!(appended.candidates().len(), 1);
    let conversation = source
        .extract_metadata(
            &appended.candidates()[0],
            MetadataBudget::new(512 * 1024).expect("metadata budget"),
        )
        .expect("appended OMP metadata");
    assert_eq!(conversation.title(), Some("Synthetic partial OMP session"));
}

#[test]
fn fixed_title_slot_replacement_is_observed_during_append_discovery() {
    let (_project_dir, project) = make_project();
    let home = TempDir::new().expect("home");
    let path = install_fixture(home.path(), &project, VALID_FIXTURE);
    let source = source(home.path(), &project);
    let initial = source
        .discover(
            &project,
            None,
            DiscoveryLimit::new(8).expect("discovery limit"),
        )
        .expect("initial OMP discovery");
    let watermark = initial.next_watermark().expect("OMP watermark").clone();

    let mut contents = fs::read(&path).expect("OMP fixture");
    contents.splice(
        ..256,
        title_slot_line("Updated synthetic OMP session", "2026-01-04T05:06:12.000Z").bytes(),
    );
    contents.extend_from_slice(
        b"{\"type\":\"reset_boundary\",\"id\":\"6c7d8e9f\",\"parentId\":\"5c6d7e8f\",\"timestamp\":\"2026-01-04T05:06:12.000Z\"}\n",
    );
    fs::write(path, contents).expect("updated OMP fixture");

    let updated = source
        .discover(
            &project,
            Some(&watermark),
            DiscoveryLimit::new(8).expect("discovery limit"),
        )
        .expect("updated OMP discovery");
    assert!(updated.errors().is_empty(), "{:?}", updated.errors());
    assert_eq!(updated.candidates().len(), 1);
    let conversation = source
        .extract_metadata(
            &updated.candidates()[0],
            MetadataBudget::new(512 * 1024).expect("metadata budget"),
        )
        .expect("updated OMP metadata");
    assert_eq!(conversation.title(), Some("Updated synthetic OMP session"));
}

#[test]
fn discovery_is_recent_first_and_resumes_older_sessions_on_the_next_page() {
    let (_project_dir, project) = make_project();
    let home = TempDir::new().expect("home");
    let older = install_fixture(home.path(), &project, VALID_FIXTURE);
    thread::sleep(Duration::from_millis(5));
    let newer = install_fixture(home.path(), &project, PARTIAL_FIXTURE);
    assert!(
        fs::metadata(newer)
            .expect("newer metadata")
            .modified()
            .expect("newer mtime")
            >= fs::metadata(older)
                .expect("older metadata")
                .modified()
                .expect("older mtime")
    );
    let source = source(home.path(), &project);

    let first = source
        .discover(&project, None, DiscoveryLimit::new(1).expect("page limit"))
        .expect("first OMP page");
    assert_eq!(first.candidates().len(), 1);
    assert_eq!(first.candidates()[0].source_reference(), PARTIAL_ID);
    assert!(first.has_more());

    let second = source
        .discover(
            &project,
            first.next_watermark(),
            DiscoveryLimit::new(1).expect("page limit"),
        )
        .expect("second OMP page");
    assert_eq!(second.candidates().len(), 1);
    assert_eq!(second.candidates()[0].source_reference(), VALID_ID);
}

#[test]
fn nested_child_runs_symlinks_and_unregistered_files_are_not_traversed() {
    let (_project_dir, project) = make_project();
    let home = TempDir::new().expect("home");
    let valid = install_fixture(home.path(), &project, VALID_FIXTURE);
    let bucket = valid.parent().expect("OMP bucket");
    let child = bucket.join(valid.file_stem().expect("session stem"));
    fs::create_dir_all(&child).expect("child-run directory");
    fs::write(child.join("child.jsonl"), "private child-run sentinel\n")
        .expect("child-run fixture");
    fs::write(bucket.join("notes.txt"), "private unrelated sentinel\n")
        .expect("unregistered fixture");
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let outside = home.path().join("outside.jsonl");
        fs::write(&outside, "private escaped sentinel\n").expect("outside fixture");
        symlink(outside, bucket.join("escape.jsonl")).expect("symlink fixture");
    }

    let batch = source(home.path(), &project)
        .discover(
            &project,
            None,
            DiscoveryLimit::new(8).expect("discovery limit"),
        )
        .expect("shape-isolated discovery");
    assert_eq!(batch.candidates().len(), 1);
    assert_eq!(batch.candidates()[0].source_reference(), VALID_ID);
    assert!(batch.errors().len() >= 2);
    assert!(
        batch.errors().iter().all(|error| {
            error.source_id().as_str() == "omp"
                && error.kind() == ConversationSourceErrorKind::UnsupportedFormat
        }),
        "{:?}",
        batch.errors()
    );
}

#[test]
fn unsupported_title_header_cwd_and_filename_variants_are_scoped_to_the_candidate() {
    let cases = [
        (
            "\"v\":1",
            "\"v\":2",
            ConversationSourceErrorKind::UnsupportedFormat,
        ),
        (
            "\"version\":3",
            "\"version\":4",
            ConversationSourceErrorKind::UnsupportedFormat,
        ),
        (
            "\"cwd\":\"/workspace/project\"",
            "\"cwd\":\"/workspace/other\"",
            ConversationSourceErrorKind::ProjectMismatch,
        ),
    ];

    for (needle, replacement, expected) in cases {
        let (_project_dir, project) = make_project();
        let home = TempDir::new().expect("home");
        let path = install_fixture(home.path(), &project, VALID_FIXTURE);
        let contents = fs::read_to_string(&path)
            .expect("OMP fixture")
            .replace(needle, replacement)
            .replace(
                project.root().to_str().expect("UTF-8 test project"),
                if expected == ConversationSourceErrorKind::ProjectMismatch {
                    "/workspace/other"
                } else {
                    project.root().to_str().expect("UTF-8 test project")
                },
            );
        fs::write(&path, contents).expect("mutated OMP fixture");

        let batch = source(home.path(), &project)
            .discover(
                &project,
                None,
                DiscoveryLimit::new(8).expect("discovery limit"),
            )
            .expect("candidate-scoped rejection");
        assert!(batch.candidates().is_empty());
        assert!(
            batch
                .errors()
                .iter()
                .any(|error| { error.source_id().as_str() == "omp" && error.kind() == expected })
        );
    }

    let (_project_dir, project) = make_project();
    let home = TempDir::new().expect("home");
    let path = install_fixture(home.path(), &project, VALID_FIXTURE);
    let wrong_name = path.with_file_name("similar-title.jsonl");
    fs::rename(path, wrong_name).expect("wrong filename fixture");
    let batch = source(home.path(), &project)
        .discover(
            &project,
            None,
            DiscoveryLimit::new(8).expect("discovery limit"),
        )
        .expect("filename-scoped rejection");
    assert!(batch.candidates().is_empty());
    assert!(batch.errors().iter().any(|error| {
        error.source_id().as_str() == "omp"
            && error.kind() == ConversationSourceErrorKind::UnsupportedFormat
    }));
}

#[test]
fn missing_store_is_non_fatal_and_similar_bucket_names_are_not_project_evidence() {
    let (_project_dir, project) = make_project();
    let home = TempDir::new().expect("home");
    let source = source(home.path(), &project);
    assert!(matches!(
        source.probe().expect("missing OMP probe"),
        StorageProbe::Unavailable { .. }
    ));

    let similar =
        omp_root(home.path()).join(format!("{}-similar", omp_directory(&project, home.path())));
    fs::create_dir_all(&similar).expect("similar bucket");
    fs::copy(fixture(VALID_FIXTURE), similar.join("similar-title.jsonl")).expect("similar fixture");
    let batch = source
        .discover(
            &project,
            None,
            DiscoveryLimit::new(8).expect("discovery limit"),
        )
        .expect("similar-bucket discovery");
    assert!(batch.candidates().is_empty());
}

#[test]
fn late_title_timestamp_does_not_break_multi_window_discovery() {
    let (_project_dir, project) = make_project();
    let home = TempDir::new().expect("home");
    let path = install_fixture(home.path(), &project, VALID_FIXTURE);
    let template = fixture_text(VALID_FIXTURE, &project);
    let second_newline = template
        .match_indices('\n')
        .nth(1)
        .map(|(index, _)| index + 1)
        .expect("title slot and header");
    let file = fs::File::create(path).expect("large OMP session");
    let mut writer = BufWriter::new(file);
    writer
        .write_all(&template.as_bytes()[..second_newline])
        .expect("OMP prefix");
    for index in 1_u32..=7_000 {
        let id = format!("{index:08x}");
        let parent = if index == 1 {
            "null".to_owned()
        } else {
            format!("\"{:08x}\"", index - 1)
        };
        writeln!(
            writer,
            "{{\"type\":\"reset_boundary\",\"id\":\"{id}\",\"parentId\":{parent},\"timestamp\":\"2026-01-04T05:06:08.000Z\"}}"
        )
        .expect("large OMP entry");
    }
    writer.flush().expect("large OMP session");

    let source = source(home.path(), &project);
    let mut after = None;
    for _ in 0..8 {
        let batch = source
            .discover(
                &project,
                after.as_ref(),
                DiscoveryLimit::new(1).expect("discovery limit"),
            )
            .expect("multi-window OMP discovery");
        assert!(batch.errors().is_empty(), "{:?}", batch.errors());
        if !batch.candidates().is_empty() {
            let conversation = source
                .extract_metadata(
                    &batch.candidates()[0],
                    MetadataBudget::new(512 * 1024).expect("metadata budget"),
                )
                .expect("large OMP metadata");
            assert_eq!(conversation.title(), Some("Synthetic OMP session"));
            return;
        }
        assert!(batch.has_more(), "incomplete scan must remain resumable");
        after = batch.next_watermark().cloned();
    }
    panic!("large OMP session was not discovered");
}

#[test]
fn total_record_bound_rejects_an_oversized_session_without_retaining_entries() {
    let (_project_dir, project) = make_project();
    let home = TempDir::new().expect("home");
    let path = install_fixture(home.path(), &project, VALID_FIXTURE);
    let file = fs::OpenOptions::new()
        .append(true)
        .open(path)
        .expect("OMP session");
    let mut writer = BufWriter::new(file);
    let mut parent = "5c6d7e8f";
    for index in 0..=32_760 {
        let id = if index % 2 == 0 {
            "6c7d8e9f"
        } else {
            "7c8d9e0f"
        };
        writeln!(
            writer,
            "{{\"type\":\"reset_boundary\",\"id\":\"{id}\",\"parentId\":\"{parent}\",\"timestamp\":\"2026-01-04T05:06:11.000Z\"}}"
        )
        .expect("bounded OMP record");
        parent = id;
    }
    writer.flush().expect("oversized OMP fixture");

    let source = source(home.path(), &project);
    let mut after = None;
    let mut rejected = false;
    for _ in 0..16 {
        let batch = source
            .discover(
                &project,
                after.as_ref(),
                DiscoveryLimit::new(1).expect("discovery limit"),
            )
            .expect("bounded OMP discovery");
        assert!(batch.candidates().is_empty());
        if !batch.errors().is_empty() {
            assert!(batch.errors().iter().any(|error| {
                error.source_id().as_str() == "omp"
                    && error.kind() == ConversationSourceErrorKind::UnsupportedFormat
            }));
            rejected = true;
            break;
        }
        assert!(batch.has_more(), "oversized scan must not publish early");
        after = batch.next_watermark().cloned();
    }
    assert!(rejected, "oversized OMP record set must be rejected");
}

#[test]
fn index_persists_the_title_but_not_conversation_bodies() {
    let (_project_dir, project) = make_project();
    let home = TempDir::new().expect("home");
    install_fixture(home.path(), &project, VALID_FIXTURE);
    let registry =
        SourceRegistry::new(vec![Box::new(source(home.path(), &project))]).expect("OMP registry");
    let state = TempDir::new().expect("state");
    let state_dir = state.path().join("plugin-state");
    let mut index = ConversationIndex::open(&state_dir, project.clone()).expect("OMP index");
    let refresh = index
        .refresh_page(
            &registry,
            DiscoveryLimit::new(8).expect("discovery limit"),
            MetadataBudget::new(512 * 1024).expect("metadata budget"),
        )
        .expect("OMP index refresh");
    assert!(refresh.errors().is_empty(), "{:?}", refresh.errors());
    assert_eq!(
        index.page(0, 8).conversations()[0].title(),
        Some("Synthetic OMP session")
    );

    let raw = fs::read_to_string(cache_file(&state_dir)).expect("OMP cache");
    assert!(raw.contains("Synthetic OMP session"));
    assert!(!raw.contains("sanitized OMP user message"));
    assert!(!raw.contains("sanitized OMP assistant message"));
    let loaded = ConversationIndex::open(&state_dir, project).expect("reloaded OMP index");
    assert_eq!(
        loaded.page(0, 8).conversations()[0].title(),
        Some("Synthetic OMP session")
    );
}
