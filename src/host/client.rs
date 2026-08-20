use std::env;
use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use command_group::{CommandGroup, GroupChild};
use serde_json::Value;

use super::{
    AgentHarness, DockIdentity, DockWidth, HostAgentSession, HostAgentStatus, HostClient,
    HostError, HostErrorKind, HostPane, HostSessionReference, OpenDockRequest, PaneId,
    ResumeConversationRequest, TabId, WorkspaceId,
};

pub const PLUGIN_ID: &str = "herdr-context";
pub const DOCK_ENTRYPOINT: &str = "dock";
pub const DOCK_TITLE: &str = "herdr-context dock";

const MAX_RIGHT_SWAPS: usize = 256;
const MAX_FOCUS_STEPS: usize = 256;
const MAX_RESIZE_ATTEMPTS: usize = 8;
const MIN_OTHER_PANE_WIDTH: u16 = 10;
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_AGENT_READY_TIMEOUT: Duration = Duration::from_secs(30);
const COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(10);
const MAX_EXECUTABLE_BUSY_RETRIES: usize = 10;
const MAX_COMMAND_OUTPUT: u64 = 1024 * 1024;
const MAX_LIVE_SESSIONS: usize = 256;
const MAX_LIVE_ID_BYTES: usize = 256;
const MAX_LIVE_LABEL_BYTES: usize = 64;
const MAX_LIVE_PATH_BYTES: usize = 4_096;
const MAX_LIVE_TITLE_BYTES: usize = 256;

/// `HostClient` backed only by Herdr's public argv CLI.
#[derive(Clone, Debug)]
pub struct CommandHostClient {
    binary: PathBuf,
    plugin_root: Option<PathBuf>,
    timeout: Duration,
    agent_ready_timeout: Duration,
}

impl CommandHostClient {
    pub fn from_env() -> Result<Self, HostError> {
        let binary = env::var_os("HERDR_BIN_PATH")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| {
                HostError::new(
                    HostErrorKind::Unavailable,
                    "missing required variable HERDR_BIN_PATH",
                )
            })?;
        let plugin_root = env::var_os("HERDR_PLUGIN_ROOT")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| {
                HostError::new(
                    HostErrorKind::Unavailable,
                    "missing required variable HERDR_PLUGIN_ROOT",
                )
            })?;
        Ok(Self::new(binary).with_plugin_root(plugin_root))
    }

    #[must_use]
    pub const fn new(binary: PathBuf) -> Self {
        Self {
            binary,
            plugin_root: None,
            timeout: DEFAULT_COMMAND_TIMEOUT,
            agent_ready_timeout: DEFAULT_AGENT_READY_TIMEOUT,
        }
    }

    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
    #[must_use]
    pub const fn with_agent_ready_timeout(mut self, timeout: Duration) -> Self {
        self.agent_ready_timeout = timeout;
        self
    }

    #[must_use]
    pub fn with_plugin_root(mut self, plugin_root: PathBuf) -> Self {
        self.plugin_root = Some(plugin_root);
        self
    }

    fn run_with_timeout<I, S>(&self, args: I, timeout: Duration) -> Result<ProcessOutput, HostError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = Command::new(&self.binary);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        run_command(command, timeout).map_err(|error| {
            let kind = match error.kind() {
                io::ErrorKind::TimedOut => HostErrorKind::OperationFailed,
                io::ErrorKind::InvalidData => HostErrorKind::InvalidResponse,
                _ => HostErrorKind::Unavailable,
            };
            HostError::new(
                kind,
                format!("failed to execute {}: {error}", self.binary.display()),
            )
        })
    }

    fn invoke<I, S>(&self, args: I) -> Result<Value, HostError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.invoke_with_timeout(args, self.timeout)
    }

    fn invoke_with_timeout<I, S>(&self, args: I, timeout: Duration) -> Result<Value, HostError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.run_with_timeout(args, timeout)?;
        let response_bytes = if output.status.success() {
            &output.stdout
        } else if output.stdout.is_empty() {
            &output.stderr
        } else {
            &output.stdout
        };
        let response: Value = serde_json::from_slice(response_bytes).map_err(|error| {
            HostError::new(
                HostErrorKind::InvalidResponse,
                format!("Herdr returned invalid JSON: {error}"),
            )
        })?;

        if !output.status.success() {
            return Err(response_error(&response, &output.stderr));
        }
        response.get("result").cloned().ok_or_else(|| {
            HostError::new(
                HostErrorKind::InvalidResponse,
                "Herdr response is missing result",
            )
        })
    }

    fn invoke_without_response<I, S>(&self, args: I) -> Result<(), HostError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.invoke_without_response_with_timeout(args, self.timeout)
    }

    fn invoke_without_response_with_timeout<I, S>(
        &self,
        args: I,
        timeout: Duration,
    ) -> Result<(), HostError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.run_with_timeout(args, timeout)?;
        if output.status.success() {
            return Ok(());
        }
        let response_bytes = if output.stdout.is_empty() {
            &output.stderr
        } else {
            &output.stdout
        };
        let response: Value = serde_json::from_slice(response_bytes).map_err(|error| {
            HostError::new(
                HostErrorKind::InvalidResponse,
                format!("Herdr returned invalid JSON: {error}"),
            )
        })?;
        Err(response_error(&response, &output.stderr))
    }

    pub fn resume_conversation(
        &self,
        request: &ResumeConversationRequest,
    ) -> Result<(), HostError> {
        let operation_timeout = self
            .agent_ready_timeout
            .saturating_add(self.timeout.saturating_mul(4));
        let deadline = Instant::now()
            .checked_add(operation_timeout)
            .ok_or_else(conversation_launch_timeout)?;
        let create_timeout = command_timeout(deadline, self.timeout, self.timeout)?;
        let result = self.invoke_with_timeout(
            [
                OsString::from("tab"),
                OsString::from("create"),
                OsString::from("--workspace"),
                OsString::from(request.workspace_id().as_str()),
                OsString::from("--cwd"),
                request.cwd().as_os_str().to_owned(),
                OsString::from("--no-focus"),
            ],
            create_timeout,
        )?;
        let tab_id = TabId::new(required_string(&result, "/tab/tab_id", "created tab id")?)
            .map_err(HostError::from)?;
        let pane_id = match required_string(&result, "/root_pane/pane_id", "created root pane id")
            .and_then(|pane_id| PaneId::new(pane_id).map_err(HostError::from))
        {
            Ok(pane_id) => pane_id,
            Err(error) => {
                self.close_created_tab(&tab_id, deadline);
                return Err(error);
            }
        };

        let start_timeout = match command_timeout(
            deadline,
            self.timeout,
            self.agent_ready_timeout.saturating_add(self.timeout),
        ) {
            Ok(timeout) => timeout,
            Err(error) => {
                self.close_created_tab(&tab_id, deadline);
                return Err(error);
            }
        };
        let start_result = self.invoke_without_response_with_timeout(
            agent_start_arguments(request, &pane_id, self.agent_ready_timeout),
            start_timeout,
        );
        if let Err(error) = start_result {
            self.close_created_tab(&tab_id, deadline);
            return Err(error);
        }

        let focus_timeout = match command_timeout(deadline, self.timeout, self.timeout) {
            Ok(timeout) => timeout,
            Err(error) => {
                self.close_created_tab(&tab_id, deadline);
                return Err(error);
            }
        };
        if let Err(error) = self
            .invoke_without_response_with_timeout(["tab", "focus", tab_id.as_str()], focus_timeout)
        {
            self.close_created_tab(&tab_id, deadline);
            return Err(error);
        }
        Ok(())
    }

    fn close_created_tab(&self, tab_id: &TabId, deadline: Instant) {
        let timeout = deadline
            .saturating_duration_since(Instant::now())
            .min(self.timeout);
        if !timeout.is_zero() {
            let _ = self
                .invoke_without_response_with_timeout(["tab", "close", tab_id.as_str()], timeout);
        }
    }

    fn layout(&self, pane_id: &PaneId) -> Result<Value, HostError> {
        let result = self.invoke(["pane", "layout", "--pane", pane_id.as_str()])?;
        expect_type(&result, "pane_layout")?;
        result.get("layout").cloned().ok_or_else(|| {
            HostError::new(
                HostErrorKind::InvalidResponse,
                "pane layout response is missing layout",
            )
        })
    }
}

fn command_timeout(
    deadline: Instant,
    reserve: Duration,
    limit: Duration,
) -> Result<Duration, HostError> {
    let available = deadline
        .saturating_duration_since(Instant::now())
        .saturating_sub(reserve)
        .min(limit);
    if available.is_zero() {
        return Err(conversation_launch_timeout());
    }
    Ok(available)
}

fn conversation_launch_timeout() -> HostError {
    HostError::new(
        HostErrorKind::OperationFailed,
        "conversation launch timed out",
    )
}

fn agent_start_arguments(
    request: &ResumeConversationRequest,
    pane_id: &PaneId,
    ready_timeout: Duration,
) -> Vec<OsString> {
    let harness = request.harness();
    let timeout_ms = ready_timeout.as_millis().clamp(1, 300_000).to_string();
    let mut args = vec![
        OsString::from("agent"),
        OsString::from("start"),
        OsString::from(harness.as_str()),
        OsString::from("--kind"),
        OsString::from(harness.as_str()),
        OsString::from("--pane"),
        OsString::from(pane_id.as_str()),
        OsString::from("--timeout"),
        OsString::from(timeout_ms),
        OsString::from("--"),
    ];
    match harness {
        AgentHarness::Claude | AgentHarness::Omp => {
            args.push(OsString::from("--resume"));
            args.push(OsString::from(request.reference()));
        }
        AgentHarness::Codex => {
            args.push(OsString::from("resume"));
            args.push(OsString::from(request.reference()));
        }
        AgentHarness::OpenCode | AgentHarness::Pi => {
            args.push(OsString::from("--session"));
            args.push(OsString::from(request.reference()));
        }
    }
    args
}

impl HostClient for CommandHostClient {
    fn pane(&self, pane_id: &PaneId) -> Result<Option<HostPane>, HostError> {
        let result = match self.invoke(["pane", "get", pane_id.as_str()]) {
            Ok(result) => result,
            Err(error) if error.kind() == HostErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        expect_type(&result, "pane_info")?;
        result
            .get("pane")
            .map(parse_pane)
            .transpose()
            .and_then(|pane| {
                pane.ok_or_else(|| {
                    HostError::new(
                        HostErrorKind::InvalidResponse,
                        "pane response is missing pane",
                    )
                })
                .map(Some)
            })
    }

    fn panes_in_tab(
        &self,
        workspace_id: &WorkspaceId,
        tab_id: &TabId,
    ) -> Result<Vec<HostPane>, HostError> {
        let result = self.invoke(["pane", "list", "--workspace", workspace_id.as_str()])?;
        expect_type(&result, "pane_list")?;
        let panes = result
            .get("panes")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                HostError::new(
                    HostErrorKind::InvalidResponse,
                    "pane list response is missing panes",
                )
            })?;
        panes
            .iter()
            .filter(|pane| pane.get("tab_id").and_then(Value::as_str) == Some(tab_id.as_str()))
            .map(parse_pane)
            .collect()
    }

    fn live_sessions(&self) -> Result<Vec<HostAgentSession>, HostError> {
        let result = self.invoke(["agent", "list"])?;
        expect_type(&result, "agent_list")?;
        let agents = result
            .get("agents")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                HostError::new(
                    HostErrorKind::InvalidResponse,
                    "agent list response is missing agents",
                )
            })?;
        let mut sessions = Vec::new();
        for agent in agents
            .iter()
            .filter(|agent| !agent.get("agent_session").is_none_or(Value::is_null))
        {
            if sessions.len() == MAX_LIVE_SESSIONS {
                return Err(HostError::new(
                    HostErrorKind::InvalidResponse,
                    "agent list exceeds the live session limit",
                ));
            }
            sessions.push(parse_live_session(agent)?);
        }
        Ok(sessions)
    }

    fn send_text(&self, pane_id: &PaneId, text: &str) -> Result<(), HostError> {
        self.invoke_without_response(["pane", "send-text", pane_id.as_str(), text])
    }

    fn focus_origin_pane(
        &self,
        dock_pane_id: &PaneId,
        origin_pane_id: &PaneId,
    ) -> Result<(), HostError> {
        if dock_pane_id == origin_pane_id {
            return Ok(());
        }
        let deadline = Instant::now().checked_add(self.timeout).ok_or_else(|| {
            HostError::new(
                HostErrorKind::OperationFailed,
                "pane focus timeout is too large",
            )
        })?;
        let mut current = dock_pane_id.clone();
        for _ in 0..MAX_FOCUS_STEPS {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let result = self.invoke_with_timeout(
                [
                    "pane",
                    "focus",
                    "--direction",
                    "left",
                    "--pane",
                    current.as_str(),
                ],
                remaining,
            )?;
            expect_type(&result, "pane_focus_direction")?;
            let focused = required_string(&result, "/focus/focused_pane_id", "focused pane id")?;
            if focused == origin_pane_id.as_str() {
                return Ok(());
            }
            let next = PaneId::new(focused).map_err(HostError::from)?;
            if next == current {
                break;
            }
            current = next;
        }
        Err(HostError::new(
            HostErrorKind::OperationFailed,
            format!(
                "could not focus origin pane {} from dock {}",
                origin_pane_id.as_str(),
                dock_pane_id.as_str()
            ),
        ))
    }

    fn verified_dock_identity(
        &mut self,
        pane: &HostPane,
    ) -> Result<Option<DockIdentity>, HostError> {
        if pane.dock_identity().is_none() {
            return Ok(None);
        }
        let result = match self.invoke(["plugin", "pane", "focus", pane.pane_id().as_str()]) {
            Ok(result) => result,
            Err(error) if error.kind() == HostErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        expect_type(&result, "plugin_pane_focused")?;
        let plugin_id = required_string(&result, "/plugin_pane/plugin_id", "plugin id")?;
        let entrypoint =
            required_string(&result, "/plugin_pane/entrypoint", "plugin pane entrypoint")?;
        Ok((plugin_id == PLUGIN_ID && entrypoint == DOCK_ENTRYPOINT)
            .then_some(DockIdentity::PluginMetadata))
    }

    fn open_dock(&mut self, request: &OpenDockRequest) -> Result<PaneId, HostError> {
        let mut origin_cwd = OsString::from("HERDR_CONTEXT_ORIGIN_CWD=");
        origin_cwd.push(request.cwd().as_os_str());
        let mut origin_pane_id = OsString::from("HERDR_CONTEXT_ORIGIN_PANE_ID=");
        origin_pane_id.push(request.origin_pane_id().as_str());
        let pane_cwd = self.plugin_root.as_deref().unwrap_or_else(|| request.cwd());
        let args = vec![
            OsString::from("plugin"),
            OsString::from("pane"),
            OsString::from("open"),
            OsString::from("--plugin"),
            OsString::from(PLUGIN_ID),
            OsString::from("--entrypoint"),
            OsString::from(DOCK_ENTRYPOINT),
            OsString::from("--placement"),
            OsString::from("split"),
            OsString::from("--target-pane"),
            OsString::from(request.origin_pane_id().as_str()),
            OsString::from("--direction"),
            OsString::from("right"),
            OsString::from("--cwd"),
            pane_cwd.as_os_str().to_owned(),
            OsString::from("--env"),
            origin_cwd,
            OsString::from("--env"),
            origin_pane_id,
            OsString::from("--focus"),
        ];
        let result = self.invoke(args)?;
        expect_type(&result, "plugin_pane_opened")?;
        let pane_id = required_string(&result, "/plugin_pane/pane/pane_id", "opened pane id")?;
        PaneId::new(pane_id).map_err(HostError::from)
    }

    fn focus_pane(&mut self, pane_id: &PaneId) -> Result<(), HostError> {
        let result = self.invoke(["plugin", "pane", "focus", pane_id.as_str()])?;
        expect_type(&result, "plugin_pane_focused")
    }

    fn close_pane(&mut self, pane_id: &PaneId) -> Result<(), HostError> {
        let result = self.invoke(["plugin", "pane", "close", pane_id.as_str()])?;
        expect_type(&result, "plugin_pane_closed")
    }

    fn move_to_right_edge(&mut self, pane_id: &PaneId) -> Result<(), HostError> {
        for _ in 0..MAX_RIGHT_SWAPS {
            let result = self.invoke([
                "pane",
                "swap",
                "--direction",
                "right",
                "--pane",
                pane_id.as_str(),
            ])?;
            expect_type(&result, "pane_swap")?;
            let changed = result
                .pointer("/swap/changed")
                .and_then(Value::as_bool)
                .ok_or_else(|| {
                    HostError::new(
                        HostErrorKind::InvalidResponse,
                        "pane swap response is missing changed",
                    )
                })?;
            if !changed {
                return Ok(());
            }
        }
        Err(HostError::new(
            HostErrorKind::OperationFailed,
            "pane did not reach the right edge within the swap bound",
        ))
    }

    fn resize_pane(&mut self, pane_id: &PaneId, width: DockWidth) -> Result<(), HostError> {
        for _ in 0..MAX_RESIZE_ATTEMPTS {
            let layout = self.layout(pane_id)?;
            let area_width = required_u16(&layout, "/area/width", "layout width")?;
            let current_width = pane_width(&layout, pane_id)?;
            let maximum = area_width.saturating_sub(MIN_OTHER_PANE_WIDTH).max(1);
            let desired = width.columns().min(maximum);
            if current_width.abs_diff(desired) <= 1 {
                return Ok(());
            }
            let direction = if current_width > desired {
                "right"
            } else {
                "left"
            };
            let amount = (f32::from(current_width.abs_diff(desired))
                / f32::from(area_width.max(1)))
            .clamp(0.005, 0.5);
            let amount = amount.to_string();
            let result = self.invoke([
                "pane",
                "resize",
                "--direction",
                direction,
                "--amount",
                &amount,
                "--pane",
                pane_id.as_str(),
            ])?;
            expect_type(&result, "pane_resize")?;
            if result.pointer("/resize/changed").and_then(Value::as_bool) != Some(true) {
                break;
            }
        }
        Err(HostError::new(
            HostErrorKind::OperationFailed,
            format!(
                "could not resize pane {} to {} columns",
                pane_id.as_str(),
                width.columns()
            ),
        ))
    }
}

fn parse_live_session(value: &Value) -> Result<HostAgentSession, HostError> {
    let session = value
        .get("agent_session")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            HostError::new(
                HostErrorKind::InvalidResponse,
                "agent response has invalid session metadata",
            )
        })?;
    let source = bounded_string(
        session.get("source"),
        "live session source",
        MAX_LIVE_LABEL_BYTES,
    )?;
    if !source.starts_with("herdr:") {
        return Err(HostError::new(
            HostErrorKind::InvalidResponse,
            "live session source is not an official Herdr integration",
        ));
    }
    let agent = bounded_string(
        session.get("agent"),
        "live session agent",
        MAX_LIVE_LABEL_BYTES,
    )?;
    if let Some(reported) = value.get("agent").filter(|reported| !reported.is_null()) {
        let reported = bounded_string(Some(reported), "agent identity", MAX_LIVE_LABEL_BYTES)?;
        if reported != agent {
            return Err(HostError::new(
                HostErrorKind::InvalidResponse,
                "agent and live session identities conflict",
            ));
        }
    }
    let kind = bounded_string(
        session.get("kind"),
        "live session reference kind",
        MAX_LIVE_LABEL_BYTES,
    )?;
    let reference = match kind {
        "id" => HostSessionReference::NativeId(
            bounded_string(
                session.get("value"),
                "live session native ID",
                MAX_LIVE_ID_BYTES,
            )?
            .to_owned(),
        ),
        "path" => {
            let raw = bounded_string(
                session.get("value"),
                "live session transcript path",
                MAX_LIVE_PATH_BYTES,
            )?;
            let path = PathBuf::from(raw);
            if !path.is_absolute() {
                return Err(HostError::new(
                    HostErrorKind::InvalidResponse,
                    "live session transcript path is not absolute",
                ));
            }
            HostSessionReference::TranscriptPath(path)
        }
        _ => {
            return Err(HostError::new(
                HostErrorKind::InvalidResponse,
                "live session reference kind is unsupported",
            ));
        }
    };
    let pane_id = PaneId::new(bounded_string(
        value.get("pane_id"),
        "live session pane ID",
        MAX_LIVE_LABEL_BYTES,
    )?)?;
    let cwd = optional_bounded_path(value.get("cwd"), "live session cwd")?;
    let foreground_cwd =
        optional_bounded_path(value.get("foreground_cwd"), "live session foreground cwd")?;
    let title = optional_bounded_string(
        value.get("title"),
        "live session title",
        MAX_LIVE_TITLE_BYTES,
    )?;
    let status = match bounded_string(
        value.get("agent_status"),
        "live session agent status",
        MAX_LIVE_LABEL_BYTES,
    )? {
        "idle" => HostAgentStatus::Idle,
        "working" => HostAgentStatus::Working,
        "blocked" => HostAgentStatus::Blocked,
        "done" => HostAgentStatus::Done,
        "unknown" => HostAgentStatus::Unknown,
        _ => {
            return Err(HostError::new(
                HostErrorKind::InvalidResponse,
                "live session agent status is unsupported",
            ));
        }
    };
    HostAgentSession::new(
        source,
        agent,
        reference,
        pane_id,
        cwd,
        foreground_cwd,
        title,
        status,
    )
}

fn bounded_string<'a>(
    value: Option<&'a Value>,
    field: &str,
    max_bytes: usize,
) -> Result<&'a str, HostError> {
    value
        .and_then(Value::as_str)
        .filter(|value| {
            !value.is_empty()
                && value.len() <= max_bytes
                && value.trim() == *value
                && !value.contains('\0')
        })
        .ok_or_else(|| {
            HostError::new(
                HostErrorKind::InvalidResponse,
                format!("Herdr response has invalid {field}"),
            )
        })
}

fn optional_bounded_string(
    value: Option<&Value>,
    field: &str,
    max_bytes: usize,
) -> Result<Option<String>, HostError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => bounded_string(Some(value), field, max_bytes)
            .map(str::to_owned)
            .map(Some),
    }
}

fn optional_bounded_path(value: Option<&Value>, field: &str) -> Result<Option<PathBuf>, HostError> {
    let Some(raw) = optional_bounded_string(value, field, MAX_LIVE_PATH_BYTES)? else {
        return Ok(None);
    };
    let path = PathBuf::from(raw);
    if !path.is_absolute() {
        return Err(HostError::new(
            HostErrorKind::InvalidResponse,
            format!("Herdr response has non-absolute {field}"),
        ));
    }
    Ok(Some(path))
}

fn parse_pane(value: &Value) -> Result<HostPane, HostError> {
    let pane_id = PaneId::new(required_string(value, "/pane_id", "pane id")?)?;
    let tab_id = TabId::new(required_string(value, "/tab_id", "tab id")?)?;
    let cwd = optional_path(value, "/cwd", "pane cwd")?;
    let foreground_cwd = optional_path(value, "/foreground_cwd", "foreground cwd")?;
    let focused = value
        .get("focused")
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            HostError::new(
                HostErrorKind::InvalidResponse,
                "pane response is missing focused",
            )
        })?;
    let pane = HostPane::new(pane_id, tab_id, cwd, foreground_cwd, focused);
    let plugin_metadata_matches = value.get("label").and_then(Value::as_str) == Some(DOCK_TITLE);
    let osc_matches = value
        .get("terminal_title_stripped")
        .and_then(Value::as_str)
        .or_else(|| value.get("terminal_title").and_then(Value::as_str))
        == Some(DOCK_TITLE);
    Ok(if plugin_metadata_matches {
        pane.with_dock_identity(DockIdentity::PluginMetadata)
    } else if osc_matches {
        pane.with_dock_identity(DockIdentity::OscTitle)
    } else {
        pane
    })
}

fn pane_width(layout: &Value, pane_id: &PaneId) -> Result<u16, HostError> {
    layout
        .get("panes")
        .and_then(Value::as_array)
        .and_then(|panes| {
            panes
                .iter()
                .find(|pane| pane.get("pane_id").and_then(Value::as_str) == Some(pane_id.as_str()))
        })
        .map(|pane| required_u16(pane, "/rect/width", "pane width"))
        .transpose()?
        .ok_or_else(|| HostError::new(HostErrorKind::NotFound, "pane is absent from layout"))
}

fn expect_type(result: &Value, expected: &str) -> Result<(), HostError> {
    match result.get("type").and_then(Value::as_str) {
        Some(actual) if actual == expected => Ok(()),
        Some(actual) => Err(HostError::new(
            HostErrorKind::InvalidResponse,
            format!("expected Herdr result type {expected}, got {actual}"),
        )),
        None => Err(HostError::new(
            HostErrorKind::InvalidResponse,
            "Herdr result is missing type",
        )),
    }
}

fn required_string(value: &Value, pointer: &str, field: &str) -> Result<String, HostError> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            HostError::new(
                HostErrorKind::InvalidResponse,
                format!("Herdr response is missing {field}"),
            )
        })
}

fn required_u16(value: &Value, pointer: &str, field: &str) -> Result<u16, HostError> {
    value
        .pointer(pointer)
        .and_then(Value::as_u64)
        .and_then(|number| u16::try_from(number).ok())
        .ok_or_else(|| {
            HostError::new(
                HostErrorKind::InvalidResponse,
                format!("Herdr response has invalid {field}"),
            )
        })
}

fn optional_path(value: &Value, pointer: &str, field: &str) -> Result<Option<PathBuf>, HostError> {
    let Some(raw) = value.pointer(pointer) else {
        return Ok(None);
    };
    if raw.is_null() {
        return Ok(None);
    }
    let path = raw.as_str().map(Path::new).ok_or_else(|| {
        HostError::new(
            HostErrorKind::InvalidResponse,
            format!("Herdr response has invalid {field}"),
        )
    })?;
    if !path.is_absolute() {
        return Err(HostError::new(
            HostErrorKind::InvalidResponse,
            format!("Herdr response has non-absolute {field}"),
        ));
    }
    Ok(Some(path.to_path_buf()))
}

struct ProcessOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn run_command(mut command: Command, timeout: Duration) -> io::Result<ProcessOutput> {
    let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "command timeout is too large")
    })?;
    let mut child = spawn_command(&mut command, deadline)?;
    let stdout = child
        .inner()
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("command stdout is unavailable"))?;
    let stderr = child
        .inner()
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("command stderr is unavailable"))?;
    let stdout_reader = thread::spawn(move || read_bounded(stdout));
    let stderr_reader = thread::spawn(move || read_bounded(stderr));

    let mut exit_status = None;
    let status = loop {
        if exit_status.is_none() {
            exit_status = child.try_wait()?;
        }
        if let Some(status) = exit_status
            && stdout_reader.is_finished()
            && stderr_reader.is_finished()
        {
            break status;
        }
        if Instant::now() >= deadline {
            terminate_group(&mut child);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Herdr command timed out",
            ));
        }
        thread::sleep(COMMAND_POLL_INTERVAL);
    };
    let stdout = join_reader(stdout_reader, "stdout");
    let stderr = join_reader(stderr_reader, "stderr");
    terminate_remaining_group(&mut child);
    Ok(ProcessOutput {
        status,
        stdout: stdout?,
        stderr: stderr?,
    })
}

fn spawn_command(command: &mut Command, deadline: Instant) -> io::Result<GroupChild> {
    let mut busy_retries = 0;
    loop {
        match command.group_spawn() {
            Ok(child) => return Ok(child),
            Err(error)
                if error.kind() == io::ErrorKind::Interrupted
                    || (executable_is_busy(&error)
                        && busy_retries < MAX_EXECUTABLE_BUSY_RETRIES) =>
            {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "Herdr command timed out before it could start",
                    ));
                }
                busy_retries += usize::from(executable_is_busy(&error));
                thread::sleep(COMMAND_POLL_INTERVAL);
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(unix)]
fn executable_is_busy(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::ETXTBSY)
}

#[cfg(not(unix))]
fn executable_is_busy(_error: &io::Error) -> bool {
    false
}

fn read_bounded(reader: impl Read) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_COMMAND_OUTPUT + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_COMMAND_OUTPUT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Herdr command output exceeded limit",
        ));
    }
    Ok(bytes)
}

fn join_reader(
    reader: thread::JoinHandle<io::Result<Vec<u8>>>,
    stream: &str,
) -> io::Result<Vec<u8>> {
    reader
        .join()
        .map_err(|_| io::Error::other(format!("Herdr {stream} reader panicked")))?
}

fn terminate_group(child: &mut GroupChild) {
    let _ = child.kill();
    let _ = child.wait();
}

fn terminate_remaining_group(child: &mut GroupChild) {
    let _ = child.kill();
}

fn response_error(response: &Value, stderr: &[u8]) -> HostError {
    let code = response
        .pointer("/error/code")
        .and_then(Value::as_str)
        .unwrap_or("operation_failed");
    let message = response
        .pointer("/error/message")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| capped_lossy(stderr));
    let kind = match code {
        "pane_not_found" | "workspace_not_found" | "plugin_not_found" | "plugin_pane_not_found" => {
            HostErrorKind::NotFound
        }
        "permission_denied" => HostErrorKind::PermissionDenied,
        "server_unavailable" => HostErrorKind::Unavailable,
        _ => HostErrorKind::OperationFailed,
    };
    HostError::new(kind, format!("Herdr {code}: {message}"))
}

fn capped_lossy(bytes: &[u8]) -> String {
    const LIMIT: usize = 512;
    String::from_utf8_lossy(&bytes[..bytes.len().min(LIMIT)]).into_owned()
}
impl From<super::LaunchContextError> for HostError {
    fn from(error: super::LaunchContextError) -> Self {
        Self::new(HostErrorKind::InvalidResponse, error.to_string())
    }
}
