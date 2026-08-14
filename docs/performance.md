# V1 performance measurement

HDC-15 is a measurement and diagnosis suite. It does not optimize runtime behavior. A failed budget must retain its evidence here and receive a separate corrective issue.

## Reproduce

Run the full optimized suite from the repository root:

```sh
scripts/measure-performance.sh docs
```

The command builds the custom Cargo benchmark with the `perf-harness` feature and writes:

- `docs/performance-baseline.json`: machine-readable schema, environment, datasets, metrics, verdicts, and failure attribution;
- `docs/performance-baseline.md`: human-readable rendering of the same report.

For a short harness smoke test without replacing the retained baseline:

```sh
scripts/measure-performance.sh target/performance-smoke --warmup 1 --samples 5 --idle-ms 250
```

The benchmark-only module is excluded from default builds. Normal plugin startup, adapters, caches, schedulers, and rendering contain no HDC-15 instrumentation.

## Budgets

Every budget uses an inclusive upper bound and emits an explicit `PASS` or `FAIL`:

| Metric | Budget |
|---|---:|
| First frame p95 | `<= 100 ms` |
| Navigation p95 | `<= 50 ms` |
| Settled idle CPU | `<= 1%` |
| Peak process RSS | `<= 50 MiB` |
| Steady process RSS | `<= 50 MiB` |
| Concurrent status commands per workspace | `<= 1` |
| Surviving workers and children | `0` |

The RSS-exclusion control faults 64 MiB in a direct child and requires the parent RSS delta to remain at most 8 MiB. This validates that `/proc/self/status` accounts only for the measured process. The 8 MiB bound is a harness-control threshold, not a product memory budget.

## Measurement boundaries

- **Profile:** Cargo bench profile, optimized release code, locked dependencies.
- **Warmup and sampling:** requested defaults are 20 warmups and 200 measured samples. Every metric records its effective counts: UI timings use the defaults (navigation enforces at least 100 measured), tree/merge work caps at 2/10, VCS uses 1/5, and destructive or aggregate observations use 0/1. Sampled durations use nearest-rank p50 and p95.
- **First frame:** from a ready `LaunchContext` through construction and a fresh 80x24 Ratatui `TestBackend` draw. `Controller::start` is never called, so background completion cannot contaminate the result.
- **Navigation:** a changed next/previous selection plus the resulting 80x24 Files render. Every sample changes state.
- **Idle:** settled no-VCS Files state for five seconds, with a conservative 50 ms scheduler-tick simulation. CPU is process user+system ticks divided by wall time; redraws are counted separately.
- **RSS:** `VmRSS` and `VmHWM` from `/proc/self/status`. Each workload class runs in a fresh child process; fixture generation occurs in the supervisor.
- **Status concurrency:** 32 rapid refreshes use a deterministic 50 ms synthetic Git command. The wrapper records only `git status`, detects overlap atomically, and records every PID. Multiple sequential latest-generation commands are allowed; more than one concurrent command for the workspace fails.
- **Recent conversations:** time through the first bounded metadata page is recorded independently from complete archive discovery.
- **Cleanup:** status PIDs are checked in `/proc`; `WorkerRuntime::shutdown` is followed by a process task/child snapshot.

## Synthetic datasets

The generator owns and replaces only its fixed temporary directory. It produces no real prompts, responses, repository content, telemetry, or network activity.

- no-VCS workspace with 64 files;
- small Git workspace with 128 files;
- Git-backed non-colocated Jujutsu and colocated Jujutsu workspaces with 64 files each;
- ignore-heavy Git monorepo with 1,024 visible and 4,096 ignored files;
- 64 project-local generic JSONL histories (the adapter's bounded shallow-directory maximum);
- 2,048 external Codex sessions plus fixture-validated Claude Code, Pi, and OMP sessions;
- one incrementally appended transcript;
- 5,000 synthetic status entries for tree merge;
- 32 rapid workspace mutations and refreshes.

Fixture counts and the complete generated root size are validated before any case starts; the root must remain at most 32 MiB, including Git/Jujutsu metadata and tool output. Adapter inputs reuse the repository's sanitized, version-bounded test shapes with deterministic IDs, timestamps, and message placeholders.

## Noise controls and interpretation

Run on an otherwise idle, plugged-in machine. The report records OS, kernel, CPU, logical CPU count, governor, load average, Rust/Cargo, Git, and Jujutsu versions. Cases run sequentially with `LC_ALL=C`, no color, no network, and isolated state/home directories.

The suite deliberately retains these uncertainties:

- Ratatui `TestBackend` excludes terminal-driver and multiplexer transport latency;
- warm page cache, CPU frequency scaling, allocator behavior, and unrelated host load vary;
- synthetic repositories do not represent slow disks, network filesystems, or extreme Git indexes;
- fixture-validated metadata does not predict future provider format changes;
- a single machine is a baseline, not a population distribution.

A failed report includes reproduction, subsystem attribution, impact, uncertainty, and risk. Create one Linear follow-up per failed budget that requires code changes; do not modify runtime behavior in HDC-15.
