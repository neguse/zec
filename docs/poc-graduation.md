# PoC graduation contract

Status: Graduated (2026-08-23)

## Meaning of graduation

PoC graduation does not mean the editor gained features. It means the
foundation of a TUI built on Zed's editing core reached the state of "does
not destroy data, scales by design with document size, and can be verified
and reproduced automatically". From that point on, the work is treated as
sustained editor development rather than a feature PoC.

The initial target is single-process execution on supported Linux
terminals. Feature completeness — LSP, plugins, advanced mouse selection,
search options — is not a graduation condition. The performance suites
target documents whose individual display lines are ordinary source-code
length while the line count is large. Pathological documents with
megabytes in one line remain a separate known constraint: Zed's current
public APIs cannot fetch only visible columns, so materializing the whole
line and walking its graphemes remains. That constraint is not silently
treated as Pass; it stays in G3 below.

## Gates

### G1. Zed is the sole editing authority

Pass (2026-08-23).

- Text, cursor, selection, transactions, undo, and dirty state are owned
  by Zed's `Editor` / `Buffer`.
- File open/save/save-as/reload goes through Zed's
  `RealFs -> WorktreeStore -> BufferStore`.
- zec owns only terminal events, the viewport, status/prompts, and the
  conversion from UTF-8 byte coordinates to terminal cell coordinates.
- No feature that duplicates Editor behavior across this boundary is
  added until a public Zed API exists for it.

`OpenDocument` holds only the Buffer and a scratch label; path, disk
state, dirty, and conflict derive from Zed's `Buffer::file()` every time.
The nearest existing non-root directory becomes an invisible worktree, so
the same Buffer Entity and its new path are tracked after an external
rename. A clean external delete is `is_dirty=false` in Zed, but
`DiskState::Deleted` is explicitly included in close/quit protection and
the `!` indicator.

Headless regression tests confirm the path/dedupe behavior after renames,
editing and saving to the new path, non-recreation of the old path, and
text retention plus discard protection after deletes — with the real
filesystem watcher involved.

### G2. Data and terminal lifecycle are safe

Pass (2026-08-23).

- Open/save/save-as of existing/new/scratch files, close/quit protection
  for dirty tabs, and auto reload plus conflict protection for external
  disk changes are in place.
- Reload and replace are Zed transactions and can be undone.
- Raw mode, the alternate screen, mouse capture, and bracketed paste are
  restored on normal exit and error paths.
- Protection confirmations clear the moment any other input arrives; no
  stale confirmation state is kept invisibly.

Catchable `SIGTERM` / `SIGHUP` update only an atomic flag from the signal
handler, and the terminal reader routes them into the normal event-loop
shutdown. After the reader stops, raw mode and the rest are restored
before the handlers are removed. Exact termios equality before and after
startup was pinned for both signals in the G4 actual-binary PTY tests. On
all paths — normal exit, dirty discard, `SIGTERM`, `SIGHUP` — the
alternate screen, mouse capture, bracketed paste, and cursor state are
restored too. `SIGKILL` and machine crashes cannot be cleaned up by the
process and are out of scope.

The actual-binary tests also confirm, black-box: exact file bytes saved
through Zed `BufferStore`, dirty-quit protection, and retention of text,
dirty state, and the original disk bytes after an `ENOTDIR` save failure
induced by making a path component a regular file.

Additionally, the pinned Zed revision's `RealFs::save` truncates the
existing file and then streams the Rope, with no atomic rename or
`fsync`. Not a regression zec introduced, but it remains a production risk
of partial disk contents on power loss or a mid-write error. A headless
failure test that replaces Zed's save destination with a directory pinned
that the Buffer text, dirty state, and the preserved disk original
survive the error; the G4 actual-binary test pins the same protection.
PoC graduation does not bypass Zed's save path. Atomic/durable saving is
a separate production-readiness blocker; saving is not reimplemented on
the zec side without an upstream Zed fix or an explicit fork.

### G3. Per-frame work is bounded by the viewport

Pass (2026-08-23).

Inside Ratatui's `try_draw` callback the real frame area is obtained,
cursor follow and viewport clamping are settled first, and only the
display rows needed for that frame are then fetched from Zed. Text, row
info, and syntax chunks are limited to `[top_row, top_row + body_height)`,
and background highlights are queried only for Anchors in the same range.
Immediately after a resize, a snapshot captured at the old height is
never drawn into the new frame.

`RenderSnapshot` holds the global `first_row` / `total_rows` / cursor
position and visible-row-only `lines` / `line_numbers` / `line_styles`.
The renderer, mouse hit test, selection, and background convert global
rows into row-local vectors. Row info for the status is kept even when
the cursor is off-screen.

A headless test with an 80x24 terminal and the 100,000-line fixture pins
that only 23 body rows are held at the top, middle, and end, while
`total_rows` represents the whole document. The terminal event channel
has capacity 1; duplicate Redraws coalesce via `try_send`, and input
applies backpressure on the reader thread.

Known constraints: a giant single display line, the full-text search on
each query change, and Zed search navigation proportional to the match
count. If needed, a visible-column API or a search hook will be proposed
upstream.

### G4. The real binary has automated PTY acceptance tests

Pass (2026-08-23).

`tests/e2e_tui.rs` starts `CARGO_BIN_EXE_zec` directly inside a
controlling PTY, reconstructs the raw ANSI output into a semantic screen
with `vt100`, and verifies these four subprocesses in order:

- Unicode insert, `Ctrl-A` selection replacement, Zed undo, continued
  editing after a PTY resize, Zed `BufferStore` save, exact file bytes,
  dirty-quit protection, and normal exit.
- An `ENOTDIR` save failure raised deterministically by placing a regular
  file mid-path; retention of text, dirty state, and the disk original;
  dirty-discard protection.
- `SIGTERM` and `SIGHUP` to the direct child PID going through zec's
  normal error exit rather than dying to the signal.
- On every path, exact termios equality before and after startup, and
  release of the alternate screen, mouse, bracketed paste, cursor, and
  application modes.

Every wait advances on a screen or raw-output predicate with a deadline;
no fixed sleeps. The parent-side slave FD closes right after spawn, and
EOF is treated as a state that does not race the cleanup bytes. On
timeout or panic, RAII kills/reaps the child and closes the writer,
master, and receiver before joining the reader thread.

### G5. A clean checkout is reproducible

Pass (2026-08-23).

`.github/workflows/ci.yml` runs on `ubuntu-24.04` with Rust 1.97.1 and
the pinned `Cargo.lock`: `cargo fmt --check`,
`cargo build --locked --bin zec`, the unit/headless tests, the
actual-binary PTY acceptance, and the `--smoke` output check. Build,
tests, and smoke share the same `target` with incremental and debug info
disabled. The job timeout is 90 minutes, and the CI-generated disk budget
of 14 GiB is checked inside the workflow.

Measurements at graduation:

- On a fresh local target, the build took 3 min 56 s, the post-test
  target was 4,827,824 KiB, and the binary 446,036,048 bytes. All 94
  tests — 93 unit/headless plus 1 PTY acceptance — and the `--smoke`
  output check passed.
- An independent clean checkout of commit
  `d32ddfbe2475c2d1eb2abb42a661cc91e7cacd4d` ran the same command set
  from a cold dependency cache in an official `ubuntu:24.04` container.
  The build took 8 min 14 s; target 4,829,120 KiB, Cargo git cache
  1,134,968 KiB, Cargo registry cache 751,928 KiB, 6,716,016 KiB
  (~6.41 GiB) total. All 94 tests and smoke passed within the 14 GiB
  budget. No source diff or container residue remained afterwards.
- The first push to the private repository ran
  [run 32623267464](https://github.com/neguse/zec/actions/runs/32623267464)
  on GitHub-hosted `ubuntu-24.04`. The Linux graduation gate took
  12 min 11 s, passing the build, the 94 tests, smoke, and the disk
  budget. CI-generated disk was 7,275 MiB, within the 14 GiB budget.

## Execution order

1. G3 (done 2026-08-23): viewport-bounded capture, bounded redraw, the
   100,000-line structural test.
2. G1 (done 2026-08-23): derive file identity from the Zed Buffer and pin
   protection across external rename/delete.
   G2 (done 2026-08-23): auto-verify the data guards and catchable
   signals over an actual-binary PTY.
3. G4 (done 2026-08-23): add the actual-binary PTY integration harness
   and the core acceptance matrix.
4. G5 (done 2026-08-23): reproduce the same matrix on pinned clean Linux
   CI with a shared target and disk budget.
5. Pass every gate in one verification and change the status to
   `Graduated` (done 2026-08-23).

After graduation, feature development proceeds as the sustained editor while
the graduation gates are maintained.
