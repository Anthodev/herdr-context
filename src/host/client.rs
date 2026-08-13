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
    DockIdentity, DockWidth, HostClient, HostError, HostErrorKind, HostPane, OpenDockRequest,
    PaneId, TabId, WorkspaceId,
};

pub const PLUGIN_ID: &str = "herdr-context";
pub const DOCK_ENTRYPOINT: &str = "dock";
pub const DOCK_TITLE: &str = "herdr-context dock";

const MAX_RIGHT_SWAPS: usize = 256;
const MAX_RESIZE_ATTEMPTS: usize = 8;
const MIN_OTHER_PANE_WIDTH: u16 = 10;
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
const COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(10);
const MAX_EXECUTABLE_BUSY_RETRIES: usize = 10;
const MAX_COMMAND_OUTPUT: u64 = 1024 * 1024;

/// `HostClient` backed only by Herdr's public argv CLI.
#[derive(Clone, Debug)]
pub struct CommandHostClient {
    binary: PathBuf,
    timeout: Duration,
}

impl CommandHostClient {
    pub fn from_env() -> Result<Self, HostError> {
        env::var_os("HERDR_BIN_PATH")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .map(Self::new)
            .ok_or_else(|| {
                HostError::new(
                    HostErrorKind::Unavailable,
                    "missing required variable HERDR_BIN_PATH",
                )
            })
    }

    #[must_use]
    pub const fn new(binary: PathBuf) -> Self {
        Self {
            binary,
            timeout: DEFAULT_COMMAND_TIMEOUT,
        }
    }

    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn invoke<I, S>(&self, args: I) -> Result<Value, HostError>
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
        let output = run_command(command, self.timeout).map_err(|error| {
            let kind = match error.kind() {
                io::ErrorKind::TimedOut => HostErrorKind::OperationFailed,
                io::ErrorKind::InvalidData => HostErrorKind::InvalidResponse,
                _ => HostErrorKind::Unavailable,
            };
            HostError::new(
                kind,
                format!("failed to execute {}: {error}", self.binary.display()),
            )
        })?;
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
            request.cwd().as_os_str().to_owned(),
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
