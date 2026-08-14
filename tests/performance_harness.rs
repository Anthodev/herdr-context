#![cfg(feature = "perf-harness")]

use std::path::Path;
use std::time::Duration;

use herdr_context::host::LaunchContext;
use herdr_context::perf::{
    BudgetVerdict, FilesProbe, ProcessMemory, capture_process_snapshot, clock_ticks_per_second,
    cpu_percent, measure_first_frame, parse_child_pids, parse_proc_stat_ticks, parse_proc_status,
    summarize_durations,
};
use tempfile::TempDir;

#[test]
fn duration_summary_uses_nearest_rank_percentiles() {
    let samples = (1..=20)
        .rev()
        .map(Duration::from_micros)
        .collect::<Vec<_>>();

    let summary = summarize_durations(&samples).expect("non-empty samples");

    assert_eq!(summary.samples(), 20);
    assert_eq!(summary.minimum_micros(), 1);
    assert_eq!(summary.p50_micros(), 10);
    assert_eq!(summary.p95_micros(), 19);
    assert_eq!(summary.maximum_micros(), 20);
}

#[test]
fn empty_duration_set_has_no_summary() {
    assert!(summarize_durations(&[]).is_none());
}

#[test]
fn upper_bound_verdict_is_inclusive_and_serializable() {
    let passing = BudgetVerdict::upper_bound("navigation_p95_ms", 50.0, 50.0, "ms");
    let failing = BudgetVerdict::upper_bound("navigation_p95_ms", 50.001, 50.0, "ms");

    assert!(passing.passed());
    assert!(!failing.passed());
    assert_eq!(passing.metric(), "navigation_p95_ms");
    assert_eq!(passing.limit(), 50.0);
    assert_eq!(passing.unit(), "ms");

    let encoded = serde_json::to_value(&failing).expect("verdict JSON");
    assert_eq!(encoded["passed"], false);
    assert_eq!(encoded["comparator"], "<=");
}

#[test]
fn proc_status_parser_reports_process_only_rss_and_peak() {
    let status =
        "Name:\therdr-context\nVmPeak:\t  99999 kB\nVmHWM:\t   49152 kB\nVmRSS:\t   32768 kB\n";

    assert_eq!(
        parse_proc_status(status).expect("valid proc status"),
        ProcessMemory::new(32 * 1024 * 1024, 48 * 1024 * 1024)
    );
}

#[test]
fn proc_stat_parser_handles_spaces_and_parentheses_in_process_name() {
    let stat = "42 (herdr (perf) worker) S 1 2 3 4 5 6 7 8 9 10 120 30 0 0 0 0 0 0";

    assert_eq!(parse_proc_stat_ticks(stat).expect("valid proc stat"), 150);
}

#[test]
fn cpu_percentage_uses_process_ticks_over_wall_time() {
    let observed =
        cpu_percent(1_000, 1_025, 100, Duration::from_secs(5)).expect("valid CPU observation");

    assert!((observed - 5.0).abs() < f64::EPSILON);
    assert!(cpu_percent(10, 9, 100, Duration::from_secs(1)).is_none());
    assert!(cpu_percent(0, 1, 0, Duration::from_secs(1)).is_none());
    assert!(cpu_percent(0, 1, 100, Duration::ZERO).is_none());
}

#[test]
fn child_pid_parser_deduplicates_task_children() {
    assert_eq!(
        parse_child_pids("42 7 42\n99")
            .into_iter()
            .collect::<Vec<_>>(),
        vec![7, 42, 99]
    );
    assert!(parse_child_pids("not-a-pid").is_empty());
}

#[test]
fn live_snapshot_is_process_local_and_internally_consistent() {
    let snapshot = capture_process_snapshot().expect("Linux process snapshot");

    assert!(snapshot.memory().rss_bytes() > 0);
    assert!(snapshot.memory().peak_rss_bytes() >= snapshot.memory().rss_bytes());
    assert!(snapshot.thread_count() >= 1);
    assert!(clock_ticks_per_second().expect("CLK_TCK") > 0);
}

#[test]
fn first_frame_measurement_does_not_start_background_work() {
    let temp = TempDir::new().expect("project");
    let summary = measure_first_frame(&context(temp.path()), 1, 3).expect("first frame");

    assert_eq!(summary.samples(), 3);
    assert!(
        capture_process_snapshot()
            .expect("post-frame process")
            .child_pids()
            .is_empty()
    );
}

#[expect(
    clippy::significant_drop_tightening,
    reason = "FilesProbe::shutdown consumes the probe and joins its workers before assertions"
)]
#[test]
fn files_probe_measures_changed_navigation_and_shuts_workers_down() {
    let temp = TempDir::new().expect("project");
    for index in 0..16 {
        std::fs::write(temp.path().join(format!("item-{index:02}")), b"synthetic")
            .expect("fixture file");
    }
    let mut probe = FilesProbe::new(&context(temp.path()), 80, 24).expect("files probe");

    let summary = probe.measure_navigation(2, 12).expect("navigation samples");
    let cleanup = probe.shutdown().expect("worker shutdown");

    assert_eq!(summary.samples(), 12);
    assert_eq!(cleanup.surviving_children(), 0);
    assert_eq!(cleanup.surviving_worker_threads(), 0);
}

#[test]
fn idle_probe_tracks_cpu_without_redrawing_clean_state() {
    let temp = TempDir::new().expect("project");
    let mut probe = FilesProbe::new(&context(temp.path()), 80, 24).expect("files probe");

    let idle = probe
        .measure_idle(Duration::from_millis(100), Duration::from_millis(10))
        .expect("idle observation");

    assert!(idle.cpu_percent().is_finite());
    assert_eq!(idle.redraws(), 0);
    probe.shutdown().expect("worker shutdown");
}

#[expect(
    clippy::significant_drop_tightening,
    reason = "FilesProbe::shutdown consumes the probe and joins its workers before assertions"
)]
#[test]
fn rapid_refresh_probe_settles_coalesced_work_before_cleanup() {
    let temp = TempDir::new().expect("project");
    let mut probe = FilesProbe::new(&context(temp.path()), 80, 24).expect("files probe");

    probe.start_background();
    for _ in 0..16 {
        probe.request_refresh();
    }
    let observation = probe
        .settle_background(Duration::from_secs(2))
        .expect("background settlement");
    let cleanup = probe.shutdown().expect("worker shutdown");

    assert!(observation.completions() >= 1);
    assert_eq!(cleanup.surviving_children(), 0);
    assert_eq!(cleanup.surviving_worker_threads(), 0);
}

fn context(root: &Path) -> LaunchContext {
    let value = serde_json::json!({
        "workspace_id": "perf-workspace",
        "tab_id": "perf-tab",
        "pane_id": "perf-pane",
        "cwd": root,
    })
    .to_string();
    LaunchContext::from_vars([("HERDR_PLUGIN_CONTEXT_JSON", value.as_str())])
        .expect("performance launch context")
}
