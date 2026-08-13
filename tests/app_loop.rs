use herdr_context::app::App;
use herdr_context::host::LaunchContext;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn context(cwd: &str) -> LaunchContext {
    LaunchContext::from_vars([(
        "HERDR_PLUGIN_CONTEXT_JSON",
        format!(r#"{{"workspace_id":"workspace","tab_id":"tab","pane_id":"pane","cwd":"{cwd}"}}"#),
    )])
    .expect("context")
}

#[test]
fn first_frame_does_not_wait_for_project_io() {
    let mut app = App::new(context("/definitely/not/a/real/project"));
    let mut terminal = Terminal::new(TestBackend::new(40, 4)).expect("terminal");

    terminal
        .draw(|frame| app.render(frame))
        .expect("first frame");

    let first_frame = terminal.backend().buffer();
    let rendered = (0..first_frame.area.height)
        .flat_map(|y| {
            (0..first_frame.area.width).map(move |x| first_frame[(x, y)].symbol().to_owned())
        })
        .collect::<String>();
    assert!(rendered.contains("Loading Files"));
    assert!(!app.is_dirty());
}

#[test]
fn clean_app_does_not_request_an_idle_redraw() {
    let mut app = App::new(context("/project"));
    let mut terminal = Terminal::new(TestBackend::new(30, 3)).expect("terminal");
    terminal.draw(|frame| app.render(frame)).expect("draw");

    assert!(!app.is_dirty());
    assert!(!app.has_pending_work());
}
