# herdr-context

`herdr-context` is a [Herdr](https://herdr.dev) plugin that adds a project
context dock to the right of the active terminal or coding agent.

```text
┌────────────────┬──────────────────────────────────┬────────────────────────┐
│ Native sidebar │ Terminal / agent                 │ herdr-context          │
│ Herdr          │                                  │ Files | Conversations  │
└────────────────┴──────────────────────────────────┴────────────────────────┘
```

The dock is a narrow Herdr pane rather than an extension of the native
sidebar. This uses Herdr's public plugin surface, preserves compatibility with
upstream Herdr, and keeps the plugin independently installable.

## Planned features

### Files

- browse the directory from which the terminal opened the dock;
- expand and collapse directories with the keyboard or mouse;
- honor VCS ignore rules, including `.gitignore`, and configurable hidden-file
  visibility;
- support both Git and Jujutsu (`jj`) workspaces;
- normalize VCS states such as added, modified, deleted, renamed, copied,
  untracked, and conflicted;
- represent deleted files even though they no longer exist on disk;
- never intentionally modify project files or VCS history.

Jujutsu status requires `jj` 0.37 or newer and is integration-tested against
`jj` 0.44.0, the latest upstream release when this adapter was implemented.
The default `Fresh` mode follows Jujutsu's documented automatic working-copy
snapshot semantics only when Files is activated or manually refreshed with
`r`; it never polls in the background.
The injectable `Passive` mode adds `--ignore-working-copy` and marks the
displayed status as potentially stale. The plugin never issues commit,
bookmark, operation-rewrite, or other explicit mutation commands.

### Conversations

- list LLM conversations associated with the current project or worktree;
- discover histories stored inside the project as the preferred source;
- discover external histories stored elsewhere on the filesystem and associate
  them through canonical project metadata;
- merge filesystem history with active Herdr sessions, using Herdr as runtime
  enrichment rather than as the sole history source;
- support an extensible set of LLM tools instead of a closed provider list;
- show at least the title, tool, timestamp, source, and live/archived state;
- isolate source-specific failures so one unreadable history cannot hide the
  others.

Automatic discovery is possible only when a tool exposes readable local
history or project metadata. Undocumented, encrypted, or remote-only histories
require a dedicated adapter and cannot be inferred safely.

## Target experience

- `Files` and `Conversations` are two views rendered by the same TUI process;
- the dock remains the rightmost pane in its tab;
- at most one dock instance is open per tab;
- one shortcut toggles between open, focus, and close;
- each view preserves its selection and scroll position across tab switches;
  Conversations also preserves collapsed provider groups across refreshes;
- the project context is captured from the originating terminal before the
  plugin receives focus.

### Controls

- `Tab` / `Shift+Tab` or `1` / `2`: switch views without restarting the dock;
- arrow keys or `h` / `j` / `k` / `l`: navigate the Files tree or visible
  Conversation provider/session rows;
- `Home` / `End`: select the first or last visible row;
- `Enter` / `Space`: expand or collapse the selected directory or Conversation
  provider; no Conversation action is attached to a session row;
- `r`: refresh the active view;
- left click selects a Files or Conversation row; right click toggles a Files
  row or Conversation provider, and clicking a Conversation disclosure marker
  also toggles its group; the mouse wheel navigates;
- `q`, `Esc`, or `Ctrl+C`: close the TUI and restore the terminal.

### Project-local generic conversations

The Conversations view checks only these registered project-relative locations:
`.herdr/conversations/` (direct children only), `.herdr/conversations.jsonl`,
and `.herdr/conversations.json`. It never scans the project recursively.

Generic JSONL records require `session_id`, the canonical project-root `cwd`, an
RFC 3339 `timestamp`, a `role` (`user`, `assistant`, `system`, or `tool`), and a
string `message`. A `.json` file contains one record with the same schema.
Discovery is read-only and bounded; message bodies are validated but never
returned to the UI or diagnostics.
Conversations are grouped alphabetically by provider, with sessions ordered
most-recent-first inside each expanded group. Provider groups can be collapsed
without loading or exposing transcript content.

### Live Herdr conversations

Conversation history remains filesystem-first. A separate coalesced background
job reads Herdr's normalized `agent list` session references, associates them
with the canonical project, and enriches matching rows without writing live
metadata to the conversation cache. Matching uses, in order, the tool plus
native session ID, an exact canonical transcript path plus file fingerprint,
then a verified tool-specific path identity. Titles, timestamps, and path
prefixes are never identities.

Unmatched active sessions appear as transient live-only rows. Their stable
documented identity lets selection survive when the filesystem transcript
appears later. Herdr failures are warnings: filesystem history remains visible.
The browser exposes status and resumability metadata only; it provides no
resume, launch, edit, delete, summarize, or upload action.

### Verified external conversations

When Herdr provides `HERDR_PLUGIN_STATE_DIR`, the Conversations worker also
checks exactly the fixture-backed Claude Code `2.1.232`, Codex CLI `0.147.0`,
and Pi `0.84.1` stores described in
[`docs/conversation-source-formats.md`](docs/conversation-source-formats.md).
Encoded directories, date paths, and filenames are only hints: native session
IDs, timestamps, and canonical `cwd` evidence must agree before metadata is
accepted.

Discovery runs in bounded recent-first pages on the low-priority worker. Its
disposable cache below `HERDR_PLUGIN_STATE_DIR/conversations/` uses private
permissions and atomic generation replacement. It contains only display,
provenance, resume, fingerprint, and source-watermark metadata—never transcript
content or opaque provider payloads.
Large JSONL sessions advance through complete-record cursors instead of being
loaded or rejected as one file; deleted or replaced sessions are removed after
a conclusive source refresh. Incomplete bounded inventories preserve prior
metadata, and source-scoped warnings remain visible beside healthy adapters.
Platforms where owner-only cache permissions cannot be enforced fail closed
instead of persisting external metadata.


## Configuration

The plugin reads `config.toml` from `HERDR_PLUGIN_CONFIG_DIR`. Loading is
read-only, byte-bounded, and performed on a worker after the first frame.
Missing or malformed files use safe defaults. Invalid fields fall back
independently and produce a sanitized warning in the dock.

```toml
[dock]
initial_width = 40       # 24..60

[files]
show_hidden = false
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
quit = ["q", "esc", "ctrl+c"]
```

Configured history roots remain subject to each adapter's version, layout,
canonical-project, and metadata bounds. Extra project roots are
project-relative; extra external roots are absolute. Unsupported or unreadable
roots are isolated so healthy sources and cached metadata remain visible.

Files and VCS refresh immediately when the Files view is activated or `refresh`
is invoked. Adaptive Git polling starts at the configured minimum, doubles
while status is unchanged up to the maximum, and resets when status changes.
It is suspended outside the Files view. Fresh Jujutsu never polls; passive
Jujutsu polls only when `passive_jujutsu_interval_ms` is non-zero and always
renders status as potentially stale.

## Design principles

- **Public Herdr integration**: manifest, injected context, CLI, and socket API.
- **One Rust/Ratatui binary**: no cross-pane IPC for the core user experience.
- **Filesystem-first conversation discovery**: project-local and external
  histories are indexed independently from Herdr's session lifecycle.
- **Responsive by construction**: disk reads, Git/Jujutsu commands, and
  transcript parsing never block the rendering thread.
- **Bounded work**: lazy tree expansion, coalesced background jobs, limited
  caches, incremental history indexing, and suspended inactive-view refreshes.
- **VCS-neutral core**: Git and Jujutsu adapters feed one normalized status
  model without leaking backend-specific behavior into the tree or UI.
- **Read-only and resilient**: a directory without a supported VCS, malformed
  transcript, or missing tool must never close the dock.
- **Local privacy**: conversation content is not sent over the network, and
  the cache stores only the metadata required for discovery and display.

## Project status

The project is in the design phase. The planned architecture, data flows,
performance constraints, and implementation order are recorded in
[`TODO.md`](TODO.md). That document is input for future ticket preparation; it
is not an implementation backlog.

## References

- [Herdr plugins](https://herdr.dev/docs/plugins/)
- [Herdr socket API](https://herdr.dev/docs/socket-api/)
- [herdr-beads](https://github.com/miiraheart/herdr-beads) for the docked-pane
  integration pattern
- [Jujutsu CLI reference](https://docs.jj-vcs.dev/latest/cli-reference/)
- [Jujutsu templates](https://docs.jj-vcs.dev/latest/templates/)
- [herdr-file-viewer](https://github.com/smarzban/herdr-file-viewer) for the
  Git-aware tree and repository trust boundaries
- [herdr-agent-inbox](https://github.com/douglascorrea/herdr-agent-inbox) for
  active-session enrichment and examples of native transcript formats