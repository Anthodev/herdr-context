use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use crate::perf_fixtures::{EXTERNAL_SESSION_COUNT, LOCAL_SESSION_COUNT, PerformanceFixtures};
use herdr_context::conversations::index::ConversationIndex;
use herdr_context::conversations::sources::{
    ConversationSource, DiscoveryLimit, GenericJsonlSource, KnownStoreRoots, MetadataBudget,
    ProjectLocalLocation, SourceRegistry,
};
use herdr_context::files::tree::{FilesTree, TreeNodeKind};
use herdr_context::host::LaunchContext;
use herdr_context::perf::{
    DurationSummary, FilesProbe, capture_process_snapshot, measure_first_frame, summarize_durations,
};
use herdr_context::project::ProjectIdentity;
use herdr_context::vcs::git::GitService;
use herdr_context::vcs::jj::{JjService, JujutsuMode};
use herdr_context::vcs::{VcsEntryStatus, VcsService, VcsStatusKind, VcsStatusSnapshot};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug)]
pub struct RunConfig {
    pub warmup_samples: usize,
    pub measured_samples: usize,
    pub idle_duration: Duration,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Metric {
    pub name: String,
    pub workload: String,
    pub observed: f64,
    pub unit: String,
    pub warmup_samples: usize,
    pub samples: usize,
    pub p50: Option<f64>,
    pub p95: Option<f64>,
    pub note: String,
}

impl Metric {
    fn value(
        name: &str,
        workload: &str,
        observed: f64,
        unit: &str,
        note: impl Into<String>,
    ) -> Self {
        Self {
            warmup_samples: 0,
            name: name.to_owned(),
            workload: workload.to_owned(),
            observed,
            unit: unit.to_owned(),
            samples: 1,
            p50: None,
            p95: None,
            note: note.into(),
        }
    }

    fn durations(
        name: &str,
        workload: &str,
        warmup_samples: usize,
        summary: DurationSummary,
        note: &str,
    ) -> Self {
        let p50 = summary.p50_micros() as f64 / 1_000.0;
        let p95 = summary.p95_micros() as f64 / 1_000.0;
        Self {
            name: name.to_owned(),
            workload: workload.to_owned(),
            observed: p95,
            unit: "ms".to_owned(),
            warmup_samples,
            samples: summary.samples(),
            p50: Some(p50),
            p95: Some(p95),
            note: note.to_owned(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CaseResult {
    pub case: String,
    pub metrics: Vec<Metric>,
    pub risks: Vec<String>,
}

pub fn run_case(
    name: &str,
    fixtures: &PerformanceFixtures,
    config: RunConfig,
) -> io::Result<CaseResult> {
    match name {
        "ui" => ui_case(fixtures, config),
        "filesystem" => filesystem_case(fixtures, config),
        "conversations" => conversations_case(fixtures),
        "concurrency" => concurrency_case(fixtures),
        "rss-exclusion" => rss_exclusion_case(),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown performance case {name:?}"),
        )),
    }
}

pub fn memory_helper() -> ! {
    let mut allocation = vec![0_u8; 64 * 1024 * 1024];
    for byte in allocation.iter_mut().step_by(4_096) {
        *byte = 1;
    }
    println!("ready");
    std::hint::black_box(&allocation);
    std::thread::sleep(Duration::from_secs(30));
    std::process::exit(0)
}

#[expect(
    clippy::significant_drop_tightening,
    reason = "FilesProbe::shutdown consumes the probe and joins its workers before report assembly"
)]
fn ui_case(fixtures: &PerformanceFixtures, config: RunConfig) -> io::Result<CaseResult> {
    let context = launch_context(fixtures.no_vcs())?;
    let first_frame =
        measure_first_frame(&context, config.warmup_samples, config.measured_samples)?;
    let mut probe = FilesProbe::new(&context, 80, 24)?;
    let navigation =
        probe.measure_navigation(config.warmup_samples, config.measured_samples.max(100))?;
    let idle = probe.measure_idle(config.idle_duration, Duration::from_millis(50))?;
    let cleanup = probe.shutdown()?;
    let snapshot = capture_process_snapshot()?;

    Ok(CaseResult {
        case: "ui".to_owned(),
        metrics: vec![
            Metric::durations(
                "first_frame_p95_ms",
                "no-vcs first shell frame",
                config.warmup_samples,
                first_frame,
                "Includes App construction and a fresh 80x24 TestBackend draw; no background start.",
            ),
            Metric::durations(
                "navigation_p95_ms",
                "selection change plus 80x24 Files render",
                config.warmup_samples,
                navigation,
                "Alternates next/previous so every measured sample changes visible selection.",
            ),
            Metric::value(
                "idle_cpu_percent",
                "settled no-vcs Files view",
                idle.cpu_percent(),
                "%",
                "Process CPU ticks over wall time; 50 ms scheduler tick simulation.",
            ),
            Metric::value(
                "idle_redraws",
                "settled no-vcs Files view",
                idle.redraws() as f64,
                "count",
                "Dirty-driven redraw count during the idle interval.",
            ),
            Metric::value(
                "steady_rss_mib",
                "settled UI process",
                bytes_to_mib(idle.memory().rss_bytes()),
                "MiB",
                "VmRSS from /proc/self/status; child memory is excluded by kernel accounting.",
            ),
            Metric::value(
                "peak_rss_mib",
                "isolated UI case process",
                bytes_to_mib(snapshot.memory().peak_rss_bytes()),
                "MiB",
                "VmHWM from /proc/self/status in a fresh case process.",
            ),
            cleanup_metric("surviving_workers", cleanup.surviving_worker_threads()),
            cleanup_metric("surviving_children", cleanup.surviving_children()),
        ],
        risks: vec![
            "TestBackend excludes terminal driver and multiplexer transport latency.".to_owned(),
            "Idle sampling uses Linux scheduler ticks, whose resolution is CLK_TCK-bound."
                .to_owned(),
        ],
    })
}

fn filesystem_case(fixtures: &PerformanceFixtures, config: RunConfig) -> io::Result<CaseResult> {
    let tree_warmup_samples = config.warmup_samples.min(2);
    let tree_measured_samples = config.measured_samples.clamp(3, 10);
    let vcs_warmup_samples = 1;
    let virtual_status = virtual_status_fixture()?;
    let vcs_measured_samples = 5;
    let tree = sample_validated(
        tree_warmup_samples,
        tree_measured_samples,
        || load_ignore_heavy_tree(fixtures.monorepo()),
        validate_ignore_heavy_tree,
    )?;
    let merge = measure_virtual_status_merge(
        fixtures.no_vcs(),
        &virtual_status,
        tree_warmup_samples,
        tree_measured_samples,
    )?;
    let git = sample_validated(
        vcs_warmup_samples,
        vcs_measured_samples,
        || measure_git_status(fixtures.small_git()),
        validate_git_status,
    )?;
    let native_jj = sample_validated(
        vcs_warmup_samples,
        vcs_measured_samples,
        || measure_jj_status(fixtures.native_jj(), JujutsuMode::Fresh),
        |snapshot| validate_jj_status(snapshot, JujutsuMode::Fresh),
    )?;
    let colocated_jj = sample_validated(
        vcs_warmup_samples,
        vcs_measured_samples,
        || measure_jj_status(fixtures.colocated_jj(), JujutsuMode::Passive),
        |snapshot| validate_jj_status(snapshot, JujutsuMode::Passive),
    )?;
    let snapshot = capture_process_snapshot()?;

    Ok(CaseResult {
        case: "filesystem".to_owned(),
        metrics: vec![
            Metric::durations(
                "ignore_heavy_tree_p95_ms",
                "1,024 visible and 4,096 ignored monorepo files",
                tree_warmup_samples,
                tree,
                "Loads every visible directory; ignored target trees must never enter the model.",
            ),
            Metric::durations(
                "status_merge_p95_ms",
                "5,000 virtual Git status entries",
                tree_warmup_samples,
                merge,
                "Measures FilesTree merge and display-parent construction.",
            ),
            Metric::durations(
                "small_git_status_p95_ms",
                "small Git workspace",
                vcs_warmup_samples,
                git,
                "Includes hardened config inspection and porcelain-v2 status child processes.",
            ),
            Metric::durations(
                "native_jj_status_p95_ms",
                "non-colocated Jujutsu workspace",
                vcs_warmup_samples,
                native_jj,
                "Fresh mode snapshots and parses the working-copy diff.",
            ),
            Metric::durations(
                "colocated_jj_status_p95_ms",
                "colocated Jujutsu workspace",
                vcs_warmup_samples,
                colocated_jj,
                "Passive mode avoids mutation and reports stale status.",
            ),
            Metric::value(
                "steady_rss_mib",
                "isolated filesystem case process",
                bytes_to_mib(snapshot.memory().rss_bytes()),
                "MiB",
                "Process-only VmRSS after tree, merge, Git, and Jujutsu workloads.",
            ),
            Metric::value(
                "peak_rss_mib",
                "isolated filesystem case process",
                bytes_to_mib(snapshot.memory().peak_rss_bytes()),
                "MiB",
                "Process-only VmHWM across tree, merge, Git, and Jujutsu workloads.",
            ),
            Metric::value(
                "filesystem_surviving_children",
                "post-workload cleanup",
                snapshot.child_pids().len() as f64,
                "count",
                "Every status subprocess has completed before observation.",
            ),
        ],
        risks: vec![
            "Synthetic repositories do not model slow disks, network filesystems, or huge Git indexes."
                .to_owned(),
        ],
    })
}

fn conversations_case(fixtures: &PerformanceFixtures) -> io::Result<CaseResult> {
    if !fixtures.append_transcript().is_file() {
        return Err(io::Error::other("append transcript fixture is missing"));
    }
    reset_directory(&fixtures.state().join("perf-local-index"))?;
    reset_directory(&fixtures.state().join("perf-external-index"))?;

    let local_project = project(fixtures.local_project())?;
    let local_registry = SourceRegistry::new(vec![Box::new(
        GenericJsonlSource::new(
            local_project.clone(),
            [ProjectLocalLocation::new(".herdr/conversations").map_err(io::Error::other)?],
        )
        .map_err(io::Error::other)?,
    )])
    .map_err(io::Error::other)?;
    let local_started = Instant::now();
    let mut local_index =
        ConversationIndex::open(fixtures.state().join("perf-local-index"), local_project)
            .map_err(io::Error::other)?;
    refresh_all(&mut local_index, &local_registry)?;
    let local_elapsed = local_started.elapsed();
    if local_index.len() != LOCAL_SESSION_COUNT {
        return Err(io::Error::other(format!(
            "indexed {} local sessions instead of {LOCAL_SESSION_COUNT}",
            local_index.len()
        )));
    }

    let external_project = project(fixtures.external_project())?;
    let external_sources = KnownStoreRoots::under_home(fixtures.home())
        .sources(external_project.clone())
        .map_err(io::Error::other)?;
    let source_count = external_sources.len();
    let external_registry = SourceRegistry::new(external_sources).map_err(io::Error::other)?;
    let mut external_index = ConversationIndex::open(
        fixtures.state().join("perf-external-index"),
        external_project,
    )
    .map_err(io::Error::other)?;
    let recent_started = Instant::now();
    let recent = external_index
        .refresh_page(&external_registry, discovery_limit(64)?, metadata_budget()?)
        .map_err(io::Error::other)?;
    let recent_elapsed = recent_started.elapsed();
    let recent_count = external_index.len();
    let expected_recent_count = 64 + 3;
    if recent_count != expected_recent_count || !recent.has_more() {
        return Err(io::Error::other(format!(
            "recent external page was not bounded: {recent_count} sessions, has_more={}",
            recent.has_more()
        )));
    }
    let total_started = Instant::now();
    let mut pages = 1;
    let mut has_more = recent.has_more();
    while has_more {
        if pages >= 64 {
            return Err(io::Error::other(
                "external archive exceeded 64 bounded pages",
            ));
        }
        let refresh = external_index
            .refresh_page(&external_registry, discovery_limit(64)?, metadata_budget()?)
            .map_err(io::Error::other)?;
        has_more = refresh.has_more();
        pages += 1;
    }
    let archive_elapsed = recent_elapsed.saturating_add(total_started.elapsed());
    let archive_count = external_index.len();
    let tools = external_index
        .page(0, archive_count.max(1))
        .conversations()
        .iter()
        .map(|conversation| conversation.tool().as_str().to_owned())
        .collect::<BTreeSet<_>>();
    let expected_tools =
        BTreeSet::from(["claude-code", "codex-cli", "omp", "pi"].map(str::to_owned));
    if archive_count != EXTERNAL_SESSION_COUNT + 3 || tools != expected_tools {
        return Err(io::Error::other(format!(
            "external coverage mismatch: {archive_count} sessions across {tools:?}"
        )));
    }

    fixtures.reset_append_transcript()?;
    let append_project = project(fixtures.append_project())?;
    let append_source = GenericJsonlSource::new(
        append_project.clone(),
        [ProjectLocalLocation::new(".herdr/conversations").map_err(io::Error::other)?],
    )
    .map_err(io::Error::other)?;
    let initial = append_source
        .discover(&append_project, None, discovery_limit(8)?)
        .map_err(io::Error::other)?;
    let watermark = initial
        .next_watermark()
        .cloned()
        .ok_or_else(|| io::Error::other("append fixture did not publish a watermark"))?;
    fixtures.append_synthetic_record()?;
    let append_started = Instant::now();
    let appended = append_source
        .discover(&append_project, Some(&watermark), discovery_limit(8)?)
        .map_err(io::Error::other)?;
    if !appended.errors().is_empty() || appended.has_more() || appended.candidates().len() != 1 {
        return Err(io::Error::other(format!(
            "append discovery returned {} candidates, has_more={}, errors={:?}",
            appended.candidates().len(),
            appended.has_more(),
            appended.errors()
        )));
    }
    let appended_conversation = append_source
        .extract_metadata(&appended.candidates()[0], metadata_budget()?)
        .map_err(io::Error::other)?;
    if appended_conversation.session_reference().id() != "appending-session"
        || appended_conversation.updated_at() != UNIX_EPOCH + Duration::from_secs(1_767_225_601)
    {
        return Err(io::Error::other(
            "append discovery did not expose the appended assistant record",
        ));
    }
    let append_elapsed = append_started.elapsed();
    let snapshot = capture_process_snapshot()?;

    Ok(CaseResult {
        case: "conversations".to_owned(),
        metrics: vec![
            duration_value(
                "local_history_total_ms",
                "64 project-local generic JSONL sessions",
                local_elapsed,
                "Complete bounded local-history indexing.",
            ),
            Metric::value(
                "local_history_session_count",
                "complete project-local archive",
                local_index.len() as f64,
                "sessions",
                "Validated after bounded paging completes.",
            ),
            duration_value(
                "recent_metadata_ms",
                "2,048 external sessions plus multiple adapters",
                recent_elapsed,
                "Time until the first bounded metadata page is available.",
            ),
            Metric::value(
                "recent_metadata_count",
                "first external page",
                recent_count as f64,
                "sessions",
                "Count available before total archive discovery completes.",
            ),
            duration_value(
                "archive_discovery_total_ms",
                "complete external archive",
                archive_elapsed,
                format!("{pages} bounded pages; recent availability measured separately."),
            ),
            Metric::value(
                "archive_session_count",
                "complete external archive",
                archive_count as f64,
                "sessions",
                "Metadata-only indexed sessions after all bounded pages.",
            ),
            duration_value(
                "append_incremental_ms",
                "concurrently appendable generic JSONL transcript",
                append_elapsed,
                "Discovers and extracts only the safe appended suffix after a watermark.",
            ),
            Metric::value(
                "conversation_tool_count",
                "known external stores",
                tools.len() as f64,
                "tools",
                format!("{} registered sources; discovered tools: {}", source_count, tools.into_iter().collect::<Vec<_>>().join(", ")),
            ),
            Metric::value(
                "steady_rss_mib",
                "isolated conversation case process",
                bytes_to_mib(snapshot.memory().rss_bytes()),
                "MiB",
                "Process-only RSS after complete archive indexing.",
            ),
            Metric::value(
                "peak_rss_mib",
                "isolated conversation case process",
                bytes_to_mib(snapshot.memory().peak_rss_bytes()),
                "MiB",
                "Process-only VmHWM across recent and complete archive discovery.",
            ),
        ],
        risks: vec![
            "External fixtures model validated metadata formats but not arbitrary provider version drift."
                .to_owned(),
            "Filesystem cache warmth materially affects archive inventory time.".to_owned(),
        ],
    })
}

#[expect(
    clippy::significant_drop_tightening,
    reason = "FilesProbe::shutdown consumes the probe and joins its workers before report assembly"
)]
fn concurrency_case(fixtures: &PerformanceFixtures) -> io::Result<CaseResult> {
    fixtures.reset_fake_git_log()?;
    let pid_log = fixtures.fake_git_bin().join("status-pids.log");
    let _ = fs::remove_file(&pid_log);
    let context = launch_context(fixtures.small_git())?;
    let mut probe = FilesProbe::new(&context, 80, 24)?;
    probe.start_background();
    for change in 0..32 {
        fs::write(
            fixtures.small_git().join("rapid-change.txt"),
            format!("synthetic change {change}\n"),
        )?;
        probe.request_refresh();
    }
    let background = probe.settle_background(Duration::from_secs(10))?;
    let cleanup = probe.shutdown()?;
    let events = fs::read_to_string(fixtures.fake_git_log())?;
    let starts = events
        .lines()
        .filter(|line| line.starts_with("start "))
        .count();
    let ends = events
        .lines()
        .filter(|line| line.starts_with("end "))
        .count();
    if starts == 0 || ends != starts {
        return Err(io::Error::other(format!(
            "status event log contains {starts} starts and {ends} completed commands"
        )));
    }
    let overlaps = events
        .lines()
        .filter(|line| line.starts_with("overlap "))
        .count();
    let max_concurrent = max_status_concurrency(&events)?.max(usize::from(overlaps > 0) * 2);
    let status_pids = fs::read_to_string(pid_log)?
        .lines()
        .map(|line| {
            line.parse::<u32>()
                .map_err(|_| io::Error::other("status PID log contains an invalid PID"))
        })
        .collect::<io::Result<BTreeSet<_>>>()?;
    if status_pids.len() != starts {
        return Err(io::Error::other(format!(
            "status PID log contains {} unique PIDs for {starts} commands",
            status_pids.len()
        )));
    }
    let surviving_status_children = status_pids
        .iter()
        .filter(|pid| Path::new("/proc").join(pid.to_string()).exists())
        .count();

    Ok(CaseResult {
        case: "concurrency".to_owned(),
        metrics: vec![
            Metric::value(
                "status_max_concurrent",
                "32 rapid refreshes in one Git workspace",
                max_concurrent as f64,
                "commands",
                format!("{starts} total git status commands; {overlaps} overlap observations."),
            ),
            Metric::value(
                "status_command_count",
                "32 rapid refreshes in one Git workspace",
                starts as f64,
                "commands",
                "Coalescing may schedule a final latest-generation command after the active one.",
            ),
            Metric::value(
                "status_surviving_children",
                "post-refresh cleanup",
                surviving_status_children as f64,
                "count",
                "Every PID observed by the synthetic git executable is checked in /proc.",
            ),
            Metric::value(
                "status_worker_completions",
                "coalesced refresh scheduler",
                background.completions() as f64,
                "count",
                format!(
                    "{} redraws while applying completed generations.",
                    background.redraws()
                ),
            ),
            cleanup_metric("surviving_workers", cleanup.surviving_worker_threads()),
            cleanup_metric("surviving_children", cleanup.surviving_children()),
        ],
        risks: vec![
            "The synthetic git executable isolates scheduler concurrency from real Git latency."
                .to_owned(),
        ],
    })
}

fn max_status_concurrency(events: &str) -> io::Result<usize> {
    let mut active = 0_usize;
    let mut maximum = 0_usize;
    for event in events.lines() {
        if event.starts_with("start ") {
            active = active.saturating_add(1);
            maximum = maximum.max(active);
        } else if event.starts_with("end ") {
            active = active
                .checked_sub(1)
                .ok_or_else(|| io::Error::other("status event ended before it started"))?;
        }
    }
    if active != 0 {
        return Err(io::Error::other(
            "status event log retained unfinished commands",
        ));
    }
    Ok(maximum)
}

fn rss_exclusion_case() -> io::Result<CaseResult> {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};

    let before = capture_process_snapshot()?;
    let executable = std::env::current_exe()?;
    let mut child = Command::new(executable)
        .arg("--memory-helper")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut ready = String::new();
    BufReader::new(
        child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("memory helper stdout unavailable"))?,
    )
    .read_line(&mut ready)?;
    if ready.trim() != "ready" {
        let _ = child.kill();
        let _ = child.wait();
        return Err(io::Error::other("memory helper did not become ready"));
    }
    let during = capture_process_snapshot()?;
    child.kill()?;
    child.wait()?;
    let after = capture_process_snapshot()?;
    let delta = during
        .memory()
        .rss_bytes()
        .saturating_sub(before.memory().rss_bytes());

    Ok(CaseResult {
        case: "rss-exclusion".to_owned(),
        metrics: vec![
            Metric::value(
                "rss_exclusion_parent_delta_mib",
                "64 MiB direct child allocation",
                bytes_to_mib(delta),
                "MiB",
                "Parent VmRSS delta while the child has faulted 64 MiB of private pages.",
            ),
            Metric::value(
                "rss_exclusion_surviving_children",
                "post-helper cleanup",
                after.child_pids().len() as f64,
                "count",
                "Helper is killed and waited before the cleanup snapshot.",
            ),
        ],
        risks: Vec::new(),
    })
}

fn launch_context(root: &Path) -> io::Result<LaunchContext> {
    let value = serde_json::json!({
        "workspace_id": "perf-workspace",
        "tab_id": "perf-tab",
        "pane_id": "perf-pane",
        "cwd": fs::canonicalize(root)?,
    })
    .to_string();
    LaunchContext::from_vars([("HERDR_PLUGIN_CONTEXT_JSON", value.as_str())])
        .map_err(io::Error::other)
}

fn load_ignore_heavy_tree(root: &Path) -> io::Result<FilesTree> {
    let mut tree = FilesTree::new(fs::canonicalize(root)?)?;
    let mut pending = VecDeque::from([PathBuf::new()]);
    let mut loaded_directories = 0_usize;
    while let Some(directory) = pending.pop_front() {
        tree.load_directory(&directory)?;
        loaded_directories += 1;
        if loaded_directories > 64 {
            return Err(io::Error::other("tree fixture exceeded directory bound"));
        }
        pending.extend(
            tree.children(&directory)
                .into_iter()
                .filter(|node| node.kind() == TreeNodeKind::Directory)
                .map(|node| node.path().to_path_buf()),
        );
    }
    Ok(tree)
}

fn validate_ignore_heavy_tree(tree: &FilesTree) -> io::Result<()> {
    let mut pending = VecDeque::from([PathBuf::new()]);
    let mut loaded_directories = 0_usize;
    let mut rust_files = 0_usize;
    let mut saw_gitignore = false;
    while let Some(directory) = pending.pop_front() {
        loaded_directories += 1;
        for node in tree.children(&directory) {
            match node.kind() {
                TreeNodeKind::Directory => pending.push_back(node.path().to_path_buf()),
                TreeNodeKind::File
                    if node.path().extension().is_some_and(|value| value == "rs") =>
                {
                    rust_files += 1;
                }
                TreeNodeKind::File if node.path() == Path::new(".gitignore") => {
                    saw_gitignore = true;
                }
                TreeNodeKind::File | TreeNodeKind::Symlink | TreeNodeKind::Virtual => {}
            }
            if node
                .path()
                .components()
                .any(|part| part.as_os_str() == "target")
            {
                return Err(io::Error::other(
                    "ignored target entry entered the Files tree",
                ));
            }
        }
    }
    if loaded_directories != 34 || rust_files != 1_024 || saw_gitignore {
        return Err(io::Error::other(format!(
            "ignore-heavy tree loaded {loaded_directories} directories, {rust_files} Rust files, hidden gitignore={saw_gitignore}"
        )));
    }
    Ok(())
}

fn virtual_status_fixture() -> io::Result<VcsStatusSnapshot> {
    let entries = (0..5_000)
        .map(|index| {
            VcsEntryStatus::new(
                PathBuf::from(format!("virtual/deleted-{index:05}.rs")),
                None,
                VcsStatusKind::Deleted,
                Some(VcsStatusKind::Deleted),
                None,
            )
            .map_err(io::Error::other)
        })
        .collect::<io::Result<Vec<_>>>()?;
    Ok(VcsStatusSnapshot::new(entries, false))
}

fn measure_virtual_status_merge(
    root: &Path,
    snapshot: &VcsStatusSnapshot,
    warmup: usize,
    measured: usize,
) -> io::Result<DurationSummary> {
    let mut samples = Vec::with_capacity(measured);
    for index in 0..warmup.saturating_add(measured) {
        let mut tree = FilesTree::new(fs::canonicalize(root)?)?;
        tree.load_directory(Path::new(""))?;
        let started = Instant::now();
        tree.merge_status(snapshot)?;
        let elapsed = started.elapsed();
        validate_virtual_status_tree(&tree)?;
        if index >= warmup {
            samples.push(elapsed);
        }
    }
    summarize_durations(&samples)
        .ok_or_else(|| io::Error::other("virtual status samples disappeared"))
}

fn validate_virtual_status_tree(tree: &FilesTree) -> io::Result<()> {
    let parent = tree
        .node(Path::new("virtual"))
        .ok_or_else(|| io::Error::other("virtual status parent is missing"))?;
    let children = tree.children(Path::new("virtual"));
    if parent.kind() != TreeNodeKind::Directory
        || children.len() != 5_000
        || children.iter().any(|node| {
            node.kind() != TreeNodeKind::Virtual || node.status() != Some(VcsStatusKind::Deleted)
        })
    {
        return Err(io::Error::other(format!(
            "virtual status merge exposed {} of 5,000 deleted children",
            children.len()
        )));
    }
    Ok(())
}

fn measure_git_status(root: &Path) -> io::Result<VcsStatusSnapshot> {
    let mut service = GitService::new(Duration::from_secs(5));
    let workspace = service
        .detect(root)
        .map_err(io::Error::other)?
        .ok_or_else(|| io::Error::other("Git fixture was not detected"))?;
    service.refresh_status(&workspace).map_err(io::Error::other)
}

fn validate_git_status(snapshot: &VcsStatusSnapshot) -> io::Result<()> {
    if snapshot.entries().len() != 128
        || snapshot
            .entries()
            .iter()
            .any(|entry| entry.kind() != VcsStatusKind::Untracked)
    {
        return Err(io::Error::other(format!(
            "Git fixture returned {} entries instead of 128 untracked files",
            snapshot.entries().len()
        )));
    }
    Ok(())
}

fn measure_jj_status(root: &Path, mode: JujutsuMode) -> io::Result<VcsStatusSnapshot> {
    let mut service = JjService::new(mode, Duration::from_secs(5));
    let workspace = service
        .detect(root)
        .map_err(io::Error::other)?
        .ok_or_else(|| io::Error::other("Jujutsu fixture was not detected"))?;
    service.refresh_status(&workspace).map_err(io::Error::other)
}

fn validate_jj_status(snapshot: &VcsStatusSnapshot, mode: JujutsuMode) -> io::Result<()> {
    if snapshot.entries().len() != 64
        || snapshot
            .entries()
            .iter()
            .any(|entry| entry.kind() != VcsStatusKind::Added)
    {
        return Err(io::Error::other(format!(
            "Jujutsu {mode:?} fixture returned {} entries instead of 64 added files",
            snapshot.entries().len()
        )));
    }
    Ok(())
}

fn sample_validated<T>(
    warmup: usize,
    measured: usize,
    mut operation: impl FnMut() -> io::Result<T>,
    mut validate: impl FnMut(&T) -> io::Result<()>,
) -> io::Result<DurationSummary> {
    let mut samples = Vec::with_capacity(measured);
    for index in 0..warmup.saturating_add(measured) {
        let started = Instant::now();
        let value = operation()?;
        let elapsed = started.elapsed();
        validate(&value)?;
        if index >= warmup {
            samples.push(elapsed);
        }
    }
    summarize_durations(&samples).ok_or_else(|| io::Error::other("validated samples disappeared"))
}

fn refresh_all(index: &mut ConversationIndex, registry: &SourceRegistry) -> io::Result<()> {
    for page in 0..64 {
        let result = index
            .refresh_page(registry, discovery_limit(64)?, metadata_budget()?)
            .map_err(io::Error::other)?;
        if !result.has_more() {
            return Ok(());
        }
        if page == 63 {
            return Err(io::Error::other("conversation index exceeded 64 pages"));
        }
    }
    Ok(())
}

fn project(root: &Path) -> io::Result<ProjectIdentity> {
    ProjectIdentity::from_canonical_root(fs::canonicalize(root)?).map_err(io::Error::other)
}

fn discovery_limit(value: usize) -> io::Result<DiscoveryLimit> {
    DiscoveryLimit::new(value)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid discovery limit"))
}

fn metadata_budget() -> io::Result<MetadataBudget> {
    MetadataBudget::new(512 * 1024)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid metadata budget"))
}

fn duration_value(
    name: &str,
    workload: &str,
    duration: Duration,
    note: impl Into<String>,
) -> Metric {
    Metric::value(name, workload, duration.as_secs_f64() * 1_000.0, "ms", note)
}

fn cleanup_metric(name: &str, count: usize) -> Metric {
    Metric::value(
        name,
        "post-workload cleanup",
        count as f64,
        "count",
        "Observed after explicit worker shutdown and subprocess wait.",
    )
}

fn bytes_to_mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn reset_directory(path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    fs::create_dir_all(path)
}
