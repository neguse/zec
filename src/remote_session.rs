//! Terminal-facing remote connection parsing, authentication, and bootstrap.
//!
//! Zed's `remote` crate remains the transport and protocol authority. This
//! module only supplies zec's non-GUI command line, an in-terminal askpass
//! delegate, and locally verified remote-server asset selection.

use std::{
    collections::BTreeMap,
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{Context as _, Result, bail, ensure};
use askpass::EncryptedPassword;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::channel::oneshot;
use gpui::{App, AsyncApp, Entity, Task};
use http_client::HttpClient;
use ratatui::{
    Frame,
    layout::{Alignment, Position, Rect},
    style::{Modifier, Style},
    text::{Line, Text},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use release_channel::ReleaseChannel;
use remote::{
    ConnectionIdentifier, DockerConnectionOptions, RemoteClient, RemoteClientDelegate,
    RemoteConnectionOptions, RemotePlatform, SshConnectionOptions, WslConnectionOptions,
};
use semver::Version;
use zeroize::Zeroize;

use crate::terminal::{TerminalEvent, ZecTerminal};

const MAX_REMOTE_PROMPT_BYTES: usize = 16 * 1024;
const MAX_REMOTE_ARCHIVE_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RemoteRequest {
    pub(crate) options: RemoteConnectionOptions,
    pub(crate) paths: Vec<PathBuf>,
}

impl RemoteRequest {
    pub(crate) fn display_name(&self) -> String {
        self.options.display_name()
    }

    pub(crate) fn persistence_key(&self) -> String {
        remote::remote_connection_identity(&self.options).persistence_key()
    }
}

pub(crate) fn parse_remote_command(arguments: &[OsString]) -> Result<RemoteRequest> {
    let transport = arguments
        .first()
        .and_then(|argument| argument.to_str())
        .context("remote requires one of: ssh, wsl, container")?;
    let arguments = arguments
        .iter()
        .skip(1)
        .map(|argument| {
            argument
                .clone()
                .into_string()
                .map_err(|_| anyhow::anyhow!("remote arguments must be UTF-8"))
        })
        .collect::<Result<Vec<_>>>()?;
    match transport {
        "ssh" => parse_ssh(arguments),
        "wsl" => parse_wsl(arguments),
        "container" | "docker" | "podman" => parse_container(arguments, transport == "podman"),
        _ => bail!("unknown remote transport: {transport}"),
    }
}

fn take_value(arguments: &[String], index: &mut usize, option: &str) -> Result<String> {
    *index = index.saturating_add(1);
    arguments
        .get(*index)
        .cloned()
        .with_context(|| format!("{option} requires a value"))
}

fn parse_ssh(arguments: Vec<String>) -> Result<RemoteRequest> {
    let mut username = None;
    let mut port = None;
    let mut args = Vec::new();
    let mut timeout = None;
    let mut nickname = None;
    let mut upload_binary = true;
    let mut positional_only = false;
    let mut positional = Vec::new();
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        if !positional_only && argument == "--" {
            positional_only = true;
        } else if !positional_only && argument.starts_with('-') {
            match argument.as_str() {
                "--user" => username = Some(take_value(&arguments, &mut index, "--user")?),
                "--port" => {
                    port = Some(
                        take_value(&arguments, &mut index, "--port")?
                            .parse::<u16>()
                            .context("--port must be an integer from 1 through 65535")?,
                    )
                }
                "--arg" => args.push(take_value(&arguments, &mut index, "--arg")?),
                "--timeout" => {
                    timeout = Some(
                        take_value(&arguments, &mut index, "--timeout")?
                            .parse::<u16>()
                            .context("--timeout must be an integer number of seconds")?,
                    )
                }
                "--nickname" => nickname = Some(take_value(&arguments, &mut index, "--nickname")?),
                "--no-upload" => upload_binary = false,
                "--password" => bail!("passwords are never accepted on the command line"),
                _ => bail!("unknown ssh remote option: {argument}"),
            }
        } else {
            positional.push(argument.clone());
        }
        index += 1;
    }
    ensure!(
        positional.len() >= 2,
        "remote ssh requires HOST PATH [PATH ...]"
    );
    let host = positional.remove(0);
    ensure!(!host.trim().is_empty(), "SSH host cannot be empty");
    ensure!(port != Some(0), "--port must be greater than zero");
    ensure!(timeout != Some(0), "--timeout must be greater than zero");
    let paths = validated_remote_paths(positional, RemotePathRequirement::Either)?;
    Ok(RemoteRequest {
        options: SshConnectionOptions {
            host: host.into(),
            username,
            port,
            password: None,
            args: (!args.is_empty()).then_some(args),
            port_forwards: None,
            connection_timeout: timeout,
            nickname,
            upload_binary_over_ssh: upload_binary,
        }
        .into(),
        paths,
    })
}

fn parse_wsl(arguments: Vec<String>) -> Result<RemoteRequest> {
    let mut user = None;
    let mut positional_only = false;
    let mut positional = Vec::new();
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        if !positional_only && argument == "--" {
            positional_only = true;
        } else if !positional_only && argument.starts_with('-') {
            match argument.as_str() {
                "--user" => user = Some(take_value(&arguments, &mut index, "--user")?),
                _ => bail!("unknown WSL remote option: {argument}"),
            }
        } else {
            positional.push(argument.clone());
        }
        index += 1;
    }
    ensure!(
        positional.len() >= 2,
        "remote wsl requires DISTRO PATH [PATH ...]"
    );
    let distro_name = positional.remove(0);
    ensure!(
        !distro_name.trim().is_empty(),
        "WSL distribution cannot be empty"
    );
    let paths = validated_remote_paths(positional, RemotePathRequirement::Unix)?;
    Ok(RemoteRequest {
        options: WslConnectionOptions { distro_name, user }.into(),
        paths,
    })
}

fn parse_container(arguments: Vec<String>, podman_alias: bool) -> Result<RemoteRequest> {
    let mut container_id = None;
    let mut remote_user = None;
    let mut use_podman = podman_alias;
    let mut upload_binary = true;
    let mut remote_env = BTreeMap::new();
    let mut positional_only = false;
    let mut positional = Vec::new();
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        if !positional_only && argument == "--" {
            positional_only = true;
        } else if !positional_only && argument.starts_with('-') {
            match argument.as_str() {
                "--id" => container_id = Some(take_value(&arguments, &mut index, "--id")?),
                "--user" => remote_user = Some(take_value(&arguments, &mut index, "--user")?),
                "--podman" => use_podman = true,
                "--docker" => use_podman = false,
                "--no-upload" => upload_binary = false,
                "--env" => {
                    let value = take_value(&arguments, &mut index, "--env")?;
                    let (name, value) =
                        value.split_once('=').context("--env requires NAME=VALUE")?;
                    ensure!(
                        valid_environment_name(name),
                        "invalid environment name: {name}"
                    );
                    ensure!(
                        remote_env
                            .insert(name.to_owned(), value.to_owned())
                            .is_none(),
                        "duplicate remote environment name: {name}"
                    );
                }
                _ => bail!("unknown container remote option: {argument}"),
            }
        } else {
            positional.push(argument.clone());
        }
        index += 1;
    }
    ensure!(
        positional.len() >= 2,
        "remote container requires NAME PATH [PATH ...]"
    );
    let name = positional.remove(0);
    ensure!(!name.trim().is_empty(), "container name cannot be empty");
    let container_id = container_id.unwrap_or_else(|| name.clone());
    let remote_user = remote_user.unwrap_or_default();
    let paths = validated_remote_paths(positional, RemotePathRequirement::Unix)?;
    Ok(RemoteRequest {
        options: RemoteConnectionOptions::Docker(DockerConnectionOptions {
            name,
            container_id,
            remote_user,
            upload_binary_over_docker_exec: upload_binary,
            use_podman,
            remote_env,
        }),
        paths,
    })
}

fn valid_environment_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

#[derive(Clone, Copy)]
enum RemotePathRequirement {
    Unix,
    Either,
}

fn validated_remote_paths(
    paths: Vec<String>,
    requirement: RemotePathRequirement,
) -> Result<Vec<PathBuf>> {
    paths
        .into_iter()
        .map(|path| {
            ensure!(!path.contains('\0'), "remote path contains NUL");
            let absolute = path.starts_with('/')
                || matches!(requirement, RemotePathRequirement::Either)
                    && is_windows_absolute(&path);
            ensure!(absolute, "remote path must be absolute: {path}");
            Ok(PathBuf::from(path))
        })
        .collect()
}

fn is_windows_absolute(path: &str) -> bool {
    path.starts_with(['\\', '/'])
        || path.as_bytes().get(1) == Some(&b':')
            && path.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
            && path
                .as_bytes()
                .get(2)
                .is_some_and(|separator| matches!(separator, b'\\' | b'/'))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RemotePromptSnapshot {
    pub(crate) prompt: String,
    pub(crate) display_input: String,
    pub(crate) cursor: usize,
    pub(crate) masked: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RemoteUiSnapshot {
    pub(crate) status: Option<String>,
    pub(crate) prompt: Option<RemotePromptSnapshot>,
}

struct PendingPrompt {
    generation: u64,
    prompt: String,
    input: String,
    masked: bool,
    sender: Option<oneshot::Sender<EncryptedPassword>>,
}

impl Drop for PendingPrompt {
    fn drop(&mut self) {
        self.input.zeroize();
    }
}

#[derive(Default)]
struct RemoteUiState {
    status: Option<String>,
    prompt: Option<PendingPrompt>,
    next_generation: u64,
}

#[derive(Clone)]
pub(crate) struct TerminalRemoteDelegate {
    state: Arc<Mutex<RemoteUiState>>,
    event_sender: async_channel::Sender<TerminalEvent>,
    http: Arc<dyn HttpClient>,
}

impl TerminalRemoteDelegate {
    pub(crate) fn new(
        event_sender: async_channel::Sender<TerminalEvent>,
        http: Arc<dyn HttpClient>,
    ) -> Arc<Self> {
        Arc::new(Self {
            state: Arc::new(Mutex::new(RemoteUiState::default())),
            event_sender,
            http,
        })
    }

    pub(crate) fn snapshot(&self) -> RemoteUiSnapshot {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let prompt = state.prompt.as_ref().map(|prompt| {
            let display_input = if prompt.masked {
                "•".repeat(prompt.input.chars().count())
            } else {
                prompt.input.clone()
            };
            RemotePromptSnapshot {
                prompt: prompt.prompt.clone(),
                cursor: display_input.chars().count(),
                display_input,
                masked: prompt.masked,
            }
        });
        RemoteUiSnapshot {
            status: state.status.clone(),
            prompt,
        }
    }

    pub(crate) fn has_prompt(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .prompt
            .is_some()
    }

    pub(crate) fn handle_key(&self, event: &KeyEvent) -> Result<bool> {
        if event.kind == KeyEventKind::Release {
            return Ok(self.has_prompt());
        }
        let mut submitted = None;
        let handled = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(prompt) = state.prompt.as_mut() else {
                return Ok(false);
            };
            match (event.code, event.modifiers) {
                (KeyCode::Esc, _) => {
                    state.prompt = None;
                }
                (KeyCode::Enter, _) => {
                    submitted = state.prompt.take();
                }
                (KeyCode::Backspace, _) => {
                    prompt.input.pop();
                }
                (KeyCode::Char('u'), KeyModifiers::CONTROL) => prompt.input.zeroize(),
                (KeyCode::Char(character), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                    let additional = character.len_utf8();
                    ensure!(
                        prompt.input.len().saturating_add(additional) <= MAX_REMOTE_PROMPT_BYTES,
                        "remote authentication response is too large"
                    );
                    prompt.input.push(character);
                }
                _ => {}
            }
            true
        };
        if let Some(mut prompt) = submitted {
            let encrypted = EncryptedPassword::try_from(prompt.input.as_str())
                .context("could not protect authentication response")?;
            prompt.input.zeroize();
            if let Some(sender) = prompt.sender.take() {
                let _ = sender.send(encrypted);
            }
        }
        self.notify();
        Ok(handled)
    }

    pub(crate) fn handle_paste(&self, text: &str) -> Result<bool> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(prompt) = state.prompt.as_mut() else {
            return Ok(false);
        };
        let text = text.replace(['\r', '\n'], "");
        ensure!(
            prompt.input.len().saturating_add(text.len()) <= MAX_REMOTE_PROMPT_BYTES,
            "remote authentication response is too large"
        );
        prompt.input.push_str(&text);
        drop(state);
        self.notify();
        Ok(true)
    }

    pub(crate) fn cancel_prompt(&self) -> bool {
        let cancelled = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .prompt
            .take()
            .is_some();
        if cancelled {
            self.notify();
        }
        cancelled
    }

    fn notify(&self) {
        let _ = self.event_sender.try_send(TerminalEvent::RemoteChanged);
    }
}

impl RemoteClientDelegate for TerminalRemoteDelegate {
    fn ask_password(
        &self,
        prompt: String,
        sender: oneshot::Sender<EncryptedPassword>,
        cancellation: oneshot::Receiver<()>,
        cx: &mut AsyncApp,
    ) {
        let generation = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.next_generation = state.next_generation.saturating_add(1);
            let generation = state.next_generation;
            state.prompt = Some(PendingPrompt {
                generation,
                masked: prompt_should_be_masked(&prompt),
                prompt,
                input: String::new(),
                sender: Some(sender),
            });
            generation
        };
        self.notify();
        let state = self.state.clone();
        let event_sender = self.event_sender.clone();
        cx.spawn(async move |_cx| {
            let _ = cancellation.await;
            let mut state = state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if state
                .prompt
                .as_ref()
                .is_some_and(|prompt| prompt.generation == generation)
            {
                state.prompt = None;
                drop(state);
                let _ = event_sender.try_send(TerminalEvent::RemoteChanged);
            }
        })
        .detach();
    }

    fn get_download_url(
        &self,
        _platform: RemotePlatform,
        _release_channel: ReleaseChannel,
        _version: Option<Version>,
        _cx: &mut AsyncApp,
    ) -> Task<Result<Option<String>>> {
        // A remote host never receives an unverified URL from zec. The local
        // archive path is checked first and uploaded through Zed's transport.
        Task::ready(Ok(None))
    }

    fn download_server_binary_locally(
        &self,
        platform: RemotePlatform,
        _release_channel: ReleaseChannel,
        version: Option<Version>,
        cx: &mut AsyncApp,
    ) -> Task<Result<PathBuf>> {
        match resolve_remote_server_archive(platform) {
            Ok(path) => Task::ready(Ok(path)),
            Err(error) if env::var_os("ZEC_REMOTE_SERVER_ARCHIVE").is_some() => {
                Task::ready(Err(error))
            }
            Err(_) => {
                let http = self.http.clone();
                cx.spawn(async move |_cx| {
                    let version = version
                        .context("this release channel requires ZEC_REMOTE_SERVER_ARCHIVE")?;
                    crate::update::acquire_remote_server_archive(platform, version, http).await
                })
            }
        }
    }

    fn set_status(&self, status: Option<&str>, _cx: &mut AsyncApp) {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .status = status.map(str::to_owned);
        self.notify();
    }
}

fn prompt_should_be_masked(prompt: &str) -> bool {
    let prompt = prompt.to_ascii_lowercase();
    !(prompt.contains("yes/no")
        || prompt.contains("yes/no/[fingerprint]")
        || prompt.contains("are you sure")
        || prompt.contains("continue connecting"))
}

fn resolve_remote_server_archive(platform: RemotePlatform) -> Result<PathBuf> {
    let expected_extension = if platform.os.is_windows() {
        "zip"
    } else {
        "gz"
    };
    let candidate = if let Some(path) = env::var_os("ZEC_REMOTE_SERVER_ARCHIVE") {
        PathBuf::from(path)
    } else {
        let executable = env::current_exe().context("locate zec executable")?;
        let directory = executable
            .parent()
            .context("zec executable has no directory")?;
        directory.join(format!(
            "zec-remote-server-{}-{}.{}",
            platform.os, platform.arch, expected_extension
        ))
    };
    validate_remote_server_archive(&candidate, expected_extension)?;
    Ok(candidate)
}

fn validate_remote_server_archive(path: &Path, expected_extension: &str) -> Result<()> {
    ensure!(
        path.extension().and_then(|extension| extension.to_str()) == Some(expected_extension),
        "remote server archive must use .{expected_extension}: {}",
        path.display()
    );
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect remote server archive {}", path.display()))?;
    ensure!(
        metadata.file_type().is_file(),
        "remote server archive is not a regular file"
    );
    ensure!(metadata.len() > 0, "remote server archive is empty");
    ensure!(
        metadata.len() <= MAX_REMOTE_ARCHIVE_BYTES,
        "remote server archive is too large"
    );
    Ok(())
}

pub(crate) fn start_connection(
    request: &RemoteRequest,
    delegate: Arc<TerminalRemoteDelegate>,
    cx: &mut App,
) -> Task<Result<Option<Entity<RemoteClient>>>> {
    let options = request.options.clone();
    cx.spawn(async move |cx| {
        let connection = remote::connect(options, delegate.clone(), cx).await?;
        let (cancellation_sender, cancellation) = oneshot::channel();
        let task = cx.update(|cx| {
            RemoteClient::new(
                ConnectionIdentifier::setup(),
                connection,
                cancellation,
                delegate,
                cx,
            )
        });
        let result = task.await;
        drop(cancellation_sender);
        result
    })
}

pub(crate) fn draw_connection_screen(
    terminal: &mut ZecTerminal,
    remote_name: &str,
    snapshot: &RemoteUiSnapshot,
) -> std::io::Result<()> {
    terminal
        .try_draw(|frame| -> std::io::Result<()> {
            let area = frame.area();
            frame.render_widget(Clear, area);
            let block = Block::default().borders(Borders::ALL).title(" zec remote ");
            frame.render_widget(block, area);
            let inner = Rect::new(
                area.x.saturating_add(2),
                area.y.saturating_add(2),
                area.width.saturating_sub(4),
                area.height.saturating_sub(4),
            );
            let mut lines = vec![
                Line::styled(
                    format!("Connecting to {remote_name}"),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Line::raw(snapshot.status.as_deref().unwrap_or("Starting transport…")),
            ];
            if let Some(prompt) = &snapshot.prompt {
                lines.push(Line::raw(""));
                lines.push(Line::raw(bounded_line(
                    &prompt.prompt,
                    usize::from(inner.width),
                )));
                lines.push(Line::raw(format!("> {}", prompt.display_input)));
                lines.push(Line::raw(if prompt.masked {
                    "Enter submit · Esc cancel · input is masked"
                } else {
                    "Enter submit · Esc cancel"
                }));
            } else {
                lines.push(Line::raw(""));
                lines.push(Line::raw("Ctrl-Q or Esc cancels"));
            }
            frame.render_widget(
                Paragraph::new(Text::from(lines))
                    .alignment(Alignment::Left)
                    .wrap(Wrap { trim: false }),
                inner,
            );
            if let Some(prompt) = &snapshot.prompt {
                let cursor_x = inner
                    .x
                    .saturating_add(2)
                    .saturating_add(u16::try_from(prompt.cursor).unwrap_or(u16::MAX))
                    .min(inner.right().saturating_sub(1));
                let cursor_y = inner
                    .y
                    .saturating_add(4)
                    .min(inner.bottom().saturating_sub(1));
                frame.set_cursor_position(Position::new(cursor_x, cursor_y));
            }
            Ok(())
        })
        .map(|_| ())
}

pub(crate) fn render_auth_overlay(frame: &mut Frame<'_>, snapshot: &RemoteUiSnapshot, outer: Rect) {
    let Some(prompt) = &snapshot.prompt else {
        return;
    };
    let width = outer.width.saturating_sub(4).min(92).max(12);
    let height = 7.min(outer.height.saturating_sub(2).max(3));
    let area = Rect::new(
        outer
            .x
            .saturating_add(outer.width.saturating_sub(width) / 2),
        outer
            .y
            .saturating_add(outer.height.saturating_sub(height) / 2),
        width,
        height,
    );
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(vec![
            Line::raw(bounded_line(
                &prompt.prompt,
                usize::from(width.saturating_sub(2)),
            )),
            Line::raw(format!("> {}", prompt.display_input)),
            Line::raw(if prompt.masked {
                "Enter submit · Esc cancel · masked"
            } else {
                "Enter submit · Esc cancel"
            }),
        ])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" remote authentication "),
        )
        .wrap(Wrap { trim: false }),
        area,
    );
    let cursor_x = area
        .x
        .saturating_add(3)
        .saturating_add(u16::try_from(prompt.cursor).unwrap_or(u16::MAX))
        .min(area.right().saturating_sub(2));
    frame.set_cursor_position(Position::new(cursor_x, area.y.saturating_add(2)));
}

fn bounded_line(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut output = text
        .replace(['\r', '\n', '\0'], " ")
        .chars()
        .take(width)
        .collect::<String>();
    if text.chars().count() > width && width >= 1 {
        output.pop();
        output.push('…');
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn parses_ssh_without_a_cli_password() {
        let request = parse_remote_command(&args(&[
            "ssh",
            "host.example",
            "--user",
            "dev",
            "--port",
            "2222",
            "--arg",
            "-oStrictHostKeyChecking=yes",
            "/srv/project",
            "/srv/project/README.md",
        ]))
        .unwrap();
        assert_eq!(request.paths[0], PathBuf::from("/srv/project"));
        let RemoteConnectionOptions::Ssh(options) = request.options else {
            panic!("expected SSH options")
        };
        assert_eq!(options.host.to_string(), "host.example");
        assert_eq!(options.username.as_deref(), Some("dev"));
        assert_eq!(options.port, Some(2222));
        assert_eq!(options.password, None);
        assert!(options.upload_binary_over_ssh);
    }

    #[test]
    fn rejects_password_and_relative_remote_paths() {
        assert!(
            parse_remote_command(&args(&["ssh", "host", "--password", "secret", "/x"]))
                .unwrap_err()
                .to_string()
                .contains("never accepted")
        );
        assert!(parse_remote_command(&args(&["wsl", "Ubuntu", "relative"])).is_err());
    }

    #[test]
    fn parses_container_identity_environment_and_podman() {
        let request = parse_remote_command(&args(&[
            "container",
            "web",
            "--id",
            "deadbeef",
            "--user",
            "app",
            "--podman",
            "--env",
            "RUST_LOG=debug",
            "/workspace",
        ]))
        .unwrap();
        let RemoteConnectionOptions::Docker(options) = request.options else {
            panic!("expected container options")
        };
        assert_eq!(options.container_id, "deadbeef");
        assert_eq!(options.remote_user, "app");
        assert!(options.use_podman);
        assert_eq!(options.remote_env["RUST_LOG"], "debug");
    }

    #[test]
    fn recognizes_unmasked_confirmation_prompts() {
        assert!(!prompt_should_be_masked(
            "Are you sure you want to continue connecting (yes/no/[fingerprint])?"
        ));
        assert!(prompt_should_be_masked("dev@host's password:"));
    }
}
