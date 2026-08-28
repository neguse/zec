# Parity 1 contract: AI, collaboration, media, and Notebook

Contract status: Implemented candidate (2026-08-28)

Parity 1 brings Zed's Agent/ACP/MCP, edit prediction, inline assistant, channels, channel notes,
following, invitations, external voice/screen bridges, and native Notebook editor into the shared
Terminal Workspace. The pinned Zed revision remains the domain authority; zec owns bounded terminal
input, projection, focus routing, explicit permission prompts, and process cleanup.

All four capabilities covered here are `candidate`, not `verified`. Promotion still requires a
retry-free default-branch hosted run and the live-service/platform evidence described below.

## Outcome

A trusted worktree can perform these workflows from the console:

- open the in-process Zed Agent or an explicitly configured stdio ACP agent, stream its thread,
  inspect tool calls, approve or reject permission requests, cancel generation, and create a new
  session;
- inspect/select ACP commands, models, modes, configuration, saved sessions, authentication methods,
  project skills, effective instructions, and Project-owned MCP servers;
- use provider-aware Zed/Copilot/Codestral/Ollama/OpenAI-compatible edit prediction and accept all,
  the next word, or the next line through Zed Editor transactions;
- stream an inline-assistant transformation, preview its Buffer transaction, accept/reject it, and
  undo an accepted edit;
- sign in/out, refresh Zed channels, create channels, accept/decline invitations, open a live
  `ChannelBuffer`, edit shared notes across splits, and follow a collaborator's channel-note cursor;
- start and stop explicitly configured voice or screen-share bridge processes after a per-launch
  confirmation, with channel URL and media kind passed as arguments;
- open `.ipynb` files through Zed `NotebookItem`/`NotebookEditor`, edit/add/delete/reorder code and
  Markdown cells, run a cell or all cells, clear outputs, interrupt/restart the Jupyter kernel, save
  valid nbformat JSON, and share one Notebook authority across split views.

## Authority and safety invariants

1. `AcpThread` owns conversation entries, tool-call state, permission options, session status,
   cancellation, and ACP session replacement. Terminal history/output is bounded presentation only.
2. With no override, `NativeAgentServer` and Zed `ThreadStore` create the Agent session. An external
   agent is launched only from validated `ZEC_ACP_AGENT` JSON; command, arguments, environment, and
   identifiers are size/NUL bounded.
3. MCP state comes from the active Project's `ContextServerStore`; AGENTS instructions and skills are
   discovered from the trusted project. Unknown slash commands are passed to ACP rather than
   reinterpreted by a second agent protocol.
4. Edit predictions use the provider selected by Zed language settings and organization policy.
   Inline-assistant edits use a Zed language-model request and one Buffer transaction; stale source
   generations are rejected before preview application.
5. Production collaboration reads `Client`, `UserStore`, and `ChannelStore`. Shared text remains a
   `ChannelBuffer`, including collaborators and replica metadata; zec does not mirror CRDT text.
6. Channel creation and invitation responses call `ChannelStore`; sign-in/out calls Zed `Client`.
   The deterministic fixture exists only for offline acceptance and is not live-service evidence.
7. Media requires both `ZEC_MEDIA_BRIDGE` configuration and the detected/declared external-media
   capability. Every start requires `y` confirmation. zec owns the child, reports exit/failure, and
   kills and waits for voice/screen children on stop or drop.
8. `NotebookEditor` owns cells, Zed cell editors, execution requests, and Jupyter routing. The normal
   Project Buffer remains the save/session authority; snapshots are synchronized only when native
   Notebook state changes.
9. Rich outputs already present in a notebook are retained by cell ID because this pinned upstream
   `NotebookEditor::to_notebook` omits some display-data variants. Running or clearing a cell
   invalidates that retained output instead of silently restoring stale data.
10. The pinned upstream can leave a local kernel process alive when it is restarted or closed while
    still starting. zec binds the kernel connection filename to the Notebook Entity and terminates
    only matching descendants/process groups during restart cleanup and final Drop.
11. Agent, collaboration, media, terminal, task, debugger, and Notebook process starts remain blocked
    behind worktree trust. Missing providers, credentials, kernels, bridge capability, or server
    access are visible failures, never synthetic success.

## Console routes

| Workflow | Route |
| --- | --- |
| Toggle Agent panel | `Ctrl-Shift-A` or `F1` → `Toggle Agent Panel` |
| Agent submit / newline / cancel / new session | `Enter` / `Shift-Enter` / `Ctrl-C` / `Ctrl-N` |
| Agent inventories | `/commands`, `/models`, `/modes`, `/config`, `/sessions` |
| Agent project/service state | `/skills`, `/instructions`, `/mcp`, `/auth` |
| Agent session/auth lifecycle | `/load ID`, `/close ID`, `/auth ID`, `/logout`, `/new`, `/cancel` |
| Inline assistant | `Ctrl-Enter`; `Enter` accepts and `Esc` rejects a preview |
| Edit prediction | `Alt-\` show; `Alt-L/K/J` accept all/word/line |
| Toggle collaboration panel | `Ctrl-Alt-C` |
| Channels / people / follow | arrows, `Enter`, `Tab`, `f` |
| Create / invitation / account | `c`, `a`/`d`, `i`; `r` refreshes |
| Voice / screen bridge | `v` / `s`, then `y` to start |
| Notebook cell selection/edit | arrows / `Enter` (`Esc` returns to command mode) |
| Notebook add/delete/move | `b` code, `m` Markdown, `dd`, `Alt-Up/Down` |
| Notebook run/control | `Ctrl-Enter`, `Shift-Enter`, `R`, `c`, `i`, `r` |

## External configuration

`ZEC_ACP_AGENT` is optional. If absent, zec uses the in-process Zed Agent. If present, it is strict
JSON with `command`, optional `args`, optional `env`, and optional `id`:

```json
{"command":"/absolute/path/to/agent","args":[],"env":{},"id":"my-agent"}
```

`ZEC_MEDIA_BRIDGE` is strict JSON with `command`, optional `args`, and optional `env`. For a confirmed
launch zec executes `command args... <voice|screen> <channel-url>`. Set `ZEC_EXTERNAL_MEDIA=1` only
after a desktop bridge is actually available; the capability flag alone does not configure one.

## Machine evidence

The deterministic local gate is:

```sh
cargo fmt --all -- --check
cargo check --locked --offline --bin zec
cargo test --locked --offline --bin zec -- --test-threads=1
cargo test --locked --offline --test parity_contract -- --test-threads=1
cargo test --locked --offline --test pty_acceptance -- --test-threads=1
```

`pty_acceptance` exercises the real `zec` binary for:

- `zed_edit_prediction_renders_and_accepts_through_the_actual_binary`;
- `zed_inline_assistant_streams_previews_rejects_accepts_and_undoes_through_the_actual_binary`;
- `beta_3_agent_acp_permissions_and_mcp_run_through_the_actual_binary`;
- `beta_3_native_zed_agent_and_local_commands_run_through_the_actual_binary`;
- `parity_1_collaboration_notes_follow_invites_and_media_run_through_the_actual_binary`;
- `parity_1_notebook_cells_outputs_kernel_controls_and_cleanup_run_through_the_actual_binary`.

The Notebook scenario additionally proves trust gating, native kernelspec launch, Markdown/code/stream
and image fallback projection, split authority sharing, nbformat persistence, failed-kernel output,
clear-output persistence, restart cleanup, final process cleanup, raw mode, and terminal restoration.

## Evidence still required for `verified`

- retry-free canonical hosted artifacts for the complete locked gate;
- a live Zed account/channel round trip with two independently connected clients and concurrent
  channel-note edits/following, without the deterministic fixture;
- real provider runs for each supported edit-prediction family and at least one native and one
  external ACP agent, including provider authentication and tool permission denial;
- a real Jupyter kernel producing new rich display-data (image/HTML/JSON) after launch, plus missing,
  crashing, remote, SSH, WSL, and Windows kernel cases;
- macOS and Windows/ConPTY lifecycle evidence for Agent processes and external media bridges;
- desktop voice/screen bridge integration on a host with actual capture/playback permissions.

These are verification gaps, not permission to mark a failed or unavailable operation as successful.
