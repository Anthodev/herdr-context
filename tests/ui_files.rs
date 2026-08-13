use std::fs;
use std::path::{Path, PathBuf};

use herdr_context::files::tree::FilesTree;
use herdr_context::ui::files::FilesView;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;
use tempfile::TempDir;

fn line(buffer: &Buffer, y: u16) -> String {
    (buffer.area.x..buffer.area.right())
        .map(|x| buffer[(x, y)].symbol())
        .collect()
}

#[test]
fn narrow_and_wide_rendering_only_draws_the_supplied_viewport_rows() {
    let temp = TempDir::new().expect("tempdir");
    for name in ["first", "middle", "last"] {
        fs::write(temp.path().join(name), []).expect("file fixture");
    }
    let mut tree = FilesTree::new(temp.path().to_path_buf()).expect("tree");
    tree.load_directory(Path::new("")).expect("root");
    let viewport = [PathBuf::from("middle")];

    for width in [12, 48] {
        let area = Rect::new(0, 0, width, 3);
        let mut buffer = Buffer::empty(area);

        FilesView::new(&tree, &viewport, Some(Path::new("middle")), None).render(area, &mut buffer);

        assert!(line(&buffer, 0).starts_with("  middle"));
        assert!(line(&buffer, 1).trim().is_empty());
        assert!(line(&buffer, 2).trim().is_empty());
        let rendered = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!rendered.contains("first"));
        assert!(!rendered.contains("last"));
    }
}

#[test]
fn zero_height_viewport_is_a_safe_noop() {
    let temp = TempDir::new().expect("tempdir");
    let tree = FilesTree::new(temp.path().to_path_buf()).expect("tree");
    let area = Rect::new(0, 0, 16, 0);
    let mut buffer = Buffer::empty(area);

    FilesView::new(&tree, &[], None, None).render(area, &mut buffer);

    assert!(buffer.content.is_empty());
}
