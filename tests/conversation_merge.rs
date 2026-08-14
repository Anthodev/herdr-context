use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use herdr_context::conversations::active::merge_live_sessions;
use herdr_context::conversations::{
    Conversation, ConversationProvenance, ConversationState, ProvenanceKind, ResumeCapability,
    SessionReference, SourceId, ToolIdentity,
};
use herdr_context::host::{HostAgentSession, HostAgentStatus, HostSessionReference, PaneId};
use herdr_context::project::ProjectIdentity;
use tempfile::TempDir;

fn conversation(
    project: &ProjectIdentity,
    tool: &str,
    id: &str,
    title: Option<&str>,
    path: Option<PathBuf>,
    updated: u64,
) -> Conversation {
    Conversation::new(
        ToolIdentity::new(tool).expect("tool"),
        SessionReference::new(tool, id).expect("session"),
        project.clone(),
        title.map(str::to_owned),
        Some(UNIX_EPOCH + Duration::from_secs(updated)),
        None,
        UNIX_EPOCH + Duration::from_secs(updated),
        ConversationState::Unknown,
        vec![ConversationProvenance::new(
            SourceId::new(format!("{tool}-filesystem")).expect("source"),
            if tool == "generic-jsonl" {
                ProvenanceKind::ProjectLocal
            } else {
                ProvenanceKind::ExternalLocal
            },
            path,
        )],
        ResumeCapability::Unsupported,
    )
    .expect("conversation")
}

fn live(
    source: &str,
    agent: &str,
    reference: HostSessionReference,
    project: &Path,
    title: &str,
) -> HostAgentSession {
    HostAgentSession::new(
        source,
        agent,
        reference,
        PaneId::new(format!("pane-{title}")).expect("pane"),
        Some(project.to_path_buf()),
        Some(project.to_path_buf()),
        Some(title.to_owned()),
        HostAgentStatus::Working,
    )
    .expect("live session")
}

#[test]
fn native_identity_wins_and_filesystem_metadata_is_enriched_not_replaced() {
    let project_dir = TempDir::new().expect("project");
    let project = ProjectIdentity::from_canonical_root(project_dir.path().to_path_buf())
        .expect("project identity");
    let transcript = project_dir.path().join("target.jsonl");
    fs::write(&transcript, "target").expect("transcript");
    let other = project_dir.path().join("other.jsonl");
    fs::write(&other, "other").expect("transcript");
    let target = conversation(
        &project,
        "codex-cli",
        "019b7c3b-af88-7000-8001-000000000001",
        Some("filesystem title"),
        Some(transcript),
        20,
    );
    let decoy = conversation(
        &project,
        "codex-cli",
        "019b7c3b-af88-7000-8001-000000000002",
        Some("decoy"),
        Some(other),
        10,
    );
    let active = live(
        "herdr:codex",
        "codex",
        HostSessionReference::NativeId("019b7c3b-af88-7000-8001-000000000001".to_owned()),
        project.root(),
        "runtime title",
    );

    let merged = merge_live_sessions(
        vec![decoy, target],
        vec![active],
        &project,
        UNIX_EPOCH + Duration::from_secs(30),
    );

    assert_eq!(merged.len(), 2);
    let target = merged
        .iter()
        .find(|item| item.session_reference().id().ends_with("0001"))
        .expect("target");
    assert_eq!(target.title(), Some("filesystem title"));
    assert_eq!(target.state(), ConversationState::Live);
    assert_eq!(target.provenance().len(), 2);
    assert_eq!(target.provenance()[0].kind(), ProvenanceKind::ExternalLocal);
    assert_eq!(target.provenance()[1].kind(), ProvenanceKind::HostRuntime);
    assert_eq!(merged[0].session_reference(), target.session_reference());
}

#[test]
fn exact_transcript_identity_precedes_documented_path_identity() {
    let project_dir = TempDir::new().expect("project");
    let project = ProjectIdentity::from_canonical_root(project_dir.path().to_path_buf())
        .expect("project identity");
    let referenced = project_dir
        .path()
        .join("2026-01-02T03-04-05-000Z_019b7ca9-8c88-7000-8003-000000000003.jsonl");
    fs::write(&referenced, "same file").expect("transcript");
    let path_match = conversation(
        &project,
        "generic-jsonl",
        "path-owned-session",
        None,
        Some(referenced.clone()),
        10,
    );
    let documented_decoy = conversation(
        &project,
        "pi",
        "019b7ca9-8c88-7000-8003-000000000003",
        None,
        None,
        20,
    );
    let active = live(
        "herdr:pi",
        "pi",
        HostSessionReference::TranscriptPath(referenced),
        project.root(),
        "live pi",
    );

    let merged = merge_live_sessions(
        vec![documented_decoy, path_match],
        vec![active],
        &project,
        UNIX_EPOCH + Duration::from_secs(30),
    );

    assert_eq!(merged.len(), 2);
    let path_match = merged
        .iter()
        .find(|item| item.session_reference().id() == "path-owned-session")
        .expect("path match");
    assert_eq!(path_match.state(), ConversationState::Live);
    let decoy = merged
        .iter()
        .find(|item| item.tool().as_str() == "pi")
        .expect("documented decoy");
    assert_eq!(decoy.state(), ConversationState::Unknown);
}

#[test]
fn unmatched_path_session_converges_to_documented_filesystem_identity() {
    let project_dir = TempDir::new().expect("project");
    let project = ProjectIdentity::from_canonical_root(project_dir.path().to_path_buf())
        .expect("project identity");
    let transcript = project_dir
        .path()
        .join("2026-01-02T03-04-05-000Z_019b7ca9-8c88-7000-8003-000000000003.jsonl");
    let active = live(
        "herdr:pi",
        "pi",
        HostSessionReference::TranscriptPath(transcript.clone()),
        project.root(),
        "live only",
    );
    let observed = UNIX_EPOCH + Duration::from_secs(30);

    let live_only = merge_live_sessions(vec![], vec![active.clone()], &project, observed);
    assert_eq!(live_only.len(), 1);
    assert_eq!(
        live_only[0].session_reference().id(),
        "019b7ca9-8c88-7000-8003-000000000003"
    );
    assert_eq!(live_only[0].state(), ConversationState::Live);

    fs::write(&transcript, "created later").expect("transcript");
    let filesystem = conversation(
        &project,
        "pi",
        "019b7ca9-8c88-7000-8003-000000000003",
        Some("filesystem"),
        Some(transcript),
        40,
    );
    let converged = merge_live_sessions(vec![filesystem], vec![active], &project, observed);

    assert_eq!(converged.len(), 1);
    assert_eq!(
        converged[0].session_reference(),
        live_only[0].session_reference()
    );
    assert_eq!(converged[0].title(), Some("filesystem"));
    assert_eq!(converged[0].provenance().len(), 2);
}

#[test]
fn sessions_outside_the_project_and_unsafe_prefixes_are_ignored() {
    let root = TempDir::new().expect("root");
    let project_path = root.path().join("app");
    let sibling_path = root.path().join("application");
    fs::create_dir_all(&project_path).expect("project");
    fs::create_dir_all(&sibling_path).expect("sibling");
    let project = ProjectIdentity::from_canonical_root(project_path).expect("project identity");
    let outside = live(
        "herdr:omp",
        "omp",
        HostSessionReference::NativeId("019b8721-4a18-7000-8005-000000000005".to_owned()),
        &sibling_path,
        "outside",
    );
    let conflicting_foreground = HostAgentSession::new(
        "herdr:omp",
        "omp",
        HostSessionReference::NativeId("019b8721-4a18-7000-8005-000000000006".to_owned()),
        PaneId::new("pane-conflicting").expect("pane"),
        Some(project.root().to_path_buf()),
        Some(sibling_path),
        Some("outside foreground".to_owned()),
        HostAgentStatus::Working,
    )
    .expect("live session");

    let merged = merge_live_sessions(
        Vec::new(),
        vec![outside, conflicting_foreground],
        &project,
        SystemTime::now(),
    );

    assert!(merged.is_empty());
}

#[test]
fn official_live_integrations_match_the_registered_tool_namespaces() {
    let project_dir = TempDir::new().expect("project");
    let project = ProjectIdentity::from_canonical_root(project_dir.path().to_path_buf())
        .expect("project identity");
    let identities = [
        (
            "claude-code",
            "herdr:claude",
            "claude",
            "11111111-1111-4111-8111-111111111111",
        ),
        (
            "codex-cli",
            "herdr:codex",
            "codex",
            "019b7c3b-af88-7000-8001-000000000001",
        ),
        (
            "pi",
            "herdr:pi",
            "pi",
            "019b7ca9-8c88-7000-8003-000000000003",
        ),
        (
            "omp",
            "herdr:omp",
            "omp",
            "019b8721-4a18-7000-8005-000000000005",
        ),
        (
            "opencode",
            "herdr:opencode",
            "opencode",
            "session_6f86a0ea4f9f5d8c",
        ),
    ];
    let filesystem = identities
        .iter()
        .enumerate()
        .map(|(index, (tool, _, _, id))| conversation(&project, tool, id, None, None, index as u64))
        .collect();
    let live = identities
        .iter()
        .map(|(_, source, agent, id)| {
            live(
                source,
                agent,
                HostSessionReference::NativeId((*id).to_owned()),
                project.root(),
                agent,
            )
        })
        .collect();

    let merged = merge_live_sessions(
        filesystem,
        live,
        &project,
        UNIX_EPOCH + Duration::from_secs(60),
    );

    assert_eq!(merged.len(), identities.len());
    assert!(merged.iter().all(|conversation| {
        conversation.state() == ConversationState::Live && conversation.provenance().len() == 2
    }));
}
