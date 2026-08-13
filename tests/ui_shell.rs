use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use herdr_context::host::LaunchContext;
use herdr_context::input::{InputMode, map_event};
use herdr_context::intent::{Intent, View};
use herdr_context::model::{AppModel, LoadingState};
use herdr_context::ui::{render_shell, sanitize_terminal_text};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

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

#[test]
fn tabs_and_compact_states_render_in_wide_and_narrow_areas() {
    let mut model = AppModel::new(context());
    let wide = Rect::new(0, 0, 40, 4);
    let mut buffer = Buffer::empty(wide);

    render_shell(&mut model, wide, &mut buffer);
    assert!(line(&buffer, 0).contains("Files"));
    assert!(line(&buffer, 0).contains("Conversations"));
    assert!(line(&buffer, 1).contains("Loading Files"));

    model.set_active_view(View::Conversations);
    model.conversations_mut().set_loading(LoadingState::Ready);
    let narrow = Rect::new(0, 0, 9, 2);
    let mut narrow_buffer = Buffer::empty(narrow);
    render_shell(&mut model, narrow, &mut narrow_buffer);
    assert!(line(&narrow_buffer, 1).contains("No conv"));
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
}
