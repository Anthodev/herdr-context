//! Benchmark-only measurement contracts and process probes.
//!
//! This module is excluded from normal builds. It deliberately exposes only
//! deterministic calculations and Linux process-local observations used by the
//! HDC-15 harness.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::process::Command;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use serde::Serialize;

use crate::app::App;
use crate::host::LaunchContext;
use crate::intent::Intent;
use crate::runtime::{FilesRuntime, RuntimeMessage};
use crate::worker::WorkerRuntime;

static PERF_PROBE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct DurationSummary {
    samples: usize,
    minimum_micros: u64,
    p50_micros: u64,
    p95_micros: u64,
    maximum_micros: u64,
}

impl DurationSummary {
    #[must_use]
    pub const fn samples(self) -> usize {
        self.samples
    }

    #[must_use]
    pub const fn minimum_micros(self) -> u64 {
        self.minimum_micros
    }

    #[must_use]
    pub const fn p50_micros(self) -> u64 {
        self.p50_micros
    }

    #[must_use]
    pub const fn p95_micros(self) -> u64 {
        self.p95_micros
    }

    #[must_use]
    pub const fn maximum_micros(self) -> u64 {
        self.maximum_micros
    }
}

#[must_use]
pub fn summarize_durations(samples: &[Duration]) -> Option<DurationSummary> {
    if samples.is_empty() {
        return None;
    }
    let mut micros = samples
        .iter()
        .map(|sample| u64::try_from(sample.as_micros()).unwrap_or(u64::MAX))
        .collect::<Vec<_>>();
    micros.sort_unstable();
    Some(DurationSummary {
        samples: micros.len(),
        minimum_micros: micros[0],
        p50_micros: nearest_rank(&micros, 50),
        p95_micros: nearest_rank(&micros, 95),
        maximum_micros: micros[micros.len() - 1],
    })
}

fn nearest_rank(sorted: &[u64], percentile: usize) -> u64 {
    let rank = sorted.len().saturating_mul(percentile).div_ceil(100).max(1);
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BudgetVerdict {
    metric: String,
    observed: f64,
    limit: f64,
    unit: String,
    comparator: &'static str,
    passed: bool,
}

impl BudgetVerdict {
    #[must_use]
    pub fn upper_bound(
        metric: impl Into<String>,
        observed: f64,
        limit: f64,
        unit: impl Into<String>,
    ) -> Self {
        Self {
            metric: metric.into(),
            observed,
            limit,
            unit: unit.into(),
            comparator: "<=",
            passed: observed.is_finite() && limit.is_finite() && observed <= limit,
        }
    }

    #[must_use]
    pub fn metric(&self) -> &str {
        &self.metric
    }

    #[must_use]
    pub const fn limit(&self) -> f64 {
        self.limit
    }

    #[must_use]
    pub fn unit(&self) -> &str {
        &self.unit
    }

    #[must_use]
    pub const fn passed(&self) -> bool {
        self.passed
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ProcessMemory {
    rss_bytes: u64,
    peak_rss_bytes: u64,
}

impl ProcessMemory {
    #[must_use]
    pub const fn new(rss_bytes: u64, peak_rss_bytes: u64) -> Self {
        Self {
            rss_bytes,
            peak_rss_bytes,
        }
    }

    #[must_use]
    pub const fn rss_bytes(self) -> u64 {
        self.rss_bytes
    }

    #[must_use]
    pub const fn peak_rss_bytes(self) -> u64 {
        self.peak_rss_bytes
    }
}

#[must_use]
pub fn parse_proc_status(status: &str) -> Option<ProcessMemory> {
    let rss = proc_kib(status, "VmRSS:")?.checked_mul(1024)?;
    let peak = proc_kib(status, "VmHWM:")?.checked_mul(1024)?;
    Some(ProcessMemory::new(rss, peak))
}

fn proc_kib(status: &str, key: &str) -> Option<u64> {
    let line = status.lines().find(|line| line.starts_with(key))?;
    let mut fields = line[key.len()..].split_ascii_whitespace();
    let value = fields.next()?.parse().ok()?;
    (fields.next()? == "kB" && fields.next().is_none()).then_some(value)
}

#[must_use]
pub fn parse_proc_stat_ticks(stat: &str) -> Option<u64> {
    let command_end = stat.rfind(") ")?;
    let fields = stat[command_end + 2..]
        .split_ascii_whitespace()
        .collect::<Vec<_>>();
    let user = fields.get(11)?.parse::<u64>().ok()?;
    let system = fields.get(12)?.parse::<u64>().ok()?;
    user.checked_add(system)
}

#[must_use]
pub fn cpu_percent(
    start_ticks: u64,
    end_ticks: u64,
    ticks_per_second: u64,
    elapsed: Duration,
) -> Option<f64> {
    let delta = end_ticks.checked_sub(start_ticks)?;
    let wall_seconds = elapsed.as_secs_f64();
    if ticks_per_second == 0 || wall_seconds == 0.0 {
        return None;
    }
    Some(100.0 * delta as f64 / ticks_per_second as f64 / wall_seconds)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProcessSnapshot {
    memory: ProcessMemory,
    cpu_ticks: u64,
    thread_count: usize,
    worker_thread_count: usize,
    child_pids: BTreeSet<u32>,
}

impl ProcessSnapshot {
    #[must_use]
    pub const fn memory(&self) -> ProcessMemory {
        self.memory
    }

    #[must_use]
    pub const fn cpu_ticks(&self) -> u64 {
        self.cpu_ticks
    }

    #[must_use]
    pub const fn thread_count(&self) -> usize {
        self.thread_count
    }

    #[must_use]
    pub const fn worker_thread_count(&self) -> usize {
        self.worker_thread_count
    }

    #[must_use]
    pub const fn child_pids(&self) -> &BTreeSet<u32> {
        &self.child_pids
    }
}

pub fn capture_process_snapshot() -> io::Result<ProcessSnapshot> {
    let status = fs::read_to_string("/proc/self/status")?;
    let stat = fs::read_to_string("/proc/self/stat")?;
    let memory = parse_proc_status(&status)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid /proc/self/status"))?;
    let cpu_ticks = parse_proc_stat_ticks(&stat)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid /proc/self/stat"))?;
    let tasks = fs::read_dir("/proc/self/task")?.collect::<Result<Vec<_>, _>>()?;
    let mut child_pids = BTreeSet::new();
    let mut worker_thread_count = 0;
    for task in &tasks {
        let children = task.path().join("children");
        if let Ok(value) = fs::read_to_string(children) {
            child_pids.extend(parse_child_pids(&value));
        }
        if fs::read_to_string(task.path().join("comm"))
            .is_ok_and(|name| name.trim().starts_with("herdr-context-"))
        {
            worker_thread_count += 1;
        }
    }
    Ok(ProcessSnapshot {
        memory,
        cpu_ticks,
        thread_count: tasks.len(),
        worker_thread_count,
        child_pids,
    })
}

#[must_use]
pub fn parse_child_pids(value: &str) -> BTreeSet<u32> {
    value
        .split_ascii_whitespace()
        .filter_map(|pid| pid.parse().ok())
        .collect()
}

pub fn clock_ticks_per_second() -> io::Result<u64> {
    let output = Command::new("getconf").arg("CLK_TCK").output()?;
    if !output.status.success() {
        return Err(io::Error::other("getconf CLK_TCK failed"));
    }
    let value = std::str::from_utf8(&output.stdout)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid CLK_TCK"))?;
    Ok(value)
}

pub fn measure_first_frame(
    context: &LaunchContext,
    warmup_samples: usize,
    measured_samples: usize,
) -> io::Result<DurationSummary> {
    if measured_samples == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "first-frame sample count must be non-zero",
        ));
    }
    let mut samples = Vec::with_capacity(measured_samples);
    for index in 0..warmup_samples.saturating_add(measured_samples) {
        let started = Instant::now();
        let mut app = App::new(context.clone());
        let backend = TestBackend::new(80, 24);
        let mut terminal = match Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(error) => match error {},
        };
        match terminal.draw(|frame| app.render(frame)) {
            Ok(_) => {}
            Err(error) => match error {},
        }
        let elapsed = started.elapsed();
        if index >= warmup_samples {
            samples.push(elapsed);
        }
    }
    summarize_durations(&samples).ok_or_else(|| io::Error::other("first-frame samples disappeared"))
}

pub struct FilesProbe {
    files: FilesRuntime,
    workers: WorkerRuntime,
    area: Rect,
    buffer: Buffer,
    baseline_worker_threads: usize,
    _exclusive: MutexGuard<'static, ()>,
}

impl FilesProbe {
    pub fn new(context: &LaunchContext, width: u16, height: u16) -> io::Result<Self> {
        let exclusive = PERF_PROBE_LOCK
            .lock()
            .map_err(|_| io::Error::other("performance probe lock was poisoned"))?;
        let baseline_worker_threads = capture_process_snapshot()?.worker_thread_count();
        let area = Rect::new(0, 0, width, height);
        let mut files = FilesRuntime::bootstrap(context).map_err(io::Error::other)?;
        let mut buffer = Buffer::empty(area);
        files.render(area, &mut buffer);
        Ok(Self {
            files,
            workers: WorkerRuntime::new(),
            area,
            buffer,
            baseline_worker_threads,
            _exclusive: exclusive,
        })
    }

    pub fn measure_navigation(
        &mut self,
        warmup_samples: usize,
        measured_samples: usize,
    ) -> io::Result<DurationSummary> {
        if measured_samples == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "navigation sample count must be non-zero",
            ));
        }
        let mut samples = Vec::with_capacity(measured_samples);
        let total = warmup_samples.saturating_add(measured_samples);
        for index in 0..total {
            let intent = if index.is_multiple_of(2) {
                Intent::SelectNext
            } else {
                Intent::SelectPrevious
            };
            let started = Instant::now();
            if !self.files.handle_intent(&intent, &mut self.workers) {
                return Err(io::Error::other(
                    "navigation fixture did not change selection",
                ));
            }
            self.files.render(self.area, &mut self.buffer);
            let elapsed = started.elapsed();
            if index >= warmup_samples {
                samples.push(elapsed);
            }
        }
        summarize_durations(&samples)
            .ok_or_else(|| io::Error::other("navigation samples disappeared"))
    }

    pub fn measure_idle(
        &mut self,
        duration: Duration,
        tick_interval: Duration,
    ) -> io::Result<IdleObservation> {
        if duration.is_zero() || tick_interval.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "idle duration and tick interval must be non-zero",
            ));
        }
        let ticks_per_second = clock_ticks_per_second()?;
        let before = capture_process_snapshot()?;
        let started = Instant::now();
        let mut redraws = 0;
        while started.elapsed() < duration {
            let remaining = duration.saturating_sub(started.elapsed());
            std::thread::sleep(tick_interval.min(remaining));
            if self.files.tick(Instant::now(), &mut self.workers) {
                self.files.render(self.area, &mut self.buffer);
                redraws += 1;
            }
        }
        let elapsed = started.elapsed();
        let after = capture_process_snapshot()?;
        let cpu_percent = cpu_percent(
            before.cpu_ticks(),
            after.cpu_ticks(),
            ticks_per_second,
            elapsed,
        )
        .ok_or_else(|| io::Error::other("invalid idle CPU observation"))?;
        Ok(IdleObservation {
            cpu_percent,
            redraws,
            memory: after.memory(),
        })
    }

    pub fn start_background(&mut self) {
        self.files.start_background(&mut self.workers);
    }

    pub fn request_refresh(&mut self) -> bool {
        self.files
            .handle_intent(&Intent::Refresh, &mut self.workers)
    }

    pub fn settle_background(&mut self, timeout: Duration) -> io::Result<BackgroundObservation> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| io::Error::other("background deadline overflow"))?;
        let mut completions = 0;
        let mut redraws = 0;
        while self.workers.has_pending_work() || self.files.has_pending_work() {
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "performance background work did not settle",
                ));
            }
            let mut dirty = self.files.tick(Instant::now(), &mut self.workers);
            if let Some(result) = self.workers.recv_timeout(Duration::from_millis(10)) {
                completions += 1;
                let kind = result.key().kind();
                let generation = result.generation();
                if result.panicked() {
                    self.files
                        .fail_background(kind, generation, &mut self.workers);
                    dirty = true;
                } else {
                    let message = result
                        .downcast::<RuntimeMessage>()
                        .map_err(|_| io::Error::other("unexpected performance worker result"))?;
                    dirty |= self.files.complete_background(*message, &mut self.workers);
                }
            }
            if dirty {
                self.files.render(self.area, &mut self.buffer);
                redraws += 1;
            }
        }
        Ok(BackgroundObservation {
            completions,
            redraws,
        })
    }

    pub fn shutdown(mut self) -> io::Result<CleanupObservation> {
        self.workers.shutdown();
        let snapshot = capture_process_snapshot()?;
        Ok(CleanupObservation {
            surviving_children: snapshot.child_pids().len(),
            surviving_worker_threads: snapshot
                .worker_thread_count()
                .saturating_sub(self.baseline_worker_threads),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct BackgroundObservation {
    completions: usize,
    redraws: usize,
}

impl BackgroundObservation {
    #[must_use]
    pub const fn completions(self) -> usize {
        self.completions
    }

    #[must_use]
    pub const fn redraws(self) -> usize {
        self.redraws
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct IdleObservation {
    cpu_percent: f64,
    redraws: usize,
    memory: ProcessMemory,
}

impl IdleObservation {
    #[must_use]
    pub const fn cpu_percent(self) -> f64 {
        self.cpu_percent
    }

    #[must_use]
    pub const fn redraws(self) -> usize {
        self.redraws
    }

    #[must_use]
    pub const fn memory(self) -> ProcessMemory {
        self.memory
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct CleanupObservation {
    surviving_children: usize,
    surviving_worker_threads: usize,
}

impl CleanupObservation {
    #[must_use]
    pub const fn surviving_children(self) -> usize {
        self.surviving_children
    }

    #[must_use]
    pub const fn surviving_worker_threads(self) -> usize {
        self.surviving_worker_threads
    }
}
