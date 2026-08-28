use std::{
    ffi::{OsStr, OsString},
    fs,
    io::{self, Read as _, Write as _},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail, ensure};
#[cfg(unix)]
use nix::{
    sys::{
        signal::{Signal, killpg},
        termios::{LocalFlags, Termios},
        wait::{WaitPidFlag, WaitStatus, waitpid},
    },
    unistd::Pid,
};
use portable_pty::{
    Child, CommandBuilder, ExitStatus, MasterPty, PtyPair, PtySize, native_pty_system,
};
#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt as _, symlink};
use vt100::{MouseProtocolEncoding, MouseProtocolMode, Parser};

const INITIAL_SIZE: PtySize = PtySize {
    rows: 32,
    cols: 160,
    pixel_width: 0,
    pixel_height: 0,
};
const RESIZED: PtySize = PtySize {
    rows: 27,
    cols: 118,
    pixel_width: 0,
    pixel_height: 0,
};
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const REMOTE_STARTUP_TIMEOUT: Duration = Duration::from_secs(180);
const ACTION_TIMEOUT: Duration = Duration::from_secs(15);
const EXIT_TIMEOUT: Duration = Duration::from_secs(15);
const EVENT_POLL: Duration = Duration::from_millis(100);
const TRANSCRIPT_LIMIT: usize = 256 * 1024;
const DIAGNOSTIC_TAIL: usize = 8 * 1024;

const CTRL_A: &[u8] = b"\x01";
const CTRL_D: &[u8] = b"\x04";
const CTRL_G: &[u8] = b"\x07";
const CTRL_N: &[u8] = b"\x0e";
const CTRL_P: &[u8] = b"\x10";
const CTRL_Q: &[u8] = b"\x11";
const CTRL_S: &[u8] = b"\x13";
const CTRL_U: &[u8] = b"\x15";
const CTRL_W: &[u8] = b"\x17";
const CTRL_Z: &[u8] = b"\x1a";
const CTRL_PAGE_DOWN: &[u8] = b"\x1b[6;5~";
// Kitty's CSI-u encoding keeps Ctrl-: distinct from the legacy Ctrl-Z byte.
const CTRL_COLON_KITTY: &[u8] = b"\x1b[58;5u";
const F7: &[u8] = b"\x1b[18~";
const F9: &[u8] = b"\x1b[20~";
const F10: &[u8] = b"\x1b[21~";
const SHIFT_F10: &[u8] = b"\x1b[21;2~";
const F11: &[u8] = b"\x1b[23~";
const CTRL_F11: &[u8] = b"\x1b[23;5~";
const SHIFT_F11: &[u8] = b"\x1b[23;2~";
const F2: &[u8] = b"\x1bOQ";
const F1: &[u8] = b"\x1bOP";
const F3: &[u8] = b"\x1bOR";
const F4: &[u8] = b"\x1bOS";
const F5: &[u8] = b"\x1b[15~";
const F8: &[u8] = b"\x1b[19~";
const DELETE: &[u8] = b"\x1b[3~";
const CTRL_ALT_LEFT: &[u8] = b"\x1b[1;7D";
const CTRL_ALT_RIGHT: &[u8] = b"\x1b[1;7C";
const CTRL_ALT_SHIFT_LEFT: &[u8] = b"\x1b[1;8D";
const ALT_F: &[u8] = b"\x1bf";
const ALT_Z: &[u8] = b"\x1bz";
const SHIFT_ALT_DOWN: &[u8] = b"\x1b[1;4B";
const ENTER: &[u8] = b"\r";

// This is one execute! call in TerminalSession::restore. Keeping the full ordered
// sequence here catches a regression where only some terminal features are reset.
const CLEANUP_ESCAPES: &[u8] = concat!(
    "\x1b[>4m",
    "\x1b[?25h",
    "\x1b[?1006l",
    "\x1b[?1015l",
    "\x1b[?1003l",
    "\x1b[?1002l",
    "\x1b[?1000l",
    "\x1b[?1004l",
    "\x1b[?2004l",
    "\x1b[?1049l",
)
.as_bytes();

const KITTY_CLEANUP_ESCAPES: &[u8] = concat!(
    "\x1b[<1u",
    "\x1b[?25h",
    "\x1b[?1006l",
    "\x1b[?1015l",
    "\x1b[?1003l",
    "\x1b[?1002l",
    "\x1b[?1000l",
    "\x1b[?1004l",
    "\x1b[?2004l",
    "\x1b[?1049l",
)
.as_bytes();

#[test]
fn zed_edit_prediction_renders_and_accepts_through_the_actual_binary() -> Result<()> {
    const ORIGINAL: &str = "fn main() {}\n";
    const PREDICTION: &str = "PREDICTED_";

    let temp = tempfile::tempdir().context("create edit prediction PTY fixture")?;
    let path = temp.path().join("prediction.rs");
    fs::write(&path, ORIGINAL).context("write edit prediction fixture")?;

    let pair = open_pty()?;
    let termios_before = capture_baseline(&pair)?;
    let environment = [(
        OsStr::new("ZEC_EDIT_PREDICTION_FIXTURE"),
        OsStr::new(PREDICTION),
    )];
    let mut session = PtySession::spawn_with_env(pair, &[path.as_os_str()], &environment)?;
    session.wait_ready()?;
    session.assert_raw_mode_enabled(&termios_before)?;
    session.wait_for_screen("edit prediction keymap ready", ACTION_TIMEOUT, |screen| {
        screen.contains("user keymap reloaded")
    })?;

    session.send(F1)?;
    session.wait_for_screen(
        "show prediction command palette",
        ACTION_TIMEOUT,
        |screen| screen.contains("Command palette:"),
    )?;
    session.paste("Show Edit Prediction")?;
    session.send(ENTER)?;
    session.wait_for_screen("Zed DisplayMap edit prediction", ACTION_TIMEOUT, |screen| {
        screen.contains(PREDICTION) && screen.contains("edit prediction visible")
    })?;
    ensure!(
        fs::read_to_string(&path)? == ORIGINAL,
        "rendering a prediction changed the file before acceptance"
    );

    session.send(F1)?;
    session.wait_for_screen(
        "accept prediction command palette",
        ACTION_TIMEOUT,
        |screen| screen.contains("Command palette:"),
    )?;
    session.paste("Accept Edit Prediction")?;
    session.send(ENTER)?;
    session.wait_for_screen("accepted edit prediction", ACTION_TIMEOUT, |screen| {
        screen.contains(&format!("{PREDICTION}fn main"))
            && screen.contains("edit prediction accepted")
    })?;
    session.send(CTRL_S)?;
    let expected = format!("{PREDICTION}{ORIGINAL}");
    session.wait_until("saved accepted edit prediction", ACTION_TIMEOUT, |_| {
        fs::read_to_string(&path).is_ok_and(|contents| contents == expected)
    })?;
    ensure!(
        fs::read_to_string(&path)? == expected,
        "accepted edit prediction did not reach the source buffer"
    );

    session.send(CTRL_Q)?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(
        status.success(),
        "edit prediction PTY exit failed: {status}"
    );
    session.assert_terminal_restored(&termios_before)
}

#[test]
fn zed_inline_assistant_streams_previews_rejects_accepts_and_undoes_through_the_actual_binary()
-> Result<()> {
    const ORIGINAL: &str = "fn main() {}\n";
    const REPLACEMENT: &str = "fn main() { println!(\"hello from zec\"); }";

    let temp = tempfile::tempdir().context("create inline assistant PTY fixture")?;
    let path = temp.path().join("inline-assistant.rs");
    fs::write(&path, ORIGINAL).context("write inline assistant fixture")?;

    let pair = open_pty()?;
    let termios_before = capture_baseline(&pair)?;
    let environment = [(
        OsStr::new("ZEC_INLINE_ASSIST_FIXTURE_RESPONSE"),
        OsStr::new(REPLACEMENT),
    )];
    let mut session = PtySession::spawn_with_env(pair, &[path.as_os_str()], &environment)?;
    session.wait_ready()?;
    session.assert_raw_mode_enabled(&termios_before)?;
    session.wait_for_screen("inline assistant keymap ready", ACTION_TIMEOUT, |screen| {
        screen.contains("user keymap reloaded")
    })?;

    session.send(F1)?;
    session.wait_for_screen(
        "inline assistant command palette",
        ACTION_TIMEOUT,
        |screen| screen.contains("Command palette:"),
    )?;
    session.paste("Inline Assistant")?;
    session.send(ENTER)?;
    session.wait_for_screen("inline assistant prompt", ACTION_TIMEOUT, |screen| {
        screen.contains("Inline Assistant:")
            && screen.contains("current line or selected complete lines")
    })?;
    session.paste("add a greeting")?;
    session.send(ENTER)?;
    session.wait_for_screen("inline assistant diff preview", ACTION_TIMEOUT, |screen| {
        screen.contains("Inline Assistant · Diff Preview")
            && screen.contains("hello from zec")
            && screen.contains("Esc reject")
    })?;
    ensure!(
        fs::read_to_string(&path)? == ORIGINAL,
        "inline assistant preview changed the file before acceptance"
    );

    session.send(b"\x1b")?;
    session.wait_for_screen("rejected inline assistant edit", ACTION_TIMEOUT, |screen| {
        screen.contains("fn main() {}")
            && screen.contains("inline assistant edit rejected")
            && !screen.contains("hello from zec")
    })?;
    ensure!(
        fs::read_to_string(&path)? == ORIGINAL,
        "rejecting the inline assistant preview changed the file"
    );

    session.send(F1)?;
    session.wait_for_screen(
        "second inline assistant command palette",
        ACTION_TIMEOUT,
        |screen| screen.contains("Command palette:"),
    )?;
    session.paste("Inline Assistant")?;
    session.send(ENTER)?;
    session.wait_for_screen("second inline assistant prompt", ACTION_TIMEOUT, |screen| {
        screen.contains("Inline Assistant:")
    })?;
    session.paste("add a greeting")?;
    session.send(ENTER)?;
    session.wait_for_screen(
        "second inline assistant diff preview",
        ACTION_TIMEOUT,
        |screen| {
            screen.contains("Inline Assistant · Diff Preview") && screen.contains("hello from zec")
        },
    )?;
    session.send(ENTER)?;
    session.wait_for_screen("accepted inline assistant edit", ACTION_TIMEOUT, |screen| {
        screen.contains("hello from zec") && screen.contains("inline assistant edit accepted")
    })?;
    session.send(CTRL_S)?;
    let expected = format!("{REPLACEMENT}\n");
    session.wait_until("saved inline assistant edit", ACTION_TIMEOUT, |_| {
        fs::read_to_string(&path).is_ok_and(|contents| contents == expected)
    })?;

    session.send(CTRL_Z)?;
    session.wait_for_screen("undone inline assistant edit", ACTION_TIMEOUT, |screen| {
        screen.contains("fn main() {}") && !screen.contains("hello from zec")
    })?;
    session.send(CTRL_S)?;
    session.wait_until("saved inline assistant undo", ACTION_TIMEOUT, |_| {
        fs::read_to_string(&path).is_ok_and(|contents| contents == ORIGINAL)
    })?;

    session.send(CTRL_Q)?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(
        status.success(),
        "inline assistant PTY exit failed: {status}"
    );
    session.assert_terminal_restored(&termios_before)
}

#[test]
fn actual_binary_preserves_edits_and_restores_the_pty_on_every_exit_path() -> Result<()> {
    let temp = tempfile::tempdir().context("create PTY acceptance fixture")?;

    normal_edit_undo_resize_save_and_quit(temp.path())?;
    workspace_split_focus_move_and_collapse(temp.path())?;
    project_panel_previews_and_replaces_by_entry_identity(temp.path())?;
    project_panel_mutations_preserve_dirty_buffer_identity(temp.path())?;
    outline_filters_follows_and_jumps_through_zed_symbols(temp.path())?;
    advanced_editor_actions_use_zed_display_and_selection_state(temp.path())?;
    editor_projection_settings_render_whitespace_and_guides(temp.path())?;
    mouse_drag_multi_click_and_additive_selection_use_zed_ranges(temp.path())?;
    workspace_session_restores_folds_wrap_and_multiple_cursors(temp.path())?;
    corrupt_workspace_generation_is_quarantined_and_falls_back(temp.path())?;
    workspace_session_restores_layout_dock_and_unsaved_content(temp.path())?;
    workspace_session_restores_a_live_editable_multibuffer(temp.path())?;
    failed_save_keeps_dirty_text_and_quit_guard(temp.path())?;
    restricted_worktree_requires_confirmation_before_lsp(temp.path())?;
    // Process signals, job control, and symlink aliases are Unix semantics.
    #[cfg(unix)]
    {
        periodic_session_snapshot_survives_sigkill(temp.path())?;
        directory_quick_open_deduplicates_symlink_alias(temp.path())?;
        signal_exit_restores_terminal(temp.path(), Signal::SIGINT)?;
        signal_exit_restores_terminal(temp.path(), Signal::SIGQUIT)?;
        signal_exit_restores_terminal(temp.path(), Signal::SIGTERM)?;
        signal_exit_restores_terminal(temp.path(), Signal::SIGHUP)?;
        suspend_restores_and_resume_reenters_the_terminal(temp.path())?;
    }

    Ok(())
}

#[test]
fn terminal_git_and_tasks_run_through_the_actual_binary() -> Result<()> {
    const READY: &str = "E2E_WORKSPACE_READY";
    const TERMINAL_READY: &str = "E2E_TERMINAL_READY";
    const TASK_READY: &str = "E2E_TASK_READY";

    let temp = tempfile::tempdir().context("create Beta 1 PTY fixture")?;
    let root = temp.path().join("beta-1-repo");
    fs::create_dir_all(root.join(".zed")).context("create Beta 1 fixture")?;
    let readme = root.join("README.md");
    fs::write(&readme, format!("{READY}\n")).context("write Beta 1 README")?;
    fs::write(
        root.join(".zed/tasks.json"),
        r#"[
          {
            "label": "Beta 1 Task",
            "command": "sh",
            "args": ["-c", "printf 'E2E_TASK_READY\\n' | tee -a beta1-task.log"],
            "reveal": "always",
            "hide": "never"
          }
        ]"#,
    )
    .context("write Beta 1 tasks")?;
    git(&root, &["init"])?;
    git(&root, &["config", "user.email", "zec@example.invalid"])?;
    git(&root, &["config", "user.name", "zec acceptance"])?;
    git(&root, &["add", "README.md"])?;
    git(&root, &["commit", "-m", "fixture"])?;
    fs::write(&readme, format!("{READY}\nmodified for Git panel\n"))
        .context("modify Beta 1 README")?;

    let pair = open_pty()?;
    let termios_before = capture_baseline(&pair)?;
    let mut session = PtySession::spawn(pair, &[root.as_os_str(), readme.as_os_str()])?;
    session.wait_for_screen("Beta 1 worktree trust", STARTUP_TIMEOUT, |screen| {
        screen.contains("Worktree Trust") && screen.contains("beta-1-repo")
    })?;
    session.send(ENTER)?;
    session.wait_for_screen("Beta 1 workspace ready", ACTION_TIMEOUT, |screen| {
        screen.contains(READY) && !screen.contains("Worktree Trust")
    })?;

    session.send(F3)?;
    session.wait_for_screen("integrated Zed terminal", ACTION_TIMEOUT, |screen| {
        screen.contains("Terminal ·") && screen.contains("terminal 1 focused")
    })?;
    session.paste(&format!(
        "printf '{TERMINAL_READY}\\n' | tee beta1-terminal.log"
    ))?;
    session.send(ENTER)?;
    session.wait_until(
        "terminal command output and side effect",
        ACTION_TIMEOUT,
        |_| {
            fs::read_to_string(root.join("beta1-terminal.log"))
                .is_ok_and(|text| text.contains(TERMINAL_READY))
        },
    )?;
    session.wait_for_screen("terminal command projected", ACTION_TIMEOUT, |screen| {
        screen.contains(TERMINAL_READY)
    })?;
    session.send(F3)?;
    session.wait_for_screen("Beta 1 terminal hidden", ACTION_TIMEOUT, |screen| {
        screen.contains("terminal panel hidden")
    })?;

    session.send(F1)?;
    session.wait_for_screen("Git command palette", ACTION_TIMEOUT, |screen| {
        screen.contains("Command palette:")
    })?;
    session.paste("Toggle Git Panel")?;
    session.send(ENTER)?;
    session.wait_for_screen("Zed Git repository snapshot", ACTION_TIMEOUT, |screen| {
        screen.contains("Git focused") && screen.contains("README.md") && screen.contains("Changes")
    })?;
    session.send(b"a")?;
    session.wait_for_screen("stage all through Zed GitStore", ACTION_TIMEOUT, |screen| {
        screen.contains("staged all changes")
    })?;
    session.wait_until("Git index changed", ACTION_TIMEOUT, |_| {
        Command::new("git")
            .args(["diff", "--cached", "--name-only"])
            .current_dir(&root)
            .output()
            .is_ok_and(|output| {
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains("README.md")
            })
    })?;
    session.send(b"\x1b")?;
    session.wait_for_screen("return from Git panel", ACTION_TIMEOUT, |screen| {
        screen.contains("editor focused")
    })?;

    session.send(F1)?;
    session.paste("Run Task")?;
    session.send(ENTER)?;
    session.wait_for_screen("Zed TaskInventory picker", ACTION_TIMEOUT, |screen| {
        screen.contains("Tasks") && screen.contains("Beta 1 Task")
    })?;
    session.send(ENTER)?;
    session.wait_for_screen("Zed task terminal output", ACTION_TIMEOUT, |screen| {
        screen.contains(TASK_READY)
    })?;
    session.wait_until("task side effect", ACTION_TIMEOUT, |_| {
        fs::read_to_string(root.join("beta1-task.log"))
            .is_ok_and(|text| text.lines().count() == 1 && text.contains(TASK_READY))
    })?;
    session.wait_for_screen("Zed task completion", ACTION_TIMEOUT, |screen| {
        screen.contains("finished successfully") || screen.contains("task finished")
    })?;

    session.send(F1)?;
    session.paste("Rerun Last Task")?;
    session.send(ENTER)?;
    session.wait_until("rerun task side effect", ACTION_TIMEOUT, |_| {
        fs::read_to_string(root.join("beta1-task.log")).is_ok_and(|text| text.lines().count() == 2)
    })?;

    session.send(F3)?;
    session.send(CTRL_Q)?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(status.success(), "Beta 1 zec exit failed: {status}");
    session.assert_terminal_restored(&termios_before)
}

#[cfg(unix)]
#[test]
fn remote_ssh_uses_zed_project_authorities_in_the_actual_binary() -> Result<()> {
    const READY: &str = "REMOTE_WORKSPACE_READY";
    const EDITED: &str = "REMOTE_EDITED";
    const TERMINAL_READY: &str = "REMOTE_TERMINAL_OK";
    const TASK_READY: &str = "REMOTE_TASK_OK";

    let Some(sshd) = sshd_executable() else {
        ensure!(
            std::env::var_os("ZEC_REQUIRE_REMOTE_SSH").is_none(),
            "ZEC_REQUIRE_REMOTE_SSH is set but sshd is unavailable"
        );
        eprintln!("skipping Beta 2 remote acceptance: sshd is unavailable");
        return Ok(());
    };

    let temp = tempfile::tempdir().context("create Beta 2 remote fixture")?;
    let root = temp.path().join("remote-project");
    fs::create_dir_all(root.join(".zed")).context("create remote fixture")?;
    let readme = root.join("README.md");
    fs::write(&readme, format!("{READY}\nREMOTE_ORIGINAL\n")).context("write remote README")?;
    fs::write(
        root.join(".zed/tasks.json"),
        format!(
            r#"[
              {{
                "label": "Remote Acceptance Task",
                "command": "sh",
                "args": ["-c", "printf '{TASK_READY}\\n' | tee remote-task.log"],
                "reveal": "always",
                "hide": "never"
              }}
            ]"#
        ),
    )
    .context("write remote task")?;
    git(&root, &["init"])?;
    git(&root, &["config", "user.email", "zec@example.invalid"])?;
    git(&root, &["config", "user.name", "zec acceptance"])?;
    git(&root, &["add", "README.md", ".zed/tasks.json"])?;
    git(&root, &["commit", "-m", "remote fixture"])?;

    let daemon = SshdFixture::start(temp.path(), &sshd)?;
    let port = daemon.port.to_string();
    let remote_server = Path::new(env!("CARGO_BIN_EXE_zec-remote-server"));
    let arguments = vec![
        "remote".into(),
        "ssh".into(),
        "127.0.0.1".into(),
        "--user".into(),
        daemon.user.clone().into(),
        "--port".into(),
        port.into(),
        "--arg".into(),
        "-F".into(),
        "--arg".into(),
        "/dev/null".into(),
        "--arg".into(),
        "-i".into(),
        "--arg".into(),
        daemon.client_key.as_os_str().to_owned(),
        "--arg".into(),
        "-oStrictHostKeyChecking=no".into(),
        "--arg".into(),
        "-oUserKnownHostsFile=/dev/null".into(),
        root.as_os_str().to_owned(),
    ];
    let argument_refs = arguments
        .iter()
        .map(OsString::as_os_str)
        .collect::<Vec<_>>();
    let environment = [
        (
            OsStr::new("ZED_COPY_REMOTE_SERVER"),
            remote_server.as_os_str(),
        ),
        (OsStr::new("ZEC_DISABLE_UPDATE_CHECK"), OsStr::new("1")),
    ];
    let pair = open_pty()?;
    let termios_before = capture_baseline(&pair)?;
    let mut session = PtySession::spawn_with_env(pair, &argument_refs, &environment)?;
    session.wait_for_screen("remote worktree trust", REMOTE_STARTUP_TIMEOUT, |screen| {
        screen.contains("Worktree Trust") && screen.contains("remote-project")
    })?;
    session.send(ENTER)?;
    session.wait_for_screen("remote workspace", ACTION_TIMEOUT, |screen| {
        screen.contains(READY) && !screen.contains("Worktree Trust")
    })?;

    session.send(CTRL_G)?;
    session.paste("2:1")?;
    session.send(ENTER)?;
    session.paste(EDITED)?;
    session.send(CTRL_S)?;
    session.wait_until("remote save", ACTION_TIMEOUT, |_| {
        fs::read_to_string(&readme).is_ok_and(|text| text.contains("REMOTE_EDITEDREMOTE_ORIGINAL"))
    })?;

    session.send(F3)?;
    session.wait_for_screen("remote Zed terminal", ACTION_TIMEOUT, |screen| {
        screen.contains("Terminal ·") && screen.contains("terminal 1 focused")
    })?;
    session.paste(&format!(
        "printf '{TERMINAL_READY}\\n' | tee remote-terminal.log"
    ))?;
    session.send(ENTER)?;
    session.wait_until("remote terminal side effect", ACTION_TIMEOUT, |_| {
        fs::read_to_string(root.join("remote-terminal.log"))
            .is_ok_and(|text| text.contains(TERMINAL_READY))
    })?;
    session.send(F3)?;
    session.wait_for_screen("remote terminal hidden", ACTION_TIMEOUT, |screen| {
        screen.contains("terminal panel hidden")
    })?;

    session.send(F1)?;
    session.paste("Toggle Git Panel")?;
    session.send(ENTER)?;
    session.wait_for_screen("remote Zed GitStore", ACTION_TIMEOUT, |screen| {
        screen.contains("Git focused") && screen.contains("README.md")
    })?;
    session.send(b"a")?;
    session.wait_until("remote Git stage", ACTION_TIMEOUT, |_| {
        Command::new("git")
            .args(["diff", "--cached", "--name-only"])
            .current_dir(&root)
            .output()
            .is_ok_and(|output| {
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains("README.md")
            })
    })?;
    session.send(b"\x1b")?;
    session.wait_for_screen("return from remote Git panel", ACTION_TIMEOUT, |screen| {
        screen.contains("editor focused")
    })?;

    session.send(F1)?;
    session.paste("Run Task")?;
    session.send(ENTER)?;
    session.wait_for_screen("remote task picker", ACTION_TIMEOUT, |screen| {
        screen.contains("Remote Acceptance Task")
    })?;
    session.send(ENTER)?;
    session.wait_until("remote task side effect", ACTION_TIMEOUT, |_| {
        fs::read_to_string(root.join("remote-task.log")).is_ok_and(|text| text.contains(TASK_READY))
    })?;
    session.wait_for_screen("remote task completion", ACTION_TIMEOUT, |screen| {
        screen.contains("finished successfully") || screen.contains("task finished")
    })?;
    session.send(F3)?;
    session.wait_for_screen("remote task terminal hidden", ACTION_TIMEOUT, |screen| {
        screen.contains("terminal panel hidden")
    })?;

    session.send(F1)?;
    session.paste("Project Search")?;
    session.send(ENTER)?;
    session.paste(EDITED)?;
    session.wait_for_screen("remote BufferStore search", ACTION_TIMEOUT, |screen| {
        screen.contains("1/1") && screen.contains("README.md:2:1")
    })?;
    session.send(b"\x1b")?;
    session.wait_for_screen("remote search closed", ACTION_TIMEOUT, |screen| {
        !screen.contains("Project search:")
    })?;

    session.send(F7)?;
    session.wait_for_screen("remote project panel", ACTION_TIMEOUT, |screen| {
        screen.contains("Project") && screen.contains("README.md")
    })?;
    session.send(b"n")?;
    session.send(CTRL_U)?;
    session.paste("remote-created.txt")?;
    session.send(ENTER)?;
    session.wait_for_screen("remote mutation preview", ACTION_TIMEOUT, |screen| {
        screen.contains("preview ready") && screen.contains("remote-created.txt")
    })?;
    session.send(ENTER)?;
    session.wait_until("remote project mutation", ACTION_TIMEOUT, |_| {
        root.join("remote-created.txt").is_file()
    })?;

    session.send(CTRL_Q)?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(status.success(), "remote zec exit failed: {status}");
    session.assert_terminal_restored(&termios_before)?;
    drop(daemon);
    Ok(())
}

#[test]
fn markdown_and_images_run_through_zed_project_in_the_actual_binary() -> Result<()> {
    const READY: &str = "E2E_RICH_CONTENT_READY";
    const UPDATED: &str = "E2E_RICH_CONTENT_LIVE_UPDATE";
    const HEADING: &str = "Target Heading";

    let temp = tempfile::tempdir().context("create Beta 2 rich-content fixture")?;
    let root = temp.path().join("rich-content-project");
    fs::create_dir_all(root.join("assets")).context("create rich-content fixture")?;
    let readme = root.join("README.md");
    let markdown = format!(
        "# Console Rich Preview\n\n{READY}\n\n- [x] completed task\n\n[Jump to guide](guide.md#target-heading)\n\n![Checker](assets/checker.png)\n\n| left | right |\n|---|---|\n| one | two |\n"
    );
    fs::write(&readme, &markdown).context("write rich Markdown fixture")?;
    fs::write(
        root.join("guide.md"),
        format!("# Guide\n\nfiller\n\n## {HEADING}\n\nBETA2_LOCAL_LINK_TARGET\n"),
    )
    .context("write Markdown link target")?;
    let checker = image::DynamicImage::ImageRgba8(image::ImageBuffer::from_fn(2, 2, |x, y| {
        if (x + y) % 2 == 0 {
            image::Rgba([255, 0, 0, 255])
        } else {
            image::Rgba([0, 0, 255, 255])
        }
    }));
    checker
        .save(root.join("assets/checker.png"))
        .context("write PNG fixture")?;

    let pair = open_pty()?;
    let termios_before = capture_baseline(&pair)?;
    let fallback_environment = [
        (OsStr::new("ZEC_IMAGE_PROTOCOL"), OsStr::new("none")),
        (OsStr::new("ZEC_EXTERNAL_MEDIA"), OsStr::new("0")),
    ];
    let mut session = PtySession::spawn_with_env(pair, &[root.as_os_str()], &fallback_environment)?;
    session.wait_for_screen("rich-content worktree trust", STARTUP_TIMEOUT, |screen| {
        screen.contains("Worktree Trust") && screen.contains("rich-content-project")
    })?;
    session.send(ENTER)?;
    session.wait_for_screen("rich-content editor ready", ACTION_TIMEOUT, |screen| {
        screen.contains(READY) && !screen.contains("Worktree Trust")
    })?;

    open_markdown_preview(&mut session)?;
    session.wait_for_screen(
        "Zed-compatible Markdown projection",
        ACTION_TIMEOUT,
        |screen| {
            screen.contains("Markdown preview")
                && screen.contains("# Console Rich Preview")
                && screen.contains("• [x] completed task")
                && screen.contains("│ left │ right │")
                && screen.contains("Jump to guide[1]")
                && screen.contains("[image 2: Checker]")
        },
    )?;

    session.send(b"\t")?;
    session.send(ENTER)?;
    session.wait_for_screen(
        "local Markdown link through Zed Project",
        ACTION_TIMEOUT,
        |screen| {
            screen.contains("E2E_LOCAL_LINK_TARGET")
                && screen.contains("opened guide.md:5:1 through Zed Project")
        },
    )?;
    session.send(CTRL_W)?;
    session.wait_for_screen("return to Markdown source tab", ACTION_TIMEOUT, |screen| {
        screen.contains(READY) && screen.contains("tab closed")
    })?;

    open_markdown_preview(&mut session)?;
    session.send(b"\t\t")?;
    session.send(ENTER)?;
    session.wait_for_screen("metadata image fallback", ACTION_TIMEOUT, |screen| {
        screen.contains("Image: Checker")
            && screen.contains("PNG · 2×2")
            && screen.contains("metadata fallback is active")
    })?;
    session.send(b"\x1b")?;
    session.wait_for_screen("image back to Markdown", ACTION_TIMEOUT, |screen| {
        screen.contains("Markdown preview") && screen.contains("# Console Rich Preview")
    })?;
    session.send(b"\x1b")?;
    session.wait_for_screen("Markdown preview closed", ACTION_TIMEOUT, |screen| {
        screen.contains("Markdown preview closed; editor focused") && screen.contains(READY)
    })?;

    session.send(CTRL_P)?;
    session.wait_for_screen("image quick-open prompt", ACTION_TIMEOUT, |screen| {
        screen.contains("Quick open:")
    })?;
    session.paste("checker.png")?;
    session.wait_for_screen("image quick-open selection", ACTION_TIMEOUT, |screen| {
        screen.contains("assets/checker.png")
    })?;
    session.send(ENTER)?;
    session.wait_for_screen("direct image tab", ACTION_TIMEOUT, |screen| {
        screen.contains("Image: checker.png")
            && screen.contains("Image preview · none · Ctrl-W close")
            && screen.contains("checker.png [Image]")
    })?;
    session.send(CTRL_W)?;
    session.wait_for_screen("direct image tab closed", ACTION_TIMEOUT, |screen| {
        screen.contains(READY) && screen.contains("tab closed")
    })?;

    let updated_markdown = markdown.replace(READY, UPDATED);
    session.send(CTRL_A)?;
    session.paste(&updated_markdown)?;
    session.wait_for_screen("Markdown source edited", ACTION_TIMEOUT, |screen| {
        screen.contains(UPDATED)
    })?;
    open_markdown_preview(&mut session)?;
    session.wait_for_screen("live Markdown preview refresh", ACTION_TIMEOUT, |screen| {
        screen.contains(UPDATED) && !screen.contains(READY) && screen.contains("Markdown preview")
    })?;
    session.send(b"\x1b")?;
    session.wait_for_screen(
        "updated Markdown preview closed",
        ACTION_TIMEOUT,
        |screen| {
            screen.contains("Markdown preview closed; editor focused") && screen.contains(UPDATED)
        },
    )?;
    session.send(CTRL_S)?;
    session.wait_until("updated Markdown persisted", ACTION_TIMEOUT, |_| {
        fs::read_to_string(&readme)
            .is_ok_and(|text| text.contains(UPDATED) && !text.contains(READY))
    })?;
    session.send(CTRL_Q)?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(
        status.success(),
        "rich-content fallback exit failed: {status}"
    );
    session.assert_terminal_restored(&termios_before)?;

    let kitty_pair = open_pty()?;
    let kitty_termios = capture_baseline(&kitty_pair)?;
    let kitty_environment = [(OsStr::new("ZEC_IMAGE_PROTOCOL"), OsStr::new("kitty"))];
    let mut kitty =
        PtySession::spawn_with_env(kitty_pair, &[root.as_os_str()], &kitty_environment)?;
    kitty.wait_for_screen(
        "Kitty rich-content worktree trust",
        STARTUP_TIMEOUT,
        |screen| screen.contains("Worktree Trust") && screen.contains("rich-content-project"),
    )?;
    kitty.send(ENTER)?;
    kitty.wait_for_screen(
        "Kitty rich-content editor ready",
        ACTION_TIMEOUT,
        |screen| screen.contains(UPDATED) && !screen.contains("Worktree Trust"),
    )?;
    open_markdown_preview(&mut kitty)?;
    kitty.send(b"\t\t")?;
    kitty.send(ENTER)?;
    kitty.wait_for_raw(
        "Kitty graphics payload from actual zec binary",
        b"\x1b_Ga=T,f=100,t=d,i=1",
        ACTION_TIMEOUT,
    )?;
    kitty.send(b"\x1b")?;
    kitty.wait_for_raw(
        "Kitty image deletion on preview back",
        b"\x1b_Ga=d,d=A,q=2\x1b\\",
        ACTION_TIMEOUT,
    )?;
    kitty.send(b"\x1b")?;
    kitty.wait_for_screen("Kitty Markdown preview closed", ACTION_TIMEOUT, |screen| {
        screen.contains("Markdown preview closed; editor focused") && screen.contains(UPDATED)
    })?;
    kitty.send(CTRL_Q)?;
    let kitty_status = kitty.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(
        kitty_status.success(),
        "Kitty rich-content exit failed: {kitty_status}"
    );
    kitty.assert_terminal_restored(&kitty_termios)?;

    let session_directory = temp.path().join("rich-content-session-state");
    let session_environment = [
        (OsStr::new("ZEC_DISABLE_SESSIONS"), OsStr::new("0")),
        (OsStr::new("ZEC_SESSION_DIR"), session_directory.as_os_str()),
        (OsStr::new("ZEC_IMAGE_PROTOCOL"), OsStr::new("none")),
    ];
    let first_session_pair = open_pty()?;
    let first_session_termios = capture_baseline(&first_session_pair)?;
    let mut first_session = PtySession::spawn_with_env(
        first_session_pair,
        &[root.as_os_str()],
        &session_environment,
    )?;
    first_session.wait_for_screen("image-session worktree trust", STARTUP_TIMEOUT, |screen| {
        screen.contains("Worktree Trust") && screen.contains("rich-content-project")
    })?;
    first_session.send(ENTER)?;
    first_session.wait_for_screen("image-session source ready", ACTION_TIMEOUT, |screen| {
        screen.contains(UPDATED) && !screen.contains("Worktree Trust")
    })?;
    first_session.send(CTRL_P)?;
    first_session.paste("checker.png")?;
    first_session.wait_for_screen("image-session selection", ACTION_TIMEOUT, |screen| {
        screen.contains("assets/checker.png")
    })?;
    first_session.send(ENTER)?;
    first_session.wait_for_screen("image-session tab ready", ACTION_TIMEOUT, |screen| {
        screen.contains("Image: checker.png") && screen.contains("Ctrl-W close")
    })?;
    first_session.send(CTRL_Q)?;
    let first_session_status = first_session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(
        first_session_status.success(),
        "image-session first exit failed"
    );
    first_session.assert_terminal_restored(&first_session_termios)?;

    let restored_pair = open_pty()?;
    let restored_termios = capture_baseline(&restored_pair)?;
    let mut restored =
        PtySession::spawn_with_env(restored_pair, &[root.as_os_str()], &session_environment)?;
    restored.wait_for_screen("restored image worktree trust", STARTUP_TIMEOUT, |screen| {
        screen.contains("Worktree Trust") && screen.contains("rich-content-project")
    })?;
    restored.send(ENTER)?;
    restored.wait_for_screen("direct image session restored", ACTION_TIMEOUT, |screen| {
        screen.contains("Image: checker.png")
            && screen.contains("checker.png [Image]")
            && screen.contains("Ctrl-W close")
    })?;
    restored.assert_raw_mode_enabled(&restored_termios)?;
    restored.send(F4)?;
    restored.wait_for_screen("restored image input ready", ACTION_TIMEOUT, |screen| {
        screen.contains("terminal keyboard=") && screen.contains("Image: checker.png")
    })?;
    restored.send(CTRL_W)?;
    restored.wait_for_screen("restored image tab closed", ACTION_TIMEOUT, |screen| {
        screen.contains(UPDATED) && screen.contains("tab closed")
    })?;
    restored.send(CTRL_Q)?;
    let restored_status = restored.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(
        restored_status.success(),
        "restored image-session exit failed"
    );
    restored.assert_terminal_restored(&restored_termios)?;

    let startup_pair = open_pty()?;
    let startup_termios = capture_baseline(&startup_pair)?;
    let startup_environment = [(OsStr::new("ZEC_IMAGE_PROTOCOL"), OsStr::new("none"))];
    let image_path = root.join("assets/checker.png");
    let mut startup = PtySession::spawn_with_env(
        startup_pair,
        &[image_path.as_os_str()],
        &startup_environment,
    )?;
    startup.wait_for_screen("startup image worktree trust", STARTUP_TIMEOUT, |screen| {
        screen.contains("Worktree Trust") && screen.contains("rich-content-project")
    })?;
    startup.send(ENTER)?;
    startup.wait_for_screen("direct startup image", ACTION_TIMEOUT, |screen| {
        screen.contains("Image: checker.png")
            && screen.contains("PNG · 2×2")
            && screen.contains("Ctrl-W close")
    })?;
    startup.send(CTRL_Q)?;
    let startup_status = startup.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(startup_status.success(), "startup image exit failed");
    startup.assert_terminal_restored(&startup_termios)
}

#[test]
fn large_file_opens_navigates_edits_and_saves_in_the_actual_binary() -> Result<()> {
    const LINE_COUNT: usize = 100_000;
    const LONG_LINE: usize = 50_000;
    const LONG_LINE_BYTES: usize = 64 * 1024;
    const TOP: &str = "E2E_LARGE_FILE_TOP";
    const LONG: &str = "E2E_LARGE_FILE_LONG_LINE";
    const BOTTOM: &str = "E2E_LARGE_FILE_BOTTOM";
    const EDIT: &str = "E2E_LARGE_FILE_EDITED_";

    let temp = tempfile::tempdir().context("create Beta 2 large-file fixture")?;
    let path = temp.path().join("large-100000-lines.txt");
    let mut contents = String::with_capacity(2 * 1024 * 1024);
    for row in 1..=LINE_COUNT {
        match row {
            1 => contents.push_str(TOP),
            LONG_LINE => {
                contents.push_str(LONG);
                contents.extend(std::iter::repeat_n(
                    'x',
                    LONG_LINE_BYTES.saturating_sub(LONG.len()),
                ));
            }
            LINE_COUNT => contents.push_str(BOTTOM),
            _ => contents.push_str(&format!("line-{row:06}")),
        }
        if row != LINE_COUNT {
            contents.push('\n');
        }
    }
    ensure!(contents.lines().count() == LINE_COUNT);
    ensure!(contents.lines().nth(LONG_LINE - 1).unwrap().len() == LONG_LINE_BYTES);
    fs::write(&path, contents).context("write Beta 2 large-file fixture")?;

    let pair = open_pty()?;
    let termios_before = capture_baseline(&pair)?;
    let startup = Instant::now();
    let mut session = PtySession::spawn(pair, &[path.as_os_str()])?;
    session.wait_for_screen("large-file first frame", STARTUP_TIMEOUT, |screen| {
        screen.contains(TOP)
    })?;
    ensure!(
        startup.elapsed() <= Duration::from_secs(15),
        "large-file first frame exceeded 15 seconds"
    );

    session.send(CTRL_G)?;
    session.wait_for_screen("large-file long-line prompt", ACTION_TIMEOUT, |screen| {
        screen.contains("Go to line:")
    })?;
    session.paste(&LONG_LINE.to_string())?;
    session.send(ENTER)?;
    session.wait_for_screen("64 KiB line rendered", ACTION_TIMEOUT, |screen| {
        screen.contains(LONG)
    })?;

    session.send(CTRL_G)?;
    session.paste(&LINE_COUNT.to_string())?;
    session.send(ENTER)?;
    session.wait_for_screen("large-file last line", ACTION_TIMEOUT, |screen| {
        screen.contains(BOTTOM)
    })?;
    session.paste(EDIT)?;
    session.wait_for_screen("large-file edit applied", ACTION_TIMEOUT, |screen| {
        screen.contains(&format!("{EDIT}{BOTTOM}"))
    })?;
    session.send(CTRL_S)?;
    session.wait_until("large-file edit persisted", ACTION_TIMEOUT, |_| {
        fs::read_to_string(&path).is_ok_and(|text| text.contains(&format!("{EDIT}{BOTTOM}")))
    })?;
    let persisted = fs::read_to_string(&path).context("read saved Beta 2 large file")?;
    ensure!(persisted.lines().count() == LINE_COUNT);
    let expected_last_line = format!("{EDIT}{BOTTOM}");
    ensure!(persisted.lines().last() == Some(expected_last_line.as_str()));

    #[cfg(target_os = "linux")]
    {
        let pid = session
            .child
            .as_ref()
            .and_then(|child| child.process_id())
            .context("large-file zec process has no PID")?;
        let status = fs::read_to_string(format!("/proc/{pid}/status"))
            .context("read large-file zec process status")?;
        let vm_hwm_bytes = status
            .lines()
            .find_map(|line| line.strip_prefix("VmHWM:"))
            .and_then(|value| value.split_whitespace().next())
            .and_then(|value| value.parse::<u64>().ok())
            .map(|kib| kib.saturating_mul(1024))
            .context("large-file zec process has no VmHWM")?;
        ensure!(
            vm_hwm_bytes <= 1_073_741_824,
            "large-file VmHWM {vm_hwm_bytes} exceeds 1 GiB"
        );
    }

    session.send(CTRL_Q)?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(status.success(), "large-file zec exit failed: {status}");
    session.assert_terminal_restored(&termios_before)
}

#[test]
fn agent_acp_permissions_and_mcp_run_through_the_actual_binary() -> Result<()> {
    const READY: &str = "E2E_AGENT_EDITOR_READY";
    const PROMPT: &str = "E2E_AGENT_PROMPT";

    let temp = tempfile::tempdir().context("create Beta 3 ACP fixture")?;
    let root = temp.path().join("agent-project");
    fs::create_dir_all(root.join(".zed")).context("create Agent fixture project")?;
    fs::write(root.join("README.md"), format!("{READY}\n"))
        .context("write Agent fixture document")?;
    fs::write(root.join("AGENTS.md"), "E2E_EXTERNAL_AGENT_INSTRUCTIONS\n")
        .context("write external Agent instructions")?;
    fs::write(
        root.join(".zed/settings.json"),
        r#"{
          "context_servers": {
            "fixture-mcp": {
              "command": "/bin/true",
              "args": [],
              "env": {}
            }
          }
        }"#,
    )
    .context("write Agent MCP project settings")?;

    let terminal_auth_log = temp.path().join("agent-terminal-auth.log");
    let agent_configuration = serde_json::to_string(&serde_json::json!({
        "command": env!("CARGO_BIN_EXE_fixture_acp"),
        "args": [],
        "env": {
            "ZEC_ACP_FIXTURE_AUTH_LOG": terminal_auth_log
        },
        "id": "fixture-agent"
    }))
    .context("encode ACP fixture configuration")?;
    let pair = open_pty()?;
    let termios_before = capture_baseline(&pair)?;
    let environment = [(
        OsStr::new("ZEC_ACP_AGENT"),
        OsStr::new(&agent_configuration),
    )];
    let mut session = PtySession::spawn_with_env(pair, &[root.as_os_str()], &environment)?;
    session.wait_for_screen("Agent worktree trust", STARTUP_TIMEOUT, |screen| {
        screen.contains("Worktree Trust") && screen.contains("agent-project")
    })?;
    session.send(ENTER)?;
    session.wait_for_screen("Agent editor ready", ACTION_TIMEOUT, |screen| {
        screen.contains(READY) && !screen.contains("Worktree Trust")
    })?;

    // Open through the command palette so this also proves Zed's canonical
    // `agent::ToggleFocus` action is terminalized into the zec reducer.
    session.send(F1)?;
    session.wait_for_screen("Agent command palette", ACTION_TIMEOUT, |screen| {
        screen.contains("Command palette:")
    })?;
    session.paste("Toggle Agent Panel")?;
    session.wait_for_screen("Agent palette action", ACTION_TIMEOUT, |screen| {
        screen.contains("Toggle Agent Panel") && screen.contains("agent::ToggleFocus")
    })?;
    session.send(ENTER)?;
    session.wait_for_screen("real Zed ACP session", STARTUP_TIMEOUT, |screen| {
        screen.contains("Agent · fixture-agent · ACP idle")
            && screen.contains("MCP 1")
            && screen.contains("connected through Zed ACP")
    })?;

    session.paste("/commands")?;
    session.send(ENTER)?;
    session.wait_for_screen("ACP slash command inventory", ACTION_TIMEOUT, |screen| {
        screen.contains("/fixture_command")
            && screen.contains("Fixture-advertised ACP slash command")
    })?;
    session.paste("/models")?;
    session.send(ENTER)?;
    session.wait_for_screen("ACP model inventory", ACTION_TIMEOUT, |screen| {
        screen.contains("Fixture Balanced") && screen.contains("fixture-fast")
    })?;
    session.paste("/model fixture-fast")?;
    session.send(ENTER)?;
    session.wait_for_screen("ACP model selection", ACTION_TIMEOUT, |screen| {
        screen.contains("model selected: fixture-fast")
    })?;
    session.paste("/modes")?;
    session.send(ENTER)?;
    session.wait_for_screen("ACP mode inventory", ACTION_TIMEOUT, |screen| {
        screen.contains("Ask (ask)") && screen.contains("Plan (plan)")
    })?;
    session.paste("/mode plan")?;
    session.send(ENTER)?;
    session.wait_for_screen("ACP mode selection", ACTION_TIMEOUT, |screen| {
        screen.contains("mode selected: plan")
    })?;
    session.paste("/config thinking true")?;
    session.send(ENTER)?;
    session.wait_for_screen("ACP config selection", ACTION_TIMEOUT, |screen| {
        screen.contains("config updated: thinking") && screen.contains("Thinking (thinking) = true")
    })?;
    session.paste("/sessions")?;
    session.send(ENTER)?;
    session.wait_for_screen("ACP session history", ACTION_TIMEOUT, |screen| {
        screen.contains("zec-acp-fixture-saved") && screen.contains("Fixture saved session")
    })?;
    session.paste("/auth")?;
    session.send(ENTER)?;
    session.wait_for_screen("ACP auth inventory", ACTION_TIMEOUT, |screen| {
        screen.contains("Fixture login (fixture-login)")
            && screen.contains("Fixture terminal login (fixture-terminal)")
    })?;
    session.paste("/instructions")?;
    session.send(ENTER)?;
    session.wait_for_screen("Agent effective instructions", ACTION_TIMEOUT, |screen| {
        screen.contains("E2E_EXTERNAL_AGENT_INSTRUCTIONS")
            && screen.contains("agent-project/AGENTS.md")
    })?;
    session.paste("/auth fixture-terminal")?;
    session.send(ENTER)?;
    session.wait_until(
        "ACP terminal authentication side effect",
        ACTION_TIMEOUT,
        |_| {
            fs::read_to_string(&terminal_auth_log)
                .is_ok_and(|text| text.contains("E2E_TERMINAL_AUTH_READY"))
        },
    )?;
    session.wait_for_screen(
        "ACP terminal authentication reconnect",
        STARTUP_TIMEOUT,
        |screen| {
            screen.contains("terminal authentication completed: fixture-terminal")
                && screen.contains("Agent reconnected")
                && screen.contains("Agent · fixture-agent · ACP idle")
        },
    )?;
    session.paste("/auth fixture-login")?;
    session.send(ENTER)?;
    session.wait_for_screen("ACP authentication", ACTION_TIMEOUT, |screen| {
        screen.contains("authentication completed: fixture-login")
    })?;
    session.paste("/logout")?;
    session.send(ENTER)?;
    session.wait_for_screen("ACP logout", ACTION_TIMEOUT, |screen| {
        screen.contains("Agent logged out")
    })?;
    session.paste("/load zec-acp-fixture-saved")?;
    session.send(ENTER)?;
    session.wait_for_screen("ACP saved session load", ACTION_TIMEOUT, |screen| {
        screen.contains("saved Agent session loaded") && screen.contains("ACP idle")
    })?;

    session.paste(PROMPT)?;
    session.wait_for_screen("Agent prompt input", ACTION_TIMEOUT, |screen| {
        screen.contains(PROMPT)
    })?;
    session.send(ENTER)?;
    session.wait_for_screen("ACP tool authorization", ACTION_TIMEOUT, |screen| {
        screen.contains("Permission: Fixture approval gate")
            && screen.contains("y allow")
            && screen.contains("n reject")
    })?;
    session.send(b"y")?;
    session.wait_for_screen(
        "ACP streamed assistant response",
        ACTION_TIMEOUT,
        |screen| {
            screen.contains(&format!("fixture approved: {PROMPT}"))
                && screen.contains("MCP servers received from Zed Project: 1")
                && screen.contains("ACP idle")
        },
    )?;
    session.paste("/close zec-acp-fixture-saved")?;
    session.send(ENTER)?;
    session.wait_for_screen(
        "ACP session close and replacement",
        STARTUP_TIMEOUT,
        |screen| {
            screen.contains("new Agent session ready")
                && screen.contains("Agent · fixture-agent · ACP idle")
        },
    )?;

    // The focused panel toggles closed and the same editor buffer remains the
    // active Zed workspace item.
    session.send(F1)?;
    session.paste("Toggle Agent Panel")?;
    session.send(ENTER)?;
    session.wait_for_screen("Agent panel hidden", ACTION_TIMEOUT, |screen| {
        screen.contains(READY) && screen.contains("Agent panel hidden")
    })?;
    session.send(CTRL_Q)?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(status.success(), "Agent ACP zec exit failed: {status}");
    session.assert_terminal_restored(&termios_before)
}

#[test]
fn native_zed_agent_and_local_commands_run_through_the_actual_binary() -> Result<()> {
    const READY: &str = "E2E_NATIVE_AGENT_EDITOR_READY";

    let temp = tempfile::tempdir().context("create native Zed Agent fixture")?;
    let root = temp.path().join("native-agent-project");
    fs::create_dir_all(root.join(".agents/skills/native-fixture"))
        .context("create native Agent fixture project")?;
    fs::write(root.join("README.md"), format!("{READY}\n"))
        .context("write native Agent fixture document")?;
    fs::write(root.join("AGENTS.md"), "E2E_NATIVE_AGENT_INSTRUCTIONS\n")
        .context("write native Agent instructions")?;
    fs::write(
        root.join(".agents/skills/native-fixture/SKILL.md"),
        "---\nname: native-fixture\ndescription: Native fixture skill\n---\n\nUse the native fixture.\n",
    )
    .context("write native Agent skill")?;

    let pair = open_pty()?;
    let termios_before = capture_baseline(&pair)?;
    let mut session = PtySession::spawn(pair, &[root.as_os_str()])?;
    session.wait_for_screen("native Agent worktree trust", STARTUP_TIMEOUT, |screen| {
        screen.contains("Worktree Trust") && screen.contains("native-agent-project")
    })?;
    session.send(ENTER)?;
    session.wait_for_screen("native Agent editor ready", ACTION_TIMEOUT, |screen| {
        screen.contains(READY) && !screen.contains("Worktree Trust")
    })?;

    session.send(F1)?;
    session.paste("Toggle Agent Panel")?;
    session.send(ENTER)?;
    session.wait_for_screen("native Zed Agent session", STARTUP_TIMEOUT, |screen| {
        screen.contains("Agent · Zed Agent · ACP idle")
            && screen.contains("Zed Agent connected through Zed ACP")
    })?;

    session.paste("/help")?;
    session.send(ENTER)?;
    session.wait_for_screen("native Agent local help", ACTION_TIMEOUT, |screen| {
        screen.contains("sent to the Agent") && screen.contains("/instructions")
    })?;
    session.paste("/instructions")?;
    session.send(ENTER)?;
    session.wait_for_screen(
        "native Agent effective instructions",
        ACTION_TIMEOUT,
        |screen| {
            screen.contains("E2E_NATIVE_AGENT_INSTRUCTIONS")
                && screen.contains("native-agent-project/AGENTS.md")
        },
    )?;
    session.paste("/skills")?;
    session.send(ENTER)?;
    session.wait_for_screen("native Agent project skills", STARTUP_TIMEOUT, |screen| {
        screen.contains("native-fixture") && screen.contains("Native fixture skill")
    })?;
    session.paste("/mcp")?;
    session.send(ENTER)?;
    session.wait_for_screen("native Agent MCP inventory", ACTION_TIMEOUT, |screen| {
        screen.contains("MCP: no enabled servers configured")
    })?;
    session.paste("/auth")?;
    session.send(ENTER)?;
    session.wait_for_screen("native Agent auth capability", ACTION_TIMEOUT, |screen| {
        screen.contains("authentication: not required")
    })?;

    session.send(CTRL_N)?;
    session.wait_for_screen("native Agent new session", STARTUP_TIMEOUT, |screen| {
        screen.contains("Agent · Zed Agent · ACP idle")
            && screen.contains("new Agent session ready")
    })?;
    session.send(CTRL_Q)?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(status.success(), "native Agent zec exit failed: {status}");
    session.assert_terminal_restored(&termios_before)
}

#[cfg(unix)]
#[test]
fn collaboration_notes_follow_invites_and_media_run_through_the_actual_binary() -> Result<()> {
    const READY: &str = "E2E_COLLABORATION_EDITOR_READY";
    const ORIGINAL_NOTES: &str = "# Shared Notes\nCOLLAB_ORIGINAL\n";
    const EDITED_NOTES: &str = "# Shared Notes\nCOLLAB_EDITED\n";
    const FINAL_NOTES: &str = "# Shared Notes\nCOLLAB_FINAL\n";
    const CHANNEL_URL: &str = "https://example.invalid/channels/terminal-team";

    let temp = tempfile::tempdir().context("create collaboration PTY fixture")?;
    let root = temp.path().join("collaboration-project");
    fs::create_dir_all(&root).context("create collaboration fixture project")?;
    fs::write(root.join("README.md"), format!("{READY}\n"))
        .context("write collaboration fixture document")?;

    let media_log = temp.path().join("media-bridge.log");
    let media_bridge = temp.path().join("media-bridge.sh");
    fs::write(
        &media_bridge,
        "#!/bin/sh\nprintf '%s|%s|%s|%s\\n' \"$$\" \"$1\" \"$2\" \"$3\" >> \"$ZEC_MEDIA_LOG\"\nexec /bin/sleep 30\n",
    )
    .context("write collaboration media bridge")?;
    let mut permissions = fs::metadata(&media_bridge)
        .context("read collaboration media bridge metadata")?
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&media_bridge, permissions)
        .context("make collaboration media bridge executable")?;

    let collaboration_fixture = serde_json::to_string(&serde_json::json!({
        "account": "terminal-user",
        "channels": [
            {
                "id": 7,
                "name": "terminal-team",
                "unread": true,
                "url": CHANNEL_URL,
                "notes": ORIGINAL_NOTES,
                "collaborators": [{
                    "userId": 42,
                    "username": "alice",
                    "online": true,
                    "host": true,
                    "row": 1,
                    "column": 3
                }]
            },
            {
                "id": 8,
                "name": "invited-accept",
                "invitation": true,
                "url": "https://example.invalid/channels/invited-accept"
            },
            {
                "id": 9,
                "name": "invited-decline",
                "invitation": true,
                "url": "https://example.invalid/channels/invited-decline"
            }
        ]
    }))
    .context("serialize collaboration fixture")?;
    let media_configuration = serde_json::to_string(&serde_json::json!({
        "command": media_bridge.to_string_lossy(),
        "args": ["fixture-prefix"],
        "env": { "ZEC_MEDIA_LOG": media_log.to_string_lossy() }
    }))
    .context("serialize media bridge configuration")?;

    let pair = open_pty()?;
    let termios_before = capture_baseline(&pair)?;
    let environment = [
        (
            OsStr::new("ZEC_COLLABORATION_FIXTURE"),
            OsStr::new(&collaboration_fixture),
        ),
        (
            OsStr::new("ZEC_MEDIA_BRIDGE"),
            OsStr::new(&media_configuration),
        ),
        (OsStr::new("ZEC_EXTERNAL_MEDIA"), OsStr::new("1")),
    ];
    let mut session = PtySession::spawn_with_env(pair, &[root.as_os_str()], &environment)?;
    session.wait_for_screen("collaboration worktree trust", STARTUP_TIMEOUT, |screen| {
        screen.contains("Worktree Trust") && screen.contains("collaboration-project")
    })?;
    session.send(ENTER)?;
    session.wait_for_screen("collaboration editor ready", ACTION_TIMEOUT, |screen| {
        screen.contains(READY) && !screen.contains("Worktree Trust")
    })?;
    session.assert_raw_mode_enabled(&termios_before)?;

    open_collaboration_panel(&mut session)?;
    session.wait_for_screen("fixture collaboration panel", ACTION_TIMEOUT, |screen| {
        screen.contains("Collaboration")
            && screen.contains("Status: fixture-connected")
            && screen.contains("Account: @terminal-user")
            && screen.contains("#terminal-team")
            && screen.contains("#invited-accept [invite]")
            && screen.contains("@alice host")
            && screen.contains("Media bridge: configured")
            && screen.contains("Voice: off  Screen: off")
    })?;

    session.send(b"i")?;
    session.wait_for_screen("fixture collaboration sign out", ACTION_TIMEOUT, |screen| {
        screen.contains("Status: signed out")
            && screen.contains("Account: signed out")
            && screen.contains("fixture signed out")
    })?;
    session.send(b"i")?;
    session.wait_for_screen("fixture collaboration sign in", ACTION_TIMEOUT, |screen| {
        screen.contains("Status: fixture-connected")
            && screen.contains("Account: @terminal-user")
            && screen.contains("fixture signed in")
    })?;
    session.send(b"r")?;
    session.wait_for_screen("fixture collaboration refresh", ACTION_TIMEOUT, |screen| {
        screen.contains("fixture refreshed")
    })?;

    session.send(b"v")?;
    session.wait_for_screen("voice bridge permission", ACTION_TIMEOUT, |screen| {
        screen.contains("Allow voice bridge? y/n")
    })?;
    ensure!(
        !media_log.exists(),
        "media bridge launched before explicit confirmation"
    );
    session.send(b"n")?;
    session.wait_for_screen("voice bridge rejection", ACTION_TIMEOUT, |screen| {
        screen.contains("voice bridge cancelled") && screen.contains("Voice: off  Screen: off")
    })?;

    session.send(b"v")?;
    session.send(b"y")?;
    session.wait_for_screen("voice bridge started", ACTION_TIMEOUT, |screen| {
        screen.contains("voice bridge started") && screen.contains("Voice: on  Screen: off")
    })?;
    session.wait_until("voice bridge invocation", ACTION_TIMEOUT, |_| {
        fs::read_to_string(&media_log).is_ok_and(|text| {
            text.lines()
                .any(|line| line.contains(&format!("|fixture-prefix|voice|{CHANNEL_URL}")))
        })
    })?;
    let voice_pid = bridge_pid(&media_log, "voice")?;
    session.send(b"v")?;
    session.wait_for_screen("voice bridge stopped", ACTION_TIMEOUT, |screen| {
        screen.contains("voice bridge stopped") && screen.contains("Voice: off  Screen: off")
    })?;
    session.wait_until("voice bridge process reaped", ACTION_TIMEOUT, |_| {
        !PathBuf::from(format!("/proc/{voice_pid}")).exists()
    })?;

    session.send(b"s")?;
    session.wait_for_screen("screen bridge permission", ACTION_TIMEOUT, |screen| {
        screen.contains("Allow screen bridge? y/n")
    })?;
    session.send(b"y")?;
    session.wait_for_screen("screen bridge started", ACTION_TIMEOUT, |screen| {
        screen.contains("screen bridge started") && screen.contains("Voice: off  Screen: on")
    })?;
    session.wait_until("screen bridge invocation", ACTION_TIMEOUT, |_| {
        fs::read_to_string(&media_log).is_ok_and(|text| {
            text.lines()
                .any(|line| line.contains(&format!("|fixture-prefix|screen|{CHANNEL_URL}")))
        })
    })?;
    let screen_pid = bridge_pid(&media_log, "screen")?;
    session.send(b"s")?;
    session.wait_for_screen("screen bridge stopped", ACTION_TIMEOUT, |screen| {
        screen.contains("screen bridge stopped") && screen.contains("Voice: off  Screen: off")
    })?;
    session.wait_until("screen bridge process reaped", ACTION_TIMEOUT, |_| {
        !PathBuf::from(format!("/proc/{screen_pid}")).exists()
    })?;

    session.send(ENTER)?;
    session.wait_for_screen("channel notes editor", ACTION_TIMEOUT, |screen| {
        screen.contains("#terminal-team notes")
            && screen.contains("COLLAB_ORIGINAL")
            && screen.contains("edits synchronize automatically")
    })?;
    session.send(F10)?;
    session.wait_for_screen("channel notes split", ACTION_TIMEOUT, |screen| {
        screen.matches("# Shared Notes").count() >= 2 && screen.contains("split right into pane")
    })?;
    session.send(CTRL_A)?;
    session.paste(EDITED_NOTES)?;
    session.wait_for_screen("shared channel notes edit", ACTION_TIMEOUT, |screen| {
        screen.matches("COLLAB_EDITED").count() >= 2
    })?;
    session.send(CTRL_S)?;
    session.wait_for_screen("channel notes auto sync", ACTION_TIMEOUT, |screen| {
        screen.contains("#terminal-team notes synchronize automatically")
    })?;
    session.send(CTRL_Z)?;
    session.wait_for_screen("shared channel notes undo", ACTION_TIMEOUT, |screen| {
        screen.matches("COLLAB_ORIGINAL").count() >= 2 && !screen.contains("COLLAB_EDITED")
    })?;
    session.send(CTRL_A)?;
    session.paste(FINAL_NOTES)?;
    session.wait_for_screen(
        "shared channel notes final edit",
        ACTION_TIMEOUT,
        |screen| screen.matches("COLLAB_FINAL").count() >= 2,
    )?;

    open_collaboration_panel(&mut session)?;
    session.wait_for_screen("active channel notes marker", ACTION_TIMEOUT, |screen| {
        screen.contains("#terminal-team") && screen.contains('\u{270e}')
    })?;
    session.send(b"\t")?;
    session.send(b"f")?;
    session.wait_for_screen("follow fixture collaborator", ACTION_TIMEOUT, |screen| {
        screen.contains("COLLAB_FINAL")
            && screen.contains("following collaborator 42 in channel notes")
    })?;

    open_collaboration_panel(&mut session)?;
    session.send(b"\t")?;
    session.send(b"c")?;
    session.wait_for_screen(
        "create collaboration channel prompt",
        ACTION_TIMEOUT,
        |screen| screen.contains("Create channel:") && screen.contains("Enter crea"),
    )?;
    session.paste("console-created")?;
    session.send(ENTER)?;
    session.wait_for_screen("fixture channel created", ACTION_TIMEOUT, |screen| {
        screen.contains("created fixture channel #con") && screen.contains("#console-created")
    })?;
    session.send(b"\x1b[A")?;
    session.send(b"d")?;
    session.wait_for_screen("fixture invitation declined", ACTION_TIMEOUT, |screen| {
        screen.contains("invitation declined") && !screen.contains("#invited-decline")
    })?;
    session.send(b"\x1b[A")?;
    session.send(b"a")?;
    session.wait_for_screen("fixture invitation accepted", ACTION_TIMEOUT, |screen| {
        screen.contains("invitation accepted")
            && screen.contains("#invited-accept")
            && !screen.contains("#invited-accept [invite]")
    })?;
    session.send(b"\x1b")?;
    session.wait_for_screen("collaboration returns to notes", ACTION_TIMEOUT, |screen| {
        screen.contains("COLLAB_FINAL") && screen.contains("editor focused")
    })?;

    // Channel notes are synchronized state, so even the deterministic local
    // fixture must not trigger a scratch-buffer discard confirmation.
    session.send(CTRL_Q)?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(status.success(), "collaboration zec exit failed: {status}");
    session.assert_terminal_restored(&termios_before)
}

#[cfg(unix)]
#[test]
fn notebook_cells_outputs_kernel_controls_and_cleanup_run_through_the_actual_binary() -> Result<()>
{
    const MARKDOWN: &str = "NOTEBOOK_MARKDOWN_READY";
    const PRELOADED: &str = "NOTEBOOK_PRELOADED_STREAM";
    const EDITED: &str = "NOTEBOOK_EDITED_FROM_TERMINAL";

    let temp = tempfile::tempdir().context("create Notebook PTY fixture")?;
    let root = temp.path().join("notebook-project");
    let bin = temp.path().join("bin");
    fs::create_dir_all(&root).context("create Notebook fixture project")?;
    fs::create_dir_all(&bin).context("create Notebook fixture bin")?;
    let notebook_path = root.join("console.ipynb");
    let notebook_json = serde_json::to_string_pretty(&serde_json::json!({
        "cells": [
            {
                "cell_type": "markdown",
                "id": "intro",
                "metadata": {},
                "source": ["# Console Notebook\n", format!("{MARKDOWN}\n")]
            },
            {
                "cell_type": "code",
                "execution_count": 7,
                "id": "fixture-code",
                "metadata": {},
                "outputs": [
                    {
                        "name": "stdout",
                        "output_type": "stream",
                        "text": [format!("{PRELOADED}\n")]
                    },
                    {
                        "data": {"image/png": "aGVsbG8="},
                        "metadata": {},
                        "output_type": "display_data"
                    }
                ],
                "source": ["print('fixture')\n"]
            }
        ],
        "metadata": {
            "kernelspec": {
                "display_name": "Python 3",
                "language": "python",
                "name": "python3"
            },
            "language_info": {"name": "python"}
        },
        "nbformat": 4,
        "nbformat_minor": 5
    }))
    .context("serialize Notebook fixture")?;
    fs::write(&notebook_path, notebook_json).context("write Notebook fixture")?;

    let kernel_log = temp.path().join("notebook-kernel.log");
    let python = bin.join("python3");
    fs::write(
        &python,
        r#"#!/bin/sh
case " $* " in
  *" ipykernel_launcher "*) ;;
  *) exec /usr/bin/python3 "$@" ;;
esac
count=0
if [ -f "$ZEC_NOTEBOOK_KERNEL_LOG" ]; then
  count=$(/usr/bin/wc -l < "$ZEC_NOTEBOOK_KERNEL_LOG")
fi
printf '%s|%s\n' "$$" "$*" >> "$ZEC_NOTEBOOK_KERNEL_LOG"
if [ "$count" -eq 1 ]; then
  exit 41
fi
/bin/sleep 60
"#,
    )
    .context("write deterministic Notebook kernel")?;
    let mut permissions = fs::metadata(&python)
        .context("read Notebook kernel metadata")?
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&python, permissions).context("make Notebook kernel executable")?;

    let fixture_path = std::env::join_paths(std::iter::once(bin.clone()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .context("construct Notebook fixture PATH")?;
    let pair = open_pty()?;
    let termios_before = capture_baseline(&pair)?;
    let environment = [
        (OsStr::new("PATH"), fixture_path.as_os_str()),
        (
            OsStr::new("ZEC_NOTEBOOK_KERNEL_LOG"),
            kernel_log.as_os_str(),
        ),
    ];
    let mut session = PtySession::spawn_with_env(
        pair,
        &[root.as_os_str(), notebook_path.as_os_str()],
        &environment,
    )?;
    session.wait_for_screen("Notebook worktree trust", STARTUP_TIMEOUT, |screen| {
        screen.contains("Worktree Trust") && screen.contains("notebook-project")
    })?;
    ensure!(
        !kernel_log.exists(),
        "Notebook kernel started before worktree trust confirmation"
    );
    session.send(ENTER)?;
    session.wait_for_screen(
        "native Zed Notebook projection",
        STARTUP_TIMEOUT,
        |screen| {
            screen.contains("Notebook · Python 3 · 2 cells · command mode")
                && screen.contains(MARKDOWN)
                && screen.contains(PRELOADED)
                && screen.contains("image/png output")
                && screen.contains("terminal metadata fallback")
                && !screen.contains("Worktree Trust")
        },
    )?;
    session.assert_raw_mode_enabled(&termios_before)?;
    session.wait_until("first native Notebook kernel", ACTION_TIMEOUT, |_| {
        notebook_kernel_pids(&kernel_log).is_ok_and(|pids| pids.len() == 1)
    })?;
    let first_pid = notebook_kernel_pids(&kernel_log)?[0];
    ensure!(
        process_is_live(first_pid),
        "first Notebook kernel did not remain alive"
    );
    ensure!(
        process_group_id(first_pid) == Some(first_pid),
        "Zed Notebook kernel {first_pid} was not its process-group leader: {:?}",
        process_group_id(first_pid)
    );
    ensure!(
        fs::read_to_string(&kernel_log)?.contains("-m ipykernel_launcher -f"),
        "Zed did not launch the native Jupyter kernelspec"
    );

    session.send(b"b")?;
    session.wait_for_screen("add native Notebook code cell", ACTION_TIMEOUT, |screen| {
        screen.contains("Notebook · Python 3 · 3 cells · edit mode")
            && screen.contains("added a code cell below; edit mode")
    })?;
    session.paste(EDITED)?;
    session.send(b"\x1b")?;
    session.wait_for_screen("edit native Notebook cell", ACTION_TIMEOUT, |screen| {
        screen.contains(EDITED) && screen.contains("command mode")
    })?;
    session.send(CTRL_S)?;
    session.wait_for_screen("save native Notebook cell", ACTION_TIMEOUT, |screen| {
        screen.contains("saved") && screen.contains(EDITED)
    })?;
    session.wait_until("Notebook JSON persistence", ACTION_TIMEOUT, |_| {
        fs::read_to_string(&notebook_path).is_ok_and(|text| {
            text.contains(EDITED) && serde_json::from_str::<serde_json::Value>(&text).is_ok()
        })
    })?;

    session.send(F10)?;
    session.wait_for_screen("shared Notebook split", ACTION_TIMEOUT, |screen| {
        screen.matches("Notebook · Python 3 · 3 cells").count() >= 2
            && screen.matches(EDITED).count() >= 2
    })?;
    session.send(CTRL_W)?;
    session.wait_for_screen("close one Notebook split", ACTION_TIMEOUT, |screen| {
        screen.matches("Notebook · Python 3 · 3 cells").count() == 1
            && screen.contains("tab closed")
    })?;
    ensure!(
        process_is_live(first_pid),
        "closing one shared Notebook split killed its live kernel"
    );

    session.send(b"i")?;
    session.wait_for_screen(
        "interrupt native Notebook kernel",
        ACTION_TIMEOUT,
        |screen| screen.contains("interrupt requested from the Zed Jupyter session"),
    )?;
    session.send(b"r")?;
    session.wait_for_screen("restart native Notebook kernel", ACTION_TIMEOUT, |screen| {
        screen.contains("kernel restart requested")
            && screen.contains("recovered 1 stale starting process")
    })?;
    session.wait_until("previous Notebook kernel cleanup", ACTION_TIMEOUT, |_| {
        !process_is_live(first_pid)
    })?;
    session.wait_until("second native Notebook kernel", ACTION_TIMEOUT, |_| {
        notebook_kernel_pids(&kernel_log).is_ok_and(|pids| pids.len() >= 2)
    })?;
    let second_pid = notebook_kernel_pids(&kernel_log)?[1];
    session.wait_until("failing Notebook kernel exited", ACTION_TIMEOUT, |_| {
        !process_is_live(second_pid)
    })?;
    session.send(b"\x1b[13;5u")?;
    session.wait_for_screen("Notebook kernel failure output", ACTION_TIMEOUT, |screen| {
        screen.contains("Kernel Error: cell could not be executed")
            && screen.contains("the kernel is still starting")
    })?;
    session.send(b"c")?;
    session.wait_for_screen("clear native Notebook outputs", ACTION_TIMEOUT, |screen| {
        screen.contains("cleared all notebook outputs")
            && !screen.contains("Kernel Error")
            && !screen.contains(PRELOADED)
    })?;

    session.send(b"r")?;
    session.wait_until("third native Notebook kernel", ACTION_TIMEOUT, |_| {
        notebook_kernel_pids(&kernel_log).is_ok_and(|pids| pids.len() >= 3)
    })?;
    let third_pid = notebook_kernel_pids(&kernel_log)?[2];
    ensure!(
        PathBuf::from(format!("/proc/{third_pid}")).exists(),
        "third Notebook kernel did not remain alive"
    );
    session.send(CTRL_S)?;
    session.wait_for_screen("save cleared Notebook outputs", ACTION_TIMEOUT, |screen| {
        screen.contains("saved") && screen.contains(EDITED)
    })?;
    let saved: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&notebook_path).context("read saved Notebook fixture")?,
    )
    .context("saved Notebook is invalid JSON")?;
    let cells = saved["cells"]
        .as_array()
        .context("saved Notebook omitted cells")?;
    ensure!(
        cells.iter().any(|cell| {
            let source = &cell["source"];
            source
                .as_str()
                .is_some_and(|source| source.contains(EDITED))
                || source.as_array().is_some_and(|lines| {
                    lines
                        .iter()
                        .any(|line| line.as_str().is_some_and(|line| line.contains(EDITED)))
                })
        }),
        "saved Notebook omitted the terminal cell edit"
    );
    ensure!(
        cells
            .iter()
            .filter(|cell| cell["cell_type"] == "code")
            .all(|cell| {
                cell["outputs"]
                    .as_array()
                    .is_some_and(|outputs| outputs.is_empty())
            }),
        "cleared Notebook outputs were not persisted"
    );

    session.send(CTRL_W)?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(status.success(), "Notebook zec exit failed: {status}");
    let kernel_pids = [first_pid, second_pid, third_pid];
    let deadline = Instant::now() + ACTION_TIMEOUT;
    while kernel_pids
        .iter()
        .any(|pid| PathBuf::from(format!("/proc/{pid}")).exists())
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(10));
    }
    ensure!(
        kernel_pids
            .iter()
            .all(|pid| !PathBuf::from(format!("/proc/{pid}")).exists()),
        "closing the final Notebook tab leaked a kernel process: {kernel_pids:?}"
    );
    session.assert_terminal_restored(&termios_before)
}

#[cfg(unix)]
fn process_group_id(pid: u32) -> Option<u32> {
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()?
        .rsplit_once(") ")?
        .1
        .split_whitespace()
        .nth(2)?
        .parse()
        .ok()
}

#[cfg(unix)]
fn process_is_live(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| stat.rsplit_once(") ").map(|(_, fields)| fields.to_owned()))
        .and_then(|fields| fields.chars().next())
        .is_some_and(|state| state != char::from(90_u8))
}

#[cfg(unix)]
fn notebook_kernel_pids(path: &Path) -> Result<Vec<u32>> {
    fs::read_to_string(path)
        .with_context(|| format!("read Notebook kernel log {}", path.display()))?
        .lines()
        .map(|line| {
            line.split_once('|')
                .context("Notebook kernel log line omitted separator")?
                .0
                .parse()
                .context("parse Notebook kernel pid")
        })
        .collect()
}

#[cfg(unix)]
fn open_collaboration_panel(session: &mut PtySession) -> Result<()> {
    session.send(F1)?;
    session.wait_for_screen("collaboration command palette", ACTION_TIMEOUT, |screen| {
        screen.contains("Command palette:")
    })?;
    session.paste("Toggle Collaboration Panel")?;
    session.send(ENTER)
}

#[cfg(unix)]
fn bridge_pid(path: &Path, kind: &str) -> Result<u32> {
    let log = fs::read_to_string(path)
        .with_context(|| format!("read {} bridge log {}", kind, path.display()))?;
    let line = log
        .lines()
        .find(|line| line.contains(&format!("|fixture-prefix|{kind}|")))
        .with_context(|| format!("{kind} invocation missing from bridge log"))?;
    line.split('|')
        .next()
        .context("bridge log omitted pid")?
        .parse()
        .with_context(|| format!("parse {kind} bridge pid"))
}

fn open_markdown_preview(session: &mut PtySession) -> Result<()> {
    session.send(F1)?;
    session.wait_for_screen("Markdown command palette", ACTION_TIMEOUT, |screen| {
        screen.contains("Command palette:")
    })?;
    session.paste("Toggle Markdown Preview")?;
    session.send(ENTER)
}

#[cfg(target_os = "linux")]
#[test]
fn debugger_and_repl_run_through_zed_dap_in_the_actual_binary() -> Result<()> {
    let require_gdb_dap = std::env::var_os("ZEC_REQUIRE_GDB_DAP").is_some();
    let gdb = Command::new("gdb").arg("--version").output();
    if !gdb.is_ok_and(|output| output.status.success()) {
        ensure!(
            !require_gdb_dap,
            "ZEC_REQUIRE_GDB_DAP is set but GDB is unavailable"
        );
        eprintln!("skipping Beta 1 DAP acceptance: GDB is unavailable");
        return Ok(());
    }

    let temp = tempfile::tempdir().context("create Beta 1 DAP fixture")?;
    let root = temp.path().join("beta-1-debugger");
    fs::create_dir_all(root.join(".zed")).context("create Beta 1 fixture")?;
    let source = root.join("main.c");
    let binary = root.join("debuggee");
    fs::write(
        &source,
        r#"#include <unistd.h>

int main(void) {
    volatile int answer = 40;
    answer += 2;
    sleep(60);
    return answer == 42 ? 0 : 1;
}
"#,
    )
    .context("write Beta 1 C source")?;
    let compile = Command::new("cc")
        .args([
            "-g",
            "-O0",
            "-fno-omit-frame-pointer",
            "main.c",
            "-o",
            "debuggee",
        ])
        .current_dir(&root)
        .status()
        .context("compile Beta 1 debuggee")?;
    ensure!(compile.success(), "Beta 1 debuggee compilation failed");
    let gdb_preflight = Command::new("gdb")
        .args([
            "-q",
            "-nx",
            "-batch",
            "-ex",
            "set debuginfod enabled off",
            "-ex",
            "starti",
        ])
        .arg(&binary)
        .output()
        .context("probe GDB inferior tracing capability")?;
    let gdb_preflight_output = format!(
        "{}{}",
        String::from_utf8_lossy(&gdb_preflight.stdout),
        String::from_utf8_lossy(&gdb_preflight.stderr)
    );
    if !gdb_preflight.status.success()
        && (gdb_preflight_output.contains("Operation not permitted")
            || gdb_preflight_output.contains("Could not trace the inferior"))
    {
        ensure!(
            !require_gdb_dap,
            "ZEC_REQUIRE_GDB_DAP is set but this environment denies ptrace: {}",
            gdb_preflight_output.trim()
        );
        eprintln!(
            "skipping Beta 1 DAP acceptance: this environment denies ptrace ({})",
            gdb_preflight_output.trim()
        );
        return Ok(());
    }
    ensure!(
        gdb_preflight.status.success(),
        "GDB inferior tracing probe failed: {}",
        gdb_preflight_output.trim()
    );
    fs::write(
        root.join(".zed/debug.json"),
        serde_json::to_vec_pretty(&serde_json::json!([
          {
            "label": "Beta 1 GDB",
            "adapter": "GDB",
            "request": "launch",
            "program": binary,
            "cwd": root
          }
        ]))?,
    )
    .context("write Beta 1 debug configuration")?;
    git(&root, &["init"])?;
    git(&root, &["config", "user.email", "zec@example.invalid"])?;
    git(&root, &["config", "user.name", "zec acceptance"])?;
    git(&root, &["add", "main.c", ".zed/debug.json"])?;
    git(&root, &["commit", "-m", "debug fixture"])?;
    ensure!(binary.is_file(), "Beta 1 debuggee binary is missing");

    let pair = open_pty()?;
    let termios_before = capture_baseline(&pair)?;
    let mut session = PtySession::spawn(pair, &[root.as_os_str(), source.as_os_str()])?;
    session.wait_for_screen("Beta 1 worktree trust", STARTUP_TIMEOUT, |screen| {
        screen.contains("Worktree Trust") && screen.contains("beta-1-debugger")
    })?;
    session.send(ENTER)?;
    session.send(CTRL_PAGE_DOWN)?;
    session.wait_for_screen("Beta 1 source ready", ACTION_TIMEOUT, |screen| {
        screen.contains("volatile int answer")
            && screen.contains("[main.c]")
            && !screen.contains("Worktree Trust")
    })?;
    session.send(b"\x1b[B\x1b[B\x1b[B\x1b[B\x1b[B")?;

    session.send(F1)?;
    session.wait_for_screen("Beta 1 breakpoint command", ACTION_TIMEOUT, |screen| {
        screen.contains("Command palette:")
    })?;
    session.paste("Toggle Breakpoint")?;
    session.send(ENTER)?;
    session.wait_for_screen("Beta 1 breakpoint toggled", ACTION_TIMEOUT, |screen| {
        screen.contains("breakpoint toggled")
    })?;

    session.send(F1)?;
    session.paste("Toggle Debugger Panel")?;
    session.send(ENTER)?;
    session.wait_for_screen(
        "Beta 1 idle debugger projection",
        ACTION_TIMEOUT,
        |screen| {
            screen.contains("Debugger · No session · DAP · idle") && screen.contains("main.c:6")
        },
    )?;
    session.send(b"\x1b")?;
    session.wait_for_screen("Beta 1 editor refocused", ACTION_TIMEOUT, |screen| {
        screen.contains("editor focused")
    })?;

    session.send(F5)?;
    session.wait_for_screen("Beta 1 configuration picker", ACTION_TIMEOUT, |screen| {
        screen.contains("Debug Configurations")
            && screen.contains("Beta 1 GDB")
            && screen.contains("GDB")
    })?;
    session.send(ENTER)?;
    session.wait_for_screen("Beta 1 breakpoint stop", STARTUP_TIMEOUT, |screen| {
        screen.contains("Debugger · Beta 1 GDB · GDB · stopped")
    })?;

    session.send(b":")?;
    session.wait_for_screen("Beta 1 Debug REPL", ACTION_TIMEOUT, |screen| {
        screen.contains("Debug console:")
    })?;
    session.paste("print 1+1")?;
    session.send(ENTER)?;
    session.wait_for_screen("Beta 1 Debug REPL result", ACTION_TIMEOUT, |screen| {
        screen.contains("> print 1+1") && screen.contains("< $1 = 2")
    })?;
    session.send(b"\x1b")?;
    session.wait_for_screen("Beta 1 Debug REPL closed", ACTION_TIMEOUT, |screen| {
        !screen.contains("Debug console:")
    })?;
    session.send(b"c")?;
    session.wait_for_screen("Beta 1 continue", ACTION_TIMEOUT, |screen| {
        screen.contains("Debugger · Beta 1 GDB · GDB · running")
    })?;
    session.send(b"p")?;
    session.wait_for_screen("Beta 1 pause", ACTION_TIMEOUT, |screen| {
        screen.contains("Debugger · Beta 1 GDB · GDB · stopped")
    })?;
    session.send(b"n")?;
    session.wait_for_screen("Beta 1 step over", ACTION_TIMEOUT, |screen| {
        screen.contains("step over requested")
            || screen.contains("Debugger · Beta 1 GDB · GDB · stopped")
    })?;
    session.send(b"k")?;
    session.wait_for_screen("Beta 1 debugger shutdown", ACTION_TIMEOUT, |screen| {
        screen.contains("Debugger · No session · DAP · idle") || screen.contains("stop requested")
    })?;

    session.send(CTRL_Q)?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(status.success(), "Beta 1 zec exit failed: {status}");
    session.assert_terminal_restored(&termios_before)
}

#[test]
fn extensions_themes_settings_and_keymap_run_through_the_actual_binary() -> Result<()> {
    const READY: &str = "E2E_CONFIGURATION_READY";

    let temp = tempfile::tempdir().context("create Beta 2 configuration fixture")?;
    let root = temp.path().join("beta-2-configuration");
    let xdg_config = temp.path().join("xdg-config");
    let xdg_data = temp.path().join("xdg-data");
    let zed_config = xdg_config.join("zed");
    let update_manifest = temp.path().join("zec-update-v1.json");
    let extension = xdg_data
        .join("zed/extensions/installed")
        .join("beta-2-console-theme");
    let dev_extension = temp.path().join("beta-2-dev-extension");
    fs::create_dir_all(&root).context("create Beta 2 root")?;
    fs::create_dir_all(&zed_config).context("create Beta 2 Zed config")?;
    fs::create_dir_all(extension.join("themes")).context("create Beta 2 installed extension")?;
    fs::create_dir_all(dev_extension.join("themes"))
        .context("create Beta 2 dev extension source")?;
    let source = root.join("README.md");
    fs::write(&source, format!("{READY}\n")).context("write Beta 2 source")?;
    fs::write(
        zed_config.join("settings.json"),
        r#"{
          "show_whitespaces": "none",
          "auto_install_extensions": { "html": false },
          "auto_update_extensions": { "html": false }
        }"#,
    )
    .context("write Beta 2 settings")?;
    fs::write(
        zed_config.join("global_settings.json"),
        r#"{"show_whitespaces":"none"}"#,
    )
    .context("write Beta 2 global settings")?;
    fs::write(zed_config.join("keymap.json"), "[]").context("write Beta 2 keymap")?;
    fs::write(
        &update_manifest,
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema_version": 1,
            "version": env!("CARGO_PKG_VERSION"),
            "release_url": format!(
                "https://github.com/neguse/zec/releases/tag/v{}",
                env!("CARGO_PKG_VERSION")
            ),
            "assets": [{
                "os": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
                "url": "unused-zec",
                "sha256": "0".repeat(64),
                "size": 1,
                "executable": "zec"
            }]
        }))?,
    )
    .context("write Beta 2 update manifest")?;
    fs::write(
        extension.join("extension.toml"),
        r#"id = "beta-2-console-theme"
name = "Beta 2 Console Theme"
description = "A local extension fixture indexed by the real Zed extension host."
version = "1.2.3"
schema_version = 1
authors = ["zec acceptance"]
themes = ["themes/beta-2-console-theme.json"]
"#,
    )
    .context("write Beta 2 extension manifest")?;
    fs::write(
        extension.join("themes/beta-2-console-theme.json"),
        r##"{
          "$schema": "https://zed.dev/schema/themes/v0.2.0.json",
          "name": "Beta 2 Console Theme Family",
          "author": "zec acceptance",
          "themes": [{
            "name": "Beta 2 Console Dark",
            "appearance": "dark",
            "style": {
              "background": "#101820ff",
              "editor.background": "#101820ff",
              "editor.foreground": "#f2aa4cff"
            }
          }]
        }"##,
    )
    .context("write Beta 2 extension theme")?;
    fs::write(
        dev_extension.join("extension.toml"),
        r#"id = "beta-2-dev-theme"
name = "Beta 2 Dev Theme"
description = "A dev extension installed from the console picker."
version = "0.4.2"
schema_version = 1
authors = ["zec acceptance"]
themes = ["themes/beta-2-dev-theme.json"]
"#,
    )
    .context("write Beta 2 dev extension manifest")?;
    fs::write(
        dev_extension.join("themes/beta-2-dev-theme.json"),
        r##"{
          "$schema": "https://zed.dev/schema/themes/v0.2.0.json",
          "name": "Beta 2 Dev Theme Family",
          "author": "zec acceptance",
          "themes": [{
            "name": "Beta 2 Dev Dark",
            "appearance": "dark",
            "style": {
              "background": "#182010ff",
              "editor.background": "#182010ff",
              "editor.foreground": "#aaf24cff"
            }
          }]
        }"##,
    )
    .context("write Beta 2 dev extension theme")?;

    let pair = open_pty()?;
    let termios_before = capture_baseline(&pair)?;
    let environment = [
        (OsStr::new("XDG_CONFIG_HOME"), xdg_config.as_os_str()),
        (OsStr::new("XDG_DATA_HOME"), xdg_data.as_os_str()),
        (
            OsStr::new("ZEC_UPDATE_MANIFEST"),
            update_manifest.as_os_str(),
        ),
    ];
    let mut session = PtySession::spawn_with_env(pair, &[source.as_os_str()], &environment)?;
    session.wait_for_screen("Beta 2 editor ready", STARTUP_TIMEOUT, |screen| {
        screen.contains(READY)
    })?;

    session.send(F1)?;
    session.paste("Extensions")?;
    session.send(ENTER)?;
    session.wait_for_screen("Zed extension store picker", ACTION_TIMEOUT, |screen| {
        screen.contains("Extensions · all") && screen.contains("Extensions [all]:")
    })?;
    session.send(b"\t")?;
    session.wait_for_screen("installed extension scope", ACTION_TIMEOUT, |screen| {
        screen.contains("Extensions · installed")
            && screen.contains("Extensions [installed]:")
            && screen.contains("Beta 2 Console Theme")
            && screen.contains("1.2.3")
    })?;
    session.send(CTRL_D)?;
    session.wait_for_screen("dev extension path prompt", ACTION_TIMEOUT, |screen| {
        screen.contains("Install Dev Extension") && screen.contains("Dev extension path:")
    })?;
    session.paste(&dev_extension.display().to_string())?;
    session.send(ENTER)?;
    session.wait_for_screen("dev extension installed", STARTUP_TIMEOUT, |screen| {
        screen.contains("Beta 2 Dev Theme")
            && screen.contains("dev 0.4.2")
            && (screen.contains("dev extension installed")
                || screen.contains("extension beta-2-dev-theme installed"))
    })?;
    session.send(b"\x1b")?;
    session.wait_for_screen("extension picker closed", ACTION_TIMEOUT, |screen| {
        !screen.contains("Extensions [installed]:")
    })?;

    session.send(F1)?;
    session.paste("Select Theme")?;
    session.send(ENTER)?;
    session.wait_for_screen("bundled theme picker", ACTION_TIMEOUT, |screen| {
        screen.contains("Themes:") && screen.contains("Themes")
    })?;
    session.paste("Beta 2 Dev Dark")?;
    session.wait_for_screen("filtered extension theme", ACTION_TIMEOUT, |screen| {
        screen.contains("Beta 2 Dev Dark")
    })?;
    session.send(ENTER)?;
    session.wait_for_screen("theme applied", ACTION_TIMEOUT, |screen| {
        screen.contains("theme applied: Beta 2 Dev Dark")
            || screen.contains("user settings reloaded")
    })?;
    session.wait_until("theme persisted to settings", ACTION_TIMEOUT, |_| {
        fs::read_to_string(zed_config.join("settings.json"))
            .is_ok_and(|settings| settings.contains("Beta 2 Dev Dark"))
    })?;

    session.send(F1)?;
    session.paste("Open Settings File")?;
    session.send(ENTER)?;
    session.wait_for_screen("settings file opened", ACTION_TIMEOUT, |screen| {
        screen.contains("settings.json") && screen.contains("Beta 2 Dev Dark")
    })?;

    session.send(F1)?;
    session.paste("Open Keymap File")?;
    session.send(ENTER)?;
    session.wait_for_screen("keymap file opened", ACTION_TIMEOUT, |screen| {
        screen.contains("keymap.json")
    })?;

    fs::write(
        dev_extension.join("extension.toml"),
        r#"id = "beta-2-dev-theme"
name = "Beta 2 Dev Theme"
description = "A dev extension rebuilt from the console picker."
version = "0.4.3"
schema_version = 1
authors = ["zec acceptance"]
themes = ["themes/beta-2-dev-theme.json"]
"#,
    )
    .context("update Beta 2 dev extension manifest")?;
    session.send(F1)?;
    session.paste("Extensions")?;
    session.send(ENTER)?;
    session.wait_for_screen("extension picker reopened", ACTION_TIMEOUT, |screen| {
        screen.contains("Extensions · all") && screen.contains("Beta 2 Dev Theme")
    })?;
    session.paste("Beta 2 Dev Theme")?;
    session.wait_for_screen(
        "dev extension selected for rebuild",
        ACTION_TIMEOUT,
        |screen| screen.contains("Beta 2 Dev Theme") && screen.contains("dev 0.4.2"),
    )?;
    session.send(ENTER)?;
    session.wait_for_screen("dev extension rebuilt", STARTUP_TIMEOUT, |screen| {
        screen.contains("Beta 2 Dev Theme") && screen.contains("dev 0.4.3")
    })?;
    session.send(DELETE)?;
    session.wait_for_screen(
        "dev extension uninstall confirmation",
        ACTION_TIMEOUT,
        |screen| screen.contains("press Delete again"),
    )?;
    session.send(DELETE)?;
    let installed_dev_extension = xdg_data
        .join("zed/extensions/installed")
        .join("beta-2-dev-theme");
    session.wait_until("dev extension symlink removed", ACTION_TIMEOUT, |_| {
        !installed_dev_extension.exists()
    })?;
    session.send(b"\t")?;
    session.wait_for_screen(
        "dev extension removed from installed scope",
        ACTION_TIMEOUT,
        |screen| screen.contains("Extensions · installed") && screen.contains(" 0/0 "),
    )?;
    session.send(b"\x1b")?;
    session.wait_for_screen(
        "extension picker closed after dev uninstall",
        ACTION_TIMEOUT,
        |screen| !screen.contains("Extensions [installed]:"),
    )?;

    session.send(F1)?;
    session.paste("Check for Updates")?;
    session.send(ENTER)?;
    session.wait_for_screen("interactive update check", ACTION_TIMEOUT, |screen| {
        screen.contains(&format!("zec {} is current", env!("CARGO_PKG_VERSION")))
    })?;

    session.send(CTRL_Q)?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(status.success(), "Beta 2 zec exit failed: {status}");
    session.assert_terminal_restored(&termios_before)
}

fn git(root: &Path, arguments: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(root)
        .output()
        .with_context(|| format!("run git {}", arguments.join(" ")))?;
    ensure!(
        output.status.success(),
        "git {} failed: {}",
        arguments.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[cfg(unix)]
struct SshdFixture {
    child: std::process::Child,
    port: u16,
    user: String,
    client_key: PathBuf,
}

#[cfg(unix)]
impl SshdFixture {
    fn start(directory: &Path, sshd: &Path) -> Result<Self> {
        let user = std::env::var("USER").context("remote acceptance requires USER")?;
        let host_key = directory.join("sshd-host-key");
        let client_key = directory.join("sshd-client-key");
        generate_ssh_key(&host_key)?;
        generate_ssh_key(&client_key)?;
        let authorized_keys = directory.join("authorized_keys");
        fs::copy(client_key.with_extension("pub"), &authorized_keys)
            .context("install remote acceptance public key")?;
        fs::set_permissions(&authorized_keys, fs::Permissions::from_mode(0o600))
            .context("protect remote acceptance authorized_keys")?;

        let listener =
            TcpListener::bind(("127.0.0.1", 0)).context("reserve remote acceptance SSH port")?;
        let port = listener.local_addr()?.port();
        drop(listener);
        let config = directory.join("sshd_config");
        fs::write(
            &config,
            format!(
                "Port {port}\n\
                 ListenAddress 127.0.0.1\n\
                 HostKey {}\n\
                 PidFile {}\n\
                 AuthorizedKeysFile {}\n\
                 PasswordAuthentication no\n\
                 KbdInteractiveAuthentication no\n\
                 ChallengeResponseAuthentication no\n\
                 PubkeyAuthentication yes\n\
                 PermitEmptyPasswords no\n\
                 UsePAM no\n\
                 StrictModes no\n\
                 AllowUsers {user}\n\
                 PrintMotd no\n\
                 LogLevel QUIET\n\
                 Subsystem sftp internal-sftp\n",
                host_key.display(),
                directory.join("sshd.pid").display(),
                authorized_keys.display(),
            ),
        )
        .context("write isolated sshd config")?;
        let mut child = Command::new(sshd)
            .args(["-D", "-e", "-f"])
            .arg(&config)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("start isolated sshd")?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            if let Some(status) = child.try_wait().context("inspect isolated sshd")? {
                let mut stderr = String::new();
                if let Some(mut pipe) = child.stderr.take() {
                    pipe.read_to_string(&mut stderr)
                        .context("read isolated sshd failure")?;
                }
                bail!(
                    "isolated sshd exited before listening: {status}: {}",
                    stderr.trim()
                );
            }
            ensure!(
                Instant::now() < deadline,
                "isolated sshd did not begin listening"
            );
            thread::sleep(Duration::from_millis(20));
        }
        Ok(Self {
            child,
            port,
            user,
            client_key,
        })
    }
}

fn sshd_executable() -> Option<PathBuf> {
    let mut candidates = vec![
        PathBuf::from("/usr/sbin/sshd"),
        PathBuf::from("/usr/bin/sshd"),
    ];
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|directory| directory.join("sshd")));
    }
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .and_then(|path| fs::canonicalize(path).ok())
}

#[cfg(unix)]
impl Drop for SshdFixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(unix)]
fn generate_ssh_key(path: &Path) -> Result<()> {
    let output = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(path)
        .output()
        .context("run ssh-keygen for remote acceptance")?;
    ensure!(
        output.status.success(),
        "ssh-keygen failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn workspace_session_restores_folds_wrap_and_multiple_cursors(directory: &Path) -> Result<()> {
    const READY: &str = "E2E_PRESENTATION_SESSION_READY";
    const INNER: &str = "E2E_PRESENTATION_FOLD_INNER";

    let root = directory.join("presentation-session-repo");
    let session_directory = directory.join("presentation-session-state");
    fs::create_dir_all(&root).context("create presentation session fixture")?;
    let path = root.join("presentation.rs");
    fs::write(
        &path,
        format!(
            "one // {READY}\ntwo\n\nfn restored_fold() {{\n    // {INNER}\n}}\n// {}E2E_PRESENTATION_WRAP_TAIL\n",
            "w".repeat(190)
        ),
    )
    .context("write presentation session fixture")?;
    let environment = [
        (OsStr::new("ZEC_DISABLE_SESSIONS"), OsStr::new("0")),
        (OsStr::new("ZEC_SESSION_DIR"), session_directory.as_os_str()),
    ];

    let first_pair = open_pty()?;
    let first_termios = capture_baseline(&first_pair)?;
    let mut first = PtySession::spawn_with_env(
        first_pair,
        &[root.as_os_str(), path.as_os_str()],
        &environment,
    )?;
    first.wait_for_screen(
        "presentation session trust prompt",
        ACTION_TIMEOUT,
        |screen| screen.contains("Worktree Trust") && screen.contains("presentation-session-repo"),
    )?;
    first.send(b"\x1b")?;
    first.wait_for_screen("presentation session ready", ACTION_TIMEOUT, |screen| {
        screen.contains(READY) && !screen.contains("Worktree Trust")
    })?;

    first.send(CTRL_G)?;
    first.paste("4:1")?;
    first.send(ENTER)?;
    first.wait_for_screen("presentation fold target", ACTION_TIMEOUT, |screen| {
        screen.contains("line 4  |") && screen.contains(INNER)
    })?;
    first.send(F11)?;
    first.wait_for_screen("presentation fold captured", ACTION_TIMEOUT, |screen| {
        screen.contains("1 fold(s)") && !screen.contains(INNER)
    })?;
    first.send(ALT_Z)?;
    first.wait_for_screen("presentation wrap captured", ACTION_TIMEOUT, |screen| {
        screen.contains("soft wrap on")
            && screen.contains("E2E_PRESENTAT")
            && screen.contains("ION_WRAP_TAIL")
    })?;
    first.send(CTRL_G)?;
    first.paste("1:1")?;
    first.send(ENTER)?;
    first.send(SHIFT_ALT_DOWN)?;
    first.wait_for_screen("presentation cursors captured", ACTION_TIMEOUT, |screen| {
        screen.contains("editor state: 2 cursor(s)")
    })?;
    first.send(CTRL_Q)?;
    let first_status = first.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(
        first_status.success(),
        "presentation session first exit failed"
    );
    first.assert_terminal_restored(&first_termios)?;

    let second_pair = open_pty()?;
    let second_termios = capture_baseline(&second_pair)?;
    let mut second = PtySession::spawn_with_env(
        second_pair,
        &[root.as_os_str(), path.as_os_str()],
        &environment,
    )?;
    second.wait_for_screen(
        "restored presentation trust prompt",
        ACTION_TIMEOUT,
        |screen| screen.contains("Worktree Trust") && screen.contains("presentation-session-repo"),
    )?;
    second.send(b"\x1b")?;
    second.wait_for_screen("fold wrap and cursors restored", ACTION_TIMEOUT, |screen| {
        screen.contains(READY)
            && !screen.contains(INNER)
            && screen.contains("E2E_PRESENTAT")
            && screen.contains("ION_WRAP_TAIL")
    })?;
    second.paste("R")?;
    second.wait_for_screen(
        "restored cursors edit both rows",
        ACTION_TIMEOUT,
        |screen| screen.contains("Rone") && screen.contains("Rtwo"),
    )?;
    second.send(CTRL_Q)?;
    second.wait_for_screen(
        "restored presentation quit guard",
        ACTION_TIMEOUT,
        |screen| screen.contains("unsaved or deleted tab"),
    )?;
    second.send(CTRL_Q)?;
    let second_status = second.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(
        second_status.success(),
        "restored presentation session exit failed"
    );
    second.assert_terminal_restored(&second_termios)?;
    fs::remove_dir_all(&root).context("remove presentation session fixture")?;
    fs::remove_dir_all(&session_directory).context("remove presentation session state")?;
    Ok(())
}

fn corrupt_workspace_generation_is_quarantined_and_falls_back(directory: &Path) -> Result<()> {
    const READY: &str = "E2E_QUARANTINE_READY";

    let root = directory.join("quarantine-session-repo");
    let session_directory = directory.join("quarantine-session-state");
    fs::create_dir_all(&root).context("create quarantine session fixture")?;
    let path = root.join("quarantine.txt");
    fs::write(&path, format!("{READY}\n")).context("write quarantine session fixture")?;
    let environment = [
        (OsStr::new("ZEC_DISABLE_SESSIONS"), OsStr::new("0")),
        (OsStr::new("ZEC_SESSION_DIR"), session_directory.as_os_str()),
    ];

    let first_pair = open_pty()?;
    let first_termios = capture_baseline(&first_pair)?;
    let mut first = PtySession::spawn_with_env(
        first_pair,
        &[root.as_os_str(), path.as_os_str()],
        &environment,
    )?;
    first.wait_for_screen(
        "quarantine session trust prompt",
        ACTION_TIMEOUT,
        |screen| screen.contains("Worktree Trust") && screen.contains("quarantine-session-repo"),
    )?;
    first.send(b"\x1b")?;
    first.wait_for_screen("quarantine baseline ready", ACTION_TIMEOUT, |screen| {
        screen.contains(READY) && !screen.contains("Worktree Trust")
    })?;
    first.send(CTRL_Q)?;
    let first_status = first.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(first_status.success(), "quarantine baseline exit failed");
    first.assert_terminal_restored(&first_termios)?;

    let generation_path = fs::read_dir(&session_directory)
        .context("read baseline session directory")?
        .filter_map(|entry| entry.ok())
        .find_map(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.ends_with(".json"))
                .then_some(entry.path())
        })
        .context("baseline session generation was not written")?;
    let generation_name = generation_path
        .file_name()
        .and_then(OsStr::to_str)
        .context("baseline session file name is not UTF-8")?;
    let without_suffix = generation_name
        .strip_suffix(".json")
        .context("baseline session has wrong suffix")?;
    let (key_and_generation, _) = without_suffix
        .rsplit_once('-')
        .context("baseline session has no digest")?;
    let (key, _) = key_and_generation
        .rsplit_once('-')
        .context("baseline session has no generation")?;
    let corrupt = session_directory.join(format!(
        "{key}-{:020}-{}.json",
        u64::MAX - 1,
        "0".repeat(64)
    ));
    fs::write(&corrupt, b"{truncated").context("write corrupt newest generation")?;

    let second_pair = open_pty()?;
    let second_termios = capture_baseline(&second_pair)?;
    let mut second = PtySession::spawn_with_env(
        second_pair,
        &[root.as_os_str(), path.as_os_str()],
        &environment,
    )?;
    second.wait_for_screen(
        "quarantine restore trust prompt",
        ACTION_TIMEOUT,
        |screen| screen.contains("Worktree Trust") && screen.contains("quarantine-session-repo"),
    )?;
    second.send(b"\x1b")?;
    second.wait_until("corrupt generation fallback", ACTION_TIMEOUT, |session| {
        let screen = session.parser.screen().contents();
        screen.contains(READY)
            && !screen.contains("Worktree Trust")
            && contains_bytes(&session.transcript, b"ignored corrupt session")
    })?;
    ensure!(
        !corrupt.exists(),
        "rejected session remained eligible for the next startup"
    );
    ensure!(
        fs::read_dir(&session_directory)
            .context("read quarantined session directory")?
            .filter_map(|entry| entry.ok())
            .any(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with(".rejected-")),
        "rejected session was not preserved under a quarantine name"
    );
    second.send(CTRL_Q)?;
    let second_status = second.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(second_status.success(), "quarantine fallback exit failed");
    second.assert_terminal_restored(&second_termios)?;

    fs::remove_dir_all(&root).context("remove quarantine session fixture")?;
    fs::remove_dir_all(&session_directory).context("remove quarantine session state")?;
    Ok(())
}

fn advanced_editor_actions_use_zed_display_and_selection_state(directory: &Path) -> Result<()> {
    const READY: &str = "E2E_ADVANCED_EDITOR_READY";
    const INNER: &str = "E2E_FOLDED_INNER";
    const WRAP_TAIL: &str = "E2E_SOFT_WRAP_TAIL";

    let root = directory.join("advanced-editor-repo");
    fs::create_dir_all(&root).context("create advanced editor fixture")?;
    let path = root.join("advanced.rs");
    let long_prefix = "x".repeat(190);
    fs::write(
        &path,
        format!(
            "target alpha // {READY}\ntarget beta\n\nfn fold_me() {{\n    // {INNER}\n}}\n// {long_prefix}{WRAP_TAIL}\n"
        ),
    )
    .context("write advanced editor fixture")?;

    let pair = open_pty()?;
    let termios_before = capture_baseline(&pair)?;
    let mut session = PtySession::spawn(pair, &[path.as_os_str()])?;
    session.wait_until(
        "advanced editor fixture or trust prompt",
        STARTUP_TIMEOUT,
        |session| {
            let screen = session.parser.screen().contents();
            screen.contains(READY) || screen.contains("Worktree Trust")
        },
    )?;
    if session
        .parser
        .screen()
        .contents()
        .contains("Worktree Trust")
    {
        session.send(b"\x1b")?;
    }
    session.wait_for_screen("advanced editor fixture ready", ACTION_TIMEOUT, |screen| {
        screen.contains(READY) && !screen.contains("Worktree Trust")
    })?;
    session.assert_raw_mode_enabled(&termios_before)?;

    session.send(CTRL_D)?;
    session.send(CTRL_D)?;
    session.wait_for_screen("Zed occurrence selections", ACTION_TIMEOUT, |screen| {
        screen.contains("editor state: 2 cursor(s)")
    })?;
    session.paste("selected")?;
    session.wait_for_screen("multi-selection edit", ACTION_TIMEOUT, |screen| {
        screen.contains("selected alpha") && screen.contains("selected beta")
    })?;
    session.send(CTRL_Z)?;
    session.wait_for_screen("multi-selection undo", ACTION_TIMEOUT, |screen| {
        screen.contains("target alpha") && screen.contains("target beta")
    })?;

    session.send(CTRL_G)?;
    session.paste("1:1")?;
    session.send(ENTER)?;
    session.wait_for_screen("single cursor reset", ACTION_TIMEOUT, |screen| {
        screen.contains("line 1  |")
    })?;
    session.send(SHIFT_ALT_DOWN)?;
    session.wait_for_screen("cursor below", ACTION_TIMEOUT, |screen| {
        screen.contains("editor state: 2 cursor(s)")
    })?;

    session.send(CTRL_G)?;
    session.paste("4:1")?;
    session.send(ENTER)?;
    session.wait_for_screen("fold target selected", ACTION_TIMEOUT, |screen| {
        screen.contains("line 4  |") && screen.contains(INNER)
    })?;
    session.send(F11)?;
    session.wait_for_screen("single fold", ACTION_TIMEOUT, |screen| {
        screen.contains("editor state: 1 cursor(s), 1 fold(s)") && !screen.contains(INNER)
    })?;
    session.send(SHIFT_F11)?;
    session.wait_for_screen("unfold all", ACTION_TIMEOUT, |screen| {
        screen.contains("0 fold(s)") && screen.contains(INNER)
    })?;
    session.send(CTRL_F11)?;
    session.wait_for_screen("fold all", ACTION_TIMEOUT, |screen| {
        screen.contains("fold(s)") && !screen.contains(INNER)
    })?;
    session.send(SHIFT_F11)?;

    session.send(CTRL_G)?;
    session.paste("7:1")?;
    session.send(ENTER)?;
    session.wait_for_screen("long line selected", ACTION_TIMEOUT, |screen| {
        screen.contains("line 7  |") && !screen.contains(WRAP_TAIL)
    })?;
    session.send(ALT_Z)?;
    session.wait_for_screen("terminal-width soft wrap", ACTION_TIMEOUT, |screen| {
        screen.contains("soft wrap on")
            && screen.contains("E2E_SOFT_WRAP")
            && screen.contains("_TAIL")
    })?;

    session.send(CTRL_Q)?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(
        status.success(),
        "advanced editor zec exit failed: {status}"
    );
    session.assert_terminal_restored(&termios_before)?;
    fs::remove_dir_all(&root).context("remove advanced editor fixture after isolation check")?;
    Ok(())
}

fn mouse_drag_multi_click_and_additive_selection_use_zed_ranges(directory: &Path) -> Result<()> {
    const READY: &str = "E2E_MOUSE_READY";

    let root = directory.join("mouse-selection-repo");
    fs::create_dir_all(&root).context("create mouse selection fixture")?;
    let path = root.join("mouse.txt");
    fs::write(
        &path,
        format!(
            "drag alpha omega // {READY}\nword double target\ntriple line target\nalt cursor one\nalt cursor two\nRECT AAAAA\nRECT BB\nRECT CCCCC\n"
        ),
    )
    .context("write mouse selection fixture")?;

    let pair = open_pty()?;
    let termios_before = capture_baseline(&pair)?;
    let mut session = PtySession::spawn(pair, &[path.as_os_str()])?;
    session.wait_until(
        "mouse selection fixture or trust prompt",
        STARTUP_TIMEOUT,
        |session| {
            let screen = session.parser.screen().contents();
            screen.contains(READY) || screen.contains("Worktree Trust")
        },
    )?;
    if session
        .parser
        .screen()
        .contents()
        .contains("Worktree Trust")
    {
        session.send(b"\x1b")?;
    }
    session.wait_for_screen("mouse selection fixture ready", ACTION_TIMEOUT, |screen| {
        screen.contains(READY) && !screen.contains("Worktree Trust")
    })?;

    let (alpha_column, alpha_row) = session
        .find_screen_text("alpha")
        .context("find drag selection start")?;
    session.mouse_down(alpha_column, alpha_row, 0)?;
    session.mouse_drag(alpha_column + 5, alpha_row, 0)?;
    session.mouse_up(alpha_column + 5, alpha_row, 0)?;
    session.paste("DRAG")?;
    session.wait_for_screen("mouse drag replaces selection", ACTION_TIMEOUT, |screen| {
        screen.contains("drag DRAG omega")
    })?;

    let (double_column, double_row) = session
        .find_screen_text("double")
        .context("find double-click word")?;
    session.mouse_click(double_column, double_row, 0)?;
    session.mouse_click(double_column, double_row, 0)?;
    session.paste("WORD")?;
    session.wait_for_screen("double click replaces one word", ACTION_TIMEOUT, |screen| {
        screen.contains("word WORD target")
    })?;

    let (triple_column, triple_row) = session
        .find_screen_text("triple")
        .context("find triple-click line")?;
    session.mouse_click(triple_column, triple_row, 0)?;
    session.mouse_click(triple_column, triple_row, 0)?;
    session.mouse_click(triple_column, triple_row, 0)?;
    session.paste("LINE\n")?;
    session.wait_for_screen(
        "triple click replaces logical line",
        ACTION_TIMEOUT,
        |screen| screen.contains("LINE") && !screen.contains("triple line target"),
    )?;

    let (one_column, one_row) = session
        .find_screen_text("one")
        .context("find primary mouse cursor")?;
    let (two_column, two_row) = session
        .find_screen_text("two")
        .context("find additive mouse cursor")?;
    session.mouse_click(one_column, one_row, 0)?;
    session.mouse_click(two_column, two_row, 8)?;
    session.paste("X")?;
    session.wait_for_screen(
        "additive mouse cursors both edit",
        ACTION_TIMEOUT,
        |screen| screen.contains("alt cursor Xone") && screen.contains("alt cursor Xtwo"),
    )?;

    let (rectangle_start_column, rectangle_start_row) = session
        .find_screen_text("AAAAA")
        .context("find rectangular selection start")?;
    let (rectangle_end_column, rectangle_end_row) = session
        .find_screen_text("CCCCC")
        .context("find rectangular selection end")?;
    const SHIFT_CONTROL_MOUSE: u8 = 4 | 16;
    session.mouse_down(
        rectangle_start_column + 1,
        rectangle_start_row,
        SHIFT_CONTROL_MOUSE,
    )?;
    session.mouse_drag(
        rectangle_end_column + 4,
        rectangle_end_row,
        SHIFT_CONTROL_MOUSE,
    )?;
    session.mouse_up(
        rectangle_end_column + 4,
        rectangle_end_row,
        SHIFT_CONTROL_MOUSE,
    )?;
    session.paste("X")?;
    session.wait_for_screen(
        "rectangular mouse selection edits every visual column",
        ACTION_TIMEOUT,
        |screen| {
            screen.contains("RECT AXA") && screen.contains("RECT BX") && screen.contains("RECT CXC")
        },
    )?;

    session.send(CTRL_Q)?;
    session.wait_for_screen("mouse fixture quit guard", ACTION_TIMEOUT, |screen| {
        screen.contains("unsaved or deleted tab")
    })?;
    session.send(CTRL_Q)?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(
        status.success(),
        "mouse selection zec exit failed: {status}"
    );
    session.assert_terminal_restored(&termios_before)?;
    fs::remove_dir_all(&root).context("remove mouse selection fixture")?;
    Ok(())
}

fn editor_projection_settings_render_whitespace_and_guides(directory: &Path) -> Result<()> {
    const READY: &str = "E2E_PROJECTION_READY";

    let workspace = directory.join("projection-workspace");
    let root = workspace.join("project");
    let config = workspace.join("config");
    fs::create_dir_all(&root).context("create projection fixture repository")?;
    fs::create_dir_all(config.join("zed")).context("create projection fixture config")?;
    let path = root.join("projection.rs");
    fs::write(
        &path,
        format!("fn main() {{ // {READY}\n\tlet value = 1;\n    \n}}\n"),
    )
    .context("write projection fixture")?;
    fs::write(
        config.join("zed/settings.json"),
        r#"{
          "show_whitespaces": "all",
          "whitespace_map": { "space": "·", "tab": "→" },
          "show_wrap_guides": true,
          "wrap_guides": [24],
          "indent_guides": {
            "enabled": true,
            "line_width": 1,
            "active_line_width": 2,
            "coloring": "fixed",
            "background_coloring": "disabled"
          }
        }"#,
    )
    .context("write projection fixture settings")?;
    fs::write(config.join("zed/global_settings.json"), "{}")
        .context("write projection fixture global settings")?;
    fs::write(config.join("zed/keymap.json"), "[]").context("write projection fixture keymap")?;

    let pair = open_pty()?;
    let termios_before = capture_baseline(&pair)?;
    let environment = [(OsStr::new("XDG_CONFIG_HOME"), config.as_os_str())];
    let mut session = PtySession::spawn_with_env(pair, &[path.as_os_str()], &environment)?;
    session.wait_until(
        "projection fixture or trust prompt",
        STARTUP_TIMEOUT,
        |session| {
            let screen = session.parser.screen().contents();
            screen.contains(READY) || screen.contains("Worktree Trust")
        },
    )?;
    if session
        .parser
        .screen()
        .contents()
        .contains("Worktree Trust")
    {
        session.send(b"\x1b")?;
    }
    session.wait_for_screen(
        "configured whitespace and guide projection",
        ACTION_TIMEOUT,
        |screen| {
            screen.contains("fn·main()·{")
                && screen.contains('→')
                && screen.contains('│')
                && !screen.contains("Worktree Trust")
        },
    )?;
    session.send(CTRL_Q)?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(
        status.success(),
        "projection fixture zec exit failed: {status}"
    );
    session.assert_terminal_restored(&termios_before)
}

fn workspace_session_restores_a_live_editable_multibuffer(directory: &Path) -> Result<()> {
    const MATCH: &str = "E2E_LIVE_MULTIBUFFER_MATCH";
    const EDIT: &str = "E2E_RESTORED_MULTIBUFFER_EDIT_";

    let root = directory.join("multibuffer-session-repo");
    let session_directory = directory.join("multibuffer-session-state");
    fs::create_dir_all(root.join("src")).context("create MultiBuffer session fixture")?;
    let first_path = root.join("README.md");
    let second_path = root.join("src/second.txt");
    fs::write(&first_path, format!("first {MATCH}\n")).context("write first MultiBuffer source")?;
    fs::write(&second_path, format!("second {MATCH}\n"))
        .context("write second MultiBuffer source")?;
    let environment = [
        (OsStr::new("ZEC_DISABLE_SESSIONS"), OsStr::new("0")),
        (OsStr::new("ZEC_SESSION_DIR"), session_directory.as_os_str()),
    ];

    let first_pair = open_pty()?;
    let first_termios = capture_baseline(&first_pair)?;
    let mut first = PtySession::spawn_with_env(first_pair, &[root.as_os_str()], &environment)?;
    first.wait_for_screen(
        "MultiBuffer session trust prompt",
        ACTION_TIMEOUT,
        |screen| screen.contains("Worktree Trust") && screen.contains("multibuffer-session-repo"),
    )?;
    first.send(b"\x1b")?;
    first.wait_for_screen(
        "MultiBuffer session repository ready",
        ACTION_TIMEOUT,
        |screen| screen.contains(MATCH) && !screen.contains("Worktree Trust"),
    )?;
    first.send(ALT_F)?;
    first.wait_for_screen(
        "MultiBuffer project search prompt",
        ACTION_TIMEOUT,
        |screen| screen.contains("Project search:"),
    )?;
    first.paste(MATCH)?;
    first.wait_for_screen(
        "MultiBuffer project search results",
        ACTION_TIMEOUT,
        |screen| screen.contains("1/2") && screen.contains(MATCH),
    )?;
    first.send(F9)?;
    first.wait_for_screen("editable MultiBuffer opened", ACTION_TIMEOUT, |screen| {
        screen.contains("opened 2 project-search result(s) in an editable MultiBuffer")
            && screen.contains("Project Search:")
    })?;
    first.send(CTRL_Q)?;
    let first_status = first.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(
        first_status.success(),
        "first MultiBuffer session exit failed"
    );
    first.assert_terminal_restored(&first_termios)?;

    let second_pair = open_pty()?;
    let second_termios = capture_baseline(&second_pair)?;
    let mut second = PtySession::spawn_with_env(second_pair, &[root.as_os_str()], &environment)?;
    second.wait_for_screen(
        "restored MultiBuffer trust prompt",
        ACTION_TIMEOUT,
        |screen| screen.contains("Worktree Trust") && screen.contains("multibuffer-session-repo"),
    )?;
    second.send(b"\x1b")?;
    second.wait_for_screen("live MultiBuffer restored", ACTION_TIMEOUT, |screen| {
        screen.contains("Project Search:")
            && screen.contains(MATCH)
            && !screen.contains("protected recovery buffer")
    })?;
    second.paste(EDIT)?;
    second.wait_for_screen("restored MultiBuffer edited", ACTION_TIMEOUT, |screen| {
        screen.contains(EDIT)
    })?;
    second.send(CTRL_S)?;
    second.wait_for_screen(
        "restored MultiBuffer sources saved",
        ACTION_TIMEOUT,
        |screen| screen.contains("saved") && !screen.contains("save failed"),
    )?;
    second.wait_until(
        "restored MultiBuffer edit reached one source",
        ACTION_TIMEOUT,
        |_| {
            [first_path.as_path(), second_path.as_path()]
                .into_iter()
                .filter(|path| fs::read_to_string(path).is_ok_and(|text| text.contains(EDIT)))
                .count()
                == 1
        },
    )?;
    second.send(CTRL_Q)?;
    let second_status = second.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(
        second_status.success(),
        "restored MultiBuffer session exit failed"
    );
    second.assert_terminal_restored(&second_termios)
}

fn outline_filters_follows_and_jumps_through_zed_symbols(directory: &Path) -> Result<()> {
    const READY: &str = "E2E_OUTLINE_READY";
    const TARGET: &str = "E2E_OUTLINE_JUMP_TARGET";

    let root = directory.join("outline-repo");
    fs::create_dir_all(&root).context("create outline fixture")?;
    let path = root.join("outline.rs");
    let filler = (0..48)
        .map(|index| format!("        // outline filler {index}\n"))
        .collect::<String>();
    fs::write(
        &path,
        format!(
            "// {READY}\nmod container {{\n    fn first() {{\n{filler}    }}\n\n    fn nested_target() {{\n        // {TARGET}\n    }}\n}}\n"
        ),
    )
    .context("write outline fixture")?;

    let pair = open_pty()?;
    let termios_before = capture_baseline(&pair)?;
    let mut session = PtySession::spawn(pair, &[path.as_os_str()])?;
    session.wait_until(
        "outline fixture or trust prompt",
        STARTUP_TIMEOUT,
        |session| {
            let screen = session.parser.screen().contents();
            screen.contains(READY) || screen.contains("Worktree Trust")
        },
    )?;
    if session
        .parser
        .screen()
        .contents()
        .contains("Worktree Trust")
    {
        session.send(b"\x1b")?;
    }
    session.wait_for_screen("outline fixture ready", ACTION_TIMEOUT, |screen| {
        screen.contains(READY) && !screen.contains("Worktree Trust")
    })?;
    session.assert_raw_mode_enabled(&termios_before)?;

    session.send(F9)?;
    session.wait_for_screen("Zed outline dock", ACTION_TIMEOUT, |screen| {
        screen.contains("Outline outline.rs")
            && screen.contains("container")
            && screen.contains("nested_target")
    })?;
    session.send(b"/")?;
    session.paste("nested_target")?;
    session.wait_for_screen("live outline filter", ACTION_TIMEOUT, |screen| {
        screen.contains("Outline /nested_target") && screen.contains("nested_target")
    })?;
    session.send(ENTER)?;
    session.wait_for_screen("outline filter accepted", ACTION_TIMEOUT, |screen| {
        screen.contains("outline filter applied: nested_target")
    })?;
    let (_, target_row) = session
        .find_screen_text_between("nested_target", 1, INITIAL_SIZE.rows.saturating_sub(1))
        .context("filtered outline row is not visible")?;
    session.mouse_click(132, target_row, 0)?;
    session.wait_for_screen("mouse outline symbol jump", ACTION_TIMEOUT, |screen| {
        screen.contains("fn nested_target()")
            && screen.contains("jumped to outline symbol fn nested_target")
            && screen.contains("container")
            && screen.contains("nested_target")
    })?;

    session.send(F9)?;
    session.wait_for_screen("outline refocused", ACTION_TIMEOUT, |screen| {
        screen.contains("Outline /nested_target") && screen.contains("outline focused")
    })?;
    session.send(b"f")?;
    session.wait_for_screen("outline follow toggled", ACTION_TIMEOUT, |screen| {
        screen.contains("outline cursor following disabled")
    })?;
    session.send(F9)?;
    session.wait_for_screen("outline hidden", ACTION_TIMEOUT, |screen| {
        screen.contains("outline panel hidden") && !screen.contains(" Outline ")
    })?;
    session.send(CTRL_Q)?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(status.success(), "outline zec exit failed: {status}");
    session.assert_terminal_restored(&termios_before)
}

#[cfg(unix)]
fn periodic_session_snapshot_survives_sigkill(directory: &Path) -> Result<()> {
    const READY: &str = "E2E_CRASH_SESSION_READY";
    const RECOVERED: &str = "E2E_PERIODIC_SNAPSHOT_SURVIVED_SIGKILL";

    let root = directory.join("crash-session-repo");
    let session_directory = directory.join("crash-session-state");
    fs::create_dir_all(&root).context("create crash session fixture")?;
    fs::write(root.join("README.md"), format!("{READY}\n"))
        .context("write crash session README")?;
    let environment = [
        (OsStr::new("ZEC_DISABLE_SESSIONS"), OsStr::new("0")),
        (OsStr::new("ZEC_SESSION_DIR"), session_directory.as_os_str()),
    ];

    let crash_pair = open_pty()?;
    let mut crashed = PtySession::spawn_with_env(crash_pair, &[root.as_os_str()], &environment)?;
    crashed.wait_for_screen("crash session trust prompt", ACTION_TIMEOUT, |screen| {
        screen.contains("Worktree Trust") && screen.contains("crash-session-repo")
    })?;
    crashed.send(b"\x1b")?;
    crashed.wait_for_screen("crash session fixture ready", ACTION_TIMEOUT, |screen| {
        screen.contains(READY) && !screen.contains("Worktree Trust")
    })?;
    crashed.send(CTRL_N)?;
    crashed.paste(RECOVERED)?;
    crashed.wait_for_screen("crash recovery text dirty", ACTION_TIMEOUT, |screen| {
        screen.contains(RECOVERED) && screen.contains("Untitled 1+")
    })?;
    crashed.wait_until("periodic recovery commit", ACTION_TIMEOUT, |_| {
        let Ok(entries) = fs::read_dir(&session_directory) else {
            return false;
        };
        let mut has_generation = false;
        let mut has_recovery = false;
        for entry in entries.flatten() {
            let path = entry.path();
            match path.extension().and_then(OsStr::to_str) {
                Some("json") => has_generation = true,
                Some("blob") => {
                    has_recovery |= fs::read(&path)
                        .is_ok_and(|bytes| contains_bytes(&bytes, RECOVERED.as_bytes()));
                }
                _ => {}
            }
        }
        has_generation && has_recovery
    })?;
    crashed.send_signal(Signal::SIGKILL)?;
    let killed = crashed.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(!killed.success(), "SIGKILL fixture exited successfully");

    let restored_pair = open_pty()?;
    let restored_termios = capture_baseline(&restored_pair)?;
    let mut restored =
        PtySession::spawn_with_env(restored_pair, &[root.as_os_str()], &environment)?;
    restored.wait_for_screen("crash recovery trust prompt", ACTION_TIMEOUT, |screen| {
        screen.contains("Worktree Trust") && screen.contains("crash-session-repo")
    })?;
    restored.send(b"\x1b")?;
    restored.wait_for_screen("periodic snapshot restored", ACTION_TIMEOUT, |screen| {
        screen.contains(RECOVERED) && screen.contains("Recovered Untitled 1")
    })?;
    restored.send(CTRL_Q)?;
    restored.wait_for_screen("crash recovery quit guard", ACTION_TIMEOUT, |screen| {
        screen.contains("unsaved or deleted tab")
    })?;
    restored.send(CTRL_Q)?;
    let status = restored.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(status.success(), "crash recovery exit failed: {status}");
    restored.assert_terminal_restored(&restored_termios)
}

fn workspace_session_restores_layout_dock_and_unsaved_content(directory: &Path) -> Result<()> {
    const READY: &str = "E2E_SESSION_FILE_READY";
    const RECOVERED: &str = "E2E_UNSAVED_SESSION_RECOVERY";

    let root = directory.join("workspace-session-repo");
    let session_directory = directory.join("workspace-session-state");
    fs::create_dir_all(&root).context("create workspace session fixture")?;
    fs::write(root.join("README.md"), format!("{READY}\n"))
        .context("write workspace session README")?;
    let environment = [
        (OsStr::new("ZEC_DISABLE_SESSIONS"), OsStr::new("0")),
        (OsStr::new("ZEC_SESSION_DIR"), session_directory.as_os_str()),
    ];

    let first_pair = open_pty()?;
    let first_termios = capture_baseline(&first_pair)?;
    let mut first = PtySession::spawn_with_env(first_pair, &[root.as_os_str()], &environment)?;
    first.wait_for_screen("session fixture trust prompt", ACTION_TIMEOUT, |screen| {
        screen.contains("Worktree Trust") && screen.contains("workspace-session-repo")
    })?;
    first.send(b"\x1b")?;
    first.wait_for_screen("session fixture ready", ACTION_TIMEOUT, |screen| {
        screen.contains(READY) && !screen.contains("Worktree Trust")
    })?;
    first.send(F10)?;
    first.wait_for_screen("session split created", ACTION_TIMEOUT, |screen| {
        screen.contains("split right into pane")
    })?;
    first.send(CTRL_N)?;
    first.wait_for_screen("session scratch created", ACTION_TIMEOUT, |screen| {
        screen.contains("Untitled 1")
    })?;
    first.paste(RECOVERED)?;
    first.wait_for_screen("session scratch dirty", ACTION_TIMEOUT, |screen| {
        screen.contains(RECOVERED) && screen.contains("Untitled 1+")
    })?;
    first.send(F7)?;
    first.wait_for_screen("session dock focused", ACTION_TIMEOUT, |screen| {
        screen.contains("Project") && screen.contains("project panel focused")
    })?;
    first.send(CTRL_Q)?;
    first.wait_for_screen("session quit guard", ACTION_TIMEOUT, |screen| {
        screen.contains("unsaved or deleted tab")
    })?;
    first.send(CTRL_Q)?;
    let first_status = first.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(
        first_status.success(),
        "first session exit failed: {first_status}"
    );
    first.assert_terminal_restored(&first_termios)?;
    ensure!(
        session_directory.is_dir(),
        "session directory was not created"
    );
    ensure!(
        fs::read_dir(&session_directory)
            .context("read committed session directory")?
            .filter_map(|entry| entry.ok())
            .any(|entry| entry.path().extension() == Some(OsStr::new("json"))),
        "clean quit wrote no session generation"
    );

    let second_pair = open_pty()?;
    let second_termios = capture_baseline(&second_pair)?;
    let mut second = PtySession::spawn_with_env(second_pair, &[root.as_os_str()], &environment)?;
    second.wait_for_screen("restored session trust prompt", ACTION_TIMEOUT, |screen| {
        screen.contains("Worktree Trust") && screen.contains("workspace-session-repo")
    })?;
    second.send(b"\x1b")?;
    second.wait_for_screen(
        "unsaved session content restored",
        ACTION_TIMEOUT,
        |screen| {
            screen.contains(RECOVERED)
                && screen.contains("Recovered Untitled 1")
                && screen.contains("Project")
        },
    )?;
    second.send(b"\x1b")?;
    second.wait_for_screen(
        "restored dock returned to editor",
        ACTION_TIMEOUT,
        |screen| screen.contains("editor focused") && screen.contains(RECOVERED),
    )?;
    second.send(CTRL_ALT_LEFT)?;
    second.wait_for_screen("restored split navigates left", ACTION_TIMEOUT, |screen| {
        screen.contains("focused left pane") && screen.contains(READY)
    })?;
    second.send(CTRL_ALT_RIGHT)?;
    second.wait_for_screen("restored split navigates right", ACTION_TIMEOUT, |screen| {
        screen.contains("focused right pane") && screen.contains(RECOVERED)
    })?;
    second.send(CTRL_Q)?;
    second.wait_for_screen("restored recovery quit guard", ACTION_TIMEOUT, |screen| {
        screen.contains("unsaved or deleted tab")
    })?;
    second.send(CTRL_Q)?;
    let second_status = second.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(
        second_status.success(),
        "restored session exit failed: {second_status}"
    );
    second.assert_terminal_restored(&second_termios)
}

fn project_panel_mutations_preserve_dirty_buffer_identity(directory: &Path) -> Result<()> {
    const READY: &str = "E2E_MUTATION_READY";
    const DIRTY: &str = "E2E_DIRTY_BUFFER_SURVIVES_RENAME";

    let root = directory.join("project-mutation-repo");
    fs::create_dir_all(&root).context("create project mutation fixture")?;
    fs::write(root.join("README.md"), format!("{READY}\n"))
        .context("write project mutation README")?;

    let created = root.join("created.txt");
    let renamed = root.join("renamed.txt");
    let copied = root.join("copied.txt");
    let pair = open_pty()?;
    let termios_before = capture_baseline(&pair)?;
    let mut session = PtySession::spawn(pair, &[root.as_os_str()])?;
    session.wait_for_screen(
        "project mutation worktree trust",
        ACTION_TIMEOUT,
        |screen| screen.contains("Worktree Trust") && screen.contains("project-mutation-repo"),
    )?;
    session.send(ENTER)?;
    session.wait_for_screen(
        "project mutation worktree trusted",
        ACTION_TIMEOUT,
        |screen| screen.contains("worktree trusted for this session") && screen.contains(READY),
    )?;
    session.assert_raw_mode_enabled(&termios_before)?;

    session.send(F7)?;
    session.wait_for_screen("project mutation panel focused", ACTION_TIMEOUT, |screen| {
        screen.contains("Project") && screen.contains("README.md")
    })?;
    session.send(b"n")?;
    session.wait_for_screen("create-file input", ACTION_TIMEOUT, |screen| {
        screen.contains("Create file: new-file")
    })?;
    session.send(CTRL_U)?;
    session.paste("created.txt")?;
    session.send(ENTER)?;
    session.wait_for_screen("create-file preview", ACTION_TIMEOUT, |screen| {
        screen.contains("Project mutation preview: create file created.txt")
    })?;
    session.send(ENTER)?;
    session.wait_for_screen("create-file applied", ACTION_TIMEOUT, |screen| {
        screen.contains("project mutation applied: create file created.txt")
    })?;
    session.wait_until("created file on disk", ACTION_TIMEOUT, |_| {
        created.is_file()
    })?;

    session.send(b"/")?;
    session.send(CTRL_U)?;
    session.paste("created.txt")?;
    session.send(ENTER)?;
    session.send(b"\x1b[B")?;
    session.wait_for_screen("created entry selected", ACTION_TIMEOUT, |screen| {
        screen.contains("created.txt")
    })?;
    session.send(ENTER)?;
    session.wait_for_screen("created file previewed", ACTION_TIMEOUT, |screen| {
        screen.contains("previewing created.txt")
    })?;
    session.paste(DIRTY)?;
    session.wait_for_screen("created file dirty", ACTION_TIMEOUT, |screen| {
        screen.contains(DIRTY) && screen.contains("created.txt+")
    })?;

    session.send(F7)?;
    session.send(F2)?;
    session.wait_for_screen("rename input", ACTION_TIMEOUT, |screen| {
        screen.contains("Rename to: created.txt")
    })?;
    session.send(CTRL_U)?;
    session.paste("renamed.txt")?;
    session.send(ENTER)?;
    session.wait_for_screen("rename preview", ACTION_TIMEOUT, |screen| {
        screen.contains("Project mutation preview: rename created.txt to renamed.txt")
    })?;
    session.send(ENTER)?;
    session.wait_for_screen("rename applied", ACTION_TIMEOUT, |screen| {
        screen.contains("project mutation applied: rename created.txt to renamed.txt")
            && screen.contains(DIRTY)
    })?;
    session.wait_until("renamed path on disk", ACTION_TIMEOUT, |_| {
        renamed.is_file() && !created.exists()
    })?;

    session.send(b"\x1b")?;
    session.wait_for_screen("editor refocused after rename", ACTION_TIMEOUT, |screen| {
        screen.contains("editor focused") && screen.contains("renamed.txt+")
    })?;
    session.send(CTRL_S)?;
    session.wait_for_screen("renamed dirty buffer saved", ACTION_TIMEOUT, |screen| {
        screen.contains("saved")
            && screen.contains("renamed.txt")
            && !screen.contains("renamed.txt+")
    })?;
    session.wait_until("renamed bytes saved", ACTION_TIMEOUT, |_| {
        fs::read_to_string(&renamed).is_ok_and(|text| text.contains(DIRTY))
    })?;

    session.send(F7)?;
    session.send(b"/")?;
    session.send(CTRL_U)?;
    session.paste("renamed.txt")?;
    session.send(ENTER)?;
    session.send(b"\x1b[B")?;
    session.send(b"\x04")?;
    session.wait_for_screen("copy input", ACTION_TIMEOUT, |screen| {
        screen.contains("Copy to: renamed-copy.txt")
    })?;
    session.send(CTRL_U)?;
    session.paste("copied.txt")?;
    session.send(ENTER)?;
    session.wait_for_screen("copy preview", ACTION_TIMEOUT, |screen| {
        screen.contains("Project mutation preview: copy renamed.txt to copied.txt")
    })?;
    session.send(ENTER)?;
    session.wait_for_screen("copy applied", ACTION_TIMEOUT, |screen| {
        screen.contains("project mutation applied: copy renamed.txt to copied.txt")
    })?;
    session.wait_until("copied bytes on disk", ACTION_TIMEOUT, |_| {
        fs::read_to_string(&copied).is_ok_and(|text| text.contains(DIRTY))
    })?;

    session.send(b"/")?;
    session.send(CTRL_U)?;
    session.paste("copied.txt")?;
    session.send(ENTER)?;
    session.send(b"\x1b[B")?;
    session.send(DELETE)?;
    session.wait_for_screen("delete preview", ACTION_TIMEOUT, |screen| {
        screen.contains("Project mutation preview: delete copied.txt permanently")
    })?;
    session.send(ENTER)?;
    session.wait_for_screen("delete applied", ACTION_TIMEOUT, |screen| {
        screen.contains("project mutation applied: delete copied.txt permanently")
    })?;
    session.wait_until("copied path deleted", ACTION_TIMEOUT, |_| !copied.exists())?;

    session.send(CTRL_Q)?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(
        status.success(),
        "project mutation zec exit failed: {status}"
    );
    ensure!(
        fs::read_to_string(&renamed).is_ok_and(|text| text.contains(DIRTY)),
        "renamed editor buffer was not preserved and saved"
    );
    session.assert_terminal_restored(&termios_before)
}

fn project_panel_previews_and_replaces_by_entry_identity(directory: &Path) -> Result<()> {
    const READY: &str = "E2E_PANEL_READY";
    const FIRST: &str = "E2E_PANEL_FIRST_PREVIEW";
    const SECOND: &str = "E2E_PANEL_SECOND_PREVIEW";

    let root = directory.join("project-panel-repo");
    fs::create_dir_all(root.join("src")).context("create project panel fixture")?;
    fs::write(root.join("README.md"), format!("{READY}\n"))
        .context("write project panel README")?;
    fs::write(root.join("src/main.rs"), format!("{FIRST}\n"))
        .context("write first project panel file")?;
    fs::write(root.join("src/second.rs"), format!("{SECOND}\n"))
        .context("write second project panel file")?;

    let pair = open_pty()?;
    let termios_before = capture_baseline(&pair)?;
    let mut session = PtySession::spawn(pair, &[root.as_os_str()])?;
    session.wait_for_screen("project panel worktree trust", ACTION_TIMEOUT, |screen| {
        screen.contains("Worktree Trust") && screen.contains("project-panel-repo")
    })?;
    session.send(b"\x1b")?;
    session.wait_for_screen("project panel repository ready", ACTION_TIMEOUT, |screen| {
        screen.contains(READY) && !screen.contains("Worktree Trust")
    })?;
    session.assert_raw_mode_enabled(&termios_before)?;

    session.send(F7)?;
    session.wait_for_screen(
        "project panel visible and focused",
        ACTION_TIMEOUT,
        |screen| {
            screen.contains("Project") && screen.contains("src") && screen.contains("README.md")
        },
    )?;
    session.mouse_down(29, 10, 0)?;
    session.mouse_drag(39, 10, 0)?;
    session.mouse_up(39, 10, 0)?;
    session.wait_for_screen("mouse-resized project dock", ACTION_TIMEOUT, |screen| {
        screen.contains("resized left dock to 40 cells") && screen.contains("Project")
    })?;
    session.mouse_click(3, 2, 0)?;
    session.mouse_click(3, 2, 0)?;
    session.wait_for_screen(
        "mouse-expanded project directory",
        ACTION_TIMEOUT,
        |screen| screen.contains("main.rs") && screen.contains("second.rs"),
    )?;
    session.mouse_click(5, 3, 0)?;
    session.wait_for_screen("mouse project file preview", ACTION_TIMEOUT, |screen| {
        screen.contains(FIRST) && screen.contains("previewing src/main.rs")
    })?;

    session.send(F7)?;
    session.mouse_click(5, 4, 0)?;
    session.wait_for_screen("mouse preview replacement", ACTION_TIMEOUT, |screen| {
        screen.contains(SECOND)
            && screen.contains("previewing src/second.rs")
            && !screen.contains(FIRST)
    })?;

    session.send(F7)?;
    session.wait_for_screen(
        "visible project panel refocused",
        ACTION_TIMEOUT,
        |screen| screen.contains("Project") && screen.contains("second.rs"),
    )?;
    session.send(F7)?;
    session.wait_for_screen("project panel hidden", ACTION_TIMEOUT, |screen| {
        screen.contains("project panel hidden") && !screen.contains(" Project ")
    })?;
    session.send(b"\x17")?;
    session.wait_for_screen("preview closed", ACTION_TIMEOUT, |screen| {
        screen.contains(READY) && !screen.contains(SECOND)
    })?;
    session.send(CTRL_Q)?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(status.success(), "project panel zec exit failed: {status}");
    session.assert_terminal_restored(&termios_before)
}

fn workspace_split_focus_move_and_collapse(directory: &Path) -> Result<()> {
    const BODY: &str = "E2E_WORKSPACE_SPLIT_BODY";
    let path = directory.join("workspace-split.txt");
    fs::write(&path, format!("{BODY}\nsecond row\n")).context("write split fixture")?;

    let pair = open_pty()?;
    let termios_before = capture_baseline(&pair)?;
    let mut session = PtySession::spawn(pair, &[path.as_os_str()])?;
    session.wait_for_screen("workspace split fixture ready", STARTUP_TIMEOUT, |screen| {
        screen.contains(BODY)
    })?;
    session.assert_raw_mode_enabled(&termios_before)?;

    session.send(F10)?;
    session.wait_for_screen("horizontal workspace split", ACTION_TIMEOUT, |screen| {
        screen.contains(BODY) && screen.contains("split right into pane")
    })?;
    let (divider_column, _) = session
        .find_screen_text("│")
        .context("horizontal split did not render a resize handle")?;
    let dragged_column = divider_column.saturating_add(20);
    session.mouse_down(divider_column, 5, 0)?;
    session.mouse_drag(dragged_column, 5, 0)?;
    session.mouse_up(dragged_column, 5, 0)?;
    session.wait_until("mouse-resized workspace split", ACTION_TIMEOUT, |session| {
        session
            .parser
            .screen()
            .contents()
            .contains("resized split to")
            && session
                .find_screen_text("│")
                .is_some_and(|(column, _)| column > divider_column)
    })?;
    let (resized_divider_column, _) = session
        .find_screen_text("│")
        .context("resized split lost its resize handle")?;
    ensure!(
        resized_divider_column > divider_column,
        "split divider did not move right: {divider_column} -> {resized_divider_column}"
    );
    session.send(SHIFT_F10)?;
    session.wait_for_screen(
        "nested vertical workspace split",
        ACTION_TIMEOUT,
        |screen| screen.contains(BODY) && screen.contains("split down into pane"),
    )?;
    session.send(CTRL_ALT_LEFT)?;
    session.wait_for_screen("directional workspace focus", ACTION_TIMEOUT, |screen| {
        screen.contains("focused left pane")
    })?;
    session.send(CTRL_ALT_RIGHT)?;
    session.wait_for_screen(
        "reverse directional workspace focus",
        ACTION_TIMEOUT,
        |screen| screen.contains("focused right pane"),
    )?;
    session.send(CTRL_ALT_SHIFT_LEFT)?;
    session.wait_for_screen(
        "move item and collapse source pane",
        ACTION_TIMEOUT,
        |screen| screen.contains("moved tab to left pane") && screen.contains(BODY),
    )?;

    session.send(CTRL_Q)?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(
        status.success(),
        "workspace split zec exit failed: {status}"
    );
    session.assert_terminal_restored(&termios_before)
}

fn normal_edit_undo_resize_save_and_quit(directory: &Path) -> Result<()> {
    const INITIAL: &str = "seed line\n";
    const INSERTED: &str = "先頭-";
    const REPLACEMENT: &str = "置換🧪\n";
    const FINAL: &str = "保存🌍 after resize\nsecond line\n";

    let path = directory.join("normal.txt");
    fs::write(&path, INITIAL).context("write normal editing fixture")?;

    let pair = open_pty()?;
    let termios_before = capture_baseline(&pair)?;
    let mut session = PtySession::spawn(pair, &[path.as_os_str()])?;
    session.wait_ready()?;
    session.assert_raw_mode_enabled(&termios_before)?;

    session.send(F4)?;
    session.wait_for_screen("terminal capability report", ACTION_TIMEOUT, |screen| {
        screen.contains("terminal keyboard=modifyOtherKeys")
            && screen.contains("mouse=on/unverified")
    })?;
    session.send(b"\x1b[O")?;
    session.wait_for_screen("terminal focus loss event", ACTION_TIMEOUT, |screen| {
        screen.contains("terminal focus lost") && screen.contains("focus=on/seen")
    })?;
    session.send(b"\x1b[I")?;
    session.wait_for_screen("terminal focus gain event", ACTION_TIMEOUT, |screen| {
        screen.contains("terminal focus gained") && screen.contains("focus=on/seen")
    })?;

    session.paste(INSERTED)?;
    session.wait_for_screen("Unicode insertion", ACTION_TIMEOUT, |screen| {
        screen.contains("先頭-seed line")
    })?;

    session.send(CTRL_A)?;
    session.paste(REPLACEMENT)?;
    session.wait_for_screen("selection replacement", ACTION_TIMEOUT, |screen| {
        screen.contains("置換🧪") && !screen.contains("seed line")
    })?;

    session.send(CTRL_Z)?;
    // Unix paste is one atomic transaction, so a single undo returns to
    // the inserted state. Windows paste arrives as keystrokes (a ConPTY
    // property), and Zed's time-based grouping folds the rapid harness
    // input into one transaction covering both edits.
    #[cfg(unix)]
    session.wait_for_screen("Zed undo", ACTION_TIMEOUT, |screen| {
        screen.contains("先頭-seed line") && !screen.contains("置換🧪")
    })?;
    #[cfg(windows)]
    session.wait_for_screen("Zed undo", ACTION_TIMEOUT, |screen| {
        screen.contains("seed line") && !screen.contains("置換🧪")
    })?;

    session.resize(RESIZED)?;
    for _ in 0..20 {
        session.resize_immediately(PtySize {
            rows: 1,
            cols: 1,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        session.resize_immediately(RESIZED)?;
    }
    // The Windows undo above returned to the original seed text.
    let post_undo_body = if cfg!(unix) {
        "先頭-seed line"
    } else {
        "seed line"
    };
    session.wait_for_screen("redraw after PTY resize storm", ACTION_TIMEOUT, |screen| {
        screen.contains("zec ") && screen.contains(post_undo_body)
    })?;
    session.send(F4)?;
    session.wait_for_screen("input after PTY resize storm", ACTION_TIMEOUT, |screen| {
        screen.contains("terminal keyboard=")
    })?;
    session.send(CTRL_A)?;
    session.paste(FINAL)?;
    session.wait_for_screen("editing after resize", ACTION_TIMEOUT, |screen| {
        screen.contains("保存🌍 after resize") && screen.contains("second line")
    })?;

    session.send(CTRL_Q)?;
    session.wait_for_screen("dirty quit confirmation", ACTION_TIMEOUT, |screen| {
        screen.contains("unsaved or deleted tab(s); press Ctrl-Q again to discard")
    })?;
    session.ensure_running("first Ctrl-Q must not discard a dirty document")?;

    // A different command clears the armed quit and saves through Zed's BufferStore.
    session.send(CTRL_S)?;
    session.wait_for_screen("successful save", ACTION_TIMEOUT, |screen| {
        screen.contains("saved  |  zec")
    })?;
    let bytes = fs::read(&path).context("read saved normal fixture")?;
    ensure!(bytes == FINAL.as_bytes(), "saved bytes differ: {bytes:?}");

    session.send(CTRL_Q)?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(status.success(), "normal zec exit failed: {status}");
    session.assert_terminal_restored(&termios_before)?;

    Ok(())
}

#[cfg(unix)]
fn directory_quick_open_deduplicates_symlink_alias(directory: &Path) -> Result<()> {
    const READY: &str = "E2E_READY_SENTINEL";
    const OPENED: &str = "E2E_QUICK_OPEN_BODY";

    let root = directory.join("quick-open-repo");
    fs::create_dir_all(root.join("src")).context("create quick-open src")?;
    fs::create_dir_all(root.join("aliases")).context("create quick-open aliases")?;
    fs::write(root.join("README.md"), format!("{READY}\n")).context("write quick-open README")?;
    fs::write(root.join("src/日本 語.rs"), format!("{OPENED}\n"))
        .context("write quick-open target")?;
    std::os::unix::fs::symlink("../src/日本 語.rs", root.join("aliases/日本 語.rs"))
        .context("create quick-open symlink alias")?;

    let pair = open_pty()?;
    let termios_before = capture_baseline(&pair)?;
    let mut session = PtySession::spawn(pair, &[root.as_os_str()])?;
    session.wait_for_screen("restricted Markdown worktree", ACTION_TIMEOUT, |screen| {
        screen.contains("Worktree Trust") && screen.contains("quick-open-repo")
    })?;
    session.send(b"\x1b")?;
    session.wait_for_screen(
        "restricted worktree prompt dismissed",
        ACTION_TIMEOUT,
        |screen| screen.contains(READY) && !screen.contains("Worktree Trust"),
    )?;
    session.assert_raw_mode_enabled(&termios_before)?;
    session.wait_for_screen("directory README ready", ACTION_TIMEOUT, |screen| {
        screen.contains(READY) && screen.contains("quick-open-repo")
    })?;

    session.send(CTRL_P)?;
    session.wait_for_screen("quick-open prompt", ACTION_TIMEOUT, |screen| {
        screen.contains("Quick open:")
    })?;
    session.paste("日本 語.rs")?;
    session.wait_for_screen("quick-open selected path", ACTION_TIMEOUT, |screen| {
        screen.contains("Quick open: 日本 語.rs") && screen.contains("src/日本 語.rs")
    })?;
    session.send(ENTER)?;
    session.wait_for_screen("quick-open target opened", ACTION_TIMEOUT, |screen| {
        screen.contains(OPENED)
            && screen.contains("opened src/日本 語.rs")
            && screen.contains("2/2")
    })?;

    session.send(CTRL_P)?;
    session.paste("aliases/日本 語.rs")?;
    session.wait_for_screen("quick-open alias selected", ACTION_TIMEOUT, |screen| {
        screen.contains("Quick open: aliases/日本 語.rs") && screen.contains("src/日本 語.rs")
    })?;
    session.send(ENTER)?;
    session.wait_for_screen("quick-open alias deduplicated", ACTION_TIMEOUT, |screen| {
        screen.contains(OPENED)
            && screen.contains("already open src/日本 語.rs")
            && screen.contains("2/2")
            && !screen.contains("2/3")
    })?;

    session.send(CTRL_Q)?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(status.success(), "quick-open zec exit failed: {status}");
    session.assert_terminal_restored(&termios_before)?;
    Ok(())
}

fn failed_save_keeps_dirty_text_and_quit_guard(directory: &Path) -> Result<()> {
    const MARKER: &str = "UNSAVED-失敗";

    let blocker = directory.join("blocker");
    let impossible_path = blocker.join("child.txt");
    fs::write(&blocker, b"original blocker bytes\n")
        .context("write the non-directory save blocker")?;

    let pair = open_pty()?;
    let termios_before = capture_baseline(&pair)?;
    let mut session = PtySession::spawn(pair, &[directory.as_os_str()])?;
    session.wait_ready()?;
    session.assert_raw_mode_enabled(&termios_before)?;
    session.wait_for_screen(
        "restricted shared test worktree",
        ACTION_TIMEOUT,
        |screen| {
            screen.contains("Worktree Trust") && screen.contains(&directory.display().to_string())
        },
    )?;
    session.send(b"\x1b")?;
    session.wait_for_screen(
        "shared worktree prompt dismissed",
        ACTION_TIMEOUT,
        |screen| !screen.contains("Worktree Trust"),
    )?;
    session.send(CTRL_N)?;
    session.wait_for_screen("scratch tab", ACTION_TIMEOUT, |screen| {
        screen.contains("Untitled 1")
    })?;

    session.paste(MARKER)?;
    session.wait_for_screen("dirty scratch text", ACTION_TIMEOUT, |screen| {
        screen.contains(MARKER)
    })?;
    session.send(CTRL_S)?;
    session.wait_for_screen("Save As prompt", ACTION_TIMEOUT, |screen| {
        screen.contains("Save as:")
    })?;
    session.paste(&impossible_path.to_string_lossy())?;
    session.send(ENTER)?;
    session.wait_for_screen("deterministic save failure", ACTION_TIMEOUT, |screen| {
        screen.contains("save failed:") && screen.contains(MARKER)
    })?;

    ensure!(
        !impossible_path.exists(),
        "failed save unexpectedly created {}",
        impossible_path.display()
    );
    ensure!(
        fs::read(&blocker).context("read blocker after failed save")?
            == b"original blocker bytes\n",
        "failed save changed the blocker file"
    );

    session.send(CTRL_Q)?;
    session.wait_for_screen(
        "failed save remains dirty and guarded",
        ACTION_TIMEOUT,
        |screen| {
            screen.contains("unsaved or deleted tab(s); press Ctrl-Q again to discard")
                && screen.contains(MARKER)
        },
    )?;
    session.ensure_running("failed save must leave the scratch buffer dirty")?;
    session.send(CTRL_Q)?;

    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(status.success(), "discarded scratch exit failed: {status}");
    session.assert_terminal_restored(&termios_before)?;

    Ok(())
}

fn restricted_worktree_requires_confirmation_before_lsp(directory: &Path) -> Result<()> {
    let workspace = directory.join("trust-workspace");
    let root = workspace.join("project");
    let source_dir = root.join("src");
    let zed_dir = root.join(".zed");
    let bin_dir = workspace.join("bin");
    let home = workspace.join("home");
    let xdg_config = workspace.join("xdg-config");
    let xdg_data = workspace.join("xdg-data");
    let xdg_cache = workspace.join("xdg-cache");
    let xdg_state = workspace.join("xdg-state");
    let rustup_home = workspace.join("rustup");
    let cargo_home = workspace.join("cargo");
    for path in [
        &source_dir,
        &zed_dir,
        &bin_dir,
        &home,
        &xdg_config.join("zed"),
        &xdg_data,
        &xdg_cache,
        &xdg_state,
        &rustup_home,
        &cargo_home,
    ] {
        fs::create_dir_all(path).context("create trust fixture directory")?;
    }
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"pty-trust\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[workspace]\n",
    )
    .context("write trust fixture manifest")?;
    fs::write(source_dir.join("main.rs"), "fn main() { alpha_ }\n")
        .context("write trust fixture source")?;
    fs::write(
        zed_dir.join("settings.json"),
        r#"{
          "format_on_save": "off",
          "remove_trailing_whitespace_on_save": false,
          "ensure_final_newline_on_save": false
        }"#,
    )
    .context("write trust fixture project settings")?;
    fs::write(
        xdg_config.join("zed/settings.json"),
        r#"{
          "diagnostics": {
            "inline": {
              "enabled": true,
              "update_debounce_ms": 0,
              "padding": 2,
              "min_column": 30,
              "max_severity": "warning"
            }
          },
          "inlay_hints": {
            "enabled": false,
            "show_type_hints": true,
            "show_parameter_hints": true,
            "show_other_hints": true,
            "edit_debounce_ms": 0,
            "scroll_debounce_ms": 0
          }
        }"#,
    )
    .context("write restricted user settings")?;
    fs::write(xdg_config.join("zed/global_settings.json"), "{}")
        .context("write restricted global settings")?;
    fs::write(xdg_config.join("zed/keymap.json"), "[]").context("write restricted keymap")?;

    install_fixture_language_server(Path::new(env!("CARGO_BIN_EXE_fixture_lsp")), &bin_dir)
        .context("link PTY fixture language server")?;
    // Unix masks any real rustup with an always-failing stub; the Windows
    // restricted PATH simply omits it.
    #[cfg(unix)]
    {
        let fake_rustup = bin_dir.join("rustup");
        fs::write(&fake_rustup, "#!/bin/sh\nexit 1\n").context("write fake rustup")?;
        let mut permissions = fs::metadata(&fake_rustup)
            .context("read fake rustup metadata")?
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&fake_rustup, permissions).context("make fake rustup executable")?;
    }

    let log = workspace.join("lsp.jsonl");
    let path = restricted_path(&bin_dir)?;
    let pair = open_pty()?;
    let termios_before = capture_baseline(&pair)?;
    let environment = [
        (OsStr::new("PATH"), OsStr::new(&path)),
        (OsStr::new("HOME"), home.as_os_str()),
        (OsStr::new("XDG_CONFIG_HOME"), xdg_config.as_os_str()),
        (OsStr::new("XDG_DATA_HOME"), xdg_data.as_os_str()),
        (OsStr::new("XDG_CACHE_HOME"), xdg_cache.as_os_str()),
        (OsStr::new("XDG_STATE_HOME"), xdg_state.as_os_str()),
        (OsStr::new("RUSTUP_HOME"), rustup_home.as_os_str()),
        (OsStr::new("CARGO_HOME"), cargo_home.as_os_str()),
        (OsStr::new("ZEC_FIXTURE_LSP_LOG"), log.as_os_str()),
        (
            OsStr::new("ZEC_FIXTURE_LSP_SCENARIO"),
            OsStr::new("inlay-hints"),
        ),
        (OsStr::new("ZEC_KEYBOARD_PROTOCOL"), OsStr::new("kitty")),
    ];
    let source = source_dir.join("main.rs");
    let mut session =
        PtySession::spawn_with_env(pair, &[root.as_os_str(), source.as_os_str()], &environment)?;
    session.wait_for_screen("worktree trust confirmation", ACTION_TIMEOUT, |screen| {
        screen.contains("Worktree Trust")
            && screen.contains("Enter: trust for this session")
            && screen.contains(&root.display().to_string())
    })?;
    session.assert_raw_mode_enabled(&termios_before)?;
    ensure!(
        !log.exists() || fs::read(&log).context("read pre-trust LSP log")?.is_empty(),
        "language server started before worktree trust confirmation"
    );

    session.send(ENTER)?;
    session.wait_for_screen("trusted worktree status", ACTION_TIMEOUT, |screen| {
        screen.contains("worktree trusted for this session")
    })?;
    session.wait_until(
        "fixture LSP initialized after trust",
        ACTION_TIMEOUT,
        |_| {
            fs::read_to_string(&log).is_ok_and(|trace| trace.contains("\"method\":\"initialized\""))
        },
    )?;
    session.send(CTRL_PAGE_DOWN)?;
    session.wait_for_screen("Rust tab after trust", ACTION_TIMEOUT, |screen| {
        screen.contains("2/2 settings.json [main.rs]") && screen.contains("fn main() { alpha_ }")
    })?;
    session.wait_for_screen("fixture inline diagnostic", ACTION_TIMEOUT, |screen| {
        screen.contains("deterministic fixture warning")
    })?;
    session.send(F8)?;
    session.wait_for_screen("persistent diagnostics dock", ACTION_TIMEOUT, |screen| {
        screen.contains("Diagnostics")
            && screen.contains("W src/main.rs:1:4 deterministic fixture warning")
            && screen.contains("Diagnostics filter:")
    })?;
    session.paste("fixture")?;
    session.wait_for_screen("live diagnostics dock filter", ACTION_TIMEOUT, |screen| {
        screen.contains("Diagnostics filter: fixture")
            && screen.contains("deterministic fixture warning")
    })?;
    session.send(CTRL_U)?;
    session.send(b"\x1b")?;
    session.wait_for_screen(
        "diagnostics dock retained after returning to editor",
        ACTION_TIMEOUT,
        |screen| {
            screen.contains("editor focused; diagnostics dock remains open")
                && screen.contains("W src/main.rs:1:4 deterministic fixture warning")
        },
    )?;
    session.mouse_down(10, 20, 0)?;
    session.mouse_drag(10, 16, 0)?;
    session.mouse_up(10, 16, 0)?;
    session.wait_for_screen("mouse-resized diagnostics dock", ACTION_TIMEOUT, |screen| {
        screen.contains("resized bottom dock to 16 cells")
            && screen.contains("deterministic fixture warning")
    })?;
    let (diagnostic_column, diagnostic_row) = session
        .find_screen_text_between("deterministic fixture warning", 17, 31)
        .context("diagnostic dock row is not visible after resize")?;
    session.mouse_click(diagnostic_column, diagnostic_row, 0)?;
    session.mouse_click(diagnostic_column, diagnostic_row, 0)?;
    session.wait_for_screen("mouse diagnostic navigation", ACTION_TIMEOUT, |screen| {
        screen.contains("opened src/main.rs:1:4") && screen.contains("Diagnostics")
    })?;
    session.send(F8)?;
    session.wait_for_screen("diagnostics dock hidden", ACTION_TIMEOUT, |screen| {
        screen.contains("diagnostics dock hidden")
            && !screen.contains("W src/main.rs:1:4 deterministic fixture warning")
    })?;
    session.send(CTRL_COLON_KITTY)?;
    session.wait_until("fixture inlay hint request", ACTION_TIMEOUT, |_| {
        fs::read_to_string(&log)
            .is_ok_and(|trace| trace.contains("\"method\":\"textDocument/inlayHint\""))
    })?;
    session
        .wait_for_screen("fixture inlay hint projection", ACTION_TIMEOUT, |screen| {
            screen.contains("fixture_type") && screen.contains("inlay hints on")
        })
        .with_context(|| {
            format!(
                "fixture LSP trace:\n{}",
                fs::read_to_string(&log).unwrap_or_else(|error| format!("<unreadable: {error}>"))
            )
        })?;
    session.send(CTRL_COLON_KITTY)?;
    session.wait_for_screen("fixture inlay hints disabled", ACTION_TIMEOUT, |screen| {
        !screen.contains("fixture_type") && screen.contains("inlay hints off")
    })?;
    session.send(b"\x1b/")?;
    session.wait_for_screen(
        "completion after worktree trust",
        ACTION_TIMEOUT,
        |screen| screen.contains("Completions") && screen.contains("alpha_completion"),
    )?;
    session.send(b"\x1b")?;
    session.wait_for_screen("completion dismissed", ACTION_TIMEOUT, |screen| {
        !screen.contains("Completions")
    })?;
    session.send(CTRL_Q)?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(
        status.success(),
        "trusted worktree zec exit failed: {status}"
    );
    session.assert_terminal_restored_with_cleanup(&termios_before, KITTY_CLEANUP_ESCAPES)
}

#[cfg(unix)]
fn signal_exit_restores_terminal(directory: &Path, signal: Signal) -> Result<()> {
    let path = directory.join(format!("signal-{}.txt", signal as i32));
    fs::write(&path, "clean signal fixture\n").context("write signal fixture")?;

    let pair = open_pty()?;
    let termios_before = capture_baseline(&pair)?;
    let mut session = PtySession::spawn(pair, &[path.as_os_str()])?;
    session.wait_ready()?;
    session.assert_raw_mode_enabled(&termios_before)?;

    session.send_signal(signal)?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(
        status.success(),
        "{signal:?} did not use a clean exit: {status}"
    );
    session.assert_terminal_restored(&termios_before)?;

    Ok(())
}

#[cfg(unix)]
fn suspend_restores_and_resume_reenters_the_terminal(directory: &Path) -> Result<()> {
    const TOKEN: &str = "resumed-after-sigtstp";
    let path = directory.join("suspend-resume.txt");
    fs::write(&path, "before suspend\n").context("write suspend fixture")?;

    let pair = open_pty()?;
    let termios_before = capture_baseline(&pair)?;
    let mut session = PtySession::spawn(pair, &[path.as_os_str()])?;
    session.wait_ready()?;
    session.assert_raw_mode_enabled(&termios_before)?;

    session.send_signal(Signal::SIGTSTP)?;
    session.wait_for_stop(EXIT_TIMEOUT)?;
    session.assert_terminal_restored(&termios_before)?;

    session.send_signal(Signal::SIGCONT)?;
    session.wait_for_screen("frame after SIGCONT", ACTION_TIMEOUT, |screen| {
        screen.contains("resumed  |  zec")
    })?;
    session.assert_raw_mode_enabled(&termios_before)?;
    session.send(CTRL_A)?;
    session.paste(TOKEN)?;
    session.send(CTRL_S)?;
    session.wait_for_screen("save after SIGCONT", ACTION_TIMEOUT, |screen| {
        screen.contains("saved  |  zec")
    })?;
    let expected = format!("{TOKEN}\n");
    let saved = fs::read(&path).context("read suspend fixture after save")?;
    ensure!(
        saved == expected.as_bytes(),
        "editing after SIGCONT did not reach disk; expected {:?}, got {:?}",
        expected.as_bytes(),
        saved
    );
    session.send(CTRL_Q)?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(status.success(), "post-SIGCONT exit failed: {status}");
    session.assert_terminal_restored(&termios_before)
}

fn open_pty() -> Result<PtyPair> {
    native_pty_system()
        .openpty(INITIAL_SIZE)
        .context("open native PTY")
}

/// Pre-start snapshot of the outer terminal. Unix proves raw-mode entry and
/// restoration against termios; ConPTY exposes no equivalent, so Windows
/// relies on the emitted VT output instead.
struct TerminalBaseline {
    #[cfg(unix)]
    termios: Termios,
}

fn capture_baseline(pair: &PtyPair) -> Result<TerminalBaseline> {
    #[cfg(unix)]
    {
        Ok(TerminalBaseline {
            termios: capture_baseline(&pair)?,
        })
    }
    #[cfg(not(unix))]
    {
        let _ = pair;
        Ok(TerminalBaseline {})
    }
}

enum ReaderEvent {
    Bytes(Vec<u8>),
    Eof,
    Error(io::Error),
}

struct PtySession {
    master: Option<Box<dyn MasterPty + Send>>,
    child: Option<Box<dyn Child + Send + Sync>>,
    writer: Option<Box<dyn io::Write + Send>>,
    reader: Option<JoinHandle<()>>,
    events: Option<Receiver<ReaderEvent>>,
    reader_eof: bool,
    parser: Parser,
    output_generation: u64,
    transcript: Vec<u8>,
    _isolated_config: Option<tempfile::TempDir>,
}

impl PtySession {
    fn spawn(pair: PtyPair, arguments: &[&OsStr]) -> Result<Self> {
        Self::spawn_with_env(pair, arguments, &[])
    }

    fn spawn_with_env(
        pair: PtyPair,
        arguments: &[&OsStr],
        environment: &[(&OsStr, &OsStr)],
    ) -> Result<Self> {
        let PtyPair { slave, master } = pair;
        let mut reader = master.try_clone_reader().context("clone PTY reader")?;
        let writer = master.take_writer().context("take PTY writer")?;
        let (event_sender, events) = mpsc::sync_channel(64);
        let reader_thread = thread::Builder::new()
            .name("zec-pty-acceptance-reader".to_owned())
            .spawn(move || {
                let mut buffer = [0_u8; 8192];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => {
                            let _ = event_sender.send(ReaderEvent::Eof);
                            break;
                        }
                        Ok(count) => {
                            if event_sender
                                .send(ReaderEvent::Bytes(buffer[..count].to_vec()))
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
                            let _ = event_sender.send(ReaderEvent::Eof);
                            break;
                        }
                        Err(error) => {
                            let _ = event_sender.send(ReaderEvent::Error(error));
                            break;
                        }
                    }
                }
            })
            .context("spawn PTY reader")?;

        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_zec"));
        command.args(arguments);
        command.env("TERM", "xterm-256color");
        command.env("LANG", "C.UTF-8");
        command.env("LC_ALL", "C.UTF-8");
        command.env("ZEC_DISABLE_SESSIONS", "1");
        command.env("ZEC_KEYBOARD_PROTOCOL", "modifyOtherKeys");
        let directory = tempfile::tempdir().context("create isolated PTY user-data home")?;
        if !environment
            .iter()
            .any(|(name, _)| *name == OsStr::new("XDG_CONFIG_HOME"))
        {
            let zed = directory.path().join("zed");
            fs::create_dir_all(&zed).context("create isolated PTY Zed config directory")?;
            fs::write(zed.join("settings.json"), r#"{"show_whitespaces":"none"}"#)
                .context("write isolated PTY settings")?;
            fs::write(
                zed.join("global_settings.json"),
                r#"{"show_whitespaces":"none"}"#,
            )
            .context("write isolated PTY global settings")?;
            fs::write(zed.join("keymap.json"), "[]").context("write isolated PTY keymap")?;
            command.env("XDG_CONFIG_HOME", directory.path());
        }
        if !environment
            .iter()
            .any(|(name, _)| *name == OsStr::new("XDG_DATA_HOME"))
        {
            command.env("XDG_DATA_HOME", directory.path().join("data"));
        }
        let isolated_config = Some(directory);
        for (name, value) in environment {
            command.env(name, value);
        }
        let child = slave
            .spawn_command(command)
            .context("spawn actual zec binary")?;
        // The parent must not keep the slave open: doing so suppresses EOF after
        // the child exits and can make failure cleanup hang forever.
        drop(slave);

        Ok(Self {
            master: Some(master),
            child: Some(child),
            writer: Some(writer),
            reader: Some(reader_thread),
            events: Some(events),
            reader_eof: false,
            parser: Parser::new(INITIAL_SIZE.rows, INITIAL_SIZE.cols, 0),
            output_generation: 0,
            transcript: Vec::new(),
            _isolated_config: isolated_config,
        })
    }

    fn wait_ready(&mut self) -> Result<()> {
        self.wait_until("zec initial frame", STARTUP_TIMEOUT, |session| {
            let screen = session.parser.screen();
            // ConPTY flattens alternate-screen, bracketed-paste, and
            // mouse negotiation; only the repaint is observable there.
            if cfg!(unix) {
                contains_bytes(&session.transcript, b"Ctrl-N new")
                    && screen.alternate_screen()
                    && screen.bracketed_paste()
                    && screen.mouse_protocol_mode() != MouseProtocolMode::None
            } else {
                contains_bytes(&session.transcript, b"F1 commands")
                    && contains_bytes(&session.transcript, b"\x1b[2J")
            }
        })
    }

    fn assert_raw_mode_enabled(&self, initial: &TerminalBaseline) -> Result<()> {
        #[cfg(unix)]
        {
            let current = self.termios()?;
            ensure!(
                &current != &initial.termios,
                "zec rendered its UI without enabling terminal raw mode"
            );
            ensure!(
                !current
                    .local_flags
                    .contains(LocalFlags::ICANON | LocalFlags::ECHO),
                "zec left canonical input or terminal echo enabled in raw mode"
            );
        }
        #[cfg(not(unix))]
        {
            let _ = initial;
            ensure!(
                contains_bytes(&self.transcript, b"\x1b[2J"),
                "zec did not repaint the ConPTY screen"
            );
        }
        Ok(())
    }

    fn send(&mut self, bytes: &[u8]) -> Result<()> {
        let writer = self.writer.as_mut().context("PTY writer is closed")?;
        writer.write_all(bytes).context("write PTY input")?;
        writer.flush().context("flush PTY input")
    }

    fn paste(&mut self, text: &str) -> Result<()> {
        #[cfg(unix)]
        {
            self.send(b"\x1b[200~")?;
            self.send(text.as_bytes())?;
            self.send(b"\x1b[201~")
        }
        #[cfg(windows)]
        {
            // ConPTY has no bracketed paste: conhost cooks raw pipe input,
            // turning LF into Ctrl-Enter and dropping non-BMP characters.
            // conhost requests win32-input-mode at startup, and encoding
            // the text as win32 key events - the same thing Windows
            // Terminal does - carries every character losslessly.
            self.send_text_as_win32_input(text)
        }
    }

    /// Sends text the way it survives conhost's input cooking: BMP
    /// characters as plain bytes (LF normalized to CR, which cooks into a
    /// plain Enter), and astral characters as win32-input-mode key-down
    /// events, whose surrogate pairs the cooked path would drop.
    #[cfg(windows)]
    fn send_text_as_win32_input(&mut self, text: &str) -> Result<()> {
        let normalized = text.replace("\r\n", "\r").replace('\n', "\r");
        let mut encoded = String::new();
        for character in normalized.chars() {
            if u32::from(character) > 0xFFFF {
                let mut units = [0_u16; 2];
                for unit in character.encode_utf16(&mut units) {
                    encoded.push_str(&format!("\x1b[0;0;{unit};1;0;1_"));
                }
            } else {
                encoded.push(character);
            }
        }
        self.send(encoded.as_bytes())
    }

    fn find_screen_text(&self, needle: &str) -> Option<(u16, u16)> {
        let (rows, columns) = self.parser.screen().size();
        self.parser
            .screen()
            .rows(0, columns)
            .take(usize::from(rows))
            .enumerate()
            .find_map(|(row, text)| {
                let column = text.find(needle)?;
                Some((u16::try_from(column).ok()?, u16::try_from(row).ok()?))
            })
    }

    fn find_screen_text_between(
        &self,
        needle: &str,
        start_row: u16,
        end_row: u16,
    ) -> Option<(u16, u16)> {
        let (_, columns) = self.parser.screen().size();
        self.parser
            .screen()
            .rows(0, columns)
            .enumerate()
            .skip(usize::from(start_row))
            .take(usize::from(end_row.saturating_sub(start_row)))
            .find_map(|(row, text)| {
                let column = text.find(needle)?;
                Some((u16::try_from(column).ok()?, u16::try_from(row).ok()?))
            })
    }

    fn sgr_mouse(&mut self, button_code: u8, column: u16, row: u16, released: bool) -> Result<()> {
        let terminator = if released { 'm' } else { 'M' };
        let sequence = format!(
            "\x1b[<{};{};{}{}",
            button_code,
            column.saturating_add(1),
            row.saturating_add(1),
            terminator
        );
        self.send(sequence.as_bytes())
    }

    fn mouse_down(&mut self, column: u16, row: u16, modifier_code: u8) -> Result<()> {
        self.sgr_mouse(modifier_code, column, row, false)
    }

    fn mouse_drag(&mut self, column: u16, row: u16, modifier_code: u8) -> Result<()> {
        self.sgr_mouse(32 | modifier_code, column, row, false)
    }

    fn mouse_up(&mut self, column: u16, row: u16, modifier_code: u8) -> Result<()> {
        self.sgr_mouse(modifier_code, column, row, true)
    }

    fn mouse_click(&mut self, column: u16, row: u16, modifier_code: u8) -> Result<()> {
        self.mouse_down(column, row, modifier_code)?;
        self.mouse_up(column, row, modifier_code)
    }

    fn resize(&mut self, size: PtySize) -> Result<()> {
        let previous_generation = self.output_generation;
        self.resize_immediately(size)?;
        self.wait_until("child redraw after PTY resize", ACTION_TIMEOUT, |session| {
            session.output_generation > previous_generation
                && session.parser.screen().size() == (size.rows, size.cols)
                && session
                    .parser
                    .screen()
                    .rows(0, size.cols)
                    .nth(usize::from(size.rows.saturating_sub(1)))
                    .is_some_and(|row| row.contains("zec "))
        })
    }

    fn resize_immediately(&mut self, size: PtySize) -> Result<()> {
        self.master
            .as_ref()
            .context("PTY master is closed")?
            .resize(size)
            .context("resize PTY")?;
        self.parser.screen_mut().set_size(size.rows, size.cols);
        Ok(())
    }

    #[cfg(unix)]
    fn send_signal(&self, signal: Signal) -> Result<()> {
        let pid = self
            .child
            .as_ref()
            .and_then(|child| child.process_id())
            .context("zec child has no process id")?;
        let pid = i32::try_from(pid).context("zec pid does not fit pid_t")?;
        killpg(Pid::from_raw(pid), signal)
            .with_context(|| format!("send {signal:?} to zec process group {pid}"))
    }

    #[cfg(unix)]
    fn wait_for_stop(&mut self, timeout: Duration) -> Result<()> {
        let pid = self
            .child
            .as_ref()
            .and_then(|child| child.process_id())
            .context("zec child has no process id")?;
        let pid = Pid::from_raw(i32::try_from(pid).context("zec pid does not fit pid_t")?);
        let deadline = Instant::now() + timeout;
        loop {
            self.drain_available()?;
            match waitpid(pid, Some(WaitPidFlag::WNOHANG | WaitPidFlag::WUNTRACED))
                .context("wait for zec to stop")?
            {
                WaitStatus::Stopped(_, Signal::SIGSTOP | Signal::SIGTSTP) => return Ok(()),
                WaitStatus::StillAlive | WaitStatus::Continued(_) => {}
                status => bail!(
                    "zec exited instead of stopping: {status:?}\n{}",
                    self.diagnostic()
                ),
            }
            let now = Instant::now();
            if now >= deadline {
                bail!("timed out waiting for zec to stop\n{}", self.diagnostic());
            }
            self.receive_one((deadline - now).min(EVENT_POLL))?;
        }
    }

    fn ensure_running(&mut self, message: &str) -> Result<()> {
        let child = self
            .child
            .as_mut()
            .context("zec child was already reaped")?;
        if let Some(status) = child.try_wait().context("poll zec child")? {
            self.child.take();
            bail!("{message}; child exited as {status}\n{}", self.diagnostic());
        }
        Ok(())
    }

    fn wait_for_screen(
        &mut self,
        description: &str,
        timeout: Duration,
        predicate: impl Fn(&str) -> bool,
    ) -> Result<()> {
        self.wait_until(description, timeout, |session| {
            predicate(&session.parser.screen().contents())
        })
    }

    fn wait_for_raw(&mut self, description: &str, needle: &[u8], timeout: Duration) -> Result<()> {
        self.wait_until(description, timeout, |session| {
            contains_bytes(&session.transcript, needle)
        })
    }

    fn wait_until(
        &mut self,
        description: &str,
        timeout: Duration,
        predicate: impl Fn(&Self) -> bool,
    ) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            self.drain_available()?;
            if predicate(self) {
                return Ok(());
            }

            if let Some(child) = self.child.as_mut()
                && let Some(status) = child.try_wait().context("poll zec while waiting")?
            {
                self.child.take();
                bail!(
                    "zec exited as {status} before {description}\n{}",
                    self.diagnostic()
                );
            }
            if self.reader_eof {
                bail!(
                    "PTY reached EOF before {description}\n{}",
                    self.diagnostic()
                );
            }

            let now = Instant::now();
            if now >= deadline {
                bail!("timed out waiting for {description}\n{}", self.diagnostic());
            }
            self.receive_one((deadline - now).min(EVENT_POLL))?;
        }
    }

    fn wait_for_exit(&mut self, timeout: Duration) -> Result<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            self.drain_available()?;
            let status = self
                .child
                .as_mut()
                .context("zec child was already reaped")?
                .try_wait()
                .context("poll zec exit")?;
            if let Some(status) = status {
                self.child.take();
                return Ok(status);
            }

            let now = Instant::now();
            if now >= deadline {
                bail!("timed out waiting for zec exit\n{}", self.diagnostic());
            }
            self.receive_one((deadline - now).min(EVENT_POLL))?;
        }
    }

    fn assert_terminal_restored(&mut self, initial: &TerminalBaseline) -> Result<()> {
        self.assert_terminal_restored_with_cleanup(initial, CLEANUP_ESCAPES)
    }

    fn assert_terminal_restored_with_cleanup(
        &mut self,
        initial: &TerminalBaseline,
        cleanup_escapes: &[u8],
    ) -> Result<()> {
        // ConPTY absorbs the client's cleanup escapes and synthesizes its
        // own teardown, so the exact sequence and termios are Unix-only
        // evidence.
        #[cfg(unix)]
        {
            self.wait_for_raw(
                "ordered terminal cleanup escapes",
                cleanup_escapes,
                EXIT_TIMEOUT,
            )?;
            self.drain_available()?;
            let current = self.termios()?;
            ensure!(
                &current == &initial.termios,
                "stty state differs after zec exit\n{}",
                self.diagnostic()
            );
        }
        #[cfg(not(unix))]
        {
            let _ = (initial, cleanup_escapes);
            let deadline = Instant::now() + EXIT_TIMEOUT;
            loop {
                self.drain_available()?;
                if !self.parser.screen().hide_cursor() {
                    break;
                }
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                self.receive_one((deadline - now).min(EVENT_POLL))?;
            }
        }

        let screen = self.parser.screen();
        ensure!(
            !screen.alternate_screen(),
            "alternate screen remained active"
        );
        ensure!(!screen.hide_cursor(), "terminal cursor remained hidden");
        ensure!(!screen.bracketed_paste(), "bracketed paste remained active");
        ensure!(
            !screen.application_keypad(),
            "application keypad remained active"
        );
        ensure!(
            !screen.application_cursor(),
            "application cursor remained active"
        );
        ensure!(
            screen.mouse_protocol_mode() == MouseProtocolMode::None,
            "mouse capture remained active: {:?}",
            screen.mouse_protocol_mode()
        );
        ensure!(
            screen.mouse_protocol_encoding() == MouseProtocolEncoding::Default,
            "mouse protocol encoding was not reset: {:?}",
            screen.mouse_protocol_encoding()
        );
        Ok(())
    }

    #[cfg(unix)]
    fn termios(&self) -> Result<Termios> {
        self.master
            .as_ref()
            .context("PTY master is closed")?
            .get_termios()
            .context("PTY no longer exposes termios")
    }

    fn drain_available(&mut self) -> Result<()> {
        loop {
            let event = match self
                .events
                .as_ref()
                .context("PTY event stream is closed")?
                .try_recv()
            {
                Ok(event) => event,
                Err(TryRecvError::Empty) => return Ok(()),
                Err(TryRecvError::Disconnected) if !self.reader_eof => {
                    bail!("PTY reader disconnected\n{}", self.diagnostic())
                }
                Err(TryRecvError::Disconnected) => return Ok(()),
            };
            self.consume(event)?;
        }
    }

    fn receive_one(&mut self, timeout: Duration) -> Result<()> {
        let event = match self
            .events
            .as_ref()
            .context("PTY event stream is closed")?
            .recv_timeout(timeout)
        {
            Ok(event) => event,
            Err(RecvTimeoutError::Timeout) => return Ok(()),
            Err(RecvTimeoutError::Disconnected) if !self.reader_eof => {
                bail!("PTY reader disconnected\n{}", self.diagnostic())
            }
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        };
        self.consume(event)
    }

    fn consume(&mut self, event: ReaderEvent) -> Result<()> {
        match event {
            ReaderEvent::Bytes(bytes) => {
                self.parser.process(&bytes);
                self.output_generation = self.output_generation.wrapping_add(1);
                append_bounded(&mut self.transcript, &bytes);
                // A ConPTY host must answer conhost's startup cursor-
                // position report or client console I/O stays deferred.
                if contains_bytes(&bytes, b"\x1b[6n") {
                    let (row, column) = self.parser.screen().cursor_position();
                    let reply = format!("\x1b[{};{}R", row + 1, column + 1);
                    if let Some(writer) = self.writer.as_mut() {
                        let _ = writer
                            .write_all(reply.as_bytes())
                            .and_then(|_| writer.flush());
                    }
                }
                Ok(())
            }
            ReaderEvent::Eof => {
                self.reader_eof = true;
                Ok(())
            }
            ReaderEvent::Error(error) => Err(error).context("read actual zec PTY output"),
        }
    }

    fn diagnostic(&self) -> String {
        let tail_start = self.transcript.len().saturating_sub(DIAGNOSTIC_TAIL);
        format!(
            "screen:\n{}\nraw tail:\n{:?}",
            self.parser.screen().contents(),
            String::from_utf8_lossy(&self.transcript[tail_start..])
        )
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.child.take();

        // UnixMasterWriter sends VEOF when dropped. The slave must then be closed
        // before joining the blocking reader so every panic/timeout is leak-free.
        self.writer.take();
        self.master.take();
        self.events.take();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn append_bounded(transcript: &mut Vec<u8>, bytes: &[u8]) {
    if bytes.len() >= TRANSCRIPT_LIMIT {
        transcript.clear();
        transcript.extend_from_slice(&bytes[bytes.len() - TRANSCRIPT_LIMIT..]);
        return;
    }

    let overflow = transcript
        .len()
        .saturating_add(bytes.len())
        .saturating_sub(TRANSCRIPT_LIMIT);
    if overflow > 0 {
        transcript.drain(..overflow);
    }
    transcript.extend_from_slice(bytes);
}

/// PATH limited to the fixture bin directory plus what processes need to
/// start at all.
fn restricted_path(bin_dir: &Path) -> Result<OsString> {
    #[cfg(unix)]
    {
        Ok(OsString::from(format!(
            "{}:/usr/local/bin:/usr/bin:/bin",
            bin_dir.display()
        )))
    }
    #[cfg(windows)]
    {
        let mut entries = vec![bin_dir.to_path_buf()];
        if let Some(system_root) = std::env::var_os("SystemRoot").map(PathBuf::from) {
            entries.push(system_root.join("System32"));
            entries.push(system_root);
        }
        std::env::join_paths(entries).context("join restricted PATH entries")
    }
}

fn install_fixture_language_server(server: &Path, bin_dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        symlink(server, bin_dir.join("rust-analyzer")).context("link fixture language server")
    }
    #[cfg(windows)]
    {
        let target = bin_dir.join("rust-analyzer.exe");
        if fs::hard_link(server, &target).is_err() {
            fs::copy(server, &target).context("copy fixture language server")?;
        }
        Ok(())
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty()
        || haystack
            .windows(needle.len())
            .any(|window| window == needle)
}
