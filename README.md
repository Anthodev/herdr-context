<h1 align="center">herdr-context</h1>

<p align="center">
  <strong>Project context, docked next to your terminal.</strong><br>
  A Herdr plugin that puts your files and LLM conversation history in a narrow
  pane beside the active terminal or coding agent.
</p>

<p align="center">
  <a href="#about">About</a>
  ·
  <a href="#highlights">Highlights</a>
  ·
  <a href="#install">Install</a>
  ·
  <a href="#controls">Controls</a>
  ·
  <a href="#configuration">Configuration</a>
</p>

<p align="center">
  <a href="https://github.com/Anthodev/herdr-context/releases"><img src="https://img.shields.io/github/v/release/Anthodev/herdr-context?label=release" alt="Latest release"></a>
  <a href="https://github.com/Anthodev/herdr-context/actions/workflows/ci.yml"><img src="https://github.com/Anthodev/herdr-context/actions/workflows/ci.yml/badge.svg?branch=develop" alt="CI"></a>
  <a href="LICENSE"><img src="https://img.shields.io/github/license/Anthodev/herdr-context" alt="License"></a>
  <img src="https://img.shields.io/badge/Herdr-0.8.0%2B-1D99F3" alt="Requires Herdr 0.8.0 or newer">
  <img src="https://img.shields.io/badge/platforms-linux%20%7C%20macOS-6B7280" alt="Linux and macOS">
</p>

> [!NOTE]
> herdr-context is an independent plugin built entirely on Herdr's public
> plugin surface, and is discoverable through the
> [community marketplace](https://herdr.dev/plugins/). External history
> discovery covers exactly the provider versions listed below; anything else
> degrades gracefully instead of closing the dock.

## About

**herdr-context** docks two views to the right of your work: a **Files**
browser colored by Git/Jujutsu status, and a **History** browser for LLM
conversations tied to the current project. Both render in one Ratatui process,
and the project context is captured from the originating terminal before the
dock ever takes focus.

```text
┌────────────────┬──────────────────────────────────┬────────────────────────┐
│ Native sidebar │ Terminal / agent                 │ herdr-context          │
│ Herdr          │                                  │ Files | History        │
└────────────────┴──────────────────────────────────┴────────────────────────┘
```

The dock is a narrow Herdr pane rather than an extension of the native
sidebar. That keeps the plugin on Herdr's public plugin surface, compatible
with upstream Herdr, and independently installable.

The goal is simple: **know where the project stands — files, changes,
conversations — without leaving the tab.**

## Highlights

- **Two views, one process** — switch between Files and History without
  restarting the dock; selection, scroll position, and collapsed groups
  survive tab switches.
- **Guide-free tree** — two-column indent and a single state chevron per
  row, under a bold uppercase project header; the focused row is a
  full-width neutral band that keeps status colors readable.
- **VCS-aware tree** — added, modified, and deleted paths light up green,
  yellow, or red; directories inherit the strongest status of their
  descendants, and deleted files remain visible even though they left disk.
- **Modified-files list** — every changed path in one flat list beneath the
  tree, with green `+N` / red `-N` workspace diff totals on the divider.
- **History across tools** — Claude Code, Codex CLI, Pi, OMP, and OpenCode
  sessions associated with the project, merged with live Herdr sessions.
- **Resume in one keystroke** — `Enter` on a session opens it in a new
  focused tab of the current workspace.
- **File references** — `Enter` on a file row inserts its project-relative
  `@path` into the originating pane and hands focus back to it.
- **Path filter** — `/` searches project-relative paths across collapsed
  directories through a bounded background index.
- **Responsive by construction** — disk reads, Git/Jujutsu commands, and
  transcript parsing never block the rendering thread.
- **Read-only and local** — the plugin never modifies project files or VCS
  history, sends nothing over the network, and caches metadata only.

## Files

- browse the directory the dock was opened from;
- expand and collapse directories with keyboard or mouse;
- show the project name — the root directory's own — as a bold uppercase
  header above the tree;
- honor `.gitignore` and configurable hidden-file visibility;
- search paths across collapsed directories; the index follows the same
  visibility and ignore rules as the tree;
- support Git and Jujutsu (`jj`) workspaces behind one normalized status
  model — added, modified, deleted, renamed, copied, untracked, conflicted;
- split into a top tree and a bottom flat list of every file carrying a
  status, separated by a rule showing `+N` / `-N` tracked line totals
  (file count when the diff is unavailable); the rule lights up while the
  flat pane holds focus, and the flat pane hides while the path filter is
  active;
- color names from semantic theme slots, with deterministic truecolor
  overrides for VCS green/yellow/red so terminal palette remapping cannot
  turn a red deletion green;
- never intentionally modify project files or VCS history.

Jujutsu status requires `jj` 0.37 or newer and is integration-tested against
`jj` 0.44.0. The default `Fresh` mode snapshots only when Files activates or
you press `r` — it never polls in the background. The opt-in `Passive` mode
adds `--ignore-working-copy` and always marks the shown status as potentially
stale. The plugin never issues commit, bookmark, or rewrite commands.

## History

- list conversations associated with the current project or worktree;
- prefer histories stored inside the project, then discover external stores
  elsewhere on the filesystem through canonical project metadata;
- merge filesystem history with live Herdr sessions as enrichment, never as
  the sole source;
- group providers alphabetically with most-recent sessions first; collapsing
  a provider group loads and exposes no transcript content;
- isolate failures per source, so one unreadable store cannot hide the
  others.

| Source | Kind |
|---|---|
| Claude Code `2.1.232` | external store |
| Codex CLI `0.147.0` | external store |
| Pi `0.84.1` | external store |
| OMP `17.3.2` | external store |
| OpenCode `1.18.18` | external store |
| Project-local records | `.herdr/conversations/`, `.jsonl`, `.json` |
| Live Herdr sessions | runtime enrichment |

Project-local records are read shallowly — direct children of
`.herdr/conversations/` plus two fixed filenames, never a recursive scan.
Each record needs `session_id`, the canonical project-root `cwd`, an RFC 3339
`timestamp`, a `role`, and a string `message`. Message bodies are validated
and never returned to the UI or diagnostics.

External stores are only hints until verified: encoded directory layouts and
filenames count for nothing unless native session IDs, timestamps, and
canonical `cwd` evidence agree. Live sessions match by tool plus native ID,
then transcript path plus fingerprint, then verified path identity — titles,
timestamps, and prefixes are never identities. Unmatched live rows appear
transiently and survive until the filesystem transcript appears; Herdr
failures are warnings while filesystem history stays visible.

Discovery runs in bounded recent-first pages on the low-priority worker.
Large JSONL sessions advance through complete-record cursors instead of being
loaded whole, and sessions disappear only after a refresh proves they were
deleted or replaced. Incomplete inventories keep prior metadata, and
source-scoped warnings stay visible beside healthy adapters.

## Install

### From GitHub

```sh
herdr plugin install Anthodev/herdr-context
```

Herdr clones the repository, runs the manifest build
(`cargo build --release --locked`), stores a managed checkout, and registers
the plugin. This path needs `git` and a Rust toolchain (the crate pins
1.97.1). There is no separate update command in plugin v1 — reinstall to
refresh.

### Requirements

- Herdr `0.8.0` or newer and a POSIX shell.
- `git` plus a Rust toolchain (the crate pins 1.97.1) for the source
  install; the build runs `cargo build --release --locked` on your machine.
- Git is optional at runtime; Jujutsu status needs `jj` `0.37` or newer,
  and missing VCS tools degrade the affected status view without closing
  the dock.

### Upgrade and uninstall

There is no separate update command in plugin v1 — run
`herdr plugin install Anthodev/herdr-context` again to refresh the managed
checkout, and `herdr plugin uninstall herdr-context` to remove it. Reinstall
and uninstall preserve Herdr-managed config, state, and conversation
histories.

## Controls

| Input | Action |
|---|---|
| `Tab` / `Shift+Tab`, `1` / `2` | Switch views without restarting the dock |
| Arrows or `h` `j` `k` `l` | Navigate the focused pane (tree, modified-files list, or conversation rows) |
| `Home` / `End` | Select the first or last visible row |
| `Enter` / `Space` | Expand or collapse directories and provider groups; insert `@path` from a file row; resume a resumable session in a new focused tab |
| `w` | Move focus between the Files tree and its modified-files list; activating a missing file reports in the notice line instead of inserting a reference |
| `/` | Edit a live path filter — `Enter` keeps it, `Ctrl+U` clears the query while editing, `Esc` clears an active filter before it can close the TUI |
| Mouse | Click selects and focuses a row, right-click toggles, wheel scrolls the focused pane |
| `q`, `Esc` with no filter, `Ctrl+C` | Close the dock and restore the terminal |

## Configuration

The plugin reads `config.toml` from `HERDR_PLUGIN_CONFIG_DIR` on a worker,
after the first frame. An absent file silently uses safe defaults; malformed
files and invalid fields fall back field-by-field and produce a sanitized
warning in the dock.

```toml
[dock]
initial_width = 40       # 24..60

[ui]
display_mode = "ascii" # ascii, unicode, or nerd
colored_icons = true   # nerd mode only: type-colored file icons

[files]
show_hidden = false
show_ignored = false          # `i` toggles ignored files in-session
search_ignored = false        # search ignored paths without showing them normally
exclusions = ["target", "generated/cache"] # project-relative paths

[conversations]
enabled_sources = [
  "claude-code",
  "codex-cli",
  "omp",
  "opencode",
  "pi",
  "project-local-generic-jsonl",
]
project_roots = [".agents/history"] # additional shallow project-local roots
page_size = 128                     # 1..512 records per source and pass
cache_entries = 4096                # 16..4096 metadata rows

[conversations.external_roots]
claude-code = ["/home/me/.claude/projects"]
pi = ["/home/me/.pi/agent/sessions"]

[vcs]
backend = "auto"            # auto, git, or jj
jujutsu_mode = "fresh"      # fresh or passive
git_cadence = "manual"      # manual or adaptive
git_min_interval_ms = 2000  # adaptive only; 250..300000
git_max_interval_ms = 30000
passive_jujutsu_interval_ms = 0 # 0 disables; otherwise 1000..300000

[keybindings]
refresh = ["r"]
search = ["/"]
toggle_files_focus = ["w"]
toggle_ignored_files = ["i"]
quit = ["q", "esc", "ctrl+c"]
```

Extra project roots are project-relative; extra external roots are absolute.
Every configured root remains subject to its adapter's version, layout, and
metadata bounds, and unreadable roots are isolated so healthy sources stay
visible.

Rows share one anatomy in every mode — status marker, two-column indent,
state glyph, optional icon, name — with no tree guide connectors:

| Mode | Look | Requires |
|---|---|---|
| `ascii` | `+` / `-` directories, `f` / `l` files; `*` / `-` / `?` History states | Nothing — the compact default |
| `unicode` | `▸` / `▾` chevrons, Unicode file and session bullets | A Unicode-capable terminal |
| `nerd` | Chevrons plus Nerd Font folder and typed-file icons, colored by type (`colored_icons = false` for monochrome) | A Nerd Font |

Selection is a full-width neutral band in both views; markers and names
keep their colors on top of it.

Changing modes reuses cached state — no additional filesystem reads or
traversal.

Files and VCS refresh immediately when Files activates or `refresh` fires.
Adaptive Git polling starts at the minimum interval, doubles while status is
unchanged up to the maximum, resets on change, and suspends outside the Files
view. Fresh Jujutsu never polls; passive Jujutsu polls only when its interval
is non-zero and always renders status as potentially stale.

## Privacy

- Config: `herdr plugin config-dir herdr-context`, normally below
  `${XDG_CONFIG_HOME:-$HOME/.config}/herdr/plugins/config/herdr-context/`.
- State and metadata-only conversation cache: normally below
  `${XDG_STATE_HOME:-$HOME/.local/state}/herdr/plugins/herdr-context/`.
- Managed source checkout: shown by `herdr plugin list` for the
  `herdr-context` entry.

Nothing is packaged or shipped: the install path is a source checkout built
on your machine. Conversation content never leaves the machine; the
disposable cache stores display, provenance, resume, and watermark metadata
only, with private permissions and atomic generation replacement. Platforms
that cannot enforce owner-only cache permissions fail closed instead of
persisting external metadata.

## Limitations

- The manifest declares Linux and macOS only; Windows is unsupported, and
  the source build targets whatever the pinned Rust toolchain builds.
- External history discovery is restricted to the fixture-validated provider
  versions above; undocumented, encrypted, or remote-only histories need a
  dedicated adapter and cannot be inferred safely.
- History can resume sessions but never edits, deletes, summarizes, or
  uploads them.

## Design principles

- **Public Herdr integration** — manifest, injected context, CLI, and socket
  API only.
- **One Rust/Ratatui binary** — no cross-pane IPC for the core experience.
- **Filesystem-first history** — project-local and external conversations are
  indexed independently of Herdr's session lifecycle.
- **Responsive by construction** — nothing slow ever runs on the rendering
  thread.
- **Bounded work** — lazy expansion, coalesced jobs, limited caches,
  incremental indexing, suspended refreshes for inactive views.
- **VCS-neutral core** — Git and Jujutsu feed one normalized model without
  leaking backend specifics into the UI.
- **Read-only and resilient** — a missing VCS, malformed transcript, or
  absent tool must never close the dock.
- **Local privacy** — conversation content stays off the network; caches hold
  only what discovery and display require.

## Build from source

Requirements: Rust 1.97.1 (pinned in `Cargo.toml`) and a POSIX shell; `git`
and `jj` 0.37+ are optional at runtime.

```sh
git clone https://github.com/Anthodev/herdr-context.git
cd herdr-context
cargo build --release --locked
herdr plugin link "$PWD"
```

CI gates every change on rustfmt, Clippy with warnings denied, and the full
test suite.

## Release status

Version `0.19.4` is the current release line. Tag `v0.19.4`, Cargo
metadata, both manifests, and the minimum Herdr version are validated
together; pushing a `v*` tag runs formatting, Clippy, the full test suite,
and the contract checks, then publishes generated GitHub release notes.
There are no packaged assets to install.

Every performance budget has a retained independent review in
`release/performance-review.toml`; a failed budget blocks a tagged release
unless its record names the accepting authority, rationale, scope, and
follow-up issue. Ratatui measurements exclude terminal-driver and
multiplexer transport latency — see [docs/performance.md](docs/performance.md)
for budgets, baselines, and residual risks.

## Acknowledgements

- [herdr-beads](https://github.com/miiraheart/herdr-beads) — the docked-pane
  integration pattern
- [herdr-file-viewer](https://github.com/smarzban/herdr-file-viewer) — the
  Git-aware tree and repository trust boundaries
- [herdr-agent-inbox](https://github.com/douglascorrea/herdr-agent-inbox) —
  active-session enrichment and native transcript formats
- [Herdr plugin docs](https://herdr.dev/docs/plugins/) and
  [socket API](https://herdr.dev/docs/socket-api/)
- [Jujutsu CLI reference](https://docs.jj-vcs.dev/latest/cli-reference/) and
  [templates](https://docs.jj-vcs.dev/latest/templates/)

## Contributing

Issues and pull requests are welcome. Bug reports should include the Herdr
version, OS, terminal, and VCS backend, with steps to reproduce — and no
conversation content or other personal data.

## License

[MIT](LICENSE).
