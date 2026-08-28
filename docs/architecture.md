# Architecture decisions

Status: Accepted (2026-08-21)

## Goal

Use Zed's `editor::Editor` as the body of the editing feature set and add
only terminal-specific input and output. Editor, Buffer, selection, undo,
and the keymap are not reimplemented.

Beyond the headless insert/undo PoC, the following are implemented: plain
text rendering in the terminal, key input, movement, undo/redo, selection
display, logical line numbers, syntax highlighting for the native language
set, paste, resize, terminal viewport scrolling, caret movement by mouse
click in the body, repository mode rooted at the current directory or an
explicit `DIRECTORY`, file discovery through one Zed Worktree and the
`RepositoryIndex`, `Ctrl-P` Quick Open, `Alt-F` project-wide search,
open/save/save-as of real files, `Ctrl-F` in-buffer search, go to
line/column, the terminal clipboard, dirty indicators, multiple tabs,
opening/closing tabs at runtime, and terminal restoration on exit.

## Repository strategy

Rather than forking the Zed monorepo, an independent binary crate consumes
Zed's crates as Git dependencies. The revision is pinned in `Cargo.toml` so
the verification target cannot drift. No fork or upstream modification is
kept until a private Zed API becomes genuinely necessary.

Repository mode creates exactly one visible Zed Worktree matching the
canonicalized `Directory root`. An immutable `RepositoryIndex` is built
from the Worktree snapshot after the scan completes; zec has no filesystem
walk of its own and no per-file worktrees. Zed's ignore/external logic and
canonical file identity are used, so symlink aliases collapse onto the same
file. Direct `FILE...` mode remains an independent startup path.

## Decision

The structure is as follows.

```text
crossterm Event
    -> channel
    -> GPUI foreground
    -> Zed Editor
    -> DisplaySnapshot
    -> zec's Ratatui Widget
    -> ratatui::Buffer
    -> CrosstermBackend
    -> terminal

DIRECTORY (current directory when no args)
    -> canonical RepositoryRoot
    -> Zed RealFs
    -> 1 visible Worktree
    -> immutable RepositoryIndex
       -> Ctrl-P Quick Open -> ProjectPath
       -> Alt-F -> Zed Search -> Buffer + Anchor range
    -> shared BufferStore
    -> Zed Buffer × N

FILE... (direct mode)
    -> Zed RealFs -> WorktreeStore -> shared BufferStore

Zed Buffer × N
    -> hidden Editor window × N
    -> tree-sitter parse / highlighted chunks
```

| Component | Responsibility |
| --- | --- |
| Zed Editor / Buffer | text, cursor, selection, edit actions, undo, dirty state |
| Zed Search / Buffer / Anchor | project-wide match discovery, text, match ranges |
| Zed BufferStore / WorktreeStore / RealFs | repository file set and ignore logic, open/save, encoding, line endings, disk state |
| Zed Language / tree-sitter | language queries, parsing, syntax highlight ranges |
| GPUI headless | Zed's runtime and window/action context |
| Crossterm | raw mode; key, paste, and resize input; terminal output |
| Ratatui | layout, cell buffer, styles, differential rendering |
| zec RepositoryRoot / RepositoryIndex | root identity, alias dedupe, deterministic Quick Open display index |
| zec | terminal event translation, Zed snapshot to cell conversion, wiring |

The source of truth for editing state is always Zed. Terminal input feeds
Zed's action/input paths, and the rendering side only reads Zed's immutable
display snapshots.

Ratatui is adopted because it already provides the cell buffer, styles,
layout, and frame-to-frame differential rendering. It does not own the
input loop, so zec translates events read through Crossterm into Zed input.

## Frame capture and redraw

A normal repaint does not copy the whole document into a terminal snapshot.
Ratatui's `try_draw` callback provides the frame's area; after cursor
follow and clipping to the document end, only the settled
`[top_row, top_row + body_height)` range is read from Zed's
`DisplaySnapshot` inside the same callback. The viewport therefore cannot
shift between resize and capture, and the race that leaves the first frame
empty is avoided.

`RenderSnapshot` keeps text, line numbers, and syntax styles in row-local
vectors starting at `first_row`, while `total_rows`, cursor, selection, and
background ranges use global display rows. Only the renderer and the mouse
hit test convert global rows to local indices. Total row count and the
widest line number come from Zed's summary APIs, visible syntax from
`highlighted_chunks` over the row range, and search-style backgrounds from
`background_highlights_in_range` over visible Anchors. The terminal never
recomputes folds or wraps.

The terminal event channel has capacity 1. Dropping duplicate Redraws is
safe because the next frame reads the latest snapshot; key/paste/mouse
events apply backpressure in the reader thread to preserve order. The
`SIGTERM` / `SIGHUP` handlers only update an async-signal-safe atomic flag;
the terminal reader converts the number into an ordinary `TerminalEvent`
and ends the GPUI loop. Cleanup is fixed to: join the reader thread, let
`TerminalSession` restore raw mode, the alternate screen, mouse capture,
and bracketed paste, then remove the signal handlers. Restoration always
attempts both the escape output and the raw-mode reset even if one fails,
and explicit restore failures are returned to the caller.

In the 100,000-line structural test the retained row count stays at 23 at
the top, middle, and end of an 80x24 terminal. This is an upper bound on
repaint relative to line count, not a claim that full-text search is
constant time. Zed's public text APIs are display-row-based, so a
multi-megabyte single line still materializes the whole line and converts
it to terminal cells. If that ever matters, the plan is to explore
visible-column iterators/hooks on the Zed side rather than a custom text
model.

## Repository root, Quick Open, and Project Search

`zec` with no arguments uses the current directory; `zec DIRECTORY` uses
the explicit directory as the repository root. Root identity is the
canonical path, so `.`, absolute paths, and symlink spellings of the same
root cannot become separate repositories or Worktrees. Only after the Zed
Worktree scan completes is that snapshot projected into the
`RepositoryIndex`. The index is an immutable presentation snapshot holding
relative paths, canonical identity, aliases, and `ProjectPath`s; it owns no
file contents, selection, undo, or dirty state.

`Ctrl-P` Quick Open filters the `RepositoryIndex` and selects from the
first 100 deterministically ranked entries. After `Enter`, the Zed
`ProjectPath` held by the index goes to the `BufferStore`, so the same file
or a symlink alias is never duplicated into another Buffer. Quick Open and
Project Search are repository-mode-only; the direct `zec FILE...`
open/save path remains as is.

`Alt-F` Project Search is a case-sensitive literal search over the
`RepositoryIndex`'s deterministic file set using Zed `SearchQuery`. Worker
count per run clamps GPUI's CPU count to 1–4, and dispatched files are
limited to twice the worker count. The closed-file disk prefilter and
deterministic retirement of no-hit files complete inside fixed background
workers/collector, and only disk hits or open buffers reach the foreground
`AsyncApp`. Open buffers bypass the disk prefilter and search every alias
of the same physical file from the `BufferStore` snapshot; closed-file hits
are opened through the `BufferStore`. Disk checks are an optimization apart
from binary exclusion; final authority is always the Zed `BufferSnapshot`
and `Anchor` ranges. zec projects path, 1-based line, BOM-less Unicode
scalar column, and preview from the snapshot, dedupes by canonical file
identity and range, and orders deterministically. The source retains up to
the exact limits of 5,000 matching files or 10,000 ranges, setting a
lower-bound flag only when the 5,001st file or 10,001st range is observed.
The terminal shows the first 100 results while retaining the total count,
and `Enter` moves the caret to the same retained Buffer and Anchor range.

Every query change bumps a prompt-local generation; the app-wide
coordinator limits concurrent project searches to 2 and pending requests to
the latest 1. Key edits cancel the old run immediately and coalesce behind
a 16 ms trailing debounce; a bracketed paste is an atomic completed query
and becomes eligible immediately. Debounced key queries keep waiting while
a search is running, so intermediate backspace queries during replacement
do not occupy the second slot, and only a completed paste advances
immediately into the reserved second slot.

Cancel closes the user signal and wakes the file-scan, queue-wait, and
snapshot-chunk boundaries. It is separate from the internal stop used for
the source cap, and only user cancel returns as an error. Producers and
fixed workers are joined to natural completion rather than dropped, and the
app-wide coordinator holds the outer Task handle until the finish event.
Supersession by paste, an empty query, `Esc`, and prompt close all
logically cancel the reducer/debounce/pending state without dropping the
in-flight Task. Only app teardown detaches the outer Task after cancel, and
that Task itself completes the worker join.

Each request is identified by the prompt session id and generation, so a
stale completion already sitting in the capacity-1 terminal channel never
publishes success/error into a newly reopened prompt session. Finish events
arriving while no prompt exists are still processed by the coordinator,
which always releases the acknowledged slot.

The repository benchmark drives the actual production binary through a
PTY, walks the VT-parsed status with the down arrow one result at a time,
and compares all of the 100 displayed paths, lines, columns, previews, and
their order directly against the spec. Result lists are never read from
fixture files or test-only interfaces, and the production search path has
no test-only file hooks.

## File I/O

Files chosen from the index in repository mode and `zec FILE...` in direct
mode both open through Zed's `RealFs -> WorktreeStore -> BufferStore`.
Neither a whole `Project` nor hand-written `std::fs::write` is used, which
leaves encoding, BOM, line endings, save versions, and external file state
to Zed's implementation.

When no matching worktree exists, the nearest existing non-root directory
containing the path becomes an invisible worktree. This lets external
renames within the directory be tracked through Zed's entry identity, and
not-yet-created nested Save As paths become tracked after creation. Only
when the sole containing directory is the filesystem root does the file
itself become a single-file worktree instead of scanning/watching the whole
root. Directory worktrees recursively scan and watch their subtree — the
trade for rename tracking is that initial I/O.

Not-yet-created paths also become `DiskState::New` Buffers with files
attached, so `save_buffer` after editing creates them.

`Ctrl-N` scratch buffers are not bare `Buffer::local`s either; they are
created from the start through the same `BufferStore`'s
`create_local_buffer`. `Ctrl-S` passes the absolutized destination from a
terminal-owned one-line prompt to `find_or_create_worktree ->
save_buffer_as`. On success the same Buffer Entity gains a file and
transitions to normal `save_buffer` without rebuilding the Editor,
selection, undo history, or dirty version. Because the LspStore is not
initialized, only the language selection for the post-save-as extension is
explicitly rerun by zec.

Save As relative paths resolve against the startup working directory, with
no shell expansion. An existing regular file warns on the first `Enter` and
overwrites only on a second `Enter` with the same input. Directories,
FIFOs, and other special files are rejected. This confirmation is a UI
boundary against mistakes, not an atomic no-clobber guarantee against
external filesystem races between confirmation and save. Multiple tabs
share one LanguageRegistry, RealFs, WorktreeStore, and BufferStore. Opening
the same ProjectPath dedupes to the same Buffer Entity in the BufferStore,
and identical CLI paths are excluded up front. If a Save As target is
already open as another tab's Buffer, binding two Buffers to one disk path
is explicitly refused.

Zed's default `Ctrl-S` keymap entry is a Workspace action, and this
binary's root is an Editor, so there is no save handler. `Ctrl-S` alone is
therefore captured on the CLI side and calls `BufferStore::save_buffer`.
Editing, undo, and movement continue through Zed's key dispatch. `Ctrl-Q` /
`Ctrl-W` on a dirty or externally deleted state warn on the first press and
discard/close only on the immediately following second press.

## External file changes

Change detection from the `RealFs` watcher through `WorktreeStore`,
`BufferStore`, and `Buffer::file_updated` uses Zed's existing path. Only
`BufferEvent::ReloadNeeded` on a clean Buffer — handled by `Project` in
normal Zed — is forwarded from the tab's subscription to
`BufferStore::reload_buffers`. zec creates no `Project` and reimplements no
reload or diff application beyond this thin glue.

A dirty Buffer is not auto-reloaded; `has_conflict` shows as `!` in the
status. `Ctrl-S` during a conflict warns first and overwrites the disk only
when pressed again. `Ctrl-R` reloads the active file explicitly, requiring
a second press when dirty. Reload uses a Zed transaction that stays in
history, so `Ctrl-Z` immediately afterwards returns to the pre-reload text.

Reload completion wakes the render loop through a terminal event and
recomputes matches when a search is active in the active tab. Completion
events re-resolve the tab by Buffer id, so a tab closed mid-flight never
leaves stale handles or labels referenced. `OpenDocument` caches no file
path/label. Display, save/reload eligibility, error context, and exit
protection derive from the current `Buffer::file()` and `DiskState`, so
after an external rename they follow the same Buffer's new path and label.
An external delete shows `!` even on a clean Buffer and joins the
close/quit discard confirmation. Pressing `Ctrl-S` again recreates the same
path through Zed's normal save path.

## Syntax highlighting

The native parsers bundled in Zed's `grammars` crate are registered
directly into the `LanguageRegistry`. In addition to the 20 pairs from
`native_grammars()`, JavaScript (sharing the TSX parser) and Zed Keybind
Context (sharing the Rust parser) are registered, covering the same 22
bundled language configs/queries as Zed itself. The languages selected for
ordinary files are Shell, C/C++, CSS, Diff, Go/Go Mod/Go Work, JSON/JSONC,
JavaScript/TypeScript/TSX, Markdown, Python, Rust, YAML, and Git Commit;
the rest are mainly hidden languages for injections.

`languages::init` initializes LSP adapters and the Node runtime, so it is
not used at this stage: tree-sitter parsing for many languages happens, but
no LSP or external process starts. Language selection is currently by
extension; shebang-only detection for extensionless scripts, grammars
outside the native set, and injection into unregistered languages are
follow-up work.

Building every language with `Language::new` at startup compiles queries
that are never used and delays the first screen by seconds. Like Zed
itself, native grammars are registered first and each config is registered
as a `LanguageRegistry::register_language` loader; queries load and compile
only when a root language or injection actually requests them. On this
checkout's debug build, the first screen for a Rust file went from about
5.6 s to about 0.6 s, and JavaScript from about 5.4 s to about 1.0 s.

For display, `DisplaySnapshot::highlighted_chunks` is asked for tree-sitter
styling and the theme-resolved styles are converted into per-row
terminal-cell ranges. Ratatui draws base/syntax first, then overlays
selection. 24-bit color, bold, italic, underline, and strikethrough map to
the terminal; expressions the terminal lacks, such as fine-grained font
weights or wavy underlines, are dropped.

Parsing completes asynchronously, so `BufferEvent::Reparsed` is fed back
into the terminal event channel for redraw. Highlights therefore appear
without waiting for an input event.

## Terminal workspace, panels, and recovery

Since the terminal-workspace work, panes, items, docks, panels, overlays,
and focus are owned solely by the `WorkspaceModel`. The layout is a binary
split tree with stable `PaneId` leaves; a pane is an ordered list of stable
`ItemId`s; a dock is an ordered list of `PanelKind`s. Every operation goes
through a reducer, and after each transaction the invariants are checked:
layout leaves match the pane map, items are unique, active/focus are valid,
preview/pin are consistent, and overlay ids are monotonic. The renderer,
mouse hit test, and session writer read the same immutable snapshot, so
there are no separate pane/tab arrays to keep in sync.

Each item's text, selection, folds, wrap, and undo remain the authority of
Zed `Editor` / `Buffer` / `DisplayMap`. A split creates a new item
projecting the same Buffer into another Editor viewport without duplicating
the Buffer Entity. The 4-pane layout, docks, tab strip, and status are
assigned non-overlapping Rects in one layout projection by
`workspace_render`; at narrow widths the active pane wins the degradation.
A keyboard route always exists, and split/dock-boundary drag plus panel-row
hit targets are added only where the mouse is supported.

The Project panel projects stable entry ids from the Zed Worktree snapshot,
and during filtering moves the selection to the first real match rather
than an ancestor. Create/rename/delete/copy re-validate canonical paths,
trust, special files, and symlink/case-fold collisions at preview time and
apply time, and only the post-success scan updates the tree. Deleting a
dirty file keeps the Buffer Entity and connects to the normal discard
guard.

Project Search runs literal/regex, case, word, ignored, open-buffer-only,
and full-path/include/exclude options through one generation-tagged
coordinator. Display virtualizes to the first 100 hits while replace-all
retains `all_matches` up to 10,000 ranges. The preview records every
source's Buffer generation and disk fingerprint; if even one changed just
before accept, the whole application is aborted rather than partially
applied. Search results, references, diagnostics, and application results
render as editable MultiBuffers sharing source Buffers and Anchors.

Outline and diagnostics are persistent right/bottom-dock panels, not
transient popups. Asynchronous outline/LSP completions match source Buffer
id and generation and drop stale results. Panel filtering, folds, soft
wrap, multiple cursors, inlay hints, inline diagnostics, and
indent/whitespace guides are projected from Zed snapshots onto visible
cells only. Terminal width is computed in Unicode grapheme cell widths, and
mouse drag/double/triple clicks invert the same projection back into Zed
selection actions.

A session saves repository identity, workspace, items, docks, navigation,
selection, viewport, and fold/wrap into versioned JSON. Generation files
are immutable, carry the payload SHA-256 in both filename and envelope, and
commit via temporary-file fsync plus atomic rename. Dirty/scratch/
MultiBuffer sources separate into content-addressed blobs, and GC after a
clean save walks every generation in the store to keep reachable blobs.
Restore validates from the newest generation, quarantines
truncated/hash/schema failures into `.rejected-*.session`, and falls back
to the previous generation or a fresh workspace. Multiple processes on the
same repository never overwrite each other's sessions thanks to leases and
monotonic generations.

Kitty keyboard, modifyOtherKeys, SGR/legacy mouse, focus reporting, OSC 52,
and OSC 8 are capability-detected at startup, and the route is shown in the
F4 status. Indistinguishable chords and unsupported mouse operations keep a
command palette/key route rather than silently dropping input.

## Local development services

The Git panel converts Zed `GitStore`'s active repository snapshot into an
immutable projection every frame. Stage/unstage/discard go back to the Zed
APIs for selected paths or the whole repository; zec builds no parallel
model that parses `git status`. The terminal panel holds
`Entity<zed_terminal::Terminal>` values and draws the Zed terminal's cells,
cursor, scroll state, and process status into the dock. The task picker
passes the active worktree/buffer `TaskContexts` to `TaskInventory` and
adds the resolved `SpawnInTerminal` to the same terminal collection through
`Project::create_terminal_task`.

Debug configurations come from the same TaskInventory and only registered
`DapRegistry` adapters are shown. A `DebugTaskDefinition` with relative
paths, Zed task variables, build tasks, and debug locators resolved goes to
`DapStore::new_session` and `boot_session`. Session
threads/frames/scopes/variables/output are snapshotted from `DebugSession`
on demand, and breakpoints keep the Project's `BreakpointStore` as
authority. DAP reverse `runInTerminal` creates a Zed integrated terminal
and returns its PID to the adapter, so the debuggee is never started
through a separate shell path.

Zed's DapStore removes the Entity from the session map on the shutdown
event. To keep error output from vanishing at the same moment, zec retains
only the 8 most recent Entities as presentation history. That is a
reference-lifetime extension, not a copy of session state, and
control/REPL against terminated Entities is refused. For adapters like GDB
that suppress continue events, the panel state prefers concrete thread
status over the stale global stop flag.

## Extension ecosystem and distribution

Interactive startup initializes the Zed production `Client`, `NodeRuntime`,
extension host, and the language/debug/theme extension bridges, making one
`ExtensionStore` the authority for the registry, installed state, and
operation lifecycle. zec's extension picker is a bounded projection of the
store snapshot; install/upgrade/uninstall/reload and development-extension
install/rebuild go back through the public APIs. Remote registry failures
never erase installed records and surface as status.

The theme/icon-theme pickers project the `ThemeRegistry`'s current values
and full candidate sets, persisting selections through Zed's user settings
API. The settings/keymap actions open the real files Zed resolves as
ordinary Buffers. Settings/keymap reloads from the watcher feed the shared
event loop, so a new theme, editor projection, or binding applies right
after editing, and a parse failure keeps the previous valid state.

Standalone update picks the raw executable for the current
OS/architecture from a strict version-1 JSON manifest. Remote input is
HTTPS-only; only candidates whose size and SHA-256 verify are synced to a
temporary file, `--version` must match exactly, and only then is the file
atomically persisted into the same directory. Explicit download never
overwrites an existing path, and Unix self-update replaces only the
resolved current regular executable. Windows never replaces a running
executable and requires an explicit swap after a verified download.

The release workflow builds Linux/Windows/macOS on x86-64/ARM64 native
runners, producing raw binaries and minimal archives.
`script/release-manifest` verifies metadata, archive members, raw/archive
identity, all checksums, and target uniqueness, and only that file set goes
to GitHub attestation and the release. OS signing/notarization is a
separate boundary requiring private keys, and the absence of credentials is
never treated as signed. The normative safety conditions live in
[`distribution.md`](distribution.md).

## Rich content and large files

Markdown preview uses the same parse options as pinned Zed and converts to
a bounded snapshot for terminal cells. The source Buffer stays under Zed's
authority, and the preview updates on per-frame change detection. Local
links resolve to the active worktree's `ProjectPath`, heading fragments
convert to Buffer positions, and navigation goes to a normal tab. External
URLs pass three stages: a scheme allowlist, reconfirmation, and platform
opener capability.

Images are not read directly on extension checks; they go through
`Project::open_image`, and only the bytes and metadata of the ImageItem Zed
returns are projected into presentation. Input bytes, decoded pixels, and
protocol output are each bounded. Kitty clears stale images by image-id
delete; iTerm2/Sixel by full-frame redraw after an alternate-screen clear.
Without a protocol, the same tab shows format, dimensions, and byte size.
Image tabs persist only their path as `SessionItemKind::Image` and save no
implementation scratch Buffer as dirty recovery. In frames with an overlay,
the Editor snapshot is drawn and Rich Content is suppressed, so
trust/picker/confirmation can never take input while invisible.

There is no separate large-file text model and no truncated save path. The
normal Zed Editor/Buffer, DisplaySnapshot, go-to-line, and BufferStore save
are used, and the 100k-line / 64 KiB-line actual-binary PTY suite measures
first frame, long-line render, tail edits, disk contents, Linux VmHWM, and
terminal lifecycle. The repository 500-edit benchmark and the distribution
boundary-shape tests remain separate oracles.

## AI, collaboration, media, and Notebook

The Agent panel holds `AcpThread` as the authority for conversation, tool
calls, permissions, and session status. With no configuration it creates
the in-process Zed Agent through `NativeAgentServer` and the global
`ThreadStore`; only an explicit `ZEC_ACP_AGENT` starts an external agent
via `AcpConnection::stdio`. The terminal side keeps only a bounded
prompt/history/local output and duplicates no stream entries or permission
outcomes. Model/mode/config/session/auth go through the ACP connection's
public API, MCP through the Project `ContextServerStore`, and
skills/instructions through Zed Agent discovery on trusted worktrees.

Edit prediction observes all hidden Editors and swaps between
Zed/Copilot/Codestral or a custom FIM delegate according to Zed language
settings and organization/user events. Display and accept-all/word/line are
the Editor's native ghost-text actions. The inline assistant captures the
target Buffer generation and range, streams through Zed's
`PromptBuilder`/`LanguageModelRegistry`, and applies the result as a
one-transaction preview. Results whose source generation changed are not
applied, and accept/reject/undo use the same Buffer transaction.

The Collaboration panel projects only snapshots of the production `Client`,
`UserStore`, and `ChannelStore`. Create/invite/sign-in/out/refresh go back
to the store APIs, and notes tabs keep text/collaborators/replica state
under `ChannelBuffer` authority. Splits share the same Buffer Entity, and
following converts collaborator points into ordinary Editor selections.
Offline fixtures are isolated purely for PTY determinism and never mix with
production state.

Voice/screen share cannot be faked in terminal cells, so they are an
external bridge. Only when strict JSON config, terminal capability, a
selected channel URL, and per-run confirmation all line up does zec start
an owned child, passing kind and URL as trailing arguments.
Start/stop/exit/error project into the panel, and stop and Drop wait after
kill.

`.ipynb` opens a `NotebookEditor` in a hidden GPUI window separate from the
normal Project Buffer. Cells, cell Editors, execution requests, and Jupyter
messages are the NotebookEditor's authority; save/session dirty state is
the Project Buffer's; the terminal renders only the immutable JSON snapshot
from `to_notebook`. Splits on the same Buffer id share one Notebook state.
Cell actions dispatch Zed notebook actions into the hidden dispatch tree,
and the Buffer updates only when the native snapshot changed. Existing
display-data that the pinned upstream serializer drops is preserved by cell
id and invalidated on run/clear.

The pinned upstream can leave a process group behind when a local kernel is
restarted/closed while Starting. zec derives `kernel-zed-<id>.json` from
the Notebook Entity id and selects, via sysinfo, only processes that are
descendants of the current process and carry that connection file in their
argv. After a restart only the pre-snapshot PIDs are terminated; the final
Drop terminates all matching PIDs — by Unix process group (per process on
non-Unix) — so other Notebooks, terminals, and tasks are never caught.

## Buffer search (`Ctrl-F`, active Buffer only)

In-buffer search here is distinct from `Alt-F` repository-wide Project
Search. `Ctrl-F` targets only the active tab's Buffer and uses neither the
repository `Search::local` nor the `RepositoryIndex`.

`Ctrl-F` does not spawn the Workspace GUI search bar; it uses the public
`SearchableItem` API that `Editor` implements. Running the `SearchQuery`,
stable match anchors, the active match, next/previous wrapping, selection,
autoscroll, and highlight colors are Zed's. zec owns only the single-line
query shown in the status row with its cursor, and a short-lived session
for passing the match list back to the API — no text or undo state.

Zed's background highlights are fetched as `DisplayPoint`s and converted to
terminal cell ranges, overlaying only a background color after syntax and
before selection. Currently a case-insensitive literal search is awaited
sequentially per query change. Regex/word/case options, history,
cancel/debounce for long searches, and multiline queries are follow-up
work.

`Ctrl-H` adds a single-line replacement prompt to the same session. The
terminal chrome owns only query/replacement input and focus; replacement
ranges, anchors, edit order, transactions, and undo go back through the
public `SearchableItem::replace` / `replace_all`. The Editor implementation
makes a single replacement one transaction and replace-all one transaction,
so zec assembles no text edits. A single replacement advances from the
pre-edit match anchor before re-searching, making it hard to stay stuck on
the same match even when the replacement contains the query. After
replace-all an explicit re-search syncs the terminal-side match count and
background highlights.

Legacy terminals may not distinguish `Ctrl-Enter` from `Enter`, so in the
replace field `Enter` is the portable single replacement and `Alt-Enter`
the portable replace-all, with `Ctrl-Enter` also accepted as replace-all
where it is distinguishable. Newlines in replacements are outside the
single-line prompt boundary and remain follow-up work. To avoid body
shortcuts leaking during the prompt, `Ctrl-Z` for a replacement runs after
closing the search with `Esc`.

## Go to line

The Zed action behind `Ctrl-G` wants a Workspace modal, and on a standalone
Editor the handler does nothing. The input field is therefore a
terminal-owned `LinePrompt`, while position resolution and the selection
change after submission go back to Zed's public APIs. Input is an absolute
`line[:column]`, 1-based.

The target resolves through the active Buffer's
`BufferSnapshot::point_from_external_input`. That API avoids confusing
Unicode columns with UTF-8 byte columns and clips out-of-range line/column
to document and line boundaries exactly like Zed itself. The resulting
Buffer Point converts to a MultiBuffer Anchor, and
`Editor::change_selections` with
`SelectionEffects::scroll(Autoscroll::center())` collapses all selections
to one caret. Text and undo transactions are untouched. zec computes no
fold/wrap/display rows.

The hidden GPUI Editor's scroll position is not the terminal viewport's
source of truth, so the existing `keep_cursor_visible` shows the target
with minimal scrolling. Zed GUI's strict centering, relative input, and
live preview highlighting during input are follow-up work.

## Terminal scrolling

The mouse wheel moves the active tab's terminal viewport by 3 display rows.
`Alt-PageUp` / `Alt-PageDown` use the terminal body height minus one row
(minimum 1), keeping one overlapping row between consecutive screens.
Neither sends keystrokes to Zed nor changes selection, cursor, or undo
transactions.

During a manual scroll only vertical cursor follow stops. The moment Zed's
cursor position changes, auto-follow resumes; horizontal follow always
stays on. A mere Redraw or Resize does not clear the manual state, and the
viewport is clipped to the valid range only when it passes the document
end.

`PageUp` / `PageDown`, plain or with `Shift`, still go to Zed's keymap. The
hidden GPUI window's page size does not match the terminal body height, so
the movement amount on that route is a known boundary. Using the terminal's
own text selection during mouse capture requires `Shift`-drag in most
terminals.

## Terminal mouse positioning

Only an unmodified left-button Down is treated as caret movement. The same
grapheme/cell widths, gutter, and viewport used for Ratatui rendering
invert the screen cell into a UTF-8 byte position on the display row. Wide
graphemes snap to the boundary nearest the cell center, and blank cells of
graphemes clipped at the viewport edge are unclickable.

The inverted result is clipped against the latest `DisplaySnapshot` at
click time and passed to `display_point_to_anchor ->
Editor::change_selections`. zec owns no caret or selection state, and a
mouse click changes neither Buffer text nor undo transactions. The gutter,
status row, out-of-body areas, right/middle buttons, and modified clicks
act on nothing. During prompt or search input, the body caret does not
move.

The entry points of Zed GUI's drag, word/line selection, and multi-cursor
mouse state machine are currently `pub(super)`. Rather than duplicating
that logic, drag/double/triple/modifier selection stays follow-up work
until it is decided whether to add a small public hook on the Zed side.

## Tabs

Each tab owns one hidden GPUI window rooted at `Editor::for_buffer`. Zed's
focus, key context, action dispatch, selection, cursor, undo, and
DisplayMap are used per window as-is; zec's only mutable state is the
active index, the terminal viewport, and its follow state. The public
`replace_root` cannot reattach an existing Editor Entity to the root, and
swapping children within one window would require a dedicated host and
focus-tree synchronization, so it is not used.

`Ctrl-PageUp` / `Ctrl-PageDown` are captured by zec since no Workspace/Pane
exists, making only the active WindowHandle the render/input target. Each
window's focus is window-local, so no OS window activation is needed.
Switching closes the search so Buffer-specific Anchors never reach another
Editor, and cancels Save As and Open prompts. The status shows the active
position and every tab's dirty state; `Ctrl-S` checks only the active tab
and `Ctrl-Q` inspects all Buffers.

`Ctrl-O` absolutizes a path from a terminal-owned single-line prompt and
passes it to the same `open_document` as startup. When the shared
BufferStore returns an existing Buffer Entity, the existing tab is
activated; only a new Buffer adds a hidden window and a syntax-redraw
subscription. On open failure the error returns to the prompt and existing
tabs and the active index stay unchanged.

`Ctrl-N` creates scratch through the `open_document(None)` path using the
same BufferStore's `create_local_buffer`, labeled `Untitled N` with a
process-monotonic number. Because no bare Buffer is created, later Save As
transitions correctly into file-path mapping and external-change watching.
New scratch uses the same hidden window, dirty protection, Save As, and
close lifecycle as normal tabs. As an active-tab-changing operation it
closes the search and cancels in-progress Save As/Open prompts just like a
tab switch.

`Ctrl-W` closes a clean tab immediately and requires a second press for a
dirty one; any other key press or paste clears the confirmation. GPUI's
WindowHandle does not own the window, so `Window::remove_window` is called
before removing the DocumentTab rather than just dropping the handle.
Closing the last window also shuts down the GPUI headless runtime. The
BufferStore holds weak references, so reopening a discarded dirty tab by
the same path loads a fresh Buffer from disk.

## Why not `ratatui-textarea`

`ratatui-textarea` owns not only display but text, cursor, selection, input
handling, and undo history. Using it for the main editing area would
duplicate editing state alongside Zed and require synchronizing undo, the
keymap, multiple selections, and folds/inlays.

The main editing area uses a stateless dedicated Ratatui Widget
implementing only the `DisplaySnapshot -> terminal cells` conversion. The
search/replace fields, Save As, Open, and Go to line are small single-line
inputs outside Zed's management, so zec keeps a common lightweight prompt
state. Since the needed operations are only character input, cursor
movement, deletion, submit, and cancel, `ratatui-textarea` is not added and
this boundary is confined to roughly one type.

## Boundaries

- Zed's display columns are UTF-8-byte-based and the terminal is
  grapheme/cell-width-based, so zec keeps exactly one coordinate
  conversion layer. The mouse hit test is the inverse conversion using the
  same cell metrics.
- All Zed selections are fetched as half-open display-coordinate ranges,
  converted to terminal cell coordinates, and only an inverted style is
  overlaid after Ratatui draws the characters. No strings or selection
  state are duplicated.
- Line numbers do not count display rows; `buffer_row` from
  `DisplaySnapshot::row_infos` is shown. Block rows and soft-wrap
  continuation rows are blank, and the gutter width is fixed from
  `widest_line_number`. Text, cursor, selection, and horizontal scroll all
  compute against the same Rect excluding the gutter.
- Initially soft wrap is disabled in favor of horizontal scrolling; GPUI
  pixel widths and terminal cell widths are never mixed.
- The terminal reader reads blocking input on its own thread and hands it
  to the GPUI foreground over a channel; the Editor itself is driven from
  a single thread. For PTYs that miss resize events, the same reader
  thread also polls the size at low frequency.
- The headless clipboard is unavailable, so bracketed-paste strings go
  directly into Zed's `do_paste`, leaving selection replacement,
  auto-indent, and undo granularity on paste to Zed.
- GPUI's Linux headless clipboard has a no-op write and an always-empty
  read, and no public API returns the `ClipboardItem` created by Zed's
  Copy/Cut. For the copy payload only, a thin adapter assembles the same
  line/multi-selection rules as upstream from Zed's public selections and
  buffer snapshot. It owns no Editor, selection, edit transaction, or undo
  state. Cut dispatches Zed's Cut action after the payload is written to
  OSC 52, leaving deletion and undo to Zed.
- OSC 52 carries only text and has no success response. Zed's clipboard
  metadata cannot be preserved across the terminal, so bracketed paste is
  always treated as external text without invented metadata. Raw text is
  capped at 256 KiB to stay under per-terminal control-string limits;
  oversized Copy is refused, and an oversized Cut leaves the text
  unchanged too.

## Deferred

Default-branch hosted evidence, a live AI provider and 2-client channel
co-editing, the desktop media bridge, image/HTML/JSON outputs newly
generated by a real kernel, remote/Windows Notebook lifecycle, and OS
signing/notarization remain as verification. In presentation, search
history, relative go-to-line and live preview, clipboard metadata/read,
grammars outside the native set with full language injection, and soft
wrap using terminal cell widths are incomplete.

Automated checks use `--smoke` and the unit tests. The terminal path is
verified over a PTY: character input, undo, saving new and existing files,
scratch save-as with overwrite confirmation, OSC 52 copy, undo of Zed Cut,
per-tab editing/undo/active save and all-tab dirty exit protection, opening
at runtime with dedupe, saving not-yet-created paths, adding scratch tabs
with Save As, auto-reload of clean external changes, reload/overwrite
confirmation for dirty external changes, disk reload after a dirty close,
exit of the last window, caret movement by mouse click on ASCII and wide
characters, and restoration of raw mode and the alternate screen.
