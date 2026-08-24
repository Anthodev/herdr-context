use std::collections::{BTreeSet, VecDeque};
use std::error::Error;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::config::{
    DisplayMode, FilesConfig, GitCadence, PluginConfig, RefreshPolicy, VcsBackendSelection,
};
use crate::files::ignore::ConfiguredVisibilityPolicy;
use crate::files::search::{
    DIRECTORIES_PER_PAGE, IndexedSearchPath, MAX_EXAMINED_ENTRIES, MAX_INDEX_ENTRIES,
    MAX_QUERY_BYTES, PreparedSearchPage, SearchPageRequest, SearchProjection, SearchablePaths,
    prepare_page, project,
};
use crate::files::tree::{ChangedFile, DirectorySnapshot, FilesTree, TreeNodeKind};
use crate::files::{FilesModel, PreparedRefreshResult};
use crate::host::LaunchContext;
use crate::intent::{Intent, PointerAction};
use crate::project::resolve_project_context_with_backend;
use crate::ui::files::{
    ChangedPane, FileSearchDisplay, FilesView, render_changed_divider, render_changed_pane,
    render_notice,
};
use crate::ui::sanitize_terminal_text;
use crate::vcs::git::GitService;
use crate::vcs::jj::{JjService, JujutsuMode};
use crate::vcs::{VcsBackendMetadata, VcsWorkspace};
use crate::worker::{Job, JobKey, JobKind, Priority, SubmitStatus, WorkerRuntime};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

const MAX_FILE_REFERENCE_BYTES: usize = 4_096;

#[derive(Clone, Debug)]
enum VcsRefresh {
    Git {
        service: GitService,
        workspace: VcsWorkspace,
    },
    Jujutsu {
        service: JjService,
        workspace: VcsWorkspace,
    },
}

impl VcsRefresh {
    const fn workspace(&self) -> &VcsWorkspace {
        match self {
            Self::Git { workspace, .. } | Self::Jujutsu { workspace, .. } => workspace,
        }
    }
    const fn is_git(&self) -> bool {
        matches!(self, Self::Git { .. })
    }

    fn refresh_status_cancellable(
        &self,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> Result<crate::vcs::VcsStatusSnapshot, crate::vcs::VcsError> {
        match self {
            Self::Git { service, workspace } => {
                service.refresh_status_cancellable(workspace, cancelled)
            }
            Self::Jujutsu { service, workspace } => {
                service.refresh_status_cancellable(workspace, cancelled)
            }
        }
    }

    fn refresh_diff_stats_cancellable(
        &self,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> Result<Option<crate::vcs::VcsDiffStats>, crate::vcs::VcsError> {
        match self {
            Self::Git { service, workspace } => {
                service.refresh_diff_stats_cancellable(workspace, cancelled)
            }
            Self::Jujutsu { service, workspace } => {
                service.refresh_diff_stats_cancellable(workspace, cancelled)
            }
        }
    }

    const fn jujutsu_mode(&self) -> Option<JujutsuMode> {
        match self {
            Self::Jujutsu { service, .. } => Some(service.mode()),
            Self::Git { .. } => None,
        }
    }

    fn set_jujutsu_mode(&mut self, mode: JujutsuMode) -> bool {
        match self {
            Self::Jujutsu { service, .. } => service.set_mode(mode),
            Self::Git { .. } => false,
        }
    }
}

#[derive(Debug)]
struct FileSearchState {
    editing: bool,
    query: String,
    index: FilesTree,
    searchable: SearchablePaths,
    matches: BTreeSet<PathBuf>,
    expanded_context: BTreeSet<PathBuf>,
    pending_directories: VecDeque<PathBuf>,
    running: Option<(u64, u64)>,
    running_directories: Vec<PathBuf>,
    page_cancel: Option<Arc<AtomicBool>>,
    projection_cancel: Option<Arc<AtomicBool>>,
    job_generation: u64,
    scan_epoch: u64,
    scanned_entries: usize,
    examined_entries: usize,
    skipped_directories: usize,
    complete: bool,
    truncated: bool,
    projection_requested: u64,
    projection_submitted: u64,
    projection_applied: u64,
}

impl FileSearchState {
    fn new(index: FilesTree) -> Self {
        let searchable: SearchablePaths = Arc::new(
            index
                .nodes()
                .filter(|node| node.kind() != TreeNodeKind::Directory)
                .map(|node| {
                    (
                        node.path().to_path_buf(),
                        IndexedSearchPath {
                            kind: node.kind(),
                            normalized: node.path().to_string_lossy().to_lowercase(),
                        },
                    )
                })
                .collect(),
        );
        let seeded_entries = searchable.len();
        Self {
            editing: true,
            query: String::new(),
            index,
            searchable,
            matches: BTreeSet::new(),
            expanded_context: BTreeSet::new(),
            pending_directories: VecDeque::from([PathBuf::new()]),
            running: None,
            running_directories: Vec::new(),
            page_cancel: None,
            projection_cancel: None,
            job_generation: 0,
            scan_epoch: 0,
            scanned_entries: seeded_entries,
            examined_entries: 0,
            skipped_directories: 0,
            complete: false,
            truncated: false,
            projection_requested: 0,
            projection_submitted: 0,
            projection_applied: 0,
        }
    }

    const fn active(&self) -> bool {
        !self.query.is_empty()
    }

    const fn should_scan(&self) -> bool {
        self.editing || self.active()
    }

    const fn scanning(&self) -> bool {
        !self.complete
            || self.running.is_some()
            || self.projection_applied < self.projection_requested
    }

    fn sync_virtual_entries(&mut self) {
        let virtual_entries = self
            .index
            .nodes()
            .filter(|node| node.kind() == TreeNodeKind::Virtual)
            .map(|node| {
                (
                    node.path().to_path_buf(),
                    IndexedSearchPath {
                        kind: TreeNodeKind::Virtual,
                        normalized: node.path().to_string_lossy().to_lowercase(),
                    },
                )
            })
            .collect::<Vec<_>>();
        let searchable = Arc::make_mut(&mut self.searchable);
        searchable.retain(|path, indexed| {
            indexed.kind != TreeNodeKind::Virtual
                || self.index.node(path).map(|node| node.kind()) == Some(TreeNodeKind::Virtual)
        });
        searchable.extend(virtual_entries);
    }
}

/// Focused region of the split Files view.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FilesPane {
    Tree,
    Changed,
}

/// Fully connected Files surface. Filesystem and VCS work is dispatched separately.
#[derive(Debug)]
pub struct FilesRuntime {
    model: FilesModel,
    root: PathBuf,
    background_active: bool,
    vcs: Option<VcsRefresh>,
    refresh_policy: RefreshPolicy,
    status_refresh_interval: Duration,
    next_status_refresh: Option<Instant>,
    last_status_fingerprint: Option<u64>,
    expanded: BTreeSet<PathBuf>,
    desired_expanded: BTreeSet<PathBuf>,
    visible_rows: Vec<PathBuf>,
    viewport_offset: usize,
    viewport_height: usize,
    viewport_y: u16,
    focused_pane: FilesPane,
    changed_selected: Option<PathBuf>,
    changed_offset: usize,
    changed_y: u16,
    changed_height: usize,
    status_seen: bool,
    filesystem_generation: u64,
    filesystem_applied_generation: u64,
    filesystem_running: Option<u64>,
    filesystem_expansions_running: BTreeSet<PathBuf>,
    filesystem_reload_running: bool,
    pending_expansions: BTreeSet<PathBuf>,
    reload_pending: bool,
    filesystem_panic_retried: bool,
    filesystem_notice: Option<String>,
    search_page_notice: Option<String>,
    search_projection_notice: Option<String>,
    pane_input_notice: Option<String>,
    backend_notice: Option<String>,
    display_mode: DisplayMode,
    colored_icons: bool,
    show_ignored: bool,
    search_ignored: bool,
    search: Option<FileSearchState>,
}

impl FilesRuntime {
    pub fn bootstrap(context: &LaunchContext) -> Result<Self, FilesRuntimeError> {
        static NOT_CANCELLED: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(false);
        let config = PluginConfig::default();
        Self::bootstrap_with_policy_cancellable(
            context,
            config.vcs().backend(),
            config.vcs().jujutsu_mode(),
            config.vcs().refresh(),
            config.ui(),
            config.files(),
            &NOT_CANCELLED,
        )
    }

    pub fn bootstrap_with_jujutsu_mode(
        context: &LaunchContext,
        jujutsu_mode: JujutsuMode,
    ) -> Result<Self, FilesRuntimeError> {
        static NOT_CANCELLED: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(false);
        let config = PluginConfig::default();
        Self::bootstrap_with_policy_cancellable(
            context,
            config.vcs().backend(),
            jujutsu_mode,
            config.vcs().refresh(),
            config.ui(),
            config.files(),
            &NOT_CANCELLED,
        )
    }

    pub fn bootstrap_with_config(
        context: &LaunchContext,
        config: &PluginConfig,
    ) -> Result<Self, FilesRuntimeError> {
        static NOT_CANCELLED: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(false);
        Self::bootstrap_with_config_cancellable(context, config, &NOT_CANCELLED)
    }

    pub(crate) fn bootstrap_with_config_cancellable(
        context: &LaunchContext,
        config: &PluginConfig,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> Result<Self, FilesRuntimeError> {
        Self::bootstrap_with_policy_cancellable(
            context,
            config.vcs().backend(),
            config.vcs().jujutsu_mode(),
            config.vcs().refresh(),
            config.ui(),
            config.files(),
            cancelled,
        )
    }

    fn bootstrap_with_policy_cancellable(
        context: &LaunchContext,
        backend: VcsBackendSelection,
        jujutsu_mode: JujutsuMode,
        refresh_policy: RefreshPolicy,
        ui: &crate::config::UiConfig,
        files_config: &FilesConfig,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> Result<Self, FilesRuntimeError> {
        if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(FilesRuntimeError::cancelled());
        }
        let opening_directory = context.foreground_cwd().unwrap_or_else(|| context.cwd());
        let project = resolve_project_context_with_backend(opening_directory, backend)
            .map_err(|error| FilesRuntimeError::new(error.to_string()))?;
        let vcs = match project.vcs() {
            Some(detected) if detected.backend().as_str() == "git" => {
                let workspace = VcsWorkspace::new(
                    detected.workspace_root().to_path_buf(),
                    VcsBackendMetadata::new("git", "Git", false)
                        .map_err(|error| FilesRuntimeError::new(error.to_string()))?,
                )
                .map_err(|error| FilesRuntimeError::new(error.to_string()))?;
                Some(VcsRefresh::Git {
                    service: GitService::default(),
                    workspace,
                })
            }
            Some(detected) if detected.backend().as_str() == "jj" => {
                let fallback = VcsWorkspace::new(
                    detected.workspace_root().to_path_buf(),
                    VcsBackendMetadata::new("jj", "Jujutsu", true)
                        .map_err(|error| FilesRuntimeError::new(error.to_string()))?,
                )
                .map_err(|error| FilesRuntimeError::new(error.to_string()))?;
                let service = JjService::new(jujutsu_mode, std::time::Duration::from_secs(5));
                let workspace = match service.detect_cancellable(project.files_root(), cancelled) {
                    Ok(Some(workspace)) => workspace,
                    Ok(None) => fallback,
                    Err(_) if !cancelled.load(std::sync::atomic::Ordering::Relaxed) => fallback,
                    Err(_) => return Err(FilesRuntimeError::cancelled()),
                };
                Some(VcsRefresh::Jujutsu { service, workspace })
            }
            Some(_) | None => None,
        };
        let visibility = Arc::new(ConfiguredVisibilityPolicy::new(
            files_config.show_hidden(),
            files_config.exclusions().to_vec(),
        ));
        let mut model = match &vcs {
            Some(vcs) => FilesModel::for_workspace_with_visibility(
                project.files_root().to_path_buf(),
                vcs.workspace().root().to_path_buf(),
                visibility,
                files_config.show_ignored(),
            )?,
            None => FilesModel::with_visibility_policy(
                project.files_root().to_path_buf(),
                visibility,
                files_config.show_ignored(),
            )?,
        };
        model.load_directory(Path::new(""))?;
        if vcs.as_ref().and_then(VcsRefresh::jujutsu_mode) == Some(JujutsuMode::Passive) {
            model.mark_status_stale();
        }
        let configured_vcs_missing = backend != VcsBackendSelection::Auto && vcs.is_none();

        let mut runtime = Self {
            model,
            root: project.files_root().to_path_buf(),
            background_active: true,
            vcs,
            refresh_policy,
            status_refresh_interval: match refresh_policy.git() {
                GitCadence::Manual => Duration::ZERO,
                GitCadence::Adaptive { minimum, .. } => minimum,
            },
            next_status_refresh: None,
            last_status_fingerprint: None,
            expanded: BTreeSet::new(),
            desired_expanded: BTreeSet::new(),
            visible_rows: Vec::new(),
            viewport_offset: 0,
            viewport_height: usize::MAX,
            viewport_y: 0,
            focused_pane: FilesPane::Tree,
            changed_selected: None,
            changed_offset: 0,
            changed_y: 0,
            changed_height: 0,
            status_seen: false,
            filesystem_generation: 0,
            filesystem_applied_generation: 0,
            filesystem_running: None,
            filesystem_reload_running: false,
            filesystem_expansions_running: BTreeSet::new(),
            pending_expansions: BTreeSet::new(),
            reload_pending: false,
            filesystem_panic_retried: false,
            filesystem_notice: None,
            search_page_notice: None,
            search_projection_notice: None,
            pane_input_notice: None,
            backend_notice: configured_vcs_missing.then(|| {
                "configured VCS backend was not found; showing the filesystem only".to_owned()
            }),
            display_mode: ui.display_mode(),
            colored_icons: ui.colored_icons(),
            show_ignored: files_config.show_ignored(),
            search_ignored: files_config.search_ignored(),
            search: None,
        };
        runtime.rebuild_visible_rows();
        Ok(runtime)
    }

    pub fn render(&mut self, area: Rect, buffer: &mut Buffer) {
        let has_notice = self.search_page_notice.is_some()
            || self.search_projection_notice.is_some()
            || self.filesystem_notice.is_some()
            || self.backend_notice.is_some()
            || self.model.failure_notice().is_some()
            || self.model.status_is_stale();
        let search_visible = self
            .search
            .as_ref()
            .is_some_and(|search| search.editing || search.active());
        let notice_height = usize::from(has_notice);
        let search_height = usize::from(search_visible);
        let content_height = usize::from(area.height)
            .saturating_sub(search_height)
            .saturating_sub(notice_height);
        let divider_height = if search_visible {
            0
        } else {
            usize::from(content_height >= 3)
        };
        let split_height = content_height.saturating_sub(divider_height);
        let changed_height = if divider_height == 0 {
            0
        } else {
            split_height / 2
        };
        let tree_height = split_height.saturating_sub(changed_height);
        // The project header consumes the first tree row inside FilesView;
        // subtract it here so selection math and pointer hit-testing share
        // the same viewport the renderer actually fills.
        let header_height = usize::from(self.root.file_name().is_some());
        let tree_height = tree_height.saturating_sub(header_height);
        self.viewport_y = area
            .y
            .saturating_add(search_height as u16)
            .saturating_add(header_height as u16);
        self.viewport_height = tree_height;
        self.changed_y = self
            .viewport_y
            .saturating_add(tree_height as u16)
            .saturating_add(divider_height as u16);
        self.changed_height = changed_height;
        self.ensure_selection_visible();
        if changed_height != 0 {
            self.ensure_changed_selection_visible();
        }
        let end = self
            .viewport_offset
            .saturating_add(self.viewport_height)
            .min(self.visible_rows.len());
        let rows = &self.visible_rows[self.viewport_offset.min(end)..end];
        let stale = self.model.status_is_stale();
        let runtime_notice = self
            .pane_input_notice
            .as_deref()
            .or(self.search_page_notice.as_deref())
            .or(self.search_projection_notice.as_deref())
            .or(self.filesystem_notice.as_deref())
            .or(self.backend_notice.as_deref());
        // Staleness alone is conveyed by the divider hint (`passive`); the
        // footer is reserved for real failure/runtime notices.
        let notice = match (runtime_notice, self.model.failure_notice(), stale) {
            (Some(message), _, true) => Some(("Files / VCS stale", message)),
            (Some(message), _, false) => Some(("Files", message)),
            (None, Some(message), true) => Some(("VCS stale", message)),
            (None, Some(message), false) => Some(("VCS", message)),
            (None, None, _) => None,
        };
        let search_display = self.search.as_ref().and_then(|search| {
            (search.editing || search.active()).then_some(FileSearchDisplay {
                query: &search.query,
                editing: search.editing,
                matches: search.matches.len(),
                scanning: search.scanning(),
                truncated: search.truncated,
                skipped_directories: search.skipped_directories,
            })
        });
        let (tree, selected, expanded) = self
            .search
            .as_ref()
            .filter(|search| search.active())
            .map_or_else(
                || {
                    (
                        self.model.tree(),
                        self.model.tree().selection(),
                        &self.expanded,
                    )
                },
                |search| {
                    (
                        &search.index,
                        search.index.selection(),
                        &search.expanded_context,
                    )
                },
            );
        let header = self
            .root
            .file_name()
            .map(|name| sanitize_terminal_text(&name.to_string_lossy()).into_owned());
        let mut view = FilesView::new(tree, rows, selected, None)
            .with_expanded(expanded)
            .with_display_mode(self.display_mode)
            .with_icon_colors(self.colored_icons);
        if let Some(header) = header.as_deref() {
            view = view.with_header(header);
        }
        if let Some(search) = search_display {
            view = view.with_search(search);
        }
        view.render(
            Rect::new(
                area.x,
                area.y,
                area.width,
                (search_height + tree_height + usize::from(header.is_some())) as u16,
            ),
            buffer,
        );
        if changed_height != 0 {
            let files = self.model.tree().changed_files();
            render_changed_divider(
                files.len(),
                self.model.diff_stats(),
                self.focused_pane == FilesPane::Changed,
                self.display_mode,
                self.refresh_hint(),
                Rect::new(area.x, self.changed_y.saturating_sub(1), area.width, 1),
                buffer,
            );
            let offset = self.changed_offset.min(files.len());
            let end = offset.saturating_add(changed_height).min(files.len());
            let selected = (self.focused_pane == FilesPane::Changed)
                .then_some(self.changed_selected.as_deref())
                .flatten();
            render_changed_pane(
                ChangedPane {
                    rows: &files[offset..end],
                    selected,
                    empty_message: Some(self.changed_empty_message()),
                },
                Rect::new(area.x, self.changed_y, area.width, changed_height as u16),
                buffer,
            );
        }
        if let Some((label, message)) = notice {
            render_notice(
                label,
                message,
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

    /// Right-aligned divider hint: whether VCS data refreshes on its own.
    const fn refresh_hint(&self) -> Option<&'static str> {
        match &self.vcs {
            Some(VcsRefresh::Git { .. }) => Some(match self.refresh_policy.git() {
                GitCadence::Adaptive { .. } => "live",
                GitCadence::Manual => "manual",
            }),
            Some(VcsRefresh::Jujutsu { service, .. }) => Some(match service.mode() {
                JujutsuMode::Passive => "passive",
                JujutsuMode::Fresh => "manual",
            }),
            None => None,
        }
    }

    /// Flips ignored-file visibility, preserving tree state, then re-enumerates.
    fn toggle_ignored_files(&mut self, workers: &mut WorkerRuntime) -> bool {
        self.show_ignored = !self.show_ignored;
        let selected = self.model.tree().selection().map(Path::to_path_buf);
        let mut tree = self.model.tree().with_show_ignored(self.show_ignored);
        tree.sync_status_overlay_from(self.model.tree());
        tree.restore_selection_from(selected.as_deref());
        self.model.replace_tree(tree);
        self.clear_search_query(false);
        self.request_reload(workers)
    }

    pub(crate) fn handle_intent(&mut self, intent: &Intent, workers: &mut WorkerRuntime) -> bool {
        match intent {
            Intent::SelectPrevious => self.move_focused_selection(-1),
            Intent::SelectNext => self.move_focused_selection(1),
            Intent::SelectFirst => self.select_focused_index(0),
            Intent::SelectLast => self.select_focused_last(),
            Intent::ExpandOrDescend => self.expand_or_descend(workers),
            Intent::CollapseOrAscend => self.collapse_or_ascend(),
            Intent::ToggleSelected => self.toggle_selected(workers),
            Intent::Refresh => self.request_reload(workers),
            Intent::ToggleIgnoredFiles => self.toggle_ignored_files(workers),
            Intent::SwitchFilesPane => self.toggle_pane_focus(),
            Intent::BeginFileSearch => self.begin_search(workers),
            Intent::FileSearchInput(value) => self.append_search_query(value, workers),
            Intent::FileSearchBackspace => self.backspace_search_query(workers),
            Intent::FileSearchClear => self.clear_search_query(true),
            Intent::FileSearchCommit => self.commit_search(),
            Intent::FileSearchCancel => self.clear_search_query(false),
            Intent::Pointer { row, action, .. } => match action {
                PointerAction::Select => self.select_viewport_row(*row),
                PointerAction::Toggle => self.toggle_viewport_row(*row, workers),
            },
            Intent::Scroll(delta) => self.move_focused_selection(isize::from(*delta)),
            Intent::Resize => true,
            Intent::Quit | Intent::SwitchView(_) | Intent::NextView | Intent::PreviousView => false,
        }
    }

    pub(crate) fn search_editing(&self) -> bool {
        self.search.as_ref().is_some_and(|search| search.editing)
    }

    pub(crate) fn search_query(&self) -> &str {
        self.search
            .as_ref()
            .map_or("", |search| search.query.as_str())
    }

    pub(crate) fn has_search_filter(&self) -> bool {
        self.search.as_ref().is_some_and(FileSearchState::active)
    }

    pub(crate) const fn finish_search_editing(&mut self) -> bool {
        let Some(search) = &mut self.search else {
            return false;
        };
        std::mem::replace(&mut search.editing, false)
    }

    fn begin_search(&mut self, workers: &mut WorkerRuntime) -> bool {
        self.focused_pane = FilesPane::Tree;
        self.search_page_notice = None;
        self.search_projection_notice = None;
        let changed = if let Some(search) = &mut self.search {
            !std::mem::replace(&mut search.editing, true)
        } else {
            let tree = self
                .model
                .tree()
                .with_show_ignored(self.show_ignored || self.search_ignored);
            let index = tree.search_index(MAX_INDEX_ENTRIES);
            self.search = Some(FileSearchState::new(index));
            true
        };
        self.start_next_search_page(workers);
        changed
    }

    fn append_search_query(&mut self, value: &str, workers: &mut WorkerRuntime) -> bool {
        let Some(search) = &mut self.search else {
            return false;
        };
        let prior_len = search.query.len();
        for character in value.chars().filter(|character| !character.is_control()) {
            if search.query.len().saturating_add(character.len_utf8()) > MAX_QUERY_BYTES {
                break;
            }
            search.query.push(character);
        }
        if search.query.len() == prior_len {
            return false;
        }
        self.queue_search_projection(workers, true);
        self.start_next_search_page(workers);
        true
    }

    fn backspace_search_query(&mut self, workers: &mut WorkerRuntime) -> bool {
        let Some(search) = &mut self.search else {
            return false;
        };
        if search.query.pop().is_none() {
            return false;
        }
        if search.query.is_empty() {
            self.clear_search_projection();
            self.rebuild_visible_rows();
        } else {
            self.queue_search_projection(workers, true);
        }
        true
    }

    fn clear_search_query(&mut self, keep_editing: bool) -> bool {
        let Some(search) = &mut self.search else {
            return false;
        };
        let changed = !search.query.is_empty() || search.editing != keep_editing;
        search.query.clear();
        search.editing = keep_editing;
        if changed {
            self.clear_search_projection();
            self.rebuild_visible_rows();
        }
        changed
    }

    const fn commit_search(&mut self) -> bool {
        self.finish_search_editing()
    }

    #[cfg(test)]
    fn handle_event(
        &mut self,
        event: crossterm::event::Event,
        workers: &mut WorkerRuntime,
    ) -> EventOutcome {
        use crate::input::{InputMode, map_event};
        use crate::model::UiGeometry;

        let geometry = UiGeometry::new(
            Rect::default(),
            Rect::default(),
            Rect::new(0, 0, u16::MAX, u16::MAX),
        );
        let Some(intent) = map_event(event, InputMode::Normal, &geometry) else {
            return EventOutcome::default();
        };
        if intent == Intent::Quit {
            return EventOutcome {
                redraw: false,
                quit: true,
            };
        }
        EventOutcome {
            redraw: self.handle_intent(&intent, workers),
            quit: false,
        }
    }

    #[must_use]
    pub(crate) fn selection(&self) -> Option<&Path> {
        self.active_tree().selection()
    }

    fn active_tree(&self) -> &FilesTree {
        self.search
            .as_ref()
            .filter(|search| search.active())
            .map_or_else(|| self.model.tree(), |search| &search.index)
    }

    pub(crate) fn selected_file_reference(&self) -> Option<Result<String, &'static str>> {
        if self.focused_pane == FilesPane::Changed {
            return self.changed_file_reference();
        }
        let tree = self.active_tree();
        let path = tree.selection()?;
        let node = tree.node(path)?;
        if node.kind() != TreeNodeKind::File {
            return None;
        }
        let Some(path) = path.to_str() else {
            return Some(Err("selected file path is not valid UTF-8"));
        };
        if path.is_empty()
            || path.len().saturating_add(2) > MAX_FILE_REFERENCE_BYTES
            || path.chars().any(char::is_control)
        {
            return Some(Err("selected file path cannot be inserted safely"));
        }
        let mut reference = String::with_capacity(path.len().saturating_add(2));
        reference.push('@');
        reference.push_str(path);
        reference.push(' ');
        Some(Ok(reference))
    }

    pub(crate) fn set_pane_input_notice(&mut self, notice: Option<String>) -> bool {
        if self.pane_input_notice == notice {
            return false;
        }
        self.pane_input_notice = notice;
        true
    }

    #[must_use]
    pub(crate) const fn scroll(&self) -> usize {
        self.viewport_offset
    }

    #[must_use]
    pub(crate) const fn generations(&self) -> (u64, u64) {
        (
            self.filesystem_generation,
            self.filesystem_applied_generation,
        )
    }

    fn navigable_len(&self) -> usize {
        self.visible_rows
            .iter()
            .filter(|path| self.path_is_navigable(path))
            .count()
    }

    fn navigable_path(&self, index: usize) -> Option<PathBuf> {
        self.visible_rows
            .iter()
            .filter(|path| self.path_is_navigable(path))
            .nth(index)
            .cloned()
    }

    fn path_is_navigable(&self, path: &Path) -> bool {
        self.search
            .as_ref()
            .filter(|search| search.active())
            .is_none_or(|search| search.matches.contains(path))
    }

    fn move_selection(&mut self, delta: isize) -> bool {
        let current = self.selection().and_then(|selected| {
            self.visible_rows
                .iter()
                .filter(|path| self.path_is_navigable(path))
                .position(|path| path == selected)
        });
        let Some(current) = current else {
            return self.select_navigable_index(0);
        };
        let last = self.navigable_len().saturating_sub(1);
        let next = if delta.is_negative() {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current.saturating_add(delta as usize).min(last)
        };
        self.select_navigable_index(next)
    }

    fn select_navigable_index(&mut self, index: usize) -> bool {
        let Some(path) = self.navigable_path(index) else {
            return false;
        };
        self.select_path(&path)
    }

    fn select_path(&mut self, path: &Path) -> bool {
        if self.selection() == Some(path) {
            return false;
        }
        let selected = if let Some(search) = self.search.as_mut().filter(|search| search.active()) {
            search.index.select(path)
        } else {
            self.model.select(path)
        };
        if selected {
            self.ensure_selection_visible();
        }
        selected
    }

    fn select_viewport_row(&mut self, terminal_row: u16) -> bool {
        if let Some(index) = self.changed_row_index(terminal_row) {
            self.focused_pane = FilesPane::Changed;
            return self.select_changed_index(index);
        }
        let Some(index) = self.tree_viewport_index(terminal_row) else {
            return false;
        };
        let Some(path) = self.visible_rows.get(index).cloned() else {
            return false;
        };
        if !self.path_is_navigable(&path) {
            return false;
        }
        self.focused_pane = FilesPane::Tree;
        self.select_path(&path)
    }

    fn toggle_viewport_row(&mut self, terminal_row: u16, workers: &mut WorkerRuntime) -> bool {
        if self.changed_row_index(terminal_row).is_some() {
            return false;
        }
        let Some(index) = self.tree_viewport_index(terminal_row) else {
            return false;
        };
        let Some(path) = self.visible_rows.get(index).cloned() else {
            return false;
        };
        if !self.path_is_navigable(&path) {
            return false;
        }
        let selection_changed = self.select_path(&path);
        self.toggle_selected(workers) || selection_changed
    }

    fn tree_viewport_index(&self, terminal_row: u16) -> Option<usize> {
        let row = terminal_row.checked_sub(self.viewport_y).map(usize::from)?;
        if row >= self.viewport_height {
            return None;
        }
        let index = self.viewport_offset.saturating_add(row);
        (index < self.visible_rows.len()).then_some(index)
    }

    fn changed_row_index(&self, terminal_row: u16) -> Option<usize> {
        if self.changed_height == 0 {
            return None;
        }
        let row = usize::from(terminal_row.checked_sub(self.changed_y)?);
        if row >= self.changed_height {
            return None;
        }
        let index = self.changed_offset.saturating_add(row);
        (index < self.model.tree().changed_files().len()).then_some(index)
    }

    fn expand_or_descend(&mut self, workers: &mut WorkerRuntime) -> bool {
        if self.focused_pane == FilesPane::Changed {
            return false;
        }
        if self.has_search_filter() {
            return false;
        }
        let Some(path) = self.model.tree().selection().map(Path::to_path_buf) else {
            return false;
        };
        if self.expanded.contains(&path) {
            let Some(child) = self
                .model
                .tree()
                .children(&path)
                .first()
                .map(|node| node.path().to_path_buf())
            else {
                return false;
            };
            let selected = self.model.select(&child);
            if selected {
                self.ensure_selection_visible();
            }
            return selected;
        }
        self.request_expansion(path, workers)
    }

    fn toggle_selected(&mut self, workers: &mut WorkerRuntime) -> bool {
        if self.focused_pane == FilesPane::Changed {
            return false;
        }
        if self.has_search_filter() {
            return false;
        }
        let Some(path) = self.model.tree().selection().map(Path::to_path_buf) else {
            return false;
        };
        if self.desired_expanded.remove(&path) {
            if self.filesystem_running.is_some() {
                self.invalidate_filesystem();
            }
            self.pending_expansions.remove(&path);
            if self.expanded.remove(&path) {
                self.rebuild_visible_rows();
            }
            return true;
        }
        self.request_expansion(path, workers)
    }

    fn collapse_or_ascend(&mut self) -> bool {
        if self.focused_pane == FilesPane::Changed {
            return false;
        }
        if self.has_search_filter() {
            return false;
        }
        let Some(path) = self.model.tree().selection().map(Path::to_path_buf) else {
            return false;
        };
        if self.desired_expanded.remove(&path) {
            if self.filesystem_running.is_some() {
                self.invalidate_filesystem();
            }
            self.pending_expansions.remove(&path);
            if self.expanded.remove(&path) {
                self.rebuild_visible_rows();
            }
            return true;
        }
        let Some(parent) = self
            .model
            .tree()
            .display_parent_of(&path)
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(Path::to_path_buf)
        else {
            return false;
        };
        let selected = self.model.select(&parent);
        if selected {
            self.ensure_selection_visible();
        }
        selected
    }

    fn request_expansion(&mut self, path: PathBuf, workers: &mut WorkerRuntime) -> bool {
        if self.model.tree().node(&path).map(|node| node.kind()) != Some(TreeNodeKind::Directory) {
            return false;
        }
        let desired = self.desired_expanded.insert(path.clone());
        if desired {
            self.invalidate_filesystem();
            self.pending_expansions.insert(path);
            self.start_next_filesystem(workers);
        }
        desired
    }

    fn request_reload(&mut self, workers: &mut WorkerRuntime) -> bool {
        self.invalidate_filesystem();
        self.reload_pending = true;
        self.start_next_filesystem(workers);
        self.request_vcs_refresh(workers);
        true
    }

    fn restart_search_scan(&mut self) {
        let Some(search) = &mut self.search else {
            return;
        };
        if let Some(cancel) = search.page_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        search.scan_epoch = search.scan_epoch.saturating_add(1);
        search.running = None;
        search.running_directories.clear();
        search.pending_directories.clear();
        search.pending_directories.push_back(PathBuf::new());
        search.scanned_entries = search
            .searchable
            .values()
            .filter(|indexed| indexed.kind == TreeNodeKind::Virtual)
            .count();
        search.examined_entries = 0;
        search.skipped_directories = 0;
        search.complete = false;
        search.truncated = false;
    }

    fn start_next_search_page(&mut self, workers: &mut WorkerRuntime) {
        if !self.background_active {
            return;
        }
        let Some(search) = &mut self.search else {
            return;
        };
        if search.running.is_some() || !search.should_scan() || search.complete {
            return;
        }
        if search.scanned_entries >= MAX_INDEX_ENTRIES
            || search.examined_entries >= MAX_EXAMINED_ENTRIES
        {
            search.truncated = true;
            search.complete = true;
            search.pending_directories.clear();
            return;
        }
        if search.pending_directories.is_empty() {
            search.complete = true;
            return;
        }

        let directories = search
            .pending_directories
            .iter()
            .take(DIRECTORIES_PER_PAGE)
            .cloned()
            .collect::<Vec<_>>();
        let index = search.index.clone();
        let loader = index.directory_loader();
        let searchable = Arc::clone(&search.searchable);
        let remaining_entries = MAX_INDEX_ENTRIES.saturating_sub(search.scanned_entries);
        let remaining_examined = MAX_EXAMINED_ENTRIES.saturating_sub(search.examined_entries);
        let generation = search.job_generation.saturating_add(1);
        let epoch = search.scan_epoch;
        let page_cancel = Arc::new(AtomicBool::new(false));
        let job_cancel = Arc::clone(&page_cancel);
        let job_directories = directories.clone();
        let job = Job::new(
            JobKey::new(JobKind::FileSearch, &self.root),
            generation,
            Priority::Low,
            move |cancelled| {
                Box::new(RuntimeMessage::FileSearch {
                    generation,
                    epoch,
                    page: prepare_page(
                        &loader,
                        SearchPageRequest {
                            index,
                            searchable,
                            directories: job_directories,
                            remaining_entries,
                            remaining_examined,
                        },
                        cancelled,
                        &job_cancel,
                    ),
                })
            },
        );
        match workers.submit(job) {
            SubmitStatus::Queued | SubmitStatus::Coalesced => {
                for _ in 0..directories.len() {
                    search.pending_directories.pop_front();
                }
                search.running = Some((generation, epoch));
                search.running_directories = directories;
                search.page_cancel = Some(page_cancel);
                search.job_generation = generation;
                self.search_page_notice = None;
            }
            SubmitStatus::RejectedStale
            | SubmitStatus::Backpressure
            | SubmitStatus::ShuttingDown => {
                self.search_page_notice =
                    Some("background file search queue is unavailable".to_owned());
            }
        }
    }

    fn complete_search_page(
        &mut self,
        generation: u64,
        epoch: u64,
        page: PreparedSearchPage,
        workers: &mut WorkerRuntime,
    ) -> bool {
        let Some(search) = &mut self.search else {
            return false;
        };
        if search.running != Some((generation, epoch)) {
            return false;
        }
        search.running = None;
        search.page_cancel = None;
        let running_directories = std::mem::take(&mut search.running_directories);
        if epoch != search.scan_epoch {
            self.start_next_search_page(workers);
            return false;
        }
        if page.cancelled {
            for directory in running_directories.into_iter().rev() {
                search.pending_directories.push_front(directory);
            }
            return false;
        }

        search.index = page.index;
        search.searchable = page.searchable;
        search
            .pending_directories
            .extend(page.discovered_directories);
        search.scanned_entries = search.scanned_entries.saturating_add(page.scanned_entries);
        search.examined_entries = search
            .examined_entries
            .saturating_add(page.examined_entries);
        search.skipped_directories = search
            .skipped_directories
            .saturating_add(page.skipped_directories);
        if page.truncated {
            search.truncated = true;
            search.complete = true;
            search.pending_directories.clear();
        } else if search.pending_directories.is_empty() {
            search.complete = true;
        }
        let changed = search.editing || search.active();
        let project = search.active();
        self.search_page_notice = None;
        if project {
            self.queue_search_projection(workers, false);
        }
        self.start_next_search_page(workers);
        changed
    }

    fn queue_search_projection(&mut self, workers: &mut WorkerRuntime, clear_rows: bool) {
        let Some(search) = &mut self.search else {
            return;
        };
        if !search.active() {
            return;
        }
        if let Some(cancel) = search.projection_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        search.projection_requested = search.projection_requested.saturating_add(1);
        if clear_rows {
            search.matches.clear();
            search.expanded_context.clear();
            self.visible_rows.clear();
            self.viewport_offset = 0;
        }
        self.start_next_search_projection(workers);
    }

    fn clear_search_projection(&mut self) {
        let Some(search) = &mut self.search else {
            return;
        };
        if let Some(cancel) = search.projection_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        search.projection_requested = search.projection_requested.saturating_add(1);
        search.projection_applied = search.projection_requested;
        search.matches.clear();
        search.expanded_context.clear();
    }

    fn start_next_search_projection(&mut self, workers: &mut WorkerRuntime) {
        if !self.background_active {
            return;
        }
        let Some(search) = &mut self.search else {
            return;
        };
        if !search.active() || search.projection_submitted >= search.projection_requested {
            return;
        }
        let generation = search.projection_requested;
        let index = search.index.clone();
        let searchable = Arc::clone(&search.searchable);
        let query = search.query.clone();
        let projection_cancel = Arc::new(AtomicBool::new(false));
        let job_cancel = Arc::clone(&projection_cancel);
        let job = Job::new(
            JobKey::new(JobKind::FileSearchProjection, &self.root),
            generation,
            Priority::Low,
            move |cancelled| {
                Box::new(RuntimeMessage::FileSearchProjection {
                    generation,
                    projection: project(&index, &searchable, &query, cancelled, &job_cancel),
                })
            },
        );
        match workers.submit(job) {
            SubmitStatus::Queued | SubmitStatus::Coalesced => {
                search.projection_submitted = generation;
                search.projection_cancel = Some(projection_cancel);
                self.search_projection_notice = None;
            }
            SubmitStatus::RejectedStale
            | SubmitStatus::Backpressure
            | SubmitStatus::ShuttingDown => {
                self.search_projection_notice =
                    Some("background file search projection queue is unavailable".to_owned());
            }
        }
    }

    fn complete_search_projection(
        &mut self,
        generation: u64,
        projection: Option<SearchProjection>,
    ) -> bool {
        let Some(search) = &mut self.search else {
            return false;
        };
        if generation != search.projection_requested || !search.active() {
            return false;
        }
        search.projection_cancel = None;
        let Some(projection) = projection else {
            return false;
        };
        search.projection_applied = generation;
        if search
            .index
            .selection()
            .is_none_or(|selected| !projection.matches.contains(selected))
            && let Some(first) = projection
                .rows
                .iter()
                .find(|path| projection.matches.contains(*path))
        {
            search.index.select(first);
        }
        search.matches = projection.matches;
        search.expanded_context = projection.expanded_context;
        self.visible_rows = projection.rows;
        self.search_projection_notice = None;
        self.ensure_selection_visible();
        true
    }

    fn start_next_filesystem(&mut self, workers: &mut WorkerRuntime) {
        if !self.background_active {
            return;
        }
        if self.filesystem_running.is_some() {
            return;
        }
        let reload = std::mem::take(&mut self.reload_pending);
        let expansions = std::mem::take(&mut self.pending_expansions);
        let mut directories = Vec::with_capacity(
            usize::from(reload)
                .saturating_add(self.expanded.len())
                .saturating_add(expansions.len()),
        );
        if reload {
            directories.push(PathBuf::new());
            directories.extend(self.expanded.iter().cloned());
        }
        directories.extend(expansions.iter().cloned());
        directories.sort_unstable_by(|left, right| {
            left.components()
                .count()
                .cmp(&right.components().count())
                .then_with(|| left.cmp(right))
        });
        directories.dedup();
        if directories.is_empty() {
            return;
        }

        let generation = self.filesystem_generation;
        let result_expansions = expansions.clone();
        let loader = self.model.tree().directory_loader();
        let job = Job::new(
            JobKey::new(JobKind::Filesystem, &self.root),
            generation,
            Priority::High,
            move |cancelled| {
                let result = directories
                    .into_iter()
                    .take_while(|_| !cancelled.load(std::sync::atomic::Ordering::Relaxed))
                    .map(|directory| {
                        let result = loader.load(directory.clone());
                        (directory, result)
                    })
                    .collect();
                Box::new(RuntimeMessage::Filesystem {
                    generation,
                    expansions: result_expansions,
                    result,
                })
            },
        );
        match workers.submit(job) {
            SubmitStatus::Queued | SubmitStatus::Coalesced => {
                self.filesystem_running = Some(generation);
                self.filesystem_reload_running = reload;
                self.filesystem_expansions_running = expansions;
            }
            SubmitStatus::RejectedStale
            | SubmitStatus::Backpressure
            | SubmitStatus::ShuttingDown => {
                self.reload_pending |= reload;
                self.pending_expansions.extend(expansions);
                self.filesystem_notice =
                    Some("background filesystem queue is unavailable".to_owned());
            }
        }
    }

    fn complete_filesystem(
        &mut self,
        generation: u64,
        expansions: BTreeSet<PathBuf>,
        result: Vec<DirectoryLoadResult>,
        workers: &mut WorkerRuntime,
    ) -> bool {
        self.filesystem_expansions_running.clear();
        if self.filesystem_running != Some(generation) {
            return false;
        }
        self.filesystem_running = None;
        let reload = std::mem::take(&mut self.filesystem_reload_running);
        if generation != self.filesystem_generation {
            self.reload_pending |= reload;
            self.pending_expansions.extend(
                expansions
                    .into_iter()
                    .filter(|path| self.desired_expanded.contains(path)),
            );
            if !self.reload_pending && self.pending_expansions.is_empty() {
                self.filesystem_applied_generation = self.filesystem_generation;
            }
            self.start_next_filesystem(workers);
            return false;
        }
        self.filesystem_panic_retried = false;
        self.filesystem_applied_generation = generation;

        let mut loaded = BTreeSet::new();
        let mut notice = None;
        let mut changed = false;
        for (directory, result) in result {
            if !directory.as_os_str().is_empty()
                && self.model.tree().node(&directory).map(|node| node.kind())
                    != Some(TreeNodeKind::Directory)
            {
                continue;
            }
            match result {
                Ok(snapshot) => {
                    self.model.apply_directory(snapshot);
                    loaded.insert(directory);
                    changed = true;
                }
                Err(error)
                    if error.kind() == io::ErrorKind::NotFound
                        && !directory.as_os_str().is_empty()
                        && self.model.tree().node(&directory).is_none() => {}
                Err(error) => {
                    notice.get_or_insert_with(|| {
                        format!("cannot load {}: {error}", directory.display())
                    });
                }
            }
        }
        for path in expansions {
            let loaded_directory = loaded.contains(&path)
                && self.model.tree().node(&path).map(|node| node.kind())
                    == Some(TreeNodeKind::Directory);
            if self.desired_expanded.contains(&path) && loaded_directory {
                self.expanded.insert(path);
            } else if !loaded_directory {
                self.desired_expanded.remove(&path);
            }
        }
        self.filesystem_notice = notice;
        if changed {
            if reload {
                self.restart_search_scan();
            }
            self.rebuild_visible_rows();
        }
        self.start_next_filesystem(workers);
        self.start_next_search_page(workers);
        true
    }

    fn request_vcs_refresh(&mut self, workers: &mut WorkerRuntime) -> bool {
        if self.vcs.is_none() {
            return false;
        }
        self.next_status_refresh = None;
        self.model.request_refresh();
        self.start_next_vcs_refresh(workers);
        true
    }

    const fn invalidate_filesystem(&mut self) {
        self.filesystem_generation = self.filesystem_generation.saturating_add(1);
    }

    fn start_next_vcs_refresh(&mut self, workers: &mut WorkerRuntime) {
        if !self.background_active {
            return;
        }
        let Some(generation) = self.model.begin_refresh() else {
            return;
        };
        let Some(vcs) = self.vcs.clone() else {
            self.model.cancel_refresh_start(generation);
            return;
        };
        let input = self.model.status_merge_input();
        let key = JobKey::new(JobKind::Vcs, vcs.workspace().root());
        let job = Job::new(key, generation, Priority::High, move |cancelled| {
            let snapshot = vcs.refresh_status_cancellable(cancelled);
            let diff_stats = vcs.refresh_diff_stats_cancellable(cancelled).ok().flatten();
            Box::new(RuntimeMessage::Vcs(PreparedRefreshResult::prepare(
                generation, input, snapshot, diff_stats,
            )))
        });
        if !matches!(
            workers.submit(job),
            SubmitStatus::Queued | SubmitStatus::Coalesced
        ) {
            self.model.cancel_refresh_start(generation);
            self.filesystem_notice = Some("background VCS queue is unavailable".to_owned());
            self.schedule_next_status_refresh(Instant::now(), None, false);
        }
    }

    fn complete_vcs_refresh(
        &mut self,
        result: PreparedRefreshResult,
        workers: &mut WorkerRuntime,
    ) -> bool {
        self.complete_vcs_refresh_at(result, workers, Instant::now())
    }

    fn complete_vcs_refresh_at(
        &mut self,
        result: PreparedRefreshResult,
        workers: &mut WorkerRuntime,
        now: Instant,
    ) -> bool {
        let fingerprint = result.status_fingerprint();
        let changed = self.model.complete_prepared_refresh(result);
        self.schedule_next_status_refresh(now, fingerprint, changed);
        if changed {
            self.status_seen = true;
            self.restore_changed_selection();
            if let Some(search) = &mut self.search {
                search.index.sync_status_overlay_from(self.model.tree());
                search.sync_virtual_entries();
            }
            self.queue_search_projection(workers, false);
            self.rebuild_visible_rows();
        }
        self.start_next_vcs_refresh(workers);
        true
    }

    fn schedule_next_status_refresh(
        &mut self,
        now: Instant,
        fingerprint: Option<u64>,
        applied: bool,
    ) {
        if !self.background_active {
            self.next_status_refresh = None;
            return;
        }
        let Some(vcs) = self.vcs.as_ref() else {
            self.next_status_refresh = None;
            return;
        };
        let interval = if vcs.is_git() {
            match self.refresh_policy.git() {
                GitCadence::Manual => None,
                GitCadence::Adaptive { minimum, maximum } => {
                    if applied {
                        let changed = fingerprint
                            .is_some_and(|value| self.last_status_fingerprint != Some(value));
                        self.last_status_fingerprint = fingerprint;
                        self.status_refresh_interval = if changed {
                            minimum
                        } else {
                            self.status_refresh_interval
                                .saturating_mul(2)
                                .clamp(minimum, maximum)
                        };
                    } else if fingerprint.is_none() {
                        self.status_refresh_interval = self
                            .status_refresh_interval
                            .saturating_mul(2)
                            .clamp(minimum, maximum);
                    }
                    Some(self.status_refresh_interval)
                }
            }
        } else if vcs.jujutsu_mode() == Some(JujutsuMode::Passive) {
            self.refresh_policy.passive_jujutsu()
        } else {
            None
        };
        self.next_status_refresh = interval.and_then(|interval| now.checked_add(interval));
    }

    fn rebuild_visible_rows(&mut self) {
        if self.has_search_filter() {
            self.ensure_selection_visible();
            return;
        }
        self.expanded.retain(|path| {
            self.model.tree().node(path).map(|node| node.kind()) == Some(TreeNodeKind::Directory)
        });
        self.desired_expanded.retain(|path| {
            self.model.tree().node(path).map(|node| node.kind()) == Some(TreeNodeKind::Directory)
        });
        let mut rows = Vec::new();
        self.append_visible_children(Path::new(""), &mut rows);
        self.visible_rows = rows;
        self.restore_visible_selection();
        self.ensure_selection_visible();
    }

    fn append_visible_children(&self, directory: &Path, rows: &mut Vec<PathBuf>) {
        for node in self.model.tree().children(directory) {
            let path = node.path().to_path_buf();
            let descend = node.kind() == TreeNodeKind::Directory && self.expanded.contains(&path);
            rows.push(path.clone());
            if descend {
                self.append_visible_children(&path, rows);
            }
        }
    }

    fn restore_visible_selection(&mut self) {
        let Some(selected) = self.model.tree().selection().map(Path::to_path_buf) else {
            return;
        };
        if self.visible_rows.iter().any(|path| path == &selected) {
            return;
        }
        let mut candidate = self
            .model
            .tree()
            .display_parent_of(&selected)
            .map(Path::to_path_buf);
        while let Some(path) = candidate {
            if self.visible_rows.iter().any(|visible| visible == &path) {
                self.model.select(&path);
                return;
            }
            candidate = self
                .model
                .tree()
                .display_parent_of(&path)
                .map(Path::to_path_buf);
        }
        if let Some(first) = self.visible_rows.first().cloned() {
            self.model.select(&first);
        }
    }

    fn ensure_selection_visible(&mut self) {
        let Some(index) = self
            .selection()
            .and_then(|selected| self.visible_rows.iter().position(|path| path == selected))
        else {
            self.viewport_offset = 0;
            return;
        };
        if index < self.viewport_offset {
            self.viewport_offset = index;
        } else if self.viewport_height != 0
            && index >= self.viewport_offset.saturating_add(self.viewport_height)
        {
            self.viewport_offset = index.saturating_add(1).saturating_sub(self.viewport_height);
        }
        self.viewport_offset = self
            .viewport_offset
            .min(self.visible_rows.len().saturating_sub(1));
    }

    fn toggle_pane_focus(&mut self) -> bool {
        if self.search_editing() || self.has_search_filter() || self.changed_height == 0 {
            return false;
        }
        self.focused_pane = match self.focused_pane {
            FilesPane::Tree => FilesPane::Changed,
            FilesPane::Changed => FilesPane::Tree,
        };
        true
    }

    fn move_focused_selection(&mut self, delta: isize) -> bool {
        match self.focused_pane {
            FilesPane::Tree => self.move_selection(delta),
            FilesPane::Changed => self.move_changed_selection(delta),
        }
    }

    fn select_focused_index(&mut self, index: usize) -> bool {
        match self.focused_pane {
            FilesPane::Tree => self.select_navigable_index(index),
            FilesPane::Changed => self.select_changed_index(index),
        }
    }

    fn select_focused_last(&mut self) -> bool {
        match self.focused_pane {
            FilesPane::Tree => self.select_navigable_index(self.navigable_len().saturating_sub(1)),
            FilesPane::Changed => {
                self.select_changed_index(self.model.tree().changed_files().len().saturating_sub(1))
            }
        }
    }

    fn changed_path_at(&self, index: usize) -> Option<&Path> {
        self.model
            .tree()
            .changed_files()
            .get(index)
            .map(ChangedFile::path)
    }

    fn select_changed_index(&mut self, index: usize) -> bool {
        let Some(path) = self.changed_path_at(index) else {
            return false;
        };
        if self.changed_selected.as_deref() == Some(path) {
            return false;
        }
        self.changed_selected = Some(path.to_path_buf());
        self.ensure_changed_selection_visible();
        true
    }

    fn move_changed_selection(&mut self, delta: isize) -> bool {
        let files = self.model.tree().changed_files();
        let Some(current) = self.changed_selected.as_ref().and_then(|selected| {
            files
                .iter()
                .position(|file| file.path() == selected.as_path())
        }) else {
            return self.select_changed_index(0);
        };
        let last = files.len().saturating_sub(1);
        let next = if delta.is_negative() {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current.saturating_add(delta as usize).min(last)
        };
        self.select_changed_index(next)
    }

    fn ensure_changed_selection_visible(&mut self) {
        let Some(selected) = self.changed_selected.clone() else {
            self.changed_offset = 0;
            return;
        };
        let files = self.model.tree().changed_files();
        let Some(index) = files
            .iter()
            .position(|file| file.path() == selected.as_path())
        else {
            self.changed_offset = 0;
            return;
        };
        if index < self.changed_offset {
            self.changed_offset = index;
        } else if self.changed_height != 0
            && index >= self.changed_offset.saturating_add(self.changed_height)
        {
            self.changed_offset = index.saturating_add(1).saturating_sub(self.changed_height);
        }
        self.changed_offset = self.changed_offset.min(files.len().saturating_sub(1));
    }

    fn restore_changed_selection(&mut self) {
        let files = self.model.tree().changed_files();
        let selected = self.changed_selected.take();
        let kept = selected
            .as_ref()
            .filter(|selected| files.iter().any(|file| file.path() == selected.as_path()));
        self.changed_selected = kept
            .cloned()
            .or_else(|| files.first().map(|file| file.path().to_path_buf()));
        self.ensure_changed_selection_visible();
    }

    fn changed_empty_message(&self) -> &'static str {
        if self.vcs.is_none()
            || self.backend_notice.is_some()
            || self.model.failure_notice().is_some()
            || self.model.status_is_stale()
        {
            "VCS status unavailable"
        } else if self.status_seen {
            "No modified files"
        } else {
            "Loading VCS status…"
        }
    }

    fn changed_file_reference(&self) -> Option<Result<String, &'static str>> {
        let path = self.changed_selected.as_ref()?;
        let file = self
            .model
            .tree()
            .changed_files()
            .iter()
            .find(|file| file.path() == path.as_path())?;
        if file.is_missing() {
            return Some(Err("modified file is missing"));
        }
        match std::fs::symlink_metadata(self.root.join(path)) {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => return Some(Err("selected path is not a regular file")),
            Err(_) => return Some(Err("modified file is missing")),
        }
        let Some(relative) = path.to_str() else {
            return Some(Err("selected file path is not valid UTF-8"));
        };
        if relative.is_empty()
            || relative.len().saturating_add(2) > MAX_FILE_REFERENCE_BYTES
            || relative.chars().any(char::is_control)
        {
            return Some(Err("selected file path cannot be inserted safely"));
        }
        let mut reference = String::with_capacity(relative.len().saturating_add(2));
        reference.push('@');
        reference.push_str(relative);
        reference.push(' ');
        Some(Ok(reference))
    }

    pub(crate) fn has_pending_work(&self) -> bool {
        self.filesystem_running.is_some()
            || self.model.refresh_is_running()
            || self
                .search
                .as_ref()
                .is_some_and(|search| search.running.is_some())
            || (self.background_active
                && (self.reload_pending
                    || !self.pending_expansions.is_empty()
                    || self.search.as_ref().is_some_and(|search| {
                        (search.should_scan() && !search.complete)
                            || search.projection_applied < search.projection_requested
                    })))
    }

    pub(crate) fn next_refresh_in(&self, now: Instant) -> Option<Duration> {
        self.background_active
            .then_some(self.next_status_refresh)
            .flatten()
            .map(|deadline| deadline.saturating_duration_since(now))
    }

    pub(crate) fn tick(&mut self, now: Instant, workers: &mut WorkerRuntime) -> bool {
        let Some(deadline) = self.next_status_refresh else {
            return false;
        };
        if !self.background_active || deadline > now {
            return false;
        }
        self.request_vcs_refresh(workers)
    }

    pub(crate) fn start_background(&mut self, workers: &mut WorkerRuntime) {
        self.background_active = true;
        self.request_reload(workers);
        self.retry_pending(workers);
    }

    #[must_use]
    pub fn jujutsu_mode(&self) -> Option<JujutsuMode> {
        self.vcs.as_ref().and_then(VcsRefresh::jujutsu_mode)
    }

    pub fn set_jujutsu_mode(&mut self, mode: JujutsuMode, workers: &mut WorkerRuntime) -> bool {
        let changed = self
            .vcs
            .as_mut()
            .is_some_and(|vcs| vcs.set_jujutsu_mode(mode));
        if !changed {
            return false;
        }
        if mode == JujutsuMode::Passive {
            self.model.mark_status_stale();
        }
        self.request_vcs_refresh(workers);
        true
    }

    pub(crate) fn pause_background(&mut self) {
        self.background_active = false;
        self.next_status_refresh = None;
        let Some(search) = &mut self.search else {
            return;
        };
        if let Some(cancel) = search.page_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        if search.running.take().is_some() {
            for directory in std::mem::take(&mut search.running_directories)
                .into_iter()
                .rev()
            {
                search.pending_directories.push_front(directory);
            }
            search.scan_epoch = search.scan_epoch.saturating_add(1);
        }
        if let Some(cancel) = search.projection_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
            search.projection_requested = search.projection_requested.saturating_add(1);
        }
    }

    pub(crate) fn complete_background(
        &mut self,
        message: RuntimeMessage,
        workers: &mut WorkerRuntime,
    ) -> bool {
        match message {
            RuntimeMessage::Filesystem {
                generation,
                expansions,
                result,
            } => self.complete_filesystem(generation, expansions, result, workers),
            RuntimeMessage::Vcs(result) => self.complete_vcs_refresh(result, workers),
            RuntimeMessage::FileSearch {
                generation,
                epoch,
                page,
            } => self.complete_search_page(generation, epoch, page, workers),
            RuntimeMessage::FileSearchProjection {
                generation,
                projection,
            } => self.complete_search_projection(generation, projection),
        }
    }

    pub(crate) fn retry_pending(&mut self, workers: &mut WorkerRuntime) {
        self.start_next_filesystem(workers);
        self.start_next_vcs_refresh(workers);
        self.start_next_search_page(workers);
        self.start_next_search_projection(workers);
    }

    pub(crate) fn fail_background(
        &mut self,
        kind: JobKind,
        generation: u64,
        workers: &mut WorkerRuntime,
    ) {
        match kind {
            JobKind::Filesystem if self.filesystem_running.is_some() => {
                self.filesystem_running = None;
                self.reload_pending |= std::mem::take(&mut self.filesystem_reload_running);
                self.pending_expansions.extend(
                    std::mem::take(&mut self.filesystem_expansions_running)
                        .into_iter()
                        .filter(|path| self.desired_expanded.contains(path)),
                );
                self.invalidate_filesystem();
                if !self.filesystem_panic_retried
                    && (self.reload_pending || !self.pending_expansions.is_empty())
                {
                    self.filesystem_panic_retried = true;
                    self.filesystem_notice =
                        Some("background filesystem worker stopped; retrying once".to_owned());
                } else {
                    self.reload_pending = false;
                    self.pending_expansions.clear();
                    self.filesystem_applied_generation = self.filesystem_generation;
                    self.filesystem_notice =
                        Some("background filesystem worker stopped unexpectedly".to_owned());
                }
            }
            JobKind::Vcs => {
                self.model.cancel_refresh_start(generation);
                self.model.request_refresh();
                self.filesystem_notice =
                    Some("background VCS worker stopped unexpectedly".to_owned());
            }
            JobKind::FileSearch => {
                if let Some(search) = &mut self.search
                    && let Some((running_generation, running_epoch)) = search.running
                    && running_generation == generation
                {
                    search.running = None;
                    search.running_directories.clear();
                    search.page_cancel = None;
                    if running_epoch == search.scan_epoch {
                        search.complete = true;
                    }
                    self.search_page_notice =
                        Some("background file search worker stopped unexpectedly".to_owned());
                }
            }
            JobKind::FileSearchProjection => {
                if let Some(search) = &mut self.search
                    && generation == search.projection_requested
                {
                    search.projection_cancel = None;
                    search.projection_applied = generation;
                    self.search_projection_notice = Some(
                        "background file search projection worker stopped unexpectedly".to_owned(),
                    );
                }
            }
            JobKind::Config
            | JobKind::Bootstrap
            | JobKind::ConversationDiscovery
            | JobKind::ConversationLive
            | JobKind::ConversationLaunch
            | JobKind::PaneInput
            | JobKind::Process
            | JobKind::Filesystem => {}
        }
        if kind != JobKind::Filesystem || self.filesystem_panic_retried {
            self.retry_pending(workers);
        }
    }
}

type DirectoryLoadResult = (PathBuf, io::Result<DirectorySnapshot>);

pub(crate) enum RuntimeMessage {
    Filesystem {
        generation: u64,
        expansions: BTreeSet<PathBuf>,
        result: Vec<DirectoryLoadResult>,
    },
    Vcs(PreparedRefreshResult),
    FileSearch {
        generation: u64,
        epoch: u64,
        page: PreparedSearchPage,
    },
    FileSearchProjection {
        generation: u64,
        projection: Option<SearchProjection>,
    },
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct EventOutcome {
    redraw: bool,
    quit: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilesRuntimeError {
    message: String,
}

impl FilesRuntimeError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub(crate) fn cancelled() -> Self {
        Self::new("Files bootstrap was cancelled")
    }
}

impl From<io::Error> for FilesRuntimeError {
    fn from(error: io::Error) -> Self {
        Self::new(error.to_string())
    }
}

impl fmt::Display for FilesRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for FilesRuntimeError {}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::io;
    #[cfg(unix)]
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use tempfile::TempDir;

    use super::{FileSearchState, FilesPane, FilesRuntime, RuntimeMessage, VcsRefresh};
    use crate::config::{DisplayMode, GitCadence, PluginConfig};
    use crate::files::PreparedRefreshResult;
    use crate::host::LaunchContext;
    use crate::intent::{Intent, PointerAction};
    use crate::vcs::git::GitService;
    use crate::vcs::jj::{JjService, JujutsuMode};
    use crate::vcs::{
        VcsBackendMetadata, VcsDiffStats, VcsEntryStatus, VcsStatusKind, VcsStatusSnapshot,
        VcsWorkspace,
    };
    use crate::worker::WorkerRuntime;

    fn runtime(temp: &TempDir) -> FilesRuntime {
        let context = LaunchContext::from_vars([(
            "HERDR_PLUGIN_CONTEXT_JSON",
            format!(
                r#"{{"workspace_id":"workspace","tab_id":"tab","pane_id":"pane","cwd":"{}"}}"#,
                temp.path().display()
            ),
        )])
        .expect("context");
        FilesRuntime::bootstrap(&context).expect("runtime")
    }

    fn press(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn workers() -> WorkerRuntime {
        WorkerRuntime::with_capacities(4, 2)
    }

    fn receive(workers: &mut WorkerRuntime) -> RuntimeMessage {
        workers
            .recv_timeout(Duration::from_secs(1))
            .expect("worker result")
            .downcast::<RuntimeMessage>()
            .map(|message| *message)
            .expect("Files runtime message")
    }

    #[test]
    fn configured_display_mode_reaches_the_files_renderer() {
        let project = TempDir::new().expect("project");
        fs::write(project.path().join("main.rs"), []).expect("Rust file");
        let config_dir = TempDir::new().expect("config");
        fs::write(
            config_dir.path().join("config.toml"),
            "[ui]\ndisplay_mode = \"unicode\"\n",
        )
        .expect("config file");
        let config = PluginConfig::load_from_dir(config_dir.path()).into_config();
        let context = LaunchContext::from_vars([(
            "HERDR_PLUGIN_CONTEXT_JSON",
            format!(
                r#"{{"workspace_id":"workspace","tab_id":"tab","pane_id":"pane","cwd":"{}"}}"#,
                project.path().display()
            ),
        )])
        .expect("context");
        let mut runtime = FilesRuntime::bootstrap_with_config(&context, &config).expect("runtime");
        let area = Rect::new(0, 0, 40, 2);
        let mut buffer = Buffer::empty(area);

        runtime.render(area, &mut buffer);

        let header = (0..area.width)
            .map(|column| buffer[(column, 0)].symbol())
            .collect::<String>();
        let project_label = project
            .path()
            .file_name()
            .expect("tempdir name")
            .to_string_lossy()
            .to_uppercase();
        assert!(header.starts_with(&project_label));
        let rendered = (0..area.width)
            .map(|column| buffer[(column, 1)].symbol())
            .collect::<String>();
        assert!(rendered.starts_with("  • main.rs"));
        assert_eq!(runtime.display_mode, DisplayMode::Unicode);
    }

    #[test]
    fn selected_file_reference_is_relative_to_the_project_root() {
        let project = TempDir::new().expect("project");
        fs::create_dir(project.path().join("src")).expect("src");
        fs::write(project.path().join("src/file.tmp"), []).expect("file");
        let mut runtime = runtime(&project);

        assert_eq!(runtime.selected_file_reference(), None);
        runtime
            .model
            .load_directory(Path::new("src"))
            .expect("load src");
        assert!(runtime.model.select(Path::new("src/file.tmp")));

        assert_eq!(
            runtime.selected_file_reference(),
            Some(Ok("@src/file.tmp ".to_owned()))
        );
    }

    #[cfg(unix)]
    #[test]
    fn selected_file_reference_rejects_terminal_control_characters() {
        let project = TempDir::new().expect("project");
        fs::write(project.path().join("unsafe\nfile"), []).expect("file");
        let runtime = runtime(&project);

        assert!(matches!(
            runtime.selected_file_reference(),
            Some(Err("selected file path cannot be inserted safely"))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn selected_symlink_never_inserts_a_file_reference() {
        let project = TempDir::new().expect("project");
        let outside = TempDir::new().expect("outside");
        let target = outside.path().join("outside.txt");
        fs::write(&target, []).expect("outside file");
        symlink(target, project.path().join("link.txt")).expect("symlink");
        let runtime = runtime(&project);

        assert_eq!(runtime.selected_file_reference(), None);
    }

    #[cfg(unix)]
    fn executable(path: &Path, contents: &str) {
        fs::write(path, contents).expect("script");
        let mut permissions = fs::metadata(path).expect("metadata").permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions).expect("permissions");
    }

    #[test]
    fn navigates_expands_and_collapses_without_eagerly_loading_descendants() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join("src")).expect("src");
        fs::write(temp.path().join("src/child.rs"), []).expect("child");
        fs::write(temp.path().join("root.rs"), []).expect("root file");
        let mut runtime = runtime(&temp);
        let mut workers = workers();

        assert_eq!(
            runtime.visible_rows,
            [Path::new("src"), Path::new("root.rs")]
        );
        assert!(
            runtime
                .model
                .tree()
                .node(Path::new("src/child.rs"))
                .is_none()
        );

        let outcome = runtime.handle_event(press(KeyCode::Right), &mut workers);
        assert!(outcome.redraw);
        let RuntimeMessage::Filesystem {
            generation,
            expansions,
            result,
        } = receive(&mut workers)
        else {
            panic!("unexpected worker result");
        };

        assert!(runtime.complete_filesystem(generation, expansions, result, &mut workers));
        assert_eq!(
            runtime.visible_rows,
            [
                Path::new("src"),
                Path::new("src/child.rs"),
                Path::new("root.rs")
            ]
        );

        runtime.handle_event(press(KeyCode::Down), &mut workers);
        assert_eq!(
            runtime.model.tree().selection(),
            Some(Path::new("src/child.rs"))
        );
        runtime.handle_event(press(KeyCode::Left), &mut workers);
        assert_eq!(runtime.model.tree().selection(), Some(Path::new("src")));
        runtime.handle_event(press(KeyCode::Left), &mut workers);
        assert_eq!(
            runtime.visible_rows,
            [Path::new("src"), Path::new("root.rs")]
        );
        assert_eq!(
            runtime.generations().0,
            runtime.generations().1,
            "quiescent collapse must not leave an unapplied generation"
        );
    }
    #[test]
    fn second_toggle_cancels_an_expansion_still_in_flight() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join("src")).expect("src");
        fs::write(temp.path().join("src/child.rs"), []).expect("child");
        let mut runtime = runtime(&temp);
        let mut workers = workers();

        runtime.handle_event(press(KeyCode::Enter), &mut workers);
        runtime.handle_event(press(KeyCode::Enter), &mut workers);
        let RuntimeMessage::Filesystem {
            generation,
            expansions,
            result,
        } = receive(&mut workers)
        else {
            panic!("unexpected worker result");
        };
        runtime.complete_filesystem(generation, expansions, result, &mut workers);

        assert_eq!(runtime.visible_rows, [Path::new("src")]);
        assert!(runtime.expanded.is_empty());
    }

    #[test]
    fn manual_refresh_reloads_the_filesystem_on_a_worker() {
        let temp = TempDir::new().expect("tempdir");
        fs::write(temp.path().join("initial"), []).expect("initial");
        let mut runtime = runtime(&temp);
        fs::write(temp.path().join("added-later"), []).expect("added later");
        let mut workers = workers();

        assert!(
            runtime
                .handle_event(press(KeyCode::Char('r')), &mut workers)
                .redraw
        );
        let RuntimeMessage::Filesystem {
            generation,
            expansions,
            result,
        } = receive(&mut workers)
        else {
            panic!("unexpected worker result");
        };
        assert!(runtime.complete_filesystem(generation, expansions, result, &mut workers));

        assert!(
            runtime
                .visible_rows
                .contains(&Path::new("added-later").to_path_buf())
        );
    }

    #[test]
    fn manual_refresh_prunes_an_expanded_directory_deleted_on_disk() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join("removed")).expect("directory");
        fs::write(temp.path().join("removed/child"), []).expect("child");
        let mut runtime = runtime(&temp);
        let mut workers = workers();

        runtime.handle_event(press(KeyCode::Right), &mut workers);
        let RuntimeMessage::Filesystem {
            generation,
            expansions,
            result,
        } = receive(&mut workers)
        else {
            panic!("unexpected worker result");
        };
        runtime.complete_filesystem(generation, expansions, result, &mut workers);
        fs::remove_dir_all(temp.path().join("removed")).expect("remove directory");

        runtime.handle_event(press(KeyCode::Char('r')), &mut workers);
        let RuntimeMessage::Filesystem {
            generation,
            expansions,
            result,
        } = receive(&mut workers)
        else {
            panic!("unexpected worker result");
        };
        runtime.complete_filesystem(generation, expansions, result, &mut workers);

        assert!(runtime.model.tree().node(Path::new("removed")).is_none());
        assert!(runtime.filesystem_notice.is_none());
    }

    #[test]
    fn mouse_selects_scrolls_and_requests_directory_expansion() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join("src")).expect("src");

        fs::write(temp.path().join("root.rs"), []).expect("root file");
        let mut runtime = runtime(&temp);

        let mut workers = workers();
        let area = Rect::new(0, 0, 20, 4);
        runtime.render(area, &mut Buffer::empty(area));

        runtime.handle_event(
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 0,
                row: 1,
                modifiers: KeyModifiers::NONE,
            }),
            &mut workers,
        );
        assert_eq!(runtime.model.tree().selection(), Some(Path::new("src")));

        runtime.handle_event(
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Right),
                column: 0,
                row: 1,
                modifiers: KeyModifiers::NONE,
            }),
            &mut workers,
        );
        assert_eq!(runtime.model.tree().selection(), Some(Path::new("src")));
        assert!(matches!(
            receive(&mut workers),
            RuntimeMessage::Filesystem { .. }
        ));
    }
    #[test]
    fn rejects_a_stale_descendant_snapshot_after_parent_removal() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join("removed")).expect("directory");
        fs::write(temp.path().join("removed/child"), []).expect("child");
        let mut runtime = runtime(&temp);
        let loader = runtime.model.tree().directory_loader();
        let stale_descendant = loader
            .load(Path::new("removed").to_path_buf())
            .expect("stale descendant");
        fs::remove_dir_all(temp.path().join("removed")).expect("remove directory");
        let current_root = loader.load(PathBuf::new()).expect("current root");
        let mut workers = workers();
        runtime.filesystem_running = Some(1);
        runtime.filesystem_generation = 1;

        runtime.complete_filesystem(
            1,
            BTreeSet::new(),
            vec![
                (PathBuf::new(), Ok(current_root)),
                (PathBuf::from("removed"), Ok(stale_descendant)),
            ],
            &mut workers,
        );

        assert!(runtime.model.tree().node(Path::new("removed")).is_none());
        assert!(
            runtime
                .model
                .tree()
                .node(Path::new("removed/child"))
                .is_none()
        );
    }
    #[test]
    fn right_click_on_empty_space_does_not_toggle_the_current_selection() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join("src")).expect("src");
        let mut runtime = runtime(&temp);
        let mut workers = workers();
        let area = Rect::new(0, 0, 20, 3);
        runtime.render(area, &mut Buffer::empty(area));

        let outcome = runtime.handle_event(
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Right),
                column: 0,
                row: 2,
                modifiers: KeyModifiers::NONE,
            }),
            &mut workers,
        );

        assert!(!outcome.redraw);
        assert!(runtime.expanded.is_empty());
        assert!(workers.try_recv().is_none());
    }

    #[test]
    fn renders_only_the_cached_viewport_and_scrolls_selection_into_view() {
        let temp = TempDir::new().expect("tempdir");
        for name in ["a", "b", "c"] {
            fs::write(temp.path().join(name), []).expect("file");
        }
        let mut runtime = runtime(&temp);
        let mut workers = workers();
        let area = Rect::new(0, 0, 10, 6);
        let mut buffer = Buffer::empty(area);
        runtime.render(area, &mut buffer);

        runtime.handle_event(press(KeyCode::End), &mut workers);
        let cached_rows = runtime.visible_rows.as_ptr();
        runtime.render(area, &mut buffer);

        assert_eq!(runtime.visible_rows.as_ptr(), cached_rows);
        assert_eq!(runtime.viewport_offset, 1);
        // Header owns row 0; offset 1 leaves room for `b` on row 1 and
        // `c` on row 2.
        let second_line = (0..area.width)
            .map(|x| buffer[(x, 1)].symbol())
            .collect::<String>();
        assert!(second_line.contains('b'));
        let third_line = (0..area.width)
            .map(|x| buffer[(x, 2)].symbol())
            .collect::<String>();
        assert!(third_line.contains('c'));
    }

    #[test]
    fn idle_events_do_not_request_a_redraw() {
        let temp = TempDir::new().expect("tempdir");
        fs::write(temp.path().join("file"), []).expect("file");
        let mut runtime = runtime(&temp);
        let mut workers = workers();

        assert!(
            !runtime
                .handle_event(Event::FocusGained, &mut workers)
                .redraw
        );
        assert!(
            runtime
                .handle_event(Event::Resize(80, 24), &mut workers)
                .redraw
        );
    }

    #[test]
    fn right_descends_to_a_virtual_child_attached_by_display_parent() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join("src")).expect("src");
        let mut runtime = runtime(&temp);
        runtime
            .model
            .tree_mut()
            .merge_status(&VcsStatusSnapshot::new(
                vec![
                    VcsEntryStatus::new(
                        PathBuf::from("src/removed/missing.rs"),
                        None,
                        VcsStatusKind::Deleted,
                        Some(VcsStatusKind::Deleted),
                        None,
                    )
                    .expect("status"),
                ],
                false,
            ))
            .expect("merge status");
        runtime.rebuild_visible_rows();
        let mut workers = workers();

        runtime.handle_event(press(KeyCode::Right), &mut workers);
        let RuntimeMessage::Filesystem {
            generation,
            expansions,
            result,
        } = receive(&mut workers)
        else {
            panic!("unexpected worker result");
        };
        runtime.complete_filesystem(generation, expansions, result, &mut workers);
        runtime.handle_event(press(KeyCode::Right), &mut workers);

        assert_eq!(
            runtime.model.tree().selection(),
            Some(Path::new("src/removed/missing.rs"))
        );
        runtime.handle_event(press(KeyCode::Left), &mut workers);
        assert_eq!(runtime.model.tree().selection(), Some(Path::new("src")));
    }

    #[test]
    fn failed_expansion_can_be_retried_with_right() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join("src")).expect("src");
        let mut runtime = runtime(&temp);
        let mut workers = workers();
        runtime.filesystem_running = Some(1);
        runtime.filesystem_generation = 1;
        runtime.desired_expanded.insert(PathBuf::from("src"));

        runtime.complete_filesystem(
            1,
            BTreeSet::from([PathBuf::from("src")]),
            vec![(
                PathBuf::from("src"),
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "denied",
                )),
            )],
            &mut workers,
        );
        runtime.handle_event(press(KeyCode::Right), &mut workers);

        assert!(matches!(
            receive(&mut workers),
            RuntimeMessage::Filesystem { .. }
        ));
    }

    #[test]
    fn restores_selection_when_a_virtual_row_moves_under_a_collapsed_directory() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir_all(temp.path().join("src/nested")).expect("nested");
        let mut runtime = runtime(&temp);
        runtime
            .model
            .tree_mut()
            .merge_status(&VcsStatusSnapshot::new(
                vec![
                    VcsEntryStatus::new(
                        PathBuf::from("src/nested/missing.rs"),
                        None,
                        VcsStatusKind::Deleted,
                        Some(VcsStatusKind::Deleted),
                        None,
                    )
                    .expect("status"),
                ],
                false,
            ))
            .expect("merge status");
        runtime.expanded.insert(PathBuf::from("src"));
        runtime.desired_expanded.insert(PathBuf::from("src"));
        runtime.rebuild_visible_rows();
        assert!(
            runtime
                .model
                .tree_mut()
                .select(Path::new("src/nested/missing.rs"))
        );
        let snapshot = runtime
            .model
            .tree()
            .directory_loader()
            .load(PathBuf::from("src"))
            .expect("src snapshot");
        runtime.filesystem_generation = 1;
        runtime.filesystem_running = Some(1);
        let mut workers = workers();

        runtime.complete_filesystem(
            1,
            BTreeSet::new(),
            vec![(PathBuf::from("src"), Ok(snapshot))],
            &mut workers,
        );

        assert_eq!(
            runtime.model.tree().selection(),
            Some(Path::new("src/nested"))
        );
        assert!(runtime.visible_rows.contains(&PathBuf::from("src/nested")));
    }

    #[test]
    fn rejects_filesystem_results_superseded_by_a_newer_request() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join("src")).expect("src");
        let mut runtime = runtime(&temp);
        let mut workers = workers();
        runtime.request_expansion(PathBuf::from("src"), &mut workers);
        let RuntimeMessage::Filesystem {
            generation,
            expansions,
            ..
        } = receive(&mut workers)
        else {
            panic!("unexpected worker result");
        };
        runtime.request_reload(&mut workers);

        assert!(!runtime.complete_filesystem(
            generation,
            expansions,
            vec![(
                PathBuf::from("src"),
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "stale")),
            )],
            &mut workers,
        ));
        assert!(runtime.filesystem_notice.is_none());

        let RuntimeMessage::Filesystem {
            generation,
            expansions,
            result,
        } = receive(&mut workers)
        else {
            panic!("unexpected worker result");
        };
        assert!(runtime.complete_filesystem(generation, expansions, result, &mut workers));
    }
    #[test]
    fn changing_jujutsu_mode_supersedes_an_in_flight_status_result() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir_all(temp.path().join(".jj/repo")).expect("repo marker");
        fs::create_dir_all(temp.path().join(".jj/working_copy")).expect("working-copy marker");
        let context = LaunchContext::from_vars([(
            "HERDR_PLUGIN_CONTEXT_JSON",
            format!(
                r#"{{"workspace_id":"workspace","tab_id":"tab","pane_id":"pane","cwd":"{}"}}"#,
                temp.path().display()
            ),
        )])
        .expect("context");
        let mut runtime =
            FilesRuntime::bootstrap_with_jujutsu_mode(&context, crate::vcs::jj::JujutsuMode::Fresh)
                .expect("runtime");
        let mut workers = workers();
        runtime.model.request_refresh();
        let generation = runtime.model.begin_refresh().expect("running generation");
        let input = runtime.model.status_merge_input();

        assert!(runtime.set_jujutsu_mode(crate::vcs::jj::JujutsuMode::Passive, &mut workers));
        assert!(runtime.model.status_is_stale());
        let old_result = crate::files::PreparedRefreshResult::prepare(
            generation,
            input,
            Ok(VcsStatusSnapshot::new(Vec::new(), false)),
            None,
        );

        assert!(!runtime.model.complete_prepared_refresh(old_result));
        assert_eq!(
            runtime.jujutsu_mode(),
            Some(crate::vcs::jj::JujutsuMode::Passive)
        );
        assert!(
            runtime.model.begin_refresh().is_some(),
            "mode change did not queue a replacement generation"
        );
    }

    #[cfg(unix)]
    #[test]
    fn jujutsu_refreshes_only_on_activation_and_explicit_refresh() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir_all(temp.path().join(".jj/repo")).expect("repo marker");
        fs::create_dir_all(temp.path().join(".jj/working_copy")).expect("working-copy marker");
        fs::write(temp.path().join("tracked"), []).expect("tracked");
        let calls = temp.path().join("calls");
        let script = temp.path().join("fake-jj");
        executable(
            &script,
            &format!(
                "#!/bin/sh\nprintf 'call\\n' >> '{}'\nprintf 'M\\000tracked\\000tracked\\000false\\000false\\000file\\000file\\000'\n",
                calls.display()
            ),
        );
        let context = LaunchContext::from_vars([(
            "HERDR_PLUGIN_CONTEXT_JSON",
            format!(
                r#"{{"workspace_id":"workspace","tab_id":"tab","pane_id":"pane","cwd":"{}"}}"#,
                temp.path().display()
            ),
        )])
        .expect("context");
        let mut runtime = FilesRuntime::bootstrap_with_jujutsu_mode(&context, JujutsuMode::Fresh)
            .expect("runtime");
        let Some(VcsRefresh::Jujutsu { service, .. }) = &mut runtime.vcs else {
            panic!("Jujutsu backend");
        };
        *service = JjService::with_executable(script, JujutsuMode::Fresh, Duration::from_secs(1));
        let mut workers = workers();

        runtime.start_background(&mut workers);
        for _ in 0..4 {
            if !runtime.has_pending_work() && !workers.has_pending_work() {
                break;
            }
            runtime.complete_background(receive(&mut workers), &mut workers);
        }
        let activation_calls = fs::read_to_string(&calls)
            .expect("activation calls")
            .lines()
            .count();
        assert!(
            (2..=4).contains(&activation_calls),
            "activation scheduled {activation_calls} Jujutsu commands"
        );
        assert!(!runtime.has_pending_work());
        assert!(!workers.has_pending_work());

        runtime.request_reload(&mut workers);
        for _ in 0..4 {
            if !runtime.has_pending_work() && !workers.has_pending_work() {
                break;
            }
            runtime.complete_background(receive(&mut workers), &mut workers);
        }
        let calls_after_refresh = fs::read_to_string(&calls)
            .expect("refresh calls")
            .lines()
            .count();
        assert!(
            (2..=4).contains(&calls_after_refresh.saturating_sub(activation_calls)),
            "manual refresh scheduled {} Jujutsu commands",
            calls_after_refresh.saturating_sub(activation_calls)
        );
        assert!(!runtime.has_pending_work());
        assert!(!workers.has_pending_work());

        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            fs::read_to_string(&calls)
                .expect("stable calls")
                .lines()
                .count(),
            calls_after_refresh,
            "unexpected periodic refresh"
        );
        assert!(workers.try_recv().is_none(), "unexpected periodic result");
    }

    #[test]
    fn toggling_ignored_files_preserves_selection_and_round_trips() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join(".git")).expect("git marker");
        fs::write(temp.path().join(".gitignore"), b"skip.txt\n").expect("gitignore");
        fs::create_dir(temp.path().join("src")).expect("src");
        fs::write(temp.path().join("src/main.rs"), []).expect("main");
        fs::write(temp.path().join("kept.rs"), []).expect("kept");
        fs::write(temp.path().join("skip.txt"), []).expect("skip");
        fs::write(temp.path().join("config.toml"), "[vcs]\nbackend = \"jj\"\n").expect("config");
        let context = LaunchContext::from_vars([(
            "HERDR_PLUGIN_CONTEXT_JSON",
            format!(
                r#"{{"workspace_id":"workspace","tab_id":"tab","pane_id":"pane","cwd":"{}"}}"#,
                temp.path().display()
            ),
        )])
        .expect("context");
        // A missing jj workspace keeps the probe subprocess-free while the
        // `.git` marker still enables the ignore matchers under test.
        let config = PluginConfig::load_from_dir(temp.path()).into_config();
        let mut runtime = FilesRuntime::bootstrap_with_config(&context, &config).expect("run");
        let mut workers = workers();
        runtime.start_background(&mut workers);
        for _ in 0..4 {
            if !runtime.has_pending_work() && !workers.has_pending_work() {
                break;
            }
            runtime.complete_background(receive(&mut workers), &mut workers);
        }
        assert!(runtime.model.tree().node(Path::new("kept.rs")).is_some());
        assert!(runtime.model.tree().node(Path::new("skip.txt")).is_none());
        assert!(runtime.model.select(Path::new("src")));

        assert!(runtime.handle_intent(&Intent::ToggleIgnoredFiles, &mut workers));
        for _ in 0..6 {
            if !runtime.has_pending_work() && !workers.has_pending_work() {
                break;
            }
            runtime.complete_background(receive(&mut workers), &mut workers);
        }
        assert!(runtime.model.tree().node(Path::new("skip.txt")).is_some());
        assert_eq!(runtime.model.tree().selection(), Some(Path::new("src")));

        assert!(runtime.handle_intent(&Intent::ToggleIgnoredFiles, &mut workers));
        for _ in 0..6 {
            if !runtime.has_pending_work() && !workers.has_pending_work() {
                break;
            }
            runtime.complete_background(receive(&mut workers), &mut workers);
        }
        assert!(runtime.model.tree().node(Path::new("skip.txt")).is_none());
        assert_eq!(runtime.model.tree().selection(), Some(Path::new("src")));
        workers.shutdown();
    }

    #[cfg(unix)]
    #[test]
    fn passive_staleness_shows_only_as_the_divider_hint() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir_all(temp.path().join(".jj/repo")).expect("repo marker");
        fs::create_dir_all(temp.path().join(".jj/working_copy")).expect("working-copy marker");
        fs::write(temp.path().join("visible"), []).expect("file");
        let script = temp.path().join("fake-jj");
        executable(&script, "#!/bin/sh\nexit 0\n");
        let context = LaunchContext::from_vars([(
            "HERDR_PLUGIN_CONTEXT_JSON",
            format!(
                r#"{{"workspace_id":"workspace","tab_id":"tab","pane_id":"pane","cwd":"{}"}}"#,
                temp.path().display()
            ),
        )])
        .expect("context");
        fs::write(
            temp.path().join("config.toml"),
            "[vcs]\njujutsu_mode = \"passive\"\npassive_jujutsu_interval_ms = 1000\n",
        )
        .expect("config");
        let config = PluginConfig::load_from_dir(temp.path()).into_config();
        let mut runtime = FilesRuntime::bootstrap_with_config(&context, &config).expect("runtime");
        let Some(VcsRefresh::Jujutsu { service, .. }) = &mut runtime.vcs else {
            panic!("Jujutsu backend");
        };
        *service = JjService::with_executable(script, JujutsuMode::Passive, Duration::from_secs(1));
        let mut workers = workers();
        runtime.start_background(&mut workers);
        for _ in 0..4 {
            if !runtime.has_pending_work() && !workers.has_pending_work() {
                break;
            }
            runtime.complete_background(receive(&mut workers), &mut workers);
        }

        assert!(runtime.model.status_is_stale());
        assert!(runtime.model.failure_notice().is_none());

        let area = Rect::new(0, 0, 64, 6);
        let mut buffer = Buffer::empty(area);
        runtime.render(area, &mut buffer);
        let lines = rendered_lines(&buffer, area);
        // Staleness is carried by the divider vocabulary alone: no footer row.
        assert!(lines.iter().all(|line| !line.contains("VCS passive")));
        assert!(lines.iter().any(|line| line.ends_with("passive")));
    }

    #[test]
    fn failed_passive_jujutsu_refresh_preserves_files_and_stale_notice() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir_all(temp.path().join(".jj/repo")).expect("repo marker");
        fs::create_dir_all(temp.path().join(".jj/working_copy")).expect("working-copy marker");
        fs::write(temp.path().join("visible"), []).expect("visible");
        let script = temp.path().join("fake-jj");
        executable(&script, "#!/bin/sh\nprintf 'failed' >&2\nexit 1\n");
        let context = LaunchContext::from_vars([(
            "HERDR_PLUGIN_CONTEXT_JSON",
            format!(
                r#"{{"workspace_id":"workspace","tab_id":"tab","pane_id":"pane","cwd":"{}"}}"#,
                temp.path().display()
            ),
        )])
        .expect("context");
        fs::write(
            temp.path().join("config.toml"),
            "[vcs]\njujutsu_mode = \"passive\"\npassive_jujutsu_interval_ms = 1000\n",
        )
        .expect("config");
        let config = PluginConfig::load_from_dir(temp.path()).into_config();
        let mut runtime = FilesRuntime::bootstrap_with_config(&context, &config).expect("runtime");
        let Some(VcsRefresh::Jujutsu { service, .. }) = &mut runtime.vcs else {
            panic!("Jujutsu backend");
        };
        *service = JjService::with_executable(script, JujutsuMode::Passive, Duration::from_secs(1));
        let mut workers = workers();

        runtime.start_background(&mut workers);
        for _ in 0..4 {
            if !runtime.has_pending_work() && !workers.has_pending_work() {
                break;
            }
            runtime.complete_background(receive(&mut workers), &mut workers);
        }

        assert!(runtime.model.tree().node(Path::new("visible")).is_some());
        assert!(
            runtime
                .model
                .failure_notice()
                .is_some_and(|notice| notice.contains("Jujutsu status failed"))
        );
        assert!(runtime.model.status_is_stale());
        assert!(
            runtime
                .next_refresh_in(Instant::now())
                .is_some_and(|wait| wait <= Duration::from_secs(1))
        );
        let area = Rect::new(0, 0, 64, 2);
        let mut buffer = Buffer::empty(area);
        runtime.render(area, &mut buffer);
        let notice = (0..area.width)
            .map(|x| buffer[(x, 1)].symbol())
            .collect::<String>();
        assert!(notice.starts_with("VCS stale: Jujutsu status failed"));
    }
    #[test]
    fn configured_backend_warning_survives_successful_filesystem_reload() {
        let temp = TempDir::new().expect("tempdir");
        fs::write(temp.path().join("visible"), b"file").expect("fixture");
        fs::write(
            temp.path().join("config.toml"),
            "[vcs]\nbackend = \"git\"\n",
        )
        .expect("config");
        let config = PluginConfig::load_from_dir(temp.path()).into_config();
        let context = LaunchContext::from_vars([(
            "HERDR_PLUGIN_CONTEXT_JSON",
            format!(
                r#"{{"workspace_id":"workspace","tab_id":"tab","pane_id":"pane","cwd":"{}"}}"#,
                temp.path().display()
            ),
        )])
        .expect("context");
        let mut runtime = FilesRuntime::bootstrap_with_config(&context, &config).expect("runtime");
        let mut workers = WorkerRuntime::with_capacities(2, 1);
        assert!(runtime.backend_notice.is_some());

        runtime.start_background(&mut workers);
        assert!(runtime.complete_background(receive(&mut workers), &mut workers));

        assert!(runtime.backend_notice.is_some());
        workers.shutdown();
    }

    #[test]
    fn adaptive_git_cadence_backs_off_resets_and_suspends() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join(".git")).expect("git marker");
        fs::write(
            temp.path().join("config.toml"),
            concat!(
                "[vcs]\n",
                "git_cadence = \"adaptive\"\n",
                "git_min_interval_ms = 1000\n",
                "git_max_interval_ms = 4000\n",
            ),
        )
        .expect("config");
        let config = PluginConfig::load_from_dir(temp.path()).into_config();
        assert!(matches!(
            config.vcs().refresh().git(),
            GitCadence::Adaptive { .. }
        ));
        let context = LaunchContext::from_vars([(
            "HERDR_PLUGIN_CONTEXT_JSON",
            format!(
                r#"{{"workspace_id":"workspace","tab_id":"tab","pane_id":"pane","cwd":"{}"}}"#,
                temp.path().display()
            ),
        )])
        .expect("context");
        let mut runtime = FilesRuntime::bootstrap_with_config(&context, &config).expect("runtime");
        runtime.vcs = Some(VcsRefresh::Git {
            service: GitService::default(),
            workspace: VcsWorkspace::new(
                temp.path().to_path_buf(),
                VcsBackendMetadata::new("git", "Git", false).expect("metadata"),
            )
            .expect("workspace"),
        });
        let now = Instant::now();

        runtime.schedule_next_status_refresh(now, Some(7), true);
        assert_eq!(runtime.next_refresh_in(now), Some(Duration::from_secs(1)));
        runtime.schedule_next_status_refresh(now, Some(7), true);
        assert_eq!(runtime.next_refresh_in(now), Some(Duration::from_secs(2)));
        runtime.schedule_next_status_refresh(now, Some(7), true);
        assert_eq!(runtime.next_refresh_in(now), Some(Duration::from_secs(4)));
        runtime.schedule_next_status_refresh(now, Some(7), true);
        assert_eq!(runtime.next_refresh_in(now), Some(Duration::from_secs(4)));
        runtime.schedule_next_status_refresh(now, Some(8), true);
        assert_eq!(runtime.next_refresh_in(now), Some(Duration::from_secs(1)));

        runtime.pause_background();
        assert_eq!(runtime.next_refresh_in(now), None);
    }

    #[test]
    fn file_search_finds_collapsed_descendants_and_restores_the_normal_tree() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join("src")).expect("src");
        fs::write(temp.path().join("src/nested.rs"), []).expect("nested file");
        fs::write(temp.path().join("root.txt"), []).expect("root file");
        let mut runtime = runtime(&temp);
        let mut workers = workers();

        assert!(runtime.handle_intent(&Intent::BeginFileSearch, &mut workers));
        assert!(runtime.handle_intent(&Intent::FileSearchInput("nested".to_owned()), &mut workers));
        assert!(runtime.visible_rows.is_empty());
        let search = runtime.search.as_ref().expect("search");
        assert!(search.projection_applied < search.projection_requested);
        while runtime
            .search
            .as_ref()
            .is_some_and(FileSearchState::scanning)
        {
            let message = receive(&mut workers);
            runtime.complete_background(message, &mut workers);
        }

        assert_eq!(
            runtime.visible_rows,
            [PathBuf::from("src"), PathBuf::from("src/nested.rs")]
        );
        assert_eq!(runtime.selection(), Some(Path::new("src/nested.rs")));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 5));
        runtime.render(Rect::new(0, 0, 40, 5), &mut buffer);
        let rendered = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Search > nested"));
        assert!(rendered.contains("nested.rs"));

        assert!(runtime.handle_intent(&Intent::FileSearchCommit, &mut workers));
        assert_eq!(
            runtime.selected_file_reference(),
            Some(Ok("@src/nested.rs ".to_owned()))
        );

        fs::write(temp.path().join("src/another-nested.rs"), []).expect("new nested file");
        assert!(runtime.handle_intent(&Intent::Refresh, &mut workers));
        while runtime.has_pending_work() || workers.has_pending_work() {
            let message = receive(&mut workers);
            runtime.complete_background(message, &mut workers);
        }
        assert!(
            runtime
                .visible_rows
                .contains(&PathBuf::from("src/another-nested.rs"))
        );
        assert!(runtime.handle_intent(&Intent::FileSearchCancel, &mut workers));
        assert!(
            !runtime
                .visible_rows
                .contains(&PathBuf::from("src/nested.rs"))
        );
        assert_eq!(runtime.selection(), Some(Path::new("src")));
        workers.shutdown();
    }

    #[test]
    fn configured_search_finds_ignored_paths_only_in_projection() {
        let project = TempDir::new().expect("project");
        fs::create_dir(project.path().join(".git")).expect("Git marker");
        fs::write(
            project.path().join(".gitignore"),
            "ignored.txt\nignored-dir/\n",
        )
        .expect("gitignore");
        fs::write(project.path().join("visible.txt"), []).expect("visible file");
        fs::write(project.path().join("ignored.txt"), []).expect("ignored file");
        fs::create_dir(project.path().join("ignored-dir")).expect("ignored directory");
        fs::write(project.path().join("ignored-dir/needle.txt"), []).expect("ignored descendant");
        fs::create_dir(project.path().join("hard")).expect("excluded directory");
        fs::write(project.path().join("hard/needle.txt"), []).expect("excluded descendant");
        fs::create_dir(project.path().join(".hidden")).expect("hidden directory");
        fs::write(project.path().join(".hidden/needle.txt"), []).expect("hidden descendant");
        let config_dir = TempDir::new().expect("config");
        fs::write(
            config_dir.path().join("config.toml"),
            "[files]\nsearch_ignored = true\nexclusions = [\"hard\"]\n",
        )
        .expect("config file");
        let config = PluginConfig::load_from_dir(config_dir.path()).into_config();
        let context = LaunchContext::from_vars([(
            "HERDR_PLUGIN_CONTEXT_JSON",
            format!(
                r#"{{"workspace_id":"workspace","tab_id":"tab","pane_id":"pane","cwd":"{}"}}"#,
                project.path().display()
            ),
        )])
        .expect("context");
        let mut runtime =
            FilesRuntime::bootstrap_with_config(&context, &config).expect("configured runtime");
        let mut workers = workers();

        assert!(
            runtime
                .model
                .tree()
                .node(Path::new("ignored.txt"))
                .is_none()
        );
        assert!(
            runtime
                .model
                .tree()
                .node(Path::new("ignored-dir"))
                .is_none()
        );
        assert!(runtime.model.tree().node(Path::new("hard")).is_none());
        assert!(runtime.handle_intent(&Intent::BeginFileSearch, &mut workers));
        assert!(runtime.handle_intent(&Intent::FileSearchInput("needle".to_owned()), &mut workers));
        while runtime
            .search
            .as_ref()
            .is_some_and(FileSearchState::scanning)
        {
            runtime.complete_background(receive(&mut workers), &mut workers);
        }

        assert_eq!(
            runtime.visible_rows,
            [
                PathBuf::from("ignored-dir"),
                PathBuf::from("ignored-dir/needle.txt")
            ]
        );
        let search = runtime.search.as_ref().expect("search");
        assert!(
            search
                .index
                .node(Path::new("ignored-dir/needle.txt"))
                .is_some_and(|node| node.is_ignored())
        );
        assert!(search.index.node(Path::new("hard/needle.txt")).is_none());
        assert!(search.index.node(Path::new(".hidden/needle.txt")).is_none());

        assert!(runtime.handle_intent(&Intent::FileSearchCancel, &mut workers));
        assert!(!runtime.visible_rows.contains(&PathBuf::from("ignored-dir")));
        assert!(
            runtime
                .model
                .tree()
                .node(Path::new("ignored-dir"))
                .is_none()
        );
        workers.shutdown();
    }

    #[test]
    fn empty_file_search_redraws_when_indexing_finishes() {
        let temp = TempDir::new().expect("tempdir");
        fs::write(temp.path().join("main.rs"), []).expect("file");
        let mut runtime = runtime(&temp);
        let mut workers = workers();

        assert!(runtime.handle_intent(&Intent::BeginFileSearch, &mut workers));
        let mut redrawn = false;
        while runtime
            .search
            .as_ref()
            .is_some_and(FileSearchState::scanning)
        {
            redrawn |= runtime.complete_background(receive(&mut workers), &mut workers);
        }
        assert!(redrawn);

        let area = Rect::new(0, 0, 40, 3);
        let mut buffer = Buffer::empty(area);
        runtime.render(area, &mut buffer);
        let first = (0..area.width)
            .map(|x| buffer[(x, 0)].symbol())
            .collect::<String>();
        assert!(first.contains("0 matches"));
        assert!(!first.contains("scanning"));
        workers.shutdown();
    }

    #[test]
    fn pausing_files_cancels_and_requeues_the_active_search_page() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join("src")).expect("src");
        let mut runtime = runtime(&temp);
        let mut workers = workers();

        assert!(runtime.handle_intent(&Intent::BeginFileSearch, &mut workers));
        let cancellation = runtime
            .search
            .as_ref()
            .and_then(|search| search.page_cancel.clone())
            .expect("page cancellation");
        runtime.pause_background();

        let search = runtime.search.as_ref().expect("search");
        assert!(cancellation.load(Ordering::Relaxed));
        assert!(search.running.is_none());
        assert_eq!(
            search.pending_directories.front().map(PathBuf::as_path),
            Some(Path::new(""))
        );
        workers.shutdown();
    }

    fn changed_entry(path: &str, kind: VcsStatusKind) -> VcsEntryStatus {
        VcsEntryStatus::new(PathBuf::from(path), None, kind, None, None).expect("status entry")
    }

    fn complete_status(
        runtime: &mut FilesRuntime,
        workers: &mut WorkerRuntime,
        entries: Vec<VcsEntryStatus>,
    ) {
        let _ = runtime.model.request_refresh();
        let generation = runtime.model.begin_refresh().expect("refresh began");
        let input = runtime.model.status_merge_input();
        let prepared = PreparedRefreshResult::prepare(
            generation,
            input,
            Ok(VcsStatusSnapshot::new(entries, false)),
            Some(VcsDiffStats::new(2, 1)),
        );
        runtime.complete_vcs_refresh_at(prepared, workers, Instant::now());
    }

    fn rendered_lines(buffer: &Buffer, area: Rect) -> Vec<String> {
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn renders_split_tree_and_flat_changed_panes() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join("src")).expect("src");
        fs::write(temp.path().join("src/kept.rs"), []).expect("kept");
        fs::write(temp.path().join("fresh.rs"), []).expect("fresh");
        let mut runtime = runtime(&temp);
        let mut workers = workers();
        let area = Rect::new(0, 0, 24, 4);

        runtime.render(area, &mut Buffer::empty(area));
        let mut buffer = Buffer::empty(area);
        runtime.render(area, &mut buffer);
        assert!(rendered_lines(&buffer, area)[2].starts_with("- Changes (0)"));
        assert!(rendered_lines(&buffer, area)[3].starts_with("VCS status unavailable"));

        complete_status(
            &mut runtime,
            &mut workers,
            vec![
                changed_entry("fresh.rs", VcsStatusKind::Untracked),
                changed_entry("gone.rs", VcsStatusKind::Deleted),
                changed_entry("src/kept.rs", VcsStatusKind::Modified),
            ],
        );
        let mut buffer = Buffer::empty(area);
        runtime.render(area, &mut buffer);
        let lines = rendered_lines(&buffer, area);
        // Header owns row 0; the first tree row follows on row 1.
        let project_label = temp
            .path()
            .file_name()
            .expect("tempdir name")
            .to_string_lossy()
            .to_uppercase();
        assert!(lines[0].starts_with(&project_label));
        assert!(lines[1].contains("src"));
        assert!(lines[2].starts_with("- Changes +2 -1"));
        assert!(lines[3].starts_with("? fresh.rs"));
        // The flat pane is viewport-bounded: rows beyond the half stay hidden.
        assert!(!lines.iter().any(|line| line.contains("kept.rs")));
        workers.shutdown();
    }

    #[test]
    fn focus_toggle_navigates_and_activates_the_changed_pane() {
        let temp = TempDir::new().expect("tempdir");
        fs::write(temp.path().join("fresh.rs"), []).expect("fresh");
        let mut runtime = runtime(&temp);
        let mut workers = workers();
        complete_status(
            &mut runtime,
            &mut workers,
            vec![
                changed_entry("fresh.rs", VcsStatusKind::Untracked),
                changed_entry("missing.rs", VcsStatusKind::Deleted),
            ],
        );
        let area = Rect::new(0, 0, 24, 4);
        runtime.render(area, &mut Buffer::empty(area));

        assert!(runtime.handle_intent(&Intent::SwitchFilesPane, &mut workers));
        assert_eq!(runtime.focused_pane, FilesPane::Changed);
        assert!(runtime.handle_intent(&Intent::SelectLast, &mut workers));
        assert_eq!(
            runtime.changed_selected.as_deref(),
            Some(Path::new("missing.rs"))
        );
        assert!(matches!(runtime.selected_file_reference(), Some(Err(_))));

        assert!(runtime.handle_intent(&Intent::SelectFirst, &mut workers));
        assert_eq!(
            runtime.selected_file_reference(),
            Some(Ok("@fresh.rs ".to_owned()))
        );

        // Tree navigation stays inert while the flat pane owns the focus.
        let tree_selection = runtime.model.tree().selection().map(Path::to_path_buf);
        assert!(!runtime.handle_intent(&Intent::ExpandOrDescend, &mut workers));
        assert_eq!(
            runtime.model.tree().selection().map(Path::to_path_buf),
            tree_selection
        );

        // A missing row activation surfaces through the shared notice path.
        assert!(runtime.handle_intent(&Intent::SelectLast, &mut workers));
        if let Some(Err(message)) = runtime.selected_file_reference() {
            assert!(runtime.set_pane_input_notice(Some(message.to_owned())));
        }
        let notice_area = Rect::new(0, 0, 40, 6);
        let mut notice_buffer = Buffer::empty(notice_area);
        runtime.render(notice_area, &mut notice_buffer);
        let lines = rendered_lines(&notice_buffer, notice_area);
        assert!(lines.last().is_some_and(|line| {
            line.contains("Files") && line.contains("modified file is missing")
        }));
        workers.shutdown();
    }

    #[test]
    fn clicks_select_within_the_clicked_pane() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join("src")).expect("src");
        fs::write(temp.path().join("src/kept.rs"), []).expect("kept");
        fs::write(temp.path().join("fresh.rs"), []).expect("fresh");
        let mut runtime = runtime(&temp);
        let mut workers = workers();
        complete_status(
            &mut runtime,
            &mut workers,
            vec![
                changed_entry("fresh.rs", VcsStatusKind::Untracked),
                changed_entry("src/kept.rs", VcsStatusKind::Modified),
            ],
        );
        let area = Rect::new(0, 0, 24, 6);
        runtime.render(area, &mut Buffer::empty(area));

        assert!(runtime.handle_intent(
            &Intent::Pointer {
                column: 0,
                row: 5,
                action: PointerAction::Select,
            },
            &mut workers,
        ));
        assert_eq!(runtime.focused_pane, FilesPane::Changed);
        assert_eq!(
            runtime.selected_file_reference(),
            Some(Ok("@src/kept.rs ".to_owned()))
        );

        // Clicking the divider row itself is a no-op.
        assert!(!runtime.handle_intent(
            &Intent::Pointer {
                column: 0,
                row: 3,
                action: PointerAction::Select,
            },
            &mut workers,
        ));

        // Right-click has no toggle semantics in the flat pane.
        assert!(!runtime.handle_intent(
            &Intent::Pointer {
                column: 0,
                row: 5,
                action: PointerAction::Toggle,
            },
            &mut workers,
        ));

        // Row 1 is the default tree selection; click the second visible
        // row so the pointer actually changes it.
        assert!(runtime.handle_intent(
            &Intent::Pointer {
                column: 0,
                row: 2,
                action: PointerAction::Select,
            },
            &mut workers,
        ));
        assert_eq!(runtime.focused_pane, FilesPane::Tree);
        let second_visible = runtime.visible_rows[runtime.viewport_offset + 1].clone();
        assert_eq!(
            runtime.model.tree().selection(),
            Some(second_visible.as_path())
        );
        assert_eq!(
            runtime.selected_file_reference(),
            Some(Ok(format!("@{} ", second_visible.display())))
        );
        workers.shutdown();
    }

    #[test]
    fn search_overlay_hides_the_changed_pane() {
        let temp = TempDir::new().expect("tempdir");
        fs::write(temp.path().join("fresh.rs"), []).expect("fresh");
        let mut runtime = runtime(&temp);
        let mut workers = workers();
        complete_status(
            &mut runtime,
            &mut workers,
            vec![changed_entry("fresh.rs", VcsStatusKind::Untracked)],
        );
        let area = Rect::new(0, 0, 24, 4);
        runtime.render(area, &mut Buffer::empty(area));
        assert!(runtime.handle_intent(&Intent::SwitchFilesPane, &mut workers));
        runtime.handle_intent(&Intent::SelectLast, &mut workers);
        assert_eq!(
            runtime.changed_selected.as_deref(),
            Some(Path::new("fresh.rs"))
        );

        assert!(runtime.handle_intent(&Intent::BeginFileSearch, &mut workers));
        runtime.render(area, &mut Buffer::empty(area));
        let mut buffer = Buffer::empty(area);
        runtime.render(area, &mut buffer);
        let lines = rendered_lines(&buffer, area);
        assert!(lines[0].starts_with("Search > "));
        assert!(lines[3].trim().is_empty());
        assert!(!runtime.handle_intent(&Intent::SwitchFilesPane, &mut workers));

        assert!(runtime.handle_intent(&Intent::FileSearchCancel, &mut workers));
        runtime.render(area, &mut buffer);
        assert!(rendered_lines(&buffer, area)[3].starts_with("? fresh.rs"));
        workers.shutdown();
    }

    #[test]
    fn tiny_terminals_keep_the_tree_priority() {
        let temp = TempDir::new().expect("tempdir");
        fs::write(temp.path().join("fresh.rs"), []).expect("fresh");
        let mut runtime = runtime(&temp);
        let mut workers = workers();
        complete_status(
            &mut runtime,
            &mut workers,
            vec![changed_entry("fresh.rs", VcsStatusKind::Untracked)],
        );
        let area = Rect::new(0, 0, 24, 1);
        runtime.render(area, &mut Buffer::empty(area));

        assert_eq!(runtime.changed_height, 0);
        assert!(!runtime.handle_intent(&Intent::SwitchFilesPane, &mut workers));

        // Two rows are still too small: the divider only appears with room
        // for at least one tree row and one flat row.
        let area = Rect::new(0, 0, 24, 2);
        runtime.render(area, &mut Buffer::empty(area));
        assert_eq!(runtime.changed_height, 0);
        assert!(!runtime.handle_intent(&Intent::SwitchFilesPane, &mut workers));
        workers.shutdown();
    }

    #[test]
    fn changed_selection_follows_refreshes() {
        let temp = TempDir::new().expect("tempdir");
        fs::write(temp.path().join("fresh.rs"), []).expect("fresh");
        let mut runtime = runtime(&temp);
        let mut workers = workers();
        complete_status(
            &mut runtime,
            &mut workers,
            vec![
                changed_entry("fresh.rs", VcsStatusKind::Untracked),
                changed_entry("gone.rs", VcsStatusKind::Deleted),
            ],
        );
        runtime.render(
            Rect::new(0, 0, 24, 4),
            &mut Buffer::empty(Rect::new(0, 0, 24, 4)),
        );
        assert!(runtime.handle_intent(&Intent::SwitchFilesPane, &mut workers));
        assert!(runtime.handle_intent(&Intent::SelectLast, &mut workers));
        assert_eq!(
            runtime.changed_selected.as_deref(),
            Some(Path::new("gone.rs"))
        );

        complete_status(
            &mut runtime,
            &mut workers,
            vec![changed_entry("fresh.rs", VcsStatusKind::Untracked)],
        );
        assert_eq!(
            runtime.changed_selected.as_deref(),
            Some(Path::new("fresh.rs"))
        );

        complete_status(&mut runtime, &mut workers, Vec::new());
        assert_eq!(runtime.changed_selected, None);
        workers.shutdown();
    }
}
