# Alpha 1 contract: repository editing loop

Contract status: Accepted (2026-08-23)

Gate status is never stored; it derives from A6's candidate/promotion rules
and the evidence checks. Only when a promotion `P` is an evidence-only
direct child does `P` inherit the Single decision rule result of its parent
candidate `C`.

## Outcome

Alpha 1 is the state where, on Linux, starting from `zec DIRECTORY` and
without enumerating target files as CLI arguments in advance, one can
complete file discovery inside the repository, project-wide search, editing
multiple files, creating a new file, saving, exiting, restarting, and
reopening.

Feature counts and human dogfooding hours play no part in the judgment.
Only actual-binary PTY tests against a fixed fixture, an exact filesystem
manifest, latency/RSS assertions, and GitHub-hosted CI exit status decide.

## Single decision rule

Only a candidate commit where the following, run in order on a clean
checkout, all exit 0 on the first attempt is eligible for promotion.

```sh
export LC_ALL=C.UTF-8 LANG=C.UTF-8 TERM=xterm-256color
rustc --edition=2024 src/bin/fixture_repository.rs -o /tmp/zec-alpha-1-fixture-verifier
/tmp/zec-alpha-1-fixture-verifier verify-oracles --repo .
cargo build --locked --release \
  --features e2e-linux \
  --bin zec --bin e2e_repository --bin e2e_repository_bench
cargo test --locked --release --bin zec -- --test-threads=1
cargo test --locked --release --test e2e_tui -- --test-threads=1
timeout --signal=TERM --kill-after=5s 45m \
  ./target/release/e2e_repository --zec ./target/release/zec \
  --repo . --assert --report target/alpha-1/acceptance.json
./target/release/e2e_repository \
  --verify-report target/alpha-1/acceptance.json
timeout --signal=TERM --kill-after=5s 20m \
  ./target/release/e2e_repository_bench --zec ./target/release/zec \
  --assert --report target/alpha-1/benchmark.json
./target/release/e2e_repository_bench \
  --verify-report target/alpha-1/benchmark.json
```

The `Alpha 1 gate` job running the same commands on a GitHub-hosted
`ubuntu-24.04` runner must also be `success`. The job timeout is 90 minutes
with 0 retries. Command timeouts, test retries, manual checks, and reruns
of known flakes do not count as Pass. The run linked from the graduation
record must be a single uncancelled attempt, and any result other than
`success` is Fail. A fix commit uses a new run.

## Fixed test environment

- runner class: GitHub-hosted Ubuntu 24.04 x86_64. The CPU model, core
  count, RAM, kernel, and runner image version are recorded in the report.
  Hardware differences and load are not retry justifications.
- locale / terminal: `C.UTF-8`, `TERM=xterm-256color`, 120x40 cells.
- binary: the same commit's `zec` built with `--release`.
- clock: `CLOCK_MONOTONIC` equivalent. Times are kept as integer
  microseconds, and sample and warm-up counts are fixed per A4 metric.
- PTY parser: `vt100 0.16.2`, pinned by `Cargo.lock`.
- repository fixture: generated from seed `0x5a45435f414c5048`. Excluding
  the root and `.git` internals: 10,000 entries (9,900 regular files, 96
  directories, 4 symlinks), 104,857,600 bytes of UTF-8 text payload in
  total, including a file with 100,000 logical lines and 99,999 LFs.
- The fixture includes space/Unicode paths, `.gitignore`d entries, binaries
  containing NUL, a control file outside the root, symlink aliases, and
  edit targets with UTF-8 BOM, CRLF, and no final newline.
- Regular files edited in Alpha 1 are at most 10 MiB, and one display line
  at most 64 KiB.

The fixture's normative inputs, queries, and expected results live in
`tests/alpha_1/spec-v1.json`. The manifest is JSONL ordered by relative
path in UTF-8 bytes, each record fixed to `path`, `kind`, 4-digit octal
`mode`, `size`, lowercase `content_sha256`, and `symlink_target`. mtime and
inode are excluded, and each record and the file end with LF. The generator
source SHA-256, spec SHA-256, and the before and expected-after manifest
SHA-256 values are recorded in the report.

## Normative oracles and timing

- The contract's oracles are the versioned `spec-v1.json`, the expected
  manifests, and the VT predicates. References to Zed API names and the
  implementation order are non-normative implementation policy.
- The harness updates the VT parser on each completed read, incrementing a
  generation by 1. A frame's arrival time is the read-completion time of
  the first generation matching the given predicate.
- Startup measurement begins immediately before the child spawn call; all
  other measurements begin right after the PTY writer flushes the last
  byte of the operation.
- The Ready predicate requires the alternate screen, 120x40, the fixture
  root label, the expected body sentinel, and a visible cursor in the body
  to coexist in one VT generation.
- p95 is nearest-rank `x[ceil(0.95 * N) - 1]` over ascending raw samples
  `x`; max is `x[N - 1]`.
- One screen-predicate wait times out at 15 seconds, child cleanup at 5
  seconds, and one PTY scenario at 120 seconds; a timeout is always Fail.
- The acceptance report must have `contract_version=1`, no duplicated or
  missing required case ids, `failed=0`, and exact matches of the
  runner/locale/terminal/binary/parser conditions, or the verify command
  exits non-zero.
- An initial state missing a required binary/report/spec is Fail.

## Gates

### A0. PoC regression

The two release-profile test commands pass all 93 unit/headless tests and
the 1 actual-binary PTY test of the existing PoC graduation.
`tests/alpha_1/poc-test-ids-v1.txt` pins the 94 baseline test ids, and the
acceptance verifier confirms the current
`cargo test --release -- --list` contains every id. Additional tests are
allowed; losing an existing case to deletion, ignore, or filtering is
Fail.

### A1. Directory root and file identity

Headless cases of `e2e_repository` assert:

- cwd, `DIRECTORY`, `.`, `..`, absolute path, and symlink alias inputs are
  enumerated literally in `spec-v1.json`.
- A directory is never opened as a file; there is exactly 1 repository
  root id and exactly 1 worktree root id under it, with no duplicated
  per-file worktrees.
- The `buffer_id` and `tab_id` of every alias listed in the spec match
  exactly.
- Dedicated sentinels sit in `.git`, `target`, and ignored entries, and
  the expected quick-open/project-search result JSON for each is empty.
- An outside-root file is opened after resetting the tracing FS, allowing
  only open/stat of the file itself and the bounded metadata
  (`stat-ancestor-git`) that pinned Zed performs for ignore judgment
  against `file/.git` and each ancestor's exact `.git` marker.
  `stat-ancestor-git` includes `file/.git`. `read_dir`, watch/watcher
  add/remove, mutation, and repository operations are forbidden, and the
  `read_dir` call count against the outside parent and siblings is 0.
- Startup receives a normal file and a self-referential symlink together.
  After displaying `ELOOP` it still reaches Ready, edits and saves the
  normal tab's fixed token, and exits with code 0.

### A2. Quick open and project search

The actual-binary PTY cases fix `Ctrl-P = 0x10` as quick open and
`Alt-F = ESC f` as project-wide literal search.

- The selected result for quick-open query `日本 語.rs` and the path after
  Enter match the expected JSON in `spec-v1.json` exactly. Reopening the
  same alias does not increase the tab count.
- Search is case-sensitive, without Unicode normalization, with a result
  limit of 100. Lines are 1-based logical lines split on LF/CRLF; columns
  are 1-based Unicode scalar indices excluding the BOM.
- Result paths, lines, columns, previews, and order match the per-query
  expected JSON exactly, and the caret after Enter lands on the expected
  match start. Restoring the pre-restart cursor is not required.
- `PROBE_EXCLUDED_SENTINEL` is placed in ignored, binary, and outside
  files, and exactly one in-scope control file carrying the same sentinel
  is asserted to return.
- Esc during a search removes the prompt, a different query produces the
  new query and its expected results, and `Ctrl-Q` collects the child —
  each observed within 15 seconds, with no stale results drawn or applied
  afterwards.

The controlled headless case uses a deterministic scheduler that swaps
only completion order into the production commands/reducer. Query A is
held, query B completes, then A completes; the publish log must contain
exactly one entry — B's expected result — and the final state must remain
B's. The actual-binary PTY case asserts the wiring of shortcut bytes,
prompt, result selection, cancel, and open.

Fetching file lists, search ranges, and open results from Zed's
worktree/project/buffer APIs — with zec duplicating no text, selection,
undo, or dirty state — is a non-normative implementation note. The
normative Pass/Fail oracles are only the expected JSON and state traces
above.

### A3. Exact multi-file workflow

`e2e_repository` starts the real binary on a controlling PTY and performs
the following through key input alone.

1. Start from the directory, open UTF-8-BOM file A via quick-open, and
   replace a selection.
2. Open CRLF file B from project search and edit another token.
3. Open no-final-newline file C via quick-open and edit the end of a line.
4. Create a scratch tab and Save As under the root as file D with a
   Unicode path/text.
5. Save A/B/C/D and confirm the tab status shows 0 dirty/conflict/deleted
   markers.
6. After exit, the full manifest matches the expected manifest exactly,
   with not a single path, mode, or byte changed outside the edit
   targets.
7. Start a fresh second process on the same directory and reopen A/B/C/D
   through PTY input. Carets opened from quick-open/search sit at each
   expected match start and the text equals the expected bytes. Restoring
   the pre-restart cursor/session is not required.
8. Exit code 0; post-exit `tcgetattr` equals the pre-start baseline
   exactly; and the alternate screen, mouse tracking, bracketed paste,
   cursor visibility, and application cursor/keypad modes return to
   baseline.

`A3_WORKFLOW_01` through `A3_WORKFLOW_20` run sequentially with a fresh
fixture, fresh config directory, and fresh process, with 0 retries.
Edit/paste payloads use serially numbered unique tokens, and the sent
token sequence, the on-screen applied sequence, and the expected file
token sequence must match exactly.

### A4. Responsiveness and resource envelope

`e2e_repository_bench --assert` asserts the following with spec-pinned
inputs and VT predicates.

- Directory startup: from just before spawn to the Ready generation. p95
  of 20 fresh launches after 2 warm-ups ≤ 3,000 ms.
- Quick-open: in the index-ready state, from PTY flush to the expected
  selected-result generation. Over 100 spec queries after 10 warm-ups:
  p95 ≤ 150 ms, max ≤ 500 ms.
- Project search: each sample uses a fresh process in the index-ready
  state and sends a never-before-run `ALPHA1_BENCH_SEARCH`. Measured to
  the complete generation with the expected 1,000 hits, 10 samples after
  2 warm-ups, p95 ≤ 5,000 ms. The total hit count is 1,000 and the list
  shown on screen is the spec-pinned first 100.
- In-flight search: query replacement ends at the new query and its
  expected results, cancel at prompt removal, quit at child collection.
  Max ≤ 250 ms over 20 attempts each.
- Editing: insert a serial token plus the spec-pinned
  `payload_suffix = LF` into the 100,000-line file with one paste,
  measured to the generation where the token appears on the expected row
  and the caret has moved to the next row. Over 500 edits after 10
  warm-ups: p95 ≤ 100 ms, max ≤ 500 ms.
- Save: flip 1 byte at a fixed offset of a 5 MiB file just before each
  sample to dirty it; from the `Ctrl-S` flush until both the dirty marker
  disappears and the expected disk bytes are read. Max ≤ 2,000 ms over 10
  samples after 2 warm-ups.
- The zec main process's `/proc/PID/status` `VmHWM` × 1,024 as bytes is ≤
  1,073,741,824, and the descendant process count during the benchmark is
  0. Descendants are tracked by confirming the matching CN_PROC LISTEN
  ACK before child spawn and combining fork/clone events with `/proc`
  children of every TID. Event loss is Fail, and an explicit IGNORE is
  sent after the final drain at shutdown.
- The report's `sent_input_ids`, `applied_input_ids`, and the expected id
  sequence match exactly, with `dropped_count=0` and no reordering.

The benchmark report saves schema version, environment, each raw sample,
warm-up/sample counts, p50/p95/max, VmHWM, and the input id sequences as
JSON. Verify mode recomputes the statistics from raw samples. A single
giant line and extreme full-match counts are excluded from the fixture and
maintained as known constraints.

### A5. Failure and terminal lifecycle

Each case runs with a fresh fixture, fresh config, fresh controlling PTY,
and fresh foreground process group.

- Open failure is `ELOOP` via a self-referential symlink; save failure is
  `ENOTDIR` via Save As to a child path whose parent is a regular file —
  both raised in the actual binary.
- Search failure returns `EIO` from a controlled provider into the
  production reducer. After the error is shown, the existing tab count,
  text, and dirty state match the preceding trace, and editing and saving
  the control tab continues.
- `SIGINT`, `SIGQUIT`, `SIGTERM`, and `SIGHUP` are sent with `killpg` to
  the foreground process group after Ready. Collection within 5 seconds
  with normal exit code 0, exact `tcgetattr` match, and baseline
  restoration of the alternate screen, mouse tracking, bracketed paste,
  cursor visibility, and application cursor/keypad modes are asserted.
- `SIGTSTP`: stopped state is confirmed via `waitpid(WUNTRACED)` within 5
  seconds of `killpg`, with the terminal restored to baseline at that
  point. After `SIGCONT`, Ready returns within 15 seconds, and the run
  completes through editing and saving the fixed token and exiting with
  code 0 via `Ctrl-Q`.
- Each child is collected via `try_wait` within 5 seconds and the reader
  thread joins within 5 seconds. After exit the process group has no
  descendant PIDs and the harness's `/proc/self/fd` count equals the
  pre-start count.

The open, search, and save failures and the INT, QUIT, TERM, HUP, and
TSTP/CONT scenarios run 20 times each. The report's required ids are the
186 expansions of `A1_ROOT_IDENTITY`, `A1_OUTSIDE_TRACE`,
`A1_PARTIAL_STARTUP`, `A2_QUICK_OPEN`, `A2_PROJECT_SEARCH`,
`A2_STALE_RESULT`, `A3_WORKFLOW_01..20`, `A5_{OPEN,SEARCH,SAVE}_01..20`,
and `A5_{INT,QUIT,TERM,HUP,TSTP_CONT}_01..20`. Every id runs exactly once;
a single missing, duplicated, or failed id fails report verification.

### A6. CI evidence

Pass judgment uses two commits — candidate and promotion — and creates no
self-reference.

1. On candidate commit `C`, complete the push-event `Alpha 1 gate` run `R`
   with 0 retries. `R` uploads the acceptance/benchmark reports as
   artifacts and its job conclusion is `success`.
2. Promotion commit `P`, a direct child of `C`, adds only
   `docs/alpha-1-evidence.json`, recording `C`'s full SHA, `R`'s run/job
   URLs and ids, the run attempt, generator/spec/before/after manifest
   SHA-256 values, report SHA-256 values, the existing test count, and
   the acceptance case count 186.
3. `P`'s `Alpha 1 evidence` job confirms via the GitHub API that
   `R.head_sha == C`, `R.event == push`, `R.run_attempt == 1`, the gate
   job conclusion is `success`, and artifact digests match. It also
   confirms that the set of run ids with the same `workflow_id`,
   `event == push`, and `head_sha == C` is exactly `[R.id]`, rejecting
   redo runs under different ids.
4. The same evidence job confirms `P^ == C`, that the changed paths in
   `C..P` are only the evidence JSON, that acceptance is 186/186, and
   that every benchmark assertion is true. It further confirms its own
   `GITHUB_RUN_ATTEMPT == 1` and that the set of run ids with the same
   `workflow_id`, `event == push`, and `head_sha == P` is only its own
   id.

Only a `success` `Alpha 1 evidence` job on the default branch is the
canonical Pass. Document status, issue checklists, field dogfood reports,
and in-progress workflow URLs play no part.

## Explicit exclusions from the gate

- LSP, completion, diagnostics, go-to-definition, rename
- file tree sidebar, Git UI, integrated terminal/task runner
- plugins, Zed settings/keymap compatibility, session restore, crash
  recovery
- project-wide replace; regex/case/word search options
- mouse drag/double/triple selection, terminal-aware soft wrap
- macOS, Windows, remote filesystems, multi-root workspaces
- package distribution, releases for external users
- atomic/durable saves that survive power loss
- files over 10 MiB, single display lines over 64 KiB, extreme
  all-matching queries

This list defines the scope Alpha 1 does not measure; it is not an
additional subjective judgment.

The non-atomic save path of the pinned Zed revision remains a known
constraint. Save success in Alpha 1 extends to the point where Zed's save
task completes write/close, dirty becomes false, and the expected disk
bytes can be read. `fsync`, atomic rename, and power-loss durability are
not required. Fixtures and dogfood targets are limited to clean
Git-managed source trees.

## Non-normative implementation notes

zec does not bypass Zed with its own text model or `std::fs` saves. The A0
PoC regression gate maintains the existing editing authority boundary, but
this implementation note itself is not a human Pass condition.

1. Before implementing features, add the fixture generator, expected
   manifests, and a failing `e2e_repository`.
2. Unify directory root/file identity.
3. Land quick-open, then project search on the same root model.
4. Turn the exact multi-file/reopen scenario green.
5. Close async cancellation, performance, and signal/job control.
6. Turn the hosted `Alpha 1 gate` green in one attempt, then the
   promotion evidence job green.

The implementation order and the policy of not building other features
first do not affect Pass/Fail.
