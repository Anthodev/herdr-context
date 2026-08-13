use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use crate::worker::process::{ProcessError, ProcessErrorKind, ProcessOutput, ProcessSpec, run};

use super::{
    VcsBackendMetadata, VcsEntryStatus, VcsError, VcsErrorKind, VcsService, VcsStatusKind,
    VcsStatusSnapshot, VcsWorkspace, find_executable,
};

const JJ_BACKEND_ID: &str = "jj";
const DEFAULT_STDOUT_LIMIT: usize = 64 * 1024 * 1024;
const DEFAULT_STDERR_LIMIT: usize = 1024 * 1024;
const ROOT_STDOUT_LIMIT: usize = 64 * 1024;
const MAX_STATUS_ENTRIES: usize = 100_000;

// TreeDiffEntry.status_char() was added in Jujutsu 0.37. The command itself is
// the feature probe: unsupported templates fail without replacing the Files tree.
const STATUS_TEMPLATE: &str = concat!(
    "self.status_char() ++ \"\\0\" ++ ",
    "self.source().path() ++ \"\\0\" ++ ",
    "self.target().path() ++ \"\\0\" ++ ",
    "self.source().conflict() ++ \"\\0\" ++ ",
    "self.target().conflict() ++ \"\\0\" ++ ",
    "self.source().file_type() ++ \"\\0\" ++ ",
    "self.target().file_type() ++ \"\\0\"",
);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum JujutsuMode {
    #[default]
    Fresh,
    Passive,
}

/// Jujutsu adapter using one bounded, templated `jj diff` per status refresh.
#[derive(Clone, Debug)]
pub struct JjService {
    executable: Option<PathBuf>,
    mode: JujutsuMode,
    timeout: Duration,
}

impl JjService {
    #[must_use]
    pub fn new(mode: JujutsuMode, timeout: Duration) -> Self {
        Self {
            executable: find_executable("jj"),
            mode,
            timeout,
        }
    }

    #[must_use]
    pub const fn with_executable(
        executable: PathBuf,
        mode: JujutsuMode,
        timeout: Duration,
    ) -> Self {
        Self {
            executable: Some(executable),
            mode,
            timeout,
        }
    }

    #[must_use]
    pub const fn mode(&self) -> JujutsuMode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: JujutsuMode) -> bool {
        if self.mode == mode {
            return false;
        }
        self.mode = mode;
        true
    }

    pub(crate) fn detect_cancellable(
        &self,
        start: &Path,
        cancelled: &AtomicBool,
    ) -> Result<Option<VcsWorkspace>, VcsError> {
        let Some(executable) = &self.executable else {
            return Ok(None);
        };
        let start =
            fs::canonicalize(start).map_err(|error| io_error("canonicalize", start, error))?;
        let start = if start.is_dir() {
            start
        } else {
            start.parent().map(Path::to_path_buf).ok_or_else(|| {
                VcsError::new(VcsErrorKind::InvalidData, "detection path has no parent")
            })?
        };
        let spec = base_spec(executable, self.timeout)
            .arg("--ignore-working-copy")
            .arg("root")
            .current_dir(&start)
            .output_limits(ROOT_STDOUT_LIMIT, DEFAULT_STDERR_LIMIT);
        let output = run(&spec, cancelled).map_err(process_error)?;
        if !output.status().success() {
            return Ok(None);
        }
        let root = parse_workspace_root(output.stdout())?;
        let root =
            fs::canonicalize(&root).map_err(|error| io_error("canonicalize", &root, error))?;
        if !start.starts_with(&root) {
            return Err(VcsError::new(
                VcsErrorKind::InvalidData,
                "Jujutsu reported a workspace root outside the detection path",
            ));
        }
        Ok(Some(VcsWorkspace::new(
            root,
            VcsBackendMetadata::new(JJ_BACKEND_ID, "Jujutsu", true)?,
        )?))
    }

    pub(crate) fn refresh_status_cancellable(
        &self,
        workspace: &VcsWorkspace,
        cancelled: &AtomicBool,
    ) -> Result<VcsStatusSnapshot, VcsError> {
        self.validate_workspace(workspace)?;
        let executable = self.executable.as_ref().ok_or_else(|| {
            VcsError::new(
                VcsErrorKind::Unavailable,
                "Jujutsu executable is unavailable",
            )
        })?;
        let mut spec = base_spec(executable, self.timeout)
            .args(["-R"])
            .arg(workspace.root())
            .current_dir(workspace.root());
        if self.mode == JujutsuMode::Passive {
            spec = spec.arg("--ignore-working-copy");
        }
        spec = spec.args(["diff", "-r", "@", "--template", STATUS_TEMPLATE]);
        let output = run(&spec, cancelled).map_err(process_error)?;
        check_status_exit(&output)?;
        parse_templated_diff(output.stdout(), self.mode == JujutsuMode::Passive)
    }

    fn validate_workspace(&self, workspace: &VcsWorkspace) -> Result<(), VcsError> {
        if workspace.backend().id() == JJ_BACKEND_ID {
            Ok(())
        } else {
            Err(VcsError::new(
                VcsErrorKind::InvalidData,
                "Jujutsu adapter received a non-Jujutsu workspace",
            ))
        }
    }
}

impl Default for JjService {
    fn default() -> Self {
        Self::new(JujutsuMode::Fresh, Duration::from_secs(5))
    }
}

impl VcsService for JjService {
    fn detect(&self, start: &Path) -> Result<Option<VcsWorkspace>, VcsError> {
        static NOT_CANCELLED: AtomicBool = AtomicBool::new(false);
        self.detect_cancellable(start, &NOT_CANCELLED)
    }

    fn refresh_status(&mut self, workspace: &VcsWorkspace) -> Result<VcsStatusSnapshot, VcsError> {
        static NOT_CANCELLED: AtomicBool = AtomicBool::new(false);
        self.refresh_status_cancellable(workspace, &NOT_CANCELLED)
    }
}

fn base_spec(executable: &Path, timeout: Duration) -> ProcessSpec {
    ProcessSpec::new(executable)
        .args(["--color=never", "--no-pager", "--quiet"])
        .timeout(timeout)
        .output_limits(DEFAULT_STDOUT_LIMIT, DEFAULT_STDERR_LIMIT)
}

fn check_status_exit(output: &ProcessOutput) -> Result<(), VcsError> {
    if output.status().success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(trim_ascii(output.stderr()));
    let suffix = if detail.is_empty() {
        String::new()
    } else {
        format!(": {detail}")
    };
    Err(VcsError::new(
        VcsErrorKind::CommandFailed,
        format!(
            "Jujutsu status failed; jj 0.37 or newer with TreeDiffEntry templates is required{suffix}"
        ),
    ))
}

fn process_error(error: ProcessError) -> VcsError {
    let kind = match error.kind() {
        ProcessErrorKind::Spawn => VcsErrorKind::Unavailable,
        ProcessErrorKind::Io => VcsErrorKind::Io,
        ProcessErrorKind::TimedOut
        | ProcessErrorKind::Cancelled
        | ProcessErrorKind::OutputLimit
        | ProcessErrorKind::InvalidTimeout => VcsErrorKind::CommandFailed,
    };
    VcsError::new(kind, format!("Jujutsu command failed: {error}"))
}

fn parse_workspace_root(output: &[u8]) -> Result<PathBuf, VcsError> {
    let Some(output) = output.strip_suffix(b"\n") else {
        return Err(invalid_output(
            "Jujutsu root output is not newline terminated",
        ));
    };
    if output.is_empty() || output.contains(&b'\0') {
        return Err(invalid_output("Jujutsu root output is malformed"));
    }
    let root = path_from_bytes(output)?;
    if !root.is_absolute() {
        return Err(invalid_output("Jujutsu workspace root is not absolute"));
    }
    Ok(root)
}

fn parse_templated_diff(output: &[u8], stale: bool) -> Result<VcsStatusSnapshot, VcsError> {
    if output.is_empty() {
        return Ok(VcsStatusSnapshot::new(Vec::new(), stale));
    }
    let Some(output) = output.strip_suffix(b"\0") else {
        return Err(invalid_output(
            "Jujutsu status record is not NUL terminated",
        ));
    };
    let mut fields = output.split(|byte| *byte == b'\0');
    let mut entries = Vec::new();
    while let Some(status) = fields.next() {
        if entries.len() == MAX_STATUS_ENTRIES {
            return Err(invalid_output("Jujutsu status entry limit exceeded"));
        }
        let source_path = next_field(&mut fields)?;
        let target_path = next_field(&mut fields)?;
        let source_conflict = parse_bool(next_field(&mut fields)?)?;
        let target_conflict = parse_bool(next_field(&mut fields)?)?;
        let source_type = parse_file_type(next_field(&mut fields)?)?;
        let target_type = parse_file_type(next_field(&mut fields)?)?;
        let base_kind = parse_status(status)?;
        let conflict = source_conflict
            || target_conflict
            || source_type == FileType::Conflict
            || target_type == FileType::Conflict;
        let kind = if conflict {
            VcsStatusKind::Conflicted
        } else if base_kind == VcsStatusKind::Modified
            && source_type != FileType::Absent
            && target_type != FileType::Absent
            && source_type != target_type
        {
            VcsStatusKind::TypeChanged
        } else {
            base_kind
        };
        let source_path = if matches!(base_kind, VcsStatusKind::Renamed | VcsStatusKind::Copied) {
            Some(path_from_bytes(source_path)?)
        } else {
            None
        };
        let entry = VcsEntryStatus::new(
            path_from_bytes(target_path)?,
            source_path,
            kind,
            source_conflict.then_some(VcsStatusKind::Conflicted),
            target_conflict.then_some(VcsStatusKind::Conflicted),
        )
        .map_err(|error| invalid_output(format!("invalid Jujutsu status path: {error}")))?;
        entries.push(entry);
    }
    Ok(VcsStatusSnapshot::new(entries, stale))
}

fn next_field<'a>(fields: &mut impl Iterator<Item = &'a [u8]>) -> Result<&'a [u8], VcsError> {
    fields
        .next()
        .ok_or_else(|| invalid_output("truncated Jujutsu status record"))
}

fn parse_status(value: &[u8]) -> Result<VcsStatusKind, VcsError> {
    match value {
        b"M" => Ok(VcsStatusKind::Modified),
        b"A" => Ok(VcsStatusKind::Added),
        b"D" => Ok(VcsStatusKind::Deleted),
        b"C" => Ok(VcsStatusKind::Copied),
        b"R" => Ok(VcsStatusKind::Renamed),
        _ => Err(invalid_output("unknown Jujutsu status code")),
    }
}

fn parse_bool(value: &[u8]) -> Result<bool, VcsError> {
    match value {
        b"true" => Ok(true),
        b"false" => Ok(false),
        _ => Err(invalid_output("invalid Jujutsu conflict field")),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FileType {
    Absent,
    File,
    Symlink,
    Tree,
    GitSubmodule,
    Conflict,
}

fn parse_file_type(value: &[u8]) -> Result<FileType, VcsError> {
    match value {
        b"" => Ok(FileType::Absent),
        b"file" => Ok(FileType::File),
        b"symlink" => Ok(FileType::Symlink),
        b"tree" => Ok(FileType::Tree),
        b"git-submodule" => Ok(FileType::GitSubmodule),
        b"conflict" => Ok(FileType::Conflict),
        _ => Err(invalid_output("invalid Jujutsu file type")),
    }
}

#[cfg(unix)]
fn path_from_bytes(path: &[u8]) -> Result<PathBuf, VcsError> {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(OsString::from_vec(path.to_vec())))
}

#[cfg(not(unix))]
fn path_from_bytes(path: &[u8]) -> Result<PathBuf, VcsError> {
    String::from_utf8(path.to_vec())
        .map(PathBuf::from)
        .map_err(|_| invalid_output("Jujutsu path is not valid UTF-8"))
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

fn invalid_output(message: impl Into<String>) -> VcsError {
    VcsError::new(VcsErrorKind::InvalidData, message)
}

fn io_error(operation: &str, path: &Path, error: std::io::Error) -> VcsError {
    let kind = if error.kind() == std::io::ErrorKind::PermissionDenied {
        VcsErrorKind::PermissionDenied
    } else {
        VcsErrorKind::Io
    };
    VcsError::new(
        kind,
        format!("cannot {operation} {}: {error}", path.display()),
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{parse_templated_diff, parse_workspace_root};
    use crate::vcs::{VcsErrorKind, VcsStatusKind};

    #[test]
    fn parser_accepts_non_utf8_paths_on_unix() {
        #[cfg(unix)]
        {
            let output = b"A\0bad-\xff\0bad-\xff\0false\0false\0\0file\0";
            let snapshot = parse_templated_diff(output, false).expect("status");
            assert_eq!(snapshot.entries()[0].kind(), VcsStatusKind::Added);
        }
    }

    #[test]
    fn parser_rejects_truncated_and_excess_records() {
        for output in [
            b"M\0path\0".as_slice(),
            b"M\0path\0path\0false\0false\0file\0file\0extra\0".as_slice(),
        ] {
            assert_eq!(
                parse_templated_diff(output, false)
                    .expect_err("invalid output")
                    .kind(),
                VcsErrorKind::InvalidData
            );
        }
    }

    #[test]
    fn workspace_root_parser_preserves_spaces_and_line_break_bytes() {
        assert_eq!(
            parse_workspace_root(b"/tmp/root with spaces\n").expect("root"),
            Path::new("/tmp/root with spaces")
        );
        assert_eq!(
            parse_workspace_root(b"/tmp/root\ninside\n").expect("embedded newline"),
            Path::new("/tmp/root\ninside")
        );
        assert_eq!(
            parse_workspace_root(b"/tmp/root\r\n").expect("trailing carriage return"),
            Path::new("/tmp/root\r")
        );
        for output in [
            b"/tmp/unterminated".as_slice(),
            b"/tmp/bad\0path\n".as_slice(),
        ] {
            assert_eq!(
                parse_workspace_root(output)
                    .expect_err("malformed root")
                    .kind(),
                VcsErrorKind::InvalidData
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn detection_honors_cancellation_while_waiting_for_jujutsu() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::thread;
        use std::time::{Duration, Instant};

        let temp = tempfile::TempDir::new().expect("tempdir");
        let script = temp.path().join("slow-jj");
        fs::write(&script, "#!/bin/sh\nwhile :; do :; done\n").expect("script");
        let mut permissions = fs::metadata(&script).expect("metadata").permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&script, permissions).expect("permissions");
        let service = super::JjService::with_executable(
            script,
            super::JujutsuMode::Fresh,
            Duration::from_secs(5),
        );
        let cancelled = Arc::new(AtomicBool::new(false));
        let trigger = Arc::clone(&cancelled);
        let cancellation = thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            trigger.store(true, Ordering::Relaxed);
        });

        let started = Instant::now();
        let error = service
            .detect_cancellable(temp.path(), &cancelled)
            .expect_err("cancelled detection");
        cancellation.join().expect("cancellation trigger");

        assert_eq!(error.kind(), VcsErrorKind::CommandFailed);
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
