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

### ConPTY input semantics

Bytes written to the ConPTY input pipe pass through conhost's input
cooking before the client reads them, which changes what a test can send:

- Bracketed paste does not exist: the `2004` markers are cooked away and
  crossterm never sees a Paste event. Tests deliver text as keystrokes;
  consequently the editor groups it per keystroke (undo granularity
  differs from a Unix paste).
- `\n` cooks into Enter with a CONTROL modifier; `\r` gives a plain
  Enter. Text is normalized to `\r` before sending.
- Non-BMP characters are dropped by the cooked path. They survive only as
  win32-input-mode events (`ESC[Vk;Sc;Uc;Kd;Cs;Rc_`), sent as key-downs
  only — interleaved key-ups break surrogate reassembly — and
  control-modified events need a real virtual-key code (Vk 0 is dropped).
- Kitty keyboard enhancement cannot cross the legacy console API, so
  capability detection clamps to the legacy encoding on Windows; chords
  that only kitty can carry route through the command palette instead.
- Control- or alt-modified SGR mouse reports leak into the input stream
  as literal characters; tests use unmodified mouse events.
- Conhost strips APC sequences from ConPTY output, so kitty graphics
  payloads are unobservable through a ConPTY transcript.
- Crossterm on Windows reports key Release events (Unix only does under
  kitty). Input handlers must ignore them explicitly; a
  double-press-to-confirm flow that resets on "any other key" would
  otherwise disarm itself on the first key's release.

## What runs on Windows

- Every integration test in `cargo test --locked` (language_service,
  settings_reload, lsp_failures, update_cli, parity_contract, repository,
  workspace_layout, workspace_sessions, workspace_search, project_panel).
  No cargo feature is required.
- `e2e_language` and `e2e_language_bench` build and run on ConPTY.
- `e2e_tui` runs all nine scenarios on ConPTY. Signal, job-control,
  symlink, sshd, gdb, jupyter, and media-bridge steps stay Unix-only
  behind `#[cfg(unix)]`, as do the ConPTY-incapable steps listed above.
- The `e2e_repository`, `e2e_workspace`, and benchmark binaries compile
  and run everywhere; signal cases and the loss-free fork tracker are
  Unix-only, so the required-id set is platform-scoped
  (`fixture::platform_case_ids`). Repository fixture generation still
  requires Unix modes and symlinks; porting the generator is the next
  step toward running the repository suite on Windows.

`zec update apply` refuses to replace the running executable on Windows with
a documented error; tests/update_cli.rs verifies the behavior of both
platforms.

Known gap: toggling inlay hints on Windows flips the editor state but the
`textDocument/inlayHint` request is never issued; the e2e_tui inlay step
carries the TODO.
