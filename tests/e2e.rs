//! The actual binary through a PTY: terminal lifecycle, editing, saving,
//! tabs, and external changes.

use std::{
    ffi::OsStr,
    fs,
    io::{self, Read as _, Write as _},
    path::{Path, PathBuf},
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
const ACTION_TIMEOUT: Duration = Duration::from_secs(15);
const EXIT_TIMEOUT: Duration = Duration::from_secs(15);
const EVENT_POLL: Duration = Duration::from_millis(100);
const TRANSCRIPT_LIMIT: usize = 256 * 1024;
const DIAGNOSTIC_TAIL: usize = 8 * 1024;

const CTRL_A: &[u8] = b"\x01";
const CTRL_N: &[u8] = b"\x0e";
const CTRL_O: &[u8] = b"\x0f";
const CTRL_P: &[u8] = b"\x10";
const CTRL_Q: &[u8] = b"\x11";
const CTRL_R: &[u8] = b"\x12";
const CTRL_S: &[u8] = b"\x13";
const CTRL_W: &[u8] = b"\x17";
const CTRL_Z: &[u8] = b"\x1a";
const CTRL_PAGE_DOWN: &[u8] = b"\x1b[6;5~";
const F10: &[u8] = b"\x1b[21~";
const SHIFT_F10: &[u8] = b"\x1b[21;2~";
const CTRL_ALT_LEFT: &[u8] = b"\x1b[1;7D";
const CTRL_ALT_RIGHT: &[u8] = b"\x1b[1;7C";
const CTRL_ALT_SHIFT_LEFT: &[u8] = b"\x1b[1;8D";
const CTRL_ALT_SHIFT_UP: &[u8] = b"\x1b[1;8A";
const CTRL_ALT_EQUALS: &[u8] = b"\x1b[61;7u";
const DOWN: &[u8] = b"\x1b[B";
const F2: &[u8] = b"\x1bOQ";
const F7: &[u8] = b"\x1b[18~";
const F9: &[u8] = b"\x1b[20~";
const F6: &[u8] = b"\x1b[17~";
const F8: &[u8] = b"\x1b[19~";
const SHIFT_F12: &[u8] = b"\x1b[24;2~";
const ALT_SLASH: &[u8] = b"\x1b/";
const CTRL_PERIOD: &[u8] = b"\x1b[46;5u";
const CTRL_SHIFT_T: &[u8] = b"\x1b[116;6u";
const CTRL_PAGE_UP: &[u8] = b"\x1b[5;5~";
const END: &[u8] = b"\x1b[F";
const DELETE: &[u8] = b"\x1b[3~";
const CTRL_U: &[u8] = b"\x15";
const ALT_F: &[u8] = b"\x1bf";
const CTRL_F: &[u8] = b"\x06";
const CTRL_G: &[u8] = b"\x07";
// CSI u forms: a legacy Ctrl-H is indistinguishable from Backspace.
const CTRL_H: &[u8] = b"\x1b[104;5u";
const SHIFT_ENTER: &[u8] = b"\x1b[13;2u";
const F1: &[u8] = b"\x1bOP";
const F4: &[u8] = b"\x1bOS";
const ESC: &[u8] = b"\x1b";
const ENTER: &[u8] = b"\r";

// This is one execute! call in the terminal session's restore. Keeping the
// full ordered sequence here catches a regression where only some terminal
// features are reset.
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

#[test]
fn edits_save_and_every_exit_path_restores_the_terminal() -> Result<()> {
    let temp = tempfile::tempdir().context("create PTY fixture")?;
    edit_undo_resize_save_and_quit(temp.path())?;
    scratch_save_as_and_failed_save_keep_the_buffer_dirty(temp.path())?;
    #[cfg(unix)]
    {
        signal_exit_restores_terminal(temp.path(), Signal::SIGINT)?;
        signal_exit_restores_terminal(temp.path(), Signal::SIGQUIT)?;
        signal_exit_restores_terminal(temp.path(), Signal::SIGTERM)?;
        signal_exit_restores_terminal(temp.path(), Signal::SIGHUP)?;
        suspend_restores_and_resume_reenters_the_terminal(temp.path())?;
    }
    Ok(())
}

#[test]
fn quick_open_matches_files_under_the_root() -> Result<()> {
    const READY: &str = "E2E_QUICK_OPEN_README";
    const OPENED: &str = "E2E_QUICK_OPEN_BODY";
    let temp = tempfile::tempdir().context("create PTY fixture")?;
    let root = temp.path().join("quick-open-repo");
    fs::create_dir_all(root.join("src"))?;
    fs::write(root.join("README.md"), format!("{READY}\n"))?;
    fs::write(root.join("src/日本 語.rs"), format!("{OPENED}\n"))?;
    let target = format!("src{}日本 語.rs", std::path::MAIN_SEPARATOR);

    let pair = open_pty()?;
    let baseline = capture_baseline(&pair)?;
    let mut session = PtySession::spawn(pair, &[root.as_os_str()])?;
    session.wait_ready()?;
    session.assert_raw_mode_enabled(&baseline)?;

    session.send(CTRL_P)?;
    session.wait_for_screen("quick open lists the worktree", ACTION_TIMEOUT, |screen| {
        screen.contains("Quick open:") && screen.contains("README.md") && screen.contains(&target)
    })?;
    session.paste("日本")?;
    session.wait_for_screen(
        "quick open filters by fuzzy match",
        ACTION_TIMEOUT,
        |screen| {
            screen.contains("Quick open: 日本")
                && screen.contains(&target)
                && !screen.contains("README.md")
        },
    )?;
    session.send(ENTER)?;
    session.wait_for_screen("quick open opens the match", ACTION_TIMEOUT, |screen| {
        screen.contains(OPENED)
            && screen.contains(&format!("opened {target}"))
            && screen.contains("2/2")
    })?;

    session.send(CTRL_Q)?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(status.success(), "quick open exit failed: {status}");
    session.assert_terminal_restored(&baseline)
}

#[test]
fn project_search_lists_hits_and_opens_the_selected_location() -> Result<()> {
    let temp = tempfile::tempdir().context("create PTY fixture")?;
    let root = temp.path().join("search-repo");
    fs::create_dir_all(root.join("src"))?;
    fs::write(root.join("a.txt"), "needle in a\n")?;
    fs::write(root.join("src/b.txt"), "hay\nhay needle in b\n")?;
    let second = format!("src{}b.txt", std::path::MAIN_SEPARATOR);

    let pair = open_pty()?;
    let baseline = capture_baseline(&pair)?;
    let mut session = PtySession::spawn(pair, &[root.as_os_str()])?;
    session.wait_ready()?;
    session.assert_raw_mode_enabled(&baseline)?;

    session.send(ALT_F)?;
    session.wait_for_screen("project search prompt", ACTION_TIMEOUT, |screen| {
        screen.contains("Project search:")
    })?;
    session.paste("needle")?;
    session.send(ENTER)?;
    session.wait_for_screen("project search hits by path", ACTION_TIMEOUT, |screen| {
        screen.contains("Matches:")
            && screen.contains("a.txt:1  needle in a")
            && screen.contains(&format!("{second}:2  hay needle in b"))
    })?;
    session.send(DOWN)?;
    session.send(ENTER)?;
    session.wait_for_screen("hit opened in a new tab", ACTION_TIMEOUT, |screen| {
        screen.contains(&format!("opened {second}")) && screen.contains("2/2")
    })?;
    session.paste("X")?;
    session.wait_for_screen("caret placed on the hit", ACTION_TIMEOUT, |screen| {
        screen.contains("hay Xneedle in b")
    })?;

    session.send(CTRL_Q)?;
    session.wait_for_screen("dirty quit guard", ACTION_TIMEOUT, |screen| {
        screen.contains("unsaved or deleted tab(s)")
    })?;
    session.send(CTRL_Q)?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(status.success(), "project search exit failed: {status}");
    session.assert_terminal_restored(&baseline)
}

#[test]
fn find_replace_and_go_to_line_drive_the_active_buffer() -> Result<()> {
    let temp = tempfile::tempdir().context("create PTY fixture")?;
    let path = temp.path().join("search.txt");
    fs::write(&path, "alpha beta\nbeta gamma\ngamma beta\n")?;

    let pair = open_pty()?;
    let baseline = capture_baseline(&pair)?;
    let mut session = PtySession::spawn(pair, &[path.as_os_str()])?;
    session.wait_ready()?;
    session.assert_raw_mode_enabled(&baseline)?;

    session.send(CTRL_F)?;
    session.wait_for_screen("find prompt", ACTION_TIMEOUT, |screen| {
        screen.contains("Find:")
    })?;
    session.paste("beta")?;
    session.wait_for_screen("incremental match count", ACTION_TIMEOUT, |screen| {
        screen.contains("Find: beta") && screen.contains("1 of 3")
    })?;
    session.send(ENTER)?;
    session.wait_for_screen("next match", ACTION_TIMEOUT, |screen| {
        screen.contains("2 of 3")
    })?;
    session.send(SHIFT_ENTER)?;
    session.wait_for_screen("previous match", ACTION_TIMEOUT, |screen| {
        screen.contains("1 of 3")
    })?;

    session.send(CTRL_H)?;
    session.wait_for_screen(
        "replace prompt keeps the matches",
        ACTION_TIMEOUT,
        |screen| screen.contains("Replace with:") && screen.contains("1 of 3"),
    )?;
    session.paste("delta")?;
    session.send(ENTER)?;
    session.wait_for_screen("one replacement", ACTION_TIMEOUT, |screen| {
        screen.contains("alpha delta") && screen.contains("replaced 1; 1 of 2")
    })?;
    session.send(SHIFT_ENTER)?;
    session.wait_for_screen("replace all", ACTION_TIMEOUT, |screen| {
        screen.contains("delta gamma")
            && screen.contains("gamma delta")
            && screen.contains("replaced 2; no matches")
    })?;
    session.send(ESC)?;
    session.wait_for_screen("replace prompt closed", ACTION_TIMEOUT, |screen| {
        !screen.contains("Replace with:") && screen.contains("search.txt+")
    })?;

    session.send(CTRL_G)?;
    session.wait_for_screen("go to line prompt", ACTION_TIMEOUT, |screen| {
        screen.contains("Go to line:")
    })?;
    session.paste("2:3")?;
    session.send(ENTER)?;
    session.wait_for_screen("caret moved", ACTION_TIMEOUT, |screen| {
        screen.contains("line 2") && !screen.contains("Go to line:")
    })?;
    session.paste("X")?;
    session.wait_for_screen("edit at line 2 column 3", ACTION_TIMEOUT, |screen| {
        screen.contains("deXlta gamma")
    })?;

    session.send(CTRL_Q)?;
    session.wait_for_screen("dirty quit guard", ACTION_TIMEOUT, |screen| {
        screen.contains("unsaved or deleted tab(s)")
    })?;
    session.send(CTRL_Q)?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(status.success(), "exit after search failed: {status}");
    session.assert_terminal_restored(&baseline)
}

#[test]
fn splits_focus_move_and_collapse_panes() -> Result<()> {
    let temp = tempfile::tempdir().context("create PTY fixture")?;
    let first = temp.path().join("first.txt");
    let second = temp.path().join("second.txt");
    fs::write(&first, "first file\n")?;
    fs::write(&second, "second file\n")?;

    let pair = open_pty()?;
    let baseline = capture_baseline(&pair)?;
    let mut session = PtySession::spawn(pair, &[first.as_os_str()])?;
    session.wait_ready()?;

    session.send(F10)?;
    session.wait_for_screen("split right", ACTION_TIMEOUT, |screen| {
        screen.contains("split right")
            && screen.matches("first file").count() == 2
            && screen.contains('│')
    })?;

    // The new pane is focused: a file opened now lands there.
    session.send(CTRL_O)?;
    session.wait_for_screen("open prompt", ACTION_TIMEOUT, |screen| {
        screen.contains("Open:")
    })?;
    session.paste(&second.to_string_lossy())?;
    session.send(ENTER)?;
    session.wait_for_screen("second file in the right pane", ACTION_TIMEOUT, |screen| {
        screen.contains("second file")
            && screen.contains("[second.txt]")
            && screen.contains("first file")
    })?;

    session.send(CTRL_ALT_LEFT)?;
    session.wait_for_screen("focus left", ACTION_TIMEOUT, |screen| {
        screen.contains("focused pane to the left")
    })?;
    session.send(CTRL_ALT_RIGHT)?;
    session.wait_for_screen("focus right", ACTION_TIMEOUT, |screen| {
        screen.contains("focused pane to the right")
    })?;

    // The moved tab joins the left pane; the right pane keeps its copy of
    // the first file.
    session.send(CTRL_ALT_SHIFT_LEFT)?;
    session.wait_for_screen("tab moved left", ACTION_TIMEOUT, |screen| {
        screen.contains("moved tab to the left")
            && screen.contains('│')
            && screen.contains("2/2")
            && screen.contains("[second.txt]")
    })?;

    // Closing the right pane's last tab collapses the split.
    session.send(CTRL_ALT_RIGHT)?;
    session.wait_for_screen("focus right again", ACTION_TIMEOUT, |screen| {
        screen.contains("focused pane to the right")
    })?;
    session.send(CTRL_W)?;
    session.wait_for_screen("pane closed", ACTION_TIMEOUT, |screen| {
        screen.contains("tab closed") && !screen.contains('│') && screen.contains("2/2")
    })?;

    session.send(SHIFT_F10)?;
    session.wait_for_screen("split down", ACTION_TIMEOUT, |screen| {
        screen.contains("split down") && screen.contains('─')
    })?;
    session.send(CTRL_ALT_EQUALS)?;
    session.wait_for_screen("grown pane", ACTION_TIMEOUT, |screen| {
        screen.contains("pane size 55%")
    })?;

    // Moving the lower pane's only tab up collapses that split.
    session.send(CTRL_ALT_SHIFT_UP)?;
    session.wait_for_screen(
        "tab moved up and split collapsed",
        ACTION_TIMEOUT,
        |screen| {
            screen.contains("moved tab above") && !screen.contains('─') && screen.contains("3/3")
        },
    )?;
    session.send(CTRL_W)?;
    session.wait_for_screen("copy closed", ACTION_TIMEOUT, |screen| {
        screen.contains("tab closed") && screen.contains("2/2")
    })?;

    session.send(CTRL_Q)?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(status.success(), "split session exit failed: {status}");
    session.assert_terminal_restored(&baseline)
}

#[test]
fn sessions_restore_split_layout_and_tabs() -> Result<()> {
    let temp = tempfile::tempdir().context("create PTY fixture")?;
    let root = temp.path().join("session-repo");
    fs::create_dir_all(&root)?;
    fs::write(root.join("alpha.txt"), "alpha file\n")?;
    fs::write(root.join("beta.txt"), "beta file\n")?;
    let data_dir = tempfile::tempdir().context("create shared data directory")?;

    let pair = open_pty()?;
    let baseline = capture_baseline(&pair)?;
    let mut session =
        PtySession::spawn_with_data_dir(pair, &[root.as_os_str()], data_dir.path(), None)?;
    session.wait_ready()?;

    session.send(CTRL_P)?;
    session.wait_for_screen("quick open", ACTION_TIMEOUT, |screen| {
        screen.contains("Quick open:") && screen.contains("alpha.txt")
    })?;
    session.paste("alpha")?;
    session.wait_for_screen("quick open filtered to alpha", ACTION_TIMEOUT, |screen| {
        screen.contains("Quick open: alpha") && screen.contains("› alpha.txt")
    })?;
    session.send(ENTER)?;
    session.wait_for_screen("alpha opened", ACTION_TIMEOUT, |screen| {
        screen.contains("alpha file") && screen.contains("opened alpha.txt")
    })?;
    session.send(F10)?;
    session.wait_for_screen("split right", ACTION_TIMEOUT, |screen| {
        screen.contains("split right")
    })?;
    session.send(CTRL_P)?;
    session.wait_for_screen("quick open again", ACTION_TIMEOUT, |screen| {
        screen.contains("Quick open:")
    })?;
    session.paste("beta")?;
    session.wait_for_screen("quick open filtered to beta", ACTION_TIMEOUT, |screen| {
        screen.contains("Quick open: beta") && screen.contains("› beta.txt")
    })?;
    session.send(ENTER)?;
    session.wait_for_screen("beta opened in the right pane", ACTION_TIMEOUT, |screen| {
        screen.contains("beta file")
            && screen.contains("[beta.txt]")
            && screen.contains("alpha file")
    })?;

    session.send(CTRL_Q)?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(status.success(), "first session exit failed: {status}");
    session.assert_terminal_restored(&baseline)?;
    let sessions = data_dir.path().join("zec-sessions");
    ensure!(
        fs::read_dir(&sessions)?
            .filter_map(Result::ok)
            .any(|entry| entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "json")),
        "no session file under {}",
        sessions.display()
    );

    let pair = open_pty()?;
    let baseline = capture_baseline(&pair)?;
    let mut session =
        PtySession::spawn_with_data_dir(pair, &[root.as_os_str()], data_dir.path(), None)?;
    // The restored split halves the status row, so the ready hint may be
    // cut off; the layout itself is the ready signal here.
    session.wait_for_screen("session restored", STARTUP_TIMEOUT, |screen| {
        screen.contains('│')
            && screen.contains("alpha file")
            && screen.contains("beta file")
            && screen.contains("[beta.txt]")
            && !screen.contains("Untitled")
    })?;
    session.send(CTRL_Q)?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(status.success(), "restored session exit failed: {status}");
    session.assert_terminal_restored(&baseline)
}

#[test]
fn language_servers_answer_through_zed() -> Result<()> {
    let temp = tempfile::tempdir().context("create PTY fixture")?;
    let root = temp.path().join("lsp-repo");
    fs::create_dir_all(root.join("src"))?;
    fs::write(root.join("src/main.rs"), "stub_\n")?;
    fs::write(root.join("src/lib.rs"), "pub fn stub_peer() {}\n")?;

    // The fixture server stands in for rust-analyzer through Zed's own
    // binary override, so the request path is the real one end to end.
    let data_dir = tempfile::tempdir().context("create isolated data directory")?;
    let config = data_dir.path().join("config");
    fs::create_dir_all(&config)?;
    let fixture = env!("CARGO_BIN_EXE_fixture_lsp").replace('\\', "/");
    fs::write(
        config.join("settings.json"),
        format!(
            r#"{{"lsp": {{"rust-analyzer": {{"binary": {{"path": "{fixture}", "arguments": []}}}}}}}}"#
        ),
    )?;
    fs::write(config.join("keymap.json"), "[]")?;

    let pair = open_pty()?;
    let baseline = capture_baseline(&pair)?;
    let data_path = data_dir.path().to_path_buf();
    let mut session =
        PtySession::spawn_with_data_dir(pair, &[root.as_os_str()], &data_path, Some(data_dir))?;
    session.wait_ready()?;

    // Zed restricts an unknown root; the verdict stays in the status row
    // until the root is trusted, and only then do language servers start.
    session.wait_for_screen("worktree restricted", ACTION_TIMEOUT, |screen| {
        screen.contains("lsp-repo [restricted]") && screen.contains("Ctrl-Shift-T trust")
    })?;
    session.send(CTRL_SHIFT_T)?;
    session.wait_for_screen("worktree trusted", ACTION_TIMEOUT, |screen| {
        screen.contains("trusted lsp-repo") && !screen.contains("[restricted]")
    })?;
    session.send(CTRL_P)?;
    session.paste("main.rs")?;
    session.wait_for_screen("quick open lists main.rs", ACTION_TIMEOUT, |screen| {
        screen.contains("Quick open: main.rs") && screen.contains("› src/main.rs")
    })?;
    session.send(ENTER)?;
    session.wait_for_screen("language server started", ACTION_TIMEOUT, |screen| {
        screen.contains("language server rust-analyzer started")
    })?;

    // Completion: the word before the caret seeds the picker's query.
    session.send(END)?;
    let mut attempts = 0;
    loop {
        attempts += 1;
        session.send(ALT_SLASH)?;
        session.wait_for_screen("completions or none", ACTION_TIMEOUT, |screen| {
            screen.contains("Completions: stub_") || screen.contains("no completions")
        })?;
        if session.screen().contains("Completions: stub_") {
            break;
        }
        ensure!(
            attempts < 10,
            "the fixture server never answered completions"
        );
        thread::sleep(Duration::from_millis(300));
    }
    session.wait_for_screen(
        "completions filtered to the word",
        ACTION_TIMEOUT,
        |screen| screen.contains("stub_completion") && !screen.contains("beta_completion"),
    )?;
    session.send(ENTER)?;
    session.wait_for_screen("completion applied", ACTION_TIMEOUT, |screen| {
        screen.contains("stub_completion()") && screen.contains("completed ")
    })?;

    // Hover is a read-only overlay.
    session.send(F2)?;
    session.wait_for_screen("hover shown", ACTION_TIMEOUT, |screen| {
        screen.contains("Hover  (Esc closes)") && screen.contains("Fixture hover with")
    })?;
    session.send(ESC)?;
    session.wait_for_screen("hover closed", ACTION_TIMEOUT, |screen| {
        !screen.contains("Fixture hover with")
    })?;

    // Diagnostics list the published warning; Enter jumps to it.
    session.send(F8)?;
    session.wait_for_screen("diagnostics listed", ACTION_TIMEOUT, |screen| {
        screen.contains("Diagnostics:")
            && screen.contains("src/main.rs:1")
            && screen.contains("warning deterministic fixture warning")
    })?;
    session.send(ENTER)?;
    session.wait_for_screen("diagnostic opened", ACTION_TIMEOUT, |screen| {
        screen.contains("switched to src/main.rs")
    })?;

    // References need a second open buffer to list more than one hit.
    session.send(CTRL_P)?;
    session.paste("lib.rs")?;
    session.wait_for_screen("quick open lists lib.rs", ACTION_TIMEOUT, |screen| {
        screen.contains("Quick open: lib.rs") && screen.contains("› src/lib.rs")
    })?;
    session.send(ENTER)?;
    session.wait_for_screen("lib.rs opened", ACTION_TIMEOUT, |screen| {
        screen.contains("opened src/lib.rs")
    })?;
    session.send(CTRL_PAGE_UP)?;
    session.wait_for_screen("back on main.rs", ACTION_TIMEOUT, |screen| {
        screen.contains("[main.rs+]")
    })?;
    session.send(SHIFT_F12)?;
    session.wait_for_screen("references listed", ACTION_TIMEOUT, |screen| {
        screen.contains("References:")
            && screen.contains("src/main.rs:1")
            && screen.contains("src/lib.rs:1")
    })?;
    session.send(DOWN)?;
    session.send(ENTER)?;
    session.wait_for_screen("reference opened", ACTION_TIMEOUT, |screen| {
        screen.contains("switched to src/lib.rs") && screen.contains("[lib.rs]")
    })?;
    session.send(CTRL_PAGE_UP)?;
    session.wait_for_screen("back on main.rs again", ACTION_TIMEOUT, |screen| {
        screen.contains("[main.rs+]")
    })?;

    // Rename is a prompt seeded by the server, applied as a Zed
    // transaction, so Ctrl-Z undoes it.
    session.send(F6)?;
    session.wait_for_screen("rename prompt", ACTION_TIMEOUT, |screen| {
        screen.contains("Rename: stub_")
    })?;
    session.send(CTRL_U)?;
    session.paste("renamed_")?;
    session.send(ENTER)?;
    session.wait_for_screen("renamed", ACTION_TIMEOUT, |screen| {
        screen.contains("renamed_completion()") && screen.contains("renamed to renamed_")
    })?;
    session.send(CTRL_Z)?;
    session.wait_for_screen("rename undone", ACTION_TIMEOUT, |screen| {
        screen.contains("stub_completion()") && !screen.contains("renamed_completion()")
    })?;

    // Code actions: the fixture's quick fix rewrites the symbol.
    session.send(CTRL_PERIOD)?;
    session.wait_for_screen("code actions listed", ACTION_TIMEOUT, |screen| {
        screen.contains("Code actions:") && screen.contains("Apply fixture quick fix")
    })?;
    session.send(ENTER)?;
    session.wait_for_screen("code action applied", ACTION_TIMEOUT, |screen| {
        screen.contains("fixture_fixedcompletion()")
            && screen.contains("applied Apply fixture quick fix")
    })?;

    session.send(CTRL_Q)?;
    session.wait_for_screen("dirty quit guard", ACTION_TIMEOUT, |screen| {
        screen.contains("unsaved or deleted tab(s)")
    })?;
    session.send(CTRL_Q)?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(status.success(), "language e2e exit failed: {status}");
    session.assert_terminal_restored(&baseline)
}

#[test]
fn project_panel_browses_opens_and_mutates_the_tree() -> Result<()> {
    let temp = tempfile::tempdir().context("create PTY fixture")?;
    let root = temp.path().join("panel-repo");
    fs::create_dir_all(root.join("src"))?;
    fs::write(root.join("README.md"), "readme\n")?;
    fs::write(root.join("src/main.rs"), "fn main() {}\n")?;

    let pair = open_pty()?;
    let baseline = capture_baseline(&pair)?;
    let mut session = PtySession::spawn(pair, &[root.as_os_str()])?;
    session.wait_ready()?;

    // The panel opens focused on the tree: directories first, collapsed.
    session.send(F7)?;
    session.wait_for_screen("project panel shown", ACTION_TIMEOUT, |screen| {
        screen.contains("project panel shown")
            && screen.contains("▸ src")
            && screen.contains("README.md")
            && !screen.contains("main.rs")
    })?;
    session.send(ENTER)?;
    session.wait_for_screen("directory expanded", ACTION_TIMEOUT, |screen| {
        screen.contains("▾ src") && screen.contains("main.rs")
    })?;
    session.send(DOWN)?;
    session.send(ENTER)?;
    session.wait_for_screen("file opened from the panel", ACTION_TIMEOUT, |screen| {
        screen.contains("fn main() {}")
            && screen.contains("opened src/main.rs")
            && screen.contains("[main.rs]")
    })?;

    // Opening returned focus to the editor; the toggle focuses the panel
    // again, where a new file goes next to the selected one.
    session.send(F7)?;
    session.wait_for_screen("project panel focused", ACTION_TIMEOUT, |screen| {
        screen.contains("project panel focused")
    })?;
    session.send(b"n")?;
    session.wait_for_screen("new file prompt", ACTION_TIMEOUT, |screen| {
        screen.contains("New file:")
    })?;
    session.paste("created.txt")?;
    session.send(ENTER)?;
    session.wait_for_screen(
        "file created through the project",
        ACTION_TIMEOUT,
        |screen| screen.contains("created src/created.txt") && screen.contains("created.txt"),
    )?;
    ensure!(
        root.join("src/created.txt").is_file(),
        "the project did not create src/created.txt"
    );

    session.send(F2)?;
    session.wait_for_screen("rename prompt", ACTION_TIMEOUT, |screen| {
        screen.contains("Rename: created.txt")
    })?;
    session.send(CTRL_U)?;
    session.paste("renamed.txt")?;
    session.send(ENTER)?;
    session.wait_for_screen("entry renamed", ACTION_TIMEOUT, |screen| {
        screen.contains("renamed to src/renamed.txt") && screen.contains("renamed.txt")
    })?;
    ensure!(
        root.join("src/renamed.txt").is_file() && !root.join("src/created.txt").exists(),
        "the project did not rename the entry"
    );

    session.send(DELETE)?;
    session.wait_for_screen("delete confirmation", ACTION_TIMEOUT, |screen| {
        screen.contains("delete renamed.txt? press Delete again to confirm")
    })?;
    session.send(DELETE)?;
    session.wait_for_screen("entry deleted", ACTION_TIMEOUT, |screen| {
        screen.contains("deleted renamed.txt") && !screen.contains("renamed.txt [")
    })?;
    session.wait_until("entry gone from disk", ACTION_TIMEOUT, |_| {
        !root.join("src/renamed.txt").exists()
    })?;

    session.send(ESC)?;
    session.wait_for_screen("editor focused", ACTION_TIMEOUT, |screen| {
        screen.contains("editor focused")
    })?;
    session.send(F7)?;
    session.send(F7)?;
    session.wait_for_screen("project panel hidden", ACTION_TIMEOUT, |screen| {
        screen.contains("project panel hidden") && !screen.contains("README.md")
    })?;

    session.send(CTRL_Q)?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(status.success(), "project panel exit failed: {status}");
    session.assert_terminal_restored(&baseline)
}

#[test]
fn outline_panel_lists_symbols_and_jumps_to_them() -> Result<()> {
    let temp = tempfile::tempdir().context("create PTY fixture")?;
    let path = temp.path().join("lib.rs");
    fs::write(&path, "fn alpha() {}\n\nfn beta() {}\n")?;

    let pair = open_pty()?;
    let baseline = capture_baseline(&pair)?;
    let mut session = PtySession::spawn(pair, &[path.as_os_str()])?;
    session.wait_ready()?;

    session.send(F9)?;
    session.wait_for_screen("outline panel shown", ACTION_TIMEOUT, |screen| {
        // The symbols appear once the buffer is parsed: in the panel, next
        // to their source lines.
        screen.contains("outline panel shown")
            && screen.contains("Outline lib.rs")
            && screen.matches("fn alpha").count() == 2
            && screen.matches("fn beta").count() == 2
    })?;
    session.send(DOWN)?;
    session.send(ENTER)?;
    session.wait_for_screen("jumped to the symbol", ACTION_TIMEOUT, |screen| {
        screen.contains("jumped to fn beta")
    })?;
    // The caret sits on the symbol's name and the editor has focus again.
    session.paste("x")?;
    session.wait_for_screen("edit lands at the symbol", ACTION_TIMEOUT, |screen| {
        screen.contains("fn xbeta() {}") && screen.contains("fn xbeta")
    })?;

    session.send(F9)?;
    session.wait_for_screen("outline panel focused", ACTION_TIMEOUT, |screen| {
        screen.contains("outline panel focused")
    })?;
    session.send(F9)?;
    session.wait_for_screen("outline panel hidden", ACTION_TIMEOUT, |screen| {
        screen.contains("outline panel hidden") && !screen.contains("Outline lib.rs")
    })?;

    session.send(CTRL_Q)?;
    session.wait_for_screen("dirty quit confirmation", ACTION_TIMEOUT, |screen| {
        screen.contains("unsaved or deleted tab(s)")
    })?;
    session.send(CTRL_Q)?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(status.success(), "outline panel exit failed: {status}");
    session.assert_terminal_restored(&baseline)
}

#[test]
fn tabs_open_switch_close_and_the_palette_runs_commands() -> Result<()> {
    let temp = tempfile::tempdir().context("create PTY fixture")?;
    let first = temp.path().join("first.txt");
    let second = temp.path().join("second.txt");
    fs::write(&first, "first file\n")?;
    fs::write(&second, "second file\n")?;

    let pair = open_pty()?;
    let baseline = capture_baseline(&pair)?;
    let mut session = PtySession::spawn(pair, &[first.as_os_str()])?;
    session.wait_ready()?;
    session.assert_raw_mode_enabled(&baseline)?;

    session.send(CTRL_O)?;
    session.wait_for_screen("open prompt", ACTION_TIMEOUT, |screen| {
        screen.contains("Open:")
    })?;
    session.paste(&second.to_string_lossy())?;
    session.send(ENTER)?;
    session.wait_for_screen("second tab", ACTION_TIMEOUT, |screen| {
        screen.contains("second file") && screen.contains("[second.txt]")
    })?;

    // Opening the same path again activates the existing tab.
    session.send(CTRL_O)?;
    session.wait_for_screen("open prompt again", ACTION_TIMEOUT, |screen| {
        screen.contains("Open:")
    })?;
    session.paste(&first.to_string_lossy())?;
    session.send(ENTER)?;
    session.wait_for_screen("switched to first tab", ACTION_TIMEOUT, |screen| {
        screen.contains("first file") && screen.contains("[first.txt]") && screen.contains("1/2")
    })?;

    session.send(CTRL_PAGE_DOWN)?;
    session.wait_for_screen("next tab", ACTION_TIMEOUT, |screen| {
        screen.contains("second file") && screen.contains("[second.txt]")
    })?;

    session.send(F1)?;
    session.wait_for_screen("command palette", ACTION_TIMEOUT, |screen| {
        screen.contains("Commands:")
    })?;
    session.paste("close")?;
    session.wait_for_screen("palette filter", ACTION_TIMEOUT, |screen| {
        screen.contains("Close Tab")
    })?;
    session.send(ENTER)?;
    session.wait_for_screen("tab closed", ACTION_TIMEOUT, |screen| {
        screen.contains("tab closed") && screen.contains("first file") && !screen.contains("2/2")
    })?;

    session.send(CTRL_W)?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(status.success(), "closing the last tab failed: {status}");
    session.assert_terminal_restored(&baseline)
}

#[test]
fn external_changes_reload_clean_buffers_and_guard_dirty_ones() -> Result<()> {
    let temp = tempfile::tempdir().context("create PTY fixture")?;
    let path = temp.path().join("watched.txt");
    fs::write(&path, "version one\n")?;

    let pair = open_pty()?;
    let baseline = capture_baseline(&pair)?;
    let mut session = PtySession::spawn(pair, &[path.as_os_str()])?;
    session.wait_ready()?;
    session.assert_raw_mode_enabled(&baseline)?;

    fs::write(&path, "version two\n")?;
    session.wait_for_screen("clean buffer auto-reload", ACTION_TIMEOUT, |screen| {
        screen.contains("version two") && !screen.contains("version one")
    })?;

    session.send(CTRL_A)?;
    session.paste("local edit\n")?;
    session.wait_for_screen("dirty marker", ACTION_TIMEOUT, |screen| {
        screen.contains("local edit") && screen.contains("watched.txt+")
    })?;
    fs::write(&path, "version three\n")?;
    session.wait_for_screen("conflict marker", ACTION_TIMEOUT, |screen| {
        screen.contains("watched.txt!") && screen.contains("local edit")
    })?;

    session.send(CTRL_S)?;
    session.wait_for_screen("save conflict confirmation", ACTION_TIMEOUT, |screen| {
        screen.contains("changed on disk; press Ctrl-S again to overwrite")
    })?;
    session.send(CTRL_R)?;
    session.wait_for_screen("reload confirmation", ACTION_TIMEOUT, |screen| {
        screen.contains("unsaved changes; press Ctrl-R again to reload from disk")
    })?;
    session.send(CTRL_R)?;
    session.wait_for_screen("reloaded", ACTION_TIMEOUT, |screen| {
        screen.contains("version three") && !screen.contains("local edit")
    })?;
    session.send(CTRL_Z)?;
    session.wait_for_screen("reload is undoable", ACTION_TIMEOUT, |screen| {
        screen.contains("local edit")
    })?;

    session.send(CTRL_Q)?;
    session.wait_for_screen("dirty quit guard", ACTION_TIMEOUT, |screen| {
        screen.contains("unsaved or deleted tab(s); press Ctrl-Q again to discard")
    })?;
    session.send(CTRL_Q)?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(status.success(), "discard exit failed: {status}");
    session.assert_terminal_restored(&baseline)
}

fn edit_undo_resize_save_and_quit(directory: &Path) -> Result<()> {
    const INITIAL: &str = "seed line\n";
    const INSERTED: &str = "先頭-";
    const REPLACEMENT: &str = "置換🧪\n";
    const FINAL: &str = "保存🌍 after resize\nsecond line\n";

    let path = directory.join("normal.txt");
    fs::write(&path, INITIAL)?;

    let pair = open_pty()?;
    let baseline = capture_baseline(&pair)?;
    let mut session = PtySession::spawn(pair, &[path.as_os_str()])?;
    session.wait_ready()?;
    session.assert_raw_mode_enabled(&baseline)?;

    session.send(F4)?;
    session.wait_for_screen("terminal capability report", ACTION_TIMEOUT, |screen| {
        screen.contains("terminal keyboard=modifyOtherKeys")
            && screen.contains("mouse=on/unverified")
    })?;
    session.send(b"\x1b[O")?;
    session.wait_for_screen("terminal focus loss event", ACTION_TIMEOUT, |screen| {
        screen.contains("terminal focus lost")
    })?;
    session.send(b"\x1b[I")?;
    session.wait_for_screen("terminal focus gain event", ACTION_TIMEOUT, |screen| {
        screen.contains("terminal focus gained")
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
    // Unix paste is one atomic transaction. ConPTY delivers paste as
    // keystrokes, which Zed's time-based grouping folds into one transaction
    // covering both edits.
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

    // A different command clears the armed quit and saves through the store.
    session.send(CTRL_S)?;
    session.wait_for_screen("successful save", ACTION_TIMEOUT, |screen| {
        screen.contains("saved  |  zec")
    })?;
    let bytes = fs::read(&path)?;
    ensure!(bytes == FINAL.as_bytes(), "saved bytes differ: {bytes:?}");

    session.send(CTRL_Q)?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(status.success(), "normal zec exit failed: {status}");
    session.assert_terminal_restored(&baseline)
}

fn scratch_save_as_and_failed_save_keep_the_buffer_dirty(directory: &Path) -> Result<()> {
    const MARKER: &str = "UNSAVED-失敗";

    let blocker = directory.join("blocker");
    let impossible = blocker.join("child.txt");
    fs::write(&blocker, b"original blocker bytes\n")?;
    let saved = directory.join("scratch-saved.txt");

    let pair = open_pty()?;
    let baseline = capture_baseline(&pair)?;
    let mut session = PtySession::spawn(pair, &[directory.as_os_str()])?;
    session.wait_ready()?;
    session.assert_raw_mode_enabled(&baseline)?;
    session.send(CTRL_N)?;
    session.wait_for_screen("scratch tab", ACTION_TIMEOUT, |screen| {
        screen.contains("Untitled 2")
    })?;

    session.paste(MARKER)?;
    session.wait_for_screen("dirty scratch text", ACTION_TIMEOUT, |screen| {
        screen.contains(MARKER)
    })?;
    session.send(CTRL_S)?;
    session.wait_for_screen("Save As prompt", ACTION_TIMEOUT, |screen| {
        screen.contains("Save as:")
    })?;
    session.paste(&impossible.to_string_lossy())?;
    session.send(ENTER)?;
    session.wait_for_screen("deterministic save failure", ACTION_TIMEOUT, |screen| {
        screen.contains("save failed:") && screen.contains(MARKER)
    })?;
    ensure!(
        !impossible.exists(),
        "failed save created {}",
        impossible.display()
    );
    ensure!(
        fs::read(&blocker)? == b"original blocker bytes\n",
        "failed save changed the blocker file"
    );
    session.send(ESC)?;
    session.wait_for_screen("Save As cancelled", ACTION_TIMEOUT, |screen| {
        screen.contains("save as cancelled")
    })?;

    // Save As over an existing file needs a second Enter.
    fs::write(&saved, b"old contents\n")?;
    session.send(CTRL_S)?;
    session.wait_for_screen("Save As prompt again", ACTION_TIMEOUT, |screen| {
        screen.contains("Save as:")
    })?;
    session.paste(&saved.to_string_lossy())?;
    session.send(ENTER)?;
    session.wait_for_screen("overwrite confirmation", ACTION_TIMEOUT, |screen| {
        screen.contains("file exists; press Enter again to overwrite")
    })?;
    session.send(ENTER)?;
    session.wait_for_screen("Save As succeeded", ACTION_TIMEOUT, |screen| {
        screen.contains("saved  |  zec") && screen.contains("scratch-saved.txt")
    })?;
    ensure!(
        fs::read(&saved)? == MARKER.as_bytes(),
        "Save As wrote different bytes"
    );

    session.send(CTRL_Q)?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(
        status.success(),
        "clean exit after Save As failed: {status}"
    );
    session.assert_terminal_restored(&baseline)
}

#[cfg(unix)]
fn signal_exit_restores_terminal(directory: &Path, signal: Signal) -> Result<()> {
    let path = directory.join(format!("signal-{}.txt", signal as i32));
    fs::write(&path, "clean signal fixture\n")?;

    let pair = open_pty()?;
    let baseline = capture_baseline(&pair)?;
    let mut session = PtySession::spawn(pair, &[path.as_os_str()])?;
    session.wait_ready()?;
    session.assert_raw_mode_enabled(&baseline)?;

    session.send_signal(signal)?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(
        status.success(),
        "{signal:?} did not use a clean exit: {status}"
    );
    session.assert_terminal_restored(&baseline)
}

#[cfg(unix)]
fn suspend_restores_and_resume_reenters_the_terminal(directory: &Path) -> Result<()> {
    const TOKEN: &str = "resumed-after-sigtstp";
    let path = directory.join("suspend-resume.txt");
    fs::write(&path, "before suspend\n")?;

    let pair = open_pty()?;
    let baseline = capture_baseline(&pair)?;
    let mut session = PtySession::spawn(pair, &[path.as_os_str()])?;
    session.wait_ready()?;
    session.assert_raw_mode_enabled(&baseline)?;

    session.send_signal(Signal::SIGTSTP)?;
    session.wait_for_stop(EXIT_TIMEOUT)?;
    session.assert_terminal_restored(&baseline)?;

    session.send_signal(Signal::SIGCONT)?;
    session.wait_for_screen("frame after SIGCONT", ACTION_TIMEOUT, |screen| {
        screen.contains("resumed  |  zec")
    })?;
    session.assert_raw_mode_enabled(&baseline)?;
    session.send(CTRL_A)?;
    session.paste(TOKEN)?;
    session.send(CTRL_S)?;
    session.wait_for_screen("save after SIGCONT", ACTION_TIMEOUT, |screen| {
        screen.contains("saved  |  zec")
    })?;
    let expected = format!("{TOKEN}\n");
    let saved = fs::read(&path)?;
    ensure!(
        saved == expected.as_bytes(),
        "editing after SIGCONT did not reach disk: {:?}",
        String::from_utf8_lossy(&saved)
    );
    session.send(CTRL_Q)?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(status.success(), "post-SIGCONT exit failed: {status}");
    session.assert_terminal_restored(&baseline)
}

fn open_pty() -> Result<PtyPair> {
    native_pty_system()
        .openpty(INITIAL_SIZE)
        .context("open a PTY pair")
}

struct TerminalBaseline {
    #[cfg(unix)]
    termios: Termios,
}

fn capture_baseline(pair: &PtyPair) -> Result<TerminalBaseline> {
    #[cfg(unix)]
    {
        Ok(TerminalBaseline {
            termios: pair
                .master
                .get_termios()
                .context("PTY does not expose its initial termios")?,
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
    /// zec's log file, shown on failure.
    log_path: PathBuf,
    _data_dir: Option<tempfile::TempDir>,
}

impl PtySession {
    fn spawn(pair: PtyPair, arguments: &[&OsStr]) -> Result<Self> {
        let data_dir = tempfile::tempdir().context("create isolated data directory")?;
        let path = data_dir.path().to_path_buf();
        Self::spawn_with_data_dir(pair, arguments, &path, Some(data_dir))
    }

    /// `owned` keeps a temporary data directory alive with the session; a
    /// test that relaunches into the same directory owns it instead.
    fn spawn_with_data_dir(
        pair: PtyPair,
        arguments: &[&OsStr],
        data_dir: &Path,
        owned: Option<tempfile::TempDir>,
    ) -> Result<Self> {
        let PtyPair { slave, master } = pair;
        let mut reader = master.try_clone_reader().context("clone PTY reader")?;
        let writer = master.take_writer().context("take PTY writer")?;
        let (event_sender, events) = mpsc::sync_channel(64);
        let reader_thread = thread::Builder::new()
            .name("zec-pty-reader".to_owned())
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

        // Isolate every Zed user directory so the test never reads or writes
        // the developer's real configuration.
        let config = data_dir.join("config");
        fs::create_dir_all(&config)?;
        if !config.join("settings.json").exists() {
            fs::write(config.join("settings.json"), "{}")?;
            fs::write(config.join("keymap.json"), "[]")?;
        }

        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_zec"));
        command.args(arguments);
        command.env("TERM", "xterm-256color");
        command.env("LANG", "C.UTF-8");
        command.env("LC_ALL", "C.UTF-8");
        command.env("ZEC_KEYBOARD_PROTOCOL", "modifyOtherKeys");
        command.env("ZEC_DATA_DIR", data_dir);
        command.env("XDG_CONFIG_HOME", data_dir);
        command.env("XDG_DATA_HOME", data_dir.join("data"));
        let log_path = data_dir.join("zec.log");
        command.env("ZEC_LOG", &log_path);
        let child = slave
            .spawn_command(command)
            .context("spawn the actual zec binary")?;
        // The parent must not keep the slave open: doing so suppresses EOF
        // after the child exits and can make failure cleanup hang forever.
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
            log_path,
            _data_dir: owned,
        })
    }

    fn wait_ready(&mut self) -> Result<()> {
        self.wait_until("zec initial frame", STARTUP_TIMEOUT, |session| {
            let screen = session.parser.screen();
            // ConPTY flattens alternate-screen, bracketed-paste, and mouse
            // negotiation; only the repaint is observable there.
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

    fn ensure_running(&mut self, description: &str) -> Result<()> {
        self.drain_available()?;
        let child = self
            .child
            .as_mut()
            .context("zec child was already reaped")?;
        if let Some(status) = child.try_wait().context("poll zec")? {
            bail!(
                "{description}; zec exited as {status}\n{}",
                self.diagnostic()
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
            // Astral characters go as win32-input-mode key events, the way
            // Windows Terminal sends them.
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

    /// The screen as it is now, after the last wait.
    fn screen(&self) -> String {
        self.parser.screen().contents()
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
        // ConPTY absorbs the client's cleanup escapes and synthesizes its own
        // teardown, so the exact sequence and termios are Unix-only evidence.
        #[cfg(unix)]
        {
            self.wait_until(
                "ordered terminal cleanup escapes",
                EXIT_TIMEOUT,
                |session| contains_bytes(&session.transcript, CLEANUP_ESCAPES),
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
            let _ = initial;
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
                // A ConPTY host must answer conhost's startup cursor-position
                // report or client console I/O stays deferred.
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
            ReaderEvent::Error(error) => Err(error).context("read zec PTY output"),
        }
    }

    fn diagnostic(&self) -> String {
        let tail_start = self.transcript.len().saturating_sub(DIAGNOSTIC_TAIL);
        let log = fs::read_to_string(&self.log_path).unwrap_or_default();
        let log_start = log.len().saturating_sub(4 * DIAGNOSTIC_TAIL);
        format!(
            "screen:\n{}\nraw tail:\n{:?}\nlog tail:\n{}",
            self.parser.screen().contents(),
            String::from_utf8_lossy(&self.transcript[tail_start..]),
            &log[log_start..]
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
        // The writer sends VEOF when dropped; close the slave side before
        // joining the blocking reader so every failure path is leak-free.
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

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty()
        || haystack
            .windows(needle.len())
            .any(|window| window == needle)
}
