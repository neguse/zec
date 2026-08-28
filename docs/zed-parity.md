# Zed experience parity contract

Status: Accepted (2026-08-26)

Implementation coverage: all 27 registered capabilities are implemented candidates (2026-08-28).
No capability is `verified` until the completion rule below is satisfied.

## Goal

zec's long-term goal is to complete Zed's editing, language intelligence,
workspace, Git, run/debug, extension, remote, AI, and collaboration
workflows from the console. Reproducing GPUI pixel-for-pixel is not a
goal; parity means applying the same operations to the same Zed models and
obtaining the same persistent state and external effects.

The normative ledger of target capabilities and their status is
[`zed-parity-v1.json`](zed-parity-v1.json). Completion phrasing in
documents, issues, or manual dogfooding must never move the ledger to
`verified`.

## Delivery modes

Each capability ships in one of the following modes. There is no
`excluded` mode that drops a capability just because the console makes it
awkward.

- `faithful`: Zed's domain model and actions are the authority; the
  terminal converts only input and output.
- `terminal-adapted`: the same state/actions project onto cells, text,
  terminal image protocols, and the like.
- `external-bridge`: media a terminal cannot own — audio, screen share, a
  browser — connect to an explicit process or OS service, while state,
  permissions, start/stop, and errors stay operable inside zec.

Where terminal capability falls short, fall back to an available mode or
display the required capability and an alternative operation. Input must
never be ignored, and an operation that did not succeed must never be
shown as successful.

## Architectural invariants

1. Text, cursor, selection, transactions, undo, and the display map take
   Zed Editor/Buffer as authority.
2. Worktrees, LSP, Git, tasks, DAP, settings, toolchains, and remote
   projects take Zed Project stores as authority. Nothing syncs into a
   parallel zec model.
3. Panes, items, focus, modals, and navigation history operate through
   one Terminal Workspace model. No per-feature event loops or focus
   flags are added.
4. The renderer reads only immutable snapshots and never mutates domain
   state during a frame.
5. Where Zed's public APIs fall short, a small presentation-neutral hook
   is shaped for upstreaming before copying logic into zec. Patches are
   tested per pinned Zed revision.
6. Network, process execution, extensions, agents, collaboration, and
   remote connections have permission boundaries with no silent
   elevation.
7. Each milestone fully re-runs every earlier canonical suite.
   Regressions caused by later features are never bypassed as known
   constraints.

## Terminal Workspace primitives

The presentation types shared by later features are fixed as:

- `Item`: Editor, MultiBuffer, Terminal, Diff, Markdown, Image, Notebook,
  Settings
- `Layout`: tab strip, split tree, dock, panel, overlay stack
- `Overlay`: prompt, completion, hover, menu, picker, confirmation,
  notification
- `Collection`: list, tree, table, virtualized result set
- `Action`: Zed action id, key context, availability, dispatch result
- `Capability`: keyboard protocol, color, mouse, clipboard, image,
  hyperlink, focus events

Existing single-purpose UIs — Quick Open, search, Save As — move onto the
same reducer at the milestone that introduces the corresponding shared
primitive. During migration, text and Zed state are still never
duplicated into the primitives.

## Milestones

### Alpha 1: repository editing loop

Guarantees file discovery, basic project search, multi-file editing,
saving, and terminal lifecycle. The existing [`alpha-1.md`](alpha-1.md)
is the normative contract.

### Alpha 2: Project-backed language editing loop

Completes the Zed Project, settings, command palette, LSP, completion,
diagnostics, navigation, code actions, rename, formatting, and
language-oriented MultiBuffers from the console. The normative contract
is [`alpha-2.md`](alpha-2.md).

### Alpha 3: terminal workspace

Integrates panes/docks, the project panel, outline, breadcrumbs, complete
project search/replace, navigation history, advanced editing, and session
restore onto the shared Terminal Workspace. The normative contract is
[`alpha-3.md`](alpha-3.md). Implementation and the local shortened
actual-binary suite are complete; fixed-scale hosted evidence is pending.

### Beta 1: local development loop

Guarantees Git, the integrated terminal, tasks/tests, the DAP debugger,
REPL/notebook, and crash recovery. Git, terminal, tasks, the DAP
debugger, the debug console, the native Zed Notebook, and the
session/crash recovery inherited from Alpha 3 are implemented candidates;
[`beta-1.md`](beta-1.md) defines the contract and local machine evidence.
Notebook has independent actual-binary kernel evidence and is not
inferred from the completion of the debug console.

### Beta 2: ecosystem and remote

Guarantees extensions, themes, the full keymap, package/update, the
large-file path, rich content, SSH/WSL/dev-container, and
distribution/compatibility across Linux/Windows/macOS. The
ExtensionStore/host, development extensions, theme/icon themes,
settings/keymap, manifest-verified updates, 6-target release assembly,
and the SSH remote workspace over Zed's remote protocol are implemented
candidates; Markdown preview, image protocols/fallbacks,
image tabs/sessions, and the 100k-line / 64 KiB-line large-file path are
implemented candidates as well. [`beta-2.md`](beta-2.md) defines the
contract and local machine evidence. Hosted proof against the public
registry, OS signing, and native-platform proof for WSL/containers remain
incomplete and must not be inferred from the mere existence of the
distribution workflow or the SSH proof.

### Parity 1: AI and collaboration

Zed Agent, external ACP agents, MCP, skills/instructions, edit
prediction, the inline assistant, channels, following, and channel notes
are integrated. Voice and screen share are `external-bridge`: explicit
confirmation, capability detection, start/stop/error, and process
collection are operated from zec. Implementation, authority, safety
conditions, and actual-binary evidence take
[`parity-1.md`](parity-1.md) as the normative contract.

AI, co-editing, and media are implemented candidates. Two-client
simultaneous editing on a live Zed channel, each provider,
parallel-agent, desktop capture/playback, and macOS/Windows hosted
evidence remain incomplete and must not be inferred as `verified` from
fixture/local PTY success.

## Completion rule

The long-term goal is declared complete only when all of the following
hold.

1. Every capability in `zed-parity-v1.json` is `verified`.
2. Every capability's evidence points at a retry-free canonical CI run on
   the default branch.
3. A diff audit against the pinned Zed revision shows no unregistered new
   user-facing domain.
4. The comprehensive actual-binary scenario succeeds from a fresh install
   on every platform.
5. Fallbacks and errors under missing terminal capabilities are verified
   by PTY/ConPTY tests.
6. Every invariant — source of truth, process cleanup, data safety,
   permission boundaries — passes the regression suites.
