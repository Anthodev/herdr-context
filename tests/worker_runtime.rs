use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, mpsc};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[cfg(unix)]
use herdr_context::worker::process::{ProcessErrorKind, ProcessSpec, run};
#[cfg(unix)]
use tempfile::TempDir;

use herdr_context::worker::{Job, JobKey, JobKind, Priority, SubmitStatus, WorkerRuntime};

fn wait_for_result(runtime: &mut WorkerRuntime) -> herdr_context::worker::CompletedJob {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(result) = runtime.recv_timeout(Duration::from_millis(20)) {
            return result;
        }
        assert!(Instant::now() < deadline, "worker result timed out");
    }
}

#[test]
fn replaceable_jobs_keep_only_the_latest_generation() {
    let mut runtime = WorkerRuntime::with_capacities(1, 1);
    let key = JobKey::new(JobKind::Filesystem, Path::new("/project"));
    let barrier = Arc::new(Barrier::new(2));
    let worker_barrier = Arc::clone(&barrier);

    assert_eq!(
        runtime.submit(Job::new(key.clone(), 1, Priority::High, move |_| {
            worker_barrier.wait();
            Box::new(1_u64)
        })),
        SubmitStatus::Queued
    );
    assert_eq!(
        runtime.submit(Job::new(key.clone(), 2, Priority::High, |_| Box::new(
            2_u64
        ))),
        SubmitStatus::Coalesced
    );
    assert_eq!(
        runtime.submit(Job::new(key.clone(), 3, Priority::High, |_| Box::new(
            3_u64
        ))),
        SubmitStatus::Coalesced
    );

    barrier.wait();
    let result = wait_for_result(&mut runtime);
    assert_eq!(result.key(), &key);
    assert_eq!(result.generation(), 3);
    assert_eq!(*result.downcast::<u64>().expect("u64 result"), 3);
    assert!(runtime.recv_timeout(Duration::from_millis(20)).is_none());
}

#[test]
fn high_priority_queue_applies_backpressure_at_its_bound() {
    let mut runtime = WorkerRuntime::with_capacities(1, 1);
    let barrier = Arc::new(Barrier::new(2));
    let worker_barrier = Arc::clone(&barrier);
    let (started_sender, started_receiver) = mpsc::sync_channel(1);

    assert_eq!(
        runtime.submit(Job::new(
            JobKey::new(JobKind::Filesystem, Path::new("/one")),
            1,
            Priority::High,
            move |_| {
                started_sender.send(()).expect("started");
                worker_barrier.wait();
                Box::new(())
            },
        )),
        SubmitStatus::Queued
    );
    started_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("worker started");
    assert_eq!(
        runtime.submit(Job::new(
            JobKey::new(JobKind::Vcs, Path::new("/two")),
            1,
            Priority::High,
            |_| Box::new(()),
        )),
        SubmitStatus::Queued
    );
    assert_eq!(
        runtime.submit(Job::new(
            JobKey::new(JobKind::Bootstrap, Path::new("/three")),
            1,
            Priority::High,
            |_| Box::new(()),
        )),
        SubmitStatus::Backpressure
    );

    barrier.wait();
}

#[test]
fn completion_backlog_is_bounded_without_dropping_accepted_results() {
    let mut runtime = WorkerRuntime::with_capacities(1, 1);
    let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    for index in 0..4 {
        let worker_completed = Arc::clone(&completed);
        assert_eq!(
            runtime.submit(Job::new(
                JobKey::new(JobKind::Filesystem, format!("/{index}")),
                1,
                Priority::High,
                move |_| {
                    worker_completed.fetch_add(1, Ordering::Relaxed);
                    Box::new(index)
                },
            )),
            SubmitStatus::Queued
        );
        let deadline = Instant::now() + Duration::from_secs(1);
        while completed.load(Ordering::Relaxed) <= index {
            assert!(Instant::now() < deadline, "worker did not complete");
            thread::yield_now();
        }
        thread::sleep(Duration::from_millis(5));
    }

    assert_eq!(
        runtime.submit(Job::new(
            JobKey::new(JobKind::Filesystem, Path::new("/overflow")),
            1,
            Priority::High,
            |_| Box::new(()),
        )),
        SubmitStatus::Backpressure
    );
    for _ in 0..4 {
        assert!(wait_for_result(&mut runtime).downcast::<usize>().is_ok());
    }
    assert!(!runtime.has_pending_work());
}

#[test]
fn shutdown_cancels_and_joins_running_workers() {
    let mut runtime = WorkerRuntime::with_capacities(1, 1);
    let observed_cancel = Arc::new(AtomicBool::new(false));
    let worker_observed_cancel = Arc::clone(&observed_cancel);
    let (started_sender, started_receiver) = mpsc::sync_channel(1);

    runtime.submit(Job::new(
        JobKey::new(JobKind::ConversationDiscovery, Path::new("/project")),
        1,
        Priority::Low,
        move |cancelled| {
            started_sender.send(()).expect("started");
            while !cancelled.load(Ordering::Relaxed) {
                thread::yield_now();
            }
            worker_observed_cancel.store(true, Ordering::Relaxed);
            Box::new(())
        },
    ));
    started_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("worker started");

    runtime.shutdown();
    assert!(observed_cancel.load(Ordering::Relaxed));
    assert!(!runtime.has_pending_work());
}

#[cfg(unix)]
fn executable_script(directory: &TempDir, body: &str) -> std::path::PathBuf {
    let path = directory.path().join("worker-command");
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write script");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("chmod script");
    path
}

#[cfg(unix)]
#[test]
fn subprocess_timeout_terminates_and_collects_the_process_group() {
    let temp = TempDir::new().expect("tempdir");
    let marker = temp.path().join("descendant-survived");
    let script = executable_script(
        &temp,
        &format!(
            "(sleep 0.2; printf survived > '{}') &\nsleep 5",
            marker.display()
        ),
    );
    let cancelled = AtomicBool::new(false);
    let started = Instant::now();

    let error = run(
        &ProcessSpec::new(script).timeout(Duration::from_millis(50)),
        &cancelled,
    )
    .expect_err("timeout");

    assert_eq!(error.kind(), ProcessErrorKind::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(1));
    thread::sleep(Duration::from_millis(300));
    assert!(
        !marker.exists(),
        "descendant survived process-group timeout"
    );
}

#[cfg(unix)]
#[test]
fn already_cancelled_subprocess_is_never_spawned() {
    let cancelled = AtomicBool::new(true);
    let error = run(
        &ProcessSpec::new("/definitely/not/a/real/executable"),
        &cancelled,
    )
    .expect_err("cancelled");

    assert_eq!(error.kind(), ProcessErrorKind::Cancelled);
}

#[cfg(unix)]
#[test]
fn subprocess_collects_bounded_stdout_and_stderr() {
    let temp = TempDir::new().expect("tempdir");
    let script = executable_script(&temp, "printf output\nprintf warning >&2");
    let cancelled = AtomicBool::new(false);

    let output = run(
        &ProcessSpec::new(script)
            .timeout(Duration::from_secs(1))
            .output_limits(32, 32),
        &cancelled,
    )
    .expect("process output");

    assert!(output.status().success());
    assert_eq!(output.stdout(), b"output");
    assert_eq!(output.stderr(), b"warning");
}
