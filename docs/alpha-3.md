# Alpha 3 contract: Terminal Workspace

Contract status: Implemented candidate; hosted evidence pending (2026-08-26)

Alpha 3 migrates the single-pane event loop of Alpha 2 into a Terminal
Workspace that centrally manages panes, tabs, docks, panels, overlays,
focus, and navigation history. Zed's Buffer, Editor, Project, and
MultiBuffer remain the authorities; the terminal side owns only layout and
presentation.

Until a hosted candidate and its evidence-only direct child satisfy the
case ids, sample counts, limits, and artifact rules in this document, the
Alpha 3 capabilities in the parity ledger stay at `candidate`.

The implemented candidate includes `e2e_workspace`, `e2e_workspace_bench`,
the four named integration targets, the prior-stage regression runner, the
artifact/evidence verifiers, and the `Alpha 3` workflow. The development
1-run actual-binary PTY matrix passes 19/19 and the reduced performance
matrix completes all 7 metrics. Reports produced with
`ZEC_E2E_DEV_RUNS` / `ZEC_E2E_DEV_SAMPLES`, however, are rejected by the
verifier as canonical evidence. Promotion from `candidate` to `verified`
requires running the 361 cases and the fixed-scale benchmark exactly as
this document specifies — with no environment-variable shortening — on a
hosted runner, and verifying the promotion commit.

## Outcome

Alpha 3 is the state where a fresh repository opened with `zec DIRECTORY`
can complete, from the console alone:

- horizontal/vertical pane splits, directional focus, item movement, ratio
  changes, and tab preview/pin/reorder
- left/right/bottom docks with the project/outline/diagnostics panels
- open/create/rename/delete/copy from the project tree, conflict
  confirmation, and trust/path boundaries
- project search with regex/case/whole-word/include/exclude, the preview
  MultiBuffer, and atomic replace
- outline, breadcrumbs, go-to-symbol, back/forward, and navigation that
  preserves Buffer identity
- folds, soft wrap, multiple selections, mouse drag/double/triple click,
  and scroll/cursor projection
- workspace/session restore after clean exit and after a crash,
  quarantine of corrupted sessions, and unsaved recovery
- terminal capability detection with explicit fallbacks for
  indistinguishable key/mouse/focus events

## Single decision rule

The candidate must run the following once, in order, on a clean checkout
of a GitHub-hosted Ubuntu 24.04 x86_64 runner, with everything exiting 0.
Retries, case filters, sample exclusion, and manual-inspection substitutes
for Pass are not recognized.

```sh
export LC_ALL=C.UTF-8 LANG=C.UTF-8 TERM=xterm-256color
cargo metadata --locked --format-version 1 --no-deps >/dev/null
cargo fmt --all -- --check
cargo test --locked --release --features e2e-linux \
  --bin zec -- --test-threads=1
cargo test --locked --release --features e2e-linux \
  --test parity_contract --test e2e_tui \
  --test language_service --test settings_reload --test lsp_failures \
  --test workspace_layout --test project_panel \
  --test workspace_search --test workspace_sessions -- --test-threads=1
cargo build --locked --release --features e2e-linux \
  --bin zec --bin e2e_repository --bin e2e_repository_bench \
  --bin fixture_lsp --bin e2e_language --bin e2e_language_bench \
  --bin e2e_workspace --bin e2e_workspace_bench
./script/run-alpha-1-and-2-gates target/alpha-3/regression
timeout --signal=TERM --kill-after=5s 60m \
  ./target/release/e2e_workspace --zec ./target/release/zec \
  --assert --report target/alpha-3/acceptance.json
./target/release/e2e_workspace \
  --verify-report target/alpha-3/acceptance.json
timeout --signal=TERM --kill-after=5s 20m \
  ./target/release/e2e_workspace_bench --zec ./target/release/zec \
  --assert --report target/alpha-3/benchmark.json
./target/release/e2e_workspace_bench \
  --verify-report target/alpha-3/benchmark.json
(cd target/alpha-3 && find . -type f ! -name SHA256SUMS -print0 \
  | sort -z | xargs -0 sha256sum > SHA256SUMS)
./script/verify-alpha-3-artifact target/alpha-3
```

If even one binary, script, report, case, or artifact is missing, the
result is Fail. This block must not be replaced with shortened
work-in-progress commands.

## C0. Prior milestone regression and ledger

- Re-run Alpha 1's 186 acceptance cases, its benchmark, and the 94 PoC
  ids with the same zec binary.
- Re-run Alpha 2's 341 acceptance cases, its benchmark, and the 34+5
  embedded evidence files with the same zec binary.
- Verify the Alpha 1/2 canonical evidence files and advance the earlier
  capabilities to `verified` in the ledger.
- Pin the Zed revision, capability ids, delivery modes, milestones, and
  evidence paths with the parity test.

## C1. One Terminal Workspace authority

A single `WorkspaceModel` reducer owns panes, tabs, docks, panels,
overlays, focus, and navigation history.

- Every Zed Editor/MultiBuffer item gets an item identity unique within
  the process and stable across the session.
- The layout is held as a binary split tree, panes as ordered item lists,
  docks as ordered panel lists.
- The same item is never registered into multiple panes; opening an
  existing item focuses the existing identity.
- Closing a pane collapses the split tree and always moves active
  pane/item/focus to a valid target.
- Overlays form a LIFO stack; only the topmost receives input, and close
  returns to the overlay below or the original focus.
- After every reducer transaction the invariants are checkable: layout
  leaves, the pane map, the item set, focus, and preview/pin.
- The renderer and the session writer read only immutable workspace
  snapshots and never mutate state mid-frame.

Zed Buffer/Editor text, selection, undo, and DisplayMap must not be
duplicated into the workspace model.

## C2. Pane, tabs, docks, and responsive rendering

- Horizontal/vertical splits, directional focus, moving the active item
  to an adjacent pane, and split-ratio changes are provided.
- Ratios survive terminal resize, 0-cell panes are never created, and
  when too narrow the active pane wins an explicit degradation.
- Tabs have preview, pin, close, reorder, and next/previous. A dirty
  preview is promoted to pinned instead of being auto-replaced.
- Left/right/bottom docks retain visibility, cell size, and the active
  panel, restoring the pre-toggle size.
- Editor panes, the tab strip, docks, status, and overlays never overlap,
  and the cursor appears only in the focused Editor.
- Split/dock boundaries can be mouse-dragged; without mouse support the
  same operations work through the command palette and key bindings.
- With 4 panes, 20 tabs each, and 100k-line buffers, capture stays
  bounded to the visible viewport.

## C3. Project panel and file operations

The Project panel takes Zed WorktreeStore entry identity and scan updates
as authority.

- Directory expand/collapse, filtering, selection, reveal-active-file,
  and ignored/hidden visibility toggles are provided.
- File open, preview/pin, new file/directory, rename, delete, copy, and
  duplicate operate via keyboard and mouse.
- Create/rename/delete/copy pass through preview and explicit
  confirmation into the Project/Worktree APIs, and the display updates
  from the post-success scan.
- Renaming a dirty/open Buffer keeps the same Buffer identity and undo;
  delete keeps the text and connects to the existing discard guard.
- Outside-repository targets, `..`, absolute escapes, symlink escapes,
  FIFO/socket/device files, and case-fold collisions are refused.
- Mutations and process starts on untrusted worktrees stop at the trust
  prompt; cancel changes neither disk nor Buffers.
- Watcher events arriving mid-sequence for rename/delete/create leave no
  duplicate entries or stale selections.

## C4. Complete project search and replace

The existing literal/case-sensitive search extends to Zed search query
semantics.

- Literal/regex, case-sensitive, whole-word, include glob, exclude glob,
  and open-buffer-only toggle freely.
- Query options and replacements have history; invalid regex/globs show
  inside the prompt without changing the text.
- Results are MultiBuffer excerpts in path/position order, with context
  lines, match highlights, and folding.
- Dirty open Buffers take precedence over disk, same-Buffer aliases
  dedupe, and ignored/binary/special-file rules hold.
- Replace-one, replace-file, and replace-all show a preview diff and
  target counts, applying only after accept as one ProjectTransaction.
- Just before apply, Buffer generations and disk fingerprints are
  re-checked; stale/overlap/conflict aborts the whole application.
- Save, undo, and redo treat all source Buffers consistently, with no
  partial application or silent skips.
- Stale generations after cancel, query changes, or pane/project close
  are neither drawn nor applied.

## C5. Outline, breadcrumbs, and navigation

- The document outline builds from Zed's language outline/symbol
  authority, showing hierarchy, kinds, ranges, and selection ranges.
- Outline filtering, expand/collapse, follow-cursor, and selection
  movement are provided, and stale parse results are dropped.
- Status breadcrumbs elide the worktree path and symbol ancestry to the
  terminal width, and each segment opens a picker.
- Go-to-symbol, definition, type definition, references, diagnostics, and
  search results use a common navigation transaction.
- Back/forward restores pane/item/path/selection/viewport; deleted
  targets skip with a stated reason without corrupting history.
- Items opened by navigation are previews; edited items become pinned;
  the same Buffer is never duplicated.

## C6. Advanced editor presentation and input

- Fold/unfold/toggle/all, fold markers, and cursor reveal use Zed fold
  actions and the DisplayMap.
- Soft wrap passes terminal cell widths into Zed wrap boundaries and
  keeps cell positions correct across wide/combining/emoji graphemes.
- Adding multiple selections/cursors, add-above/below, and
  select-next/all-occurrences run as Zed Editor actions, and every
  cursor renders.
- Mouse drag, double-click word, triple-click line, Shift extend, and
  Ctrl/Alt add-selection are handled according to terminal event
  capability.
- Rectangular selection, inlays, inline diagnostics, indent guides, and
  whitespace rendering project from Zed snapshots into the terminal.
- Kitty keyboard protocol / modifyOtherKeys / focus events / mouse motion
  / OSC 52 / OSC 8 are detected, with availability shown in the status.
- Indistinguishable operations keep a portable route via the command
  palette; input is never silently dropped.

## C7. Session and crash recovery

The session file saves version, repository identity, layout, pane/tab
order, active item, docks, panels, cursor/selection, viewport, folds, and
search/navigation history with atomic writes.

- Restart after a clean quit restores file-backed items and layout
  exactly.
- Scratch/dirty Buffers are saved into content-addressed recovery blobs
  and open as recovered items rather than auto-overwriting their original
  paths.
- After a successful save the corresponding blob is collected, without
  deleting other sessions' or newer generations' blobs.
- Process crash, SIGKILL, and partial session writes are induced by
  fixture, restoring from the last atomic snapshot and the recovery
  journal.
- Schema/version/hash violations, truncated JSON, oversized sessions, and
  symlinked session paths are quarantined; startup proceeds with a fresh
  workspace and shows the error.
- Two processes on the same repository use the lock/generation scheme so
  a later process never destroys the earlier session.
- Renamed/deleted restore targets are tracked by worktree identity, and
  unknown ones are shown explicitly as missing items.

## C8. Failure, security, and lifecycle matrix

The following 9 scenarios run in 20 fresh processes each.

1. corrupt/truncated session
2. project-panel symlink escape
3. file-operation permission denied
4. dirty delete/rename conflict
5. replace fingerprint conflict
6. resize storm from a 1x1 terminal
7. watcher overflow/rescan reorder
8. stale outline/panel/search generation
9. crash during recovery journal commit

Each case preserves existing Buffers/undo/dirty state, the UI responds
again within 250 ms, and children, watchers, the PTY, and lock files are
collected within 5 seconds at exit. Inputs, filesystem mutations, session
writes, and external processes carry monotonic ids, and
request-versus-apply order, loss, and duplication are stored in the
report.

## C9. Acceptance and performance envelope

`e2e_workspace` runs exactly these 361 ids:

- `C1_WORKSPACE_MODEL`, 1 case
- `C2_LAYOUT_*`, `C2_TABS_*`, `C3_PROJECT_PANEL_*`,
  `C4_SEARCH_REPLACE_*`, `C5_NAV_OUTLINE_*`, `C6_ADVANCED_EDITOR_*`,
  `C7_SESSION_RESTORE_*`, `C7_CRASH_RECOVERY_*`,
  `C8_CAPABILITY_FALLBACK_*`, 20 cases each
- the 9 failure scenarios as `C8_<SCENARIO>_*`, 20 cases each

The benchmark, on the release actual binary with a 120x40 PTY and raw
samples that exclude no warmup, must satisfy:

- 4-pane redraw: 100 samples after 10 warm-ups, p95 ≤ 16 ms, max ≤ 50 ms
- project panel initial ready (10,000 entries): 20 samples, p95 ≤ 750 ms,
  max ≤ 1,500 ms
- outline update (10,000 symbols): 100 samples, p95 ≤ 100 ms, max ≤
  250 ms
- regex search first visible result (10,000 files): 100 samples, p95 ≤
  150 ms, max ≤ 500 ms
- 1,000-file replace preview: 20 samples, p95 ≤ 750 ms, max ≤ 2,000 ms
- back/forward apply: 500 samples after 10 warm-ups, p95 ≤ 16 ms, max ≤
  50 ms
- 20-pane/session restore: 20 samples, p95 ≤ 1,500 ms, max ≤ 3,000 ms
- VmHWM ≤ 1,879,048,192 bytes in each independent scenario: 100k lines ×
  4 panes, a 10,000-entry tree, 10,000 results + MultiBuffer, and 10,000
  symbols

The report verifier recomputes nearest-rank statistics, raw sample
counts, every case id, every correlation sequence, binary SHAs, the
environment, and the manifest/session/event traces inside the artifact.

The `e2e_workspace_bench` workload is also stored in the report as fixed
values; verification fails if any of the 10,000 tree files, 10,000
outline symbols, 1,000 replacement sources, the 100,000-line source, or
the 20-pane session is smaller. During narrow-width 4-pane degradation,
judgments rely not on long status text but on dock appearance/removal,
real excerpts, and total search counts read from the VT screen. A
measurement sample spans from writing the input to the PTY reader
completing the production frame containing its result. Regex latency is
measured with a regex that scans 10,000 files and matches exactly one;
the capacity scenario verifies, in a separate actual-binary process, the
completion of `1/10000` and the MultiBuffer built from all its results.

## C10. Hosted evidence

The same candidate `C` / promotion `P` rules as Alpha 1/2.

1. The `Alpha 3 gate` on `C` pushed to the default branch succeeds at run
   attempt 1 with 0 retries.
2. The 4 regression reports, Alpha 3 acceptance/benchmark, and the
   PTY/event/session/manifests are stored in one artifact.
3. `P` is a direct child of `C` adding only `docs/alpha-3-evidence.json`.
4. The evidence job reads back run/job/artifact uniqueness, SHA, event,
   attempt, conclusion, and digests from the GitHub API.
5. The downloaded artifact is re-verified with `verify-alpha-3-artifact`,
   and only the evidence job's success on the default branch is Pass.

The artifact has this self-contained layout:

```text
alpha-3/
  acceptance.json
  acceptance-artifacts/{event-trace,manifest,session-trace}.json
  benchmark.json
  benchmark-artifacts/{latency-trace,memory-observations,session-generation,workload-manifest}.json
  regression/alpha-2/
    acceptance.json
    benchmark.json
    alpha-1/{acceptance,benchmark}.json
    ... Alpha 1/2 embedded evidence and SHA256SUMS
  SHA256SUMS
```

The root verifier recomputes size/SHA-256 of every embedded file, the
order of the 361 cases, the 860 benchmark correlation ids, nearest-rank
statistics, and agreement of every binary digest, and has the nested
Alpha 2 verifier re-verify the prior-stage artifact.

## Explicit exclusions from Alpha 3 gate

The following are not excluded from parity scope; they move to later
milestones.

- Git staging/commit/branch/remote UI, integrated terminal, tasks/tests
- DAP debugger, REPL/notebook, process/session recovery
- extension/theme/icon/package/update, remote development, platform
  packages
- AI/ACP/MCP/edit prediction, collaboration, voice/screen-share bridge
