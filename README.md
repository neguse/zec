# zec

zec is a terminal editor built on Zed's editing core. It is an implemented
candidate for repository/language editing, the terminal workspace, the local
development loop (Git, terminal, tasks, DAP), the Zed extension host, themes,
settings/keymap, manifest-verified updates, a 6-target release pipeline, SSH
workspaces over Zed's remote protocol, Markdown and images, the 100k-line
large-file path, Zed Agent/ACP/MCP, edit prediction and the inline assistant,
channels/channel notes/following, the voice/screen bridge, and native Zed
Notebooks. All 27 capabilities in the machine-readable ledger carry
implementation evidence. The 361-case actual-binary acceptance suite and the
860-id benchmark have completed locally, but default-branch hosted evidence,
live services, and evidence on every platform are still pending, so zec is
neither verified nor production-ready yet.

Today zec runs `editor::Editor` on the GPUI runtime, feeds input from
Crossterm into Zed's keymap, and renders text, cursor, selection, and syntax
styles with Ratatui. Linux uses the headless platform; Windows uses the
native platform with hidden windows.

Design decisions and the road ahead live in
[docs/architecture.md](docs/architecture.md); PoC graduation criteria,
verification records, and known constraints in
[docs/poc-graduation.md](docs/poc-graduation.md). Long-term Zed-experience
parity is tracked in [docs/zed-parity.md](docs/zed-parity.md) and
[docs/zed-parity-v1.json](docs/zed-parity-v1.json). The repository editing
loop is governed by the machine-checked contract in
[docs/e2e-repository.md](docs/e2e-repository.md), the project-backed
language editing loop by [docs/e2e-language.md](docs/e2e-language.md), the
terminal workspace by [docs/e2e-workspace.md](docs/e2e-workspace.md), the
local development loop by
[docs/development-loop.md](docs/development-loop.md), ecosystem and
distribution by [docs/distribution.md](docs/distribution.md), and
AI/collaboration/media by
[docs/ai-collaboration-media.md](docs/ai-collaboration-media.md). Windows build requirements and
verification coverage are recorded in [docs/windows.md](docs/windows.md).

The everyday regression run, on top of unit and PTY tests, is this
actual-binary integration set; see [docs/e2e-language.md](docs/e2e-language.md)
for the full judgment procedure including the canonical 20-process
acceptance run and the benchmark. Every push to main also runs the three e2e suites
(repository, language, workspace) from one release build via
`.github/workflows/e2e.yml`; `script/run-e2e-suites` reproduces that run
locally.

```sh
cargo test --locked --test parity_contract --test e2e_tui \
  --test language_service --test settings_reload --test lsp_failures \
  -- --test-threads=1
```

With no arguments zec opens the current directory; with one directory
argument it opens that directory as the repository root. From a development
checkout these are `cargo run --` and `cargo run -- path/to/repository`.

```sh
zec
zec DIRECTORY
```

In repository mode `Ctrl-P` opens Quick Open to filter files under the root
and `Enter` opens the selection. `Alt-F` is a case-sensitive literal search
across the repository. Results show path, 1-based line, Unicode column, and
a preview; the first 100 hits can be walked with the arrow keys and `Enter`
jumps to the match. Unicode normalization, regex, and project-wide replace
are not performed.

Quick Open and Project Search cancel with `Esc`. When the query changes
mid-search, only the newest query's results are shown; results of older
queries that complete out of order are neither drawn nor applied, and
results that arrive after `Esc` or after moving to another operation are
ignored.

The traditional direct file mode remains. If the first argument is a file,
any number of existing or not-yet-created files can be given, and more can
be added at runtime with `Ctrl-O`. This mode has no repository, so `Ctrl-P`
and `Alt-F` are unavailable.

```sh
zec path/to/file another/file
```

`Ctrl-N` adds a scratch tab named `Untitled N` with no destination yet.
Opening a Buffer that is already open moves to the existing tab instead of
duplicating it. `Ctrl-PageUp` / `Ctrl-PageDown` switch tabs and `Ctrl-W`
closes the active one. An unsaved tab, or one deleted on disk, is discarded
only when `Ctrl-W` is pressed a second time; closing the last tab exits.
Each tab's text, cursor, selection, and undo live in an independent Zed
Editor, with only the viewport kept on the terminal side. `Ctrl-S` saves
just the active tab through Zed's `BufferStore`. `Ctrl-Q` quits; if any tab
is unsaved or deleted, a second `Ctrl-Q` confirms discarding. The status
line marks dirty tabs with `+` and external-change conflicts or on-disk
deletion with `!`.

A clean file changed on disk reloads automatically. During local edits the
text is not overwritten: `!` is shown and `Ctrl-S` overwrites the disk only
when pressed again. `Ctrl-R` reloads the active file from disk, requiring a
second press when dirty. Reload is a Zed transaction, so `Ctrl-Z`
immediately afterwards returns to the pre-reload text. An external rename
follows the same Zed Buffer's file identity, updating the tab label and
future save destination. After an external delete the text is kept, `!` is
shown, and pressing `Ctrl-S` again recreates the same path. Catchable
`SIGTERM` / `SIGHUP` go through the normal exit path, restoring the
terminal mode before exiting.

`Ctrl-N` opens an empty scratch buffer. `Ctrl-S` enters a one-line Save As
prompt; relative paths resolve against the working directory zec started
in. Parent directories are created when needed, and an existing regular
file is overwritten only after a second `Enter`. `Esc` cancels. The Save As
and `Ctrl-O` path prompts do not go through a shell, so `~` does not expand
to the home directory.

Selections via `Shift` + arrows and `Ctrl-A` also run through Zed's keymap,
with the selected range shown inverted in the terminal. Gutter line numbers
come from Zed's display snapshot, so display lines are not naively
recounted after folds.

The mouse wheel scrolls the active tab's view by 3 display rows;
`Alt-PageUp` / `Alt-PageDown` scroll by the terminal body height minus one
line (one line when the body is a single line). Zed's cursor, selection,
and undo are untouched, and the view returns to auto-follow once the cursor
moves. To use the terminal's own text selection while mouse capture is
active, most terminals require dragging with `Shift`.

An unmodified left click moves Zed's caret to the clicked text position.
The gutter, the status row, and wide characters cut in half at the viewport
edge are not click targets. Drag selection, double/triple click, and
modified mouse operations are not covered yet.

`Ctrl-C` copies the selection (or the current line when empty) to the
terminal clipboard; `Ctrl-X` deletes through Zed's Cut action only after
the copy succeeds. Transfer uses OSC 52, so terminal support or tmux
clipboard configuration may be required; terminals return no success
response, so unsupported terminals silently ignore the operation. To avoid
giant control sequences the limit is 256 KiB per operation. Paste still
passes the terminal's bracketed paste into Zed.

`Ctrl-F` starts a case-insensitive literal search that updates matches as
you type. `Enter` or the down arrow moves to the next match, the up arrow
(`Shift-Enter` where the terminal can distinguish it) to the previous, and
`Esc` closes the search. Matching, movement, selection, and autoscroll use
Zed's search implementation. `Ctrl-H` also shows a replace field. `Tab` /
`BackTab` move between the query and replace fields; in the replace field
`Enter` replaces the current match and `Alt-Enter` (`Ctrl-Enter` where
detectable) replaces all. After closing with `Esc`, both single and
replace-all undo with `Ctrl-Z`.

`Ctrl-G` jumps to a 1-based `line[:column]` position. Out-of-range lines or
columns clip to document and line boundaries through Zed's BufferSnapshot,
and Unicode columns resolve as character positions rather than bytes. Empty
or non-numeric input shows an error inside the prompt for correction.

Zed's bundled native tree-sitter parsers, configs, and queries highlight
Shell, C/C++, CSS, Diff, Go, JSON, JavaScript/TypeScript, Markdown, Python,
Rust, YAML, and more. At startup only configs and matchers are registered;
parsers and queries needed by open files and injections load lazily. In
repository mode the same Zed `Project` owns the LanguageRegistry and
LspStore, and each language adapter starts its language server through
normal PATH discovery. The Node runtime follows Zed's settings, probing
configured/system paths and initializing ready to download when necessary.
With `NO_COLOR` set, colors are suppressed per the Crossterm convention.

`F1` or `Ctrl-Shift-P` opens the command palette with action ids, display
names, bindings, and current enabled state. `Ctrl-Space` (or `Alt-/`)
completes, `F2` shows hover, `F8` project diagnostics, `F12` / `Alt-F12` /
`Shift-F12` definition / type definition / references, and `Ctrl-T` project
symbols. After a jump, `Alt-Left` / `Alt-Right` walk history including
selection and viewport. `F6` opens the rename prompt after prepareRename,
`Ctrl-.` the code action picker, `Shift-Alt-F` formats the document and
`Ctrl-Alt-F` the selection. Multi-file renames and code actions are held as
Zed `ProjectTransaction`s, and `Ctrl-Z` / `Ctrl-Y` undo/redo across every
affected Buffer at once.

`Ctrl-Shift-G` projects Zed's active repository snapshot into the Git
panel, where individual or all changes can be staged, unstaged, and
discarded through the Zed GitStore. `` Ctrl-` `` opens the bottom-dock
integrated terminal and `` Ctrl-Shift-` `` opens another Zed terminal. With
the terminal focused, keys, paste, resize, and scrollback go to the Zed
terminal, and `Esc` returns to the editor.

`Ctrl-Shift-B` opens the task picker resolved by Zed's TaskInventory;
`Ctrl-Alt-B` reruns the last completed task. Tasks run in the integrated
terminal and honor Zed task settings such as concurrency restriction,
reveal, save, cwd, and environment. Untrusted worktrees start no processes.

`F5` picks a debug scenario from `.zed/debug.json` and task-derived
scenarios, starting a registered Zed DAP adapter. `Ctrl-Shift-D` opens the
Debugger panel, `Ctrl-F9` toggles a source breakpoint, `Ctrl-F5` /
`Ctrl-F6` / `Shift-F5` continue / pause / stop, and `Alt-F10` / `Alt-F11` /
`Alt-Shift-F11` step over / in / out. The debug console on `Ctrl-Shift-R`
(`:` while the panel is focused) evaluates adapter-native REPL commands.
Ended sessions leave the operable set, but the last adapter output remains
for post-mortems. See [docs/development-loop.md](docs/development-loop.md) for details and the
real-GDB acceptance run.

`Ctrl-Shift-X` is the Zed ExtensionStore's all/installed/updates view.
`Enter` installs or updates, `Delete` twice uninstalls, `Ctrl-D` adds a
development extension with an `extension.toml`, and `Enter` afterwards
rebuilds it from source. `Ctrl-Alt-T` / `Ctrl-Alt-I` pick theme and icon
theme including extension-provided ones; `Ctrl-,` / `Ctrl-Alt-,` open the
real Zed settings and keymap files. `Ctrl-Alt-U` checks the update
manifest. See [docs/distribution.md](docs/distribution.md) for CLI verification,
download, apply, and the release format.

In a Markdown tab `Ctrl-Shift-V` opens the preview using Zed-compatible
feature flags; `Tab` selects links/images and `Enter` opens them. Local
links are restricted to the Zed ProjectPath, and external URLs go to the OS
after confirmation. PNG/JPEG/GIF/WebP/BMP/TIFF/ICO/PNM images open as
read-only tabs from Quick Open, the Project Panel, `Ctrl-O`, or
`zec image.png`. Kitty/iTerm2/Sixel support is capability-detected; on
unsupported terminals the format, dimensions, and size are shown. Trust and
command overlays always sit above Markdown/image tabs.

The actual-binary suite covering a 100k-line file and a 64 KiB single line
verifies open, `Ctrl-G` navigation, rendering, editing, saving, Linux
`VmHWM ≤ 1 GiB`, and terminal restoration — through the normal Zed
Editor/Buffer path, not a simplified text model.

`zec remote ssh` opens a remote Project over the same remote protocol and
server as Zed. Authority for the editor, BufferStore, WorktreeStore, LSP,
Git, terminal, tasks, DAP, project search, and the project panel stays with
the remote Zed Project. Passwords are not accepted as arguments; required
authentication happens through a masked in-terminal askpass. WSL and
Docker/Podman transports share the same entry point.

```sh
zec --version
zec remote ssh HOST /absolute/project
zec remote wsl DISTRO /absolute/project
zec remote container NAME /absolute/project
zec update check
zec update download --output ./zec-new
zec update apply
```

## Agent, collaboration, and Notebook

`Ctrl-Shift-A` (or `Toggle Agent Panel` in the command palette) opens the
Agent panel. The default is the in-process native Zed Agent; an external
stdio ACP agent is used only when `ZEC_ACP_AGENT` is set to strict JSON
with command/args/env/id. Prompting, streaming, tool calls,
permission allow/reject, cancel, and new sessions work, along with
`/models`, `/modes`, `/config`, `/sessions`, `/skills`, `/instructions`,
`/mcp`, and `/auth`. Process starts, including external agents, happen only
after worktree trust.

`Alt-\` shows a Zed edit prediction, and `Alt-L` / `Alt-K` / `Alt-J` accept
all / next word / next line as Zed transactions. The provider follows the
Zed/Copilot/Codestral/Ollama/OpenAI-compatible configuration in Zed
language settings. The `Ctrl-Enter` inline assistant turns streamed results
into a diff preview; `Enter` accepts and `Esc` rejects.

`Ctrl-Alt-C` opens the Collaboration panel over Zed's
Client/UserStore/ChannelStore. Arrows and `Enter` open channel notes, `Tab`
and `f` follow collaborators, `c` creates a channel, `a` / `d` answer
invites, and `i` signs in/out. Notes are Zed `ChannelBuffer`s, so splits
share the same collaborative authority. Voice/screen are an external
bridge, never faked inside the terminal: only with both `ZEC_MEDIA_BRIDGE`
and `ZEC_EXTERNAL_MEDIA=1` set does `v` / `s` followed by `y` each time
start them, and owned processes are reclaimed on stop and exit.

`.ipynb` opens in Zed's `NotebookItem` / `NotebookEditor`. Arrows select
cells, `Enter` edits, `Ctrl-Enter` / `Shift-Enter` run / run-and-advance,
`b` / `m` add code/Markdown cells, `dd` deletes, `Alt-Up/Down` move, `R`
runs all, `c` clears outputs, and `i` / `r` interrupt/restart the kernel.
Stream/error/Markdown outputs and metadata fallbacks of existing rich
outputs are projected, and nbformat JSON saves through a normal Project
Buffer. Splits share one Notebook authority, and no local Jupyter process
group is left behind during restarts or at the final close. See
[docs/ai-collaboration-media.md](docs/ai-collaboration-media.md) for details, constraints, and
actual-binary evidence.

A headless smoke that checks insertion and undo against the Zed Editor
without a terminal:

```sh
cargo run -- --smoke
```

The first build compiles the whole Zed dependency set, which takes time and
disk space.

## Windows

Windows needs the MSVC C++ build tools, the Windows SDK, and the Visual
Studio Spectre-mitigated libs component (see
[docs/windows.md](docs/windows.md)). From a ConPTY-capable terminal such as
Windows Terminal, open PowerShell and build, smoke, and start zec:

```powershell
cargo build --locked --bin zec
.\target\debug\zec.exe --smoke
.\target\debug\zec.exe .
```

Normal editing, repository mode, saving, and search all work on Windows,
including the ConPTY integrated terminal, the TUI acceptance suite, and
the e2e_language suite. Every e2e binary builds on Windows with no cargo
feature; POSIX signal/job-control cases and repository fixture generation
remain Unix-only (see docs/windows.md).
