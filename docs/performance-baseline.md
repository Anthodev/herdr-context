# HDC-15 V1 performance baseline

Overall verdict: **PASS**  
Generated at Unix time `1786720128` with `Cargo bench profile (optimized release code)`.

## Reproduction

```sh
scripts/measure-performance.sh docs
```

Noise controls: 
- Optimized bench profile; harness overhead excluded from child case metrics.
- Each workload class runs in a fresh process; fixture generation runs in the supervisor.
- Cases run sequentially with LC_ALL=C and no network or real user data.
- Warmups precede sampled timings; status, archive, concurrency, and cleanup work is fully awaited.
- Run on an otherwise idle host; record governor and load average with the result.

## Machine and method

- OS/kernel: `Fedora Linux 44 (KDE Plasma Desktop Edition)` / `Linux 7.1.7-cachyos1.fc44.x86_64 x86_64 GNU/Linux`
- CPU: `AMD Ryzen 7 7800X3D 8-Core Processor` (8 logical CPUs)
- CPU governor: `powersave`
- Load average: `2.20 1.79 1.43 2/2583 1219846`
- Toolchain: `rustc 1.97.1 (8bab26f4f 2026-07-14)`; `cargo 1.97.1 (c980f4866 2026-06-30)`
- VCS tools: `git version 2.55.0`; `jj 0.44.0`
- Requested sampling defaults: 20 warmup, 200 measured; idle 5000 ms
- Effective sampling: Each metric records its effective warmup/measured count; UI uses requested defaults (navigation enforces at least 100 measured), tree/merge cap at 2/10, VCS uses 1/5, and destructive or aggregate observations use 0/1.
- First frame: LaunchContext ready through fresh TestBackend draw; background start excluded
- RSS: Linux /proc/self/status VmRSS/VmHWM for each isolated case process

## Synthetic datasets

- no-VCS: 64 files
- small Git: 128 files
- Jujutsu: 64 files each, native/non-colocated and colocated
- ignore-heavy monorepo: 1024 visible + 4096 ignored files
- project-local histories: 64 sessions
- external histories: 2048 Codex sessions plus Claude Code, Pi, and OMP fixtures
- payload: 2.44 MiB; content is deterministic and synthetic

## Budget verdicts

| Budget | Observed | Limit | Verdict |
|---|---:|---:|:---:|
| first_frame_p95_ms | 0.093 ms | <= 100.000 ms | **PASS** |
| navigation_p95_ms | 0.016 ms | <= 50.000 ms | **PASS** |
| idle_cpu_percent | 0.000 % | <= 1.000 % | **PASS** |
| peak_rss_mib | 27.430 MiB | <= 50.000 MiB | **PASS** |
| steady_rss_mib | 21.160 MiB | <= 50.000 MiB | **PASS** |
| status_max_concurrent | 1.000 commands | <= 1.000 commands | **PASS** |
| status_surviving_children | 0.000 count | <= 0.000 count | **PASS** |
| surviving_workers | 0.000 count | <= 0.000 count | **PASS** |
| surviving_children | 0.000 count | <= 0.000 count | **PASS** |
| filesystem_surviving_children | 0.000 count | <= 0.000 count | **PASS** |
| rss_exclusion_parent_delta_mib | 0.336 MiB | <= 8.000 MiB | **PASS** |
| rss_exclusion_surviving_children | 0.000 count | <= 0.000 count | **PASS** |

## Measurements

| Case | Metric | Workload | Warmup | Measured | p50 | p95 / value | Notes |
|---|---|---|---:|---:|---:|---:|---|
| ui | first_frame_p95_ms | no-vcs first shell frame | 20 | 200 | 0.072 ms | 0.093 ms | Includes App construction and a fresh 80x24 TestBackend draw; no background start. |
| ui | navigation_p95_ms | selection change plus 80x24 Files render | 20 | 200 | 0.015 ms | 0.016 ms | Alternates next/previous so every measured sample changes visible selection. |
| ui | idle_cpu_percent | settled no-vcs Files view | 0 | 1 | — | 0.000 % | Process CPU ticks over wall time; 50 ms scheduler tick simulation. |
| ui | idle_redraws | settled no-vcs Files view | 0 | 1 | — | 0.000 count | Dirty-driven redraw count during the idle interval. |
| ui | steady_rss_mib | settled UI process | 0 | 1 | — | 5.383 MiB | VmRSS from /proc/self/status; child memory is excluded by kernel accounting. |
| ui | peak_rss_mib | isolated UI case process | 0 | 1 | — | 5.383 MiB | VmHWM from /proc/self/status in a fresh case process. |
| ui | surviving_workers | post-workload cleanup | 0 | 1 | — | 0.000 count | Observed after explicit worker shutdown and subprocess wait. |
| ui | surviving_children | post-workload cleanup | 0 | 1 | — | 0.000 count | Observed after explicit worker shutdown and subprocess wait. |
| filesystem | ignore_heavy_tree_p95_ms | 1,024 visible and 4,096 ignored monorepo files | 2 | 10 | 16.403 ms | 16.629 ms | Loads every visible directory; ignored target trees must never enter the model. |
| filesystem | status_merge_p95_ms | 5,000 virtual Git status entries | 2 | 10 | 41.343 ms | 41.664 ms | Measures FilesTree merge and display-parent construction. |
| filesystem | small_git_status_p95_ms | small Git workspace | 1 | 5 | 20.393 ms | 20.404 ms | Includes hardened config inspection and porcelain-v2 status child processes. |
| filesystem | native_jj_status_p95_ms | non-colocated Jujutsu workspace | 1 | 5 | 20.379 ms | 20.425 ms | Fresh mode snapshots and parses the working-copy diff. |
| filesystem | colocated_jj_status_p95_ms | colocated Jujutsu workspace | 1 | 5 | 20.389 ms | 20.420 ms | Passive mode avoids mutation and reports stale status. |
| filesystem | steady_rss_mib | isolated filesystem case process | 0 | 1 | — | 6.832 MiB | Process-only VmRSS after tree, merge, Git, and Jujutsu workloads. |
| filesystem | peak_rss_mib | isolated filesystem case process | 0 | 1 | — | 7.039 MiB | Process-only VmHWM across tree, merge, Git, and Jujutsu workloads. |
| filesystem | filesystem_surviving_children | post-workload cleanup | 0 | 1 | — | 0.000 count | Every status subprocess has completed before observation. |
| conversations | local_history_total_ms | 64 project-local generic JSONL sessions | 0 | 1 | — | 2.742 ms | Complete bounded local-history indexing. |
| conversations | local_history_session_count | complete project-local archive | 0 | 1 | — | 64.000 sessions | Validated after bounded paging completes. |
| conversations | recent_metadata_ms | 2,048 external sessions plus multiple adapters | 0 | 1 | — | 64.937 ms | Time until the first bounded metadata page is available. |
| conversations | recent_metadata_count | first external page | 0 | 1 | — | 67.000 sessions | Count available before total archive discovery completes. |
| conversations | archive_discovery_total_ms | complete external archive | 0 | 1 | — | 2440.094 ms | 32 bounded pages; recent availability measured separately. |
| conversations | archive_session_count | complete external archive | 0 | 1 | — | 2051.000 sessions | Metadata-only indexed sessions after all bounded pages. |
| conversations | append_incremental_ms | concurrently appendable generic JSONL transcript | 0 | 1 | — | 0.045 ms | Discovers and extracts only the safe appended suffix after a watermark. |
| conversations | conversation_tool_count | known external stores | 0 | 1 | — | 4.000 tools | 5 registered sources; discovered tools: claude-code, codex-cli, omp, pi |
| conversations | steady_rss_mib | isolated conversation case process | 0 | 1 | — | 21.160 MiB | Process-only RSS after complete archive indexing. |
| conversations | peak_rss_mib | isolated conversation case process | 0 | 1 | — | 27.430 MiB | Process-only VmHWM across recent and complete archive discovery. |
| concurrency | status_max_concurrent | 32 rapid refreshes in one Git workspace | 0 | 1 | — | 1.000 commands | 3 total git status commands; 0 overlap observations. |
| concurrency | status_command_count | 32 rapid refreshes in one Git workspace | 0 | 1 | — | 3.000 commands | Coalescing may schedule a final latest-generation command after the active one. |
| concurrency | status_surviving_children | post-refresh cleanup | 0 | 1 | — | 0.000 count | Every PID observed by the synthetic git executable is checked in /proc. |
| concurrency | status_worker_completions | coalesced refresh scheduler | 0 | 1 | — | 5.000 count | 4 redraws while applying completed generations. |
| concurrency | surviving_workers | post-workload cleanup | 0 | 1 | — | 0.000 count | Observed after explicit worker shutdown and subprocess wait. |
| concurrency | surviving_children | post-workload cleanup | 0 | 1 | — | 0.000 count | Observed after explicit worker shutdown and subprocess wait. |
| rss-exclusion | rss_exclusion_parent_delta_mib | 64 MiB direct child allocation | 0 | 1 | — | 0.336 MiB | Parent VmRSS delta while the child has faulted 64 MiB of private pages. |
| rss-exclusion | rss_exclusion_surviving_children | post-helper cleanup | 0 | 1 | — | 0.000 count | Helper is killed and waited before the cleanup snapshot. |

## Failed-budget attribution

No measured budget failed; no corrective follow-up issue is required.

## Residual risks

- TestBackend excludes terminal driver and multiplexer transport latency.
- Idle sampling uses Linux scheduler ticks, whose resolution is CLK_TCK-bound.
- Synthetic repositories do not model slow disks, network filesystems, or huge Git indexes.
- External fixtures model validated metadata formats but not arbitrary provider version drift.
- Filesystem cache warmth materially affects archive inventory time.
- The synthetic git executable isolates scheduler concurrency from real Git latency.
- Single-machine synthetic baselines do not characterize all user hardware.
- OS page cache, CPU frequency scaling, and unrelated host load remain measurable noise.
- No real transcript content is used; only fixture-validated synthetic metadata shapes are covered.
