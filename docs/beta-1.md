# Beta 1 contract: Local Development Loop

Contract status: Implemented candidate for Git, integrated terminal, tasks, DAP debugger,
debug console, and native Notebook (2026-08-28)

Beta 1 brings the local development loop onto the shared Terminal Workspace without creating
parallel Git, process, task, or debugger models. Zed `GitStore`, `zed_terminal::Terminal`,
`TaskInventory`, `DapStore`, and `DebugSession` remain authoritative. zec owns only terminal
projection, focus routing, capability reporting, and bounded presentation history.

The parity ledger remains `candidate` until a default-branch hosted run records canonical evidence.
Notebook has its own native-editor and kernel evidence; it is not inferred from the debug REPL.

## Outcome

A trusted local worktree can complete these workflows entirely from the console:

- inspect Zed's active repository snapshot, stage/unstage individual paths or all paths, and discard
  changes through `GitStore`;
- create multiple Zed integrated terminals, send keys/paste/resize, retain scrollback, and observe
  exit state without leaking child processes;
- discover `.zed/tasks.json` and language tasks through `TaskInventory`, launch the resolved task in
  a Zed terminal, and rerun the last task only after its lifecycle has completed;
- discover `.zed/debug.json` scenarios, resolve task variables and optional build tasks, boot the
  registered Zed DAP adapter, and service DAP `runInTerminal` reverse requests through the integrated
  terminal;
- toggle source breakpoints and inspect session state, threads, stack frames, variables, source
  breakpoints, and bounded console output;
- continue, pause, stop, step over/in/out, and evaluate adapter-native debug-console commands.

- open `.ipynb` through Zed `NotebookItem`/`NotebookEditor`, edit and reorder code/Markdown cells,
  run/advance/all, project stream/error/rich-output fallback, interrupt/restart kernels, clear outputs,
  persist nbformat JSON, share one authority across splits, and clean up every local kernel process.

## Authority and safety invariants

1. Git status and mutations are obtained from the active Zed repository snapshot and `GitStore`.
   zec does not parse porcelain output into a second repository model.
2. Terminal and task processes are created by Zed Project APIs. The Terminal Workspace stores only
   `Entity<zed_terminal::Terminal>` handles and projects their current terminal cells.
3. Debug scenarios come from `TaskInventory::list_debug_scenarios`; adapter names must be registered
   in `DapRegistry` before they are shown.
4. Worktree variables, relative `program`/`cwd`, debug build tasks, adapter locators, and adapter
   configuration conversion are resolved before `DapStore::boot_session`.
5. Untrusted worktrees cannot start terminals, tasks, build commands, debug adapters, or debuggees.
6. DAP `runInTerminal` requests are represented by an integrated terminal. An external-terminal
   request is explicitly reported as adapted rather than silently pretending an external window was
   opened.
7. `DapStore` removes shutdown sessions immediately, so zec retains at most eight entity handles for
   post-mortem projection. Terminated sessions reject REPL and control commands; their final output
   remains visible.
8. Terminal text and debugger output are control-sanitized and bounded before rendering. A missing
   adapter, process ID, thread, worktree, or terminal capability produces an actionable status error.
9. `NotebookEditor` owns cells, cell editors, execution requests, and Jupyter routing. The Project
   Buffer owns save/session state; terminal snapshots do not become a second notebook model.
10. A kernel never starts before worktree trust. Split views share the Notebook Entity, while closing
    the final view drops it and reaps any matching local Jupyter process group.
11. Existing rich outputs omitted by the pinned upstream serializer are retained by cell ID until
    that cell is run or outputs are cleared, preventing an unrelated edit from erasing notebook data.

## Console routes

Every route is also available through `F1` command search, which is the portable fallback when a
terminal cannot distinguish a key sequence.

| Workflow | Default route |
| --- | --- |
| Git panel | `Ctrl-Shift-G` |
| Terminal / new terminal | `Ctrl-\`` / `Ctrl-Shift-\`` |
| Run / rerun task | `Ctrl-Shift-B` / `Ctrl-Alt-B` |
| Start debugging | `F5` |
| Debugger panel | `Ctrl-Shift-D` |
| Toggle breakpoint | `Ctrl-F9` |
| Continue / pause / stop | `Ctrl-F5` / `Ctrl-F6` / `Shift-F5` |
| Step over / in / out | `Alt-F10` / `Alt-F11` / `Alt-Shift-F11` |
| Debug console | `Ctrl-Shift-R` or `:` while the debugger panel is focused |
| Notebook edit/run | arrows + `Enter`; `Ctrl-Enter` / `Shift-Enter` |
| Notebook cells/kernel | `b`/`m`, `dd`, `Alt-Up/Down`, `R`, `c`, `i`, `r` |

Within a focused terminal, key and paste events are written to the Zed terminal, `Esc` returns to
the editor, and scroll input changes only terminal scrollback. Within a focused debugger panel,
up/down selects a reported thread, `c/p/n/i/o/k` perform the corresponding control operation, `:`
opens the debug console, and `Esc` returns to the editor.

## Machine evidence

The normal deterministic gate is:

```sh
cargo fmt --all -- --check
cargo test --locked --bin zec -- --test-threads=1
cargo test --locked --test pty_acceptance -- --test-threads=1
```

`pty_acceptance` contains three Beta 1 actual-binary scenarios:

- `beta_1_terminal_git_and_tasks_run_through_the_actual_binary` exercises a real repository, Zed Git
  staging, an interactive integrated shell, task discovery/execution, task completion, and rerun.
- `beta_1_debugger_and_repl_run_through_zed_dap_in_the_actual_binary` compiles a C debuggee, loads a
  real `.zed/debug.json`, starts Zed's GDB adapter, resolves a source breakpoint, evaluates
  `print 1+1`, continues, pauses, steps, shuts down, and verifies PTY restoration.
- `parity_1_notebook_cells_outputs_kernel_controls_and_cleanup_run_through_the_actual_binary` opens
  native Zed Notebook state after trust, edits/saves/splits cells, projects stream/image/error output,
  interrupts/restarts/clears kernels, validates nbformat JSON, reaps old/final process groups, and
  restores the PTY.

The GDB case first probes whether the environment permits tracing an inferior. A sandbox that
denies `ptrace` reports a capability skip; it must not be counted as real-DAP evidence. On
2026-08-28 the complete locked PTY target and binary unit target pass locally. Canonical CI installs
GDB and sets
`ZEC_REQUIRE_GDB_DAP=1`, which turns an unavailable adapter or denied `ptrace` capability into a hard
failure instead of a skip.

## Remaining Beta 1 verification

- hosted artifact and evidence-only promotion for the candidate capabilities;
- real kernels that produce new image/HTML/JSON display data, plus missing/crashing/remote and
  Windows kernel lifecycle cases;
- broader adapter matrix and deterministic failure cases for missing binaries, malformed debug
  configuration, failed build tasks, rejected reverse requests, and adapter crashes.
