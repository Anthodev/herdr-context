use std::collections::BTreeSet;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use command_group::{CommandGroup, GroupChild};

use super::{
    VcsBackendMetadata, VcsEntryStatus, VcsError, VcsErrorKind, VcsService, VcsStatusKind,
    VcsStatusSnapshot, VcsWorkspace,
};

const GIT_BACKEND_ID: &str = "git";
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const DEFAULT_OUTPUT_LIMITS: OutputLimits = OutputLimits {
    stdout: 64 * 1024 * 1024,
    stderr: 1024 * 1024,
};
const FILTER_CONFIG_OUTPUT_LIMITS: OutputLimits = OutputLimits {
    stdout: 1024 * 1024,
    stderr: 1024 * 1024,
};
const MAX_FILTER_OVERRIDES: usize = 1024;
const MAX_STATUS_ENTRIES: usize = 100_000;

#[derive(Clone, Copy, Debug)]
struct OutputLimits {
    stdout: usize,
    stderr: usize,
}

/// Read-only Git adapter with a bounded, configuration-independent process boundary.
#[derive(Clone, Debug)]
pub struct GitService {
    executable: Option<PathBuf>,
    timeout: Duration,
}

impl GitService {
    #[must_use]
    pub fn new(timeout: Duration) -> Self {
        Self {
            executable: find_executable("git"),
            timeout,
        }
    }

    #[must_use]
    pub const fn with_executable(executable: PathBuf, timeout: Duration) -> Self {
        Self {
            executable: Some(executable),
            timeout,
        }
    }

    fn status(&self, root: &Path, cancelled: &AtomicBool) -> Result<VcsStatusSnapshot, VcsError> {
        let executable = self.executable.as_ref().ok_or_else(|| {
            VcsError::new(VcsErrorKind::Unavailable, "Git executable is unavailable")
        })?;
        let output = run_status(
            executable,
            root,
            self.timeout,
            DEFAULT_OUTPUT_LIMITS,
            cancelled,
        )?;
        parse_porcelain_v2(&output)
    }

    pub(crate) fn refresh_status_cancellable(
        &self,
        workspace: &VcsWorkspace,
        cancelled: &AtomicBool,
    ) -> Result<VcsStatusSnapshot, VcsError> {
        self.validate_workspace(workspace)?;
        self.status(workspace.root(), cancelled)
    }

    fn validate_workspace(&self, workspace: &VcsWorkspace) -> Result<(), VcsError> {
        if workspace.backend().id() == GIT_BACKEND_ID {
            Ok(())
        } else {
            Err(VcsError::new(
                VcsErrorKind::InvalidData,
                "Git adapter received a non-Git workspace",
            ))
        }
    }
}

impl Default for GitService {
    fn default() -> Self {
        Self::new(Duration::from_secs(5))
    }
}

impl VcsService for GitService {
    fn detect(&self, start: &Path) -> Result<Option<VcsWorkspace>, VcsError> {
        if self.executable.is_none() {
            return Ok(None);
        }
        let start =
            fs::canonicalize(start).map_err(|error| io_error("canonicalize", start, error))?;
        let start = if start.is_dir() {
            start
        } else {
            start.parent().map(Path::to_path_buf).ok_or_else(|| {
                VcsError::new(VcsErrorKind::InvalidData, "detection path has no parent")
            })?
        };

        let mut nearest_git = None;
        for ancestor in start.ancestors() {
            if valid_jj_marker(ancestor)? {
                return Ok(None);
            }
            if nearest_git.is_none() && valid_git_marker(ancestor)? {
                nearest_git = Some(ancestor.to_path_buf());
            }
        }

        nearest_git
            .map(|root| {
                VcsWorkspace::new(root, VcsBackendMetadata::new(GIT_BACKEND_ID, "Git", false)?)
            })
            .transpose()
    }

    fn refresh_status(&mut self, workspace: &VcsWorkspace) -> Result<VcsStatusSnapshot, VcsError> {
        static NOT_CANCELLED: AtomicBool = AtomicBool::new(false);
        self.refresh_status_cancellable(workspace, &NOT_CANCELLED)
    }
}

fn run_status(
    executable: &Path,
    root: &Path,
    timeout: Duration,
    limits: OutputLimits,
    cancelled: &AtomicBool,
) -> Result<Vec<u8>, VcsError> {
    let deadline = Instant::now().checked_add(timeout);
    let mut config = git_command(executable, root);
    config.args(["config", "--null", "--name-only", "--list"]);
    let config = run_command(config, deadline, FILTER_CONFIG_OUTPUT_LIMITS, cancelled)?;
    check_exit(config.status, &config.stderr)?;
    let filter_overrides = parse_filter_overrides(&config.stdout)?;

    let mut status = git_command(executable, root);
    for key in filter_overrides {
        status.args(["-c", &format!("{key}=")]);
    }
    status.args(["status", "--porcelain=v2", "-z", "--untracked-files=all"]);
    let status = run_command(status, deadline, limits, cancelled)?;
    check_exit(status.status, &status.stderr)?;
    Ok(status.stdout)
}

fn git_command(executable: &Path, root: &Path) -> Command {
    let mut command = Command::new(executable);
    command
        .arg("--no-optional-locks")
        .args(["-c", hooks_path_config()])
        .args(["-c", "core.fsmonitor=false"])
        .args(["-c", "pager.status=false"])
        .args(["-c", "diff.external="])
        .args(["-c", "diff.trustExitCode=false"])
        .args(["-C"])
        .arg(root)
        .arg("--work-tree")
        .arg(root)
        .env_clear()
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", null_device())
        .env("SSH_ASKPASS", null_device())
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat")
        .env("GIT_EXTERNAL_DIFF", "")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

struct ProcessOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn run_command(
    mut command: Command,
    deadline: Option<Instant>,
    limits: OutputLimits,
    cancelled: &AtomicBool,
) -> Result<ProcessOutput, VcsError> {
    let mut child = command.group_spawn().map_err(|error| {
        let kind = if error.kind() == io::ErrorKind::NotFound {
            VcsErrorKind::Unavailable
        } else {
            VcsErrorKind::Io
        };
        VcsError::new(kind, format!("cannot start Git: {error}"))
    })?;
    let stdout = child.inner().stdout.take().expect("piped stdout");
    let stderr = child.inner().stderr.take().expect("piped stderr");
    let stdout_exceeded = Arc::new(AtomicBool::new(false));
    let stderr_exceeded = Arc::new(AtomicBool::new(false));
    let stdout_limit = Arc::clone(&stdout_exceeded);
    let stderr_limit = Arc::clone(&stderr_exceeded);
    let stdout_reader = thread::spawn(move || read_limited(stdout, limits.stdout, stdout_limit));
    let stderr_reader = thread::spawn(move || read_limited(stderr, limits.stderr, stderr_limit));

    let mut exit_status = None;
    let status = loop {
        if exit_status.is_none() {
            match child.try_wait() {
                Ok(status) => exit_status = status,
                Err(error) => {
                    terminate_group(&mut child);
                    return Err(VcsError::new(
                        VcsErrorKind::Io,
                        format!("cannot wait for Git: {error}"),
                    ));
                }
            }
        }
        if let Some((stream, limit)) = exceeded_limit(
            &stdout_exceeded,
            limits.stdout,
            &stderr_exceeded,
            limits.stderr,
        ) {
            terminate_group(&mut child);
            return Err(output_limit_error(stream, limit));
        }
        if cancelled.load(Ordering::Relaxed) {
            terminate_group(&mut child);
            return Err(VcsError::new(
                VcsErrorKind::CommandFailed,
                "Git command was cancelled",
            ));
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            terminate_group(&mut child);
            return Err(VcsError::new(
                VcsErrorKind::CommandFailed,
                "Git command timed out",
            ));
        }
        if let Some(status) = exit_status
            && stdout_reader.is_finished()
            && stderr_reader.is_finished()
        {
            break status;
        }
        thread::sleep(POLL_INTERVAL);
    };

    let stdout = join_reader(stdout_reader, "stdout");
    let stderr = join_reader(stderr_reader, "stderr");
    if let Some((stream, limit)) = exceeded_limit(
        &stdout_exceeded,
        limits.stdout,
        &stderr_exceeded,
        limits.stderr,
    ) {
        terminate_remaining_group(&mut child);
        return Err(output_limit_error(stream, limit));
    }
    let stdout = stdout?;
    let stderr = stderr?;
    terminate_remaining_group(&mut child);
    Ok(ProcessOutput {
        status,
        stdout,
        stderr,
    })
}

fn parse_filter_overrides(output: &[u8]) -> Result<BTreeSet<String>, VcsError> {
    if !output.is_empty() && output.last() != Some(&0) {
        return Err(invalid_output("Git config output is not NUL-terminated"));
    }
    let mut filters = BTreeSet::new();
    for field in output
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
    {
        let key = std::str::from_utf8(field)
            .map_err(|_| invalid_output("Git config key is not valid UTF-8"))?;
        if key.starts_with("filter.") && (key.ends_with(".clean") || key.ends_with(".process")) {
            filters.insert(key.to_owned());
            if filters.len() > MAX_FILTER_OVERRIDES {
                return Err(invalid_output(
                    "Git config defines too many content filters",
                ));
            }
        }
    }
    Ok(filters)
}

fn exceeded_limit(
    stdout: &AtomicBool,
    stdout_limit: usize,
    stderr: &AtomicBool,
    stderr_limit: usize,
) -> Option<(&'static str, usize)> {
    if stdout.load(Ordering::Relaxed) {
        Some(("stdout", stdout_limit))
    } else if stderr.load(Ordering::Relaxed) {
        Some(("stderr", stderr_limit))
    } else {
        None
    }
}

fn output_limit_error(stream: &str, limit: usize) -> VcsError {
    VcsError::new(
        VcsErrorKind::InvalidData,
        format!("Git {stream} exceeded the {limit}-byte output limit"),
    )
}

fn terminate_group(child: &mut GroupChild) {
    let _ = child.kill();
    let _ = child.wait();
}

fn terminate_remaining_group(child: &mut GroupChild) {
    let _ = child.kill();
}

fn read_limited(
    mut reader: impl Read,
    limit: usize,
    exceeded: Arc<AtomicBool>,
) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(limit.min(8192));
    let mut buffer = [0; 8192];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok(bytes);
        }
        if read > limit.saturating_sub(bytes.len()) {
            exceeded.store(true, Ordering::Relaxed);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Git stream exceeded its output limit",
            ));
        }
        let required = bytes.len() + read;
        if required > bytes.capacity() {
            let target = bytes.capacity().saturating_mul(2).max(required).min(limit);
            bytes
                .try_reserve_exact(target - bytes.len())
                .map_err(io::Error::other)?;
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
}

fn join_reader(
    reader: thread::JoinHandle<io::Result<Vec<u8>>>,
    stream: &str,
) -> Result<Vec<u8>, VcsError> {
    reader
        .join()
        .map_err(|_| VcsError::new(VcsErrorKind::Io, format!("Git {stream} reader panicked")))?
        .map_err(|error| {
            VcsError::new(
                VcsErrorKind::Io,
                format!("cannot read Git {stream}: {error}"),
            )
        })
}

fn check_exit(status: ExitStatus, stderr: &[u8]) -> Result<(), VcsError> {
    if status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(stderr);
    let detail = detail.trim();
    let message = if detail.is_empty() {
        format!("Git status failed with {status}")
    } else {
        format!("Git status failed with {status}: {detail}")
    };
    Err(VcsError::new(VcsErrorKind::CommandFailed, message))
}

fn parse_porcelain_v2(output: &[u8]) -> Result<VcsStatusSnapshot, VcsError> {
    if !output.is_empty() && output.last() != Some(&0) {
        return Err(invalid_output("porcelain output is not NUL-terminated"));
    }
    if output.is_empty() {
        return Ok(VcsStatusSnapshot::new(Vec::new(), false));
    }

    let mut records = output[..output.len() - 1].split(|byte| *byte == 0);
    let mut entries = Vec::new();
    while let Some(record) = records.next() {
        let Some(tag) = record.first().copied() else {
            return Err(invalid_output("porcelain output contains an empty record"));
        };
        match tag {
            b'#' => {}
            b'1' => push_entry(&mut entries, parse_ordinary(record)?)?,
            b'2' => {
                let source = records
                    .next()
                    .ok_or_else(|| invalid_output("rename/copy record has no source path"))?;
                push_entry(&mut entries, parse_rename_or_copy(record, source)?)?;
            }
            b'u' => push_entry(&mut entries, parse_unmerged(record)?)?,
            b'?' => push_entry(&mut entries, parse_untracked(record)?)?,
            b'!' => {
                let path = record
                    .strip_prefix(b"! ")
                    .ok_or_else(|| invalid_output("malformed ignored record"))?;
                path_from_bytes(path)?;
            }
            _ => return Err(invalid_output("unknown porcelain v2 record type")),
        }
    }
    entries.sort_unstable_by(|left, right| left.path().cmp(right.path()));
    Ok(VcsStatusSnapshot::new(entries, false))
}

fn push_entry(entries: &mut Vec<VcsEntryStatus>, entry: VcsEntryStatus) -> Result<(), VcsError> {
    if entries.len() == MAX_STATUS_ENTRIES {
        return Err(invalid_output("Git status contains too many entries"));
    }
    entries.push(entry);
    Ok(())
}

fn parse_ordinary(record: &[u8]) -> Result<VcsEntryStatus, VcsError> {
    let mut fields = record.splitn(9, |byte| *byte == b' ');
    expect_marker(&mut fields, b"1", "ordinary")?;
    let xy = next_field(&mut fields, "ordinary")?;
    let (index_state, worktree_state) = parse_xy(xy)?;
    validate_ordinary_xy(xy)?;
    validate_submodule(next_field(&mut fields, "ordinary")?)?;
    validate_fields(&mut fields, 3, "ordinary", validate_mode)?;
    validate_fields(&mut fields, 2, "ordinary", validate_object_id)?;
    make_entry(
        path_from_bytes(next_field(&mut fields, "ordinary")?)?,
        None,
        combined_kind(index_state, worktree_state)?,
        index_state,
        worktree_state,
    )
}

fn parse_rename_or_copy(record: &[u8], source: &[u8]) -> Result<VcsEntryStatus, VcsError> {
    let mut fields = record.splitn(10, |byte| *byte == b' ');
    expect_marker(&mut fields, b"2", "rename/copy")?;
    let xy = next_field(&mut fields, "rename/copy")?;
    let (index_state, worktree_state) = parse_xy(xy)?;
    validate_rename_xy(xy)?;
    validate_submodule(next_field(&mut fields, "rename/copy")?)?;
    validate_fields(&mut fields, 3, "rename/copy", validate_mode)?;
    validate_fields(&mut fields, 2, "rename/copy", validate_object_id)?;
    let score = next_field(&mut fields, "rename/copy")?;
    let Some((&tag, similarity)) = score.split_first() else {
        return Err(invalid_output("invalid rename/copy score"));
    };
    if similarity.is_empty()
        || similarity.len() > 3
        || !similarity.iter().all(u8::is_ascii_digit)
        || similarity
            .iter()
            .fold(0_u16, |value, digit| value * 10 + u16::from(*digit - b'0'))
            > 100
    {
        return Err(invalid_output("invalid rename/copy score"));
    }
    let kind = match tag {
        b'R' => VcsStatusKind::Renamed,
        b'C' => VcsStatusKind::Copied,
        _ => return Err(invalid_output("invalid rename/copy score")),
    };
    if index_state != Some(kind) && worktree_state != Some(kind) {
        return Err(invalid_output(
            "rename/copy score does not match the status field",
        ));
    }
    make_entry(
        path_from_bytes(next_field(&mut fields, "rename/copy")?)?,
        Some(path_from_bytes(source)?),
        kind,
        index_state,
        worktree_state,
    )
}

fn parse_unmerged(record: &[u8]) -> Result<VcsEntryStatus, VcsError> {
    let mut fields = record.splitn(11, |byte| *byte == b' ');
    expect_marker(&mut fields, b"u", "unmerged")?;
    let xy = next_field(&mut fields, "unmerged")?;
    validate_unmerged_xy(xy)?;
    let (index_state, worktree_state) = parse_xy(xy)?;
    validate_submodule(next_field(&mut fields, "unmerged")?)?;
    validate_fields(&mut fields, 4, "unmerged", validate_mode)?;
    validate_fields(&mut fields, 3, "unmerged", validate_object_id)?;
    make_entry(
        path_from_bytes(next_field(&mut fields, "unmerged")?)?,
        None,
        VcsStatusKind::Conflicted,
        index_state,
        worktree_state,
    )
}

fn parse_untracked(record: &[u8]) -> Result<VcsEntryStatus, VcsError> {
    let path = record
        .strip_prefix(b"? ")
        .ok_or_else(|| invalid_output("malformed untracked record"))?;
    make_entry(
        path_from_bytes(path)?,
        None,
        VcsStatusKind::Untracked,
        None,
        Some(VcsStatusKind::Untracked),
    )
}

fn next_field<'a>(
    fields: &mut impl Iterator<Item = &'a [u8]>,
    record_name: &str,
) -> Result<&'a [u8], VcsError> {
    fields
        .next()
        .filter(|field| !field.is_empty())
        .ok_or_else(|| invalid_output(format!("malformed {record_name} record")))
}

fn expect_marker<'a>(
    fields: &mut impl Iterator<Item = &'a [u8]>,
    expected: &[u8],
    record_name: &str,
) -> Result<(), VcsError> {
    if next_field(fields, record_name)? == expected {
        Ok(())
    } else {
        Err(invalid_output(format!(
            "invalid {record_name} record marker"
        )))
    }
}

fn validate_fields<'a>(
    fields: &mut impl Iterator<Item = &'a [u8]>,
    count: usize,
    record_name: &str,
    validate: fn(&[u8]) -> Result<(), VcsError>,
) -> Result<(), VcsError> {
    for _ in 0..count {
        validate(next_field(fields, record_name)?)?;
    }
    Ok(())
}

fn validate_submodule(field: &[u8]) -> Result<(), VcsError> {
    let valid = field == b"N..." || matches!(field, [b'S', b'.' | b'C', b'.' | b'M', b'.' | b'U']);
    if valid {
        Ok(())
    } else {
        Err(invalid_output("invalid submodule state"))
    }
}

fn validate_mode(field: &[u8]) -> Result<(), VcsError> {
    if field.len() == 6 && field.iter().all(|byte| matches!(byte, b'0'..=b'7')) {
        Ok(())
    } else {
        Err(invalid_output("invalid file mode"))
    }
}

fn validate_object_id(field: &[u8]) -> Result<(), VcsError> {
    if matches!(field.len(), 40 | 64) && field.iter().all(u8::is_ascii_hexdigit) {
        Ok(())
    } else {
        Err(invalid_output("invalid object id"))
    }
}

fn validate_ordinary_xy(field: &[u8]) -> Result<(), VcsError> {
    if field
        .iter()
        .all(|state| matches!(state, b'.' | b'M' | b'T' | b'A' | b'D'))
        && field != b".."
    {
        Ok(())
    } else {
        Err(invalid_output("invalid ordinary status field"))
    }
}

fn validate_rename_xy(field: &[u8]) -> Result<(), VcsError> {
    if field
        .iter()
        .all(|state| matches!(state, b'.' | b'M' | b'T' | b'A' | b'D' | b'R' | b'C'))
        && field != b".."
    {
        Ok(())
    } else {
        Err(invalid_output("invalid rename/copy status field"))
    }
}

fn validate_unmerged_xy(field: &[u8]) -> Result<(), VcsError> {
    if matches!(field, b"DD" | b"AU" | b"UD" | b"UA" | b"DU" | b"AA" | b"UU") {
        Ok(())
    } else {
        Err(invalid_output("invalid unmerged status field"))
    }
}

fn parse_xy(field: &[u8]) -> Result<(Option<VcsStatusKind>, Option<VcsStatusKind>), VcsError> {
    if field.len() != 2 {
        return Err(invalid_output("status field must contain two bytes"));
    }
    Ok((map_state(field[0])?, map_state(field[1])?))
}

fn map_state(state: u8) -> Result<Option<VcsStatusKind>, VcsError> {
    match state {
        b'.' => Ok(None),
        b'M' => Ok(Some(VcsStatusKind::Modified)),
        b'A' => Ok(Some(VcsStatusKind::Added)),
        b'D' => Ok(Some(VcsStatusKind::Deleted)),
        b'R' => Ok(Some(VcsStatusKind::Renamed)),
        b'C' => Ok(Some(VcsStatusKind::Copied)),
        b'T' => Ok(Some(VcsStatusKind::TypeChanged)),
        b'U' => Ok(Some(VcsStatusKind::Conflicted)),
        _ => Err(invalid_output("unknown status code")),
    }
}

fn combined_kind(
    index: Option<VcsStatusKind>,
    worktree: Option<VcsStatusKind>,
) -> Result<VcsStatusKind, VcsError> {
    const PRECEDENCE: [VcsStatusKind; 8] = [
        VcsStatusKind::Conflicted,
        VcsStatusKind::Renamed,
        VcsStatusKind::Copied,
        VcsStatusKind::TypeChanged,
        VcsStatusKind::Deleted,
        VcsStatusKind::Added,
        VcsStatusKind::Modified,
        VcsStatusKind::Untracked,
    ];
    PRECEDENCE
        .into_iter()
        .find(|kind| index == Some(*kind) || worktree == Some(*kind))
        .ok_or_else(|| invalid_output("ordinary record has no change"))
}

fn make_entry(
    path: PathBuf,
    source_path: Option<PathBuf>,
    kind: VcsStatusKind,
    index_state: Option<VcsStatusKind>,
    worktree_state: Option<VcsStatusKind>,
) -> Result<VcsEntryStatus, VcsError> {
    VcsEntryStatus::new(path, source_path, kind, index_state, worktree_state)
        .map_err(|error| invalid_output(error.to_string()))
}

#[cfg(unix)]
fn path_from_bytes(path: &[u8]) -> Result<PathBuf, VcsError> {
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(OsString::from_vec(path.to_vec())))
}

#[cfg(not(unix))]
fn path_from_bytes(path: &[u8]) -> Result<PathBuf, VcsError> {
    String::from_utf8(path.to_vec())
        .map(PathBuf::from)
        .map_err(|_| invalid_output("Git path is not valid UTF-8 on this platform"))
}

fn invalid_output(message: impl Into<String>) -> VcsError {
    VcsError::new(VcsErrorKind::InvalidData, message)
}

fn valid_jj_marker(root: &Path) -> Result<bool, VcsError> {
    let marker = root.join(".jj");
    Ok(metadata(&marker)?.is_some_and(|value| value.is_dir())
        && marker.join("repo").is_dir()
        && marker.join("working_copy").is_dir())
}

fn valid_git_marker(root: &Path) -> Result<bool, VcsError> {
    let marker = root.join(".git");
    let Some(marker_metadata) = metadata(&marker)? else {
        return Ok(false);
    };
    if marker_metadata.is_dir() {
        return Ok(valid_git_admin_dir(&marker));
    }
    if !marker_metadata.is_file() {
        return Ok(false);
    }
    let contents = fs::read(&marker).map_err(|error| io_error("read", &marker, error))?;
    let contents = trim_ascii(&contents);
    let Some(target) = contents.strip_prefix(b"gitdir:") else {
        return Ok(false);
    };
    let target = gitdir_path(trim_ascii(target))?;
    if target.as_os_str().is_empty() {
        return Ok(false);
    }
    let target = if target.is_absolute() {
        target
    } else {
        root.join(target)
    };
    match fs::canonicalize(target) {
        Ok(target) => Ok(valid_git_admin_dir(&target)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error("canonicalize", &marker, error)),
    }
}

fn trim_ascii(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(u8::is_ascii_whitespace) {
        value = &value[1..];
    }
    while value.last().is_some_and(u8::is_ascii_whitespace) {
        value = &value[..value.len() - 1];
    }
    value
}

#[cfg(unix)]
fn gitdir_path(value: &[u8]) -> Result<PathBuf, VcsError> {
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(OsString::from_vec(value.to_vec())))
}

#[cfg(not(unix))]
fn gitdir_path(value: &[u8]) -> Result<PathBuf, VcsError> {
    String::from_utf8(value.to_vec())
        .map(PathBuf::from)
        .map_err(|_| invalid_output("Git administrative path is not valid UTF-8"))
}

fn valid_git_admin_dir(path: &Path) -> bool {
    path.join("HEAD").is_file()
        && (path.join("objects").is_dir() || path.join("commondir").is_file())
}

fn metadata(path: &Path) -> Result<Option<fs::Metadata>, VcsError> {
    match fs::metadata(path) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_error("inspect", path, error)),
    }
}

fn io_error(operation: &str, path: &Path, error: io::Error) -> VcsError {
    let kind = if error.kind() == io::ErrorKind::PermissionDenied {
        VcsErrorKind::PermissionDenied
    } else {
        VcsErrorKind::Io
    };
    VcsError::new(
        kind,
        format!("cannot {operation} {}: {error}", path.display()),
    )
}

fn find_executable(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    find_executable_in(name, env::split_paths(&path))
}

fn find_executable_in(
    name: &str,
    directories: impl IntoIterator<Item = PathBuf>,
) -> Option<PathBuf> {
    directories
        .into_iter()
        .map(|directory| directory.join(executable_name(name)))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(windows)]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(windows)]
fn executable_name(name: &str) -> OsString {
    OsString::from(format!("{name}.exe"))
}

#[cfg(not(windows))]
fn executable_name(name: &str) -> OsString {
    OsString::from(name)
}

#[cfg(windows)]
const fn hooks_path_config() -> &'static str {
    "core.hooksPath=NUL"
}

#[cfg(not(windows))]
const fn hooks_path_config() -> &'static str {
    "core.hooksPath=/dev/null"
}

#[cfg(windows)]
const fn null_device() -> &'static str {
    "NUL"
}

#[cfg(not(windows))]
const fn null_device() -> &'static str {
    "/dev/null"
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[cfg(unix)]
    use tempfile::TempDir;

    use super::{
        MAX_STATUS_ENTRIES, OutputLimits, find_executable_in, parse_porcelain_v2, run_status,
    };
    use crate::vcs::{VcsErrorKind, VcsStatusKind};

    const HASH: &str = "0123456789012345678901234567890123456789";

    #[test]
    fn parses_every_porcelain_record_without_quoting_paths() {
        let ordinary = format!("1 .M N... 100644 100644 100644 {HASH} {HASH} src/sp ace.rs\0");
        let renamed = format!(
            "2 R. N... 100644 100644 100644 {HASH} {HASH} R100 src/renommé.rs\0src/old name.rs\0"
        );
        let copied = format!(
            "2 C. N... 100644 100644 100644 {HASH} {HASH} C75 src/copy.rs\0src/source.rs\0"
        );
        let unmerged =
            format!("u UU N... 100644 100644 100644 100644 {HASH} {HASH} {HASH} conflicted file\0");
        let output = [
            ordinary.as_bytes(),
            renamed.as_bytes(),
            copied.as_bytes(),
            unmerged.as_bytes(),
            b"? untracked/naive file\0",
        ]
        .concat();

        let snapshot = parse_porcelain_v2(&output).expect("valid porcelain");
        let entries = snapshot.entries();
        assert_eq!(entries.len(), 5);
        assert_eq!(entries[0].path(), Path::new("conflicted file"));
        assert_eq!(entries[0].kind(), VcsStatusKind::Conflicted);
        assert_eq!(entries[1].path(), Path::new("src/copy.rs"));
        assert_eq!(entries[1].kind(), VcsStatusKind::Copied);
        assert_eq!(entries[2].path(), Path::new("src/renommé.rs"));
        assert_eq!(entries[2].kind(), VcsStatusKind::Renamed);
        assert_eq!(entries[2].source_path(), Some(Path::new("src/old name.rs")));
        assert_eq!(entries[3].path(), Path::new("src/sp ace.rs"));
        assert_eq!(entries[3].kind(), VcsStatusKind::Modified);
        assert_eq!(entries[4].kind(), VcsStatusKind::Untracked);
    }

    #[test]
    fn maps_type_changes_in_index_and_worktree() {
        let output = format!("1 TT N... 100644 120000 160000 {HASH} {HASH} typed\0");
        let snapshot = parse_porcelain_v2(output.as_bytes()).expect("valid porcelain");
        let entry = &snapshot.entries()[0];
        assert_eq!(entry.kind(), VcsStatusKind::TypeChanged);
        assert_eq!(entry.index_state(), Some(VcsStatusKind::TypeChanged));
        assert_eq!(entry.worktree_state(), Some(VcsStatusKind::TypeChanged));
    }

    #[test]
    fn rejects_truncated_and_unknown_records() {
        assert!(parse_porcelain_v2(b"? missing terminator").is_err());
        assert!(parse_porcelain_v2(b"2 R. truncated\0").is_err());
        assert!(parse_porcelain_v2(b"x unsupported\0").is_err());
    }

    #[test]
    fn rejects_malformed_rename_and_copy_scores() {
        for score in ["R", "Rgarbage", "C999", "R101", "C-1"] {
            let output =
                format!("2 R. N... 100644 100644 100644 {HASH} {HASH} {score} target\0source\0");
            assert!(
                parse_porcelain_v2(output.as_bytes()).is_err(),
                "accepted malformed score {score}"
            );
        }
    }

    #[test]
    fn rejects_noncanonical_record_markers_and_mismatched_scores() {
        let malformed = [
            format!("1garbage .M N... 100644 100644 100644 {HASH} {HASH} target\0"),
            format!("2garbage R. N... 100644 100644 100644 {HASH} {HASH} R100 target\0source\0"),
            format!("ugarbage UU N... 100644 100644 100644 100644 {HASH} {HASH} {HASH} target\0"),
            format!("2 M. N... 100644 100644 100644 {HASH} {HASH} R100 target\0source\0"),
        ];
        for output in malformed {
            assert!(
                parse_porcelain_v2(output.as_bytes()).is_err(),
                "accepted malformed output {output:?}"
            );
        }
    }

    #[test]
    fn rejects_malformed_porcelain_metadata_and_ignored_records() {
        let malformed = [
            format!("1 .M broken 100644 100644 100644 {HASH} {HASH} target\0"),
            format!("1 .M N... 10x644 100644 100644 {HASH} {HASH} target\0"),
            format!("1 .M N... 100644 100644 100644 bad {HASH} target\0"),
            format!("u .. N... 100644 100644 100644 100644 {HASH} {HASH} {HASH} target\0"),
            "!garbage\0".to_owned(),
        ];
        for output in malformed {
            assert!(
                parse_porcelain_v2(output.as_bytes()).is_err(),
                "accepted malformed output {output:?}"
            );
        }
    }

    #[test]
    fn bounds_the_number_of_parsed_status_entries() {
        let mut output = Vec::with_capacity((MAX_STATUS_ENTRIES + 1) * 4);
        for _ in 0..=MAX_STATUS_ENTRIES {
            output.extend_from_slice(b"? a\0");
        }

        assert!(parse_porcelain_v2(&output).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn bounds_stdout_and_stderr_from_the_git_process() {
        for (name, body) in [
            (
                "large-stdout",
                "#!/bin/sh\nfor value in 1 2 3 4 5 6 7 8 9 10; do printf '0123456789'; done\n",
            ),
            (
                "large-stderr",
                "#!/bin/sh\nfor value in 1 2 3 4 5 6 7 8 9 10; do printf '0123456789' >&2; done\n",
            ),
        ] {
            let temp = TempDir::new().expect("tempdir");
            let script = temp.path().join(name);
            fs::write(&script, body).expect("script");
            let mut permissions = fs::metadata(&script).expect("metadata").permissions();
            permissions.set_mode(0o700);
            fs::set_permissions(&script, permissions).expect("permissions");

            let error = run_status(
                &script,
                temp.path(),
                Duration::from_secs(5),
                OutputLimits {
                    stdout: 64,
                    stderr: 64,
                },
                &AtomicBool::new(false),
            )
            .expect_err("oversized output");
            assert_eq!(error.kind(), VcsErrorKind::InvalidData);
        }
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_terminates_an_active_git_process() {
        let temp = TempDir::new().expect("tempdir");
        let script = temp.path().join("slow-git");
        fs::write(&script, "#!/bin/sh\nsleep 5\n").expect("script");
        let mut permissions = fs::metadata(&script).expect("metadata").permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&script, permissions).expect("permissions");
        let cancelled = Arc::new(AtomicBool::new(false));
        let trigger = Arc::clone(&cancelled);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            trigger.store(true, Ordering::Relaxed);
        });

        let started = Instant::now();
        let error = run_status(
            &script,
            temp.path(),
            Duration::from_secs(5),
            OutputLimits {
                stdout: 64,
                stderr: 64,
            },
            &cancelled,
        )
        .expect_err("cancelled command");

        assert_eq!(error.kind(), VcsErrorKind::CommandFailed);
        assert!(started.elapsed() < Duration::from_secs(1));
    }
    #[cfg(unix)]
    #[test]
    fn executable_lookup_skips_non_executable_files() {
        let temp = TempDir::new().expect("tempdir");
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        fs::create_dir(&first).expect("first path entry");
        fs::create_dir(&second).expect("second path entry");
        fs::write(first.join("git"), []).expect("non executable Git");
        fs::write(second.join("git"), []).expect("executable Git");
        let mut permissions = fs::metadata(second.join("git"))
            .expect("metadata")
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(second.join("git"), permissions).expect("permissions");

        assert_eq!(
            find_executable_in("git", [first, second]),
            Some(temp.path().join("second/git"))
        );
    }
}
