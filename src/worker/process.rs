//! Argv-only subprocess execution with cancellation, timeout, and bounded output.

use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use command_group::{CommandGroup, GroupChild};

const POLL_INTERVAL: Duration = Duration::from_millis(10);
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_OUTPUT_LIMIT: usize = 1024 * 1024;

#[derive(Clone, Debug)]
pub struct ProcessSpec {
    program: PathBuf,
    args: Vec<OsString>,
    current_dir: Option<PathBuf>,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
}

impl ProcessSpec {
    #[must_use]
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            current_dir: None,
            timeout: DEFAULT_TIMEOUT,
            stdout_limit: DEFAULT_OUTPUT_LIMIT,
            stderr_limit: DEFAULT_OUTPUT_LIMIT,
        }
    }

    #[must_use]
    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_os_string());
        self
    }

    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|arg| arg.as_ref().to_os_string()));
        self
    }

    #[must_use]
    pub fn current_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(path.into());
        self
    }

    #[must_use]
    pub const fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    #[must_use]
    pub const fn output_limits(mut self, stdout: usize, stderr: usize) -> Self {
        self.stdout_limit = stdout;
        self.stderr_limit = stderr;
        self
    }
}

#[derive(Debug)]
pub struct ProcessOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl ProcessOutput {
    #[must_use]
    pub const fn status(&self) -> ExitStatus {
        self.status
    }

    #[must_use]
    pub fn stdout(&self) -> &[u8] {
        &self.stdout
    }

    #[must_use]
    pub fn stderr(&self) -> &[u8] {
        &self.stderr
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessErrorKind {
    Spawn,
    Io,
    TimedOut,
    Cancelled,
    OutputLimit,
    InvalidTimeout,
}

#[derive(Debug)]
pub struct ProcessError {
    kind: ProcessErrorKind,
    message: String,
    source: Option<io::Error>,
}

impl ProcessError {
    fn new(kind: ProcessErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
        }
    }

    fn with_source(kind: ProcessErrorKind, message: impl Into<String>, source: io::Error) -> Self {
        Self {
            kind,
            message: message.into(),
            source: Some(source),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> ProcessErrorKind {
        self.kind
    }
}

impl fmt::Display for ProcessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ProcessError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source.as_ref().map(|source| source as _)
    }
}

pub fn run(spec: &ProcessSpec, cancelled: &AtomicBool) -> Result<ProcessOutput, ProcessError> {
    if cancelled.load(Ordering::Relaxed) {
        return Err(ProcessError::new(
            ProcessErrorKind::Cancelled,
            "subprocess was cancelled",
        ));
    }
    let deadline = Instant::now().checked_add(spec.timeout).ok_or_else(|| {
        ProcessError::new(
            ProcessErrorKind::InvalidTimeout,
            "subprocess timeout is too large",
        )
    })?;
    let mut command = std::process::Command::new(&spec.program);
    command
        .args(&spec.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(current_dir) = &spec.current_dir {
        command.current_dir(current_dir);
    }

    let mut child = command.group_spawn().map_err(|source| {
        ProcessError::with_source(
            ProcessErrorKind::Spawn,
            format!("cannot start {}", spec.program.display()),
            source,
        )
    })?;
    let stdout = child
        .inner()
        .stdout
        .take()
        .expect("piped subprocess stdout");
    let stderr = child
        .inner()
        .stderr
        .take()
        .expect("piped subprocess stderr");
    let stdout_exceeded = Arc::new(AtomicBool::new(false));
    let stderr_exceeded = Arc::new(AtomicBool::new(false));
    let stdout_reader = spawn_reader(stdout, spec.stdout_limit, Arc::clone(&stdout_exceeded));
    let stderr_reader = spawn_reader(stderr, spec.stderr_limit, Arc::clone(&stderr_exceeded));

    let mut exit_status = None;
    let status = loop {
        if exit_status.is_none() {
            match child.try_wait() {
                Ok(status) => exit_status = status,
                Err(source) => {
                    return fail_and_collect(
                        &mut child,
                        stdout_reader,
                        stderr_reader,
                        ProcessError::with_source(
                            ProcessErrorKind::Io,
                            "cannot wait for subprocess",
                            source,
                        ),
                    );
                }
            }
        }
        if stdout_exceeded.load(Ordering::Relaxed) || stderr_exceeded.load(Ordering::Relaxed) {
            return fail_and_collect(
                &mut child,
                stdout_reader,
                stderr_reader,
                ProcessError::new(
                    ProcessErrorKind::OutputLimit,
                    "subprocess output exceeded its configured limit",
                ),
            );
        }
        if cancelled.load(Ordering::Relaxed) {
            return fail_and_collect(
                &mut child,
                stdout_reader,
                stderr_reader,
                ProcessError::new(ProcessErrorKind::Cancelled, "subprocess was cancelled"),
            );
        }
        if Instant::now() >= deadline {
            return fail_and_collect(
                &mut child,
                stdout_reader,
                stderr_reader,
                ProcessError::new(ProcessErrorKind::TimedOut, "subprocess timed out"),
            );
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
    terminate_remaining_group(&mut child);
    Ok(ProcessOutput {
        status,
        stdout: stdout?,
        stderr: stderr?,
    })
}

fn spawn_reader(
    reader: impl Read + Send + 'static,
    limit: usize,
    exceeded: Arc<AtomicBool>,
) -> JoinHandle<io::Result<Vec<u8>>> {
    thread::spawn(move || read_limited(reader, limit, &exceeded))
}

fn read_limited(mut reader: impl Read, limit: usize, exceeded: &AtomicBool) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(limit.min(8192));
    let mut buffer = [0_u8; 8192];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            return Ok(bytes);
        }
        if count > limit.saturating_sub(bytes.len()) {
            exceeded.store(true, Ordering::Relaxed);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "subprocess stream exceeded limit",
            ));
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
}

fn join_reader(
    reader: JoinHandle<io::Result<Vec<u8>>>,
    stream: &str,
) -> Result<Vec<u8>, ProcessError> {
    reader
        .join()
        .map_err(|_| ProcessError::new(ProcessErrorKind::Io, format!("{stream} reader panicked")))?
        .map_err(|source| {
            ProcessError::with_source(
                ProcessErrorKind::Io,
                format!("cannot read subprocess {stream}"),
                source,
            )
        })
}

fn fail_and_collect(
    child: &mut GroupChild,
    stdout: JoinHandle<io::Result<Vec<u8>>>,
    stderr: JoinHandle<io::Result<Vec<u8>>>,
    error: ProcessError,
) -> Result<ProcessOutput, ProcessError> {
    terminate_group(child);
    let _ = stdout.join();
    let _ = stderr.join();
    Err(error)
}

fn terminate_group(child: &mut GroupChild) {
    let _ = child.kill();
    let _ = child.wait();
}

fn terminate_remaining_group(child: &mut GroupChild) {
    let _ = child.kill();
}
