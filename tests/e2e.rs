//! The actual binary through a PTY: terminal lifecycle, editing, saving,
//! tabs, and external changes.

use std::{
    ffi::OsStr,
    fs,
    io::{self, Read as _, Write as _},
    path::Path,
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
const DOWN: &[u8] = b"\x1b[B";
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
    _data_dir: tempfile::TempDir,
}

impl PtySession {
    fn spawn(pair: PtyPair, arguments: &[&OsStr]) -> Result<Self> {
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
        let data_dir = tempfile::tempdir().context("create isolated data directory")?;
        let config = data_dir.path().join("config");
        fs::create_dir_all(&config)?;
        fs::write(config.join("settings.json"), "{}")?;
        fs::write(config.join("keymap.json"), "[]")?;

        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_zec"));
        command.args(arguments);
        command.env("TERM", "xterm-256color");
        command.env("LANG", "C.UTF-8");
        command.env("LC_ALL", "C.UTF-8");
        command.env("ZEC_KEYBOARD_PROTOCOL", "modifyOtherKeys");
        command.env("ZEC_DATA_DIR", data_dir.path());
        command.env("XDG_CONFIG_HOME", data_dir.path());
        command.env("XDG_DATA_HOME", data_dir.path().join("data"));
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
            _data_dir: data_dir,
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
