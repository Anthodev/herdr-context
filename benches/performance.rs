#[path = "../tests/support/perf_fixtures.rs"]
mod perf_fixtures;
mod workloads;

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use herdr_context::perf::BudgetVerdict;
use perf_fixtures::{FixtureManifest, PerformanceFixtures};
use serde::Serialize;
use workloads::{CaseResult, Metric, RunConfig};

const CASES: [&str; 5] = [
    "ui",
    "filesystem",
    "conversations",
    "concurrency",
    "rss-exclusion",
];

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("performance harness: {error}");
            ExitCode::from(2)
        }
    }
}

fn run() -> io::Result<()> {
    let mut raw_arguments = env::args().skip(1).peekable();
    if raw_arguments.peek().is_none() {
        return Ok(());
    }
    let arguments = Arguments::parse(raw_arguments)?;
    if arguments.memory_helper {
        workloads::memory_helper();
    }
    if let Some(case) = arguments.case.as_deref() {
        return run_child(case, &arguments);
    }
    run_supervisor(&arguments)
}

fn run_child(case: &str, arguments: &Arguments) -> io::Result<()> {
    let parent = arguments
        .fixtures_parent
        .as_deref()
        .ok_or_else(|| io::Error::other("child case is missing --fixtures-parent"))?;
    let fixtures = PerformanceFixtures::from_existing(parent)?;
    let result = workloads::run_case(
        case,
        &fixtures,
        RunConfig {
            warmup_samples: arguments.warmup_samples,
            measured_samples: arguments.measured_samples,
            idle_duration: arguments.idle_duration,
        },
    )?;
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}

fn run_supervisor(arguments: &Arguments) -> io::Result<()> {
    let fixture_parent = tempfile::tempdir()?;
    let fixtures = PerformanceFixtures::create(fixture_parent.path())?;
    let manifest = fixtures.validate()?;
    let executable = env::current_exe()?;
    let mut cases = Vec::with_capacity(CASES.len());
    for case in CASES {
        let mut command = Command::new(&executable);
        command
            .arg("--case")
            .arg(case)
            .arg("--fixtures-parent")
            .arg(fixture_parent.path())
            .arg("--warmup")
            .arg(arguments.warmup_samples.to_string())
            .arg("--samples")
            .arg(arguments.measured_samples.to_string())
            .arg("--idle-ms")
            .arg(arguments.idle_duration.as_millis().to_string())
            .env("HOME", fixtures.home())
            .env("HERDR_PLUGIN_STATE_DIR", fixtures.state())
            .env(
                "HERDR_PLUGIN_CONFIG_DIR",
                fixtures.root().join("empty-config"),
            )
            .env("LC_ALL", "C")
            .env("NO_COLOR", "1");
        if case == "concurrency" {
            let path = env::var_os("PATH").unwrap_or_default();
            let paths = std::iter::once(fixtures.fake_git_bin().to_path_buf())
                .chain(env::split_paths(&path));
            command.env("PATH", env::join_paths(paths).map_err(io::Error::other)?);
        }
        let output = command.output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "case {case} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let result = serde_json::from_slice::<CaseResult>(&output.stdout).map_err(|error| {
            io::Error::other(format!(
                "case {case} emitted invalid JSON ({error}): {}",
                String::from_utf8_lossy(&output.stdout).trim()
            ))
        })?;
        cases.push(result);
    }

    let report = Report::new(arguments, manifest, cases)?;
    fs::create_dir_all(&arguments.output_dir)?;
    let json_path = arguments.output_dir.join("performance-baseline.json");
    let markdown_path = arguments.output_dir.join("performance-baseline.md");
    fs::write(&json_path, serde_json::to_vec_pretty(&report)?)?;
    fs::write(&markdown_path, report.markdown())?;
    println!("JSON: {}", json_path.display());
    println!("Markdown: {}", markdown_path.display());
    println!(
        "Verdict: {}",
        if report.overall_pass { "PASS" } else { "FAIL" }
    );
    Ok(())
}

#[derive(Debug)]
struct Arguments {
    output_dir: PathBuf,
    fixtures_parent: Option<PathBuf>,
    case: Option<String>,
    warmup_samples: usize,
    measured_samples: usize,
    idle_duration: Duration,
    memory_helper: bool,
}

impl Arguments {
    fn parse(arguments: impl Iterator<Item = String>) -> io::Result<Self> {
        let mut output = Self {
            output_dir: PathBuf::from("target/performance"),
            fixtures_parent: None,
            case: None,
            warmup_samples: 20,
            measured_samples: 200,
            idle_duration: Duration::from_secs(5),
            memory_helper: false,
        };
        let mut arguments = arguments.peekable();
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--output-dir" => output.output_dir = next_path(&mut arguments, &argument)?,
                "--fixtures-parent" => {
                    output.fixtures_parent = Some(next_path(&mut arguments, &argument)?)
                }
                "--case" => output.case = Some(next_value(&mut arguments, &argument)?),
                "--warmup" => {
                    output.warmup_samples = parse_usize(
                        &next_value(&mut arguments, &argument)?,
                        "warmup samples",
                        true,
                    )?
                }
                "--samples" => {
                    output.measured_samples = parse_usize(
                        &next_value(&mut arguments, &argument)?,
                        "measured samples",
                        false,
                    )?
                }
                "--idle-ms" => {
                    let millis = parse_usize(
                        &next_value(&mut arguments, &argument)?,
                        "idle milliseconds",
                        false,
                    )?;
                    output.idle_duration =
                        Duration::from_millis(u64::try_from(millis).map_err(io::Error::other)?);
                }
                "--memory-helper" => output.memory_helper = true,
                "--bench" => {}
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("unknown performance argument {argument:?}"),
                    ));
                }
            }
        }
        Ok(output)
    }
}

fn next_path(arguments: &mut impl Iterator<Item = String>, option: &str) -> io::Result<PathBuf> {
    next_value(arguments, option).map(PathBuf::from)
}

fn next_value(arguments: &mut impl Iterator<Item = String>, option: &str) -> io::Result<String> {
    arguments.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{option} requires a value"),
        )
    })
}

fn parse_usize(value: &str, field: &str, allow_zero: bool) -> io::Result<usize> {
    let value = value.parse::<usize>().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{field} must be an integer"),
        )
    })?;
    if !allow_zero && value == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{field} must be non-zero"),
        ));
    }
    Ok(value)
}

#[derive(Debug, Serialize)]
struct Report {
    schema_version: u32,
    generated_at_unix_seconds: u64,
    ticket: &'static str,
    profile: &'static str,
    machine: Machine,
    method: Method,
    datasets: Datasets,
    cases: Vec<CaseResult>,
    verdicts: Vec<BudgetVerdict>,
    failures: Vec<FailureAttribution>,
    overall_pass: bool,
    residual_risks: Vec<String>,
}

impl Report {
    fn new(
        arguments: &Arguments,
        manifest: FixtureManifest,
        cases: Vec<CaseResult>,
    ) -> io::Result<Self> {
        let metrics = cases
            .iter()
            .flat_map(|case| case.metrics.iter())
            .collect::<Vec<_>>();
        let verdicts = budgets(&metrics)?;
        let failures = verdicts
            .iter()
            .filter(|verdict| !verdict.passed())
            .map(failure_attribution)
            .collect::<Vec<_>>();
        let overall_pass = failures.is_empty();
        let residual_risks = cases
            .iter()
            .flat_map(|case| case.risks.iter().cloned())
            .chain([
                "Single-machine synthetic baselines do not characterize all user hardware.".to_owned(),
                "OS page cache, CPU frequency scaling, and unrelated host load remain measurable noise."
                    .to_owned(),
                "No real transcript content is used; only fixture-validated synthetic metadata shapes are covered."
                    .to_owned(),
            ])
            .collect();
        Ok(Self {
            schema_version: 1,
            generated_at_unix_seconds: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(io::Error::other)?
                .as_secs(),
            ticket: "HDC-15",
            profile: "Cargo bench profile (optimized release code)",
            machine: Machine::capture(),
            method: Method {
                warmup_samples: arguments.warmup_samples,
                measured_samples: arguments.measured_samples,
                idle_duration_ms: u64::try_from(arguments.idle_duration.as_millis())
                    .unwrap_or(u64::MAX),
                percentile: "nearest-rank p50/p95 over wall-clock samples",
                first_frame_boundary: "LaunchContext ready through fresh TestBackend draw; background start excluded",
                rss_boundary: "Linux /proc/self/status VmRSS/VmHWM for each isolated case process",
                sample_policy: "Each metric records its effective warmup/measured count; UI uses requested defaults (navigation enforces at least 100 measured), tree/merge cap at 2/10, VCS uses 1/5, and destructive or aggregate observations use 0/1.",
                noise_controls: vec![
                    "Optimized bench profile; harness overhead excluded from child case metrics.",
                    "Each workload class runs in a fresh process; fixture generation runs in the supervisor.",
                    "Cases run sequentially with LC_ALL=C and no network or real user data.",
                    "Warmups precede sampled timings; status, archive, concurrency, and cleanup work is fully awaited.",
                    "Run on an otherwise idle host; record governor and load average with the result.",
                ],
                reproduction_command: "scripts/measure-performance.sh docs",
            },
            datasets: Datasets::from(manifest),
            cases,
            verdicts,
            failures,
            overall_pass,
            residual_risks,
        })
    }

    fn markdown(&self) -> String {
        let mut output = String::new();
        output.push_str("# HDC-15 V1 performance baseline\n\n");
        output.push_str(&format!(
            "Overall verdict: **{}**  \nGenerated at Unix time `{}` with `{}`.\n\n",
            if self.overall_pass { "PASS" } else { "FAIL" },
            self.generated_at_unix_seconds,
            self.profile
        ));
        output.push_str("## Reproduction\n\n");
        output.push_str(&format!(
            "```sh\n{}\n```\n\n",
            self.method.reproduction_command
        ));
        output.push_str("Noise controls: \n");
        for control in &self.method.noise_controls {
            output.push_str(&format!("- {control}\n"));
        }
        output.push_str("\n## Machine and method\n\n");
        output.push_str(&format!(
            "- OS/kernel: `{}` / `{}`\n- CPU: `{}` ({} logical CPUs)\n- CPU governor: `{}`\n- Load average: `{}`\n- Toolchain: `{}`; `{}`\n- VCS tools: `{}`; `{}`\n- Requested sampling defaults: {} warmup, {} measured; idle {} ms\n- Effective sampling: {}\n- First frame: {}\n- RSS: {}\n\n",
            self.machine.os,
            self.machine.kernel,
            self.machine.cpu,
            self.machine.logical_cpus,
            self.machine.cpu_governor,
            self.machine.load_average,
            self.machine.rustc,
            self.machine.cargo,
            self.machine.git,
            self.machine.jj,
            self.method.warmup_samples,
            self.method.measured_samples,
            self.method.idle_duration_ms,
            self.method.sample_policy,
            self.method.first_frame_boundary,
            self.method.rss_boundary,
        ));
        output.push_str("## Synthetic datasets\n\n");
        output.push_str(&format!(
            "- no-VCS: {} files\n- small Git: {} files\n- Jujutsu: {} files each, native/non-colocated and colocated\n- ignore-heavy monorepo: {} visible + {} ignored files\n- project-local histories: {} sessions\n- external histories: {} Codex sessions plus Claude Code, Pi, and OMP fixtures\n- payload: {:.2} MiB; content is deterministic and synthetic\n\n",
            self.datasets.no_vcs_files,
            self.datasets.small_git_files,
            self.datasets.jj_files_each,
            self.datasets.monorepo_visible_files,
            self.datasets.monorepo_ignored_files,
            self.datasets.local_sessions,
            self.datasets.external_sessions,
            self.datasets.total_payload_mib,
        ));
        output.push_str("## Budget verdicts\n\n");
        output.push_str("| Budget | Observed | Limit | Verdict |\n|---|---:|---:|:---:|\n");
        for verdict in &self.verdicts {
            let value = serde_json::to_value(verdict).expect("verdict serialization");
            output.push_str(&format!(
                "| {} | {:.3} {} | {} {:.3} {} | **{}** |\n",
                value["metric"].as_str().unwrap_or("unknown"),
                value["observed"].as_f64().unwrap_or(f64::NAN),
                value["unit"].as_str().unwrap_or(""),
                value["comparator"].as_str().unwrap_or(""),
                value["limit"].as_f64().unwrap_or(f64::NAN),
                value["unit"].as_str().unwrap_or(""),
                if verdict.passed() { "PASS" } else { "FAIL" },
            ));
        }
        output.push_str("\n## Measurements\n\n");
        output.push_str("| Case | Metric | Workload | Warmup | Measured | p50 | p95 / value | Notes |\n|---|---|---|---:|---:|---:|---:|---|\n");
        for case in &self.cases {
            for metric in &case.metrics {
                output.push_str(&format!(
                    "| {} | {} | {} | {} | {} | {} | {:.3} {} | {} |\n",
                    escape(&case.case),
                    escape(&metric.name),
                    escape(&metric.workload),
                    metric.warmup_samples,
                    metric.samples,
                    metric
                        .p50
                        .map(|value| format!("{value:.3} {}", metric.unit))
                        .unwrap_or_else(|| "—".to_owned()),
                    metric.observed,
                    escape(&metric.unit),
                    escape(&metric.note),
                ));
            }
        }
        output.push_str("\n## Failed-budget attribution\n\n");
        if self.failures.is_empty() {
            output.push_str(
                "No measured budget failed; no corrective follow-up issue is required.\n",
            );
        } else {
            for failure in &self.failures {
                output.push_str(&format!(
                    "### {}\n\n- Reproduction: `{}`\n- Attribution: {}\n- Impact: {}\n- Uncertainty: {}\n- Risk: {}\n\n",
                    failure.metric,
                    failure.reproduction,
                    failure.attribution,
                    failure.impact,
                    failure.uncertainty,
                    failure.risk,
                ));
            }
        }
        output.push_str("\n## Residual risks\n\n");
        for risk in &self.residual_risks {
            output.push_str(&format!("- {risk}\n"));
        }
        output
    }
}

#[derive(Debug, Serialize)]
struct Machine {
    os: String,
    kernel: String,
    cpu: String,
    logical_cpus: usize,
    cpu_governor: String,
    load_average: String,
    rustc: String,
    cargo: String,
    git: String,
    jj: String,
}

impl Machine {
    fn capture() -> Self {
        Self {
            os: os_pretty_name(),
            kernel: command_text("uname", &["-srmo"]),
            cpu: cpu_model(),
            logical_cpus: std::thread::available_parallelism().map_or(0, usize::from),
            cpu_governor: read_trimmed("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor"),
            load_average: read_trimmed("/proc/loadavg"),
            rustc: recorded_tool_version("HDC15_RUSTC_VERSION", "rustc"),
            cargo: recorded_tool_version("HDC15_CARGO_VERSION", "cargo"),
            git: command_text("git", &["--version"]),
            jj: command_text("jj", &["--version"]),
        }
    }
}

#[derive(Debug, Serialize)]
struct Method {
    warmup_samples: usize,
    measured_samples: usize,
    idle_duration_ms: u64,
    percentile: &'static str,
    first_frame_boundary: &'static str,
    sample_policy: &'static str,
    rss_boundary: &'static str,
    noise_controls: Vec<&'static str>,
    reproduction_command: &'static str,
}

#[derive(Debug, Serialize)]
struct Datasets {
    no_vcs_files: usize,
    small_git_files: usize,
    jj_files_each: usize,
    monorepo_visible_files: usize,
    monorepo_ignored_files: usize,
    local_sessions: usize,
    external_sessions: usize,
    total_payload_mib: f64,
}

impl From<FixtureManifest> for Datasets {
    fn from(manifest: FixtureManifest) -> Self {
        Self {
            no_vcs_files: 64,
            small_git_files: 128,
            jj_files_each: 64,
            monorepo_visible_files: manifest.monorepo_visible_files(),
            monorepo_ignored_files: manifest.monorepo_ignored_files(),
            local_sessions: manifest.local_sessions(),
            external_sessions: manifest.external_sessions(),
            total_payload_mib: manifest.total_payload_bytes() as f64 / (1024.0 * 1024.0),
        }
    }
}

#[derive(Debug, Serialize)]
struct FailureAttribution {
    metric: String,
    reproduction: &'static str,
    attribution: &'static str,
    impact: &'static str,
    uncertainty: &'static str,
    risk: &'static str,
}

fn budgets(metrics: &[&Metric]) -> io::Result<Vec<BudgetVerdict>> {
    const BUDGETS: [(&str, f64, &str); 12] = [
        ("first_frame_p95_ms", 100.0, "ms"),
        ("navigation_p95_ms", 50.0, "ms"),
        ("idle_cpu_percent", 1.0, "%"),
        ("peak_rss_mib", 50.0, "MiB"),
        ("steady_rss_mib", 50.0, "MiB"),
        ("status_max_concurrent", 1.0, "commands"),
        ("status_surviving_children", 0.0, "count"),
        ("surviving_workers", 0.0, "count"),
        ("surviving_children", 0.0, "count"),
        ("filesystem_surviving_children", 0.0, "count"),
        ("rss_exclusion_parent_delta_mib", 8.0, "MiB"),
        ("rss_exclusion_surviving_children", 0.0, "count"),
    ];
    BUDGETS
        .into_iter()
        .map(|(name, limit, unit)| {
            let matching = metrics
                .iter()
                .filter(|metric| metric.name == name)
                .collect::<Vec<_>>();
            if matching.is_empty() {
                return Err(io::Error::other(format!(
                    "required budget metric {name} was not emitted"
                )));
            }
            let observed = matching
                .into_iter()
                .map(|metric| metric.observed)
                .fold(f64::NEG_INFINITY, f64::max);
            Ok(BudgetVerdict::upper_bound(name, observed, limit, unit))
        })
        .collect()
}

fn failure_attribution(verdict: &BudgetVerdict) -> FailureAttribution {
    let metric = verdict.metric();
    let (attribution, impact, uncertainty, risk) = if metric.contains("first_frame") {
        (
            "Synchronous shell construction or initial render exceeds the V1 frame budget.",
            "The dock appears late even though background discovery is excluded.",
            "TestBackend omits terminal and multiplexer transport latency.",
            "Real first-frame latency may be higher on slower terminals.",
        )
    } else if metric.contains("navigation") {
        (
            "Selection, viewport reconciliation, or Files rendering exceeds the interaction budget.",
            "Keyboard navigation can feel visibly sluggish.",
            "The synthetic visible tree is smaller than extreme user workspaces.",
            "Larger viewports or terminals may amplify render cost.",
        )
    } else if metric.contains("idle_cpu") {
        (
            "The settled scheduler consumes process CPU while no user-visible state changes.",
            "Persistent dock instances consume host capacity at rest.",
            "CLK_TCK resolution makes very low CPU percentages coarse.",
            "Longer sampling may reveal periodic refresh spikes.",
        )
    } else if metric == "rss_exclusion_parent_delta_mib" {
        (
            "The process-only RSS control observed helper memory in the parent measurement.",
            "Reported dock memory would include child allocations and invalidate the budget.",
            "VmRSS sampling is page-granular and allocator activity can add small parent-side noise.",
            "Without a passing control, memory regressions can be attributed to the wrong process.",
        )
    } else if metric == "rss_exclusion_surviving_children" {
        (
            "The RSS-accounting helper remained observable after benchmark cleanup.",
            "Measurement controls can leak a child process after the suite exits.",
            "The /proc observation is a point-in-time post-wait snapshot.",
            "Leaked helpers consume memory and invalidate subsequent host measurements.",
        )
    } else if metric.contains("rss") {
        (
            "Process-resident allocations exceed the V1 memory or RSS-accounting bound.",
            "Each dock consumes more resident memory than budgeted.",
            "Allocator and page-cache behavior vary across libc and kernels.",
            "Multiple simultaneous docks multiply the resident footprint.",
        )
    } else if metric.contains("status_max_concurrent") {
        (
            "Rapid refresh generations overlap status subprocesses for one workspace.",
            "Duplicate VCS work increases CPU and process pressure during file churn.",
            "The synthetic command duration is fixed at 50 ms.",
            "Long real status commands would increase overlap impact.",
        )
    } else {
        (
            "Explicit shutdown left a benchmark worker or child process observable.",
            "Closing the dock can leak resources or continue background work.",
            "The /proc observation is a point-in-time post-wait snapshot.",
            "Leaked processes may mutate state or accumulate across launches.",
        )
    };
    FailureAttribution {
        metric: metric.to_owned(),
        reproduction: "scripts/measure-performance.sh docs",
        attribution,
        impact,
        uncertainty,
        risk,
    }
}

fn recorded_tool_version(variable: &str, fallback: &str) -> String {
    env::var(variable)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty() && !value.contains('\r') && !value.contains('\n'))
        .unwrap_or_else(|| command_text(fallback, &["--version"]))
}

fn command_text(program: &str, arguments: &[&str]) -> String {
    Command::new(program)
        .args(arguments)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unavailable".to_owned())
}

fn read_trimmed(path: impl AsRef<Path>) -> String {
    fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unavailable".to_owned())
}

fn os_pretty_name() -> String {
    fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|contents| {
            contents
                .lines()
                .find_map(|line| line.strip_prefix("PRETTY_NAME="))
                .map(|value| value.trim_matches('"').to_owned())
        })
        .unwrap_or_else(|| "unavailable".to_owned())
}

fn cpu_model() -> String {
    fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|contents| {
            contents.lines().find_map(|line| {
                line.split_once(':')
                    .filter(|(key, _)| key.trim() == "model name")
                    .map(|(_, value)| value.trim().to_owned())
            })
        })
        .unwrap_or_else(|| "unavailable".to_owned())
}

fn escape(value: &str) -> String {
    value.replace('|', "\\|").replace('\n', " ")
}
