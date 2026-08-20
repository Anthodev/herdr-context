use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use std::time::{Duration, UNIX_EPOCH};

use herdr_context::config::DisplayMode;
use herdr_context::conversations::{
    Conversation, ConversationProvenance, ConversationState, ProvenanceKind, ResumeCapability,
    ResumeReference, SessionReference, SourceId, ToolIdentity,
};
use herdr_context::host::LaunchContext;
use herdr_context::input::{InputMode, map_event};
use herdr_context::intent::{Intent, View};
use herdr_context::model::{AppModel, LoadingState};
use herdr_context::project::ProjectIdentity;
use herdr_context::ui::{render_shell, sanitize_terminal_text};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use tempfile::TempDir;

fn context() -> LaunchContext {
    LaunchContext::from_vars([(
        "HERDR_PLUGIN_CONTEXT_JSON",
        r#"{"workspace_id":"workspace","tab_id":"tab","pane_id":"pane","cwd":"/project"}"#,
    )])
    .expect("context")
}

fn line(buffer: &Buffer, y: u16) -> String {
    (buffer.area.x..buffer.area.right())
        .map(|x| buffer[(x, y)].symbol())
        .collect::<String>()
}

fn conversation(
    project: &ProjectIdentity,
    tool: &str,
    session_id: &str,
    updated_seconds: u64,
) -> Conversation {
    conversation_with_state(
        project,
        tool,
        session_id,
        updated_seconds,
        ConversationState::Unknown,
    )
}

fn conversation_with_state(
    project: &ProjectIdentity,
    tool: &str,
    session_id: &str,
    updated_seconds: u64,
    state: ConversationState,
) -> Conversation {
    Conversation::new(
        ToolIdentity::new(tool).expect("tool"),
        SessionReference::new(tool, session_id).expect("session"),
        project.clone(),
        None,
        Some(UNIX_EPOCH + Duration::from_secs(updated_seconds)),
        None,
        UNIX_EPOCH + Duration::from_secs(updated_seconds),
        state,
        vec![ConversationProvenance::new(
            SourceId::new(tool).expect("source"),
            ProvenanceKind::ExternalLocal,
            None,
        )],
        ResumeCapability::Unsupported,
    )
    .expect("conversation")
}

#[test]
fn conversations_are_grouped_by_provider_with_expandable_headers() {
    let project_dir = TempDir::new().expect("project");
    let project = ProjectIdentity::from_canonical_root(project_dir.path().to_path_buf())
        .expect("project identity");
    let mut model = AppModel::new(context());
    model.set_active_view(View::Conversations);
    model.conversations_mut().replace_items(
        vec![
            conversation(&project, "pi", "pi-session", 30),
            conversation(&project, "codex-cli", "codex-new", 20),
            conversation(&project, "codex-cli", "codex-old", 10),
        ],
        1,
    );
    let area = Rect::new(0, 0, 50, 7);
    let mut buffer = Buffer::empty(area);

    render_shell(&mut model, area, &mut buffer);

    assert!(line(&buffer, 1).starts_with("- codex-cli (2)"));
    assert!(line(&buffer, 2).starts_with("  ? codex-new"));
    assert!(line(&buffer, 3).starts_with("  ? codex-old"));
    assert!(line(&buffer, 4).starts_with("- pi (1)"));
    assert!(line(&buffer, 5).starts_with("  ? pi-session"));
    assert_eq!(buffer[(0, 1)].fg, Color::Magenta);
    assert!(buffer[(0, 1)].modifier.contains(Modifier::REVERSED));
    assert_eq!(buffer[(13, 2)].fg, Color::DarkGray);
}

#[test]
fn conversations_render_ascii_unicode_and_nerd_glyphs() {
    let project_dir = TempDir::new().expect("project");
    let project = ProjectIdentity::from_canonical_root(project_dir.path().to_path_buf())
        .expect("project identity");
    let mut model = AppModel::new(context());
    model.set_active_view(View::Conversations);
    model.conversations_mut().replace_items(
        vec![
            conversation_with_state(&project, "omp", "live", 30, ConversationState::Live),
            conversation_with_state(&project, "omp", "archived", 20, ConversationState::Archived),
            conversation_with_state(&project, "omp", "unknown", 10, ConversationState::Unknown),
        ],
        1,
    );
    let area = Rect::new(0, 0, 48, 5);
    let render = |model: &mut AppModel, mode| {
        model.set_display_mode(mode);
        let mut buffer = Buffer::empty(area);
        render_shell(model, area, &mut buffer);
        (0..area.height)
            .map(|row| line(&buffer, row))
            .collect::<Vec<_>>()
    };

    let ascii = render(&mut model, DisplayMode::Ascii);
    assert!(ascii[1].starts_with("- omp (3)"));
    assert!(ascii[2].starts_with("  * live"));
    assert!(ascii[3].starts_with("  - archived"));
    assert!(ascii[4].starts_with("  ? unknown"));

    let unicode = render(&mut model, DisplayMode::Unicode);
    assert!(unicode[1].starts_with("▾ omp (3)"));
    assert!(unicode[2].starts_with("  ● live"));
    assert!(unicode[3].starts_with("  ○ archived"));
    assert!(unicode[4].starts_with("  • unknown"));

    let nerd = render(&mut model, DisplayMode::Nerd);
    assert!(nerd[1].starts_with(" omp (3)"));
    assert!(nerd[2].starts_with("   live"));
    assert!(nerd[3].starts_with("   archived"));
    assert!(nerd[4].starts_with("   unknown"));
}

#[test]
fn tabs_and_compact_states_render_in_wide_and_narrow_areas() {
    let mut model = AppModel::new(context());
    let wide = Rect::new(0, 0, 40, 4);
    let mut buffer = Buffer::empty(wide);

    render_shell(&mut model, wide, &mut buffer);
    assert!(line(&buffer, 0).contains("Files"));
    assert!(line(&buffer, 0).contains("Conversations"));
    assert!(line(&buffer, 1).contains("Loading Files"));
    assert_eq!(buffer[(0, 0)].fg, Color::Magenta);
    assert!(buffer[(0, 0)].modifier.contains(Modifier::REVERSED));
    assert_eq!(buffer[(7, 0)].fg, Color::DarkGray);

    model.set_active_view(View::Conversations);
    model.conversations_mut().set_loading(LoadingState::Ready);
    let narrow = Rect::new(0, 0, 9, 2);
    let mut narrow_buffer = Buffer::empty(narrow);
    render_shell(&mut model, narrow, &mut narrow_buffer);
    assert!(line(&narrow_buffer, 1).contains("No conv"));

    model.conversations_mut().set_source_errors(vec![
        "UnsupportedFormat: conversation source codex-cli: malformed record".to_owned(),
        "PermissionDenied: conversation source pi: unreadable store".to_owned(),
    ]);
    let mut warning_buffer = Buffer::empty(wide);
    render_shell(&mut model, wide, &mut warning_buffer);
    assert!(line(&warning_buffer, 1).contains("Warning"));
    assert!(line(&warning_buffer, 1).contains("(+1 more)"));
    assert_eq!(warning_buffer[(0, 1)].fg, Color::Yellow);
}

#[test]
fn input_mapping_is_pure_and_uses_rendered_mouse_geometry() {
    let mut model = AppModel::new(context());
    let area = Rect::new(5, 3, 40, 5);
    render_shell(&mut model, area, &mut Buffer::empty(area));

    assert_eq!(
        map_event(
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            InputMode::Normal,
            model.geometry(),
        ),
        Some(Intent::NextView)
    );
    assert_eq!(
        map_event(
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 30,
                row: 3,
                modifiers: KeyModifiers::NONE,
            }),
            InputMode::Normal,
            model.geometry(),
        ),
        Some(Intent::SwitchView(View::Conversations))
    );
    assert_eq!(
        map_event(Event::Resize(20, 8), InputMode::Normal, model.geometry()),
        Some(Intent::Resize)
    );
}

#[test]
fn switching_views_preserves_independent_state_and_stable_selection() {
    let mut model = AppModel::new(context());
    model.files_mut().set_selection(Some("src/main.rs".into()));
    model.files_mut().set_scroll(7);
    model.files_mut().set_filter("main");
    model.files_mut().set_generations(4, 3);

    model.set_active_view(View::Conversations);
    model.conversations_mut().set_scroll(2);
    model.conversations_mut().set_filter("codex");
    model.set_active_view(View::Files);

    assert_eq!(
        model.files().selection(),
        Some(std::path::Path::new("src/main.rs"))
    );
    assert_eq!(model.files().scroll(), 7);
    assert_eq!(model.files().filter(), "main");
    assert_eq!(model.files().generations(), (4, 3));
    assert_eq!(model.conversations().scroll(), 2);
    assert_eq!(model.conversations().filter(), "codex");
}

#[test]
fn terminal_control_characters_are_never_rendered_verbatim() {
    assert_eq!(sanitize_terminal_text("safe"), "safe");
    assert_eq!(
        sanitize_terminal_text("bad\u{1b}]2;title\u{7}"),
        "bad�]2;title�"
    );

    let mut model = AppModel::new(context());
    model
        .files_mut()
        .set_loading(LoadingState::Error("failed\u{1b}[2J".to_owned()));
    let area = Rect::new(0, 0, 30, 3);
    let mut buffer = Buffer::empty(area);
    render_shell(&mut model, area, &mut buffer);

    assert!(!line(&buffer, 1).contains('\u{1b}'));

    let project_dir = TempDir::new().expect("project");
    let project = ProjectIdentity::from_canonical_root(project_dir.path().to_path_buf())
        .expect("project identity");
    let mut model = AppModel::new(context());
    model.set_active_view(View::Conversations);
    model.conversations_mut().replace_items(
        vec![conversation(&project, "bad\u{1b}[2J", "session", 1)],
        1,
    );
    let area = Rect::new(0, 0, 80, 4);
    let mut buffer = Buffer::empty(area);
    render_shell(&mut model, area, &mut buffer);
    assert!((0..area.height).all(|row| !line(&buffer, row).contains('\u{1b}')));
}

#[test]
fn live_metadata_and_background_states_render_in_wide_and_compact_layouts() {
    let project_dir = TempDir::new().expect("project");
    let project = ProjectIdentity::from_canonical_root(project_dir.path().to_path_buf())
        .expect("project identity");
    let rich = Conversation::new(
        ToolIdentity::new("omp").expect("tool"),
        SessionReference::new("omp", "session-live").expect("session"),
        project,
        Some("live\u{1b} title".to_owned()),
        None,
        None,
        UNIX_EPOCH + Duration::from_secs(60),
        ConversationState::Live,
        vec![
            ConversationProvenance::new(
                SourceId::new("omp").expect("source"),
                ProvenanceKind::ExternalLocal,
                None,
            ),
            ConversationProvenance::new(
                SourceId::new("herdr:omp").expect("source"),
                ProvenanceKind::HostRuntime,
                None,
            ),
        ],
        ResumeCapability::Supported(
            ResumeReference::new("session-live").expect("resume reference"),
        ),
    )
    .expect("conversation");
    let mut model = AppModel::new(context());
    model.set_active_view(View::Conversations);
    model
        .conversations_mut()
        .replace_items(vec![rich.clone()], 1);

    let wide = Rect::new(0, 0, 120, 4);
    let mut wide_buffer = Buffer::empty(wide);
    render_shell(&mut model, wide, &mut wide_buffer);
    let wide_row = line(&wide_buffer, 2);
    assert!(wide_row.contains("live� title"));
    assert!(wide_row.contains("tool=omp"));
    assert!(wide_row.contains("updated=1970-01-01 00:01Z"));
    assert!(wide_row.contains("source=external+live"));
    assert!(wide_row.contains("resume=yes"));
    assert!(wide_row.contains("state=live"));

    model.conversations_mut().replace_items(vec![rich], 2);
    let compact = Rect::new(0, 0, 80, 4);
    let mut compact_buffer = Buffer::empty(compact);
    render_shell(&mut model, compact, &mut compact_buffer);
    let compact_row = line(&compact_buffer, 2);
    assert!(compact_row.contains("live� title · omp · 1970-01-01 00:01Z · E+L · R · live"));

    model.conversations_mut().set_live_loading(true);
    let mut loading_buffer = Buffer::empty(wide);
    render_shell(&mut model, wide, &mut loading_buffer);
    assert!(line(&loading_buffer, 1).contains("Loading live sessions"));

    model.conversations_mut().set_live_loading(false);
    model
        .conversations_mut()
        .set_live_error(Some("bad\u{1b} live response".to_owned()));
    let mut error_buffer = Buffer::empty(wide);
    render_shell(&mut model, wide, &mut error_buffer);
    assert!(line(&error_buffer, 1).contains("bad� live response"));
    assert!(!line(&error_buffer, 1).contains('\u{1b}'));
}
