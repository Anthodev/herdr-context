use std::fs;
use std::time::{Duration, UNIX_EPOCH};

use herdr_context::conversations::{
    Conversation, ConversationProvenance, ConversationState, ProvenanceKind, ResumeCapability,
    SessionReference, SourceId, ToolIdentity,
};
use herdr_context::model::ConversationsViewState;
use herdr_context::project::ProjectIdentity;
use tempfile::TempDir;

fn conversation(project: &ProjectIdentity, id: &str, state: ConversationState) -> Conversation {
    Conversation::new(
        ToolIdentity::new("omp").expect("tool"),
        SessionReference::new("omp", id).expect("session"),
        project.clone(),
        Some("session".to_owned()),
        None,
        None,
        UNIX_EPOCH + Duration::from_secs(10),
        state,
        vec![ConversationProvenance::new(
            SourceId::new(if state == ConversationState::Live {
                "herdr:omp"
            } else {
                "omp"
            })
            .expect("source"),
            if state == ConversationState::Live {
                ProvenanceKind::HostRuntime
            } else {
                ProvenanceKind::ExternalLocal
            },
            None,
        )],
        ResumeCapability::Unsupported,
    )
    .expect("conversation")
}

#[test]
fn stale_live_generations_cannot_replace_visible_items_or_selection() {
    let project_dir = TempDir::new().expect("project");
    let project = ProjectIdentity::from_canonical_root(project_dir.path().to_path_buf())
        .expect("project identity");
    let id = "019b8721-4a18-7000-8005-000000000005";
    let mut state = ConversationsViewState::default();
    state.replace_items(vec![conversation(&project, id, ConversationState::Live)], 1);
    state.set_selection(Some(SessionReference::new("omp", id).expect("selection")));
    state.set_live_generations(3, 1);
    state.set_live_loading(true);

    assert!(!state.replace_live_items(Vec::new(), 2));
    assert_eq!(state.items().len(), 1);
    assert_eq!(state.selection().map(SessionReference::id), Some(id));
    assert!(state.live_loading());

    assert!(state.replace_live_items(
        vec![conversation(&project, id, ConversationState::Unknown)],
        3,
    ));
    assert_eq!(state.live_generations(), (3, 3));
    assert_eq!(state.selection().map(SessionReference::id), Some(id));
    assert!(!state.live_loading());
}

#[test]
fn live_errors_are_separate_non_fatal_warnings() {
    let mut state = ConversationsViewState::default();
    state.set_source_errors(vec![herdr_context::model::VisibleError::quiet(
        "filesystem warning".to_owned(),
    )]);
    state.set_live_error(Some("Herdr unavailable".to_owned()));

    assert_eq!(
        state
            .visible_errors()
            .iter()
            .map(|error| error.message())
            .collect::<Vec<_>>(),
        ["filesystem warning", "Herdr unavailable"]
    );

    state.set_live_error(None);
    assert_eq!(
        state
            .visible_errors()
            .iter()
            .map(|error| error.message())
            .collect::<Vec<_>>(),
        ["filesystem warning"]
    );
}

#[test]
fn selection_follows_a_unique_transcript_when_live_only_identity_converges() {
    let project_dir = TempDir::new().expect("project");
    let project = ProjectIdentity::from_canonical_root(project_dir.path().to_path_buf())
        .expect("project identity");
    let transcript = project_dir.path().join("custom-session.jsonl");
    fs::write(&transcript, "synthetic").expect("transcript");
    let live_reference = SessionReference::new("custom", "herdr:custom:path:/custom-session.jsonl")
        .expect("live reference");
    let filesystem_reference =
        SessionReference::new("custom", "native-session").expect("filesystem reference");
    let runtime_provenance = ConversationProvenance::new(
        SourceId::new("herdr:custom").expect("source"),
        ProvenanceKind::HostRuntime,
        Some(transcript.clone()),
    );
    let live_only = Conversation::new(
        ToolIdentity::new("custom").expect("tool"),
        live_reference.clone(),
        project.clone(),
        Some("session".to_owned()),
        None,
        None,
        UNIX_EPOCH + Duration::from_secs(10),
        ConversationState::Live,
        vec![runtime_provenance.clone()],
        ResumeCapability::Unsupported,
    )
    .expect("live conversation");
    let converged = Conversation::new(
        ToolIdentity::new("custom").expect("tool"),
        filesystem_reference.clone(),
        project,
        Some("session".to_owned()),
        None,
        None,
        UNIX_EPOCH + Duration::from_secs(20),
        ConversationState::Live,
        vec![
            ConversationProvenance::new(
                SourceId::new("custom-filesystem").expect("source"),
                ProvenanceKind::ProjectLocal,
                Some(transcript),
            ),
            runtime_provenance,
        ],
        ResumeCapability::Unsupported,
    )
    .expect("converged conversation");
    let mut state = ConversationsViewState::default();
    state.replace_items(vec![live_only], 1);
    state.set_selection(Some(live_reference));
    state.set_live_generations(2, 1);

    assert!(state.replace_live_items(vec![converged], 2));
    assert_eq!(state.selection(), Some(&filesystem_reference));
}
