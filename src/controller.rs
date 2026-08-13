//! Intent transitions and orchestration between state and bounded workers.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::conversations::Conversation;
use crate::conversations::discovery::discover_conversations;
use crate::conversations::sources::{
    DiscoveryLimit, GenericJsonlSource, MetadataBudget, SourceRegistry,
};
use crate::host::LaunchContext;
use crate::intent::{Intent, View};
use crate::model::{AppModel, LoadingState};
use crate::project::resolve_project_context;
use crate::runtime::{FilesRuntime, RuntimeMessage};
use crate::ui::render_shell;
use crate::worker::{CompletedJob, Job, JobKey, JobKind, Priority, SubmitStatus, WorkerRuntime};

struct BootstrapResult(Result<FilesRuntime, crate::runtime::FilesRuntimeError>);
struct ConversationsResult(ConversationJobResult);

enum ConversationJobResult {
    Ready(Vec<Conversation>),
    Cancelled,
    Error(&'static str),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Transition {
    pub(super) dirty: bool,
    pub(super) quit: bool,
}

pub struct Controller {
    model: AppModel,
    files: Option<FilesRuntime>,
}

impl Controller {
    pub(super) fn new(context: LaunchContext) -> Self {
        Self {
            model: AppModel::new(context),
            files: None,
        }
    }

    pub(super) fn start(&mut self, workers: &mut WorkerRuntime) {
        let context = self.model.launch_context().clone();
        let root = context
            .foreground_cwd()
            .unwrap_or_else(|| context.cwd())
            .to_path_buf();
        let bootstrap = Job::new(
            JobKey::new(JobKind::Bootstrap, root),
            1,
            Priority::High,
            move |cancelled| {
                Box::new(BootstrapResult(FilesRuntime::bootstrap_cancellable(
                    &context, cancelled,
                )))
            },
        );
        if !accepted(workers.submit(bootstrap)) {
            self.model.files_mut().set_loading(LoadingState::Error(
                "background bootstrap queue is unavailable".to_owned(),
            ));
        }
        self.schedule_conversations(workers);
    }

    fn schedule_conversations(&mut self, workers: &mut WorkerRuntime) {
        let (requested, applied) = self.model.conversations().generations();
        let generation = requested.saturating_add(1);
        self.model
            .conversations_mut()
            .set_generations(generation, applied);
        self.model
            .conversations_mut()
            .set_loading(LoadingState::Loading);
        let root = self
            .model
            .launch_context()
            .foreground_cwd()
            .unwrap_or_else(|| self.model.launch_context().cwd())
            .to_path_buf();
        let job = Job::new(
            JobKey::new(JobKind::ConversationDiscovery, &root),
            generation,
            Priority::Low,
            move |cancelled| Box::new(ConversationsResult(load_conversations(&root, cancelled))),
        );
        if !accepted(workers.submit(job)) {
            self.model
                .conversations_mut()
                .set_loading(LoadingState::Error(
                    "background conversation queue is unavailable".to_owned(),
                ));
        }
    }

    pub(super) fn apply(&mut self, intent: Intent, workers: &mut WorkerRuntime) -> Transition {
        match intent {
            Intent::Quit => Transition {
                dirty: false,
                quit: true,
            },
            Intent::SwitchView(view) => Transition {
                dirty: self.switch_view(view, workers),
                quit: false,
            },
            Intent::NextView => Transition {
                dirty: self.switch_view(self.model.active_view().next(), workers),
                quit: false,
            },
            Intent::PreviousView => Transition {
                dirty: self.switch_view(self.model.active_view().previous(), workers),
                quit: false,
            },
            Intent::Resize => Transition {
                dirty: true,
                quit: false,
            },
            Intent::Refresh if self.model.active_view() == View::Conversations => {
                self.schedule_conversations(workers);
                Transition {
                    dirty: true,
                    quit: false,
                }
            }
            intent if self.model.active_view() == View::Files => {
                let dirty = self
                    .files
                    .as_mut()
                    .is_some_and(|files| files.handle_intent(&intent, workers));
                self.sync_files_state();
                Transition { dirty, quit: false }
            }
            _ => Transition::default(),
        }
    }

    fn switch_view(&mut self, view: View, workers: &mut WorkerRuntime) -> bool {
        if self.model.active_view() == view {
            return false;
        }
        self.model.set_active_view(view);
        if let Some(files) = &mut self.files {
            if view == View::Files {
                files.start_background(workers);
            } else {
                files.pause_background();
            }
        }
        true
    }

    pub(super) fn apply_result(
        &mut self,
        result: CompletedJob,
        workers: &mut WorkerRuntime,
    ) -> bool {
        if result.panicked() {
            let kind = result.key().kind();
            let generation = result.generation();
            if matches!(kind, JobKind::Filesystem | JobKind::Vcs)
                && let Some(files) = &mut self.files
            {
                files.fail_background(kind, generation, workers);
            }
            if !matches!(kind, JobKind::Filesystem | JobKind::Vcs) {
                self.set_worker_error(kind);
            }
            self.retry_files(workers);
            return true;
        }
        match result.key().kind() {
            JobKind::Bootstrap => {
                let Ok(result) = result.downcast::<BootstrapResult>() else {
                    self.model.files_mut().set_loading(LoadingState::Error(
                        "invalid bootstrap worker result".to_owned(),
                    ));
                    return true;
                };
                match result.0 {
                    Ok(mut files) => {
                        if self.model.active_view() == View::Files {
                            files.start_background(workers);
                        }
                        self.files = Some(files);
                        self.model.files_mut().set_loading(LoadingState::Ready);
                        self.sync_files_state();
                    }
                    Err(error) => self
                        .model
                        .files_mut()
                        .set_loading(LoadingState::Error(error.to_string())),
                }
                true
            }
            JobKind::ConversationDiscovery => {
                let generation = result.generation();
                let requested = self.model.conversations().generations().0;
                if generation < requested {
                    return false;
                }
                let Ok(result) = result.downcast::<ConversationsResult>() else {
                    self.model
                        .conversations_mut()
                        .set_loading(LoadingState::Error(
                            "invalid conversation worker result".to_owned(),
                        ));
                    return true;
                };
                match result.0 {
                    ConversationJobResult::Ready(conversations) => self
                        .model
                        .conversations_mut()
                        .replace_items(conversations, generation),
                    ConversationJobResult::Cancelled => {
                        self.model
                            .conversations_mut()
                            .set_generations(requested, generation);
                        self.model
                            .conversations_mut()
                            .set_loading(LoadingState::Ready);
                        true
                    }
                    ConversationJobResult::Error(message) => {
                        self.model
                            .conversations_mut()
                            .set_loading(LoadingState::Error(message.to_owned()));
                        true
                    }
                }
            }
            JobKind::Filesystem | JobKind::Vcs => {
                let Ok(message) = result.downcast::<RuntimeMessage>() else {
                    self.model.files_mut().set_loading(LoadingState::Error(
                        "invalid Files worker result".to_owned(),
                    ));
                    return true;
                };
                let changed = self.files.as_mut().is_some_and(|files| {
                    let changed = files.complete_background(*message, workers);
                    files.retry_pending(workers);
                    changed
                });
                self.sync_files_state();
                changed
            }
            JobKind::Process => {
                self.retry_files(workers);
                false
            }
        }
    }

    fn set_worker_error(&mut self, kind: JobKind) {
        let state = LoadingState::Error("background worker stopped unexpectedly".to_owned());
        match kind {
            JobKind::ConversationDiscovery => self.model.conversations_mut().set_loading(state),
            JobKind::Bootstrap | JobKind::Filesystem | JobKind::Vcs | JobKind::Process => {
                self.model.files_mut().set_loading(state);
            }
        }
    }

    fn retry_files(&mut self, workers: &mut WorkerRuntime) {
        if let Some(files) = &mut self.files {
            files.retry_pending(workers);
        }
    }

    fn sync_files_state(&mut self) {
        let Some(files) = &self.files else {
            return;
        };
        self.model
            .files_mut()
            .set_selection(files.selection().map(PathBuf::from));
        self.model.files_mut().set_scroll(files.scroll());
        let (requested, applied) = files.generations();
        self.model.files_mut().set_generations(requested, applied);
    }

    pub(super) fn render(&mut self, area: Rect, buffer: &mut Buffer) {
        render_shell(&mut self.model, area, buffer);
        if self.model.active_view() == View::Files
            && matches!(self.model.files().loading(), LoadingState::Ready)
            && let Some(files) = &mut self.files
        {
            files.render(self.model.geometry().content(), buffer);
            self.sync_files_state();
        }
    }

    pub(super) const fn model(&self) -> &AppModel {
        &self.model
    }

    pub(super) const fn model_mut(&mut self) -> &mut AppModel {
        &mut self.model
    }

    pub(super) fn files_have_pending_work(&self) -> bool {
        self.files
            .as_ref()
            .is_some_and(FilesRuntime::has_pending_work)
    }
}

fn load_conversations(root: &Path, cancelled: &AtomicBool) -> ConversationJobResult {
    if cancelled.load(Ordering::Relaxed) {
        return ConversationJobResult::Cancelled;
    }
    let project = match resolve_project_context(root) {
        Ok(context) => context.conversation_identity().clone(),
        Err(_) => return ConversationJobResult::Error("project identity is unavailable"),
    };
    if cancelled.load(Ordering::Relaxed) {
        return ConversationJobResult::Cancelled;
    }
    let source = match GenericJsonlSource::for_project(project.clone()) {
        Ok(source) => source,
        Err(_) => {
            return ConversationJobResult::Error(
                "project-local conversation source is unavailable",
            );
        }
    };
    let registry = match SourceRegistry::new(vec![Box::new(source)]) {
        Ok(registry) => registry,
        Err(_) => {
            return ConversationJobResult::Error(
                "project-local conversation source registration failed",
            );
        }
    };
    let discovery = discover_conversations(
        &registry,
        &project,
        &HashMap::new(),
        DiscoveryLimit::new(256).expect("non-zero conversation discovery limit"),
        MetadataBudget::new(256 * 1024).expect("non-zero conversation metadata budget"),
    );
    if cancelled.load(Ordering::Relaxed) {
        ConversationJobResult::Cancelled
    } else {
        ConversationJobResult::Ready(discovery.into_conversations())
    }
}

const fn accepted(status: SubmitStatus) -> bool {
    matches!(status, SubmitStatus::Queued | SubmitStatus::Coalesced)
}

#[cfg(test)]
mod tests {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use std::fs;
    use std::path::Path;

    use tempfile::TempDir;

    use super::Controller;
    use crate::host::LaunchContext;
    use crate::intent::{Intent, View};
    use crate::model::LoadingState;
    use crate::worker::{JobKind, WorkerRuntime};

    fn controller(temp: &TempDir) -> Controller {
        let context = LaunchContext::from_vars([(
            "HERDR_PLUGIN_CONTEXT_JSON",
            format!(
                r#"{{"workspace_id":"workspace","tab_id":"tab","pane_id":"pane","cwd":"{}"}}"#,
                temp.path().display()
            ),
        )])
        .expect("context");
        Controller::new(context)
    }

    #[test]
    fn resize_is_dirty_in_every_view_and_during_bootstrap() {
        let temp = TempDir::new().expect("tempdir");
        let mut controller = controller(&temp);
        let mut workers = WorkerRuntime::with_capacities(2, 1);

        assert!(controller.apply(Intent::Resize, &mut workers).dirty);
        controller.model.set_active_view(View::Conversations);
        assert!(controller.apply(Intent::Resize, &mut workers).dirty);
    }

    #[test]
    fn bootstrap_completion_uses_runtime_generations_and_respects_inactive_files() {
        let temp = TempDir::new().expect("tempdir");
        fs::write(temp.path().join("file"), []).expect("file");
        let mut controller = controller(&temp);
        let mut workers = WorkerRuntime::with_capacities(2, 1);
        controller.start(&mut workers);
        controller.model.set_active_view(View::Conversations);

        let bootstrap = loop {
            let result = workers
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("bootstrap result");
            if result.key().kind() == JobKind::Bootstrap {
                break result;
            }
            controller.apply_result(result, &mut workers);
        };
        controller.apply_result(bootstrap, &mut workers);

        assert_eq!(controller.model.files().generations(), (0, 0));
        assert!(!controller.files_have_pending_work());
        assert!(matches!(
            controller.model.files().loading(),
            LoadingState::Ready
        ));
    }

    #[test]
    fn a_panicked_files_job_clears_runtime_work_state() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join("src")).expect("src");
        let mut controller = controller(&temp);
        let mut workers = WorkerRuntime::with_capacities(2, 1);
        controller.files = Some(
            crate::runtime::FilesRuntime::bootstrap(controller.model.launch_context())
                .expect("files"),
        );
        {
            let files = controller.files.as_mut().expect("files");
            assert!(files.handle_intent(&Intent::ExpandOrDescend, &mut workers));
        }
        while let Some(result) = workers.recv_timeout(std::time::Duration::from_millis(20)) {
            controller.apply_result(result, &mut workers);
            if !controller.files_have_pending_work() {
                break;
            }
        }
        let files = controller.files.as_mut().expect("files");
        assert!(files.handle_intent(&Intent::Refresh, &mut workers));
        let generation = files.generations().0;
        files.fail_background(JobKind::Filesystem, generation, &mut workers);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while controller.files_have_pending_work() || workers.has_pending_work() {
            let result = workers
                .recv_timeout(std::time::Duration::from_millis(50))
                .expect("worker result");
            controller.apply_result(result, &mut workers);
            assert!(
                std::time::Instant::now() < deadline,
                "Files work did not recover"
            );
        }

        assert!(!controller.files_have_pending_work());
        assert_eq!(controller.model.files().selection(), Some(Path::new("src")));
    }

    #[test]
    fn repeated_filesystem_panics_settle_after_one_retry() {
        let temp = TempDir::new().expect("tempdir");
        fs::write(temp.path().join("file"), []).expect("file");
        let mut controller = controller(&temp);
        let mut workers = WorkerRuntime::with_capacities(2, 1);
        controller.files = Some(
            crate::runtime::FilesRuntime::bootstrap(controller.model.launch_context())
                .expect("files"),
        );
        let files = controller.files.as_mut().expect("files");
        assert!(files.handle_intent(&Intent::Refresh, &mut workers));
        let first = files.generations().0;
        files.fail_background(JobKind::Filesystem, first, &mut workers);
        let retry = files.generations().0;
        files.fail_background(JobKind::Filesystem, retry, &mut workers);

        assert!(!files.has_pending_work());
        assert_eq!(files.generations().0, files.generations().1);
    }
    #[test]
    fn switching_views_retains_expansion_selection_and_scroll() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join("src")).expect("src");
        fs::write(temp.path().join("src/child"), []).expect("child");
        fs::write(temp.path().join("z-file"), []).expect("root file");
        let mut controller = controller(&temp);
        let mut workers = WorkerRuntime::with_capacities(2, 1);
        controller.files = Some(
            crate::runtime::FilesRuntime::bootstrap(controller.model.launch_context())
                .expect("files"),
        );

        let files = controller.files.as_mut().expect("files");
        assert_eq!(files.selection(), Some(Path::new("src")));
        assert!(files.handle_intent(&Intent::ExpandOrDescend, &mut workers));
        while files.has_pending_work() || workers.has_pending_work() {
            let result = workers
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("filesystem result");
            files.complete_background(
                *result
                    .downcast::<crate::runtime::RuntimeMessage>()
                    .expect("runtime message"),
                &mut workers,
            );
        }
        assert!(files.handle_intent(&Intent::ExpandOrDescend, &mut workers));
        let area = Rect::new(0, 0, 20, 1);
        files.render(area, &mut Buffer::empty(area));
        let selected = files.selection().map(Path::to_path_buf);
        let scroll = files.scroll();
        assert_eq!(selected.as_deref(), Some(Path::new("src/child")));
        assert_eq!(scroll, 1);

        assert!(
            controller
                .apply(Intent::SwitchView(View::Conversations), &mut workers)
                .dirty
        );
        assert!(
            controller
                .apply(Intent::SwitchView(View::Files), &mut workers)
                .dirty
        );

        let files = controller.files.as_mut().expect("files");
        assert_eq!(files.selection(), selected.as_deref());
        assert_eq!(files.scroll(), scroll);
        let area = Rect::new(0, 0, 20, 2);
        let mut buffer = Buffer::empty(area);
        files.render(area, &mut buffer);
        let rendered = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("src/child"));
    }
    #[test]
    fn low_priority_orchestration_loads_project_local_conversations() {
        let temp = TempDir::new().expect("tempdir");
        let directory = temp.path().join(".herdr/conversations");
        fs::create_dir_all(&directory).expect("conversation directory");
        fs::write(
            directory.join("session.jsonl"),
            format!(
                "{}\n",
                serde_json::json!({
                    "session_id": "controller-session",
                    "cwd": temp.path(),
                    "timestamp": "2026-01-02T03:04:05Z",
                    "role": "user",
                    "message": "private controller fixture",
                })
            ),
        )
        .expect("conversation fixture");
        let mut controller = controller(&temp);
        controller.model.set_active_view(View::Conversations);
        let mut workers = WorkerRuntime::with_capacities(2, 1);
        controller.start(&mut workers);

        loop {
            let result = workers
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("worker result");
            let kind = result.key().kind();
            controller.apply_result(result, &mut workers);
            if kind == JobKind::ConversationDiscovery {
                break;
            }
        }

        assert!(matches!(
            controller.model.conversations().loading(),
            LoadingState::Ready
        ));
        assert_eq!(controller.model.conversations().items().len(), 1);
        assert_eq!(
            controller.model.conversations().items()[0]
                .session_reference()
                .id(),
            "controller-session"
        );
        workers.shutdown();
    }
}
