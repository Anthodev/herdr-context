mod support;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use herdr_context::conversations::sources::{
    ConversationSource, ConversationSourceErrorKind, DiscoveryLimit, MetadataBudget,
    ProjectEvidenceKind,
};
use herdr_context::conversations::sources::{GenericJsonlSource, ProjectLocalLocation};
use herdr_context::conversations::{ProvenanceKind, ResumeCapability};
use herdr_context::project::ProjectIdentity;
use support::conversation_source::{SourceConformanceCase, assert_source_conforms};
use tempfile::TempDir;

fn source_at(temp: &TempDir) -> (ProjectIdentity, GenericJsonlSource, PathBuf) {
    let project = ProjectIdentity::from_canonical_root(temp.path().to_path_buf()).expect("project");
    let relative = PathBuf::from(".herdr/conversations");
    let directory = temp.path().join(&relative);
    fs::create_dir_all(&directory).expect("conversation directory");
    let source = GenericJsonlSource::new(
        project.clone(),
        [ProjectLocalLocation::new(relative).expect("registered location")],
    )
    .expect("source");
    (project, source, directory)
}

fn record(
    project: &ProjectIdentity,
    session: &str,
    timestamp: &str,
    role: &str,
    body: &str,
) -> String {
    serde_json::json!({
        "session_id": session,
        "cwd": project.root(),
        "timestamp": timestamp,
        "role": role,
        "message": body,
    })
    .to_string()
}

#[test]
fn valid_jsonl_yields_only_display_safe_metadata_and_evidence() {
    let temp = TempDir::new().expect("tempdir");
    let (project, source, directory) = source_at(&temp);
    let secret = "PRIVATE-PROMPT-DO-NOT-RETURN";
    fs::write(
        directory.join("session.jsonl"),
        format!(
            "{}\n{}\n",
            record(
                &project,
                "session-1",
                "2026-01-02T03:04:05Z",
                "user",
                secret
            ),
            record(
                &project,
                "session-1",
                "2026-01-02T03:05:06Z",
                "assistant",
                "PRIVATE-RESPONSE-DO-NOT-RETURN"
            )
        ),
    )
    .expect("fixture");

    let batch = source
        .discover(&project, None, DiscoveryLimit::new(8).expect("limit"))
        .expect("discovery");
    assert!(batch.errors().is_empty());
    assert_eq!(batch.candidates().len(), 1);
    assert!(!format!("{batch:?}").contains(secret));

    let candidate = &batch.candidates()[0];
    let conversation = source
        .extract_metadata(candidate, MetadataBudget::new(256 * 1024).expect("budget"))
        .expect("metadata");
    assert_eq!(conversation.session_reference().id(), "session-1");
    assert_eq!(conversation.tool().as_str(), "generic-jsonl");
    assert_eq!(conversation.title(), None);
    assert!(conversation.created_at().is_some());
    assert!(conversation.updated_at() > conversation.created_at().expect("created"));
    assert_eq!(
        conversation.provenance()[0].kind(),
        ProvenanceKind::ProjectLocal
    );
    assert_eq!(
        conversation.resume_capability(),
        &ResumeCapability::Supported(
            herdr_context::conversations::ResumeReference::new("session-1").expect("resume")
        )
    );
    assert!(!format!("{conversation:?}").contains(secret));

    let evidence = source
        .project_evidence(candidate, &project)
        .expect("evidence");
    assert!(evidence.iter().any(|item| {
        item.kind() == ProjectEvidenceKind::CanonicalWorkingDirectory
            && item.canonical_path() == project.root()
    }));
    assert!(evidence.iter().any(|item| {
        item.kind() == ProjectEvidenceKind::RecognizedProjectLocalPath
            && item.canonical_path() == project.root()
    }));
}

#[test]
fn strict_invariants_isolate_unknown_and_malformed_files() {
    let temp = TempDir::new().expect("tempdir");
    let (project, source, directory) = source_at(&temp);
    fs::write(
        directory.join("valid.jsonl"),
        format!(
            "{}\n",
            record(
                &project,
                "valid-session",
                "2026-01-02T03:04:05Z",
                "user",
                "safe"
            )
        ),
    )
    .expect("valid fixture");
    fs::write(directory.join("malformed.jsonl"), "{not json}\n").expect("malformed fixture");
    fs::write(
        directory.join("wrong-cwd.jsonl"),
        serde_json::json!({
            "session_id": "wrong-cwd",
            "cwd": format!("{}-other", project.root().display()),
            "timestamp": "2026-01-02T03:04:05Z",
            "role": "user",
            "message": "do not associate",
        })
        .to_string(),
    )
    .expect("wrong cwd fixture");
    fs::write(
        directory.join("missing-message.jsonl"),
        serde_json::json!({
            "session_id": "missing-message",
            "cwd": project.root(),
            "timestamp": "2026-01-02T03:04:05Z",
            "role": "user",
        })
        .to_string(),
    )
    .expect("missing message fixture");
    fs::write(
        directory.join("bad-time.jsonl"),
        serde_json::json!({
            "session_id": "bad-time",
            "cwd": project.root(),
            "timestamp": "yesterday",
            "role": "user",
            "message": "invalid timestamp",
        })
        .to_string(),
    )
    .expect("bad timestamp fixture");

    let batch = source
        .discover(&project, None, DiscoveryLimit::new(16).expect("limit"))
        .expect("isolated discovery");
    assert_eq!(batch.candidates().len(), 1);
    assert_eq!(batch.candidates()[0].source_reference(), "valid-session");
    assert_eq!(batch.errors().len(), 4);
    assert!(batch.errors().iter().all(|error| {
        matches!(
            error.kind(),
            ConversationSourceErrorKind::MalformedData | ConversationSourceErrorKind::InvalidData
        )
    }));
}

#[test]
fn partial_tail_and_later_append_are_safe_and_resumable() {
    let temp = TempDir::new().expect("tempdir");
    let (project, source, directory) = source_at(&temp);
    let path = directory.join("append.jsonl");
    let first = record(
        &project,
        "append-session",
        "2026-01-02T03:04:05Z",
        "user",
        "first",
    );
    fs::write(&path, format!("{first}\n{{not json")).expect("partial fixture");

    let initial = source
        .discover(&project, None, DiscoveryLimit::new(4).expect("limit"))
        .expect("initial discovery");
    assert_eq!(initial.candidates().len(), 1);
    assert!(initial.errors().is_empty());
    source
        .extract_metadata(
            &initial.candidates()[0],
            MetadataBudget::new(256 * 1024).expect("budget"),
        )
        .expect("partial final line is ignored");

    let unchanged = source
        .discover(
            &project,
            initial.next_watermark(),
            DiscoveryLimit::new(4).expect("limit"),
        )
        .expect("watermarked discovery");
    assert!(unchanged.candidates().is_empty());

    fs::write(
        &path,
        format!(
            "{first}\n{}\n",
            record(
                &project,
                "append-session",
                "2026-01-02T03:05:06Z",
                "assistant",
                "second"
            )
        ),
    )
    .expect("completed append fixture");
    let appended = source
        .discover(
            &project,
            unchanged.next_watermark(),
            DiscoveryLimit::new(4).expect("limit"),
        )
        .expect("append discovery");
    assert_eq!(appended.candidates().len(), 1);
    let conversation = source
        .extract_metadata(
            &appended.candidates()[0],
            MetadataBudget::new(256 * 1024).expect("budget"),
        )
        .expect("appended metadata");
    assert!(conversation.updated_at() > conversation.created_at().expect("created"));
}

#[test]
fn json_single_record_is_supported_but_complete_transcripts_are_bounded() {
    let temp = TempDir::new().expect("tempdir");
    let (project, source, directory) = source_at(&temp);
    fs::write(
        directory.join("single.json"),
        record(
            &project,
            "json-session",
            "2026-01-02T03:04:05Z",
            "user",
            "single record",
        ),
    )
    .expect("json fixture");
    fs::write(
        directory.join("oversized.jsonl"),
        format!(
            "{}\n",
            record(
                &project,
                "oversized",
                "2026-01-02T03:04:05Z",
                "user",
                &"x".repeat(300 * 1024)
            )
        ),
    )
    .expect("oversized fixture");

    let batch = source
        .discover(&project, None, DiscoveryLimit::new(8).expect("limit"))
        .expect("discovery");
    assert_eq!(batch.candidates().len(), 1);
    assert_eq!(batch.candidates()[0].source_reference(), "json-session");
    assert_eq!(batch.errors().len(), 1);
    assert_eq!(
        batch.errors()[0].kind(),
        ConversationSourceErrorKind::InvalidData
    );
}

#[test]
fn metadata_rejects_candidates_changed_after_discovery() {
    let temp = TempDir::new().expect("tempdir");
    let (project, source, directory) = source_at(&temp);
    let path = directory.join("racing.jsonl");
    fs::write(
        &path,
        format!(
            "{}\n",
            record(&project, "racing", "2026-01-02T03:04:05Z", "user", "before")
        ),
    )
    .expect("fixture");
    let batch = source
        .discover(&project, None, DiscoveryLimit::new(2).expect("limit"))
        .expect("discovery");

    let mut file = OpenOptions::new().append(true).open(&path).expect("append");
    writeln!(
        file,
        "{}",
        record(
            &project,
            "racing",
            "2026-01-02T03:05:06Z",
            "assistant",
            "after"
        )
    )
    .expect("append record");

    let error = source
        .extract_metadata(
            &batch.candidates()[0],
            MetadataBudget::new(256 * 1024).expect("budget"),
        )
        .expect_err("stale candidate must not mix snapshots");
    assert_eq!(error.kind(), ConversationSourceErrorKind::InvalidData);
}

#[test]
fn generic_source_satisfies_the_shared_filesystem_source_contract() {
    let temp = TempDir::new().expect("tempdir");
    let foreign = TempDir::new().expect("foreign tempdir");
    let (project, source, directory) = source_at(&temp);
    fs::write(
        directory.join("conformance.jsonl"),
        format!(
            "{}\n",
            record(
                &project,
                "conformance-session",
                "2026-01-02T03:04:05Z",
                "user",
                "conformance body"
            )
        ),
    )
    .expect("fixture");
    let foreign_project = ProjectIdentity::from_canonical_root(foreign.path().to_path_buf())
        .expect("foreign project");
    let expected_resume = ResumeCapability::Supported(
        herdr_context::conversations::ResumeReference::new("conformance-session").expect("resume"),
    );

    assert_source_conforms(SourceConformanceCase {
        source: &source,
        project: &project,
        foreign_project: &foreign_project,
        metadata_budget: MetadataBudget::new(256 * 1024).expect("budget"),
        expected_resume: &expected_resume,
        expected_evidence_kind: ProjectEvidenceKind::CanonicalWorkingDirectory,
        small_budget_error: ConversationSourceErrorKind::InvalidData,
    });
}

#[cfg(unix)]
#[test]
fn same_length_replacement_with_restored_mtime_invalidates_the_snapshot() {
    let temp = TempDir::new().expect("tempdir");
    let (project, source, directory) = source_at(&temp);
    let path = directory.join("replacement.jsonl");
    let before = format!(
        "{}\n",
        record(
            &project,
            "replacement",
            "2026-01-02T03:04:05Z",
            "user",
            "before"
        )
    );
    let after = format!(
        "{}\n",
        record(
            &project,
            "replacement",
            "2026-01-02T03:05:06Z",
            "user",
            "after!"
        )
    );
    assert_eq!(before.len(), after.len());
    fs::write(&path, before).expect("initial fixture");
    let batch = source
        .discover(&project, None, DiscoveryLimit::new(1).expect("limit"))
        .expect("discovery");
    let original_mtime = fs::metadata(&path)
        .expect("metadata")
        .modified()
        .expect("mtime");

    fs::write(&path, after).expect("replacement fixture");
    OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("replacement file")
        .set_times(std::fs::FileTimes::new().set_modified(original_mtime))
        .expect("restore mtime");

    let error = source
        .extract_metadata(
            &batch.candidates()[0],
            MetadataBudget::new(256 * 1024).expect("budget"),
        )
        .expect_err("replacement must invalidate the candidate");
    assert_eq!(error.kind(), ConversationSourceErrorKind::InvalidData);
}

#[test]
fn duplicate_session_identifiers_are_rejected_as_ambiguous() {
    let temp = TempDir::new().expect("tempdir");
    let (project, source, directory) = source_at(&temp);
    for (name, timestamp) in [
        ("first.jsonl", "2026-01-02T03:04:05Z"),
        ("second.jsonl", "2026-01-02T03:05:06Z"),
    ] {
        fs::write(
            directory.join(name),
            format!(
                "{}\n",
                record(&project, "duplicate", timestamp, "user", "body")
            ),
        )
        .expect("fixture");
    }

    let batch = source
        .discover(&project, None, DiscoveryLimit::new(8).expect("limit"))
        .expect("isolated discovery");
    assert!(batch.candidates().is_empty());
    assert_eq!(batch.errors().len(), 2);
    assert!(batch.errors().iter().all(|error| {
        error.kind() == ConversationSourceErrorKind::InvalidData && error.path().is_some()
    }));
}
