use std::fs;

use herdr_context::host::LaunchContext;
use herdr_context::runtime::FilesRuntime;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use tempfile::TempDir;

#[test]
fn launch_context_bootstraps_and_renders_the_files_view() {
    let temp = TempDir::new().expect("tempdir");
    fs::write(temp.path().join("visible.txt"), []).expect("visible file");
    let context = LaunchContext::from_vars([(
        "HERDR_PLUGIN_CONTEXT_JSON",
        format!(
            r#"{{"workspace_id":"workspace","tab_id":"tab","pane_id":"pane","cwd":"{}"}}"#,
            temp.path().display()
        ),
    )])
    .expect("launch context");

    let mut runtime = FilesRuntime::bootstrap(&context).expect("runtime");
    let area = Rect::new(0, 0, 40, 2);
    let mut buffer = Buffer::empty(area);
    runtime.render(area, &mut buffer);

    // Row 0 is the project header; the tree starts on row 1.
    let tree_line = (0..area.width)
        .map(|x| buffer[(x, 1)].symbol())
        .collect::<String>();
    assert!(tree_line.contains("visible.txt"));
}
