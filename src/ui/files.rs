use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use crate::config::DisplayMode;
use crate::files::tree::{ChangedFile, FilesTree, TreeNodeKind};
use crate::vcs::{VcsDiffStats, VcsStatusKind};

use super::{file_display, sanitize_terminal_cow, sanitize_terminal_text, theme};

/// Search bar snapshot rendered above the tree while a search is active.
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
    icon_colors: bool,
    header: Option<&'a str>,
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
            icon_colors: true,
            header: None,
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

    #[must_use]
    pub const fn with_icon_colors(mut self, icon_colors: bool) -> Self {
        self.icon_colors = icon_colors;
        self
    }

    #[must_use]
    pub const fn with_header(mut self, header: &'a str) -> Self {
        self.header = Some(header);
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
        let header_height = u16::from(self.header.is_some());
        let row_limit = area
            .height
            .saturating_sub(search_height)
            .saturating_sub(notice_height)
            .saturating_sub(header_height) as usize;
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
                Rect::new(
                    area.x,
                    area.y
                        .saturating_add(search_height)
                        .saturating_add(header_height),
                    area.width,
                    1,
                ),
                buffer,
            );
        }
        if let Some(header) = self.header {
            render_header(
                header,
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
                .saturating_add(header_height)
                .saturating_add(offset as u16);
            let (marker, marker_style) = status_marker(node.status());
            let row_style = if node.is_ignored() && node.status().is_none() {
                theme::inactive()
            } else {
                marker_style
            };
            let expanded = self
                .expanded
                .is_some_and(|expanded| expanded.contains(node.path()));
            let depth = self.tree.display_depth(node.path());
            let mut spans = Vec::with_capacity(depth.saturating_add(8));
            spans.push(Span::styled(marker, marker_style));
            spans.push(Span::raw(" "));
            for _ in 0..depth {
                spans.push(Span::raw("  "));
            }
            spans.push(Span::styled(
                file_display::state_glyph(self.display_mode, node.kind(), expanded),
                row_style,
            ));
            let icon_accent = (self.display_mode == DisplayMode::Nerd && self.icon_colors)
                .then(|| file_display::icon_rgb(node.kind(), node.path()))
                .flatten()
                .filter(|_| !(node.is_ignored() && node.status().is_none()));
            if self.display_mode == DisplayMode::Nerd {
                let icon_style =
                    icon_accent.map_or(row_style, |(r, g, b)| Style::new().fg(Color::Rgb(r, g, b)));
                spans.push(Span::styled(
                    file_display::icon(self.display_mode, node, expanded),
                    icon_style,
                ));
            }
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                sanitize_terminal_cow(self.tree.display_path(node.path()).to_string_lossy()),
                row_style,
            ));
            if node.kind() == TreeNodeKind::Virtual {
                spans.push(Span::styled(" (missing)", theme::inactive()));
            }
            Line::from(spans).render(Rect::new(area.x, y, area.width, 1), buffer);
            if self.selected == Some(node.path()) {
                buffer.set_style(Rect::new(area.x, y, area.width, 1), theme::selection_band());
            }
        }

        if let Some((label, message)) = self.notice {
            let rect = Rect::new(
                area.x,
                area.y.saturating_add(area.height.saturating_sub(1)),
                area.width,
                1,
            );
            render_notice(label, message, rect, buffer);
        }
    }
}

pub(crate) fn render_search(search: FileSearchDisplay<'_>, area: Rect, buffer: &mut Buffer) {
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

/// Renders the uppercase project header above the tree viewport. Overflow
/// past the pane width clips; the name is the root directory's own.
fn render_header(header: &str, area: Rect, buffer: &mut Buffer) {
    if area.is_empty() {
        return;
    }
    let label = sanitize_terminal_text(header).to_uppercase();
    Span::styled(label, Style::new().fg(theme::ACCENT).bold()).render(area, buffer);
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

pub(crate) fn render_notice(label: &str, message: &str, area: Rect, buffer: &mut Buffer) {
    Line::from(vec![
        Span::styled(format!("{label}: "), theme::error().bold()),
        Span::styled(sanitize_terminal_text(message), theme::error()),
    ])
    .render(area, buffer);
}

/// View-model for the flat modified-files pane.
#[derive(Clone, Copy, Debug)]
pub struct ChangedPane<'a> {
    pub rows: &'a [ChangedFile],
    pub selected: Option<&'a Path>,
    pub empty_message: Option<&'a str>,
}

/// Renders the flat modified-files list; rows are marker + relative path.
pub(crate) fn render_changed_pane(pane: ChangedPane<'_>, area: Rect, buffer: &mut Buffer) {
    if area.is_empty() {
        return;
    }
    if pane.rows.is_empty() {
        if let Some(message) = pane.empty_message {
            Span::styled(sanitize_terminal_text(message), theme::inactive()).render(area, buffer);
        }
        return;
    }
    for (offset, file) in pane.rows.iter().enumerate() {
        let y = area.y.saturating_add(offset as u16);
        let (marker, marker_style) = status_marker(Some(file.kind()));
        let mut spans = Vec::with_capacity(4);
        spans.push(Span::styled(marker, marker_style));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            sanitize_terminal_cow(file.path().to_string_lossy()),
            marker_style,
        ));
        if file.is_missing() {
            spans.push(Span::styled(" (missing)", theme::inactive()));
        }
        Line::from(spans).render(Rect::new(area.x, y, area.width, 1), buffer);
        if pane.selected == Some(file.path()) {
            buffer.set_style(Rect::new(area.x, y, area.width, 1), theme::selection_band());
        }
    }
}

const fn divider_glyph(mode: DisplayMode) -> &'static str {
    match mode {
        DisplayMode::Ascii => "-",
        DisplayMode::Unicode | DisplayMode::Nerd => "─",
    }
}

/// Renders the labeled rule separating the tree pane from the flat list.
/// The rule stays muted; the label carries the entry count or, when the
/// backend reported line totals, green/red `+/−` numbers that light up with
/// the accent color while keyboard focus lives in the flat pane.
pub(crate) fn render_changed_divider(
    count: usize,
    diff: Option<VcsDiffStats>,
    focused: bool,
    mode: DisplayMode,
    refresh_hint: Option<&'static str>,
    area: Rect,
    buffer: &mut Buffer,
) {
    if area.is_empty() {
        return;
    }
    let glyph = divider_glyph(mode);
    let width = usize::from(area.width);
    let line_style = theme::inactive();
    let label_style = if focused {
        Style::new().fg(theme::ACCENT)
    } else {
        theme::inactive()
    };
    // Build the labeled rule first; fall back to a bare full-width rule when
    // the pane is too narrow to carry the label unclipped. The refresh hint
    // rides right-aligned and is dropped first when width runs out.
    let hint_width = refresh_hint.map_or(0, |hint| hint.chars().count() + 1);
    let mut labeled = Vec::with_capacity(8);
    let mut plain_width = glyph.chars().count() + 2;
    labeled.push(Span::styled(glyph, line_style));
    labeled.push(Span::raw(" "));
    plain_width += "Changed".chars().count();
    labeled.push(Span::styled("Changed", label_style));
    match diff {
        Some(stats) if stats.insertions() != 0 || stats.deletions() != 0 => {
            let insertions = format!(" +{}", stats.insertions());
            plain_width += insertions.chars().count();
            labeled.push(Span::styled(insertions, Style::new().fg(theme::VCS_ADDED)));
            let deletions = format!(" -{}", stats.deletions());
            plain_width += deletions.chars().count();
            labeled.push(Span::styled(deletions, Style::new().fg(theme::VCS_DELETED)));
        }
        None => {
            let count_label = format!(" ({count})");
            plain_width += count_label.chars().count();
            labeled.push(Span::styled(count_label, label_style));
        }
        Some(_) => {}
    }
    labeled.push(Span::raw(" "));
    if width >= plain_width + hint_width {
        labeled.push(Span::styled(
            glyph.repeat(width.saturating_sub(plain_width + hint_width)),
            line_style,
        ));
        if let Some(hint) = refresh_hint {
            labeled.push(Span::styled(format!(" {hint}"), line_style));
        }
        Line::from(labeled).render(area, buffer);
    } else if width >= plain_width {
        labeled.push(Span::styled(
            glyph.repeat(width.saturating_sub(plain_width)),
            line_style,
        ));
        Line::from(labeled).render(area, buffer);
    } else {
        Span::styled(glyph.repeat(width), line_style).render(area, buffer);
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

    use super::{FileSearchDisplay, FilesView, theme};
    use crate::config::DisplayMode;
    use crate::files::tree::FilesTree;
    use crate::vcs::{VcsDiffStats, VcsEntryStatus, VcsStatusKind, VcsStatusSnapshot};
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
        assert!(first.starts_with("D ! deleted file (missing)"));
        assert!(second.starts_with("VCS: status failed"));
        // Selection is a full-width neutral band; foregrounds survive it.
        assert_eq!(buffer[(0, 0)].bg, theme::SELECTION);
        assert_eq!(buffer[(39, 0)].bg, theme::SELECTION);
        assert_eq!(buffer[(0, 0)].fg, Color::Rgb(248, 81, 73));
        assert_eq!(buffer[(2, 0)].fg, Color::Rgb(248, 81, 73));
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
    fn renders_conflict_marker_rows() {
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

        FilesView::new(&tree, &rows, None, None).render(area, &mut buffer);

        let first = (0..area.width)
            .map(|x| buffer[(x, 0)].symbol())
            .collect::<String>();
        assert!(first.starts_with("! f conflicted"));
        assert_eq!(buffer[(0, 0)].fg, Color::Rgb(248, 81, 73));
    }

    #[test]
    fn renders_ascii_unicode_and_nerd_tree_modes() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join("expanded")).expect("expanded directory");
        fs::create_dir(temp.path().join("collapsed")).expect("collapsed directory");
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
        assert!(ascii[0].starts_with("  + collapsed"));
        assert!(ascii[1].starts_with("  - expanded"));
        assert!(ascii[2].starts_with("    f child.rs"));
        assert!(ascii[3].starts_with("  f main.rs"));

        let unicode = render(DisplayMode::Unicode);
        assert!(unicode[0].starts_with("  ▸ collapsed"));
        assert!(unicode[1].starts_with("  ▾ expanded"));
        assert!(unicode[2].starts_with("    • child.rs"));
        assert!(unicode[3].starts_with("  • main.rs"));

        // Nerd rows keep the typed icons (private-use glyphs stay untyped
        // here); buffer rows are space-padded, so trim before anchoring.
        let nerd = render(DisplayMode::Nerd);
        assert!(
            nerd[0].trim_end().starts_with("  ▸") && nerd[0].trim_end().ends_with(" collapsed")
        );
        assert!(nerd[1].trim_end().starts_with("  ▾") && nerd[1].trim_end().ends_with(" expanded"));
        assert!(
            nerd[2].trim_end().starts_with("       ") && nerd[2].trim_end().ends_with(" child.rs")
        );
        assert!(
            nerd[3].trim_end().starts_with("     ") && nerd[3].trim_end().ends_with(" main.rs")
        );
    }

    #[test]
    fn renders_the_project_header_above_the_tree() {
        let temp = TempDir::new().expect("tempdir");
        fs::write(temp.path().join("main.rs"), []).expect("rust file");
        let mut tree = FilesTree::new(temp.path().to_path_buf()).expect("tree");
        tree.load_directory(Path::new("")).expect("root");
        let rows = [PathBuf::from("main.rs")];
        let area = Rect::new(0, 0, 20, 2);
        let mut buffer = Buffer::empty(area);

        FilesView::new(&tree, &rows, None, None)
            .with_header("proj")
            .render(area, &mut buffer);

        let header = (0..area.width)
            .map(|column| buffer[(column, 0)].symbol())
            .collect::<String>();
        assert!(header.starts_with("PROJ"));
        assert_eq!(buffer[(0, 0)].fg, theme::ACCENT);
        assert!(
            buffer[(0, 0)]
                .modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
        let row = (0..area.width)
            .map(|column| buffer[(column, 1)].symbol())
            .collect::<String>();
        assert!(row.starts_with("  f main.rs"));
    }

    #[test]
    fn nerd_icon_colors_follow_the_configuration() {
        let temp = TempDir::new().expect("tempdir");
        fs::write(temp.path().join("main.rs"), []).expect("rust file");
        let mut tree = FilesTree::new(temp.path().to_path_buf()).expect("tree");
        tree.load_directory(Path::new("")).expect("root");
        let rows = [PathBuf::from("main.rs")];
        let area = Rect::new(0, 0, 24, 1);

        let mut colored = Buffer::empty(area);
        FilesView::new(&tree, &rows, None, None)
            .with_display_mode(DisplayMode::Nerd)
            .render(area, &mut colored);
        assert_eq!(colored[(5, 0)].fg, Color::Rgb(222, 165, 132));

        let mut monochrome = Buffer::empty(area);
        FilesView::new(&tree, &rows, None, None)
            .with_display_mode(DisplayMode::Nerd)
            .with_icon_colors(false)
            .render(area, &mut monochrome);
        assert_eq!(monochrome[(5, 0)].fg, Color::Reset);
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
        // New row anatomy: [marker][space][indent][state][space][name].
        assert_eq!(buffer[(0, 0)].fg, Color::Rgb(210, 153, 34));
        assert_eq!(buffer[(2, 0)].fg, Color::Rgb(210, 153, 34));
        assert_eq!(buffer[(4, 0)].fg, Color::Rgb(210, 153, 34));
        assert_eq!(buffer[(4, 1)].fg, Color::Rgb(210, 153, 34));
        assert_eq!(buffer[(6, 1)].fg, Color::Rgb(210, 153, 34));
        assert_eq!(buffer[(0, 2)].fg, Color::Rgb(63, 185, 80));
        assert_eq!(buffer[(2, 2)].fg, Color::Rgb(63, 185, 80));
        assert_eq!(buffer[(4, 2)].fg, Color::Rgb(63, 185, 80));
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

    #[test]
    fn renders_the_changed_pane_with_markers_missing_rows_and_selection() {
        let temp = TempDir::new().expect("tempdir");
        fs::write(temp.path().join("mod.rs"), []).expect("modified file");
        let mut tree = FilesTree::new(temp.path().to_path_buf()).expect("tree");
        tree.merge_status(&VcsStatusSnapshot::new(
            vec![
                status_entry("gone.rs", VcsStatusKind::Deleted),
                status_entry("mod.rs", VcsStatusKind::Modified),
                status_entry("new.rs", VcsStatusKind::Untracked),
            ],
            false,
        ))
        .expect("merge status");

        let area = Rect::new(0, 0, 24, 3);
        let mut buffer = Buffer::empty(area);
        super::render_changed_pane(
            super::ChangedPane {
                rows: tree.changed_files(),
                selected: Some(Path::new("mod.rs")),
                empty_message: Some("No modified files"),
            },
            area,
            &mut buffer,
        );
        let line = |y: u16| {
            (0..area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        };
        assert!(line(0).starts_with("D gone.rs (missing)"));
        assert!(line(1).starts_with("M mod.rs"));
        assert!(line(2).starts_with("? new.rs"));

        let mut plain = Buffer::empty(area);
        super::render_changed_pane(
            super::ChangedPane {
                rows: tree.changed_files(),
                selected: None,
                empty_message: None,
            },
            area,
            &mut plain,
        );
        assert_ne!(buffer[(0, 1)].style(), plain[(0, 1)].style());
    }

    #[test]
    fn renders_the_changed_pane_empty_message_without_rows() {
        let area = Rect::new(0, 0, 20, 2);
        let mut buffer = Buffer::empty(area);
        super::render_changed_pane(
            super::ChangedPane {
                rows: &[],
                selected: None,
                empty_message: Some("No modified files"),
            },
            area,
            &mut buffer,
        );
        let line: String = (0..area.width).map(|x| buffer[(x, 0)].symbol()).collect();
        assert!(line.starts_with("No modified files"));
    }

    #[test]
    fn renders_the_changed_divider_with_diff_totals_and_focus_accent() {
        let area = Rect::new(0, 0, 24, 1);
        let mut muted = Buffer::empty(area);
        super::render_changed_divider(
            3,
            Some(VcsDiffStats::new(12, 4)),
            false,
            DisplayMode::Ascii,
            None,
            area,
            &mut muted,
        );
        let mut focused = Buffer::empty(area);
        super::render_changed_divider(
            3,
            Some(VcsDiffStats::new(12, 4)),
            true,
            DisplayMode::Ascii,
            None,
            area,
            &mut focused,
        );
        let line = |buffer: &Buffer| {
            (0..area.width)
                .map(|x| buffer[(x, 0)].symbol())
                .collect::<String>()
        };
        assert!(line(&muted).starts_with("- Changed +12 -4 -"));
        assert!(line(&muted).ends_with('-'));
        // Same glyphs in both states; only the label styling changes.
        assert_eq!(line(&muted), line(&focused));
        assert_ne!(muted[(2, 0)].style(), focused[(2, 0)].style());
        // Insertions and deletions carry the VCS added/deleted colors.
        assert_eq!(muted[(11, 0)].fg, theme::VCS_ADDED);
        assert_eq!(muted[(15, 0)].fg, theme::VCS_DELETED);
    }

    #[test]
    fn falls_back_to_the_file_count_without_diff_stats() {
        let area = Rect::new(0, 0, 24, 1);
        let mut buffer = Buffer::empty(area);
        super::render_changed_divider(3, None, false, DisplayMode::Ascii, None, area, &mut buffer);
        let line: String = (0..area.width).map(|x| buffer[(x, 0)].symbol()).collect();
        assert!(line.starts_with("- Changed (3) -"));

        // Zeroed totals stay quiet instead of showing "+0 -0".
        let mut zeroed = Buffer::empty(area);
        super::render_changed_divider(
            0,
            Some(VcsDiffStats::new(0, 0)),
            false,
            DisplayMode::Ascii,
            None,
            area,
            &mut zeroed,
        );
        let line: String = (0..area.width).map(|x| zeroed[(x, 0)].symbol()).collect();
        assert!(line.starts_with("- Changed -"));
    }

    #[test]
    fn renders_the_unicode_rule_in_unicode_mode() {
        let area = Rect::new(0, 0, 16, 1);
        let mut buffer = Buffer::empty(area);
        super::render_changed_divider(
            1,
            None,
            false,
            DisplayMode::Unicode,
            None,
            area,
            &mut buffer,
        );
        let line: String = (0..area.width).map(|x| buffer[(x, 0)].symbol()).collect();
        assert!(line.starts_with("─ Changed (1) ─"));
    }

    #[test]
    fn falls_back_to_a_plain_rule_when_the_label_does_not_fit() {
        let area = Rect::new(0, 0, 6, 1);
        let mut buffer = Buffer::empty(area);
        super::render_changed_divider(3, None, false, DisplayMode::Ascii, None, area, &mut buffer);
        let line: String = (0..area.width).map(|x| buffer[(x, 0)].symbol()).collect();
        assert_eq!(line, "------");
    }

    #[test]
    fn renders_the_refresh_hint_right_aligned_and_drops_it_when_narrow() {
        let area = Rect::new(0, 0, 24, 1);
        let mut buffer = Buffer::empty(area);
        super::render_changed_divider(
            3,
            None,
            false,
            DisplayMode::Ascii,
            Some("manual"),
            area,
            &mut buffer,
        );
        let line: String = (0..area.width).map(|x| buffer[(x, 0)].symbol()).collect();
        assert!(line.starts_with("- Changed (3) "));
        assert!(line.ends_with(" manual"));

        // Too narrow for the hint: it disappears before the label does.
        let narrow = Rect::new(0, 0, 18, 1);
        let mut buffer = Buffer::empty(narrow);
        super::render_changed_divider(
            3,
            None,
            false,
            DisplayMode::Ascii,
            Some("manual"),
            narrow,
            &mut buffer,
        );
        let line: String = (0..narrow.width).map(|x| buffer[(x, 0)].symbol()).collect();
        assert!(!line.contains("manual"));
        assert!(line.ends_with('-'));
    }

    #[test]
    fn dims_ignored_rows_without_status_and_keeps_other_coloring() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join(".git")).expect("git marker");
        fs::write(temp.path().join(".gitignore"), b"skipped.txt\n").expect("gitignore");
        fs::write(temp.path().join("skipped.txt"), []).expect("ignored file");
        fs::write(temp.path().join("kept.rs"), []).expect("kept file");
        let mut tree = FilesTree::with_visibility_policy(
            temp.path().to_path_buf(),
            std::sync::Arc::new(crate::files::ignore::ConfiguredVisibilityPolicy::new(
                false,
                Vec::new(),
            )),
            true,
        )
        .expect("tree");
        tree.load_directory(Path::new("")).expect("root");
        assert!(
            tree.node(Path::new("skipped.txt"))
                .is_some_and(|node| node.is_ignored())
        );
        let rows = [PathBuf::from("kept.rs"), PathBuf::from("skipped.txt")];
        let area = Rect::new(0, 0, 40, 2);
        let mut buffer = Buffer::empty(area);
        FilesView::new(&tree, &rows, None, None).render(area, &mut buffer);

        // Ignored without status renders dim somewhere on icon/name; the
        // ordinary row keeps default coloring. Layout-agnostic on purpose.
        let row_colors = |row: u16| {
            (0..area.width)
                .map(|x| buffer[(x, row)].fg)
                .collect::<Vec<_>>()
        };
        assert!(row_colors(1).contains(&Color::DarkGray));
        assert!(!row_colors(0).contains(&Color::DarkGray));
    }

    fn status_entry(path: &str, kind: VcsStatusKind) -> VcsEntryStatus {
        VcsEntryStatus::new(PathBuf::from(path), None, kind, None, None).expect("status entry")
    }
}
