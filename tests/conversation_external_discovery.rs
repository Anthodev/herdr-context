use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use herdr_context::conversations::discovery::discover_conversations;
use herdr_context::conversations::sources::{
    ClaudeCodeSource, CodexCliSource, ConversationSource, ConversationSourceErrorKind,
    DiscoveryLimit, KnownStoreRoots, MetadataBudget, OmpSource, OpenCodeSource, PiSource,
    SourceRegistry, SourceWatermark,
};
use herdr_context::conversations::{ProvenanceKind, ResumeCapability};
use herdr_context::project::ProjectIdentity;
use rusqlite::Connection;
use tempfile::TempDir;

const FIXTURE_ROOT: &str = "tests/fixtures/conversations";

fn fixture(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(FIXTURE_ROOT)
        .join(relative)
}

fn project() -> (TempDir, ProjectIdentity) {
    let directory = TempDir::new().expect("project tempdir");
    let identity = ProjectIdentity::from_canonical_root(directory.path().to_path_buf())
        .expect("canonical project");
    (directory, identity)
}

fn fixture_text(relative: &str, project: &ProjectIdentity) -> String {
    let root = project.root().to_str().expect("UTF-8 test project path");
    fs::read_to_string(fixture(relative))
        .expect("fixture")
        .replace("/workspace/project", root)
}
fn install_opencode_fixture(home: &Path, project: &ProjectIdentity) {
    let destination = home.join(".local/share/opencode/opencode.db");
    fs::create_dir_all(destination.parent().expect("fixture parent")).expect("store");
    fs::copy(fixture("opencode/opencode.db"), &destination).expect("installed OpenCode fixture");
    let root = project.root().to_str().expect("UTF-8 test project path");
    let connection = Connection::open(&destination).expect("OpenCode fixture");
    connection
        .execute("UPDATE project SET worktree = ?1", [root])
        .expect("OpenCode worktree");
    connection
        .execute("UPDATE project_directory SET directory = ?1", [root])
        .expect("OpenCode project directory");
    connection
        .execute("UPDATE session SET directory = ?1", [root])
        .expect("OpenCode session directory");
    connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .expect("OpenCode fixture checkpoint");
}

fn claude_directory(project: &ProjectIdentity) -> String {
    let cwd = project.root().to_string_lossy();
    let encoded = cwd
        .encode_utf16()
        .map(|unit| {
            if u8::try_from(unit).is_ok_and(|byte| byte.is_ascii_alphanumeric()) {
                char::from_u32(u32::from(unit)).expect("ASCII")
            } else {
                '-'
            }
        })
        .collect::<String>();
    if encoded.len() <= 200 {
        return encoded;
    }

    let hash = cwd.encode_utf16().fold(0_i32, |hash, unit| {
        hash.wrapping_mul(31).wrapping_add(i32::from(unit))
    });
    format!(
        "{}-{}",
        &encoded[..200],
        base36(i64::from(hash).unsigned_abs())
    )
}

fn base36(mut value: u64) -> String {
    if value == 0 {
        return "0".to_owned();
    }
    let mut reversed = Vec::new();
    while value != 0 {
        let digit = u8::try_from(value % 36).expect("base-36 digit");
        reversed.push(if digit < 10 {
            b'0' + digit
        } else {
            b'a' + digit - 10
        });
        value /= 36;
    }
    reversed.reverse();
    String::from_utf8(reversed).expect("ASCII")
}

fn pi_directory(project: &ProjectIdentity) -> String {
    format!(
        "--{}--",
        project
            .root()
            .to_string_lossy()
            .trim_start_matches('/')
            .replace('/', "-")
    )
}

fn encode_omp_relative(prefix: &str, relative: &Path) -> String {
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
        return encode_omp_relative("-", relative);
    }
    if let Ok(relative) = project.root().strip_prefix(&temp) {
        return encode_omp_relative("-tmp", relative);
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

fn install_valid_fixtures(home: &Path, project: &ProjectIdentity) {
    let claude = home
        .join(".claude/projects")
        .join(claude_directory(project))
        .join("11111111-1111-4111-8111-111111111111.jsonl");
    let codex = home.join(".codex/sessions/2026/01/02/rollout-2026-01-02T03-04-05-019b7c3b-af88-7000-8001-000000000001.jsonl");
    let pi = home
        .join(".pi/agent/sessions")
        .join(pi_directory(project))
        .join("2026-01-02T03-04-05-000Z_019b7ca9-8c88-7000-8003-000000000003.jsonl");
    let omp = home
        .join(".omp/agent/sessions")
        .join(omp_directory(project, home))
        .join("2026-01-04T05-06-07-000Z_019b8721-4a18-7000-8005-000000000005.jsonl");

    for (destination, relative) in [
        (
            claude,
            "claude-code/-workspace-project/11111111-1111-4111-8111-111111111111.jsonl",
        ),
        (
            codex,
            "codex-cli/2026/01/02/rollout-2026-01-02T03-04-05-019b7c3b-af88-7000-8001-000000000001.jsonl",
        ),
        (
            pi,
            "pi/--workspace-project--/2026-01-02T03-04-05-000Z_019b7ca9-8c88-7000-8003-000000000003.jsonl",
        ),
        (
            omp,
            "omp/--workspace-project--/2026-01-04T05-06-07-000Z_019b8721-4a18-7000-8005-000000000005.jsonl",
        ),
    ] {
        fs::create_dir_all(destination.parent().expect("fixture parent")).expect("store");
        fs::write(destination, fixture_text(relative, project)).expect("installed fixture");
    }
    install_opencode_fixture(home, project);
}

fn discover_one(
    source: &dyn ConversationSource,
    project: &ProjectIdentity,
) -> herdr_context::conversations::Conversation {
    let batch = source
        .discover(
            project,
            None,
            DiscoveryLimit::new(8).expect("non-zero limit"),
        )
        .expect("discovery");
    assert!(batch.errors().is_empty(), "{:?}", batch.errors());
    assert_eq!(batch.candidates().len(), 1);
    let candidate = &batch.candidates()[0];
    let evidence = source
        .project_evidence(candidate, project)
        .expect("canonical evidence");
    assert!(
        evidence
            .iter()
            .any(|item| item.canonical_path() == project.root())
    );
    source
        .extract_metadata(
            candidate,
            MetadataBudget::new(512 * 1024).expect("non-zero budget"),
        )
        .expect("metadata")
}

#[test]
fn registers_exactly_the_five_fixture_backed_external_adapters() {
    let (_project_dir, project) = project();
    let home = TempDir::new().expect("home");
    install_valid_fixtures(home.path(), &project);
    let roots = KnownStoreRoots::under_home(home.path());
    let registry = SourceRegistry::new(roots.sources(project.clone()).expect("known sources"))
        .expect("unique sources");

    let ids = registry
        .iter()
        .map(|source| source.source_id().as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, ["claude-code", "codex-cli", "omp", "opencode", "pi"]);

    let discovery = discover_conversations(
        &registry,
        &project,
        &HashMap::new(),
        DiscoveryLimit::new(8).expect("non-zero limit"),
        MetadataBudget::new(512 * 1024).expect("non-zero budget"),
    );
    assert!(discovery.errors().is_empty(), "{:?}", discovery.errors());
    assert_eq!(discovery.conversations().len(), 5);
    assert_eq!(
        discovery
            .conversations()
            .iter()
            .map(|conversation| conversation.tool().as_str())
            .collect::<Vec<_>>(),
        ["omp", "opencode", "pi", "claude-code", "codex-cli"]
    );
    for conversation in discovery.conversations() {
        match conversation.tool().as_str() {
            "omp" => assert_eq!(conversation.title(), Some("Synthetic OMP session")),
            "opencode" => {
                assert_eq!(conversation.title(), Some("Sanitized OpenCode fixture"));
            }
            _ => assert!(conversation.title().is_none()),
        }
        assert!(matches!(
            conversation.resume_capability(),
            ResumeCapability::Supported(_)
        ));
        assert_eq!(conversation.provenance().len(), 1);
        assert_eq!(
            conversation.provenance()[0].kind(),
            ProvenanceKind::ExternalLocal
        );
    }
}

#[test]
fn codex_current_legacy_history_omits_rollout_ordinals() {
    let (_project_dir, project) = project();
    let home = TempDir::new().expect("home");
    install_valid_fixtures(home.path(), &project);
    let roots = KnownStoreRoots::under_home(home.path());
    let path = roots
        .codex_cli()
        .join("2026/01/02/rollout-2026-01-02T03-04-05-019b7c3b-af88-7000-8001-000000000001.jsonl");
    let mut legacy = String::new();
    for line in fs::read_to_string(&path)
        .expect("paginated fixture")
        .lines()
    {
        let mut record: serde_json::Value = serde_json::from_str(line).expect("fixture record");
        record
            .as_object_mut()
            .expect("record object")
            .remove("ordinal");
        if record["type"] == "session_meta" {
            record["payload"]["history_mode"] = serde_json::json!("legacy");
        }
        legacy.push_str(&record.to_string());
        legacy.push('\n');
    }
    fs::write(&path, legacy).expect("legacy fixture");

    let source =
        CodexCliSource::new(project.clone(), roots.codex_cli().to_path_buf()).expect("source");
    let conversation = discover_one(&source, &project);
    assert_eq!(conversation.tool().as_str(), "codex-cli");
}

#[test]
fn claude_rejects_records_outside_the_verified_current_shapes() {
    for case in [
        "unknown type",
        "attachment identity fields",
        "missing message payload",
        "missing attachment payload",
    ] {
        let (_project_dir, project) = project();
        let home = TempDir::new().expect("home");
        install_valid_fixtures(home.path(), &project);
        let roots = KnownStoreRoots::under_home(home.path());
        let path = roots
            .claude_code()
            .join(claude_directory(&project))
            .join("11111111-1111-4111-8111-111111111111.jsonl");
        let mut records = fs::read_to_string(&path)
            .expect("Claude fixture")
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("record"))
            .collect::<Vec<_>>();
        match case {
            "unknown type" => records[1]["type"] = serde_json::json!("unknown"),
            "attachment identity fields" => {
                records[1]["cwd"] = serde_json::json!(project.root());
            }
            "missing message payload" => {
                records[0]
                    .as_object_mut()
                    .expect("user record")
                    .remove("message");
            }
            "missing attachment payload" => {
                records[1]
                    .as_object_mut()
                    .expect("attachment record")
                    .remove("attachment");
            }
            _ => unreachable!(),
        }
        let invalid = records
            .into_iter()
            .map(|record| format!("{record}\n"))
            .collect::<String>();
        fs::write(&path, invalid).expect("invalid Claude fixture");

        let source = ClaudeCodeSource::new(project.clone(), roots.claude_code().to_path_buf())
            .expect("source");
        let batch = source
            .discover(&project, None, DiscoveryLimit::new(8).expect("limit"))
            .expect("adapter-scoped result");
        assert!(batch.candidates().is_empty(), "{case}");
        assert!(
            batch.errors().iter().any(|error| matches!(
                error.kind(),
                ConversationSourceErrorKind::UnsupportedFormat
            ))
        );
    }
}

#[test]
fn codex_rollout_path_uses_recorded_local_time_without_assuming_an_offset() {
    let (_project_dir, project) = project();
    let home = TempDir::new().expect("home");
    install_valid_fixtures(home.path(), &project);
    let roots = KnownStoreRoots::under_home(home.path());
    let directory = roots.codex_cli().join("2026/01/02");
    let original =
        directory.join("rollout-2026-01-02T03-04-05-019b7c3b-af88-7000-8001-000000000001.jsonl");
    let utc_named =
        directory.join("rollout-2026-01-02T01-04-05-019b7c3b-af88-7000-8001-000000000001.jsonl");
    fs::rename(original, utc_named).expect("timezone-neutral rollout path");

    let source =
        CodexCliSource::new(project.clone(), roots.codex_cli().to_path_buf()).expect("source");
    assert_eq!(
        discover_one(&source, &project).session_reference().id(),
        "019b7c3b-af88-7000-8001-000000000001"
    );
}

#[test]
fn codex_rejects_invalid_current_record_shapes_and_timeline_headers() {
    for case in [
        "response payload",
        "response payload type",
        "event payload type",
        "world-state payload",
        "record-write timestamp",
        "initial ordinal",
    ] {
        let (_project_dir, project) = project();
        let home = TempDir::new().expect("home");
        install_valid_fixtures(home.path(), &project);
        let roots = KnownStoreRoots::under_home(home.path());
        let path = roots.codex_cli().join(
            "2026/01/02/rollout-2026-01-02T03-04-05-019b7c3b-af88-7000-8001-000000000001.jsonl",
        );
        let mut records = fs::read_to_string(&path)
            .expect("Codex fixture")
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("record"))
            .collect::<Vec<_>>();
        match case {
            "response payload" => {
                records[3]["payload"] = serde_json::json!({"type": "message"});
            }
            "event payload type" => {
                records[1]["payload"]["type"] = serde_json::json!("unknown");
            }
            "response payload type" => {
                records[3]["payload"]["type"] = serde_json::json!("future_variant");
            }
            "world-state payload" => {
                records[2]["payload"] = serde_json::json!({});
            }
            "record-write timestamp" => {
                records[0]["timestamp"] = serde_json::json!("2026-01-02T01:04:04.000Z");
            }
            "initial ordinal" => {
                records[0]["ordinal"] = serde_json::json!(1);
                records.truncate(1);
            }
            _ => unreachable!(),
        }
        let invalid = records
            .into_iter()
            .map(|record| format!("{record}\n"))
            .collect::<String>();

        fs::write(&path, invalid).expect("invalid Codex fixture");

        let source =
            CodexCliSource::new(project.clone(), roots.codex_cli().to_path_buf()).expect("source");
        let batch = source
            .discover(&project, None, DiscoveryLimit::new(8).expect("limit"))
            .expect("adapter-scoped result");
        assert!(batch.candidates().is_empty(), "{case}");
        assert!(
            batch.errors().iter().any(|error| matches!(
                error.kind(),
                ConversationSourceErrorKind::UnsupportedFormat
            )),
            "{case}"
        );
    }
}
#[test]
fn pi_rejects_missing_variant_payloads() {
    let (_project_dir, project) = project();
    let home = TempDir::new().expect("home");
    install_valid_fixtures(home.path(), &project);
    let roots = KnownStoreRoots::under_home(home.path());
    let path = roots
        .pi()
        .join(pi_directory(&project))
        .join("2026-01-02T03-04-05-000Z_019b7ca9-8c88-7000-8003-000000000003.jsonl");
    let mut records = fs::read_to_string(&path)
        .expect("Pi fixture")
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("record"))
        .collect::<Vec<_>>();
    records[2]
        .as_object_mut()
        .expect("model change")
        .remove("provider");
    records[2]
        .as_object_mut()
        .expect("model change")
        .remove("modelId");
    let invalid = records
        .into_iter()
        .map(|record| format!("{record}\n"))
        .collect::<String>();
    fs::write(path, invalid).expect("invalid Pi fixture");

    let source = PiSource::new(project.clone(), roots.pi().to_path_buf()).expect("source");
    let batch = source
        .discover(&project, None, DiscoveryLimit::new(8).expect("limit"))
        .expect("adapter-scoped result");
    assert!(batch.candidates().is_empty());
    assert!(
        batch
            .errors()
            .iter()
            .any(|error| matches!(error.kind(), ConversationSourceErrorKind::UnsupportedFormat))
    );
}

#[test]
fn claude_current_long_project_paths_use_the_hashed_directory_layout() {
    let parent = TempDir::new().expect("project parent");
    let project_path = parent.path().join("x".repeat(210));
    fs::create_dir(&project_path).expect("long project directory");
    let project =
        ProjectIdentity::from_canonical_root(project_path).expect("canonical long project");
    let encoded = claude_directory(&project);
    assert!(encoded.len() > 200);
    assert!(encoded.len() <= 255);

    let home = TempDir::new().expect("home");
    let destination = home
        .path()
        .join(".claude/projects")
        .join(&encoded)
        .join("11111111-1111-4111-8111-111111111111.jsonl");
    fs::create_dir_all(destination.parent().expect("fixture parent")).expect("store");
    fs::write(
        destination,
        fixture_text(
            "claude-code/-workspace-project/11111111-1111-4111-8111-111111111111.jsonl",
            &project,
        ),
    )
    .expect("installed fixture");

    let source = ClaudeCodeSource::new(project.clone(), home.path().join(".claude/projects"))
        .expect("Claude source");
    assert_eq!(
        discover_one(&source, &project).session_reference().id(),
        "11111111-1111-4111-8111-111111111111"
    );
}

#[test]
fn adapters_extract_native_identity_time_and_canonical_cwd_evidence() {
    let (_project_dir, project) = project();
    let home = TempDir::new().expect("home");
    install_valid_fixtures(home.path(), &project);
    let roots = KnownStoreRoots::under_home(home.path());

    let cases: Vec<(Box<dyn ConversationSource>, &str, &str)> = vec![
        (
            Box::new(
                ClaudeCodeSource::new(project.clone(), roots.claude_code().to_path_buf())
                    .expect("Claude source"),
            ),
            "11111111-1111-4111-8111-111111111111",
            "claude-code",
        ),
        (
            Box::new(
                CodexCliSource::new(project.clone(), roots.codex_cli().to_path_buf())
                    .expect("Codex source"),
            ),
            "019b7c3b-af88-7000-8001-000000000001",
            "codex-cli",
        ),
        (
            Box::new(
                OmpSource::new(project.clone(), roots.omp().to_path_buf()).expect("OMP source"),
            ),
            "019b8721-4a18-7000-8005-000000000005",
            "omp",
        ),
        (
            Box::new(
                OpenCodeSource::new(project.clone(), roots.opencode().to_path_buf())
                    .expect("OpenCode source"),
            ),
            "ses_ffffffffffffffffffffffffff",
            "opencode",
        ),
        (
            Box::new(PiSource::new(project.clone(), roots.pi().to_path_buf()).expect("Pi source")),
            "019b7ca9-8c88-7000-8003-000000000003",
            "pi",
        ),
    ];

    for (source, expected_id, expected_namespace) in cases {
        let conversation = discover_one(source.as_ref(), &project);
        assert_eq!(conversation.session_reference().id(), expected_id);
        assert_eq!(
            conversation.session_reference().namespace(),
            expected_namespace
        );
        assert!(conversation.created_at().is_some());
        assert!(conversation.updated_at() >= conversation.created_at().expect("created"));
    }
}

#[test]
fn partial_final_records_are_ignored_but_complete_prefixes_are_indexed() {
    let (_project_dir, project) = project();
    let home = TempDir::new().expect("home");
    install_opencode_fixture(home.path(), &project);
    let roots = KnownStoreRoots::under_home(home.path());
    let cases = [
        (
            roots
                .claude_code()
                .join(claude_directory(&project))
                .join("66666666-6666-4666-8666-666666666666.jsonl"),
            "claude-code/-workspace-project/66666666-6666-4666-8666-666666666666.jsonl",
        ),
        (
            roots.codex_cli().join(
                "2026/01/03/rollout-2026-01-03T04-05-06-019b8199-e850-7000-8002-000000000002.jsonl",
            ),
            "codex-cli/2026/01/03/rollout-2026-01-03T04-05-06-019b8199-e850-7000-8002-000000000002.jsonl",
        ),
        (
            roots
                .pi()
                .join(pi_directory(&project))
                .join("2026-01-03T04-05-06-000Z_019b8207-c550-7000-8004-000000000004.jsonl"),
            "pi/--workspace-project--/2026-01-03T04-05-06-000Z_019b8207-c550-7000-8004-000000000004.jsonl",
        ),
        (
            roots
                .omp()
                .join(omp_directory(&project, home.path()))
                .join("2026-01-05T06-07-08-000Z_019b8c49-5e80-7000-8006-000000000006.jsonl"),
            "omp/--workspace-project--/2026-01-05T06-07-08-000Z_019b8c49-5e80-7000-8006-000000000006.jsonl",
        ),
    ];
    for (destination, relative) in cases {
        fs::create_dir_all(destination.parent().expect("parent")).expect("store");
        fs::write(destination, fixture_text(relative, &project)).expect("partial fixture");
    }

    for source in roots.sources(project.clone()).expect("sources") {
        let conversation = discover_one(source.as_ref(), &project);
        assert!(conversation.updated_at() >= conversation.created_at().expect("header timestamp"));
    }
}

#[test]
fn conflicting_cwd_and_malformed_versions_are_adapter_scoped() {
    let (_project_dir, project) = project();
    let foreign = TempDir::new().expect("foreign project");
    let foreign = fs::canonicalize(foreign.path()).expect("canonical foreign path");
    let home = TempDir::new().expect("home");
    install_valid_fixtures(home.path(), &project);
    let roots = KnownStoreRoots::under_home(home.path());

    let claude_path = roots
        .claude_code()
        .join(claude_directory(&project))
        .join("11111111-1111-4111-8111-111111111111.jsonl");
    let claude = fs::read_to_string(&claude_path)
        .expect("Claude fixture")
        .replace(
            project.root().to_str().expect("project path"),
            foreign.to_str().expect("foreign path"),
        );
    fs::write(&claude_path, claude).expect("conflicting Claude fixture");

    let codex_path = roots
        .codex_cli()
        .join("2026/01/02/rollout-2026-01-02T03-04-05-019b7c3b-af88-7000-8001-000000000001.jsonl");
    let codex = fs::read_to_string(&codex_path)
        .expect("Codex fixture")
        .replace("0.147.0", "0.149-current");
    fs::write(&codex_path, codex).expect("malformed Codex fixture");

    let registry =
        SourceRegistry::new(roots.sources(project.clone()).expect("sources")).expect("registry");
    let discovery = discover_conversations(
        &registry,
        &project,
        &HashMap::new(),
        DiscoveryLimit::new(8).expect("limit"),
        MetadataBudget::new(512 * 1024).expect("budget"),
    );

    assert_eq!(
        discovery.conversations().len(),
        3,
        "OMP, OpenCode, and Pi remain healthy"
    );
    assert_eq!(
        discovery
            .conversations()
            .iter()
            .map(|conversation| conversation.tool().as_str())
            .collect::<Vec<_>>(),
        ["omp", "opencode", "pi"]
    );
    assert!(!discovery.errors().iter().any(|error| {
        error.source_id().as_str() == "claude-code"
            && error.kind() == ConversationSourceErrorKind::ProjectMismatch
    }));
    assert!(discovery.errors().iter().any(|error| {
        error.source_id().as_str() == "codex-cli"
            && error.kind() == ConversationSourceErrorKind::UnsupportedFormat
    }));
}

#[test]
fn compatible_patch_versions_are_accepted_by_validated_shape() {
    let (_project_dir, project) = project();
    let home = TempDir::new().expect("home");
    install_valid_fixtures(home.path(), &project);
    let roots = KnownStoreRoots::under_home(home.path());

    let claude_path = roots
        .claude_code()
        .join(claude_directory(&project))
        .join("11111111-1111-4111-8111-111111111111.jsonl");
    let claude = fs::read_to_string(&claude_path)
        .expect("Claude fixture")
        .replace("2.1.232", "2.1.140");
    fs::write(&claude_path, claude).expect("compatible Claude fixture");

    let codex_path = roots
        .codex_cli()
        .join("2026/01/02/rollout-2026-01-02T03-04-05-019b7c3b-af88-7000-8001-000000000001.jsonl");
    let codex = fs::read_to_string(&codex_path)
        .expect("Codex fixture")
        .replace("0.147.0", "0.149.1");
    fs::write(&codex_path, codex).expect("compatible Codex fixture");

    let registry =
        SourceRegistry::new(roots.sources(project.clone()).expect("sources")).expect("registry");
    let discovery = discover_conversations(
        &registry,
        &project,
        &HashMap::new(),
        DiscoveryLimit::new(8).expect("limit"),
        MetadataBudget::new(512 * 1024).expect("budget"),
    );

    assert_eq!(discovery.conversations().len(), 5);
    assert!(discovery.errors().is_empty(), "{:?}", discovery.errors());
}

#[test]
fn adapter_revision_revisits_legacy_cached_rejections() {
    let (_project_dir, project) = project();
    let home = TempDir::new().expect("home");
    install_valid_fixtures(home.path(), &project);
    let roots = KnownStoreRoots::under_home(home.path());
    let source =
        CodexCliSource::new(project.clone(), roots.codex_cli().to_path_buf()).expect("source");
    let first = source
        .discover(
            &project,
            None,
            DiscoveryLimit::new(8).expect("discovery limit"),
        )
        .expect("initial discovery");
    let mut legacy = serde_json::from_str::<serde_json::Value>(
        first.next_watermark().expect("watermark").token(),
    )
    .expect("watermark JSON");
    for entry in legacy.as_object_mut().expect("watermark map").values_mut() {
        entry
            .as_object_mut()
            .expect("watermark entry")
            .remove("adapter_revision");
    }
    let legacy = SourceWatermark::new(
        source.source_id().clone(),
        serde_json::to_string(&legacy).expect("legacy watermark"),
    )
    .expect("source watermark");

    let refreshed = source
        .discover(
            &project,
            Some(&legacy),
            DiscoveryLimit::new(8).expect("discovery limit"),
        )
        .expect("refreshed discovery");

    assert_eq!(refreshed.candidates().len(), 1);
    assert!(refreshed.errors().is_empty(), "{:?}", refreshed.errors());
}

#[test]
fn global_store_overrides_resolve_without_home_scans() {
    let home = TempDir::new().expect("home");
    let codex = TempDir::new().expect("Codex home");
    let claude = TempDir::new().expect("Claude config");

    let roots =
        KnownStoreRoots::with_overrides(home.path(), Some(codex.path()), Some(claude.path()));

    assert_eq!(roots.codex_cli(), codex.path().join("sessions"));
    assert_eq!(roots.claude_code(), claude.path().join("projects"));
    assert_eq!(roots.pi(), home.path().join(".pi/agent/sessions"));
}

#[test]
fn claude_uses_transcript_cwd_when_project_directory_is_overridden() {
    let (_project_dir, project) = project();
    let home = TempDir::new().expect("home");
    let destination = home
        .path()
        .join(".claude/projects/custom-project-key")
        .join("11111111-1111-4111-8111-111111111111.jsonl");
    fs::create_dir_all(destination.parent().expect("fixture parent")).expect("store");
    fs::write(
        destination,
        fixture_text(
            "claude-code/-workspace-project/11111111-1111-4111-8111-111111111111.jsonl",
            &project,
        ),
    )
    .expect("Claude fixture");

    let source = ClaudeCodeSource::new(project.clone(), home.path().join(".claude/projects"))
        .expect("Claude source");

    assert_eq!(
        discover_one(&source, &project).session_reference().id(),
        "11111111-1111-4111-8111-111111111111"
    );
}

#[test]
fn claude_accepts_current_auxiliary_record_shapes() {
    let (_project_dir, project) = project();
    let home = TempDir::new().expect("home");
    let session_id = "11111111-1111-4111-8111-111111111111";
    let destination = home
        .path()
        .join(".claude/projects/custom-project-key")
        .join(format!("{session_id}.jsonl"));
    fs::create_dir_all(destination.parent().expect("fixture parent")).expect("store");
    let cwd = project.root();
    let records = [
        serde_json::json!({
            "type": "last-prompt",
            "leafUuid": "22222222-2222-4222-8222-222222222222",
            "sessionId": session_id,
        }),
        serde_json::json!({
            "type": "permission-mode",
            "permissionMode": "default",
            "sessionId": session_id,
        }),
        serde_json::json!({
            "parentUuid": null,
            "isSidechain": false,
            "attachment": {"type": "hook_success"},
            "cwd": cwd,
            "sessionId": session_id,
            "version": "2.1.140",
            "type": "attachment",
            "uuid": "22222222-2222-4222-8222-222222222222",
            "timestamp": "2026-05-13T17:13:35.288Z",
        }),
        serde_json::json!({
            "type": "file-history-snapshot",
            "messageId": "33333333-3333-4333-8333-333333333333",
            "snapshot": {"trackedFileBackups": {}},
            "isSnapshotUpdate": false,
        }),
        serde_json::json!({
            "type": "ai-title",
            "aiTitle": "sanitized title",
            "sessionId": session_id,
        }),
        serde_json::json!({
            "type": "queue-operation",
            "operation": "enqueue",
            "sessionId": session_id,
            "timestamp": "2026-05-13T17:13:35.788Z",
        }),
        serde_json::json!({
            "parentUuid": "22222222-2222-4222-8222-222222222222",
            "isSidechain": false,
            "subtype": "compact_boundary",
            "cwd": cwd,
            "sessionId": session_id,
            "version": "2.1.140",
            "type": "system",
            "uuid": "44444444-4444-4444-8444-444444444444",
            "timestamp": "2026-05-13T17:13:36.288Z",
        }),
    ];
    let transcript = records
        .iter()
        .map(serde_json::Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(destination, format!("{transcript}\n")).expect("Claude fixture");
    let source = ClaudeCodeSource::new(project.clone(), home.path().join(".claude/projects"))
        .expect("Claude source");

    assert_eq!(
        discover_one(&source, &project).session_reference().id(),
        session_id
    );
}

#[test]
fn claude_accepts_compact_boundaries_that_restart_the_chain() {
    let (_project_dir, project) = project();
    let home = TempDir::new().expect("home");
    let session_id = "11111111-1111-4111-8111-111111111111";
    let destination = home
        .path()
        .join(".claude/projects/custom-project-key")
        .join(format!("{session_id}.jsonl"));
    fs::create_dir_all(destination.parent().expect("fixture parent")).expect("store");
    let cwd = project.root();
    let records = [
        serde_json::json!({
            "parentUuid": null,
            "isSidechain": false,
            "cwd": cwd,
            "sessionId": session_id,
            "version": "2.1.140",
            "type": "user",
            "message": {"role": "user", "content": "before compaction"},
            "uuid": "22222222-2222-4222-8222-222222222222",
            "timestamp": "2026-05-13T17:13:35.288Z",
        }),
        serde_json::json!({
            "parentUuid": null,
            "logicalParentUuid": "22222222-2222-4222-8222-222222222222",
            "isSidechain": false,
            "subtype": "compact_boundary",
            "content": "Conversation compacted",
            "cwd": cwd,
            "sessionId": session_id,
            "version": "2.1.140",
            "type": "system",
            "uuid": "44444444-4444-4444-8444-444444444444",
            "timestamp": "2026-05-13T17:13:36.288Z",
        }),
        serde_json::json!({
            "parentUuid": "44444444-4444-4444-8444-444444444444",
            "isSidechain": false,
            "cwd": cwd,
            "sessionId": session_id,
            "version": "2.1.140",
            "type": "user",
            "message": {"role": "user", "content": "after compaction"},
            "uuid": "55555555-5555-4555-8555-555555555555",
            "timestamp": "2026-05-13T17:13:37.288Z",
        }),
    ];
    let transcript = records
        .iter()
        .map(serde_json::Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(destination, format!("{transcript}\n")).expect("Claude fixture");
    let source = ClaudeCodeSource::new(project.clone(), home.path().join(".claude/projects"))
        .expect("Claude source");
    let batch = source
        .discover(
            &project,
            None,
            DiscoveryLimit::new(8).expect("discovery limit"),
        )
        .expect("discovery");
    assert!(batch.errors().is_empty(), "{:?}", batch.errors());
    assert_eq!(batch.candidates().len(), 1, "boundary session detected");
}

#[test]
fn claude_compact_boundary_without_logical_parent_is_rejected() {
    let (_project_dir, project) = project();
    let home = TempDir::new().expect("home");
    let session_id = "11111111-1111-4111-8111-111111111111";
    let destination = home
        .path()
        .join(".claude/projects/custom-project-key")
        .join(format!("{session_id}.jsonl"));
    fs::create_dir_all(destination.parent().expect("fixture parent")).expect("store");
    let cwd = project.root();
    let records = [
        serde_json::json!({
            "parentUuid": null,
            "isSidechain": false,
            "cwd": cwd,
            "sessionId": session_id,
            "version": "2.1.140",
            "type": "user",
            "message": {"role": "user", "content": "root"},
            "uuid": "22222222-2222-4222-8222-222222222222",
            "timestamp": "2026-05-13T17:13:35.288Z",
        }),
        serde_json::json!({
            "parentUuid": null,
            "isSidechain": false,
            "subtype": "compact_boundary",
            "content": "Conversation compacted",
            "cwd": cwd,
            "sessionId": session_id,
            "version": "2.1.140",
            "type": "system",
            "uuid": "44444444-4444-4444-8444-444444444444",
            "timestamp": "2026-05-13T17:13:36.288Z",
        }),
    ];
    let transcript = records
        .iter()
        .map(serde_json::Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(destination, format!("{transcript}\n")).expect("Claude fixture");
    let source = ClaudeCodeSource::new(project.clone(), home.path().join(".claude/projects"))
        .expect("Claude source");
    let batch = source
        .discover(
            &project,
            None,
            DiscoveryLimit::new(8).expect("discovery limit"),
        )
        .expect("discovery");
    assert!(
        batch.candidates().is_empty(),
        "unanchored boundary must not be detected"
    );
}

#[test]
fn claude_store_directories_are_benign() {
    let (_project_dir, project) = project();
    let home = TempDir::new().expect("home");
    let store = home.path().join(".claude/projects/custom-project-key");
    let destination = store.join("11111111-1111-4111-8111-111111111111.jsonl");
    fs::create_dir_all(destination.parent().expect("store parent")).expect("store");
    fs::write(
        destination,
        fixture_text(
            "claude-code/-workspace-project/11111111-1111-4111-8111-111111111111.jsonl",
            &project,
        ),
    )
    .expect("Claude fixture");
    fs::create_dir_all(store.join("11111111-1111-4111-8111-111111111111/tool-results"))
        .expect("session tool results directory");
    fs::write(
        store.join("11111111-1111-4111-8111-111111111111/tool-results/share.jsonl"),
        [],
    )
    .expect("nested artifact");
    fs::create_dir_all(store.join("memory")).expect("memory directory");
    fs::write(store.join("memory/MEMORY.md"), "# memory").expect("memory file");

    let source = ClaudeCodeSource::new(project.clone(), home.path().join(".claude/projects"))
        .expect("Claude source");
    let batch = source
        .discover(
            &project,
            None,
            DiscoveryLimit::new(8).expect("discovery limit"),
        )
        .expect("discovery");
    assert_eq!(batch.candidates().len(), 1, "session still discovered");
    assert!(batch.errors().is_empty(), "{:?}", batch.errors());
}

#[test]
fn codex_retains_distinct_canonical_origin_directories() {
    let (project_dir, project) = project();
    let origins = [
        project_dir.path().join("one"),
        project_dir.path().join("two"),
    ];
    for origin in &origins {
        fs::create_dir(origin).expect("origin directory");
    }
    let home = TempDir::new().expect("home");
    let store = home.path().join(".codex/sessions");
    let fixture = fixture_text(
        "codex-cli/2026/01/02/rollout-2026-01-02T03-04-05-019b7c3b-af88-7000-8001-000000000001.jsonl",
        &project,
    );
    for (index, origin) in origins.iter().enumerate() {
        let id = format!("019b7c3b-af88-7000-8001-{:012}", index + 1);
        let path = store.join(format!(
            "2026/01/02/rollout-2026-01-02T03-04-0{}-{id}.jsonl",
            index + 5
        ));
        fs::create_dir_all(path.parent().expect("fixture parent")).expect("store");
        fs::write(
            path,
            fixture
                .replace("019b7c3b-af88-7000-8001-000000000001", id.as_str())
                .replace(
                    project.root().to_str().expect("project root"),
                    origin.to_str().expect("origin"),
                ),
        )
        .expect("Codex fixture");
    }
    let source = CodexCliSource::new(project.clone(), store).expect("Codex source");
    let batch = source
        .discover(
            &project,
            None,
            DiscoveryLimit::new(8).expect("discovery limit"),
        )
        .expect("discovery");
    assert!(batch.errors().is_empty(), "{:?}", batch.errors());
    assert_eq!(batch.candidates().len(), 2);

    let mut discovered_origins = batch
        .candidates()
        .iter()
        .map(|candidate| {
            source
                .project_evidence(candidate, &project)
                .expect("evidence")[0]
                .canonical_path()
                .to_path_buf()
        })
        .collect::<Vec<_>>();
    discovered_origins.sort_unstable();
    let mut expected_origins =
        origins.map(|path| fs::canonicalize(path).expect("canonical origin"));
    expected_origins.sort_unstable();
    assert_eq!(discovered_origins, expected_origins);
}

#[test]
fn compressed_codex_rollouts_emit_one_actionable_diagnostic() {
    let (_project_dir, project) = project();
    let home = TempDir::new().expect("home");
    install_valid_fixtures(home.path(), &project);
    let roots = KnownStoreRoots::under_home(home.path());
    let directory = roots.codex_cli().join("2026/01/02");
    for id in ["one", "two"] {
        fs::write(directory.join(format!("rollout-{id}.jsonl.zst")), []).expect("cold rollout");
    }
    let source =
        CodexCliSource::new(project.clone(), roots.codex_cli().to_path_buf()).expect("source");

    let batch = source
        .discover(
            &project,
            None,
            DiscoveryLimit::new(8).expect("discovery limit"),
        )
        .expect("discovery");

    assert_eq!(batch.candidates().len(), 1);
    assert_eq!(batch.errors().len(), 1);
    assert_eq!(
        batch.errors()[0].kind(),
        ConversationSourceErrorKind::UnsupportedFormat
    );
}
#[test]
fn native_filename_hints_must_match_the_verified_exact_grammar() {
    let (_project_dir, project) = project();
    let home = TempDir::new().expect("home");
    install_valid_fixtures(home.path(), &project);
    let roots = KnownStoreRoots::under_home(home.path());
    let codex = roots
        .codex_cli()
        .join("2026/01/02/rollout-2026-01-02T03-04-05-019b7c3b-af88-7000-8001-000000000001.jsonl");
    fs::rename(
        &codex,
        codex.with_file_name(
            "rollout-2026-01-02T03-04-05-019b7c3b-af88-7000-8001-000000000001-copy.jsonl",
        ),
    )
    .expect("rename Codex fixture");
    let pi = roots
        .pi()
        .join(pi_directory(&project))
        .join("2026-01-02T03-04-05-000Z_019b7ca9-8c88-7000-8003-000000000003.jsonl");
    fs::rename(
        &pi,
        pi.with_file_name(
            "2026-01-02T03-04-05-000Z_019b7ca9-8c88-7000-8003-000000000003-copy.jsonl",
        ),
    )
    .expect("rename Pi fixture");
    let omp = roots
        .omp()
        .join(omp_directory(&project, home.path()))
        .join("2026-01-04T05-06-07-000Z_019b8721-4a18-7000-8005-000000000005.jsonl");
    fs::rename(&omp, omp.with_file_name("similar-title.jsonl")).expect("rename OMP fixture");

    let registry =
        SourceRegistry::new(roots.sources(project.clone()).expect("sources")).expect("registry");
    let discovery = discover_conversations(
        &registry,
        &project,
        &HashMap::new(),
        DiscoveryLimit::new(8).expect("limit"),
        MetadataBudget::new(512 * 1024).expect("budget"),
    );
    assert_eq!(
        discovery.conversations().len(),
        2,
        "Claude and OpenCode remain healthy"
    );
    for source_id in ["codex-cli", "omp", "pi"] {
        assert!(discovery.errors().iter().any(|error| {
            error.source_id().as_str() == source_id
                && error.kind() == ConversationSourceErrorKind::UnsupportedFormat
        }));
    }
}

#[cfg(unix)]
#[test]
fn special_store_entries_are_reported_as_unsupported_shapes() {
    use std::os::unix::net::UnixListener;

    let (_project_dir, project) = project();
    let home = TempDir::new().expect("home");
    install_valid_fixtures(home.path(), &project);
    let roots = KnownStoreRoots::under_home(home.path());
    let socket_path = roots
        .claude_code()
        .join(claude_directory(&project))
        .join("unexpected.socket");
    let _socket = UnixListener::bind(&socket_path).expect("fixture socket");
    let source = ClaudeCodeSource::new(project.clone(), roots.claude_code().to_path_buf())
        .expect("Claude source");

    let batch = source
        .discover(&project, None, DiscoveryLimit::new(8).expect("limit"))
        .expect("bounded discovery");
    assert!(batch.errors().iter().any(|error| {
        error.kind() == ConversationSourceErrorKind::UnsupportedFormat
            && error.path() == Some(socket_path.as_path())
    }));
}

#[test]
fn known_store_discovery_honors_preexisting_cancellation() {
    let (_project_dir, project) = project();
    let home = TempDir::new().expect("home");
    install_valid_fixtures(home.path(), &project);
    let roots = KnownStoreRoots::under_home(home.path());
    let source = ClaudeCodeSource::new(project.clone(), roots.claude_code().to_path_buf())
        .expect("Claude source");
    let cancelled = AtomicBool::new(true);
    let error = source
        .discover_cancellable(
            &project,
            None,
            DiscoveryLimit::new(8).expect("limit"),
            &cancelled,
        )
        .expect_err("cancelled discovery");
    assert_eq!(error.kind(), ConversationSourceErrorKind::Io);
}

#[test]
fn claude_first_page_uses_modification_time_not_uuid_filename_order() {
    let (_project_dir, project) = project();
    let home = TempDir::new().expect("home");
    install_valid_fixtures(home.path(), &project);
    let roots = KnownStoreRoots::under_home(home.path());
    let directory = roots.claude_code().join(claude_directory(&project));
    std::thread::sleep(std::time::Duration::from_millis(5));
    let second_id = "22222222-2222-4222-8222-222222222222";
    let second = fixture_text(
        "claude-code/-workspace-project/11111111-1111-4111-8111-111111111111.jsonl",
        &project,
    )
    .replace("11111111-1111-4111-8111-111111111111", second_id);
    fs::write(directory.join(format!("{second_id}.jsonl")), second).expect("newer Claude session");

    let source = ClaudeCodeSource::new(project.clone(), roots.claude_code().to_path_buf())
        .expect("Claude source");
    let batch = source
        .discover(&project, None, DiscoveryLimit::new(1).expect("limit"))
        .expect("recent page");
    assert!(batch.has_more());
    assert_eq!(batch.candidates().len(), 1);
    assert_eq!(batch.candidates()[0].source_reference(), second_id);
}
#[test]
fn unchanged_rejected_sessions_do_not_starve_older_valid_pages() {
    let (_project_dir, project) = project();
    let home = TempDir::new().expect("home");
    install_valid_fixtures(home.path(), &project);
    let roots = KnownStoreRoots::under_home(home.path());
    let directory = roots.claude_code().join(claude_directory(&project));
    std::thread::sleep(std::time::Duration::from_millis(5));
    fs::write(
        directory.join("99999999-9999-4999-8999-999999999999.jsonl"),
        "{}\n",
    )
    .expect("newer rejected session");

    let source = ClaudeCodeSource::new(project.clone(), roots.claude_code().to_path_buf())
        .expect("Claude source");
    let first = source
        .discover(&project, None, DiscoveryLimit::new(1).expect("limit"))
        .expect("rejected page");
    assert!(first.candidates().is_empty());
    assert_eq!(first.errors().len(), 1);
    assert!(first.has_more());

    let second = source
        .discover(
            &project,
            first.next_watermark(),
            DiscoveryLimit::new(1).expect("limit"),
        )
        .expect("older page");
    assert_eq!(second.candidates().len(), 1);
    assert_eq!(
        second.candidates()[0].source_reference(),
        "11111111-1111-4111-8111-111111111111"
    );
    assert_eq!(second.errors().len(), 1);
    assert!(matches!(
        second.errors()[0].kind(),
        ConversationSourceErrorKind::UnsupportedFormat
    ));
}

#[cfg(unix)]
#[test]
fn unchanged_transient_read_failures_are_retried() {
    use std::os::unix::fs::PermissionsExt;

    let (_project_dir, project) = project();
    let home = TempDir::new().expect("home");
    install_valid_fixtures(home.path(), &project);
    let roots = KnownStoreRoots::under_home(home.path());
    let path = roots
        .claude_code()
        .join(claude_directory(&project))
        .join("11111111-1111-4111-8111-111111111111.jsonl");
    let mut permissions = fs::metadata(&path).expect("fixture metadata").permissions();
    permissions.set_mode(0o000);
    fs::set_permissions(&path, permissions).expect("make fixture unreadable");

    let source = ClaudeCodeSource::new(project.clone(), roots.claude_code().to_path_buf())
        .expect("Claude source");
    let first = source
        .discover(&project, None, DiscoveryLimit::new(1).expect("limit"))
        .expect("transient failure page");
    assert!(first.candidates().is_empty());
    assert!(first.errors().iter().any(|error| {
        error.kind() == ConversationSourceErrorKind::PermissionDenied
            && error.path() == Some(path.as_path())
    }));

    let mut permissions = fs::metadata(&path).expect("fixture metadata").permissions();
    permissions.set_mode(0o600);
    fs::set_permissions(&path, permissions).expect("restore fixture permissions");
    let retried = source
        .discover(
            &project,
            first.next_watermark(),
            DiscoveryLimit::new(1).expect("limit"),
        )
        .expect("retried page");
    assert_eq!(retried.candidates().len(), 1);
}

#[cfg(unix)]
#[test]
fn incomplete_store_inventory_preserves_prior_sessions() {
    use std::os::unix::fs::PermissionsExt;

    let (_project_dir, project) = project();
    let home = TempDir::new().expect("home");
    install_valid_fixtures(home.path(), &project);
    let roots = KnownStoreRoots::under_home(home.path());
    let source = ClaudeCodeSource::new(project.clone(), roots.claude_code().to_path_buf())
        .expect("Claude source");
    let first = source
        .discover(&project, None, DiscoveryLimit::new(8).expect("limit"))
        .expect("initial discovery");
    let directory = roots.claude_code().join(claude_directory(&project));
    let mut permissions = fs::metadata(&directory)
        .expect("project store metadata")
        .permissions();
    permissions.set_mode(0o000);
    fs::set_permissions(&directory, permissions).expect("make project store unreadable");

    let inaccessible = source.discover(
        &project,
        first.next_watermark(),
        DiscoveryLimit::new(8).expect("limit"),
    );
    let mut permissions = fs::metadata(&directory)
        .expect("project store metadata")
        .permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&directory, permissions).expect("restore project store permissions");
    let inaccessible = inaccessible.expect("adapter-scoped inventory failure");

    assert!(inaccessible.candidates().is_empty());
    assert!(inaccessible.removals().is_empty());
    assert!(inaccessible.errors().iter().any(|error| matches!(
        error.kind(),
        ConversationSourceErrorKind::PermissionDenied | ConversationSourceErrorKind::Io
    )));
    assert_eq!(
        inaccessible.next_watermark().map(|value| value.token()),
        first.next_watermark().map(|value| value.token())
    );
}

#[test]
fn file_cap_is_applied_after_selecting_the_most_recent_sessions() {
    let (_project_dir, project) = project();
    let home = TempDir::new().expect("home");
    let roots = KnownStoreRoots::under_home(home.path());
    let directory = roots.claude_code().join(claude_directory(&project));
    fs::create_dir_all(&directory).expect("Claude store");
    let original_id = "11111111-1111-4111-8111-111111111111";
    let template = fixture_text(
        "claude-code/-workspace-project/11111111-1111-4111-8111-111111111111.jsonl",
        &project,
    );
    for index in 0_u64..=4_096 {
        let id = format!("{index:08x}-0000-4000-8000-{index:012x}");
        fs::write(
            directory.join(format!("{id}.jsonl")),
            template.replace(original_id, &id),
        )
        .expect("session fixture");
    }
    std::thread::sleep(std::time::Duration::from_millis(5));
    let newest_id = "00000000-0000-4000-8000-000000000000";
    fs::write(
        directory.join(format!("{newest_id}.jsonl")),
        template.replace(original_id, newest_id),
    )
    .expect("newest session");

    let source = ClaudeCodeSource::new(project.clone(), roots.claude_code().to_path_buf())
        .expect("Claude source");
    let batch = source
        .discover(&project, None, DiscoveryLimit::new(4_096).expect("limit"))
        .expect("bounded inventory");
    assert_eq!(batch.candidates().len(), 4_096);
    assert!(
        batch
            .candidates()
            .iter()
            .any(|candidate| candidate.source_reference() == newest_id)
    );
    assert!(
        batch
            .errors()
            .iter()
            .any(|error| matches!(error.kind(), ConversationSourceErrorKind::InvalidData))
    );
    std::thread::sleep(std::time::Duration::from_millis(5));
    let added_id = "ffffffff-ffff-4fff-8fff-ffffffffffff";
    fs::write(
        directory.join(format!("{added_id}.jsonl")),
        template.replace(original_id, added_id),
    )
    .expect("added session");
    let churn = source
        .discover(
            &project,
            batch.next_watermark(),
            DiscoveryLimit::new(1).expect("limit"),
        )
        .expect("bounded churn");
    let encoded: serde_json::Value =
        serde_json::from_str(churn.next_watermark().expect("churn watermark").token())
            .expect("watermark JSON");
    assert_eq!(encoded.as_object().expect("watermark object").len(), 4_096);
    std::thread::sleep(std::time::Duration::from_millis(5));
    fs::write(directory.join(format!("{added_id}.jsonl")), "{}\n").expect("reject current session");
    let rejected = source
        .discover(
            &project,
            churn.next_watermark(),
            DiscoveryLimit::new(1).expect("limit"),
        )
        .expect("current rejection under incomplete inventory");
    assert!(
        rejected
            .removals()
            .iter()
            .any(|removal| { removal.session_reference().id() == added_id })
    );
    source
        .discover(
            &project,
            rejected.next_watermark(),
            DiscoveryLimit::new(1).expect("limit"),
        )
        .expect("bounded watermark remains decodable");
}

#[test]
fn truncated_codex_directory_inventory_preserves_prior_sessions() {
    let (_project_dir, project) = project();
    let home = TempDir::new().expect("home");
    let roots = KnownStoreRoots::under_home(home.path());
    let id = "019b7c3b-af88-7000-8001-000000000001";
    let initial = roots
        .codex_cli()
        .join(format!("0000/01/02/rollout-0000-01-02T03-04-05-{id}.jsonl"));
    fs::create_dir_all(initial.parent().expect("parent")).expect("Codex store");
    fs::write(
        &initial,
        fixture_text(
            "codex-cli/2026/01/02/rollout-2026-01-02T03-04-05-019b7c3b-af88-7000-8001-000000000001.jsonl",
            &project,
        ),
    )
    .expect("Codex fixture");
    let source = CodexCliSource::new(project.clone(), roots.codex_cli().to_path_buf())
        .expect("Codex source");
    let first = source
        .discover(&project, None, DiscoveryLimit::new(8).expect("limit"))
        .expect("initial discovery");
    assert_eq!(first.candidates().len(), 1);

    for year in 1..=2_000 {
        fs::create_dir(roots.codex_cli().join(format!("{year:04}")))
            .expect("bounded year directory");
    }
    let truncated = source
        .discover(
            &project,
            first.next_watermark(),
            DiscoveryLimit::new(8).expect("limit"),
        )
        .expect("incomplete traversal");
    assert!(truncated.candidates().is_empty());
    assert!(truncated.removals().is_empty());
    assert!(
        truncated
            .errors()
            .iter()
            .any(|error| matches!(error.kind(), ConversationSourceErrorKind::InvalidData))
    );
    assert_eq!(
        truncated.next_watermark().map(|value| value.token()),
        first.next_watermark().map(|value| value.token())
    );
}

#[test]
fn duplicate_session_recovery_reindexes_the_surviving_file() {
    let (_project_dir, project) = project();
    let home = TempDir::new().expect("home");
    install_valid_fixtures(home.path(), &project);
    let roots = KnownStoreRoots::under_home(home.path());
    let source =
        CodexCliSource::new(project.clone(), roots.codex_cli().to_path_buf()).expect("source");
    let first = source
        .discover(&project, None, DiscoveryLimit::new(8).expect("limit"))
        .expect("initial discovery");
    let id = "019b7c3b-af88-7000-8001-000000000001";
    let original = roots
        .codex_cli()
        .join(format!("2026/01/02/rollout-2026-01-02T03-04-05-{id}.jsonl"));
    let duplicate = roots
        .codex_cli()
        .join(format!("2026/01/04/rollout-2026-01-04T03-04-05-{id}.jsonl"));
    fs::create_dir_all(duplicate.parent().expect("parent")).expect("duplicate directory");
    fs::copy(&original, &duplicate).expect("duplicate session");

    let conflicted = source
        .discover(
            &project,
            first.next_watermark(),
            DiscoveryLimit::new(8).expect("limit"),
        )
        .expect("duplicate discovery");
    assert!(conflicted.candidates().is_empty());
    assert_eq!(conflicted.removals().len(), 1);
    fs::remove_file(duplicate).expect("remove duplicate");

    let recovered = source
        .discover(
            &project,
            conflicted.next_watermark(),
            DiscoveryLimit::new(8).expect("limit"),
        )
        .expect("survivor recovery");
    assert_eq!(recovered.candidates().len(), 1);
    assert_eq!(recovered.candidates()[0].source_reference(), id);
}

#[test]
fn size_mtime_fingerprints_and_watermarks_revisit_only_changed_sessions() {
    let (_project_dir, project) = project();
    let home = TempDir::new().expect("home");
    install_valid_fixtures(home.path(), &project);
    let roots = KnownStoreRoots::under_home(home.path());
    let source = ClaudeCodeSource::new(project.clone(), roots.claude_code().to_path_buf())
        .expect("Claude source");

    let first = source
        .discover(&project, None, DiscoveryLimit::new(8).expect("limit"))
        .expect("initial discovery");
    assert_eq!(first.candidates().len(), 1);
    let watermark = first.next_watermark().expect("watermark").clone();
    let unchanged = source
        .discover(
            &project,
            Some(&watermark),
            DiscoveryLimit::new(8).expect("limit"),
        )
        .expect("incremental discovery");
    assert!(unchanged.candidates().is_empty());

    let path = roots
        .claude_code()
        .join(claude_directory(&project))
        .join("11111111-1111-4111-8111-111111111111.jsonl");
    let mut content = fs::read_to_string(&path).expect("fixture");
    content.push_str("{\"partial\":");
    fs::write(&path, content).expect("append partial record");

    let changed = source
        .discover(
            &project,
            unchanged.next_watermark(),
            DiscoveryLimit::new(8).expect("limit"),
        )
        .expect("changed discovery");
    assert_eq!(changed.candidates().len(), 1);
    let conversation = source
        .extract_metadata(
            &changed.candidates()[0],
            MetadataBudget::new(512 * 1024).expect("budget"),
        )
        .expect("complete prefix remains valid");
    assert_eq!(
        conversation.session_reference().id(),
        "11111111-1111-4111-8111-111111111111"
    );
}

#[test]
fn large_transcripts_are_validated_incrementally_and_appends_resume_from_the_watermark() {
    let (_project_dir, project) = project();
    let home = TempDir::new().expect("home");
    install_valid_fixtures(home.path(), &project);
    let roots = KnownStoreRoots::under_home(home.path());
    let path = roots
        .claude_code()
        .join(claude_directory(&project))
        .join("11111111-1111-4111-8111-111111111111.jsonl");
    let existing = fs::read_to_string(&path).expect("fixture");
    let mut parent = serde_json::from_str::<serde_json::Value>(
        existing.lines().last().expect("last fixture record"),
    )
    .expect("fixture JSON")["uuid"]
        .as_str()
        .expect("fixture UUID")
        .to_owned();
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("append fixture");
    let padding = "x".repeat(2_048);
    for index in 0..600_u64 {
        let uuid = format!("00000000-0000-4000-8000-{index:012x}");
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "parentUuid": parent,
                "isSidechain": false,
                "cwd": project.root(),
                "sessionId": "11111111-1111-4111-8111-111111111111",
                "version": "2.1.232",
                "type": "assistant",
                "message": {"role": "assistant", "content": padding},
                "uuid": uuid,
                "timestamp": "2026-01-02T03:04:06.000Z"
            })
        )
        .expect("append record");
        parent = uuid;
    }
    file.flush().expect("flush transcript");
    assert!(fs::metadata(&path).expect("metadata").len() > 512 * 1024);

    let source = ClaudeCodeSource::new(project.clone(), roots.claude_code().to_path_buf())
        .expect("Claude source");
    let limit = DiscoveryLimit::new(8).expect("limit");
    let mut after = None;
    let mut final_batch = None;
    for _ in 0..8 {
        let batch = source
            .discover(&project, after.as_ref(), limit)
            .expect("incremental page");
        after = batch.next_watermark().cloned();
        if !batch.has_more() {
            final_batch = Some(batch);
            break;
        }
        assert!(batch.candidates().is_empty());
    }
    let final_batch = final_batch.expect("bounded scan eventually completes");
    assert_eq!(final_batch.candidates().len(), 1);
    source
        .extract_metadata(
            &final_batch.candidates()[0],
            MetadataBudget::new(512 * 1024).expect("budget"),
        )
        .expect("large transcript metadata");

    let uuid = "00000000-0000-4000-8000-000000000600";
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "parentUuid": parent,
            "isSidechain": false,
            "cwd": project.root(),
            "sessionId": "11111111-1111-4111-8111-111111111111",
            "version": "2.1.232",
            "type": "user",
            "message": {"role": "user", "content": "append"},
            "uuid": uuid,
            "timestamp": "2026-01-02T03:04:07.000Z"
        })
    )
    .expect("append incremental record");
    file.flush().expect("flush append");
    let appended = source
        .discover(&project, after.as_ref(), limit)
        .expect("append discovery");
    assert!(!appended.has_more());
    assert_eq!(appended.candidates().len(), 1);
}

#[test]
fn in_place_prefix_rewrites_are_not_resumed_from_stale_metadata() {
    let (_project_dir, project) = project();
    let home = TempDir::new().expect("home");
    install_valid_fixtures(home.path(), &project);
    let roots = KnownStoreRoots::under_home(home.path());
    let source = ClaudeCodeSource::new(project.clone(), roots.claude_code().to_path_buf())
        .expect("Claude source");
    let first = source
        .discover(&project, None, DiscoveryLimit::new(8).expect("limit"))
        .expect("initial discovery");
    let path = roots
        .claude_code()
        .join(claude_directory(&project))
        .join("11111111-1111-4111-8111-111111111111.jsonl");
    let original = fs::read_to_string(&path).expect("fixture");
    let rewritten = original.replace(
        "11111111-1111-4111-8111-111111111111",
        "99999999-9999-4999-8999-999999999999",
    );
    let appended = format!(
        "{rewritten}{}\n",
        serde_json::json!({
            "parentUuid": "55555555-5555-4555-8555-555555555555",
            "isSidechain": false,
            "userType": "external",
            "cwd": project.root(),
            "sessionId": "99999999-9999-4999-8999-999999999999",
            "version": "2.1.232",
            "type": "assistant",
            "message": {
                "id": "synthetic-rewrite",
                "type": "message",
                "role": "assistant",
                "content": [{"type": "text", "text": "sanitized rewrite"}]
            },
            "uuid": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            "timestamp": "2026-01-02T03:04:08.000Z"
        })
    );
    fs::write(&path, appended).expect("in-place rewrite and append");

    let changed = source
        .discover(
            &project,
            first.next_watermark(),
            DiscoveryLimit::new(8).expect("limit"),
        )
        .expect("adapter-scoped rewrite result");
    assert!(changed.candidates().is_empty());
    assert!(
        changed
            .errors()
            .iter()
            .any(|error| matches!(error.kind(), ConversationSourceErrorKind::UnsupportedFormat))
    );
}

#[test]
fn oversized_unterminated_append_keeps_the_complete_prefix_without_repeating_the_page() {
    let (_project_dir, project) = project();
    let home = TempDir::new().expect("home");
    install_valid_fixtures(home.path(), &project);
    let roots = KnownStoreRoots::under_home(home.path());
    let source = ClaudeCodeSource::new(project.clone(), roots.claude_code().to_path_buf())
        .expect("Claude source");
    let first = source
        .discover(&project, None, DiscoveryLimit::new(8).expect("limit"))
        .expect("initial discovery");
    let path = roots
        .claude_code()
        .join(claude_directory(&project))
        .join("11111111-1111-4111-8111-111111111111.jsonl");
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(path)
        .expect("append fixture");
    file.write_all(&vec![b'x'; 600 * 1024])
        .expect("oversized partial record");

    let changed = source
        .discover(
            &project,
            first.next_watermark(),
            DiscoveryLimit::new(8).expect("limit"),
        )
        .expect("bounded partial-tail handling");
    assert!(!changed.has_more());
    assert!(changed.errors().is_empty());
    assert_eq!(changed.candidates().len(), 1);
    let conversation = source
        .extract_metadata(
            &changed.candidates()[0],
            MetadataBudget::new(512 * 1024).expect("budget"),
        )
        .expect("complete prefix");
    assert_eq!(
        conversation.session_reference().id(),
        "11111111-1111-4111-8111-111111111111"
    );

    file.write_all(b"\n").expect("complete oversized record");
    file.flush().expect("flush oversized record");
    let completed = source
        .discover(
            &project,
            changed.next_watermark(),
            DiscoveryLimit::new(8).expect("limit"),
        )
        .expect("adapter-scoped oversized-record result");
    assert!(!completed.has_more());
    assert!(completed.candidates().is_empty());
    assert!(
        completed
            .errors()
            .iter()
            .any(|error| matches!(error.kind(), ConversationSourceErrorKind::UnsupportedFormat))
    );
}
