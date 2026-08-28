# Beta 2 contract: Ecosystem and Distribution

Contract status: Implemented candidate for extensions, themes, settings/keymaps, package assembly,
updates, SSH remote development, rich content, and the large-file path; WSL/container still require
native-platform evidence (2026-08-26)

Beta 2 connects the console workspace to Zed's extension and configuration infrastructure and adds
a reproducible, manifest-driven distribution path. Zed `ExtensionStore`, extension host,
`ThemeRegistry`, `SettingsStore`, keymap loader, language-extension adapters, and Node runtime remain
authoritative. zec owns the bounded terminal pickers, update CLI, and release-asset validation.

These capabilities remain `candidate` until the default branch and a tagged release produce hosted
evidence on all target platforms. A local fixture is not evidence that the public extension registry,
codesigning, notarization, or an Internet update has succeeded.

## Outcome

- `Ctrl-Shift-X` opens one extension view over the real Zed `ExtensionStore`. `Tab` cycles all,
  installed, and updates; `Enter` installs, upgrades, or rebuilds the selected development extension;
  `Delete` twice uninstalls; `Ctrl-R` reloads the store.
- `Ctrl-D` in the extension view accepts a local source directory containing `extension.toml` and
  calls Zed's development-extension install path. Reopening and pressing `Enter` rebuilds it from the
  source directory, and uninstall removes the installed development link.
- `Ctrl-Alt-T` and `Ctrl-Alt-I` select Zed themes and icon themes, including resources contributed by
  extensions. The selected names are persisted through Zed's user settings API.
- `Ctrl-,` and `Ctrl-Alt-,` open the actual Zed user settings and keymap files as normal editable
  buffers. File watchers reload valid edits; parse failures are surfaced without replacing the last
  valid configuration.
- Zed's production client, Node runtime, language/debug/theme extension bridges, and extension host
  are initialized for interactive runs. Node may be resolved from configured/system paths or fetched
  by Zed according to `node` settings.
- `Ctrl-Alt-U` (or `Check for Updates` in the command palette) checks the configured update manifest.
  The standalone CLI can check, verify, download, and, on Unix, atomically apply an update.
- A tag-triggered workflow builds raw binaries and archives for Linux, Windows, and macOS on x86-64
  and ARM64, verifies their exact contents and SHA-256 values, creates a versioned update manifest,
  attests the published file set, and creates the GitHub release.
- `zec remote ssh HOST /absolute/project` connects through Zed's pinned remote protocol and remote
  server. The resulting remote `Project`, `WorktreeStore`, `BufferStore`, LSP/Git/task/DAP stores,
  integrated terminal, search, and project mutations remain authoritative on the connected host.
- The same parser and Zed transport support WSL and Docker/Podman connections. Passwords are never
  accepted as command-line values; interactive authentication uses a masked terminal askpass.
- Every release target includes a separately checksummed compressed `zec-remote-server` asset. The
  client uses an explicit/sibling server in development, otherwise downloads only the exact client
  version and verifies manifest target, byte size, and SHA-256 before caching it.
- `Ctrl-Shift-V` projects Zed-compatible Markdown semantics into terminal cells. Links resolve
  through the active Zed `ProjectPath`; raster images open through `Project::open_image` and use
  Kitty, iTerm2, or Sixel when detected, with a bounded metadata fallback otherwise.
- Raster images can be opened from Quick Open, Project Panel, the Open prompt, or directly as
  `zec image.png`. Image tabs are read-only workspace items and survive crash-tolerant session
  persistence without inventing a text recovery buffer.
- The actual binary opens 100,000 logical lines including one 64 KiB line, navigates to the long
  line and final line, edits and saves through Zed Editor/Buffer, and on Linux stays below a 1 GiB
  `VmHWM` ceiling.

## Authority and safety invariants

1. Installed/remote extension records and extension operations come from Zed `ExtensionStore`; zec
   does not maintain a second extension database. Registry failure leaves installed extensions usable
   and is displayed as an error.
2. Extension language servers, DAP adapters, themes, and icon themes are loaded through Zed extension
   bridges. Interactive initialization is skipped only in in-process unit tests; actual-binary PTY
   tests exercise the host and on-disk store.
3. Development-extension sources are canonical directories with a regular `extension.toml`. Shell
   command execution is not used to interpret the entered path.
4. Settings and keymap buffers use Zed's configured paths and normal file lifecycle. Theme and icon
   selections are written with Zed settings APIs, not a parallel zec preference file.
5. Update manifests use schema version 1, strict fields, SemVer without build metadata, unique OS/arch
   targets, a lower-case SHA-256, exact size, and the expected executable name. Remote manifests and
   assets require HTTPS and do not accept embedded credentials.
6. Manifests are limited to 1 MiB and binaries to 512 MiB. Local update inputs must be regular files;
   relative assets cannot escape the manifest directory. Every candidate must match size and digest
   and execute `--version` with the exact manifest version before it is installed.
7. Explicit downloads refuse to overwrite an existing path. They are written, flushed, marked
   executable, checked, and atomically persisted in the destination directory. Unix self-update uses
   the same candidate path and atomically replaces only the resolved regular current executable.
8. Windows deliberately refuses to replace its running executable. Use `update download`, exit zec,
   and replace `zec.exe` explicitly. No updater requests privilege elevation.
9. Release assembly rejects symlinks, duplicate targets, unexpected archive members, raw/archive
   divergence, missing checksums, and post-metadata modification. Archives contain exactly the
   executable, target-specific compressed remote server, `README.md`, and `LICENSE` below
   `zec-VERSION/`.
10. Remote roots are interpreted with the connected host's Unix or Windows path style and are never
    canonicalized through the client's local filesystem. Remote file open, search, save, Git,
    terminal, task, and project mutation operations go through Zed project stores.
11. SSH passwords are not accepted on the process command line. Askpass data is encrypted in transit
    to the delegate, masked while entered, zeroized after use, and cancellable. Connection failure,
    reconnect authentication, server stderr closure, and shutdown are visible terminal states.
12. A downloaded remote server must match the client SemVer exactly. Manifests allow only supported
    OS/arch pairs, unique targets, HTTPS for hosted assets, `.gz` on Unix or `.zip` on Windows, exact
    byte size, and lower-case SHA-256. Cache hits are revalidated before use.
13. Markdown parsing uses the pinned Zed Markdown feature flags and fixed source/output bounds.
    Local references reject traversal outside the worktree; external URLs allow only
    `http`/`https`/`mailto`, require explicit confirmation, and open only when the platform bridge is
    available.
14. Image inputs are capped at 32 MiB and 100 million decoded pixels. Protocol payloads are bounded;
    Kitty images use explicit delete commands, while iTerm2/Sixel removal clears and fully redraws
    the alternate screen. Rich content never hides an authoritative trust or command overlay.
15. The large-file gate observes the actual debug binary through a PTY, verifies first-frame
    latency, navigation, rendering, disk persistence, process high-water memory, exit status, and
    exact terminal-mode restoration. A screen-only marker is not sufficient evidence of saving.

## Remote interface

```sh
zec remote ssh HOST [--user USER] [--port PORT] [--arg ARG]... \
  [--timeout SECONDS] [--nickname NAME] [--no-upload] ABSOLUTE_PATH...
zec remote wsl DISTRO [--user USER] ABSOLUTE_PATH...
zec remote container NAME [--id ID] [--user USER] [--podman|--docker] \
  [--env NAME=VALUE]... [--no-upload] ABSOLUTE_PATH...
```

All remote project paths must be absolute according to the connected platform. `--arg` passes one
SSH argument without invoking a shell. Container environment entries require explicit `NAME=VALUE`;
duplicate or invalid names are rejected. Worktree trust is still required before remote processes
can start.

## Update interface

The default source is
`https://github.com/neguse/zec/releases/latest/download/zec-update-v1.json`. `--manifest` overrides it
for one invocation; `ZEC_UPDATE_MANIFEST` provides an offline or enterprise default.

```sh
zec --version
zec update check [--manifest PATH|HTTPS_URL]
zec update verify --binary PATH [--manifest PATH|HTTPS_URL]
zec update download --output PATH [--manifest PATH|HTTPS_URL]
zec update apply [--manifest PATH|HTTPS_URL]
```

Successful CLI operations emit a versioned JSON report on stdout. Validation or download failure is
non-zero and does not materialize the requested output. The interactive action only checks; it tells
the user to exit and run `zec update apply` when a newer version exists.

## Release interface

`script/release-manifest` is the deterministic release boundary:

```sh
python3 script/release-manifest self-test
python3 script/release-manifest metadata --help
python3 script/release-manifest assemble --help
python3 script/release-manifest verify --help
```

`.github/workflows/release.yml` accepts only `vSEMVER` tags matching `Cargo.toml`, builds Linux,
Windows, and macOS for `x86_64` and `aarch64`, runs the release binary's `--version` and `--smoke`,
and publishes only the file set covered by the verified `SHA256SUMS`. GitHub artifact attestations
cover that set. The workflow currently does not claim Apple notarization or Authenticode signing;
those require repository-held signing identities and a separate secret-backed signing step.

## Machine evidence

The local deterministic gate is:

```sh
cargo fmt --all -- --check
cargo test --locked --bin zec -- --test-threads=1
cargo test --locked --test update_cli -- --test-threads=1
cargo test --locked --test e2e_tui \
  extensions_themes_settings_and_keymap_run_through_the_actual_binary \
  -- --test-threads=1
cargo test --locked --test e2e_tui \
  markdown_and_images_run_through_zed_project_in_the_actual_binary \
  -- --exact --test-threads=1
cargo test --locked --test e2e_tui \
  large_file_opens_navigates_edits_and_saves_in_the_actual_binary \
  -- --exact --test-threads=1
ZEC_REQUIRE_REMOTE_SSH=1 cargo test --locked --test e2e_tui \
  remote_ssh_uses_zed_project_authorities_in_the_actual_binary \
  -- --exact --test-threads=1
python3 script/release-manifest self-test
```

On 2026-08-26 the 245-test binary unit target, all three actual-binary update CLI cases, the Beta 2
ecosystem, rich-content, large-file, and SSH PTY cases, and the release-manifest self-test passed
locally. The ecosystem PTY case indexes a real installed theme
extension, installs/rebuilds/uninstalls a development extension, applies and persists its theme,
opens the actual settings/keymap buffers, and performs the interactive update check. The CLI cases
verify check/download/verify success, atomically self-update a disposable actual-binary copy, and
prove that a checksum mismatch creates no output. The SSH case starts an isolated localhost `sshd`,
uploads the actual pinned remote-server binary, accepts worktree trust, edits and saves a remote
buffer, runs a remote integrated-terminal command and Zed task, stages through remote GitStore,
searches through remote BufferStore, creates a project entry, exits cleanly, and tears down all
connection processes. The rich-content case covers live Markdown, local heading links, bounded image
fallback, Kitty graphics/deletion, direct image tabs, session restore, overlay precedence, and
standalone image startup. The large-file case covers 100,000 lines, a 64 KiB line, edit/save, the
1 GiB Linux `VmHWM` ceiling, and terminal restoration.

## Remaining Beta 2 work

- public-registry install/update and HTTPS update evidence in a controlled hosted environment;
- Apple Developer ID signing/notarization and Windows Authenticode signing, if credentials are made
  available; fresh-machine installation and ConPTY/native-platform acceptance for all six targets;
- browser/media launch acceptance on every supported desktop platform;
- WSL and Docker/Podman native-platform acceptance, reconnect/fault-injection coverage, and hosted
  exact-version remote-server download evidence;
- memory-pressure and latency evidence on release builds, plus promotion of candidate capabilities
  from immutable hosted artifacts.
