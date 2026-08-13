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
- the project context is captured from the originating terminal before the
  plugin receives focus.

### Controls

- `Tab` / `Shift+Tab` or `1` / `2`: switch views without restarting the dock;
- arrow keys or `h` / `j` / `k` / `l`: navigate the Files tree;
- `Home` / `End`: select the first or last visible row;
- `Enter` / `Space`: expand or collapse the selected directory;
- `r`: refresh the active view;
- left click selects, right click toggles a Files row, and the mouse wheel navigates;
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