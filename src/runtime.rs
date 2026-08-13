use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::Duration;

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton,
    MouseEventKind,
};
use crossterm::execute;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::{Paragraph, Widget};

use crate::files::tree::{DirectorySnapshot, TreeNodeKind};
use crate::files::{FilesModel, PreparedRefreshResult};
use crate::host::LaunchContext;
use crate::project::resolve_project_context;
use crate::ui::files::FilesView;
use crate::vcs::git::GitService;
use crate::vcs::{VcsBackendMetadata, VcsWorkspace};

const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Clone, Debug)]
struct GitRefresh {
    service: GitService,
    workspace: VcsWorkspace,
}

/// Fully connected Files surface. Filesystem and VCS work is dispatched separately.
#[derive(Debug)]
pub struct FilesRuntime {
    model: FilesModel,
    git: Option<GitRefresh>,
    expanded: BTreeSet<PathBuf>,
    desired_expanded: BTreeSet<PathBuf>,
    visible_rows: Vec<PathBuf>,
    viewport_offset: usize,
    viewport_height: usize,
    viewport_y: u16,
    filesystem_generation: u64,
    filesystem_running: Option<u64>,
    filesystem_reload_running: bool,
    pending_expansions: BTreeSet<PathBuf>,
    reload_pending: bool,
    filesystem_notice: Option<String>,
    vcs_cancel: Option<Arc<AtomicBool>>,
    vcs_worker: Option<thread::JoinHandle<()>>,
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
            git,
            expanded: BTreeSet::new(),
            desired_expanded: BTreeSet::new(),
            visible_rows: Vec::new(),
            viewport_offset: 0,
            viewport_height: usize::MAX,
            viewport_y: 0,
            filesystem_generation: 0,
            filesystem_running: None,
            filesystem_reload_running: false,
            pending_expansions: BTreeSet::new(),
            reload_pending: false,
            filesystem_notice: None,
            vcs_cancel: None,
            vcs_worker: None,
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

    fn handle_event(&mut self, input: Event, sender: &Sender<RuntimeMessage>) -> EventOutcome {
        match input {
            Event::Key(key) if key.kind == KeyEventKind::Press => self.handle_key(key.code, sender),
            Event::Mouse(mouse) => {
                let redraw = match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => self.select_viewport_row(mouse.row),
                    MouseEventKind::Down(MouseButton::Right) => {
                        self.toggle_viewport_row(mouse.row, sender)
                    }
                    MouseEventKind::ScrollUp => self.move_selection(-1),
                    MouseEventKind::ScrollDown => self.move_selection(1),
                    _ => false,
                };
                EventOutcome {
                    redraw,
                    quit: false,
                }
            }
            Event::Resize(_, _) => EventOutcome {
                redraw: true,
                quit: false,
            },
            _ => EventOutcome::default(),
        }
    }

    fn handle_key(&mut self, code: KeyCode, sender: &Sender<RuntimeMessage>) -> EventOutcome {
        let (redraw, quit) = match code {
            KeyCode::Esc | KeyCode::Char('q') => (false, true),
            KeyCode::Up | KeyCode::Char('k') => (self.move_selection(-1), false),
            KeyCode::Down | KeyCode::Char('j') => (self.move_selection(1), false),
            KeyCode::Home => (self.select_index(0), false),
            KeyCode::End => (
                self.select_index(self.visible_rows.len().saturating_sub(1)),
                false,
            ),
            KeyCode::Right | KeyCode::Char('l') => (self.expand_or_descend(sender), false),
            KeyCode::Left | KeyCode::Char('h') => (self.collapse_or_ascend(), false),
            KeyCode::Enter | KeyCode::Char(' ') => (self.toggle_selected(sender), false),
            KeyCode::Char('r') => (self.request_reload(sender), false),
            _ => (false, false),
        };
        EventOutcome { redraw, quit }
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

    fn toggle_viewport_row(&mut self, terminal_row: u16, sender: &Sender<RuntimeMessage>) -> bool {
        let Some(index) = self.viewport_index(terminal_row) else {
            return false;
        };
        let selection_changed = self.select_index(index);
        self.toggle_selected(sender) || selection_changed
    }

    fn viewport_index(&self, terminal_row: u16) -> Option<usize> {
        let row = terminal_row.checked_sub(self.viewport_y).map(usize::from)?;
        if row >= self.viewport_height {
            return None;
        }
        let index = self.viewport_offset.saturating_add(row);
        (index < self.visible_rows.len()).then_some(index)
    }

    fn expand_or_descend(&mut self, sender: &Sender<RuntimeMessage>) -> bool {
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
        self.request_expansion(path, sender)
    }

    fn toggle_selected(&mut self, sender: &Sender<RuntimeMessage>) -> bool {
        let Some(path) = self.model.tree().selection().map(Path::to_path_buf) else {
            return false;
        };
        if self.desired_expanded.remove(&path) {
            self.invalidate_filesystem();
            self.pending_expansions.remove(&path);
            if self.expanded.remove(&path) {
                self.rebuild_visible_rows();
            }
            return true;
        }
        self.request_expansion(path, sender)
    }

    fn collapse_or_ascend(&mut self) -> bool {
        let Some(path) = self.model.tree().selection().map(Path::to_path_buf) else {
            return false;
        };
        if self.desired_expanded.remove(&path) {
            self.invalidate_filesystem();
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

    fn request_expansion(&mut self, path: PathBuf, sender: &Sender<RuntimeMessage>) -> bool {
        if self.model.tree().node(&path).map(|node| node.kind()) != Some(TreeNodeKind::Directory) {
            return false;
        }
        let desired = self.desired_expanded.insert(path.clone());
        if desired {
            self.invalidate_filesystem();
            self.pending_expansions.insert(path);
            self.start_next_filesystem(sender);
        }
        desired
    }

    fn request_reload(&mut self, sender: &Sender<RuntimeMessage>) -> bool {
        self.invalidate_filesystem();
        self.reload_pending = true;
        self.start_next_filesystem(sender);
        self.request_vcs_refresh(sender);
        true
    }

    fn start_next_filesystem(&mut self, sender: &Sender<RuntimeMessage>) {
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
        self.filesystem_running = Some(generation);
        self.filesystem_reload_running = reload;
        let loader = self.model.tree().directory_loader();
        let sender = sender.clone();
        thread::spawn(move || {
            let result = directories
                .into_iter()
                .map(|directory| {
                    let result = loader.load(directory.clone());
                    (directory, result)
                })
                .collect();
            let _ = sender.send(RuntimeMessage::Filesystem {
                generation,
                expansions,
                result,
            });
        });
    }

    fn complete_filesystem(
        &mut self,
        generation: u64,
        expansions: BTreeSet<PathBuf>,
        result: Vec<DirectoryLoadResult>,
        sender: &Sender<RuntimeMessage>,
    ) -> bool {
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
            self.start_next_filesystem(sender);
            return false;
        }

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
        self.start_next_filesystem(sender);
        true
    }

    fn request_vcs_refresh(&mut self, sender: &Sender<RuntimeMessage>) -> bool {
        if self.git.is_none() {
            return false;
        }
        self.model.request_refresh();
        self.start_next_vcs_refresh(sender);
        true
    }

    const fn invalidate_filesystem(&mut self) {
        self.filesystem_generation = self.filesystem_generation.saturating_add(1);
    }

    fn start_next_vcs_refresh(&mut self, sender: &Sender<RuntimeMessage>) {
        let Some(generation) = self.model.begin_refresh() else {
            return;
        };
        let Some(git) = self.git.clone() else {
            return;
        };
        let input = self.model.status_merge_input();
        let cancelled = Arc::new(AtomicBool::new(false));
        self.vcs_cancel = Some(Arc::clone(&cancelled));
        let sender = sender.clone();
        self.vcs_worker = Some(thread::spawn(move || {
            let snapshot = git
                .service
                .refresh_status_cancellable(&git.workspace, &cancelled);
            let result = PreparedRefreshResult::prepare(generation, input, snapshot);
            let _ = sender.send(RuntimeMessage::Vcs(result));
        }));
    }

    fn complete_vcs_refresh(
        &mut self,
        result: PreparedRefreshResult,
        sender: &Sender<RuntimeMessage>,
    ) -> bool {
        if let Some(worker) = self.vcs_worker.take() {
            let _ = worker.join();
        }
        self.vcs_cancel = None;
        let changed = self.model.complete_prepared_refresh(result);
        if changed {
            self.rebuild_visible_rows();
        }
        self.start_next_vcs_refresh(sender);
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

    const fn has_pending_work(&self) -> bool {
        self.filesystem_running.is_some() || self.model.refresh_is_running()
    }

    fn shutdown_workers(&mut self) {
        if let Some(cancelled) = &self.vcs_cancel {
            cancelled.store(true, Ordering::Relaxed);
        }
        if let Some(worker) = self.vcs_worker.take() {
            let _ = worker.join();
        }
        self.vcs_cancel = None;
    }
}

type DirectoryLoadResult = (PathBuf, io::Result<DirectorySnapshot>);

#[derive(Debug)]
enum RuntimeMessage {
    Filesystem {
        generation: u64,
        expansions: BTreeSet<PathBuf>,
        result: Vec<DirectoryLoadResult>,
    },
    Vcs(PreparedRefreshResult),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct EventOutcome {
    redraw: bool,
    quit: bool,
}

/// Draws immediately, then bootstraps filesystem and Git work on workers.
pub fn run_files_terminal(context: LaunchContext) -> Result<(), FilesRuntimeError> {
    let (sender, receiver) = mpsc::channel();
    let (bootstrap_sender, bootstrap_receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = bootstrap_sender.send(FilesRuntime::bootstrap(&context));
    });

    let _mouse_capture = MouseCapture::enable()?;
    ratatui::run(|terminal| {
        terminal.draw(|frame| {
            frame.render_widget(Paragraph::new("Loading Files…"), frame.area());
        })?;
        let mut runtime = None;
        let mut startup_error = None;
        let mut bootstrap_pending = true;
        let mut redraw = false;
        loop {
            if bootstrap_pending {
                match bootstrap_receiver.try_recv() {
                    Ok(result) => {
                        bootstrap_pending = false;
                        match result {
                            Ok(mut ready) => {
                                ready.request_vcs_refresh(&sender);
                                runtime = Some(ready);
                            }
                            Err(error) => startup_error = Some(error.to_string()),
                        }
                        redraw = true;
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                    Err(mpsc::TryRecvError::Disconnected) => {
                        bootstrap_pending = false;
                        startup_error = Some("Files worker stopped unexpectedly".to_owned());
                        redraw = true;
                    }
                }
            }
            while let Ok(message) = receiver.try_recv() {
                redraw = true;
                match message {
                    RuntimeMessage::Filesystem {
                        generation,
                        expansions,
                        result,
                    } => {
                        if let Some(runtime) = &mut runtime {
                            runtime.complete_filesystem(generation, expansions, result, &sender);
                        }
                    }
                    RuntimeMessage::Vcs(result) => {
                        if let Some(runtime) = &mut runtime {
                            runtime.complete_vcs_refresh(result, &sender);
                        }
                    }
                }
            }

            if redraw {
                terminal.draw(|frame| {
                    if let Some(runtime) = &mut runtime {
                        runtime.render(frame.area(), frame.buffer_mut());
                    } else if let Some(error) = &startup_error {
                        frame.render_widget(Paragraph::new(error.as_str()), frame.area());
                    } else {
                        frame.render_widget(Paragraph::new("Loading Files…"), frame.area());
                    }
                })?;
                redraw = false;
            }

            let worker_pending =
                bootstrap_pending || runtime.as_ref().is_some_and(FilesRuntime::has_pending_work);
            let input = if worker_pending {
                event::poll(WORKER_POLL_INTERVAL)?
                    .then(event::read)
                    .transpose()?
            } else {
                Some(event::read()?)
            };
            if let Some(input) = input {
                if let Some(runtime) = &mut runtime {
                    let outcome = runtime.handle_event(input, &sender);
                    redraw |= outcome.redraw;
                    if outcome.quit {
                        runtime.shutdown_workers();
                        return Ok::<(), io::Error>(());
                    }
                } else if let Event::Key(key) = input
                    && key.kind == KeyEventKind::Press
                    && matches!(key.code, KeyCode::Esc | KeyCode::Char('q'))
                {
                    return Ok(());
                }
            }
        }
    })
    .map_err(FilesRuntimeError::from)
}

struct MouseCapture;

impl MouseCapture {
    fn enable() -> io::Result<Self> {
        execute!(io::stdout(), EnableMouseCapture)?;
        Ok(Self)
    }
}

impl Drop for MouseCapture {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), DisableMouseCapture);
    }
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
    use std::sync::mpsc;
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

    #[test]
    fn navigates_expands_and_collapses_without_eagerly_loading_descendants() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join("src")).expect("src");
        fs::write(temp.path().join("src/child.rs"), []).expect("child");
        fs::write(temp.path().join("root.rs"), []).expect("root file");
        let mut runtime = runtime(&temp);
        let (sender, receiver) = mpsc::channel();

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

        let outcome = runtime.handle_event(press(KeyCode::Right), &sender);
        assert!(outcome.redraw);
        let RuntimeMessage::Filesystem {
            generation,
            expansions,
            result,
        } = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("filesystem result")
        else {
            panic!("unexpected worker result");
        };

        assert!(runtime.complete_filesystem(generation, expansions, result, &sender));
        assert_eq!(
            runtime.visible_rows,
            [
                Path::new("src"),
                Path::new("src/child.rs"),
                Path::new("root.rs")
            ]
        );

        runtime.handle_event(press(KeyCode::Down), &sender);
        assert_eq!(
            runtime.model.tree().selection(),
            Some(Path::new("src/child.rs"))
        );
        runtime.handle_event(press(KeyCode::Left), &sender);
        assert_eq!(runtime.model.tree().selection(), Some(Path::new("src")));
        runtime.handle_event(press(KeyCode::Left), &sender);
        assert_eq!(
            runtime.visible_rows,
            [Path::new("src"), Path::new("root.rs")]
        );
    }
    #[test]
    fn second_toggle_cancels_an_expansion_still_in_flight() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join("src")).expect("src");
        fs::write(temp.path().join("src/child.rs"), []).expect("child");
        let mut runtime = runtime(&temp);
        let (sender, receiver) = mpsc::channel();

        runtime.handle_event(press(KeyCode::Enter), &sender);
        runtime.handle_event(press(KeyCode::Enter), &sender);
        let RuntimeMessage::Filesystem {
            generation,
            expansions,
            result,
        } = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("filesystem result")
        else {
            panic!("unexpected worker result");
        };
        runtime.complete_filesystem(generation, expansions, result, &sender);

        assert_eq!(runtime.visible_rows, [Path::new("src")]);
        assert!(runtime.expanded.is_empty());
    }

    #[test]
    fn manual_refresh_reloads_the_filesystem_on_a_worker() {
        let temp = TempDir::new().expect("tempdir");
        fs::write(temp.path().join("initial"), []).expect("initial");
        let mut runtime = runtime(&temp);
        fs::write(temp.path().join("added-later"), []).expect("added later");
        let (sender, receiver) = mpsc::channel();

        assert!(
            runtime
                .handle_event(press(KeyCode::Char('r')), &sender)
                .redraw
        );
        let RuntimeMessage::Filesystem {
            generation,
            expansions,
            result,
        } = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("filesystem refresh")
        else {
            panic!("unexpected worker result");
        };
        assert!(runtime.complete_filesystem(generation, expansions, result, &sender));

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
        let (sender, receiver) = mpsc::channel();

        runtime.handle_event(press(KeyCode::Right), &sender);
        let RuntimeMessage::Filesystem {
            generation,
            expansions,
            result,
        } = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("expansion")
        else {
            panic!("unexpected worker result");
        };
        runtime.complete_filesystem(generation, expansions, result, &sender);
        fs::remove_dir_all(temp.path().join("removed")).expect("remove directory");

        runtime.handle_event(press(KeyCode::Char('r')), &sender);
        let RuntimeMessage::Filesystem {
            generation,
            expansions,
            result,
        } = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("filesystem refresh")
        else {
            panic!("unexpected worker result");
        };
        runtime.complete_filesystem(generation, expansions, result, &sender);

        assert!(runtime.model.tree().node(Path::new("removed")).is_none());
        assert!(runtime.filesystem_notice.is_none());
    }

    #[test]
    fn mouse_selects_scrolls_and_requests_directory_expansion() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join("src")).expect("src");

        fs::write(temp.path().join("root.rs"), []).expect("root file");
        let mut runtime = runtime(&temp);

        let (sender, receiver) = mpsc::channel();
        let area = Rect::new(0, 0, 20, 2);
        runtime.render(area, &mut Buffer::empty(area));

        runtime.handle_event(
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 0,
                row: 1,
                modifiers: KeyModifiers::NONE,
            }),
            &sender,
        );
        assert_eq!(runtime.model.tree().selection(), Some(Path::new("root.rs")));

        runtime.handle_event(
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Right),
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            }),
            &sender,
        );
        assert_eq!(runtime.model.tree().selection(), Some(Path::new("src")));
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(1)),
            Ok(RuntimeMessage::Filesystem { .. })
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
        let (sender, _receiver) = mpsc::channel();
        runtime.filesystem_running = Some(1);
        runtime.filesystem_generation = 1;

        runtime.complete_filesystem(
            1,
            BTreeSet::new(),
            vec![
                (PathBuf::new(), Ok(current_root)),
                (PathBuf::from("removed"), Ok(stale_descendant)),
            ],
            &sender,
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
        let (sender, receiver) = mpsc::channel();
        let area = Rect::new(0, 0, 20, 3);
        runtime.render(area, &mut Buffer::empty(area));

        let outcome = runtime.handle_event(
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Right),
                column: 0,
                row: 2,
                modifiers: KeyModifiers::NONE,
            }),
            &sender,
        );

        assert!(!outcome.redraw);
        assert!(runtime.expanded.is_empty());
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn renders_only_the_cached_viewport_and_scrolls_selection_into_view() {
        let temp = TempDir::new().expect("tempdir");
        for name in ["a", "b", "c"] {
            fs::write(temp.path().join(name), []).expect("file");
        }
        let mut runtime = runtime(&temp);
        let (sender, _receiver) = mpsc::channel();
        let area = Rect::new(0, 0, 10, 2);
        let mut buffer = Buffer::empty(area);
        runtime.render(area, &mut buffer);

        runtime.handle_event(press(KeyCode::End), &sender);
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
        let (sender, _receiver) = mpsc::channel();

        assert!(!runtime.handle_event(Event::FocusGained, &sender).redraw);
        assert!(runtime.handle_event(Event::Resize(80, 24), &sender).redraw);
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
        let (sender, receiver) = mpsc::channel();

        runtime.handle_event(press(KeyCode::Right), &sender);
        let RuntimeMessage::Filesystem {
            generation,
            expansions,
            result,
        } = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("filesystem result")
        else {
            panic!("unexpected worker result");
        };
        runtime.complete_filesystem(generation, expansions, result, &sender);
        runtime.handle_event(press(KeyCode::Right), &sender);

        assert_eq!(
            runtime.model.tree().selection(),
            Some(Path::new("src/removed/missing.rs"))
        );
        runtime.handle_event(press(KeyCode::Left), &sender);
        assert_eq!(runtime.model.tree().selection(), Some(Path::new("src")));
    }

    #[test]
    fn failed_expansion_can_be_retried_with_right() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join("src")).expect("src");
        let mut runtime = runtime(&temp);
        let (sender, receiver) = mpsc::channel();
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
            &sender,
        );
        runtime.handle_event(press(KeyCode::Right), &sender);

        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(1)),
            Ok(RuntimeMessage::Filesystem { .. })
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
        let (sender, _receiver) = mpsc::channel();

        runtime.complete_filesystem(
            1,
            BTreeSet::new(),
            vec![(PathBuf::from("src"), Ok(snapshot))],
            &sender,
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
        let (sender, receiver) = mpsc::channel();
        runtime.request_expansion(PathBuf::from("src"), &sender);
        let RuntimeMessage::Filesystem {
            generation,
            expansions,
            ..
        } = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("first filesystem result")
        else {
            panic!("unexpected worker result");
        };
        runtime.request_reload(&sender);

        assert!(!runtime.complete_filesystem(
            generation,
            expansions,
            vec![(
                PathBuf::from("src"),
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "stale")),
            )],
            &sender,
        ));
        assert!(runtime.filesystem_notice.is_none());

        let RuntimeMessage::Filesystem {
            generation,
            expansions,
            result,
        } = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("replacement filesystem result")
        else {
            panic!("unexpected worker result");
        };
        assert!(runtime.complete_filesystem(generation, expansions, result, &sender));
    }
}
