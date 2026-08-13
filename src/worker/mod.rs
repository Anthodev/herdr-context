//! Bounded background execution with keyed, latest-wins jobs.

pub mod process;

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub type JobOutput = Box<dyn Any + Send>;
type JobTask = Box<dyn FnOnce(&AtomicBool) -> JobOutput + Send + 'static>;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum JobKind {
    Bootstrap,
    Filesystem,
    Vcs,
    ConversationDiscovery,
    Process,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct JobKey {
    kind: JobKind,
    root: PathBuf,
}

impl JobKey {
    #[must_use]
    pub fn new(kind: JobKind, root: impl AsRef<Path>) -> Self {
        Self {
            kind,
            root: root.as_ref().to_path_buf(),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> JobKind {
        self.kind
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Priority {
    High,
    Low,
}

pub struct Job {
    key: JobKey,
    generation: u64,
    priority: Priority,
    task: JobTask,
}

impl Job {
    pub fn new<F>(key: JobKey, generation: u64, priority: Priority, task: F) -> Self
    where
        F: FnOnce(&AtomicBool) -> JobOutput + Send + 'static,
    {
        Self {
            key,
            generation,
            priority,
            task: Box::new(task),
        }
    }
}

impl fmt::Debug for Job {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Job")
            .field("key", &self.key)
            .field("generation", &self.generation)
            .field("priority", &self.priority)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmitStatus {
    Queued,
    Coalesced,
    RejectedStale,
    Backpressure,
    ShuttingDown,
}

enum Completion {
    Success(JobOutput),
    Panicked,
}

struct RawCompletion {
    key: JobKey,
    generation: u64,
    completion: Completion,
}

pub struct CompletedJob {
    key: JobKey,
    generation: u64,
    completion: Completion,
}

impl CompletedJob {
    #[must_use]
    pub const fn key(&self) -> &JobKey {
        &self.key
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub const fn panicked(&self) -> bool {
        matches!(self.completion, Completion::Panicked)
    }

    pub fn downcast<T: Any + Send>(self) -> Result<Box<T>, Self> {
        let Self {
            key,
            generation,
            completion,
        } = self;
        match completion {
            Completion::Success(output) => match output.downcast::<T>() {
                Ok(output) => Ok(output),
                Err(output) => Err(Self {
                    key,
                    generation,
                    completion: Completion::Success(output),
                }),
            },
            Completion::Panicked => Err(Self {
                key,
                generation,
                completion: Completion::Panicked,
            }),
        }
    }
}

impl fmt::Debug for CompletedJob {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompletedJob")
            .field("key", &self.key)
            .field("generation", &self.generation)
            .field("panicked", &self.panicked())
            .finish()
    }
}

struct Lane {
    sender: Option<SyncSender<Job>>,
    worker: Option<JoinHandle<()>>,
}

impl Lane {
    fn spawn(
        name: &'static str,
        capacity: usize,
        results: SyncSender<RawCompletion>,
        cancelled: Arc<AtomicBool>,
    ) -> Self {
        let (sender, receiver) = mpsc::sync_channel(capacity);
        let worker = thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || run_lane(receiver, &results, &cancelled))
            .expect("worker thread creation failed");
        Self {
            sender: Some(sender),
            worker: Some(worker),
        }
    }

    fn try_send(&self, job: Job) -> Result<(), Job> {
        let Some(sender) = &self.sender else {
            return Err(job);
        };
        match sender.try_send(job) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(job) | TrySendError::Disconnected(job)) => Err(job),
        }
    }

    fn stop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub struct WorkerRuntime {
    high: Lane,
    low: Lane,
    results: Receiver<RawCompletion>,
    cancelled: Arc<AtomicBool>,
    active: HashSet<JobKey>,
    latest: HashMap<JobKey, u64>,
    pending: HashMap<JobKey, Job>,
    outstanding_capacity: usize,
    shutting_down: bool,
}

impl WorkerRuntime {
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacities(8, 4)
    }

    #[must_use]
    pub fn with_capacities(high_capacity: usize, low_capacity: usize) -> Self {
        let result_capacity = high_capacity
            .saturating_add(low_capacity)
            .saturating_add(2)
            .max(2);
        let (result_sender, results) = mpsc::sync_channel(result_capacity);
        let cancelled = Arc::new(AtomicBool::new(false));
        let high = Lane::spawn(
            "herdr-context-high",
            high_capacity,
            result_sender.clone(),
            Arc::clone(&cancelled),
        );
        let low = Lane::spawn(
            "herdr-context-low",
            low_capacity,
            result_sender,
            Arc::clone(&cancelled),
        );
        Self {
            high,
            low,
            results,
            cancelled,
            active: HashSet::new(),
            latest: HashMap::new(),
            pending: HashMap::new(),
            outstanding_capacity: result_capacity,
            shutting_down: false,
        }
    }

    pub fn submit(&mut self, job: Job) -> SubmitStatus {
        if self.shutting_down {
            return SubmitStatus::ShuttingDown;
        }
        if self
            .latest
            .get(&job.key)
            .is_some_and(|generation| job.generation <= *generation)
        {
            return SubmitStatus::RejectedStale;
        }
        if self.active.contains(&job.key) || self.pending.contains_key(&job.key) {
            self.latest.insert(job.key.clone(), job.generation);
            self.pending.insert(job.key.clone(), job);
            return SubmitStatus::Coalesced;
        }

        self.queue_new(job)
    }

    fn outstanding_len(&self) -> usize {
        self.active.len()
            + self
                .pending
                .keys()
                .filter(|key| !self.active.contains(*key))
                .count()
    }
    fn queue_new(&mut self, job: Job) -> SubmitStatus {
        if self.outstanding_len() >= self.outstanding_capacity {
            return SubmitStatus::Backpressure;
        }
        let key = job.key.clone();
        let generation = job.generation;
        if self.lane(job.priority).try_send(job).is_err() {
            return SubmitStatus::Backpressure;
        }
        self.latest.insert(key.clone(), generation);
        self.active.insert(key);
        SubmitStatus::Queued
    }

    const fn lane(&self, priority: Priority) -> &Lane {
        match priority {
            Priority::High => &self.high,
            Priority::Low => &self.low,
        }
    }

    #[must_use]
    pub fn has_pending_work(&self) -> bool {
        !self.active.is_empty() || !self.pending.is_empty()
    }

    pub fn try_recv(&mut self) -> Option<CompletedJob> {
        loop {
            match self.results.try_recv() {
                Ok(result) => {
                    if let Some(result) = self.accept_completion(result) {
                        return Some(result);
                    }
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => {
                    self.pump_pending();
                    return None;
                }
            }
        }
    }

    pub fn recv_timeout(&mut self, timeout: Duration) -> Option<CompletedJob> {
        if let Some(result) = self.try_recv() {
            return Some(result);
        }
        let deadline = Instant::now().checked_add(timeout)?;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let result = self.results.recv_timeout(remaining).ok()?;
            if let Some(result) = self.accept_completion(result) {
                return Some(result);
            }
        }
    }

    fn accept_completion(&mut self, result: RawCompletion) -> Option<CompletedJob> {
        self.active.remove(&result.key);
        self.pump_key(&result.key);
        let latest = self.latest.get(&result.key).copied();
        (latest == Some(result.generation)).then_some(CompletedJob {
            key: result.key,
            generation: result.generation,
            completion: result.completion,
        })
    }

    fn pump_key(&mut self, key: &JobKey) {
        let Some(job) = self.pending.remove(key) else {
            return;
        };
        match self.lane(job.priority).try_send(job) {
            Ok(()) => {
                self.active.insert(key.clone());
            }
            Err(job) => {
                self.pending.insert(key.clone(), job);
            }
        }
    }

    fn pump_pending(&mut self) {
        let keys = self.pending.keys().cloned().collect::<Vec<_>>();
        for key in keys {
            if !self.active.contains(&key) {
                self.pump_key(&key);
            }
        }
    }

    pub fn shutdown(&mut self) {
        if self.shutting_down {
            return;
        }
        self.shutting_down = true;
        self.cancelled.store(true, Ordering::Relaxed);
        self.pending.clear();
        self.high.stop();
        self.low.stop();
        while self.results.try_recv().is_ok() {}
        self.active.clear();
        self.latest.clear();
    }
}

impl Default for WorkerRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for WorkerRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run_lane(receiver: Receiver<Job>, results: &SyncSender<RawCompletion>, cancelled: &AtomicBool) {
    while let Ok(job) = receiver.recv() {
        if cancelled.load(Ordering::Relaxed) {
            continue;
        }
        let Job {
            key,
            generation,
            task,
            ..
        } = job;
        let completion = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| task(cancelled)))
            .map_or(Completion::Panicked, Completion::Success);
        if results
            .send(RawCompletion {
                key,
                generation,
                completion,
            })
            .is_err()
        {
            break;
        }
    }
}
