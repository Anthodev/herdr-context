use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::files::tree::{DirectorySnapshot, TreeNodeKind};
use crate::files::{FilesModel, PreparedRefreshResult};
use crate::host::LaunchContext;
use crate::intent::{Intent, PointerAction};
use crate::project::resolve_project_context;
use crate::ui::files::FilesView;
use crate::vcs::git::GitService;
use crate::vcs::{VcsBackendMetadata, VcsWorkspace};
use crate::worker::{Job, JobKey, JobKind, Priority, SubmitStatus, WorkerRuntime};

#[derive(Clone, Debug)]
struct GitRefresh {
    service: GitService,
    workspace: VcsWorkspace,
}

/// Fully connected Files surface. Filesystem and VCS work is dispatched separately.
#[derive(Debug)]
pub struct FilesRuntime {
    model: FilesModel,
    root: PathBuf,
    background_active: bool,
    git: Option<GitRefresh>,
    expanded: BTreeSet<PathBuf>,
    desired_expanded: BTreeSet<PathBuf>,
    visible_rows: Vec<PathBuf>,
    viewport_offset: usize,
    viewport_height: usize,
    viewport_y: u16,
    filesystem_generation: u64,
    filesystem_applied_generation: u64,
    filesystem_running: Option<u64>,
    filesystem_expansions_running: BTreeSet<PathBuf>,
    filesystem_reload_running: bool,
    pending_expansions: BTreeSet<PathBuf>,
    reload_pending: bool,
    filesystem_panic_retried: bool,
    filesystem_notice: Option<String>,
}

impl FilesRuntime {
    pub fn bootstrap(context: &LaunchContext) -> Result<Self, FilesRuntimeError> {
        let opening_directory = context.foreground_cwd().unwrap_or_else(|| context.cwd());
        let project = resolve_project_context(opening_directory)
            .map_err(|error| FilesRuntimeError::new(error.to_string()))?;
        let mut model = match project.vcs() {
            Some(vcs) => FilesModel::for_workspace(
                project.files_root().to_path_buf(),
                vcs.workspace_root().to_path_buf(),
            )?,
            None => FilesModel::new(project.files_root().to_path_buf())?,
        };
        model.load_directory(Path::new(""))?;

        let git = project
            .vcs()
            .filter(|vcs| vcs.backend().as_str() == "git")
            .map(|vcs| {
                let workspace = VcsWorkspace::new(
                    vcs.workspace_root().to_path_buf(),
                    VcsBackendMetadata::new("git", "Git", false)
                        .map_err(|error| FilesRuntimeError::new(error.to_string()))?,
                )
                .map_err(|error| FilesRuntimeError::new(error.to_string()))?;
                Ok::<_, FilesRuntimeError>(GitRefresh {
                    service: GitService::default(),
                    workspace,
                })
            })
            .transpose()?;

        let mut runtime = Self {
            model,
            root: project.files_root().to_path_buf(),
            background_active: true,
            git,
            expanded: BTreeSet::new(),
            desired_expanded: BTreeSet::new(),
            visible_rows: Vec::new(),
            viewport_offset: 0,
            viewport_height: usize::MAX,
            viewport_y: 0,
            filesystem_generation: 0,
            filesystem_applied_generation: 0,
            filesystem_running: None,
            filesystem_reload_running: false,
            filesystem_expansions_running: BTreeSet::new(),
            pending_expansions: BTreeSet::new(),
            reload_pending: false,
            filesystem_panic_retried: false,
            filesystem_notice: None,
        };
        runtime.rebuild_visible_rows();
        Ok(runtime)
    }

    pub fn render(&mut self, area: Rect, buffer: &mut Buffer) {
        let notice_height =
            usize::from(self.filesystem_notice.is_some() || self.model.failure_notice().is_some());
        self.viewport_y = area.y;
        self.viewport_height = usize::from(area.height).saturating_sub(notice_height);
        self.ensure_selection_visible();
        let end = self
            .viewport_offset
            .saturating_add(self.viewport_height)
            .min(self.visible_rows.len());
        let rows = &self.visible_rows[self.viewport_offset.min(end)..end];
        let notice = self
            .filesystem_notice
            .as_deref()
            .map(|message| ("Files", message))
            .or_else(|| self.model.failure_notice().map(|message| ("VCS", message)));
        FilesView::new(
            self.model.tree(),
            rows,
            self.model.tree().selection(),
            notice,
        )
        .render(area, buffer);
    }

    pub(crate) fn handle_intent(&mut self, intent: &Intent, workers: &mut WorkerRuntime) -> bool {
        match intent {
            Intent::SelectPrevious => self.move_selection(-1),
            Intent::SelectNext => self.move_selection(1),
            Intent::SelectFirst => self.select_index(0),
            Intent::SelectLast => self.select_index(self.visible_rows.len().saturating_sub(1)),
            Intent::ExpandOrDescend => self.expand_or_descend(workers),
            Intent::CollapseOrAscend => self.collapse_or_ascend(),
            Intent::ToggleSelected => self.toggle_selected(workers),
            Intent::Refresh => self.request_reload(workers),
            Intent::Pointer { row, action, .. } => match action {
                PointerAction::Select => self.select_viewport_row(*row),
                PointerAction::Toggle => self.toggle_viewport_row(*row, workers),
            },
            Intent::Scroll(delta) => self.move_selection(isize::from(*delta)),
            Intent::Resize => true,
            Intent::Quit | Intent::SwitchView(_) | Intent::NextView | Intent::PreviousView => false,
        }
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
        self.model.tree().selection()
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

    fn move_selection(&mut self, delta: isize) -> bool {
        let Some(current) = self
            .model
            .tree()
            .selection()
            .and_then(|selected| self.visible_rows.iter().position(|path| path == selected))
        else {
            return self.select_index(0);
        };
        let last = self.visible_rows.len().saturating_sub(1);
        let next = if delta.is_negative() {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current.saturating_add(delta as usize).min(last)
        };
        self.select_index(next)
    }

    fn select_index(&mut self, index: usize) -> bool {
        let Some(path) = self.visible_rows.get(index).cloned() else {
            return false;
        };
        if self.model.tree().selection() == Some(path.as_path()) {
            return false;
        }
        let selected = self.model.select(&path);
        if selected {
            self.ensure_selection_visible();
        }
        selected
    }

    fn select_viewport_row(&mut self, terminal_row: u16) -> bool {
        let Some(index) = self.viewport_index(terminal_row) else {
            return false;
        };
        self.select_index(index)
    }

    fn toggle_viewport_row(&mut self, terminal_row: u16, workers: &mut WorkerRuntime) -> bool {
        let Some(index) = self.viewport_index(terminal_row) else {
            return false;
        };
        let selection_changed = self.select_index(index);
        self.toggle_selected(workers) || selection_changed
    }

    fn viewport_index(&self, terminal_row: u16) -> Option<usize> {
        let row = terminal_row.checked_sub(self.viewport_y).map(usize::from)?;
        if row >= self.viewport_height {
            return None;
        }
        let index = self.viewport_offset.saturating_add(row);
        (index < self.visible_rows.len()).then_some(index)
    }

    fn expand_or_descend(&mut self, workers: &mut WorkerRuntime) -> bool {
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
            self.rebuild_visible_rows();
        }
        self.start_next_filesystem(workers);
        true
    }

    fn request_vcs_refresh(&mut self, workers: &mut WorkerRuntime) -> bool {
        if self.git.is_none() {
            return false;
        }
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
        let Some(git) = self.git.clone() else {
            self.model.cancel_refresh_start(generation);
            return;
        };
        let input = self.model.status_merge_input();
        let key = JobKey::new(JobKind::Vcs, git.workspace.root());
        let job = Job::new(key, generation, Priority::High, move |cancelled| {
            let snapshot = git
                .service
                .refresh_status_cancellable(&git.workspace, cancelled);
            Box::new(RuntimeMessage::Vcs(PreparedRefreshResult::prepare(
                generation, input, snapshot,
            )))
        });
        if !matches!(
            workers.submit(job),
            SubmitStatus::Queued | SubmitStatus::Coalesced
        ) {
            self.model.cancel_refresh_start(generation);
            self.filesystem_notice = Some("background VCS queue is unavailable".to_owned());
        }
    }

    fn complete_vcs_refresh(
        &mut self,
        result: PreparedRefreshResult,
        workers: &mut WorkerRuntime,
    ) -> bool {
        let changed = self.model.complete_prepared_refresh(result);
        if changed {
            self.rebuild_visible_rows();
        }
        self.start_next_vcs_refresh(workers);
        true
    }

    fn rebuild_visible_rows(&mut self) {
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
            .model
            .tree()
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

    pub(crate) fn has_pending_work(&self) -> bool {
        self.filesystem_running.is_some()
            || self.model.refresh_is_running()
            || (self.background_active
                && (self.reload_pending || !self.pending_expansions.is_empty()))
    }

    pub(crate) fn start_background(&mut self, workers: &mut WorkerRuntime) {
        self.background_active = true;
        self.request_vcs_refresh(workers);
        self.retry_pending(workers);
    }

    pub(crate) const fn pause_background(&mut self) {
        self.background_active = false;
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
        }
    }

    pub(crate) fn retry_pending(&mut self, workers: &mut WorkerRuntime) {
        self.start_next_filesystem(workers);
        self.start_next_vcs_refresh(workers);
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
            JobKind::Bootstrap
            | JobKind::ConversationDiscovery
            | JobKind::Process
            | JobKind::Filesystem => {}
        }
        if kind != JobKind::Filesystem || self.filesystem_panic_retried {
            self.retry_pending(workers);
        }
    }
}

type DirectoryLoadResult = (PathBuf, io::Result<DirectorySnapshot>);

#[derive(Debug)]
pub(crate) enum RuntimeMessage {
    Filesystem {
        generation: u64,
        expansions: BTreeSet<PathBuf>,
        result: Vec<DirectoryLoadResult>,
    },
    Vcs(PreparedRefreshResult),
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
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use tempfile::TempDir;

    use super::{FilesRuntime, RuntimeMessage};
    use crate::host::LaunchContext;
    use crate::vcs::{VcsEntryStatus, VcsStatusKind, VcsStatusSnapshot};
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
        let area = Rect::new(0, 0, 20, 2);
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
        assert_eq!(runtime.model.tree().selection(), Some(Path::new("root.rs")));

        runtime.handle_event(
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Right),
                column: 0,
                row: 0,
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
        let area = Rect::new(0, 0, 10, 2);
        let mut buffer = Buffer::empty(area);
        runtime.render(area, &mut buffer);

        runtime.handle_event(press(KeyCode::End), &mut workers);
        let cached_rows = runtime.visible_rows.as_ptr();
        runtime.render(area, &mut buffer);

        assert_eq!(runtime.visible_rows.as_ptr(), cached_rows);
        assert_eq!(runtime.viewport_offset, 1);
        let second_line = (0..area.width)
            .map(|x| buffer[(x, 1)].symbol())
            .collect::<String>();
        assert!(second_line.contains('c'));
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
}
