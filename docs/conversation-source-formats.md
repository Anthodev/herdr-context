# Conversation source formats

## Scope

HDC-12 records the format boundary and sanitized evidence used by the three external adapters added in HDC-13. HDC-13 adds metadata-only indexing and UI discovery; it does not add network access, tool launch, transcript mutation, or compatibility beyond these fixtures. The current bounds below were verified against the latest official package releases and reproduced by sanitized fixtures.

The inventory contains exactly these stores:

| Source | Store root | Fixture-backed bound |
| --- | --- | --- |
| Claude Code | `~/.claude/projects` | JSONL identity records whose `version` is exactly `2.1.232` |
| Codex CLI | `~/.codex/sessions` | JSONL sessions whose `session_meta.payload.cli_version` is exactly `0.147.0` |
| Pi | `~/.pi/agent/sessions` | JSONL sessions from Pi CLI `0.84.1` whose header `version` is exactly `3` |

These are evidence bounds, not claims that later or earlier versions are incompatible. Versions outside the table remain unsupported until a structurally faithful fixture and validation are committed. Pi session files encode schema version `3`, not the CLI version; the executable-version bound is therefore maintained by this inventory and its fixture rather than inferred from a session file.

## Claude Code

### Layout and framing

- Layout: `<encoded-project-path>/<session-uuid>.jsonl` below the store root.
- The project directory replaces every non-ASCII-alphanumeric UTF-16 code unit with `-`; encodings over 200 code units use a 200-character prefix plus the signed 32-bit Java-style path hash in base 36.
- Each complete line is one JSON object. Records are appended to the same file.
- A writer may leave an incomplete final line. Consumers may retain the complete prefix, but the partial line is not a valid record.

### Identity, time, and project evidence

- `sessionId` is the stable UUIDv4 session ID and is repeated across transcript records.
- `uuid` is a UUIDv4 transcript-record ID; `parentUuid` forms a transcript tree and may point to an earlier record after rewind. Direct session metadata records need not join that tree.
- `timestamp` is an RFC 3339 string.
- Project evidence consists of the encoded project directory plus the explicit absolute `cwd` on identity-bearing transcript records. An adapter validates both the encoding and canonical `cwd`; directory names or titles alone are insufficient.

### Fixture-backed record set and bounds

`tests/fixtures/conversations/claude-code/-workspace-project/11111111-1111-4111-8111-111111111111.jsonl` contains a `user` record with a `message` payload, a direct `attachment` metadata record with an `attachment` payload, an `assistant` record, and a branched `user` record whose `parentUuid` points back to the first user. Transcript records carry `sessionId`, `uuid`, `cwd`, `timestamp`, and `version: "2.1.232"`; the metadata record intentionally omits transcript identity fields. The sibling `66666666-6666-4666-8666-666666666666.jsonl` contains one complete record and one truncated appended record.

Unsupported variants include non-JSONL files, record types or payload markers outside the committed set, identity-bearing records without the required IDs/timestamp/project evidence, non-matching record versions, malformed records before the final line, and nested sidechain/agent directories.

## Codex CLI

### Layout and framing

- Layout: `YYYY/MM/DD/rollout-<timestamp>-<session-id>.jsonl` below the store root.
- The first complete record is `session_meta`; later records must match the current `RolloutItem` payload shape for their declared type. Legacy mode omits ordinals, while paginated mode requires contiguous ordinals beginning at zero.
- A partial final line is a recoverable append boundary only. It is not a valid event; an over-limit unterminated append is parked without repeated rescans and rejected if it later becomes a complete over-limit record.

### Identity, time, and project evidence

- `session_meta.payload.id` and `session_meta.payload.session_id` are the same UUIDv7 session ID, repeated in the rollout filename.
- `session_meta.payload.timestamp` is the UTC session-start instant. Top-level `timestamp` is the same or a later UTC record-write instant. The rollout filename and `YYYY/MM/DD` directories use independently recorded local wall time; the fixture relationship is explicit UTC+02 (`01:04:05Z` → `03-04-05`) and must not be generalized into a fixed offset.
- `session_meta.payload.cwd` is the canonical project-evidence candidate. Git metadata is supplementary and must not replace canonical path matching.
- Event/message text and titles are never project evidence.

### Fixture-backed record set and bounds

`tests/fixtures/conversations/codex-cli/2026/01/02/rollout-2026-01-02T03-04-05-019b7c3b-af88-7000-8001-000000000001.jsonl` contains paginated `session_meta`, `event_msg`, `world_state`, and `response_item` records with contiguous ordinals. The metadata record carries `cli_version: "0.147.0"`, matching UUIDv7 thread/session IDs, an absolute `cwd`, distinct session-start/record-write timestamps, and synthetic Git metadata. The corresponding `2026/01/03` rollout fixture ends with a truncated `response_item`.

Unsupported variants include sessions without a leading `session_meta`, missing or conflicting UUID/cwd/timestamps, record-write timestamps before session start, non-matching CLI versions, history-mode/ordinal conflicts, non-JSONL framing, malformed records before the final line, payloads that do not match their declared current rollout item type, and record types outside the current rollout item set.

## Pi

### Layout and framing

- Layout: `<encoded-cwd>/<timestamp>_<session-id>.jsonl` below the store root.
- The first complete object is a `session` header. Later entries are appended JSON objects.
- A partial final line is a recoverable append boundary only and is not an entry.

### Identity, time, and project evidence

- The `session` header `id` is the UUIDv7 session ID.
- Entry `id` and `parentId` fields are eight lowercase hexadecimal characters and form a session tree; a new branch may point to an earlier entry rather than the preceding line.
- Header and entry `timestamp` values are RFC 3339 strings; nested message timestamps are Unix epoch milliseconds.
- Project evidence consists of the encoded parent directory plus the session header's absolute `cwd`. Both must resolve to the same canonical project identity.

### Fixture-backed record set and bounds

`tests/fixtures/conversations/pi/--workspace-project--/2026-01-02T03-04-05-000Z_019b7ca9-8c88-7000-8003-000000000003.jsonl` contains a schema `version: 3` session header, an appended user message, a `model_change` with its required provider/model markers, an assistant message, and a branching `session_info` entry whose `parentId` points to the earlier user entry. The sibling dated session fixture contains the header followed by a truncated message.

Unsupported variants include schema versions other than `3`, files without a leading session header, missing ID/cwd/timestamps, invalid entry root/parent shapes, missing variant-specific payload fields, non-JSONL framing, malformed records before the final line, nested child-run directories, and entry types outside Pi `0.84.1`'s current session-entry union.

## Fixture safety and validation

All fixture paths, IDs, timestamps, messages, model/provider labels, instructions, Git metadata, and usage values are synthetic. The fixtures contain no captured prompts, responses, secrets, usernames, home paths, or repository names.

`tests/conversation_source_fixtures.rs` enforces:

- the exact recursive on-disk layouts for all three sources and the correspondence between filenames, IDs, timestamps, and project evidence;
- source-specific JSONL framing, version bounds, append linkage, and incremental partial-record cases;
- exact malformed trailing records for every partial fixture;
- an exhaustive per-source allowlist for every complete JSON string value, plus raw-content rejection of private paths, local identifiers, and common secret markers.

HDC-13 adapter and index tests additionally enforce canonical project evidence,
unsupported-version rejection, exact native filenames, safe partial-tail
handling, bounded complete-record cursors for large and appended transcripts,
bounded source watermarks, recent-first paging, source-scoped deletion,
cooperative cancellation, private atomic cache publication, corruption and
generation-mismatch rebuilds, and the persisted metadata allowlist.

Any compatibility expansion must update this document, add sanitized structural evidence, and extend the fixture validation in the same change.
