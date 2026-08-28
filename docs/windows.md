# Windows support

zec treats Windows as a first-class native target. GPUI runs on the native
platform with hidden windows, the outer TUI runs in any VT-capable console
such as Windows Terminal, and the integrated terminal runs on ConPTY.

## Build requirements

- The MSVC toolchain (the pinned Rust from rust-toolchain.toml).
- The Visual Studio "MSVC C++ x64/x86 Spectre-mitigated libs" component.
  `msvc_spectre_libs`, pulled in through `languages` (Zed) → `pet`, requires
  it and its build script panics without it. GitHub-hosted runners already
  include it.

## Isolation and data placement

Setting `ZEC_DATA_DIR` redirects the whole set of Zed-side user directories
(config, data, logs, databases) under `$ZEC_DATA_DIR` (config lives at
`$ZEC_DATA_DIR/config`) through one mechanism that behaves identically on
every platform. The test harness and the e2e suites isolate themselves with
it; the XDG variables only cover Unix-specific residual paths.

Workspace sessions default to `ZEC_SESSION_DIR` >
`ZEC_DATA_DIR/state/workspaces` > (Unix: `XDG_STATE_HOME` or
`~/.local/state`; Windows: `LOCALAPPDATA`) + `/zec/workspaces`.

## Test harness

`PtySession` (src/bin/e2e_support) drives the real binary through
portable-pty's native PTY (a Unix pty, ConPTY on Windows). Platform
differences are concentrated in `TerminalBaseline`:

- Unix captures a termios snapshot, so raw-mode entry and restoration are
  proven against kernel state.
- ConPTY has no equivalent, so Windows judges those properties from the
  emitted VT output (the synthesized full repaint and cursor restoration).
  ConPTY also flattens the alternate-screen switch and absorbs the client's
  cleanup escapes, and the host must answer the cursor-position report that
  conhost sends at startup; `PtySession` does.

APIs built on signals (SIGTERM/SIGHUP/SIGTSTP), process groups, and /proc
metrics stay Unix-only behind `#[cfg(unix)]`. Windows has no process-group
or job-control convention for TUIs, so those cases are meaningful only as
Unix evidence. The Windows counterpart of VmHWM is PeakWorkingSetSize.

## What runs on Windows

- Every integration test in `cargo test --locked` (language_service,
  settings_reload, lsp_failures, update_cli, parity_contract, repository,
  workspace_layout, workspace_sessions, workspace_search, project_panel).
  No cargo feature is required.
- `e2e_language` and `e2e_language_bench` build and run on ConPTY.
- `e2e_tui` remains `#![cfg(target_os = "linux")]`. Enabling it per case on
  Windows, after separating the termios/signal-dependent cases, is the next
  step.
- The `e2e_repository` and `e2e_workspace` binaries stay behind the
  `e2e-linux` feature: the repository fixture manifest encodes Unix modes
  and symlinks as contract, so they cannot be ported without redesigning
  that contract.

`zec update apply` refuses to replace the running executable on Windows with
a documented error; tests/update_cli.rs verifies the behavior of both
platforms.
