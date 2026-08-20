use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use crate::config::DisplayMode;
use crate::files::tree::{FilesTree, TreeNodeKind};
use crate::vcs::VcsStatusKind;

use super::{file_display, sanitize_terminal_cow, sanitize_terminal_text, theme};

#[derive(Clone, Copy, Debug)]
pub struct FileSearchDisplay<'a> {
    pub query: &'a str,
    pub editing: bool,
    pub matches: usize,
    pub scanning: bool,
    pub truncated: bool,
    pub skipped_directories: usize,
}

/// Renders a caller-provided viewport slice; it never scans or indexes the tree.
pub struct FilesView<'a> {
    tree: &'a FilesTree,
    rows: &'a [PathBuf],
    selected: Option<&'a Path>,
    notice: Option<(&'a str, &'a str)>,
    expanded: Option<&'a BTreeSet<PathBuf>>,
    search: Option<FileSearchDisplay<'a>>,
    display_mode: DisplayMode,
}

impl<'a> FilesView<'a> {
    #[must_use]
    pub const fn new(
        tree: &'a FilesTree,
        rows: &'a [PathBuf],
        selected: Option<&'a Path>,
        notice: Option<(&'a str, &'a str)>,
    ) -> Self {
        Self {
            tree,
            rows,
            selected,
            notice,
            expanded: None,
            search: None,
            display_mode: DisplayMode::Ascii,
        }
    }

    #[must_use]
    pub const fn with_expanded(mut self, expanded: &'a BTreeSet<PathBuf>) -> Self {
        self.expanded = Some(expanded);
        self
    }

    #[must_use]
    pub const fn with_search(mut self, search: FileSearchDisplay<'a>) -> Self {
        self.search = Some(search);
        self
    }

    #[must_use]
    pub const fn with_display_mode(mut self, display_mode: DisplayMode) -> Self {
        self.display_mode = display_mode;
        self
    }
}

impl Widget for FilesView<'_> {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        if area.is_empty() {
            return;
        }
        let search_height = u16::from(self.search.is_some());
        let notice_height = u16::from(self.notice.is_some());
        let row_limit = area
            .height
            .saturating_sub(search_height)
            .saturating_sub(notice_height) as usize;
        if let Some(search) = self.search {
            render_search(search, Rect::new(area.x, area.y, area.width, 1), buffer);
        }
        if let Some(search) = self.search
            && !search.query.is_empty()
            && search.matches == 0
            && row_limit != 0
        {
            let message = if search.scanning {
                "No matches yet".to_owned()
            } else if search.truncated || search.skipped_directories != 0 {
                "No matches in searched paths".to_owned()
            } else {
                format!(
                    "No files match \"{}\"",
                    sanitize_terminal_text(search.query)
                )
            };
            Span::styled(message, theme::inactive()).render(
                Rect::new(area.x, area.y.saturating_add(search_height), area.width, 1),
                buffer,
            );
        }
        for (offset, node) in self
            .rows
            .iter()
            .filter_map(|path| self.tree.node(path))
            .take(row_limit)
            .enumerate()
        {
            let y = area
                .y
                .saturating_add(search_height)
                .saturating_add(offset as u16);
            let (marker, marker_style) = status_marker(node.status());
            let expanded = self
                .expanded
                .is_some_and(|expanded| expanded.contains(node.path()));
            let depth = self.tree.display_depth(node.path());
            let mut spans = Vec::with_capacity(depth.saturating_add(7));
            spans.push(Span::styled(marker, marker_style));
            spans.push(Span::raw(" "));
            push_tree_prefix(&mut spans, self.tree, node.path(), self.display_mode, depth);
            spans.push(Span::styled(
                file_display::icon(self.display_mode, node, expanded),
                marker_style,
            ));
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                sanitize_terminal_cow(self.tree.display_path(node.path()).to_string_lossy()),
                marker_style,
            ));
            if node.kind() == TreeNodeKind::Virtual {
                spans.push(Span::styled(" (missing)", theme::inactive()));
            }
            let mut line = Line::from(spans);
            if self.selected == Some(node.path()) {
                line = line.style(theme::selected_neutral());
            }
            line.render(Rect::new(area.x, y, area.width, 1), buffer);
        }

        if let Some((label, notice)) = self.notice {
            Line::from(vec![
                Span::styled(format!("{label}: "), theme::error().bold()),
                Span::styled(sanitize_terminal_text(notice), theme::error()),
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

fn render_search(search: FileSearchDisplay<'_>, area: Rect, buffer: &mut Buffer) {
    if area.is_empty() {
        return;
    }
    let status = if search.scanning {
        format!("{} scanning…", search.matches)
    } else if search.skipped_directories != 0 {
        format!(
            "{} partial, {} skipped",
            search.matches, search.skipped_directories
        )
    } else if search.truncated {
        format!("{} partial", search.matches)
    } else {
        format!(
            "{} match{}",
            search.matches,
            if search.matches == 1 { "" } else { "es" }
        )
    };
    let status_width = status.chars().count().min(usize::from(area.width) / 2);
    let left_width = usize::from(area.width)
        .saturating_sub(status_width)
        .saturating_sub(1);
    let prefix = "Search > ";
    let cursor_width = usize::from(search.editing);
    let query_width = left_width
        .saturating_sub(prefix.chars().count())
        .saturating_sub(cursor_width);
    let query_chars = search.query.chars().collect::<Vec<_>>();
    let display_query = if query_chars.len() <= query_width {
        search.query.to_owned()
    } else if query_width == 0 {
        String::new()
    } else {
        let tail = query_chars
            .iter()
            .skip(
                query_chars
                    .len()
                    .saturating_sub(query_width.saturating_sub(1)),
            )
            .collect::<String>();
        format!("…{tail}")
    };
    let mut left = vec![
        Span::styled(prefix, theme::inactive()),
        Span::raw(sanitize_terminal_text(&display_query).into_owned()),
    ];
    if search.editing {
        left.push(Span::styled(" ", theme::selected()));
    }
    Line::from(left).render(Rect::new(area.x, area.y, left_width as u16, 1), buffer);
    let status_style = if search.truncated || search.skipped_directories != 0 {
        theme::warning()
    } else {
        theme::inactive()
    };
    Span::styled(status, status_style).render(
        Rect::new(
            area.x
                .saturating_add(area.width.saturating_sub(status_width as u16)),
            area.y,
            status_width as u16,
            1,
        ),
        buffer,
    );
}

fn push_tree_prefix<'a>(
    spans: &mut Vec<Span<'a>>,
    tree: &FilesTree,
    path: &Path,
    mode: DisplayMode,
    depth: usize,
) {
    let prefix_start = spans.len();
    for _ in 0..depth {
        spans.push(Span::raw(""));
    }
    let mut prefix_index = depth;
    let mut parent = tree.display_parent_of(path);
    while let Some(current) = parent.filter(|parent| !parent.as_os_str().is_empty()) {
        prefix_index = prefix_index.saturating_sub(1);
        spans[prefix_start.saturating_add(prefix_index)] = Span::raw(file_display::ancestor(
            mode,
            tree.is_last_display_child(current),
        ));
        parent = tree.display_parent_of(current);
    }
    spans.push(Span::raw(file_display::branch(
        mode,
        tree.is_last_display_child(path),
    )));
}

const fn status_marker(status: Option<VcsStatusKind>) -> (&'static str, Style) {
    match status {
        Some(VcsStatusKind::Added) => ("A", Style::new().fg(theme::VCS_ADDED)),
        Some(VcsStatusKind::Modified) => ("M", Style::new().fg(theme::VCS_MODIFIED)),
        Some(VcsStatusKind::Deleted) => ("D", Style::new().fg(theme::VCS_DELETED)),
        Some(VcsStatusKind::Renamed) => ("R", Style::new().fg(theme::MOVED)),
        Some(VcsStatusKind::Copied) => ("C", Style::new().fg(theme::MOVED)),
        Some(VcsStatusKind::Untracked) => ("?", Style::new().fg(theme::UNTRACKED)),
        Some(VcsStatusKind::Conflicted) => ("!", Style::new().fg(theme::VCS_DELETED).bold()),
        Some(VcsStatusKind::TypeChanged) => ("T", Style::new().fg(theme::ACCENT)),
        None => (" ", Style::new()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::{Path, PathBuf};

    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::Color;
    use ratatui::widgets::Widget;

    use super::{FileSearchDisplay, FilesView};
    use crate::config::DisplayMode;
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
        assert!(first.starts_with("D `- ! deleted file (missing)"));
        assert!(second.starts_with("VCS: status failed"));
        assert!(
            buffer[(0, 0)]
                .modifier
                .contains(ratatui::style::Modifier::REVERSED)
        );
        assert_eq!(buffer[(2, 0)].fg, Color::Reset);
        assert!(
            buffer[(2, 0)]
                .modifier
                .contains(ratatui::style::Modifier::REVERSED)
        );
        assert_eq!(buffer[(0, 0)].fg, Color::Rgb(248, 81, 73));
        assert_eq!(buffer[(7, 0)].fg, Color::Rgb(248, 81, 73));
        assert_eq!(buffer[(20, 0)].fg, Color::DarkGray);
        assert_eq!(buffer[(0, 1)].fg, Color::Red);
    }

    #[test]
    fn disambiguates_virtual_paths_reparented_to_the_same_row_level() {
        let temp = TempDir::new().expect("tempdir");
        let mut tree = FilesTree::new(temp.path().to_path_buf()).expect("tree");
        let deleted = |path| {
            VcsEntryStatus::new(
                PathBuf::from(path),
                None,
                VcsStatusKind::Deleted,
                Some(VcsStatusKind::Deleted),
                None,
            )
            .expect("deleted status")
        };
        tree.merge_status(&VcsStatusSnapshot::new(
            vec![deleted("old/src/main.rs"), deleted("new/src/main.rs")],
            false,
        ))
        .expect("merge status");
        let rows = tree
            .children(Path::new(""))
            .into_iter()
            .map(|node| node.path().to_path_buf())
            .collect::<Vec<_>>();
        let area = Rect::new(0, 0, 48, 2);
        let mut buffer = Buffer::empty(area);

        FilesView::new(&tree, &rows, None, None).render(area, &mut buffer);

        let rendered = (0..area.height)
            .map(|row| {
                (0..area.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert!(rendered.iter().any(|line| line.contains("new/src/main.rs")));
        assert!(rendered.iter().any(|line| line.contains("old/src/main.rs")));
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
        assert!(first.starts_with("! `- f conflicted"));
        assert!(second.starts_with("VCS stale: passive mode"));
        assert_eq!(buffer[(0, 0)].fg, Color::Rgb(248, 81, 73));
        assert_eq!(buffer[(0, 1)].fg, Color::Red);
    }

    #[test]
    fn renders_ascii_unicode_and_nerd_tree_modes() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join("collapsed")).expect("collapsed directory");
        fs::create_dir(temp.path().join("expanded")).expect("expanded directory");
        fs::write(temp.path().join("expanded/child.rs"), []).expect("nested Rust file");
        fs::write(temp.path().join("main.rs"), []).expect("root Rust file");
        let mut tree = FilesTree::new(temp.path().to_path_buf()).expect("tree");
        tree.load_directory(Path::new("")).expect("root");
        tree.load_directory(Path::new("expanded"))
            .expect("expanded directory");
        let rows = [
            PathBuf::from("collapsed"),
            PathBuf::from("expanded"),
            PathBuf::from("expanded/child.rs"),
            PathBuf::from("main.rs"),
        ];
        let expanded = BTreeSet::from([PathBuf::from("expanded")]);
        let area = Rect::new(0, 0, 40, 4);
        let render = |mode| {
            let mut buffer = Buffer::empty(area);
            FilesView::new(&tree, &rows, None, None)
                .with_expanded(&expanded)
                .with_display_mode(mode)
                .render(area, &mut buffer);
            (0..area.height)
                .map(|row| {
                    (0..area.width)
                        .map(|column| buffer[(column, row)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
        };

        let ascii = render(DisplayMode::Ascii);
        assert!(ascii[0].starts_with("  |- + collapsed"));
        assert!(ascii[1].starts_with("  |- - expanded"));
        assert!(ascii[2].starts_with("  |  `- f child.rs"));
        assert!(ascii[3].starts_with("  `- f main.rs"));

        let unicode = render(DisplayMode::Unicode);
        assert!(unicode[0].starts_with("  ├── ▸ collapsed"));
        assert!(unicode[1].starts_with("  ├── ▾ expanded"));
        assert!(unicode[2].starts_with("  │   └── • child.rs"));
        assert!(unicode[3].starts_with("  └── • main.rs"));

        let nerd = render(DisplayMode::Nerd);
        assert!(nerd[0].starts_with("  ├──  collapsed"));
        assert!(nerd[1].starts_with("  ├──  expanded"));
        assert!(nerd[2].starts_with("  │   └──  child.rs"));
        assert!(nerd[3].starts_with("  └──  main.rs"));
    }

    #[test]
    fn colors_directory_and_file_icons_and_names_from_vcs_status() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join("src")).expect("src");
        fs::write(temp.path().join("src/main.rs"), []).expect("Rust file");
        fs::write(temp.path().join("added.rs"), []).expect("added Rust file");
        let mut tree = FilesTree::new(temp.path().to_path_buf()).expect("tree");
        tree.load_directory(Path::new("")).expect("root");
        tree.load_directory(Path::new("src")).expect("src");
        let modified = VcsEntryStatus::new(
            PathBuf::from("src/main.rs"),
            None,
            VcsStatusKind::Modified,
            None,
            Some(VcsStatusKind::Modified),
        )
        .expect("modified status");
        let added = VcsEntryStatus::new(
            PathBuf::from("added.rs"),
            None,
            VcsStatusKind::Added,
            Some(VcsStatusKind::Added),
            None,
        )
        .expect("added status");
        tree.merge_status(&VcsStatusSnapshot::new(vec![modified, added], false))
            .expect("merge status");
        let rows = [
            PathBuf::from("src"),
            PathBuf::from("src/main.rs"),
            PathBuf::from("added.rs"),
        ];
        let expanded = BTreeSet::from([PathBuf::from("src")]);
        let area = Rect::new(0, 0, 32, 3);
        let mut buffer = Buffer::empty(area);

        FilesView::new(&tree, &rows, None, None)
            .with_expanded(&expanded)
            .render(area, &mut buffer);

        assert_eq!(buffer[(5, 0)].fg, Color::Rgb(210, 153, 34));
        assert_eq!(buffer[(7, 0)].fg, Color::Rgb(210, 153, 34));
        assert_eq!(buffer[(8, 1)].fg, Color::Rgb(210, 153, 34));
        assert_eq!(buffer[(10, 1)].fg, Color::Rgb(210, 153, 34));
        assert_eq!(buffer[(5, 2)].fg, Color::Rgb(63, 185, 80));
        assert_eq!(buffer[(7, 2)].fg, Color::Rgb(63, 185, 80));
    }

    #[test]
    fn renders_search_status_and_empty_result_message_in_the_files_view() {
        let temp = TempDir::new().expect("tempdir");
        let tree = FilesTree::new(temp.path().to_path_buf()).expect("tree");
        let area = Rect::new(0, 0, 40, 2);
        let mut buffer = Buffer::empty(area);

        FilesView::new(&tree, &[], None, None)
            .with_search(FileSearchDisplay {
                query: "auth",
                editing: true,
                matches: 0,
                scanning: false,
                truncated: false,
                skipped_directories: 2,
            })
            .render(area, &mut buffer);

        let first = (0..area.width)
            .map(|x| buffer[(x, 0)].symbol())
            .collect::<String>();
        let second = (0..area.width)
            .map(|x| buffer[(x, 1)].symbol())
            .collect::<String>();
        assert!(first.contains("Search > auth"));
        assert!(first.ends_with("0 partial, 2 skipped"));
        assert!(second.starts_with("No matches in searched paths"));
    }
}
