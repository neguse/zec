# Roadmap

zec was rebuilt on [architecture.md](architecture.md). Features from the
previous tree returned one at a time as `src/features/<name>/`; the last
complete previous tree is commit `f40e791`, whose `docs/` describe each
feature and whose `tests/` are its acceptance criteria. This page records
what is deliberately absent: what stayed in history when a feature
returned, and what is deferred.

## Left in history

Parts of the previous tree that a returning feature did not bring back.
Each stays in `f40e791` and returns only with a reason.

| Feature | Left in history |
| --- | --- |
| Quick Open | the `RepositoryIndex`, its benchmark, symlink alias dedupe; Zed's worktree snapshot and `fuzzy_nucleo` replace them |
| project search | worker pools, disk prefilter, source budgets, benchmark, the 5,000-file / 10,000-range limits; `Project::search` replaces them |
| buffer search | `ActiveSearch`, `GoToLinePrompt`; the Editor's `SearchableItem` replaces them |
| splits and layout | divider dragging, preview and pinned tabs, tab reordering |
| sessions | generations, recovery blobs, leases, quarantine, `ZEC_SESSION_DIR` |
| project and outline panels | filter prompts, outline follow-cursor, entry copy, hidden-file toggle, mouse dock resizing, dock state in sessions |
| language | the failure matrix and `lsp_failures`, MultiBuffer result tabs, the rename preview |
| git, terminal, tasks | the diff view, terminal search, task-log scenarios |
| edit prediction | Codestral |
| large files | nothing special-cased: the normal Buffer and Editor path carries a 100,000-line file with a 64 KiB line |

## Deferred

Scope cut for now, not decided against: each of these needs something
this tree cannot verify on one machine, and returns when that exists.

| Feature | Old source in `f40e791` | Needs before it returns |
| --- | --- | --- |
| debugger | `debugger_panel.rs` | a DAP adapter such as gdb in CI, `.zed/debug.json` scenarios |
| extension picker, icon themes | `extension_picker.rs`, `theme_picker.rs` | the extension registry; icons have no terminal projection |
| self-update CLI | `update.rs`, `script/release-manifest` | a release pipeline publishing the signed manifest |
| markdown preview, images | `rich_content.rs` | a decision on a terminal renderer; image protocols per terminal |
| remote SSH | `remote_session.rs`, `zec_remote_server.rs` | the `zec-remote-server` binary and its archive distribution |
| agent panel, inline assistant | `agent_panel.rs`, `inline_assistant.rs`, `fixture_acp` | an ACP fixture and a language model account in tests |
| collaboration, notebook | `collaboration_panel.rs`, `notebook.rs` | a Zed account and collab server; a Jupyter kernel |
