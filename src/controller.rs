//! Intent transitions and orchestration between state and bounded workers.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::env;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::config::{ConfigLoad, ExternalHistoryRoot, KeyAction, KeyBindings, PluginConfig};
use crate::conversations::active::{
    FilesystemConversationSnapshot, LiveConversationSnapshot, merge_filesystem_snapshots,
    merge_prepared_live_sessions, prepare_filesystem_conversations, prepare_live_conversations,
};
use crate::conversations::discovery::discover_conversations_cancellable;
use crate::conversations::index::{ConversationIndex, IndexStatus};
use crate::conversations::sources::{
    ClaudeCodeSource, CodexCliSource, ConversationSource, ConversationSourceError,
    ConversationSourceErrorKind, DiscoveryLimit, GenericJsonlSource, KnownStoreRoots,
    MetadataBudget, OmpSource, OpenCodeSource, PiSource, ProjectLocalLocation, SourceId,
    SourceRegistry,
};
use crate::conversations::{Conversation, ResumeCapability};
use crate::host::client::CommandHostClient;
use crate::host::{AgentHarness, HostClient, LaunchContext, ResumeConversationRequest};
use crate::input::InputMode;
use crate::intent::{Intent, View};
use crate::model::{AppModel, LoadingState, NoticeSeverity, VisibleError};
use crate::project::{ProjectIdentity, resolve_project_context_with_backend};
use crate::runtime::{FilesRuntime, RuntimeMessage};
use crate::ui::render_shell;
use crate::worker::{CompletedJob, Job, JobKey, JobKind, Priority, SubmitStatus, WorkerRuntime};

const MAX_PENDING_PANE_INPUTS: usize = 16;

struct ConfigResult(ConfigLoad);
struct BootstrapResult(Result<FilesRuntime, crate::runtime::FilesRuntimeError>);
struct ConversationsResult(ConversationJobResult);
struct LiveConversationsResult(LiveConversationJobResult);
struct ConversationLaunchResult(Result<(), crate::host::HostError>);
struct PaneInputResult(Result<(), crate::host::HostError>);

enum ConversationJobResult {
    Ready {
        project: ProjectIdentity,
        conversations: FilesystemConversationSnapshot,
        has_more: bool,
        source_errors: Vec<VisibleError>,
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

/// Adaptive poller for Herdr live agent sessions driving real-time conversation creation.
#[derive(Debug)]
struct LiveSensor {
    minimum: Duration,
    maximum: Duration,
    interval: Duration,
    next_tick: Instant,
    fingerprint: Option<u64>,
    references: BTreeMap<String, String>,
    pending_generation: Option<u64>,
}

impl LiveSensor {
    fn new(minimum: Duration, maximum: Duration, now: Instant) -> Self {
        Self {
            minimum,
            maximum,
            interval: minimum,
            next_tick: now + minimum,
            fingerprint: None,
            references: BTreeMap::new(),
            pending_generation: None,
        }
    }

    fn arm_reset(&mut self, now: Instant) {
        self.interval = self.minimum;
        self.next_tick = now + self.minimum;
    }

    fn arm_backoff(&mut self, now: Instant) {
        self.interval = self
            .interval
            .saturating_mul(2)
            .clamp(self.minimum, self.maximum);
        self.next_tick = now + self.interval;
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
    live_sensor: Option<LiveSensor>,
    conversation_launch_generation: u64,
    conversation_launch_running: bool,
    pane_input_generation: u64,
    pane_input_queue: VecDeque<String>,
    pane_input_running: bool,
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
            conversation_launch_generation: 0,
            conversation_launch_running: false,
            pane_input_generation: 0,
            pane_input_queue: VecDeque::new(),
            pane_input_running: false,
            live_snapshot: None,
            live_sensor: None,
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
            conversation_launch_generation: 0,
            conversation_launch_running: false,
            pane_input_generation: 0,
            pane_input_queue: VecDeque::new(),
            pane_input_running: false,
            live_snapshot: None,
            live_sensor: None,
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
        self.live_sensor = config
            .conversations()
            .live_cadence()
            .adaptive()
            .map(|(minimum, maximum)| LiveSensor::new(minimum, maximum, Instant::now()));
        self.config = config;
        self.model.set_display_mode(self.config.ui().display_mode());
        let search_hint = self
            .config
            .keybindings()
            .bindings_for(KeyAction::Search)
            .first()
            .map_or_else(String::new, |binding| format!("{binding} search"));
        self.model.set_search_hint(search_hint);
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
        self.schedule_live_poll(workers, false);
    }

    fn schedule_live_poll(&mut self, workers: &mut WorkerRuntime, quiet: bool) {
        let Some(binary) = self.host_binary.clone() else {
            if !quiet {
                self.model.conversations_mut().set_live_loading(false);
            }
            return;
        };
        let (requested, applied) = self.model.conversations().live_generations();
        let generation = requested.saturating_add(1);
        self.model
            .conversations_mut()
            .set_live_generations(generation, applied);
        if quiet {
            let Some(sensor) = self.live_sensor.as_mut() else {
                return;
            };
            sensor.pending_generation = Some(generation);
            let now = Instant::now();
            sensor.next_tick = now + sensor.interval;
        } else {
            self.model.conversations_mut().set_live_loading(true);
            if let Some(sensor) = self.live_sensor.as_mut() {
                sensor.pending_generation = None;
            }
        }
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
            if quiet {
                self.sync_and_back_off_live_sensor(generation, generation);
                return;
            }
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

    fn take_pending_live_sensor(&mut self, generation: u64) -> bool {
        match self.live_sensor.as_mut() {
            Some(sensor) if sensor.pending_generation == Some(generation) => {
                sensor.pending_generation = None;
                true
            }
            _ => false,
        }
    }

    fn sync_and_back_off_live_sensor(&mut self, requested: u64, generation: u64) {
        self.model
            .conversations_mut()
            .set_live_generations(requested, generation);
        if let Some(sensor) = self.live_sensor.as_mut() {
            sensor.arm_backoff(Instant::now());
        }
    }

    /// Applies one quiet sensor snapshot: merge on change, discover only on reference drift.
    fn apply_sensor_snapshot(
        &mut self,
        snapshot: LiveSnapshot,
        generation: u64,
        requested: u64,
        workers: &mut WorkerRuntime,
    ) -> bool {
        let fingerprint = snapshot.sessions.fingerprint();
        if self
            .live_sensor
            .as_ref()
            .is_some_and(|sensor| sensor.fingerprint == Some(fingerprint))
        {
            self.model
                .conversations_mut()
                .set_live_generations(requested, generation);
            self.model.conversations_mut().set_live_loading(false);
            if let Some(sensor) = self.live_sensor.as_mut() {
                sensor.arm_backoff(Instant::now());
            }
            return false;
        }
        let reference_changed = {
            let references = snapshot.sessions.pane_references();
            self.live_sensor.as_ref().is_some_and(|sensor| {
                references.iter().any(|(pane, reference)| {
                    sensor.references.get(*pane).map(String::as_str) != Some(*reference)
                })
            })
        };
        self.seed_live_baseline(&snapshot);
        self.live_snapshot = Some(snapshot);
        self.model.conversations_mut().set_live_error(None);
        let visible = self.merged_conversations();
        let changed = self
            .model
            .conversations_mut()
            .replace_live_items(visible, generation);
        if reference_changed {
            self.schedule_conversation_page(workers, false);
        }
        changed
    }

    /// Re-baselines the sensor after any applied live snapshot, loud or quiet.
    fn seed_live_baseline(&mut self, snapshot: &LiveSnapshot) {
        let fingerprint = snapshot.sessions.fingerprint();
        let references: BTreeMap<String, String> = snapshot
            .sessions
            .pane_references()
            .into_iter()
            .map(|(pane, reference)| (pane.to_owned(), reference.to_owned()))
            .collect();
        if let Some(sensor) = self.live_sensor.as_mut() {
            sensor.fingerprint = Some(fingerprint);
            sensor.references = references;
            sensor.arm_reset(Instant::now());
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
            Intent::ToggleSelected if self.model.active_view() == View::Conversations => {
                let conversation = self.model.conversations().selected_conversation().cloned();
                let dirty = match conversation {
                    Some(conversation) => self.launch_conversation(conversation, workers),
                    None => {
                        let area = self.model.geometry().content();
                        self.model
                            .conversations_mut()
                            .handle_intent(&Intent::ToggleSelected, area)
                    }
                };
                Transition { dirty, quit: false }
            }
            intent if self.model.active_view() == View::Conversations => {
                let area = self.model.geometry().content();
                let dirty = self.model.conversations_mut().handle_intent(&intent, area);
                Transition { dirty, quit: false }
            }
            Intent::ToggleSelected if self.model.active_view() == View::Files => {
                let reference = self
                    .files
                    .as_ref()
                    .and_then(FilesRuntime::selected_file_reference);
                let dirty =
                    match reference {
                        Some(Ok(reference)) => self.send_file_reference(reference, workers),
                        Some(Err(message)) => self.files.as_mut().is_some_and(|files| {
                            files.set_pane_input_notice(Some(message.to_owned()))
                        }),
                        None => self.files.as_mut().is_some_and(|files| {
                            files.handle_intent(&Intent::ToggleSelected, workers)
                        }),
                    };
                self.sync_files_state();
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

    fn launch_conversation(
        &mut self,
        conversation: Conversation,
        workers: &mut WorkerRuntime,
    ) -> bool {
        if self.conversation_launch_running {
            return false;
        }
        let reference = match conversation.resume_capability() {
            ResumeCapability::Supported(reference) => reference.as_str(),
            ResumeCapability::Unsupported => {
                self.model
                    .conversations_mut()
                    .set_launch_error(Some("selected conversation cannot be resumed".to_owned()));
                return true;
            }
        };
        let Some(harness) = AgentHarness::from_tool(conversation.tool().as_str()) else {
            self.model
                .conversations_mut()
                .set_launch_error(Some(format!(
                    "no supported harness for {}",
                    conversation.tool().as_str()
                )));
            return true;
        };
        let Some(binary) = self.host_binary.clone() else {
            self.model
                .conversations_mut()
                .set_launch_error(Some("Herdr conversation launch is unavailable".to_owned()));
            return true;
        };
        let request = match ResumeConversationRequest::new(
            self.model.launch_context().workspace_id().clone(),
            conversation.project_identity().root().to_path_buf(),
            harness,
            reference,
        ) {
            Ok(request) => request,
            Err(error) => {
                self.model
                    .conversations_mut()
                    .set_launch_error(Some(format!("cannot open conversation: {error}")));
                return true;
            }
        };
        self.conversation_launch_generation = self.conversation_launch_generation.saturating_add(1);
        let generation = self.conversation_launch_generation;
        let key = JobKey::new(JobKind::ConversationLaunch, request.cwd());
        let job = Job::new(key, generation, Priority::High, move |cancelled| {
            let result = if cancelled.load(Ordering::Relaxed) {
                Err(crate::host::HostError::new(
                    crate::host::HostErrorKind::OperationFailed,
                    "conversation launch was cancelled",
                ))
            } else {
                CommandHostClient::new(binary).resume_conversation(&request)
            };
            Box::new(ConversationLaunchResult(result))
        });
        if accepted(workers.submit(job)) {
            self.conversation_launch_running = true;
            self.model.conversations_mut().set_launch_error(None);
            return true;
        }
        self.model.conversations_mut().set_launch_error(Some(
            "Herdr conversation launch queue is unavailable".to_owned(),
        ));
        true
    }

    fn send_file_reference(&mut self, reference: String, workers: &mut WorkerRuntime) -> bool {
        if self.pane_input_queue.len() >= MAX_PENDING_PANE_INPUTS {
            return self.files.as_mut().is_some_and(|files| {
                files.set_pane_input_notice(Some("Herdr pane input queue is full".to_owned()))
            });
        }
        self.pane_input_queue.push_back(reference);
        self.start_next_pane_input(workers)
    }

    fn start_next_pane_input(&mut self, workers: &mut WorkerRuntime) -> bool {
        if self.pane_input_running {
            return false;
        }
        let Some(reference) = self.pane_input_queue.pop_front() else {
            return false;
        };
        let Some(binary) = self.host_binary.clone() else {
            self.pane_input_queue.clear();
            return self.files.as_mut().is_some_and(|files| {
                files.set_pane_input_notice(Some("Herdr pane input is unavailable".to_owned()))
            });
        };
        let origin_pane_id = self.model.launch_context().focused_pane_id().clone();
        let dock_pane_id = self.model.launch_context().runtime_pane_id().clone();
        self.pane_input_generation = self.pane_input_generation.saturating_add(1);
        let generation = self.pane_input_generation;
        let job = Job::new(
            JobKey::new(JobKind::PaneInput, origin_pane_id.as_str()),
            generation,
            Priority::High,
            move |_| {
                let client = CommandHostClient::new(binary);
                let result = client
                    .send_text(&origin_pane_id, &reference)
                    .and_then(|()| client.focus_origin_pane(&dock_pane_id, &origin_pane_id));
                Box::new(PaneInputResult(result))
            },
        );
        if accepted(workers.submit(job)) {
            self.pane_input_running = true;
            return false;
        }
        self.pane_input_queue.clear();
        self.files.as_mut().is_some_and(|files| {
            files.set_pane_input_notice(Some("Herdr pane input queue is unavailable".to_owned()))
        })
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
                files.finish_search_editing();
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
            if matches!(
                kind,
                JobKind::Filesystem
                    | JobKind::FileSearch
                    | JobKind::FileSearchProjection
                    | JobKind::Vcs
            ) && let Some(files) = &mut self.files
            {
                files.fail_background(kind, generation, workers);
            } else if kind == JobKind::ConversationLive {
                let requested = self.model.conversations().live_generations().0;
                if generation < requested {
                    return false;
                }
                if self.take_pending_live_sensor(generation) {
                    self.sync_and_back_off_live_sensor(requested, generation);
                    return true;
                }
                self.live_snapshot = None;
                self.model.conversations_mut().set_live_error(Some(
                    "Herdr live session worker stopped unexpectedly".to_owned(),
                ));
                let visible = self.merged_conversations();
                self.model
                    .conversations_mut()
                    .replace_live_items(visible, generation);
            } else if kind == JobKind::ConversationLaunch {
                self.conversation_launch_running = false;
                self.model.conversations_mut().set_launch_error(Some(
                    "Herdr conversation launch worker stopped unexpectedly".to_owned(),
                ));
            } else if kind == JobKind::PaneInput {
                self.pane_input_running = false;
                self.set_worker_error(kind);
                self.start_next_pane_input(workers);
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
                        let degraded_cache = source_errors.iter().any(|error| {
                            error
                                .message()
                                .starts_with("Cache: metadata index is unavailable")
                        });
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
                        visible_errors
                            .sort_unstable_by(|left, right| left.message().cmp(right.message()));
                        visible_errors.dedup_by(|left, right| left.message() == right.message());
                        visible_errors.truncate(8);
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
                let quiet = self.take_pending_live_sensor(generation);
                let Ok(result) = result.downcast::<LiveConversationsResult>() else {
                    if quiet {
                        self.sync_and_back_off_live_sensor(requested, generation);
                        return false;
                    }
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
                        if quiet {
                            return self
                                .apply_sensor_snapshot(snapshot, generation, requested, workers);
                        }
                        self.seed_live_baseline(&snapshot);
                        self.live_snapshot = Some(snapshot);
                        self.model.conversations_mut().set_live_error(None);
                    }
                    LiveConversationJobResult::Cancelled => {
                        if quiet {
                            self.sync_and_back_off_live_sensor(requested, generation);
                            return false;
                        }
                        self.model
                            .conversations_mut()
                            .set_live_generations(requested, generation);
                        self.model.conversations_mut().set_live_loading(false);
                        return true;
                    }
                    LiveConversationJobResult::Error(message) => {
                        if quiet {
                            self.sync_and_back_off_live_sensor(requested, generation);
                            return false;
                        }
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
            JobKind::ConversationLaunch => {
                self.conversation_launch_running = false;
                let launch_error = result.downcast::<ConversationLaunchResult>().map_or_else(
                    |_| Some("invalid Herdr conversation launch worker result".to_owned()),
                    |result| {
                        result
                            .0
                            .err()
                            .map(|error| format!("cannot open conversation: {error}"))
                    },
                );
                self.model
                    .conversations_mut()
                    .set_launch_error(launch_error);
                true
            }
            JobKind::PaneInput => {
                self.pane_input_running = false;
                let notice = result.downcast::<PaneInputResult>().map_or_else(
                    |_| Some("invalid Herdr pane input worker result".to_owned()),
                    |result| {
                        result
                            .0
                            .err()
                            .map(|error| format!("cannot insert file reference: {error}"))
                    },
                );
                let changed = self
                    .files
                    .as_mut()
                    .is_some_and(|files| files.set_pane_input_notice(notice));
                self.start_next_pane_input(workers) || changed
            }
            JobKind::Filesystem
            | JobKind::FileSearch
            | JobKind::FileSearchProjection
            | JobKind::Vcs => {
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
            JobKind::ConversationLaunch => {
                self.conversation_launch_running = false;
                self.model.conversations_mut().set_launch_error(Some(
                    "Herdr conversation launch worker stopped unexpectedly".to_owned(),
                ));
            }
            JobKind::PaneInput => {
                if let Some(files) = &mut self.files {
                    files.set_pane_input_notice(Some(
                        "Herdr pane input worker stopped unexpectedly".to_owned(),
                    ));
                }
            }
            JobKind::Bootstrap
            | JobKind::Filesystem
            | JobKind::FileSearch
            | JobKind::FileSearchProjection
            | JobKind::Vcs
            | JobKind::Process => {
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
        self.model.files_mut().set_filter(files.search_query());
        self.model
            .files_mut()
            .set_search_editing(files.search_editing());
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

    pub(super) fn input_mode(&self) -> InputMode {
        if self.model.active_view() != View::Files {
            return InputMode::Normal;
        }
        self.files.as_ref().map_or(InputMode::Normal, |files| {
            if files.search_editing() {
                InputMode::FileSearch
            } else if files.has_search_filter() {
                InputMode::FileSearchActive
            } else {
                InputMode::Normal
            }
        })
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
        let files_wait = if self.model.active_view() == View::Files {
            self.files
                .as_ref()
                .and_then(|files| files.next_refresh_in(now))
        } else {
            None
        };
        let sensor_wait = if self.model.active_view() == View::Conversations {
            self.live_sensor
                .as_ref()
                .map(|sensor| sensor.next_tick.saturating_duration_since(now))
        } else {
            None
        };
        match (files_wait, sensor_wait) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (left, right) => left.or(right),
        }
    }

    pub(super) fn tick(&mut self, now: Instant, workers: &mut WorkerRuntime) -> bool {
        let files_ticked = self.model.active_view() == View::Files
            && self
                .files
                .as_mut()
                .is_some_and(|files| files.tick(now, workers));
        files_ticked || self.tick_live_sensor(now, workers)
    }

    fn tick_live_sensor(&mut self, now: Instant, workers: &mut WorkerRuntime) -> bool {
        let due = self.model.active_view() == View::Conversations
            && self.host_binary.is_some()
            && self
                .live_sensor
                .as_ref()
                .is_some_and(|sensor| sensor.next_tick <= now);
        if !due {
            return false;
        }
        self.schedule_live_poll(workers, true);
        false
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
                            IndexStatus::RebuiltCorrupt => source_errors.push(VisibleError::quiet(
                                "Cache: corrupt metadata index was rebuilt".to_owned(),
                            )),
                            IndexStatus::RebuiltIncompatible => {
                                source_errors.push(VisibleError::quiet(
                                    "Cache: incompatible metadata index was rebuilt".to_owned(),
                                ))
                            }
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
                        errors.push(VisibleError::quiet(
                            "Cache: metadata index is unavailable; using nonpersistent discovery"
                                .to_owned(),
                        ));
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
                errors.push(VisibleError::quiet(
                    "Cache: metadata index is unavailable; using nonpersistent discovery"
                        .to_owned(),
                ));
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
    source_errors.extend(setup_errors.into_iter().map(VisibleError::alert));
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
) -> (Vec<Conversation>, Vec<VisibleError>) {
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

fn source_error_messages(errors: &[ConversationSourceError]) -> Vec<VisibleError> {
    let severity = |kind: ConversationSourceErrorKind| match kind {
        ConversationSourceErrorKind::UnsupportedFormat
        | ConversationSourceErrorKind::MalformedData
        | ConversationSourceErrorKind::InvalidData => NoticeSeverity::Quiet,
        ConversationSourceErrorKind::Unavailable
        | ConversationSourceErrorKind::PermissionDenied
        | ConversationSourceErrorKind::SourceMismatch
        | ConversationSourceErrorKind::ProjectMismatch
        | ConversationSourceErrorKind::Io => NoticeSeverity::Alert,
    };
    let mut notices = errors
        .iter()
        .map(|error| {
            VisibleError::new(
                severity(error.kind()),
                format!("{:?}: {error}", error.kind()),
            )
        })
        .collect::<Vec<_>>();
    notices.sort_unstable_by(|left, right| left.message().cmp(right.message()));
    notices.dedup_by(|left, right| left.message() == right.message());
    notices.truncate(8);
    notices
}

const fn accepted(status: SubmitStatus) -> bool {
    matches!(status, SubmitStatus::Queued | SubmitStatus::Coalesced)
}

#[cfg(test)]
mod tests {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant, UNIX_EPOCH};
    use tempfile::TempDir;

    use super::{
        Controller, ConversationJobResult, ConversationPaths, configured_external_source_id,
        load_conversations,
    };
    use crate::config::{LiveCadence, PluginConfig};
    use crate::conversations::active::{
        merge_filesystem_snapshots, prepare_filesystem_conversations,
    };
    use crate::conversations::{
        Conversation, ConversationProvenance, ConversationState, ProvenanceKind, ResumeCapability,
        ResumeReference, SessionReference, SourceId, ToolIdentity,
    };
    use crate::host::LaunchContext;
    use crate::input::InputMode;
    use crate::intent::{Intent, PointerAction, View};
    use crate::model::{LoadingState, NoticeSeverity};
    use crate::project::ProjectIdentity;
    use crate::runtime::FilesRuntime;
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

    #[cfg(unix)]
    #[test]
    fn files_enter_inserts_selected_reference_into_origin_pane() {
        let project = TempDir::new().expect("project");
        fs::write(project.path().join("main.rs"), []).expect("file");
        let host = TempDir::new().expect("host");
        let argv = host.path().join("argv.log");
        let script = host.path().join("fake-herdr");
        fs::write(
            &script,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$*" in
  "pane send-text origin @main.rs ")
    ;;
  "pane focus --direction left --pane dock")
    printf '%s\n' '{{"id":"test","result":{{"type":"pane_focus_direction","focus":{{"changed":true,"focused_pane_id":"origin","source_pane_id":"dock"}}}}}}'
    ;;
  *)
    exit 1
    ;;
esac
"#,
                argv.display()
            ),
        )
        .expect("script");
        let mut permissions = fs::metadata(&script).expect("metadata").permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&script, permissions).expect("permissions");

        let context = LaunchContext::from_vars([
            (
                "HERDR_PLUGIN_CONTEXT_JSON",
                format!(
                    r#"{{"workspace_id":"workspace","tab_id":"tab","pane_id":"dock","cwd":"{}"}}"#,
                    project.path().display()
                ),
            ),
            ("HERDR_CONTEXT_ORIGIN_PANE_ID", "origin".to_owned()),
        ])
        .expect("context");
        let mut controller = Controller::new(context);
        controller.files = Some(
            FilesRuntime::bootstrap(controller.model.launch_context()).expect("Files runtime"),
        );
        controller
            .model
            .files_mut()
            .set_loading(LoadingState::Ready);
        controller.host_binary = Some(script);
        let mut workers = WorkerRuntime::with_capacities(2, 1);

        let transition = controller.apply(Intent::ToggleSelected, &mut workers);
        assert!(!transition.quit);
        let result = workers
            .recv_timeout(Duration::from_secs(1))
            .expect("pane input result");
        assert_eq!(result.key().kind(), JobKind::PaneInput);
        controller.apply_result(result, &mut workers);
        workers.shutdown();

        assert_eq!(
            fs::read_to_string(argv).expect("argv"),
            "pane send-text origin @main.rs \npane focus --direction left --pane dock\n"
        );
    }

    fn conversation(
        project: &ProjectIdentity,
        tool: &str,
        session_id: &str,
        updated_seconds: u64,
    ) -> Conversation {
        conversation_with_resume(
            project,
            tool,
            session_id,
            updated_seconds,
            ResumeCapability::Unsupported,
        )
    }

    fn resumable_conversation(
        project: &ProjectIdentity,
        tool: &str,
        session_id: &str,
    ) -> Conversation {
        conversation_with_resume(
            project,
            tool,
            session_id,
            1,
            ResumeCapability::Supported(ResumeReference::new(session_id).expect("resume")),
        )
    }

    fn conversation_with_resume(
        project: &ProjectIdentity,
        tool: &str,
        session_id: &str,
        updated_seconds: u64,
        resume: ResumeCapability,
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
            resume,
        )
        .expect("conversation")
    }

    #[cfg(unix)]
    #[test]
    fn conversation_enter_launches_one_resumed_harness_in_a_new_tab() {
        let project_dir = TempDir::new().expect("project");
        let project = ProjectIdentity::from_canonical_root(project_dir.path().to_path_buf())
            .expect("project identity");
        let host = TempDir::new().expect("host");
        let argv = host.path().join("argv.log");
        let script = host.path().join("fake-herdr");
        fs::write(
            &script,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$*" in
  tab\ create*)
    printf '%s\n' '{{"id":"test","result":{{"type":"tab_created","tab":{{"tab_id":"created-tab"}},"root_pane":{{"pane_id":"created-pane"}}}}}}'
    ;;
  agent\ start*|"tab focus created-tab")
    ;;
  *)
    exit 1
    ;;
esac
"#,
                argv.display(),
            ),
        )
        .expect("script");
        let mut permissions = fs::metadata(&script).expect("metadata").permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&script, permissions).expect("permissions");

        let mut controller = controller(&project_dir);
        controller.host_binary = Some(script);
        controller.model.set_active_view(View::Conversations);
        controller.model.conversations_mut().replace_items(
            vec![resumable_conversation(&project, "omp", "session-id")],
            1,
        );
        let area = Rect::new(0, 0, 80, 8);
        controller.render(area, &mut Buffer::empty(area));
        let mut workers = WorkerRuntime::with_capacities(2, 1);

        assert!(controller.apply(Intent::SelectNext, &mut workers).dirty);
        assert!(controller.apply(Intent::ToggleSelected, &mut workers).dirty);
        assert!(!controller.apply(Intent::ToggleSelected, &mut workers).dirty);
        let result = workers
            .recv_timeout(Duration::from_secs(1))
            .expect("conversation launch result");
        assert_eq!(result.key().kind(), JobKind::ConversationLaunch);
        assert!(controller.apply_result(result, &mut workers));
        workers.shutdown();

        assert!(controller.model.conversations().visible_errors().is_empty());
        assert_eq!(
            fs::read_to_string(argv).expect("argv"),
            format!(
                "tab create --workspace workspace --cwd {} --no-focus\nagent start omp --kind omp --pane created-pane --timeout 30000 -- --resume session-id\ntab focus created-tab\n",
                project.root().display()
            )
        );
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
        assert!(rendered_line(&buffer, 1).starts_with("- codex-cli (2)"));
        assert!(rendered_line(&buffer, 2).starts_with("  ? codex-new"));
        assert!(rendered_line(&buffer, 3).starts_with("  ? codex-old"));
        assert!(rendered_line(&buffer, 4).starts_with("+ pi (1)"));
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
        assert!((1..small.height).any(|row| rendered_line(&paged, row).starts_with("- pi (1)")));

        let large = Rect::new(0, 0, 40, 15);
        let mut grown = Buffer::empty(large);
        controller.render(large, &mut grown);
        assert_eq!(controller.model.conversations().scroll(), 0);
        assert!((1..large.height).any(|row| rendered_line(&grown, row).starts_with("- pi (1)")));

        let one_row = Rect::new(0, 0, 40, 2);
        let mut shrunk = Buffer::empty(one_row);
        controller.render(one_row, &mut shrunk);
        assert!(rendered_line(&shrunk, 1).starts_with("- pi (1)"));
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
        let area = Rect::new(0, 0, 20, 2);
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
        assert!(rendered.contains("f child"));
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
            error
                .message()
                .contains("metadata index is unavailable; using nonpersistent discovery")
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
        assert!(source_errors.iter().any(|error| {
            error
                .message()
                .contains("pi: user home directory is unavailable")
        }));

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
                .all(|error| !error.message().contains("incompatible metadata index"))
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
    fn missing_optional_configuration_does_not_render_a_warning_row() {
        let temp = TempDir::new().expect("tempdir");
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

        assert!(!rendered.contains("Config:"));
        assert_eq!(controller.model.geometry().content().y, 1);
        assert!(workers.has_pending_work());
        workers.shutdown();
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
                .any(|error| error.message().starts_with("Herdr:"))
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

    #[test]
    fn file_search_mode_and_query_survive_view_switches() {
        let project = TempDir::new().expect("project");
        fs::write(project.path().join("main.rs"), []).expect("file");
        let mut controller = controller(&project);
        controller.files = Some(
            FilesRuntime::bootstrap(controller.model.launch_context()).expect("Files runtime"),
        );
        controller
            .model
            .files_mut()
            .set_loading(LoadingState::Ready);
        let mut workers = WorkerRuntime::with_capacities(2, 2);

        assert!(
            controller
                .apply(Intent::BeginFileSearch, &mut workers)
                .dirty
        );
        assert_eq!(controller.input_mode(), InputMode::FileSearch);
        assert!(
            controller
                .apply(Intent::FileSearchInput("main".to_owned()), &mut workers)
                .dirty
        );
        assert_eq!(controller.model.files().filter(), "main");
        assert!(
            controller
                .apply(Intent::FileSearchCommit, &mut workers)
                .dirty
        );
        assert_eq!(controller.input_mode(), InputMode::FileSearchActive);

        assert!(
            controller
                .apply(Intent::SwitchView(View::Conversations), &mut workers)
                .dirty
        );
        assert_eq!(controller.input_mode(), InputMode::Normal);
        assert!(
            controller
                .apply(Intent::SwitchView(View::Files), &mut workers)
                .dirty
        );
        assert_eq!(controller.input_mode(), InputMode::FileSearchActive);
        assert_eq!(controller.model.files().filter(), "main");
        workers.shutdown();
    }

    fn write_agent_list_fixture(binary: &Path, agents: &str) {
        let response =
            format!(r#"{{"id":"test","result":{{"type":"agent_list","agents":[{agents}]}}}}"#);
        fs::write(
            binary,
            format!("#!/bin/sh\nsleep 0.02\nprintf '%s\\n' '{response}'\n"),
        )
        .expect("fake Herdr script");
        fs::set_permissions(binary, fs::Permissions::from_mode(0o700)).expect("executable");
    }

    fn omp_agent_json(cwd: &Path, pane: &str, session_id: &str) -> String {
        serde_json::json!({
            "agent": "omp",
            "agent_session": {
                "source": "herdr:omp",
                "agent": "omp",
                "kind": "id",
                "value": session_id,
            },
            "agent_status": "working",
            "cwd": cwd,
            "foreground_cwd": cwd,
            "pane_id": pane,
            "title": "sensor fixture",
        })
        .to_string()
    }

    fn adaptive_config_dir(min_ms: u64, max_ms: u64) -> TempDir {
        let dir = TempDir::new().expect("config dir");
        fs::write(
            dir.path().join("config.toml"),
            format!(
                "[conversations]\nlive_cadence = \"adaptive\"\nlive_min_interval_ms = {min_ms}\nlive_max_interval_ms = {max_ms}\n"
            ),
        )
        .expect("adaptive config");
        dir
    }

    fn drain_workers(controller: &mut Controller, workers: &mut WorkerRuntime) {
        while workers.has_pending_work() {
            let result = workers
                .recv_timeout(Duration::from_secs(2))
                .expect("worker result");
            controller.apply_result(result, workers);
        }
    }

    #[test]
    fn live_cadence_configuration_parses_with_validation() {
        let empty = TempDir::new().expect("empty config dir");
        assert!(matches!(
            PluginConfig::load_from_dir(empty.path())
                .into_config()
                .conversations()
                .live_cadence(),
            LiveCadence::Manual
        ));

        let adaptive = adaptive_config_dir(250, 2_000);
        match PluginConfig::load_from_dir(adaptive.path())
            .into_config()
            .conversations()
            .live_cadence()
        {
            LiveCadence::Adaptive { minimum, maximum } => {
                assert_eq!(minimum, Duration::from_millis(250));
                assert_eq!(maximum, Duration::from_millis(2_000));
            }
            LiveCadence::Manual => panic!("adaptive cadence was expected"),
        }

        let broken = TempDir::new().expect("broken config dir");
        fs::write(
            broken.path().join("config.toml"),
            "[conversations]\nlive_cadence = \"turbo\"\n",
        )
        .expect("broken config");
        let (config, warnings) = PluginConfig::load_from_dir(broken.path()).into_parts();
        assert!(matches!(
            config.conversations().live_cadence(),
            LiveCadence::Manual
        ));
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("conversations.live_cadence"))
        );

        let inverted = TempDir::new().expect("inverted config dir");
        fs::write(
            inverted.path().join("config.toml"),
            "[conversations]\nlive_cadence = \"adaptive\"\nlive_min_interval_ms = 5000\nlive_max_interval_ms = 1000\n",
        )
        .expect("inverted config");
        let (config, warnings) = PluginConfig::load_from_dir(inverted.path()).into_parts();
        match config.conversations().live_cadence() {
            LiveCadence::Adaptive { minimum, maximum } => {
                assert_eq!(minimum, Duration::from_millis(2_000));
                assert_eq!(maximum, Duration::from_millis(30_000));
            }
            LiveCadence::Manual => panic!("fallback must stay adaptive"),
        }
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("conversations.live_cadence"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn live_sensor_polls_quietly_backs_off_and_pauses_outside_view() {
        let project = TempDir::new().expect("project");
        let binary = project.path().join("fake-herdr");
        let session_a = "019b8721-4a18-7000-8005-000000000005";
        write_agent_list_fixture(
            &binary,
            &omp_agent_json(project.path(), "pane-live", session_a),
        );

        let mut controller = controller(&project);
        controller.host_binary = Some(binary);
        let mut workers = WorkerRuntime::with_capacities(2, 4);
        controller.model.set_active_view(View::Conversations);
        let config_dir = adaptive_config_dir(250, 2_000);
        controller.apply_config_load(PluginConfig::load_from_dir(config_dir.path()), &mut workers);
        drain_workers(&mut controller, &mut workers);

        assert_eq!(
            controller.model.conversations().live_generations(),
            (1, 1),
            "startup live load must seed the sensor baseline"
        );
        assert!(
            controller
                .model
                .conversations()
                .items()
                .iter()
                .any(|item| item.session_reference().id() == session_a)
        );

        controller.tick(Instant::now() + Duration::from_secs(1), &mut workers);
        drain_workers(&mut controller, &mut workers);
        assert_eq!(controller.model.conversations().generations().0, 1);
        assert!(!controller.model.conversations().live_loading());
        let wait = controller
            .next_refresh_in(Instant::now())
            .expect("sensor deadline");
        assert!(
            wait > Duration::from_millis(250) && wait <= Duration::from_millis(500),
            "unchanged poll must back off past the minimum, got {wait:?}"
        );

        controller.apply(Intent::SwitchView(View::Files), &mut workers);
        drain_workers(&mut controller, &mut workers);
        let before = controller.model.conversations().live_generations();
        controller.tick(Instant::now() + Duration::from_secs(60), &mut workers);
        drain_workers(&mut controller, &mut workers);
        assert_eq!(
            before,
            controller.model.conversations().live_generations(),
            "sensor must pause outside the Conversations view"
        );

        controller.apply(Intent::SwitchView(View::Conversations), &mut workers);
        controller.tick(Instant::now() + Duration::from_secs(60), &mut workers);
        assert!(
            workers.has_pending_work(),
            "re-entering the view must fire an immediate catch-up poll"
        );
        drain_workers(&mut controller, &mut workers);
        workers.shutdown();
    }

    #[cfg(unix)]
    #[test]
    fn live_sensor_reference_change_triggers_discovery_once_and_resets_backoff() {
        let project = TempDir::new().expect("project");
        let binary = project.path().join("fake-herdr");
        let session_a = "019b8721-4a18-7000-8005-000000000005";
        write_agent_list_fixture(
            &binary,
            &omp_agent_json(project.path(), "pane-live", session_a),
        );

        let mut controller = controller(&project);
        controller.host_binary = Some(binary.clone());
        let mut workers = WorkerRuntime::with_capacities(2, 4);
        controller.model.set_active_view(View::Conversations);
        let config_dir = adaptive_config_dir(250, 2_000);
        controller.apply_config_load(PluginConfig::load_from_dir(config_dir.path()), &mut workers);
        drain_workers(&mut controller, &mut workers);
        controller.tick(Instant::now() + Duration::from_secs(1), &mut workers);
        drain_workers(&mut controller, &mut workers);
        assert_eq!(controller.model.conversations().generations().0, 1);

        let session_b = "019b8721-4a18-7000-8005-000000000006";
        write_agent_list_fixture(
            &binary,
            &omp_agent_json(project.path(), "pane-live", session_b),
        );
        controller.tick(Instant::now() + Duration::from_secs(1), &mut workers);
        drain_workers(&mut controller, &mut workers);
        assert_eq!(
            controller.model.conversations().generations().0,
            2,
            "reference change must trigger exactly one quiet discovery"
        );
        assert!(
            controller
                .model
                .conversations()
                .items()
                .iter()
                .any(|item| item.session_reference().id() == session_b)
        );
        let wait = controller
            .next_refresh_in(Instant::now())
            .expect("reset deadline");
        assert!(
            wait <= Duration::from_millis(250),
            "reference change must reset the interval to the minimum, got {wait:?}"
        );

        controller.tick(Instant::now() + Duration::from_secs(1), &mut workers);
        drain_workers(&mut controller, &mut workers);
        assert_eq!(
            controller.model.conversations().generations().0,
            2,
            "stable references must not re-trigger discovery"
        );
        workers.shutdown();
    }

    #[cfg(unix)]
    #[test]
    fn live_sensor_failures_back_off_silently_and_keep_rows() {
        let project = TempDir::new().expect("project");
        let binary = project.path().join("fake-herdr");
        let session_a = "019b8721-4a18-7000-8005-000000000005";
        write_agent_list_fixture(
            &binary,
            &omp_agent_json(project.path(), "pane-live", session_a),
        );

        let mut controller = controller(&project);
        controller.host_binary = Some(binary.clone());
        let mut workers = WorkerRuntime::with_capacities(2, 4);
        controller.model.set_active_view(View::Conversations);
        let config_dir = adaptive_config_dir(250, 2_000);
        controller.apply_config_load(PluginConfig::load_from_dir(config_dir.path()), &mut workers);
        drain_workers(&mut controller, &mut workers);
        controller.tick(Instant::now() + Duration::from_secs(1), &mut workers);
        drain_workers(&mut controller, &mut workers);

        fs::write(&binary, "#!/bin/sh\nprintf 'not json\\n'\n").expect("broken fake Herdr");
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).expect("executable");
        controller.tick(Instant::now() + Duration::from_secs(5), &mut workers);
        drain_workers(&mut controller, &mut workers);

        assert!(
            !controller
                .model
                .conversations()
                .visible_errors()
                .iter()
                .any(|error| error.message().starts_with("Herdr:")),
            "quiet sensor failures must not surface error notices"
        );
        assert!(
            controller
                .model
                .conversations()
                .items()
                .iter()
                .any(|item| item.session_reference().id() == session_a),
            "quiet failures must retain the last known live rows"
        );
        assert!(!controller.model.conversations().live_loading());
        let wait = controller
            .next_refresh_in(Instant::now())
            .expect("backoff deadline after failure");
        assert!(wait > Duration::from_millis(250));
        workers.shutdown();
    }

    #[cfg(unix)]
    #[test]
    fn manual_refresh_seeds_baseline_without_redundant_sensor_discovery() {
        let project = TempDir::new().expect("project");
        let binary = project.path().join("fake-herdr");
        let session_a = "019b8721-4a18-7000-8005-000000000005";
        write_agent_list_fixture(
            &binary,
            &omp_agent_json(project.path(), "pane-live", session_a),
        );

        let mut controller = controller(&project);
        controller.host_binary = Some(binary);
        let mut workers = WorkerRuntime::with_capacities(2, 4);
        controller.model.set_active_view(View::Conversations);
        let config_dir = adaptive_config_dir(250, 2_000);
        controller.apply_config_load(PluginConfig::load_from_dir(config_dir.path()), &mut workers);
        drain_workers(&mut controller, &mut workers);

        controller.apply(Intent::Refresh, &mut workers);
        drain_workers(&mut controller, &mut workers);
        let discovery_after_refresh = controller.model.conversations().generations().0;

        controller.tick(Instant::now() + Duration::from_secs(1), &mut workers);
        drain_workers(&mut controller, &mut workers);
        assert_eq!(
            controller.model.conversations().generations().0,
            discovery_after_refresh,
            "manual refresh baseline must prevent a redundant sensor discovery"
        );
        workers.shutdown();
    }

    #[test]
    fn source_error_severity_splits_parse_rejections_from_environmental_failures() {
        use crate::conversations::sources::{ConversationSourceError, ConversationSourceErrorKind};

        let id = |name: &str| SourceId::new(name).expect("static source ID is valid");
        let errors = [
            ConversationSourceError::new(
                id("codex-cli"),
                ConversationSourceErrorKind::UnsupportedFormat,
                "malformed record",
            ),
            ConversationSourceError::new(
                id("pi"),
                ConversationSourceErrorKind::Io,
                "store unreadable",
            ),
        ];

        let notices = super::source_error_messages(&errors);
        let severity_of = |prefix: &str| {
            notices
                .iter()
                .find(|notice| notice.message().starts_with(prefix))
                .map(|notice| notice.severity())
        };
        assert_eq!(
            severity_of("UnsupportedFormat:"),
            Some(NoticeSeverity::Quiet)
        );
        assert_eq!(severity_of("Io:"), Some(NoticeSeverity::Alert));
    }
}
