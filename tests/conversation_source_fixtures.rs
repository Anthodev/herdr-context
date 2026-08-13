use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};

const FIXTURE_ROOT: &str = "tests/fixtures/conversations";

struct FixtureCase {
    source: &'static str,
    valid: &'static str,
    partial: &'static str,
    partial_tail: &'static str,
    allowed_strings: &'static [&'static str],
}

const CLAUDE: FixtureCase = FixtureCase {
    source: "claude-code",
    valid: "claude-code/-workspace-project/11111111-1111-4111-8111-111111111111.jsonl",
    partial: "claude-code/-workspace-project/66666666-6666-4666-8666-666666666666.jsonl",
    partial_tail: "{\"parentUuid\":\"77777777-7777-4777-8777-777777777777\",\"isSidechain\":false,\"type\":\"assistant\",\"message\":",
    allowed_strings: &[
        "/workspace/project",
        "11111111-1111-4111-8111-111111111111",
        "2.1.143",
        "2026-01-02T03:04:05.000Z",
        "2026-01-02T03:04:06.000Z",
        "2026-01-03T03:04:05.000Z",
        "22222222-2222-4222-8222-222222222222",
        "33333333-3333-4333-8333-333333333333",
        "66666666-6666-4666-8666-666666666666",
        "77777777-7777-4777-8777-777777777777",
        "assistant",
        "cli",
        "end_turn",
        "external",
        "main",
        "message",
        "sanitized assistant message",
        "sanitized user message",
        "standard",
        "synthetic",
        "synthetic-message-1",
        "synthetic-model",
        "synthetic-prompt-1",
        "synthetic-prompt-2",
        "synthetic-request-1",
        "text",
        "user",
    ],
};

const CODEX: FixtureCase = FixtureCase {
    source: "codex-cli",
    valid: "codex-cli/2026/01/02/rollout-2026-01-02T03-04-05-019b7c3b-af88-7000-8001-000000000001.jsonl",
    partial: "codex-cli/2026/01/03/rollout-2026-01-03T04-05-06-019b8199-e850-7000-8002-000000000002.jsonl",
    partial_tail: "{\"timestamp\":\"2026-01-03T02:06:14.000Z\",\"type\":\"response_item\",\"payload\":",
    allowed_strings: &[
        "/workspace/project",
        "0.136.0",
        "0000000000000000000000000000000000000000",
        "019b7c3b-af88-7000-8001-000000000001",
        "019b8199-e850-7000-8002-000000000002",
        "2026-01-02T01:04:05.000Z",
        "2026-01-02T01:05:12.000Z",
        "2026-01-02T01:05:13.000Z",
        "2026-01-02T01:05:14.000Z",
        "2026-01-03T02:05:06.000Z",
        "2026-01-03T02:06:13.000Z",
        "assistant",
        "cli",
        "codex-tui",
        "event_msg",
        "https://example.invalid/sanitized.git",
        "message",
        "output_text",
        "response_item",
        "sanitized assistant message",
        "sanitized instructions",
        "sanitized user message",
        "session_meta",
        "synthetic-provider",
        "user_message",
    ],
};

const PI: FixtureCase = FixtureCase {
    source: "pi",
    valid: "pi/--workspace-project--/2026-01-02T03-04-05-000Z_019b7ca9-8c88-7000-8003-000000000003.jsonl",
    partial: "pi/--workspace-project--/2026-01-03T04-05-06-000Z_019b8207-c550-7000-8004-000000000004.jsonl",
    partial_tail: "{\"type\":\"message\",\"id\":\"2a3b4c5d\",\"parentId\":null,\"timestamp\":\"2026-01-03T04:05:07.000Z\",\"message\":",
    allowed_strings: &[
        "/workspace/project",
        "019b7ca9-8c88-7000-8003-000000000003",
        "019b8207-c550-7000-8004-000000000004",
        "0a1b2c3d",
        "1a2b3c4d",
        "2026-01-02T03:04:05.000Z",
        "2026-01-02T03:04:06.000Z",
        "2026-01-02T03:04:07.000Z",
        "2026-01-03T04:05:06.000Z",
        "assistant",
        "message",
        "sanitized assistant message",
        "sanitized user message",
        "session",
        "stop",
        "synthetic-api",
        "synthetic-model",
        "synthetic-provider",
        "text",
        "user",
    ],
};

const CASES: [&FixtureCase; 3] = [&CLAUDE, &CODEX, &PI];

fn fixture(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(FIXTURE_ROOT)
        .join(relative)
}

fn fixture_lines(relative: &str) -> Vec<String> {
    fs::read_to_string(fixture(relative))
        .expect("fixture must be readable")
        .lines()
        .map(str::to_owned)
        .collect()
}

fn parse_complete_records(case: &FixtureCase) -> Vec<Value> {
    fixture_lines(case.valid)
        .iter()
        .map(|line| serde_json::from_str(line).expect("complete fixture line must be JSON"))
        .collect()
}

fn parse_partial_header(case: &FixtureCase) -> Value {
    serde_json::from_str(&fixture_lines(case.partial)[0])
        .expect("partial fixture must start with a complete header")
}

fn field<'a>(value: &'a Value, pointer: &str) -> &'a Value {
    value.pointer(pointer).expect("required fixture field")
}

fn assert_rfc3339_timestamp(value: &Value) {
    let value = value.as_str().expect("timestamp must be a string");
    OffsetDateTime::parse(value, &Rfc3339).expect("timestamp must conform to RFC 3339");
}

fn assert_uuid_version(value: &Value, version: char) {
    let value = value.as_str().expect("ID must be a string");
    assert_eq!(value.len(), 36);
    assert_eq!(
        value
            .chars()
            .enumerate()
            .filter_map(|(index, character)| (character == '-').then_some(index))
            .collect::<Vec<_>>(),
        [8, 13, 18, 23]
    );
    assert!(
        value
            .chars()
            .filter(|character| *character != '-')
            .all(|character| character.is_ascii_hexdigit())
    );
    assert_eq!(value.chars().nth(14), Some(version));
    assert!(matches!(value.chars().nth(19), Some('8'..='b')));
}

fn assert_lower_hex_entry_id(value: &Value) {
    let value = value.as_str().expect("entry ID must be a string");
    assert_eq!(value.len(), 8);
    assert!(
        value
            .chars()
            .all(|character| character.is_ascii_digit() || matches!(character, 'a'..='f'))
    );
}

fn collect_files(root: &Path, directory: &Path, output: &mut BTreeSet<PathBuf>) {
    for entry in fs::read_dir(directory).expect("fixture directory must be readable") {
        let path = entry.expect("fixture entry must be readable").path();
        if path.is_dir() {
            collect_files(root, &path, output);
        } else {
            output.insert(
                path.strip_prefix(root)
                    .expect("fixture must remain below source root")
                    .to_path_buf(),
            );
        }
    }
}

fn collect_strings(value: &Value, output: &mut BTreeSet<String>) {
    match value {
        Value::Array(values) => {
            for value in values {
                collect_strings(value, output);
            }
        }
        Value::Object(fields) => {
            for value in fields.values() {
                collect_strings(value, output);
            }
        }
        Value::String(value) => {
            output.insert(value.clone());
        }
        _ => {}
    }
}

fn assert_filename_contains_header_identity(case: &FixtureCase, relative: &str, header: &Value) {
    let path = Path::new(relative);
    let file_name = path
        .file_name()
        .expect("fixture filename")
        .to_string_lossy();
    match case.source {
        "claude-code" => {
            assert_eq!(
                path.parent()
                    .and_then(Path::file_name)
                    .and_then(|name| name.to_str()),
                Some("-workspace-project")
            );
            let session_id = field(header, "/sessionId");
            assert_uuid_version(session_id, '4');
            assert_eq!(
                path.file_stem().and_then(|name| name.to_str()),
                session_id.as_str()
            );
        }
        "codex-cli" => {
            let id = field(header, "/payload/id");
            assert_uuid_version(id, '7');
            let id = id.as_str().expect("Codex ID");
            let payload_timestamp = OffsetDateTime::parse(
                field(header, "/payload/timestamp")
                    .as_str()
                    .expect("Codex payload timestamp"),
                &Rfc3339,
            )
            .expect("Codex payload timestamp must conform to RFC 3339");
            let filename_time = payload_timestamp
                .to_offset(UtcOffset::from_hms(2, 0, 0).expect("valid fixture-local UTC offset"));
            let date_directory = format!(
                "{:04}/{:02}/{:02}",
                filename_time.year(),
                u8::from(filename_time.month()),
                filename_time.day()
            );
            let filename_prefix = format!(
                "rollout-{:04}-{:02}-{:02}T{:02}-{:02}-{:02}-",
                filename_time.year(),
                u8::from(filename_time.month()),
                filename_time.day(),
                filename_time.hour(),
                filename_time.minute(),
                filename_time.second()
            );
            assert_eq!(
                path.parent()
                    .expect("date directory")
                    .strip_prefix("codex-cli")
                    .expect("Codex source prefix"),
                Path::new(&date_directory)
            );
            assert!(file_name.starts_with(&filename_prefix));
            assert!(file_name.ends_with(&format!("-{id}.jsonl")));
        }
        "pi" => {
            let id = field(header, "/id");
            assert_uuid_version(id, '7');
            let id = id.as_str().expect("Pi ID");
            let timestamp = field(header, "/timestamp").as_str().expect("Pi timestamp");
            assert_eq!(
                path.parent()
                    .and_then(Path::file_name)
                    .and_then(|name| name.to_str()),
                Some("--workspace-project--")
            );
            assert!(file_name.ends_with(&format!("_{id}.jsonl")));
            assert!(file_name.contains(&timestamp.replace([':', '.'], "-")));
        }
        source => panic!("unexpected fixture source {source}"),
    }
}

#[test]
fn inventory_reproduces_exactly_the_three_observed_store_layouts()
-> Result<(), Box<dyn std::error::Error>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_ROOT);
    let source_names: BTreeSet<_> = fs::read_dir(&root)?
        .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
        .collect::<Result<_, _>>()?;
    assert_eq!(
        source_names,
        CASES.iter().map(|case| case.source.to_owned()).collect()
    );

    for case in CASES {
        let source_root = root.join(case.source);
        let mut paths = BTreeSet::new();
        collect_files(&source_root, &source_root, &mut paths);
        let expected = [case.valid, case.partial]
            .into_iter()
            .map(|relative| {
                Path::new(relative)
                    .strip_prefix(case.source)
                    .expect("source-relative fixture")
                    .to_path_buf()
            })
            .collect();
        assert_eq!(paths, expected);

        let valid_header = parse_complete_records(case).remove(0);
        assert_filename_contains_header_identity(case, case.valid, &valid_header);
        let partial_header = parse_partial_header(case);
        assert_filename_contains_header_identity(case, case.partial, &partial_header);
    }
    Ok(())
}

#[test]
fn claude_fixture_matches_the_observed_jsonl_variant() {
    let records = parse_complete_records(&CLAUDE);
    assert_eq!(records.len(), 2, "fixture must represent appended records");
    assert_eq!(field(&records[0], "/type"), "user");
    assert_eq!(field(&records[1], "/type"), "assistant");
    assert_eq!(field(&records[0], "/version"), "2.1.143");
    assert_eq!(field(&records[1], "/version"), "2.1.143");
    assert_eq!(field(&records[0], "/cwd"), "/workspace/project");
    assert_eq!(
        field(&records[0], "/sessionId"),
        field(&records[1], "/sessionId")
    );
    assert_uuid_version(field(&records[0], "/sessionId"), '4');
    assert_uuid_version(field(&records[0], "/uuid"), '4');
    assert_eq!(
        field(&records[1], "/parentUuid"),
        field(&records[0], "/uuid")
    );
    assert_rfc3339_timestamp(field(&records[0], "/timestamp"));
    assert_rfc3339_timestamp(field(&records[1], "/timestamp"));
}

#[test]
fn codex_fixture_matches_the_observed_jsonl_variant() {
    let records = parse_complete_records(&CODEX);
    assert_eq!(records.len(), 3, "fixture must represent appended records");
    assert_eq!(field(&records[0], "/type"), "session_meta");
    assert_eq!(field(&records[1], "/type"), "event_msg");
    assert_eq!(field(&records[1], "/payload/type"), "user_message");
    assert_eq!(field(&records[2], "/type"), "response_item");
    assert_eq!(field(&records[2], "/payload/type"), "message");
    assert_eq!(field(&records[0], "/payload/cli_version"), "0.136.0");
    assert_eq!(field(&records[0], "/payload/cwd"), "/workspace/project");
    assert_uuid_version(field(&records[0], "/payload/id"), '7');
    for record in &records {
        assert_rfc3339_timestamp(field(record, "/timestamp"));
    }
    let payload_timestamp = OffsetDateTime::parse(
        field(&records[0], "/payload/timestamp")
            .as_str()
            .expect("Codex payload timestamp"),
        &Rfc3339,
    )
    .expect("Codex payload timestamp must conform to RFC 3339");
    let record_timestamp = OffsetDateTime::parse(
        field(&records[0], "/timestamp")
            .as_str()
            .expect("Codex record timestamp"),
        &Rfc3339,
    )
    .expect("Codex record timestamp must conform to RFC 3339");
    assert!(
        record_timestamp > payload_timestamp,
        "Codex record write time must remain distinct from session-start time"
    );
}

#[test]
fn pi_fixture_matches_the_observed_jsonl_variant() {
    let records = parse_complete_records(&PI);
    assert_eq!(records.len(), 3, "fixture must represent appended records");
    assert_eq!(field(&records[0], "/type"), "session");
    assert_eq!(field(&records[0], "/version"), 3);
    assert_eq!(field(&records[0], "/cwd"), "/workspace/project");
    assert_uuid_version(field(&records[0], "/id"), '7');
    assert_eq!(field(&records[1], "/type"), "message");
    assert_eq!(field(&records[2], "/type"), "message");
    assert_eq!(field(&records[2], "/parentId"), field(&records[1], "/id"));
    assert_lower_hex_entry_id(field(&records[1], "/id"));
    assert_lower_hex_entry_id(field(&records[2], "/id"));
    for record in &records {
        assert_rfc3339_timestamp(field(record, "/timestamp"));
    }
    for record in &records[1..] {
        let timestamp = field(record, "/message/timestamp")
            .as_i64()
            .expect("Pi message timestamp must be integral epoch milliseconds");
        assert!(timestamp > 0);
    }
}

#[test]
fn partial_fixtures_end_with_the_expected_malformed_append()
-> Result<(), Box<dyn std::error::Error>> {
    for case in CASES {
        let lines = fixture_lines(case.partial);
        assert_eq!(lines.len(), 2, "{} partial fixture shape", case.source);
        serde_json::from_str::<Value>(&lines[0])?;
        assert_eq!(lines[1], case.partial_tail);
        assert!(serde_json::from_str::<Value>(&lines[1]).is_err());
    }
    Ok(())
}

#[test]
fn every_fixture_string_is_explicitly_synthetic() -> Result<(), Box<dyn std::error::Error>> {
    const FORBIDDEN_RAW: [&str; 11] = [
        "/home/",
        "/users/",
        "anthodev",
        "herdr-context",
        "authorization:",
        "bearer ",
        "api_key",
        "ghp_",
        "sk-",
        "begin private key",
        "c:\\users\\",
    ];

    for case in CASES {
        let mut actual = BTreeSet::new();
        for record in parse_complete_records(case) {
            collect_strings(&record, &mut actual);
        }
        collect_strings(&parse_partial_header(case), &mut actual);
        let expected: BTreeSet<_> = case
            .allowed_strings
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            actual, expected,
            "{} synthetic string allowlist",
            case.source
        );

        for relative in [case.valid, case.partial] {
            let lowercase = fs::read_to_string(fixture(relative))?.to_ascii_lowercase();
            for forbidden in FORBIDDEN_RAW {
                assert!(
                    !lowercase.contains(forbidden),
                    "{relative} contains forbidden private material marker {forbidden}"
                );
            }
        }
    }
    Ok(())
}
