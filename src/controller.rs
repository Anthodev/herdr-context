//! Intent transitions and orchestration between state and bounded workers.

use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::conversations::Conversation;
use crate::conversations::discovery::discover_conversations_cancellable;
use crate::conversations::index::ConversationIndex;
use crate::conversations::sources::{
    ConversationSourceError, DiscoveryLimit, GenericJsonlSource, KnownStoreRoots, MetadataBudget,
    SourceRegistry,
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
    Ready {
        conversations: Vec<Conversation>,
        has_more: bool,
        source_errors: Vec<String>,
        reset_source_errors: bool,
    },
    Cancelled,
    Error(String),
}
#[derive(Clone, Debug)]
struct ConversationPaths {
    state_dir: PathBuf,
    home: PathBuf,
}

impl ConversationPaths {
    fn from_env() -> Option<Self> {
        let state_dir = env::var_os("HERDR_PLUGIN_STATE_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)?;
        let home = env::var_os("HOME")
            .or_else(|| env::var_os("USERPROFILE"))
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)?;
        Some(Self { state_dir, home })
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Transition {
    pub(super) dirty: bool,
    pub(super) quit: bool,
}

pub struct Controller {
    model: AppModel,
    files: Option<FilesRuntime>,
    conversation_paths: Option<ConversationPaths>,
}

impl Controller {
    pub(super) fn new(context: LaunchContext) -> Self {
        Self {
            model: AppModel::new(context),
            files: None,
            conversation_paths: ConversationPaths::from_env(),
        }
    }
    #[cfg(test)]
    fn new_with_conversation_paths(
        context: LaunchContext,
        state_dir: PathBuf,
        home: PathBuf,
    ) -> Self {
        Self {
            model: AppModel::new(context),
            files: None,
            conversation_paths: Some(ConversationPaths { state_dir, home }),
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
        self.schedule_conversation_page(workers, true);
    }

    fn schedule_conversation_page(&mut self, workers: &mut WorkerRuntime, show_loading: bool) {
        let (requested, applied) = self.model.conversations().generations();
        let generation = requested.saturating_add(1);
        self.model
            .conversations_mut()
            .set_generations(generation, applied);
        if show_loading {
            self.model
                .conversations_mut()
                .set_loading(LoadingState::Loading);
        }
        let root = self
            .model
            .launch_context()
            .foreground_cwd()
            .unwrap_or_else(|| self.model.launch_context().cwd())
            .to_path_buf();
        let paths = self.conversation_paths.clone();
        let job = Job::new(
            JobKey::new(JobKind::ConversationDiscovery, &root),
            generation,
            Priority::Low,
            move |cancelled| {
                Box::new(ConversationsResult(load_conversations(
                    &root,
                    paths.as_ref(),
                    cancelled,
                    show_loading,
                )))
            },
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
            intent if self.model.active_view() == View::Conversations => {
                let area = self.model.geometry().content();
                let dirty = self.model.conversations_mut().handle_intent(&intent, area);
                Transition { dirty, quit: false }
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
                    ConversationJobResult::Ready {
                        conversations,
                        has_more,
                        source_errors,
                        reset_source_errors,
                    } => {
                        let changed = self
                            .model
                            .conversations_mut()
                            .replace_items(conversations, generation);
                        let mut visible_errors = if reset_source_errors {
                            Vec::new()
                        } else {
                            self.model.conversations().source_errors().to_vec()
                        };
                        visible_errors.extend(source_errors);
                        visible_errors.sort_unstable();
                        visible_errors.dedup();
                        visible_errors.truncate(8);
                        self.model
                            .conversations_mut()
                            .set_source_errors(visible_errors);
                        if has_more {
                            self.schedule_conversation_page(workers, false);
                        }
                        changed
                    }
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
                            .set_loading(LoadingState::Error(message));
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

fn load_conversations(
    root: &Path,
    paths: Option<&ConversationPaths>,
    cancelled: &AtomicBool,
    reset_source_errors: bool,
) -> ConversationJobResult {
    if cancelled.load(Ordering::Relaxed) {
        return ConversationJobResult::Cancelled;
    }
    let paths = paths.filter(|_| cfg!(unix));
    let project = match resolve_project_context(root) {
        Ok(context) => context.conversation_identity().clone(),
        Err(_) => {
            return ConversationJobResult::Error("project identity is unavailable".to_owned());
        }
    };
    if cancelled.load(Ordering::Relaxed) {
        return ConversationJobResult::Cancelled;
    }
    let generic = match GenericJsonlSource::for_project(project.clone()) {
        Ok(source) => source,
        Err(_) => {
            return ConversationJobResult::Error(
                "project-local conversation source is unavailable".to_owned(),
            );
        }
    };
    let mut sources: Vec<Box<dyn crate::conversations::sources::ConversationSource>> =
        vec![Box::new(generic)];
    if let Some(paths) = paths {
        let roots = KnownStoreRoots::under_home(&paths.home);
        match roots.sources(project.clone()) {
            Ok(external) => sources.extend(external),
            Err(error) => return ConversationJobResult::Error(error.to_string()),
        }
    }
    let registry = match SourceRegistry::new(sources) {
        Ok(registry) => registry,
        Err(error) => return ConversationJobResult::Error(error.to_string()),
    };
    let limit = DiscoveryLimit::new(if paths.is_some() { 128 } else { 256 })
        .expect("non-zero conversation discovery limit");
    let budget = MetadataBudget::new(512 * 1024).expect("non-zero conversation metadata budget");
    let (conversations, has_more, source_errors) = if let Some(paths) = paths {
        let mut index = match ConversationIndex::open(&paths.state_dir, project) {
            Ok(index) => index,
            Err(error) => return ConversationJobResult::Error(error.to_string()),
        };
        let refresh = match index.refresh_page_cancellable(&registry, limit, budget, cancelled) {
            Ok(refresh) => refresh,
            Err(error) => return ConversationJobResult::Error(error.to_string()),
        };
        if refresh.is_cancelled() {
            return ConversationJobResult::Cancelled;
        }
        let source_errors = source_error_messages(refresh.errors());
        (
            index.page(0, 4_096).into_conversations(),
            refresh.has_more(),
            source_errors,
        )
    } else {
        let discovery = discover_conversations_cancellable(
            &registry,
            &project,
            &HashMap::new(),
            limit,
            budget,
            cancelled,
        );
        let source_errors = source_error_messages(discovery.errors());
        (discovery.into_conversations(), false, source_errors)
    };
    if cancelled.load(Ordering::Relaxed) {
        ConversationJobResult::Cancelled
    } else {
        ConversationJobResult::Ready {
            conversations,
            has_more,
            source_errors,
            reset_source_errors,
        }
    }
}

fn source_error_messages(errors: &[ConversationSourceError]) -> Vec<String> {
    let mut messages = errors
        .iter()
        .map(|error| format!("{:?}: {error}", error.kind()))
        .collect::<Vec<_>>();
    messages.sort_unstable();
    messages.dedup();
    messages.truncate(8);
    messages
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
    use std::time::{Duration, UNIX_EPOCH};
    use tempfile::TempDir;

    use super::Controller;
    use crate::conversations::{
        Conversation, ConversationProvenance, ConversationState, ProvenanceKind, ResumeCapability,
        SessionReference, SourceId, ToolIdentity,
    };
    use crate::host::LaunchContext;
    use crate::intent::{Intent, PointerAction, View};
    use crate::model::LoadingState;
    use crate::project::ProjectIdentity;
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

    fn conversation(
        project: &ProjectIdentity,
        tool: &str,
        session_id: &str,
        updated_seconds: u64,
    ) -> Conversation {
        Conversation::new(
            ToolIdentity::new(tool).expect("tool"),
            SessionReference::new(tool, session_id).expect("session"),
            project.clone(),
            None,
            Some(UNIX_EPOCH + Duration::from_secs(updated_seconds)),
            None,
            UNIX_EPOCH + Duration::from_secs(updated_seconds),
            ConversationState::Unknown,
            vec![ConversationProvenance::new(
                SourceId::new(tool).expect("source"),
                ProvenanceKind::ExternalLocal,
                None,
            )],
            ResumeCapability::Unsupported,
        )
        .expect("conversation")
    }

    fn rendered_line(buffer: &Buffer, row: u16) -> String {
        (buffer.area.x..buffer.area.right())
            .map(|column| buffer[(column, row)].symbol())
            .collect()
    }

    #[test]
    fn conversation_provider_groups_toggle_by_keyboard_and_pointer_across_refreshes() {
        let project_dir = TempDir::new().expect("project");
        let project = ProjectIdentity::from_canonical_root(project_dir.path().to_path_buf())
            .expect("project identity");
        let mut controller = controller(&project_dir);
        controller.model.set_active_view(View::Conversations);
        controller.model.conversations_mut().replace_items(
            vec![
                conversation(&project, "pi", "pi-session", 30),
                conversation(&project, "codex-cli", "codex-new", 20),
                conversation(&project, "codex-cli", "codex-old", 10),
            ],
            1,
        );
        let area = Rect::new(0, 0, 50, 7);
        controller.render(area, &mut Buffer::empty(area));
        let mut workers = WorkerRuntime::with_capacities(2, 1);

        assert!(controller.apply(Intent::ToggleSelected, &mut workers).dirty);
        assert!(controller.apply(Intent::SelectNext, &mut workers).dirty);
        assert!(
            controller
                .apply(Intent::CollapseOrAscend, &mut workers)
                .dirty
        );
        controller.model.conversations_mut().replace_items(
            vec![
                conversation(&project, "pi", "pi-session", 40),
                conversation(&project, "codex-cli", "codex-new", 20),
                conversation(&project, "codex-cli", "codex-old", 10),
            ],
            2,
        );
        assert!(
            controller
                .apply(Intent::SwitchView(View::Files), &mut workers)
                .dirty
        );
        assert!(
            controller
                .apply(Intent::SwitchView(View::Conversations), &mut workers)
                .dirty
        );

        assert!(
            controller
                .apply(
                    Intent::Pointer {
                        column: area.x,
                        row: area.y.saturating_add(1),
                        action: PointerAction::Select,
                    },
                    &mut workers,
                )
                .dirty
        );
        let mut buffer = Buffer::empty(area);
        controller.render(area, &mut buffer);
        assert!(rendered_line(&buffer, 1).starts_with("▾ codex-cli (2)"));
        assert!(rendered_line(&buffer, 2).starts_with("  codex-new"));
        assert!(rendered_line(&buffer, 3).starts_with("  codex-old"));
        assert!(rendered_line(&buffer, 4).starts_with("▸ pi (1)"));
        workers.shutdown();
    }

    #[test]
    fn conversation_viewport_tracks_selected_provider_across_pages_and_resizes() {
        let project_dir = TempDir::new().expect("project");
        let project = ProjectIdentity::from_canonical_root(project_dir.path().to_path_buf())
            .expect("project identity");
        let mut controller = controller(&project_dir);
        controller.model.set_active_view(View::Conversations);
        controller.model.conversations_mut().replace_items(
            vec![
                conversation(&project, "pi", "pi-session", 20),
                conversation(&project, "codex-cli", "codex-00", 10),
            ],
            1,
        );
        let small = Rect::new(0, 0, 40, 4);
        controller.render(small, &mut Buffer::empty(small));
        let mut workers = WorkerRuntime::with_capacities(2, 1);
        assert!(controller.apply(Intent::SelectNext, &mut workers).dirty);

        let mut next_page = (0_u64..10)
            .map(|index| {
                conversation(
                    &project,
                    "codex-cli",
                    &format!("codex-{index:02}"),
                    100 - index,
                )
            })
            .collect::<Vec<_>>();
        next_page.push(conversation(&project, "pi", "pi-session", 20));
        controller
            .model
            .conversations_mut()
            .replace_items(next_page, 2);
        let mut paged = Buffer::empty(small);
        controller.render(small, &mut paged);
        assert!((1..small.height).any(|row| rendered_line(&paged, row).starts_with("▾ pi (1)")));

        let large = Rect::new(0, 0, 40, 15);
        let mut grown = Buffer::empty(large);
        controller.render(large, &mut grown);
        assert_eq!(controller.model.conversations().scroll(), 0);
        assert!((1..large.height).any(|row| rendered_line(&grown, row).starts_with("▾ pi (1)")));

        let one_row = Rect::new(0, 0, 40, 2);
        let mut shrunk = Buffer::empty(one_row);
        controller.render(one_row, &mut shrunk);
        assert!(rendered_line(&shrunk, 1).starts_with("▾ pi (1)"));
        workers.shutdown();
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
        for index in 0..128 {
            fs::write(
                directory.join(format!("session-{index:03}.jsonl")),
                format!(
                    "{}\n",
                    serde_json::json!({
                        "session_id": format!("controller-session-{index:03}"),
                        "cwd": temp.path(),
                        "timestamp": "2026-01-02T03:04:05Z",
                        "role": "user",
                        "message": "private controller fixture",
                    })
                ),
            )
            .expect("conversation fixture");
        }
        for (path, index) in [
            (temp.path().join(".herdr/conversations.jsonl"), 128),
            (temp.path().join(".herdr/conversations.json"), 129),
        ] {
            fs::write(
                path,
                format!(
                    "{}\n",
                    serde_json::json!({
                        "session_id": format!("controller-session-{index:03}"),
                        "cwd": temp.path(),
                        "timestamp": "2026-01-02T03:04:05Z",
                        "role": "user",
                        "message": "private controller fixture",
                    })
                ),
            )
            .expect("direct conversation fixture");
        }
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
        assert_eq!(controller.model.conversations().items().len(), 130);
        assert!(
            controller
                .model
                .conversations()
                .items()
                .iter()
                .any(|conversation| {
                    conversation.session_reference().id() == "controller-session-000"
                })
        );
        workers.shutdown();
    }
    #[test]
    fn paged_index_publishes_recent_results_and_schedules_older_pages() {
        let project = TempDir::new().expect("project");
        let directory = project.path().join(".herdr/conversations");
        fs::create_dir_all(&directory).expect("conversation directory");
        let record = |session_id: String| {
            format!(
                "{}\n",
                serde_json::json!({
                    "session_id": session_id,
                    "cwd": project.path(),
                    "timestamp": "2026-01-02T03:04:05Z",
                    "role": "user",
                    "message": "private paged controller fixture",
                })
            )
        };
        for index in 0..128 {
            fs::write(
                directory.join(format!("session-{index:03}.jsonl")),
                record(format!("session-{index:03}")),
            )
            .expect("conversation fixture");
        }
        fs::write(
            project.path().join(".herdr/conversations.jsonl"),
            record("session-jsonl".to_owned()),
        )
        .expect("direct JSONL fixture");
        fs::write(
            project.path().join(".herdr/conversations.json"),
            record("session-json".to_owned()).trim_end(),
        )
        .expect("direct JSON fixture");

        let context = LaunchContext::from_vars([(
            "HERDR_PLUGIN_CONTEXT_JSON",
            format!(
                r#"{{"workspace_id":"workspace","tab_id":"tab","pane_id":"pane","cwd":"{}"}}"#,
                project.path().display()
            ),
        )])
        .expect("context");
        let state = TempDir::new().expect("state");
        let home = TempDir::new().expect("home");
        let mut controller = Controller::new_with_conversation_paths(
            context,
            state.path().join("plugin-state"),
            home.path().to_path_buf(),
        );
        controller.model.set_active_view(View::Conversations);
        let mut workers = WorkerRuntime::with_capacities(2, 1);
        controller.start(&mut workers);

        loop {
            let result = workers
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("worker result");
            controller.apply_result(result, &mut workers);
            if !workers.has_pending_work()
                && matches!(
                    controller.model.conversations().loading(),
                    LoadingState::Ready
                )
            {
                break;
            }
        }

        assert_eq!(controller.model.conversations().items().len(), 130);
        let (requested, applied) = controller.model.conversations().generations();
        assert!(requested >= 2);
        assert_eq!(applied, requested);
        workers.shutdown();
    }
}
