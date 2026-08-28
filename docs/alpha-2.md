# Alpha 2 contract: Project-backed language editing loop

Contract status: Accepted (2026-08-26)

Gate status is never stored. It derives from the candidate commit, the
retry-free hosted gate artifacts, and the verification results of the
evidence-only direct child. The canonical evidence rules of Alpha 1 are
inherited, and an Alpha 2 promotion re-verifies Alpha 1.

## Implementation checkpoint (not gate status)

The working candidate as of 2026-08-26 wires a single Zed Project, the
native LanguageRegistry/LspStore, the command palette, completion, hover,
project diagnostics, definition/type-definition/references, project
symbols, back/forward history, editable MultiBuffers,
prepareRename/rename, code actions, and document/range formatting into the
production paths. Settings/keymap precedence and live reload,
format-on-save, WorkspaceEdit file operations, worktree trust,
path/symlink/special-file boundaries, LSP failure/restart/process cleanup,
and large-payload limits are also checked against the actual `zec` binary.

The deterministic fixture LSP starts through normal `rust-analyzer` PATH
discovery. Alongside the integration tests there is a 341-case acceptance
harness — the language/PTY workflow across 20 fresh processes plus 15
failure scenarios at 20 fresh processes each — and a benchmark harness
that stores raw samples, correlation ids, and VmHWM. A development run
with shortened samples has completed, but that is not a canonical Pass
declaration for B0–B9. Until the retry-free hosted candidate artifacts and
the evidence-only promotion succeed, the ledger stays at `candidate` and
the single decision rule below is not shortened.

## Outcome

Alpha 2 is the state where a fresh Rust repository on Linux, opened with
`zec DIRECTORY`, can complete command discovery, completion, diagnostics,
hover, definition/reference navigation, code actions, rename, formatting,
and multi-file saves entirely from the console, through Zed's local
`Project`, settings, LanguageRegistry, and LspStore.

No external language server is fetched from the network; the gate-built
deterministic fixture server is selected by production PATH discovery. No
test-only provider is injected into zec; the running binary uses the
normal Project/LSP paths.

## Single decision rule

A candidate must run the following commands in order on a clean checkout,
all exiting 0 on the first attempt. The concrete binary names and
arguments are pinned to the same commit as the fixture/harness
implementation, and Alpha 2 must not be promoted until they are appended
to this section.

```sh
export LC_ALL=C.UTF-8 LANG=C.UTF-8 TERM=xterm-256color
rustc --edition=2024 src/bin/fixture_repository.rs -o /tmp/zec-alpha-1-fixture-verifier
/tmp/zec-alpha-1-fixture-verifier verify-oracles --repo .
cargo test --locked --release --features e2e-linux \
  --bin zec -- --test-threads=1
cargo test --locked --release --features e2e-linux \
  --test parity_contract --test e2e_tui \
  --test language_service --test settings_reload --test lsp_failures \
  -- --test-threads=1
cargo build --locked --release --features e2e-linux \
  --bin zec --bin e2e_repository --bin e2e_repository_bench \
  --bin fixture_lsp --bin e2e_language --bin e2e_language_bench
mkdir -p target/alpha-2/alpha-1
timeout --signal=TERM --kill-after=5s 45m \
  ./target/release/e2e_repository --zec ./target/release/zec \
  --repo . --assert --report target/alpha-2/alpha-1/acceptance.json
./target/release/e2e_repository \
  --verify-report target/alpha-2/alpha-1/acceptance.json
timeout --signal=TERM --kill-after=5s 20m \
  ./target/release/e2e_repository_bench --zec ./target/release/zec \
  --assert --report target/alpha-2/alpha-1/benchmark.json
./target/release/e2e_repository_bench \
  --verify-report target/alpha-2/alpha-1/benchmark.json
timeout --signal=TERM --kill-after=5s 45m \
  ./target/release/e2e_language --zec ./target/release/zec \
  --lsp ./target/release/fixture_lsp --assert \
  --report target/alpha-2/acceptance.json
./target/release/e2e_language \
  --verify-report target/alpha-2/acceptance.json
timeout --signal=TERM --kill-after=5s 20m \
  ./target/release/e2e_language_bench --zec ./target/release/zec \
  --lsp ./target/release/fixture_lsp --assert \
  --report target/alpha-2/benchmark.json
./target/release/e2e_language_bench \
  --verify-report target/alpha-2/benchmark.json
(cd target/alpha-2 && find . -type f ! -name SHA256SUMS -print0 \
  | sort -z | xargs -0 sha256sum > SHA256SUMS)
./script/verify-alpha-2-artifact target/alpha-2
```

A state with unimplemented binaries or missing reports is Fail; no
provisional Pass that skips the block above is recognized.

## Fixed environment and fixture LSP

- The Linux runner is GitHub-hosted Ubuntu 24.04 x86_64, locale
  `C.UTF-8`, terminal `xterm-256color` at 120x40.
- The fixture repository contains 8 Rust files, config files,
  definition/reference/rename/code-action/format targets, Unicode
  identifiers, CRLF, same-named symbols, and ignored files.
- The harness links or copies the fixture server as `rust-analyzer` into a
  temporary `bin` directory placed at the head of the child-only `PATH`.
  The user's global Zed/Cargo config is isolated into fresh temporary
  directories.
- The fixture server succeeds at `--help` and speaks LSP 3.17 framing over
  stdio. It deterministically implements initialize capabilities,
  completion, hover, publishDiagnostics, definition, references,
  prepareRename/rename, codeAction, formatting, and shutdown/exit.
- The request/notification log is written by the server itself to
  append-only JSONL. The oracles are the screen, the disk manifest, and
  the server log; zec-internal state is never read through test-only
  APIs.
- Server response order, delays, crashes, and invalid responses are
  controlled by scenario files. No scheduler/provider hook is placed in
  production zec.

## Gates

### B0. Alpha 1 and parity-ledger regression

- Re-run Alpha 1's canonical command set on the same candidate without
  deleting, ignoring, or filtering existing cases.
- Run `tests/parity_contract.rs`, checking consistency of every
  capability id, delivery mode, milestone, evidence path, and the pinned
  Zed revision.
- `PROJECT_SERVICE_GRAPH`, `COMMAND_PALETTE`, `LANGUAGE_INTELLIGENCE`,
  `LANGUAGE_MULTIBUFFER`, and `SETTINGS_KEYMAP` change to `verified` only
  at the Alpha 2 promotion.

### B1. Project is the sole project-service authority

Controlled headless cases and actual-binary traces assert:

- Exactly one Zed `Project` Entity per repository session.
- WorktreeStore, BufferStore, LspStore, GitStore, TaskStore, DapStore,
  SettingsObserver, and ToolchainStore are obtained from that Project;
  zec never creates parallel stores for the same repository.
- Quick Open, project search, and open/save/reload use the same
  Project-owned Worktree/Buffer identity, preserving Alpha 1's alias
  dedupe and outside-file boundary.
- After a scratch Save As, the same Buffer Entity, Editor, selection, and
  undo history are preserved.
- Within 5 seconds of the Project drop, the fixture LSP, watchers, and
  background tasks are collected; descendant processes and the harness fd
  count return to baseline.

Static type names alone are not the oracle; identity, request traces, and
external effects are verified.

### B2. Settings, language registration, and command discovery

- Global settings, the repository's `.zed/settings.json`, and language
  overrides are read through the Zed SettingsObserver path, with
  precedence matching the fixture JSON exactly.
- Settings changes are detected at runtime; completion enable/disable,
  format-on-save, and tab size apply without a restart.
- Beyond the built-in native languages, first-line/shebang, file
  associations, and injections resolve through the Zed registry.
- `Ctrl-Shift-P` opens the command palette, deterministically filtering to
  actions available in the current focus. It carries action ids, display
  names, key bindings, and enabled state, and never shows an unwired
  action as successful.
- The user keymap can rebind the command palette, LSP actions, and
  existing editing actions, resolving precedence against the default
  keymap in the same key contexts as Zed.

### B3. Completion, hover, and diagnostics

The actual-binary PTY case runs the following in 20 fresh processes.

1. Explicitly trigger completion at a marker position and show every
   fixture-server item in a picker ordered by label/detail/kind.
2. Operate filtering, up/down movement, documentation display, cancel,
   and commit, applying text edits and additional text edits as Zed
   Editor transactions. One `Ctrl-Z` returns to the pre-commit state.
3. With request A delayed, change position/query and complete request B;
   returning A afterwards leaves only B's popup and Buffer.
4. Open hover, showing plain text and Markdown in the terminal-adapted
   view; links appear as hyperlinks only where OSC 8 is available, with
   the URL text still present otherwise.
5. Draw warning/error/hint diagnostics with underlines or explicit
   markers, and operate the status summary, next/previous, detail
   overlay, and project diagnostics list. Stale diagnostics disappear
   after fixes.

LSP UTF-16 positions versus Zed Buffer/Display versus terminal
grapheme/cell conversions are verified with Unicode fixtures; byte
columns, Unicode scalars, and UTF-16 units are never confused.

### B4. Semantic navigation and editable MultiBuffer

- Definition, type definition, references, and project symbols run from
  both the command palette and the default key bindings.
- A single target navigates within the same or another file, and
  back/forward history returns to the original selection and viewport.
- Multiple targets, references, and project diagnostics display as Zed
  MultiBuffer excerpts in one item, rendering excerpt headers, paths,
  context, cursor, and selection without duplicating source Buffer
  contents.
- Edits, undo, and save inside a MultiBuffer propagate to all source
  Buffers, with the same dirty/conflict protection as Alpha 1.
- Late-arriving results are never applied to a closed item, another pane,
  or another project.

### B5. Rename, code actions, and formatting

- The prepareRename placeholder appears in the prompt; invalid names,
  errors, and cancel leave the text unchanged.
- The text edits a rename WorkspaceEdit makes across 3 existing files are
  shown in a preview MultiBuffer; accept applies, reject changes nothing,
  and undo after application restores every Buffer consistently.
- WorkspaceEdits containing create/rename/delete file operations show
  their target paths and effects up front and reach the Project API only
  after explicit accept. Outside-repository targets, symlink escapes, and
  special files are refused.
- The code action picker shows kind/preferred/disabled reasons and
  distinguishes edit and command success/failure.
- Document formatting and range formatting run; format-on-save executes
  at most once per save request, and on formatter failure the dirty text
  is kept and the save failure is shown.
- After rename, code actions, and formatting, the exact manifest, the LSP
  request log, and undo/redo results match the oracles.

### B6. Overlay, focus, and input routing

The command palette, completion, hover, diagnostic detail, code action,
and rename prompts use a common overlay stack.

- Only the topmost overlay receives key/paste/mouse input; nothing leaks
  into body shortcuts.
- `Esc` closes only the topmost overlay, unwinding nested overlays in
  order. Tab/pane switches, resize, and LSP redraws never steal focus.
- Every key chord a terminal cannot distinguish has exactly one portable
  binding.
- Where the Kitty keyboard protocol or modifyOtherKeys is available,
  extended keys are used; on unsupported terminals the fallback is shown
  in the status after capability negotiation.
- Overlay results, errors, and cancels carry a session id and generation,
  and stale completions into a closed session are dropped.

### B7. Failure, security, and process lifecycle

Each scenario runs in 20 fresh processes.

- Server not found, spawn failure, initialize error, malformed frames,
  request errors, unexpected EOF, crash, hang, and restart are induced.
- After an error, the existing Buffers, selection, undo, and dirty state
  are preserved, and LSP-less editing, saving, and exiting continue.
- Request cancel during a hang, tab close, project close, and quit reach
  the UI within 250 ms; on quit the child is terminated then killed and
  collected within 5 seconds.
- Workspace edits, commands, and document links are checked against
  worktree trust and path boundaries; no unapproved outside-repository
  mutation or process start occurs.
- Giant server stderr output, 10,000 diagnostics/items, and 64 KiB
  documentation keep queues and render memory bounded.

### B8. Responsiveness and resource envelope

Measured with the fixture server's response delay at 0:

- Directory Ready with the Project added: Alpha 1's startup p95 ≤
  3,000 ms is maintained.
- LSP initialize to the language-ready indicator: p95 ≤ 1,500 ms, max ≤
  3,000 ms over 20 samples.
- Completion request flush to popup complete: p95 ≤ 100 ms, max ≤ 250 ms
  over 100 samples after 10 warm-ups.
- Diagnostics publish to visible marker: p95 ≤ 100 ms, max ≤ 250 ms over
  100 samples.
- Definition/reference response to target/MultiBuffer visible: p95 ≤
  150 ms, max ≤ 500 ms over 100 samples each.
- 3-file rename preview to apply/save completion: p95 ≤ 500 ms, max ≤
  1,500 ms over 20 samples.
- Even receiving 10,000 completion items and diagnostics, the zec main
  process VmHWM is ≤ 1,610,612,736 bytes, and the render snapshot stays
  bounded to the viewport and visible popup rows.

The report retains raw samples, nearest-rank statistics, VmHWM,
request/response id sequences, and input/apply id sequences, and verify
mode recomputes them. No retries, no warm-cache sample substitution, no
outlier exclusion.

### B9. CI evidence

The same candidate `C` / evidence-only promotion `P` scheme as Alpha 1.

1. On `C`'s push event, the `Alpha 2 gate` succeeds at run attempt 1 with
   0 retries.
2. Acceptance, benchmark, the fixture server log, before/after manifests,
   and the environment are saved as artifacts.
3. `P` is a direct child of `C` adding only `docs/alpha-2-evidence.json`.
4. The evidence job verifies the SHA, event, attempt, unique run id, job
   conclusion, artifact id/digest, report digests, required case ids,
   every assertion, `P^ == C`, and the changed paths — from both the
   GitHub API and the local checkout.
5. Only a `success` `Alpha 2 evidence` job on the default branch is the
   canonical Pass.

The `docs/alpha-2-evidence.json` added by the promotion commit is fixed to
the following shape. The 4 report digests are computed from the candidate
artifacts, and the `gate` and `artifact` coordinates come from the same
retry-free run.

```json
{
  "contract_version": 2,
  "candidate_sha": "40 hexadecimal digits",
  "gate": {
    "run_id": 0,
    "job_id": 0,
    "run_attempt": 1,
    "run_url": "https://github.com/OWNER/REPO/actions/runs/RUN_ID",
    "job_url": "https://github.com/OWNER/REPO/actions/runs/RUN_ID/job/JOB_ID"
  },
  "artifact": {
    "id": 0,
    "digest": "64 hexadecimal digits"
  },
  "reports": {
    "acceptance_sha256": "64 hexadecimal digits",
    "benchmark_sha256": "64 hexadecimal digits",
    "e2e_repository_sha256": "64 hexadecimal digits",
    "e2e_repository_benchmark_sha256": "64 hexadecimal digits"
  },
  "counts": {
    "acceptance_case_count": 341,
    "failure_scenario_count": 15,
    "fresh_process_runs": 20
  }
}
```

`script/verify-alpha-2-evidence` independently re-verifies the
direct-parent relation, the changed paths, run/job/artifact uniqueness and
digests, every case id, the raw benchmark statistics, correlation
sequences, every embedded evidence file, and the Alpha 1 re-run reports.

## Explicit exclusions from Alpha 2 gate

The following are not excluded from long-term parity scope; they are
measured by later milestones.

- full project panel, pane split/dock, tab preview/pin/reorder, session
  restore
- regex/path-filter project replace and the general search MultiBuffer
- Git UI, integrated terminal, tasks, debugger, REPL/notebook
- extension gallery, theme/icon, package/update, remote development
- AI, ACP/MCP, edit prediction, collaboration, media bridge
- terminal images, terminal-aware soft wrap, full mouse multi-selection
- macOS actual-binary suites and per-platform packages

These may be built during Alpha 2, but they do not substitute for the
B0–B9 Pass conditions.

## Implementation order

1. Add the parity ledger and failing contract tests.
2. Move the current `FileServices` onto Zed `Project`-owned stores and
   turn Alpha 1 green again.
3. Wire full language/settings initialization and the fixture LSP.
4. Build the action registry, the command palette, and the common overlay
   stack.
5. Wire completion/hover/diagnostics.
6. Wire navigation, MultiBuffers, and rename/code actions/formatting.
7. Close the failure/process/security/performance suites.
8. Pass the hosted candidate and the evidence-only promotion without
   retries.
