# Rebuild plan

Status: in progress. This document is temporary and is deleted when the
last row of the return table is done.

## Approach

The `rebuild` branch replaces the previous tree instead of refactoring it.
The last complete tree is commit `f40e791`; its `docs/` describe every
feature's behavior and its `tests/` are the acceptance criteria for
bringing a feature back. The first commit removes `src/main.rs`, every
feature module, the e2e binaries under `src/bin/`, their docs, and the e2e
workflows, then builds the core on `docs/architecture.md`. Features return
one at a time as `src/features/<name>/`. Returning a feature is also the
moment to decide whether zec wants it; one that is not wanted stays in
history.

## Core

`zec FILE...` and `zec DIRECTORY`: open, edit, save, Save As, reload,
external change tracking; tabs on a single-pane `WorkspaceModel`; quit and
close confirmation; syntax highlighting; settings and keymap loading;
command palette; status row; terminal lifecycle; `--smoke`.

Transplanted with trimming: `terminal.rs` (split into session,
capabilities, reader), `render.rs`, `clipboard.rs`, `prompt.rs`, `tabs.rs`,
`workspace_model.rs`, `workspace_render.rs`, `cli.rs`, the palette search
from `actions.rs`. Rewritten: everything that lived in `main.rs`.

Done when: no `let mut` state in the loop, the contract tests pass, and the
PTY lifecycle and edit/save scenarios pass on Linux and Windows.

## Return order

| # | Feature | Needs | Old sources | Acceptance |
| --- | --- | --- | --- | --- |
| 1 | repository: root, index, Quick Open | core | `repository.rs`, `QuickOpenPrompt` | `e2e_tui` quick open, `e2e_repository` |
| 2 | project search | 1 | `project_search.rs`, `ProjectSearch*` in `main.rs` | `e2e_tui` search, `e2e_repository_bench` |
| 3 | buffer search and replace, go to line | core | `ActiveSearch`, `GoToLinePrompt` | `e2e_tui` |
| 4 | LSP: completion, hover, diagnostics, locations, rename, code actions | 1 | `LanguageOverlay`, six `*Prompt`s, `fixture_lsp` | `language_service`, `lsp_failures`, `e2e_language` |
| 5 | project panel, outline panel | 1 | `project_panel.rs`, `outline_panel.rs` | `project_panel`, `e2e_tui` |
| 6 | splits, docks, sessions | core | `workspace_session.rs` | `workspace_*`, `e2e_workspace` |
| 7 | git, terminal, tasks, debugger | 1 | `git_panel.rs`, `terminal_panel.rs`, `task_picker.rs`, `debugger_panel.rs` | development-loop scenarios |
| 8 | extensions, themes, update | core | `extension_picker.rs`, `theme_picker.rs`, `update.rs` | `update_cli`, `settings_reload` |
| 9 | markdown, images, large files | core | `rich_content.rs` | `e2e_tui` |
| 10 | remote SSH | core | `remote_session.rs`, `zec_remote_server.rs` | `e2e_tui` remote |
| 11 | agent, inline assistant, edit prediction | 1 | `agent_panel.rs`, `inline_assistant.rs`, `edit_prediction.rs`, `fixture_acp` | `e2e_tui` |
| 12 | collaboration, notebook | 1 | `collaboration_panel.rs`, `notebook.rs` | `e2e_tui` |

## Behavior to carry into the core

Non-obvious constraints from the previous tree that the core must keep:

- Start the reader thread after raw mode is active; starting earlier races
  fast session restores and swallows the first keystroke.
- Capture snapshots inside `try_draw`; the first frame is empty otherwise.
- Bounded event channel: input applies backpressure, `Redraw` coalesces.
- Restore raw mode, alternate screen, mouse, paste, and keyboard protocol
  even when an earlier step fails; remove signal handlers last.
- Repaint cost is bounded by the viewport, not the document (100k lines).
- `Ctrl-S` is handled by zec; Zed's binding is a Workspace action.
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
cargo test --locked --test e2e_tui -- --test-threads=1
./target/debug/zec --smoke
```
