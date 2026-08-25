#![cfg(target_os = "linux")]

use std::{
    ffi::OsStr,
    fs,
    io::{self, Read as _, Write as _},
    os::unix::fs::{PermissionsExt as _, symlink},
    path::Path,
    sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail, ensure};
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
const CTRL_P: &[u8] = b"\x10";
const CTRL_Q: &[u8] = b"\x11";
const CTRL_S: &[u8] = b"\x13";
const CTRL_Z: &[u8] = b"\x1a";
const CTRL_PAGE_DOWN: &[u8] = b"\x1b[6;5~";
const ENTER: &[u8] = b"\r";

// This is one execute! call in TerminalSession::restore. Keeping the full ordered
// sequence here catches a regression where only some terminal features are reset.
const CLEANUP_ESCAPES: &[u8] = concat!(
    "\x1b[?25h",
    "\x1b[?1006l",
    "\x1b[?1015l",
    "\x1b[?1003l",
    "\x1b[?1002l",
    "\x1b[?1000l",
    "\x1b[?2004l",
    "\x1b[?1049l",
)
.as_bytes();

#[test]
fn actual_binary_preserves_edits_and_restores_the_pty_on_every_exit_path() -> Result<()> {
    let temp = tempfile::tempdir().context("create PTY acceptance fixture")?;

    normal_edit_undo_resize_save_and_quit(temp.path())?;
    directory_quick_open_deduplicates_symlink_alias(temp.path())?;
    failed_save_keeps_dirty_text_and_quit_guard(temp.path())?;
    restricted_worktree_requires_confirmation_before_lsp(temp.path())?;
    signal_exit_restores_terminal(temp.path(), Signal::SIGINT)?;
    signal_exit_restores_terminal(temp.path(), Signal::SIGQUIT)?;
    signal_exit_restores_terminal(temp.path(), Signal::SIGTERM)?;
    signal_exit_restores_terminal(temp.path(), Signal::SIGHUP)?;
    suspend_restores_and_resume_reenters_the_terminal(temp.path())?;

    Ok(())
}

fn normal_edit_undo_resize_save_and_quit(directory: &Path) -> Result<()> {
    const INITIAL: &str = "seed line\n";
    const INSERTED: &str = "先頭-";
    const REPLACEMENT: &str = "置換🧪\n";
    const FINAL: &str = "保存🌍 after resize\nsecond line\n";

    let path = directory.join("normal.txt");
    fs::write(&path, INITIAL).context("write normal editing fixture")?;

    let pair = open_pty()?;
    let termios_before = pair
        .master
        .get_termios()
        .context("PTY does not expose its initial termios")?;
    let mut session = PtySession::spawn(pair, &[path.as_os_str()])?;
    session.wait_ready()?;
    session.assert_raw_mode_enabled(&termios_before)?;

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
    session.wait_for_screen("Zed undo", ACTION_TIMEOUT, |screen| {
        screen.contains("先頭-seed line") && !screen.contains("置換🧪")
    })?;

    session.resize(RESIZED)?;
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

fn directory_quick_open_deduplicates_symlink_alias(directory: &Path) -> Result<()> {
    const READY: &str = "ALPHA1_READY_SENTINEL";
    const OPENED: &str = "ALPHA1_QUICK_OPEN_BODY";

    let root = directory.join("quick-open-repo");
    fs::create_dir_all(root.join("src")).context("create quick-open src")?;
    fs::create_dir_all(root.join("aliases")).context("create quick-open aliases")?;
    fs::write(root.join("README.md"), format!("{READY}\n")).context("write quick-open README")?;
    fs::write(root.join("src/日本 語.rs"), format!("{OPENED}\n"))
        .context("write quick-open target")?;
    std::os::unix::fs::symlink("../src/日本 語.rs", root.join("aliases/日本 語.rs"))
        .context("create quick-open symlink alias")?;

    let pair = open_pty()?;
    let termios_before = pair
        .master
        .get_termios()
        .context("PTY does not expose its initial termios")?;
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
    let termios_before = pair
        .master
        .get_termios()
        .context("PTY does not expose its initial termios")?;
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
    fs::write(xdg_config.join("zed/settings.json"), "{}")
        .context("write restricted user settings")?;
    fs::write(xdg_config.join("zed/global_settings.json"), "{}")
        .context("write restricted global settings")?;
    fs::write(xdg_config.join("zed/keymap.json"), "[]").context("write restricted keymap")?;

    symlink(
        Path::new(env!("CARGO_BIN_EXE_alpha_2_fixture_lsp")),
        bin_dir.join("rust-analyzer"),
    )
    .context("link PTY fixture language server")?;
    let fake_rustup = bin_dir.join("rustup");
    fs::write(&fake_rustup, "#!/bin/sh\nexit 1\n").context("write fake rustup")?;
    let mut permissions = fs::metadata(&fake_rustup)
        .context("read fake rustup metadata")?
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&fake_rustup, permissions).context("make fake rustup executable")?;

    let log = workspace.join("lsp.jsonl");
    let path = format!("{}:/usr/local/bin:/usr/bin:/bin", bin_dir.display());
    let pair = open_pty()?;
    let termios_before = pair
        .master
        .get_termios()
        .context("PTY does not expose its initial termios")?;
    let environment = [
        (OsStr::new("PATH"), OsStr::new(&path)),
        (OsStr::new("HOME"), home.as_os_str()),
        (OsStr::new("XDG_CONFIG_HOME"), xdg_config.as_os_str()),
        (OsStr::new("XDG_DATA_HOME"), xdg_data.as_os_str()),
        (OsStr::new("XDG_CACHE_HOME"), xdg_cache.as_os_str()),
        (OsStr::new("XDG_STATE_HOME"), xdg_state.as_os_str()),
        (OsStr::new("RUSTUP_HOME"), rustup_home.as_os_str()),
        (OsStr::new("CARGO_HOME"), cargo_home.as_os_str()),
        (OsStr::new("ZEC_ALPHA2_LSP_LOG"), log.as_os_str()),
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
    session.assert_terminal_restored(&termios_before)
}

fn signal_exit_restores_terminal(directory: &Path, signal: Signal) -> Result<()> {
    let path = directory.join(format!("signal-{}.txt", signal as i32));
    fs::write(&path, "clean signal fixture\n").context("write signal fixture")?;

    let pair = open_pty()?;
    let termios_before = pair
        .master
        .get_termios()
        .context("PTY does not expose its initial termios")?;
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

fn suspend_restores_and_resume_reenters_the_terminal(directory: &Path) -> Result<()> {
    const TOKEN: &str = "resumed-after-sigtstp";
    let path = directory.join("suspend-resume.txt");
    fs::write(&path, "before suspend\n").context("write suspend fixture")?;

    let pair = open_pty()?;
    let termios_before = pair
        .master
        .get_termios()
        .context("PTY does not expose its initial termios")?;
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
        })
    }

    fn wait_ready(&mut self) -> Result<()> {
        self.wait_until("zec initial frame", STARTUP_TIMEOUT, |session| {
            let screen = session.parser.screen();
            screen.contents().contains("Ctrl-N new")
                && screen.alternate_screen()
                && screen.bracketed_paste()
                && screen.mouse_protocol_mode() != MouseProtocolMode::None
        })
    }

    fn assert_raw_mode_enabled(&self, initial: &Termios) -> Result<()> {
        let current = self.termios()?;
        ensure!(
            &current != initial,
            "zec rendered its UI without enabling terminal raw mode"
        );
        ensure!(
            !current
                .local_flags
                .contains(LocalFlags::ICANON | LocalFlags::ECHO),
            "zec left canonical input or terminal echo enabled in raw mode"
        );
        Ok(())
    }

    fn send(&mut self, bytes: &[u8]) -> Result<()> {
        let writer = self.writer.as_mut().context("PTY writer is closed")?;
        writer.write_all(bytes).context("write PTY input")?;
        writer.flush().context("flush PTY input")
    }

    fn paste(&mut self, text: &str) -> Result<()> {
        self.send(b"\x1b[200~")?;
        self.send(text.as_bytes())?;
        self.send(b"\x1b[201~")
    }

    fn resize(&mut self, size: PtySize) -> Result<()> {
        let previous_generation = self.output_generation;
        self.master
            .as_ref()
            .context("PTY master is closed")?
            .resize(size)
            .context("resize PTY")?;
        self.parser.screen_mut().set_size(size.rows, size.cols);
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

    fn assert_terminal_restored(&mut self, initial: &Termios) -> Result<()> {
        self.wait_for_raw(
            "ordered terminal cleanup escapes",
            CLEANUP_ESCAPES,
            EXIT_TIMEOUT,
        )?;
        self.drain_available()?;

        let current = self.termios()?;
        ensure!(
            &current == initial,
            "stty state differs after zec exit\n{}",
            self.diagnostic()
        );

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

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty()
        || haystack
            .windows(needle.len())
            .any(|window| window == needle)
}
