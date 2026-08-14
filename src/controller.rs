//! Intent transitions and orchestration between state and bounded workers.

use std::collections::{HashMap, HashSet};
use std::env;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::config::{ConfigLoad, ExternalHistoryRoot, KeyBindings, PluginConfig};
use crate::conversations::Conversation;
use crate::conversations::active::{
    FilesystemConversationSnapshot, LiveConversationSnapshot, merge_filesystem_snapshots,
    merge_prepared_live_sessions, prepare_filesystem_conversations, prepare_live_conversations,
};
use crate::conversations::discovery::discover_conversations_cancellable;
use crate::conversations::index::{ConversationIndex, IndexStatus};
use crate::conversations::sources::{
    ClaudeCodeSource, CodexCliSource, ConversationSource, ConversationSourceError, DiscoveryLimit,
    GenericJsonlSource, KnownStoreRoots, MetadataBudget, OmpSource, OpenCodeSource, PiSource,
    ProjectLocalLocation, SourceId, SourceRegistry,
};
use crate::host::client::CommandHostClient;
use crate::host::{HostClient, LaunchContext};
use crate::intent::{Intent, View};
use crate::model::{AppModel, LoadingState};
use crate::project::{ProjectIdentity, resolve_project_context_with_backend};
use crate::runtime::{FilesRuntime, RuntimeMessage};
use crate::ui::render_shell;
use crate::worker::{CompletedJob, Job, JobKey, JobKind, Priority, SubmitStatus, WorkerRuntime};

struct ConfigResult(ConfigLoad);
struct BootstrapResult(Result<FilesRuntime, crate::runtime::FilesRuntimeError>);
struct ConversationsResult(ConversationJobResult);
struct LiveConversationsResult(LiveConversationJobResult);

enum ConversationJobResult {
    Ready {
        project: ProjectIdentity,
        conversations: FilesystemConversationSnapshot,
        has_more: bool,
        source_errors: Vec<String>,
        reset_source_errors: bool,
    },
    Cancelled,
    Error(String),
}

enum LiveConversationJobResult {
    Ready(LiveSnapshot),
    Cancelled,
    Error(String),
}

#[derive(Clone)]
struct LiveSnapshot {
    project: ProjectIdentity,
    sessions: LiveConversationSnapshot,
    observed_at: SystemTime,
}
#[derive(Clone, Debug, Default)]
struct ConversationPaths {
    state_dir: Option<PathBuf>,
    home: Option<PathBuf>,
}

impl ConversationPaths {
    fn from_env() -> Self {
        let state_dir = env::var_os("HERDR_PLUGIN_STATE_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let home = env::var_os("HOME")
            .or_else(|| env::var_os("USERPROFILE"))
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        Self { state_dir, home }
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
    conversation_paths: ConversationPaths,
    host_binary: Option<PathBuf>,
    filesystem_conversations: Option<FilesystemConversationSnapshot>,
    conversation_project: Option<ProjectIdentity>,
    live_snapshot: Option<LiveSnapshot>,
    config: PluginConfig,
    subsystems_started: bool,
}

impl Controller {
    pub(super) fn new(context: LaunchContext) -> Self {
        Self {
            model: AppModel::new(context),
            files: None,
            conversation_paths: ConversationPaths::from_env(),
            host_binary: env::var_os("HERDR_BIN_PATH")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
            filesystem_conversations: None,
            conversation_project: None,
            live_snapshot: None,
            config: PluginConfig::default(),
            subsystems_started: false,
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
            conversation_paths: ConversationPaths {
                state_dir: Some(state_dir),
                home: Some(home),
            },
            host_binary: None,
            filesystem_conversations: None,
            conversation_project: None,
            live_snapshot: None,
            config: PluginConfig::default(),
            subsystems_started: false,
        }
    }

    pub(super) fn start(&mut self, workers: &mut WorkerRuntime) {
        let config_root = env::var_os("HERDR_PLUGIN_CONFIG_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("<defaults>"));
        let job = Job::new(
            JobKey::new(JobKind::Config, config_root),
            1,
            Priority::High,
            |_| Box::new(ConfigResult(PluginConfig::load_from_env())),
        );
        if !accepted(workers.submit(job)) {
            self.apply_config_load(
                ConfigLoad::with_runtime_warning(
                    "Config: background configuration queue is unavailable; using defaults",
                ),
                workers,
            );
        }
    }

    fn start_subsystems(&mut self, workers: &mut WorkerRuntime) {
        if self.subsystems_started {
            return;
        }
        self.subsystems_started = true;
        let context = self.model.launch_context().clone();
        let root = context
            .foreground_cwd()
            .unwrap_or_else(|| context.cwd())
            .to_path_buf();
        let config = self.config.clone();
        let bootstrap = Job::new(
            JobKey::new(JobKind::Bootstrap, root),
            1,
            Priority::High,
            move |cancelled| {
                Box::new(BootstrapResult(
                    FilesRuntime::bootstrap_with_config_cancellable(&context, &config, cancelled),
                ))
            },
        );
        if !accepted(workers.submit(bootstrap)) {
            self.model.files_mut().set_loading(LoadingState::Error(
                "background bootstrap queue is unavailable".to_owned(),
            ));
        }
        self.schedule_conversations(workers);
    }

    fn apply_config_load(&mut self, load: ConfigLoad, workers: &mut WorkerRuntime) {
        let (config, warnings) = load.into_parts();
        self.config = config;
        self.model.set_config_warnings(warnings);
        self.start_subsystems(workers);
    }

    fn schedule_conversations(&mut self, workers: &mut WorkerRuntime) {
        self.schedule_conversation_page(workers, true);
        self.schedule_live_conversations(workers);
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
        let config = self.config.clone();
        let job = Job::new(
            JobKey::new(JobKind::ConversationDiscovery, &root),
            generation,
            Priority::Low,
            move |cancelled| {
                Box::new(ConversationsResult(load_conversations(
                    &root,
                    &paths,
                    &config,
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

    fn schedule_live_conversations(&mut self, workers: &mut WorkerRuntime) {
        let Some(binary) = self.host_binary.clone() else {
            self.model.conversations_mut().set_live_loading(false);
            return;
        };
        let (requested, applied) = self.model.conversations().live_generations();
        let generation = requested.saturating_add(1);
        self.model
            .conversations_mut()
            .set_live_generations(generation, applied);
        self.model.conversations_mut().set_live_loading(true);
        let root = self
            .model
            .launch_context()
            .foreground_cwd()
            .unwrap_or_else(|| self.model.launch_context().cwd())
            .to_path_buf();
        let backend = self.config.vcs().backend();
        let job = Job::new(
            JobKey::new(JobKind::ConversationLive, &root),
            generation,
            Priority::Low,
            move |cancelled| {
                Box::new(LiveConversationsResult(load_live_conversations(
                    &root, binary, backend, cancelled,
                )))
            },
        );
        if !accepted(workers.submit(job)) {
            self.live_snapshot = None;
            self.model
                .conversations_mut()
                .set_live_error(Some("Herdr live session queue is unavailable".to_owned()));
            let visible = self.merged_conversations();
            self.model
                .conversations_mut()
                .replace_live_items(visible, generation);
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
            if kind == JobKind::Config {
                self.apply_config_load(
                    ConfigLoad::with_runtime_warning(
                        "Config: background configuration worker stopped; using defaults",
                    ),
                    workers,
                );
                return true;
            }
            if matches!(kind, JobKind::Filesystem | JobKind::Vcs)
                && let Some(files) = &mut self.files
            {
                files.fail_background(kind, generation, workers);
            } else if kind == JobKind::ConversationLive {
                let requested = self.model.conversations().live_generations().0;
                if generation < requested {
                    return false;
                }
                self.live_snapshot = None;
                self.model.conversations_mut().set_live_error(Some(
                    "Herdr live session worker stopped unexpectedly".to_owned(),
                ));
                let visible = self.merged_conversations();
                self.model
                    .conversations_mut()
                    .replace_live_items(visible, generation);
            } else {
                self.set_worker_error(kind);
            }
            self.retry_files(workers);
            return true;
        }
        match result.key().kind() {
            JobKind::Config => {
                let Ok(result) = result.downcast::<ConfigResult>() else {
                    self.apply_config_load(
                        ConfigLoad::with_runtime_warning(
                            "Config: invalid configuration worker result; using defaults",
                        ),
                        workers,
                    );
                    return true;
                };
                self.apply_config_load(result.0, workers);
                true
            }
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
                        project,
                        conversations,
                        has_more,
                        source_errors,
                        reset_source_errors,
                    } => {
                        self.conversation_project = Some(project);
                        let degraded_cache = source_errors
                            .iter()
                            .any(|error| error.starts_with("Cache: metadata index is unavailable"));
                        let conversations = if degraded_cache {
                            if let Some(previous) = self.filesystem_conversations.as_ref() {
                                merge_filesystem_snapshots(previous, conversations)
                            } else {
                                conversations
                            }
                        } else {
                            conversations
                        };
                        self.filesystem_conversations = Some(conversations);
                        let visible = self.merged_conversations();
                        let changed = self
                            .model
                            .conversations_mut()
                            .replace_items(visible, generation);
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
            JobKind::ConversationLive => {
                let generation = result.generation();
                let requested = self.model.conversations().live_generations().0;
                if generation < requested {
                    return false;
                }
                let Ok(result) = result.downcast::<LiveConversationsResult>() else {
                    self.live_snapshot = None;
                    self.model
                        .conversations_mut()
                        .set_live_error(Some("invalid live conversation worker result".to_owned()));
                    let visible = self.merged_conversations();
                    self.model
                        .conversations_mut()
                        .replace_live_items(visible, generation);
                    return true;
                };
                match result.0 {
                    LiveConversationJobResult::Ready(snapshot) => {
                        self.live_snapshot = Some(snapshot);
                        self.model.conversations_mut().set_live_error(None);
                    }
                    LiveConversationJobResult::Cancelled => {
                        self.model
                            .conversations_mut()
                            .set_live_generations(requested, generation);
                        self.model.conversations_mut().set_live_loading(false);
                        return true;
                    }
                    LiveConversationJobResult::Error(message) => {
                        self.live_snapshot = None;
                        self.model
                            .conversations_mut()
                            .set_live_error(Some(format!("Herdr: {message}")));
                    }
                }
                let visible = self.merged_conversations();
                self.model
                    .conversations_mut()
                    .replace_live_items(visible, generation)
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
            JobKind::Config => {
                self.model.set_config_warnings(vec![
                    "Config: background configuration worker stopped; using defaults".to_owned(),
                ]);
            }
            JobKind::ConversationDiscovery => self.model.conversations_mut().set_loading(state),
            JobKind::ConversationLive => {
                self.model.conversations_mut().set_live_error(Some(
                    "Herdr live session worker stopped unexpectedly".to_owned(),
                ));
                self.model.conversations_mut().set_live_loading(false);
            }
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

    fn merged_conversations(&self) -> Vec<Conversation> {
        let Some(filesystem) = &self.filesystem_conversations else {
            return Vec::new();
        };
        let Some(project) = &self.conversation_project else {
            return Vec::new();
        };
        let Some(live) = self
            .live_snapshot
            .as_ref()
            .filter(|live| live.project == *project)
        else {
            return filesystem.conversations().to_vec();
        };
        merge_prepared_live_sessions(filesystem, &live.sessions, project, live.observed_at)
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

    pub(super) const fn keybindings(&self) -> &KeyBindings {
        self.config.keybindings()
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

    pub(super) fn next_refresh_in(&self, now: Instant) -> Option<Duration> {
        if self.model.active_view() != View::Files {
            return None;
        }
        self.files
            .as_ref()
            .and_then(|files| files.next_refresh_in(now))
    }

    pub(super) fn tick(&mut self, now: Instant, workers: &mut WorkerRuntime) -> bool {
        self.model.active_view() == View::Files
            && self
                .files
                .as_mut()
                .is_some_and(|files| files.tick(now, workers))
    }
}

fn load_conversations(
    root: &Path,
    paths: &ConversationPaths,
    config: &PluginConfig,
    cancelled: &AtomicBool,
    reset_source_errors: bool,
) -> ConversationJobResult {
    if cancelled.load(Ordering::Relaxed) {
        return ConversationJobResult::Cancelled;
    }
    let state_dir = if cfg!(unix) {
        paths.state_dir.as_ref()
    } else {
        None
    };
    let project = match resolve_project_context_with_backend(root, config.vcs().backend()) {
        Ok(context) => context.conversation_identity().clone(),
        Err(_) => {
            return ConversationJobResult::Error("project identity is unavailable".to_owned());
        }
    };
    if cancelled.load(Ordering::Relaxed) {
        return ConversationJobResult::Cancelled;
    }

    let config = config.conversations();
    let mut sources: Vec<Box<dyn ConversationSource>> = Vec::new();
    let mut setup_errors = Vec::new();
    let mut desired_source_ids = Vec::new();
    if config.source_enabled("project-local-generic-jsonl") {
        desired_source_ids.push(
            SourceId::new("project-local-generic-jsonl")
                .expect("static conversation source ID is valid"),
        );
        let locations = [
            PathBuf::from(".herdr/conversations"),
            PathBuf::from(".herdr/conversations.jsonl"),
            PathBuf::from(".herdr/conversations.json"),
        ]
        .into_iter()
        .chain(config.project_roots().iter().cloned())
        .map(ProjectLocalLocation::new)
        .collect::<Result<Vec<_>, _>>();
        match locations
            .map_err(|error| error.to_string())
            .and_then(|locations| {
                GenericJsonlSource::new(project.clone(), locations)
                    .map_err(|error| error.to_string())
            }) {
            Ok(source) => sources.push(Box::new(source)),
            Err(error) => setup_errors.push(error),
        }
    }
    for source in ["claude-code", "codex-cli", "omp", "opencode", "pi"] {
        if config.source_enabled(source) {
            desired_source_ids
                .push(SourceId::new(source).expect("static conversation source ID is valid"));
        }
    }
    if let Some(home) = &paths.home {
        let roots = KnownStoreRoots::under_home(home);
        match roots.sources(project.clone()) {
            Ok(external) => sources.extend(
                external
                    .into_iter()
                    .filter(|source| config.source_enabled(source.source_id().as_str())),
            ),
            Err(error) => setup_errors.push(error.to_string()),
        }
    } else {
        for source in ["claude-code", "codex-cli", "omp", "opencode", "pi"] {
            if config.source_enabled(source) {
                setup_errors.push(format!(
                    "{source}: user home directory is unavailable; retaining cached metadata"
                ));
            }
        }
    }
    for root in config.external_roots() {
        if !config.source_enabled(root.source()) {
            continue;
        }
        let id = configured_external_source_id(root);
        desired_source_ids.push(id.clone());
        match configured_external_source(project.clone(), root, id) {
            Ok(source) => sources.push(source),
            Err(error) => setup_errors.push(error),
        }
    }
    let registry = match SourceRegistry::new_with_desired_source_ids(sources, desired_source_ids) {
        Ok(registry) => registry,
        Err(error) => return ConversationJobResult::Error(error.to_string()),
    };
    let limit = DiscoveryLimit::new(config.page_size().get())
        .expect("validated conversation discovery limit is non-zero");
    let budget = MetadataBudget::new(512 * 1024).expect("non-zero conversation metadata budget");
    let (mut conversations, has_more, mut source_errors) = if let Some(state_dir) = state_dir {
        match ConversationIndex::open_with_max_entries(
            state_dir,
            project.clone(),
            config.cache_entries(),
        ) {
            Ok(mut index) => {
                let cache_status = index.status();
                match index.refresh_page_cancellable(&registry, limit, budget, cancelled) {
                    Ok(refresh) if refresh.is_cancelled() => {
                        return ConversationJobResult::Cancelled;
                    }
                    Ok(refresh) => {
                        let mut source_errors = source_error_messages(refresh.errors());
                        match cache_status {
                            IndexStatus::RebuiltCorrupt => source_errors
                                .push("Cache: corrupt metadata index was rebuilt".to_owned()),
                            IndexStatus::RebuiltIncompatible => source_errors
                                .push("Cache: incompatible metadata index was rebuilt".to_owned()),
                            IndexStatus::Loaded | IndexStatus::RebuiltMissing => {}
                        }
                        (
                            index
                                .page(0, config.cache_entries().get())
                                .into_conversations(),
                            refresh.has_more(),
                            source_errors,
                        )
                    }
                    Err(_) => {
                        let (conversations, mut errors) = discover_without_index(
                            &registry,
                            &project,
                            limit,
                            budget,
                            config.cache_entries().get(),
                            cancelled,
                        );
                        errors.push(
                            "Cache: metadata index is unavailable; using nonpersistent discovery"
                                .to_owned(),
                        );
                        (conversations, false, errors)
                    }
                }
            }
            Err(_) => {
                let (conversations, mut errors) = discover_without_index(
                    &registry,
                    &project,
                    limit,
                    budget,
                    config.cache_entries().get(),
                    cancelled,
                );
                errors.push(
                    "Cache: metadata index is unavailable; using nonpersistent discovery"
                        .to_owned(),
                );
                (conversations, false, errors)
            }
        }
    } else {
        let (conversations, errors) = discover_without_index(
            &registry,
            &project,
            limit,
            budget,
            config.cache_entries().get(),
            cancelled,
        );
        (conversations, false, errors)
    };
    conversations.truncate(config.cache_entries().get());
    source_errors.extend(setup_errors);
    if cancelled.load(Ordering::Relaxed) {
        return ConversationJobResult::Cancelled;
    }
    let conversations = prepare_filesystem_conversations(conversations);
    if cancelled.load(Ordering::Relaxed) {
        ConversationJobResult::Cancelled
    } else {
        ConversationJobResult::Ready {
            project,
            conversations,
            has_more,
            source_errors,
            reset_source_errors,
        }
    }
}

fn discover_without_index(
    registry: &SourceRegistry,
    project: &ProjectIdentity,
    limit: DiscoveryLimit,
    budget: MetadataBudget,
    max_entries: usize,
    cancelled: &AtomicBool,
) -> (Vec<Conversation>, Vec<String>) {
    let mut watermarks = HashMap::new();
    let mut conversations = Vec::new();
    let mut seen = HashSet::new();
    let mut errors = Vec::new();
    let max_pages = max_entries.div_ceil(limit.get()).saturating_add(1);
    for _ in 0..max_pages {
        if cancelled.load(Ordering::Relaxed) {
            break;
        }
        let discovery = discover_conversations_cancellable(
            registry,
            project,
            &watermarks,
            limit,
            budget,
            cancelled,
        );
        errors.extend(source_error_messages(discovery.errors()));
        let has_more = discovery.has_more();
        let next_watermarks = discovery.watermarks().clone();
        for conversation in discovery.into_conversations() {
            let key = (
                conversation.session_reference().namespace().to_owned(),
                conversation.session_reference().id().to_owned(),
            );
            if seen.insert(key) {
                conversations.push(conversation);
            }
        }
        if !has_more || conversations.len() >= max_entries || next_watermarks == watermarks {
            break;
        }
        watermarks = next_watermarks;
    }
    (conversations, errors)
}

fn configured_external_source_id(root: &ExternalHistoryRoot) -> SourceId {
    let mut fingerprint = 0xcbf2_9ce4_8422_2325_u64;
    for byte in root
        .source()
        .as_bytes()
        .iter()
        .copied()
        .chain([0])
        .chain(root.path().as_os_str().as_encoded_bytes().iter().copied())
    {
        fingerprint ^= u64::from(byte);
        fingerprint = fingerprint.wrapping_mul(0x0000_0100_0000_01b3);
    }
    SourceId::new(format!("{}:extra:{fingerprint:016x}", root.source()))
        .expect("configured source ID has a non-empty static prefix")
}

fn configured_external_source(
    project: ProjectIdentity,
    root: &ExternalHistoryRoot,
    id: SourceId,
) -> Result<Box<dyn ConversationSource>, String> {
    match root.source() {
        "claude-code" => {
            ClaudeCodeSource::new_with_source_id(project, root.path().to_path_buf(), id)
                .map(|source| Box::new(source) as Box<dyn ConversationSource>)
        }
        "codex-cli" => CodexCliSource::new_with_source_id(project, root.path().to_path_buf(), id)
            .map(|source| Box::new(source) as Box<dyn ConversationSource>),
        "omp" => OmpSource::new_with_source_id(project, root.path().to_path_buf(), id)
            .map(|source| Box::new(source) as Box<dyn ConversationSource>),
        "opencode" => OpenCodeSource::new_with_source_id(project, root.path().to_path_buf(), id)
            .map(|source| Box::new(source) as Box<dyn ConversationSource>),
        "pi" => PiSource::new_with_source_id(project, root.path().to_path_buf(), id)
            .map(|source| Box::new(source) as Box<dyn ConversationSource>),
        _ => return Err("configured conversation source is unsupported".to_owned()),
    }
    .map_err(|error| error.to_string())
}
fn load_live_conversations(
    root: &Path,
    binary: PathBuf,
    backend: crate::project::VcsBackendSelection,
    cancelled: &AtomicBool,
) -> LiveConversationJobResult {
    if cancelled.load(Ordering::Relaxed) {
        return LiveConversationJobResult::Cancelled;
    }
    let project = match resolve_project_context_with_backend(root, backend) {
        Ok(context) => context.conversation_identity().clone(),
        Err(_) => {
            return LiveConversationJobResult::Error("project identity is unavailable".to_owned());
        }
    };
    let sessions = match CommandHostClient::new(binary).live_sessions() {
        Ok(sessions) => prepare_live_conversations(sessions, &project),
        Err(error) => return LiveConversationJobResult::Error(error.to_string()),
    };
    if cancelled.load(Ordering::Relaxed) {
        LiveConversationJobResult::Cancelled
    } else {
        LiveConversationJobResult::Ready(LiveSnapshot {
            project,
            sessions,
            observed_at: SystemTime::now(),
        })
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
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, UNIX_EPOCH};
    use tempfile::TempDir;

    use super::{
        Controller, ConversationJobResult, ConversationPaths, configured_external_source_id,
        load_conversations,
    };
    use crate::config::PluginConfig;
    use crate::conversations::active::{
        merge_filesystem_snapshots, prepare_filesystem_conversations,
    };
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
            Some(session_id.to_owned()),
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

    #[test]
    fn degraded_cache_refresh_merges_fresh_rows_into_the_retained_snapshot() {
        let project_dir = TempDir::new().expect("project");
        let project = ProjectIdentity::from_canonical_root(project_dir.path().to_path_buf())
            .expect("project");
        let previous = prepare_filesystem_conversations(vec![
            conversation(&project, "pi", "updated", 1),
            conversation(&project, "codex-cli", "retained", 2),
        ]);
        let fresh = prepare_filesystem_conversations(vec![
            conversation(&project, "pi", "updated", 3),
            conversation(&project, "omp", "added", 4),
        ]);

        let merged = merge_filesystem_snapshots(&previous, fresh);

        assert_eq!(merged.conversations().len(), 3);
        assert_eq!(
            merged
                .conversations()
                .iter()
                .find(|conversation| conversation.session_reference().id() == "updated")
                .map(Conversation::updated_at),
            Some(UNIX_EPOCH + Duration::from_secs(3))
        );
        assert!(
            merged
                .conversations()
                .iter()
                .any(|conversation| { conversation.session_reference().id() == "retained" })
        );
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
    fn configured_project_history_root_is_discovered_without_recursive_scanning() {
        let temp = TempDir::new().expect("tempdir");
        let history = temp.path().join(".agents/history");
        fs::create_dir_all(&history).expect("history");
        fs::write(
            history.join("configured.jsonl"),
            format!(
                "{}\n",
                serde_json::json!({
                    "session_id": "configured-project-root",
                    "cwd": temp.path(),
                    "timestamp": "2026-01-02T03:04:05Z",
                    "role": "user",
                    "message": "private fixture",
                })
            ),
        )
        .expect("conversation");
        fs::write(
            history.join("second.jsonl"),
            format!(
                "{}\n",
                serde_json::json!({
                    "session_id": "configured-project-root-second",
                    "cwd": temp.path(),
                    "timestamp": "2026-01-02T03:04:06Z",
                    "role": "user",
                    "message": "second private fixture",
                })
            ),
        )
        .expect("second conversation");
        fs::write(
            temp.path().join("config.toml"),
            concat!(
                "[conversations]\n",
                "enabled_sources = [\"project-local-generic-jsonl\"]\n",
                "project_roots = [\".agents/history\"]\n",
                "page_size = 1\n",
            ),
        )
        .expect("config");
        let config = PluginConfig::load_from_dir(temp.path()).into_config();
        let cancelled = AtomicBool::new(false);

        let ConversationJobResult::Ready { conversations, .. } = load_conversations(
            temp.path(),
            &ConversationPaths::default(),
            &config,
            &cancelled,
            true,
        ) else {
            panic!("configured conversation discovery");
        };

        assert_eq!(conversations.conversations().len(), 2);
        assert!(conversations.conversations().iter().any(|conversation| {
            conversation.session_reference().id() == "configured-project-root"
        }));
        assert!(conversations.conversations().iter().any(|conversation| {
            conversation.session_reference().id() == "configured-project-root-second"
        }));

        let blocked_state = temp.path().join("blocked-state");
        fs::write(&blocked_state, b"not a directory").expect("blocked state fixture");
        let paths = ConversationPaths {
            state_dir: Some(blocked_state),
            home: None,
        };
        let ConversationJobResult::Ready {
            conversations,
            source_errors,
            ..
        } = load_conversations(temp.path(), &paths, &config, &AtomicBool::new(false), true)
        else {
            panic!("nonpersistent cache fallback");
        };
        assert_eq!(conversations.conversations().len(), 2);
        assert!(source_errors.iter().any(|error| {
            error.contains("metadata index is unavailable; using nonpersistent discovery")
        }));
    }

    #[test]
    fn configured_external_root_loads_without_home_or_cache_state() {
        let project = TempDir::new().expect("project");
        let project_identity = ProjectIdentity::from_canonical_root(project.path().to_path_buf())
            .expect("project identity");
        let store = TempDir::new().expect("Pi store");
        let encoded_project = format!(
            "--{}--",
            project_identity
                .root()
                .to_string_lossy()
                .trim_start_matches('/')
                .replace('/', "-")
        );
        let session = store
            .path()
            .join(&encoded_project)
            .join("2026-01-02T03-04-05-000Z_019b7ca9-8c88-7000-8003-000000000003.jsonl");
        fs::create_dir_all(session.parent().expect("session parent")).expect("session directory");
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/conversations/pi/--workspace-project--")
            .join("2026-01-02T03-04-05-000Z_019b7ca9-8c88-7000-8003-000000000003.jsonl");
        let fixture = fs::read_to_string(fixture).expect("Pi fixture").replace(
            "/workspace/project",
            project_identity.root().to_str().expect("UTF-8 root"),
        );
        fs::write(&session, &fixture).expect("installed Pi fixture");
        let home = TempDir::new().expect("home");
        let default_session = home
            .path()
            .join(".pi/agent/sessions")
            .join(&encoded_project)
            .join("2026-01-02T03-04-05-000Z_019b7ca9-8c88-7000-8003-000000000003.jsonl");
        fs::create_dir_all(default_session.parent().expect("default session parent"))
            .expect("default session directory");
        fs::write(default_session, &fixture).expect("installed default Pi fixture");
        let config_dir = TempDir::new().expect("config");
        fs::write(
            config_dir.path().join("config.toml"),
            format!(
                concat!(
                    "[conversations]\n",
                    "enabled_sources = [\"pi\"]\n",
                    "[conversations.external_roots]\n",
                    "pi = [\"{}\"]\n",
                ),
                store.path().display()
            ),
        )
        .expect("config");
        let config = PluginConfig::load_from_dir(config_dir.path()).into_config();

        let ConversationJobResult::Ready {
            conversations,
            source_errors,
            ..
        } = load_conversations(
            project.path(),
            &ConversationPaths::default(),
            &config,
            &AtomicBool::new(false),
            true,
        )
        else {
            panic!("configured external conversation discovery");
        };

        assert_eq!(conversations.conversations().len(), 1);
        assert_eq!(
            conversations.conversations()[0].session_reference().id(),
            "019b7ca9-8c88-7000-8003-000000000003"
        );
        assert!(
            source_errors
                .iter()
                .any(|error| { error.contains("pi: user home directory is unavailable") })
        );

        let state = TempDir::new().expect("state");
        let cached_paths = ConversationPaths {
            state_dir: Some(state.path().join("plugin-state")),
            home: Some(home.path().to_path_buf()),
        };
        let ConversationJobResult::Ready {
            conversations: cached,
            ..
        } = load_conversations(
            project.path(),
            &cached_paths,
            &config,
            &AtomicBool::new(false),
            true,
        )
        else {
            panic!("configured external cached conversation discovery");
        };
        assert_eq!(cached.conversations().len(), 1);
        let ConversationJobResult::Ready {
            conversations: reloaded,
            source_errors,
            ..
        } = load_conversations(
            project.path(),
            &cached_paths,
            &config,
            &AtomicBool::new(false),
            true,
        )
        else {
            panic!("reloaded configured external cache");
        };
        assert_eq!(reloaded.conversations().len(), 1);
        assert!(
            source_errors
                .iter()
                .all(|error| !error.contains("incompatible metadata index"))
        );

        let base_config_dir = TempDir::new().expect("base config");
        fs::write(
            base_config_dir.path().join("config.toml"),
            "[conversations]\nenabled_sources = [\"pi\"]\n",
        )
        .expect("base config");
        let base_config = PluginConfig::load_from_dir(base_config_dir.path()).into_config();
        let ConversationJobResult::Ready {
            conversations: surviving,
            ..
        } = load_conversations(
            project.path(),
            &cached_paths,
            &base_config,
            &AtomicBool::new(false),
            true,
        )
        else {
            panic!("surviving default source");
        };
        assert_eq!(surviving.conversations().len(), 1);
    }

    #[test]
    fn configured_external_root_identity_does_not_depend_on_sibling_roots() {
        let roots = TempDir::new().expect("roots");
        let first = roots.path().join("first");
        let second = roots.path().join("second");
        let both_config = TempDir::new().expect("both config");
        fs::write(
            both_config.path().join("config.toml"),
            format!(
                concat!(
                    "[conversations.external_roots]\n",
                    "pi = [\"{}\", \"{}\"]\n",
                ),
                first.display(),
                second.display()
            ),
        )
        .expect("both config");
        let both = PluginConfig::load_from_dir(both_config.path());
        let second_with_sibling = both
            .config()
            .conversations()
            .external_roots()
            .iter()
            .find(|root| root.path() == second)
            .map(configured_external_source_id)
            .expect("second root");
        let single_config = TempDir::new().expect("single config");
        fs::write(
            single_config.path().join("config.toml"),
            format!(
                "[conversations.external_roots]\npi = [\"{}\"]\n",
                second.display()
            ),
        )
        .expect("single config");
        let single = PluginConfig::load_from_dir(single_config.path());
        let second_alone =
            configured_external_source_id(&single.config().conversations().external_roots()[0]);

        assert_eq!(second_with_sibling, second_alone);
    }

    #[test]
    fn malformed_configuration_warns_without_blocking_subsystem_startup() {
        let temp = TempDir::new().expect("tempdir");
        fs::write(temp.path().join("config.toml"), "[dock").expect("config");
        let load = PluginConfig::load_from_dir(temp.path());
        let mut controller = controller(&temp);
        let mut workers = WorkerRuntime::with_capacities(2, 1);

        controller.apply_config_load(load, &mut workers);
        let area = Rect::new(0, 0, 80, 5);
        let mut buffer = Buffer::empty(area);
        controller.render(area, &mut buffer);
        let rendered = (0..area.height)
            .map(|row| rendered_line(&buffer, row))
            .collect::<String>();

        assert!(rendered.contains("Config: config.toml is malformed; using defaults"));
        assert!(!rendered.contains("Config: Config:"));
        assert!(workers.has_pending_work());
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
    #[cfg(unix)]
    #[test]
    fn filesystem_and_live_jobs_merge_coalesce_and_fail_independently() {
        use std::os::unix::fs::PermissionsExt;

        let project = TempDir::new().expect("project");
        let directory = project.path().join(".herdr/conversations");
        fs::create_dir_all(&directory).expect("conversation directory");
        fs::write(
            directory.join("filesystem.jsonl"),
            format!(
                "{}\n",
                serde_json::json!({
                    "session_id": "filesystem-session",
                    "cwd": project.path(),
                    "timestamp": "2026-01-02T03:04:05Z",
                    "role": "user",
                    "message": "private controller fixture",
                })
            ),
        )
        .expect("filesystem conversation");
        let live_id = "019b8721-4a18-7000-8005-000000000005";
        let response = serde_json::json!({
            "id": "test",
            "result": {
                "type": "agent_list",
                "agents": [{
                    "agent": "omp",
                    "agent_session": {
                        "source": "herdr:omp",
                        "agent": "omp",
                        "kind": "id",
                        "value": live_id,
                    },
                    "agent_status": "working",
                    "cwd": project.path(),
                    "foreground_cwd": project.path(),
                    "pane_id": "pane-live",
                    "title": "live only",
                }],
            },
        });
        let binary = project.path().join("fake-herdr");
        fs::write(
            &binary,
            format!("#!/bin/sh\nsleep 0.05\nprintf '%s\\n' '{response}'\n"),
        )
        .expect("fake Herdr");
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700))
            .expect("executable fake Herdr");

        let mut controller = controller(&project);
        controller.host_binary = Some(binary.clone());
        controller.model.set_active_view(View::Conversations);
        let mut workers = WorkerRuntime::with_capacities(2, 4);
        controller.start(&mut workers);
        controller.schedule_live_conversations(&mut workers);
        while workers.has_pending_work() {
            let result = workers
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("worker result");
            controller.apply_result(result, &mut workers);
        }

        assert!(
            controller
                .model
                .conversations()
                .items()
                .iter()
                .any(|item| item.session_reference().id() == "filesystem-session")
        );
        assert!(
            controller
                .model
                .conversations()
                .items()
                .iter()
                .any(|item| item.session_reference().id() == live_id)
        );
        let (requested, applied) = controller.model.conversations().live_generations();
        assert_eq!(requested, 2);
        assert_eq!(applied, requested);

        fs::write(&binary, "#!/bin/sh\nprintf 'not json\\n'\n").expect("broken fake Herdr");
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700))
            .expect("executable fake Herdr");
        controller.apply(Intent::Refresh, &mut workers);
        while workers.has_pending_work() {
            let result = workers
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("worker result");
            controller.apply_result(result, &mut workers);
        }

        assert_eq!(
            controller.model.conversations().items().len(),
            1,
            "Herdr failure must remove only transient live rows"
        );
        assert_eq!(
            controller.model.conversations().items()[0]
                .session_reference()
                .id(),
            "filesystem-session"
        );
        assert!(
            controller
                .model
                .conversations()
                .visible_errors()
                .iter()
                .any(|error| error.starts_with("Herdr:"))
        );
        workers.shutdown();
    }
    #[test]
    fn conversation_row_selection_survives_refresh_filter_and_view_switches() {
        let project_dir = TempDir::new().expect("project");
        let project = ProjectIdentity::from_canonical_root(project_dir.path().to_path_buf())
            .expect("project identity");
        let mut controller = controller(&project_dir);
        controller.model.set_active_view(View::Conversations);
        controller.model.conversations_mut().replace_items(
            vec![
                conversation(&project, "omp", "new", 20),
                conversation(&project, "omp", "old", 10),
            ],
            1,
        );
        let area = Rect::new(0, 0, 50, 6);
        controller.render(area, &mut Buffer::empty(area));
        let mut workers = WorkerRuntime::with_capacities(2, 1);

        assert!(controller.apply(Intent::SelectNext, &mut workers).dirty);
        assert_eq!(
            controller
                .model
                .conversations()
                .selection()
                .map(SessionReference::id),
            Some("new")
        );
        controller.model.conversations_mut().set_filter("no-match");
        controller.model.conversations_mut().replace_items(
            vec![
                conversation(&project, "omp", "new", 30),
                conversation(&project, "omp", "old", 10),
            ],
            2,
        );
        assert_eq!(
            controller
                .model
                .conversations()
                .selection()
                .map(SessionReference::id),
            Some("new")
        );
        controller.apply(Intent::SwitchView(View::Files), &mut workers);
        controller.apply(Intent::SwitchView(View::Conversations), &mut workers);
        assert_eq!(
            controller
                .model
                .conversations()
                .selection()
                .map(SessionReference::id),
            Some("new")
        );
        controller.model.conversations_mut().set_filter("");
        controller.render(area, &mut Buffer::empty(area));
        assert!(
            controller
                .apply(
                    Intent::Pointer {
                        column: area.x.saturating_add(3),
                        row: area.y.saturating_add(3),
                        action: PointerAction::Select,
                    },
                    &mut workers,
                )
                .dirty
        );
        assert_eq!(
            controller
                .model
                .conversations()
                .selection()
                .map(SessionReference::id),
            Some("old")
        );
        workers.shutdown();
    }

    #[test]
    fn filtered_navigation_enters_at_the_first_visible_row() {
        let project_dir = TempDir::new().expect("project");
        let project = ProjectIdentity::from_canonical_root(project_dir.path().to_path_buf())
            .expect("project identity");
        let mut controller = controller(&project_dir);
        controller.model.set_active_view(View::Conversations);
        controller.model.conversations_mut().replace_items(
            vec![
                conversation(&project, "omp", "hidden", 20),
                conversation(&project, "omp", "visible", 10),
            ],
            1,
        );
        controller.model.conversations_mut().set_selection(Some(
            SessionReference::new("omp", "hidden").expect("selection"),
        ));
        controller.model.conversations_mut().set_filter("visible");
        let area = Rect::new(0, 0, 50, 6);
        controller.render(area, &mut Buffer::empty(area));
        let mut workers = WorkerRuntime::with_capacities(2, 1);

        assert!(controller.apply(Intent::SelectNext, &mut workers).dirty);
        assert_eq!(controller.model.conversations().selection(), None);
        assert_eq!(
            controller.model.conversations().selected_provider(),
            Some("omp")
        );
        assert!(controller.apply(Intent::SelectNext, &mut workers).dirty);
        assert_eq!(
            controller
                .model
                .conversations()
                .selection()
                .map(SessionReference::id),
            Some("visible")
        );
        workers.shutdown();
    }
}
