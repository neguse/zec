# Rebuild plan

Status: core in place; features returning. This document is temporary and
is deleted when the last row of the return table is done.

## Approach

The `rebuild` branch replaces the previous tree instead of refactoring it.
The last complete tree is commit `f40e791`; its `docs/` describe every
feature's behavior and its `tests/` are the acceptance criteria for
bringing a feature back. The first commit removed `src/main.rs`, every
feature module, the e2e binaries under `src/bin/`, their docs, and the e2e
workflows; the second built the core on `docs/architecture.md`. Features
return one at a time as `src/features/<name>/`. Returning a feature is also
the moment to decide whether zec wants it; one that is not wanted stays in
history.

## Core

`zec FILE...` and `zec DIRECTORY`: open, edit, save, Save As, reload,
external change tracking; tabs on a single-pane `WorkspaceModel`; quit and
close confirmation; syntax highlighting; settings and keymap loading;
command palette; status row; terminal lifecycle; `--smoke`.

Transplanted with trimming: `terminal.rs` (split into session,
capabilities, reader), `render.rs`, `clipboard.rs`, `prompt.rs`, `tabs.rs`,
`workspace_model.rs`, `cli.rs`, the palette search from `actions.rs`.
Rewritten: everything that lived in `main.rs`.

## Return order

| # | Feature | Needs | Old sources | Acceptance |
| --- | --- | --- | --- | --- |
| 1 | repository: root, index, Quick Open | core | `repository.rs`, `QuickOpenPrompt` | `e2e_tui` quick open, `e2e_repository` |
| 2 | project search | 1 | `project_search.rs`, `ProjectSearch*` in `main.rs` | `e2e_tui` search, `e2e_repository_bench` |
| 3 | buffer search and replace, go to line | core | `ActiveSearch`, `GoToLinePrompt` | `e2e_tui` |
| 4 | LSP: completion, hover, diagnostics, locations, rename, code actions | 1 | `LanguageOverlay`, six `*Prompt`s, `fixture_lsp` | `language_service`, `lsp_failures`, `e2e_language` |
| 5 | project panel, outline panel | 1 | `project_panel.rs`, `outline_panel.rs` | `project_panel`, `e2e_tui` |
| 6 | splits, docks, layout, sessions | core | `workspace_model.rs`, `workspace_render.rs`, `workspace_session.rs` | `workspace_*`, `e2e_workspace` |
| 7 | git, terminal, tasks, debugger | 1 | `git_panel.rs`, `terminal_panel.rs`, `task_picker.rs`, `debugger_panel.rs` | development-loop scenarios |
| 8 | extensions, themes, update | core | `extension_picker.rs`, `theme_picker.rs`, `update.rs` | `update_cli`, `settings_reload` |
| 9 | markdown, images, large files | core | `rich_content.rs` | `e2e_tui` |
| 10 | remote SSH | core | `remote_session.rs`, `zec_remote_server.rs` | `e2e_tui` remote |
| 11 | agent, inline assistant, edit prediction | 1 | `agent_panel.rs`, `inline_assistant.rs`, `edit_prediction.rs`, `fixture_acp` | `e2e_tui` |
| 12 | collaboration, notebook | 1 | `collaboration_panel.rs`, `notebook.rs` | `e2e_tui` |

Old sources and acceptance suites are paths in `f40e791`; the acceptance
scenarios return into `tests/e2e.rs` or a per-feature test file.

Returned: 1 as `features/quick_open` (Zed's worktree snapshot and
`fuzzy_nucleo`, the file finder's matcher; the previous `RepositoryIndex`,
its benchmark, and symlink alias dedupe stay in history). 2 as
`features/project_search` (`Project::search` with a text `SearchQuery`,
hits sorted by path and capped at 1000; the previous worker pools, disk
prefilter, source budgets, benchmark, and 5,000-file / 10,000-range limits
stay in history). 3 as `features/buffer_search` (Zed's `SearchableItem` on
the Editor owns matching, highlights, activation, and the replacement
transactions; the previous `ActiveSearch` and `GoToLinePrompt` stay in
history). 6 as core splits and layout (a pane tree in `workspace.rs`, the
render plan in `layout.rs`; keyboard focus, move, and resize, and a mouse
click focuses a pane; divider dragging, preview and pinned tabs, tab
reordering, and navigation history stay in history) plus
`features/sessions` (one JSON per root under the data directory, written
when the tab set or layout changes and at exit; generations, recovery
blobs, leases, quarantine, and `ZEC_SESSION_DIR` stay in history). 5 as
`features/project_panel` and `features/outline_panel` on docks in the core
(left and right; the bottom dock arrives with the first bottom panel):
rows are Zed's worktree
snapshot and Zed's buffer outline read on every frame, mutations go
through `Project::{create,rename,delete}_entry`; the previous filter
prompts, outline follow-cursor, entry copy, hidden-file toggle, preview
tabs, mouse dock resizing, and dock state in sessions stay in history. 4
as `features/language`: completion, hover, diagnostics, definitions, type
definitions, references, rename, and code actions through `Project`'s
LSP requests, each a picker, prompt, or text overlay; the completion menu
is a zec picker over `Project::completions` (the Editor's own menu has no
public read access and is disabled in the hidden window), applied through
the Editor with the server's extra edits from `LspStore`. Zed's worktree
trust gate is kept: an unknown root shows `[restricted]` until
`TrustWorktree` trusts it, and Zed downloads server binaries as it does in
the app unless `settings.json` names a binary. The tests use
`src/bin/fixture_lsp.rs`, a trimmed transplant, bound as rust-analyzer;
the failure matrix, `lsp_failures`, MultiBuffer result tabs, and the
rename preview stay in history. `ZEC_LOG=path` now captures Zed's log
records. 7 minus the debugger: `features/git_panel` (Zed's active
repository snapshot sectioned on every frame; stage, unstage, and commit
through `Repository`; commit uses the repository's own identity and fails
rather than prompting for a password), the bottom dock in the core with
`features/terminal_panel` (Zed's `terminal` entities; keys the keymap does
not claim go to the shell as Zed's escape sequences or bytes, the grid is
synced and projected every frame), and `features/tasks` (Zed's task
inventory resolved against the root and the active buffer, run through
`Project::create_terminal_task` into the terminal panel). The debugger is
deferred, decided with rows 8 to 12; the previous diff view, terminal
search, and task-log scenarios stay in history.

Returned in part: 8 as `features/theme_picker` (Zed's `ThemeRegistry`,
applied live through `GlobalTheme` and saved through Zed's settings file;
icon themes, the extension picker, and the update CLI are decided with the
remaining rows). 11 as `features/edit_prediction` (Zed's providers on the
hidden Editors, chosen from Zed's language settings; ghost text and
acceptance are the Editor's own; Codestral, the agent panel, and the
inline assistant are decided with the remaining rows). 9 as the large-file
PTY scenario only: no special casing exists, the normal Buffer and Editor
path carries a 100,000-line file with a 64 KiB line.

## Behavior carried into the core

Non-obvious constraints from the previous tree that the core keeps:

- Start the reader thread after raw mode is active; starting earlier races
  fast session restores and swallows the first keystroke.
- Capture snapshots inside `try_draw`; the first frame is empty otherwise.
- Bounded event channel: input applies backpressure, `Redraw` coalesces.
- Restore raw mode, alternate screen, mouse, paste, and keyboard protocol
  even when an earlier step fails; remove signal handlers last.
- Repaint cost is bounded by the viewport, not the document (100k lines).
- `Ctrl-S` is handled by zec; Zed's binding is a Workspace action.
- Save formats through `Project::format` with the save trigger before the
  write, like Zed's editor; a formatter failure is logged and the write
  still happens.
- Save As resolves relative to the startup cwd with no shell expansion,
  overwrites a regular file only on a confirmed second submit, refuses
  directories and special files, and refuses to bind two Buffers to one
  path.
- A clean Buffer auto-reloads on external change; a dirty one shows `!`
  and saves over the disk only after confirmation. Reload is an undoable
  Zed transaction. An external rename follows the Buffer's file identity;
  an external delete keeps the text and recreates the path on save.
- Scratch buffers come from `BufferStore::create_local_buffer`, never
  `Buffer::local`.
- Grammars register lazily; eager registration costs seconds on the first
  frame.
- Windows: native GPUI platform with hidden windows, and the `PATH` key
  canonicalized before `Project::local`.

Each returning feature takes its own list from the `f40e791` architecture
doc section that describes it.

## Verification per step

```sh
cargo fmt --all -- --check
cargo test --locked --bin zec -- --test-threads=1
cargo test --locked --test e2e -- --test-threads=1
./target/debug/zec --smoke
```
