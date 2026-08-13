use std::path::{Path, PathBuf};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use crate::files::tree::TreeNodeKind;
use crate::vcs::VcsStatusKind;

use super::{sanitize_terminal_cow, sanitize_terminal_text};

/// Renders a caller-provided viewport slice; it never scans or indexes the tree.
pub struct FilesView<'a> {
    tree: &'a crate::files::tree::FilesTree,
    rows: &'a [PathBuf],
    selected: Option<&'a Path>,
    notice: Option<(&'a str, &'a str)>,
}

impl<'a> FilesView<'a> {
    #[must_use]
    pub const fn new(
        tree: &'a crate::files::tree::FilesTree,
        rows: &'a [PathBuf],
        selected: Option<&'a Path>,
        notice: Option<(&'a str, &'a str)>,
    ) -> Self {
        Self {
            tree,
            rows,
            selected,
            notice,
        }
    }
}

impl Widget for FilesView<'_> {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        if area.is_empty() {
            return;
        }
        let notice_height = u16::from(self.notice.is_some());
        let row_limit = area.height.saturating_sub(notice_height) as usize;
        for (offset, node) in self
            .rows
            .iter()
            .filter_map(|path| self.tree.node(path))
            .take(row_limit)
            .enumerate()
        {
            let y = area.y.saturating_add(offset as u16);
            let (marker, marker_style) = status_marker(node.status());
            let mut spans = Vec::with_capacity(4);
            spans.push(Span::styled(marker, marker_style));
            spans.push(Span::raw(" "));
            spans.push(Span::raw(sanitize_terminal_cow(
                node.path().to_string_lossy(),
            )));
            if node.kind() == TreeNodeKind::Virtual {
                spans.push(Span::styled(" (missing)", Style::new().fg(Color::DarkGray)));
            }
            let mut line = Line::from(spans);
            if self.selected == Some(node.path()) {
                line = line.style(Style::new().add_modifier(Modifier::REVERSED));
            }
            line.render(Rect::new(area.x, y, area.width, 1), buffer);
        }

        if let Some((label, notice)) = self.notice {
            Line::from(vec![
                Span::styled(format!("{label}: "), Style::new().fg(Color::Red).bold()),
                Span::styled(sanitize_terminal_text(notice), Style::new().fg(Color::Red)),
            ])
            .render(
                Rect::new(
                    area.x,
                    area.y.saturating_add(area.height.saturating_sub(1)),
                    area.width,
                    1,
                ),
                buffer,
            );
        }
    }
}

const fn status_marker(status: Option<VcsStatusKind>) -> (&'static str, Style) {
    match status {
        Some(VcsStatusKind::Added) => ("A", Style::new().fg(Color::Green)),
        Some(VcsStatusKind::Modified) => ("M", Style::new().fg(Color::Yellow)),
        Some(VcsStatusKind::Deleted) => ("D", Style::new().fg(Color::Red)),
        Some(VcsStatusKind::Renamed) => ("R", Style::new().fg(Color::Cyan)),
        Some(VcsStatusKind::Copied) => ("C", Style::new().fg(Color::Cyan)),
        Some(VcsStatusKind::Untracked) => ("?", Style::new().fg(Color::Blue)),
        Some(VcsStatusKind::Conflicted) => ("!", Style::new().fg(Color::Red).bold()),
        Some(VcsStatusKind::TypeChanged) => ("T", Style::new().fg(Color::Magenta)),
        None => (" ", Style::new()),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::widgets::Widget;

    use super::FilesView;
    use crate::files::tree::FilesTree;
    use crate::vcs::{VcsEntryStatus, VcsStatusKind, VcsStatusSnapshot};
    use tempfile::TempDir;

    #[test]
    fn renders_status_virtual_marker_selection_and_failure_notice() {
        let temp = TempDir::new().expect("tempdir");
        let mut tree = FilesTree::new(temp.path().to_path_buf()).expect("tree");
        let deleted = VcsEntryStatus::new(
            PathBuf::from("deleted file"),
            None,
            VcsStatusKind::Deleted,
            Some(VcsStatusKind::Deleted),
            None,
        )
        .expect("deleted status");
        tree.merge_status(&VcsStatusSnapshot::new(vec![deleted], false))
            .expect("merge status");
        let rows = [PathBuf::from("deleted file")];
        let area = Rect::new(0, 0, 40, 2);
        let mut buffer = Buffer::empty(area);

        FilesView::new(
            &tree,
            &rows,
            Some(Path::new("deleted file")),
            Some(("VCS", "status failed")),
        )
        .render(area, &mut buffer);

        let first = (0..area.width)
            .map(|x| buffer[(x, 0)].symbol())
            .collect::<String>();
        let second = (0..area.width)
            .map(|x| buffer[(x, 1)].symbol())
            .collect::<String>();
        assert!(first.starts_with("D deleted file (missing)"));
        assert!(second.starts_with("VCS: status failed"));
        assert!(
            buffer[(0, 0)]
                .modifier
                .contains(ratatui::style::Modifier::REVERSED)
        );
    }
    #[test]
    fn renders_conflict_marker_and_passive_stale_notice() {
        let temp = TempDir::new().expect("tempdir");
        fs::write(temp.path().join("conflicted"), []).expect("file");
        let mut tree = FilesTree::new(temp.path().to_path_buf()).expect("tree");
        tree.load_directory(Path::new("")).expect("root");
        let conflicted = VcsEntryStatus::new(
            PathBuf::from("conflicted"),
            None,
            VcsStatusKind::Conflicted,
            None,
            Some(VcsStatusKind::Conflicted),
        )
        .expect("conflicted status");
        tree.merge_status(&VcsStatusSnapshot::new(vec![conflicted], true))
            .expect("merge status");
        let rows = [PathBuf::from("conflicted")];
        let area = Rect::new(0, 0, 56, 2);
        let mut buffer = Buffer::empty(area);

        FilesView::new(
            &tree,
            &rows,
            None,
            Some(("VCS stale", "passive mode; working copy not snapshotted")),
        )
        .render(area, &mut buffer);

        let first = (0..area.width)
            .map(|x| buffer[(x, 0)].symbol())
            .collect::<String>();
        let second = (0..area.width)
            .map(|x| buffer[(x, 1)].symbol())
            .collect::<String>();
        assert!(first.starts_with("! conflicted"));
        assert!(second.starts_with("VCS stale: passive mode"));
    }
}
