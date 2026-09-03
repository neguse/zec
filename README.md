# zec

zec is a terminal editor built on Zed's editing core. Zed's `editor::Editor`
runs on the GPUI runtime (headless on Linux, hidden native windows on
Windows); zec feeds Crossterm input into Zed's keymap and projects Zed's
display snapshot onto terminal cells with Ratatui. Zed's crates are pinned
Git dependencies; there is no fork.

The tree is being rebuilt around [docs/architecture.md](docs/architecture.md).
Today it is the core: open, edit, save, Save As, reload with external change
tracking, tabs, syntax highlighting, Zed settings and keymap, a command
palette, and a terminal lifecycle that restores the terminal on every exit
path. Features from the previous tree return one at a time in the order
given in [docs/rebuild-plan.md](docs/rebuild-plan.md).

## Usage

```sh
zec              # current directory as the root, one scratch tab
zec DIRECTORY    # that directory as the root
zec FILE...      # one tab per file
zec --smoke      # headless insert/undo check against the Zed Editor
zec --version
```

Editing keys are Zed's default keymap. zec adds these bindings, all of
which are also listed in the command palette:

| Key | Command |
| --- | --- |
| `F1`, `Ctrl-Shift-P` | Command Palette |
| `Ctrl-N` / `Ctrl-O` | New File / Open File |
| `Ctrl-P` | Quick Open, a fuzzy file picker over the root |
| `Alt-F` | Search Project: literal, case-insensitive, hits open at their line |
| `Ctrl-F` | Find in the buffer as you type; Enter / Shift-Enter step through matches |
| `Ctrl-H` | Replace the current match (Enter) or all of them (Shift-Enter) |
| `Ctrl-G` | Go to `line` or `line:column` |
| `Ctrl-S` | Save (Save As is in the palette) |
| `Ctrl-R` | Reload from disk |
| `Ctrl-W` | Close Tab |
| `Ctrl-PgUp` / `Ctrl-PgDn` | Previous / Next Tab |
| `F10` / `Shift-F10` | Split Right / Split Down |
| `Ctrl-Alt-Arrow` | Focus the pane in that direction |
| `Ctrl-Alt-Shift-Arrow` | Move the tab to the pane in that direction |
| `Ctrl-Alt-=` / `Ctrl-Alt--` | Grow / Shrink Pane |
| `F7` / `Ctrl-Shift-E` | Toggle the project panel (left dock) |
| `F9` / `Ctrl-Shift-O` | Toggle the outline panel (right dock) |
| `Ctrl-C` / `Ctrl-X` | Copy / Cut through OSC 52 |
| `Ctrl-,` / `Ctrl-Alt-,` | Open Settings / Open Keymap |
| `F4` | Show Terminal Capabilities |
| `Ctrl-Q` | Quit |

A command that would discard work asks for the same key again; any other
input dismisses the question. Inside a panel, arrows move and expand or
collapse, Enter opens or jumps, Esc returns to the editor, and the project
panel adds `n` / `N` (new file / directory), `F2` (rename), `Delete`, `r`
(reveal the active file), and `i` (show ignored entries). Zed's `settings.json` and `keymap.json` are
read from Zed's configuration directory and reloaded on change.
`ZEC_DATA_DIR` moves that directory, and `ZEC_KEYBOARD_PROTOCOL`
(`kitty`, `modifyOtherKeys`, or `legacy`) overrides keyboard protocol
detection. In directory mode zec remembers the layout and open files per
root under its data directory and restores them on the next start.

## Building

```sh
cargo build --locked --bin zec
cargo test --locked --bin zec -- --test-threads=1
cargo test --locked --test e2e -- --test-threads=1
./target/debug/zec --smoke
```

The first build compiles the whole Zed dependency set, which takes time and
disk space. Windows needs the MSVC C++ build tools, the Windows SDK, and the
Visual Studio Spectre-mitigated libs component, and a ConPTY-capable
terminal such as Windows Terminal.
