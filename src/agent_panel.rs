//! Terminal projection for Zed's ACP-backed agent thread.
//!
//! The conversation, tool calls, permissions, cancellation, project context,
//! and MCP hand-off remain owned by [`acp_thread::AcpThread`]. This module owns
//! only bounded terminal input and a cell-oriented projection of that model.

use std::{collections::BTreeMap, env, path::PathBuf, rc::Rc, sync::Arc, time::Duration};

use acp_thread::{
    AcpThread, AgentModelList, AgentSessionListRequest, AgentThreadEntry, PermissionOptions,
    SelectedPermissionOutcome, ThreadStatus, ToolCallStatus,
};
use agent::{NativeAgentServer, ThreadStore};
use agent_client_protocol::schema::v1 as acp;
use agent_servers::{AcpConnection, AgentServer as _, AgentServerDelegate};
use anyhow::{Context as _, Result, anyhow, ensure};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use gpui::{AsyncApp, Entity};
use project::{AgentId, Project, ProjectPath, agent_server_store::AgentServerCommand};
use ratatui::{
    buffer::Buffer,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Clear, Widget},
};
use serde::Deserialize;
use unicode_width::UnicodeWidthChar as _;
use util::rel_path::RelPath;
use zed_fs::Fs;

pub(crate) const ACP_AGENT_ENV: &str = "ZEC_ACP_AGENT";
const MAX_CONFIG_BYTES: usize = 256 * 1024;
const MAX_ARGUMENTS: usize = 256;
const MAX_ENVIRONMENT_VARIABLES: usize = 256;
const MAX_CONFIG_VALUE_BYTES: usize = 32 * 1024;
const MAX_PROMPT_BYTES: usize = 64 * 1024;
const MAX_CONVERSATION_BYTES: usize = 256 * 1024;
const MAX_CONVERSATION_ENTRIES: usize = 256;
const MAX_LOCAL_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_LOCAL_OUTPUT_ENTRIES: usize = 64;
const MAX_PROMPT_ROWS: usize = 4;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AgentLaunchConfig {
    pub(crate) command: PathBuf,
    #[serde(default)]
    pub(crate) args: Vec<String>,
    #[serde(default)]
    pub(crate) env: BTreeMap<String, String>,
    #[serde(default = "default_agent_id")]
    pub(crate) id: String,
}

fn default_agent_id() -> String {
    "zec-external".to_owned()
}

impl AgentLaunchConfig {
    fn parse(json: &str) -> Result<Self> {
        ensure!(
            json.len() <= MAX_CONFIG_BYTES,
            "{ACP_AGENT_ENV} exceeds {MAX_CONFIG_BYTES} bytes"
        );
        let config: Self =
            serde_json::from_str(json).with_context(|| format!("parse {ACP_AGENT_ENV} as JSON"))?;
        ensure!(
            !config.command.as_os_str().is_empty(),
            "{ACP_AGENT_ENV}.command must not be empty"
        );
        ensure!(
            config.args.len() <= MAX_ARGUMENTS,
            "{ACP_AGENT_ENV}.args exceeds {MAX_ARGUMENTS} entries"
        );
        ensure!(
            config.env.len() <= MAX_ENVIRONMENT_VARIABLES,
            "{ACP_AGENT_ENV}.env exceeds {MAX_ENVIRONMENT_VARIABLES} entries"
        );
        ensure!(
            !config.id.trim().is_empty() && config.id.len() <= 128,
            "{ACP_AGENT_ENV}.id must contain 1..=128 bytes"
        );
        ensure!(
            config.args.iter().all(
                |argument| argument.len() <= MAX_CONFIG_VALUE_BYTES && !argument.contains('\0')
            ),
            "{ACP_AGENT_ENV}.args contains an oversized or NUL-bearing value"
        );
        ensure!(
            config.env.iter().all(|(name, value)| {
                !name.is_empty()
                    && name.len() <= MAX_CONFIG_VALUE_BYTES
                    && value.len() <= MAX_CONFIG_VALUE_BYTES
                    && !name.contains(['=', '\0'])
                    && !value.contains('\0')
            }),
            "{ACP_AGENT_ENV}.env contains an invalid name or value"
        );
        Ok(config)
    }

    pub(crate) fn display_name(&self) -> &str {
        &self.id
    }
}

pub(crate) fn launch_config_from_environment() -> Result<Option<AgentLaunchConfig>> {
    env::var(ACP_AGENT_ENV)
        .ok()
        .map(|json| AgentLaunchConfig::parse(&json))
        .transpose()
}

pub(crate) async fn connect(
    config: Option<&AgentLaunchConfig>,
    project: Entity<Project>,
    file_system: std::sync::Arc<dyn Fs>,
    cx: &mut AsyncApp,
) -> Result<Entity<AcpThread>> {
    if let Some(config) = config {
        return connect_external(config, project, cx).await;
    }

    let (work_dirs, connection_task) = cx.update(|cx| {
        let work_dirs = project.read(cx).default_path_list(cx);
        let delegate =
            AgentServerDelegate::new(project.read(cx).agent_server_store().clone(), None, None);
        let server = NativeAgentServer::new(file_system, ThreadStore::global(cx));
        let connection_task = server.connect(delegate, project.clone(), cx);
        (work_dirs, connection_task)
    });
    let connection = connection_task
        .await
        .context("start in-process Zed Agent")?;
    let session = cx.update(|cx| connection.new_session(project, work_dirs, cx));
    session.await.context("create Zed Agent session")
}

async fn connect_external(
    config: &AgentLaunchConfig,
    project: Entity<Project>,
    cx: &mut AsyncApp,
) -> Result<Entity<AcpThread>> {
    let (agent_server_store, work_dirs) = project.read_with(cx, |project, cx| {
        (
            project.agent_server_store().downgrade(),
            project.default_path_list(cx),
        )
    });
    let command = AgentServerCommand {
        path: config.command.clone(),
        args: config.args.clone(),
        env: Some(
            config
                .env
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
        ),
    };
    let connection = AcpConnection::stdio(
        AgentId::new(config.id.clone()),
        project.clone(),
        command,
        agent_server_store,
        None,
        Default::default(),
        cx,
    )
    .await
    .with_context(|| format!("start ACP agent {}", config.id))?;
    let connection: Rc<dyn acp_thread::AgentConnection> = Rc::new(connection);
    let session = cx.update(|cx| connection.new_session(project, work_dirs, cx));
    session
        .await
        .with_context(|| format!("create ACP session for {}", config.id))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AgentPhase {
    Disconnected,
    Connecting,
    Ready,
    Error(String),
}

impl AgentPhase {
    fn label(&self) -> &str {
        match self {
            Self::Disconnected => "disconnected",
            Self::Connecting => "connecting",
            Self::Ready => "ready",
            Self::Error(_) => "error",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AgentInput {
    Consumed,
    Submit(String),
    FocusEditor,
    CancelGeneration,
    NewSession,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AgentLocalCommand {
    Help,
    Commands,
    Models,
    SetModel(String),
    Modes,
    SetMode(String),
    Config,
    SetConfig { id: String, value: String },
    Sessions,
    LoadSession(String),
    CloseSession(String),
    Skills,
    Instructions,
    Mcp,
    Auth,
    Authenticate(String),
    Logout,
    Clear,
    NewSession,
    Cancel,
    Invalid(String),
}

#[derive(Debug)]
pub(crate) enum AgentCommandEffect {
    Output(String),
    ReplaceThread(Entity<AcpThread>),
    TerminalAuthentication {
        method_id: acp::AuthMethodId,
        task: task::SpawnInTerminal,
    },
    Clear,
    NewSession,
    Cancel,
}

pub(crate) fn parse_local_command(prompt: &str) -> Option<AgentLocalCommand> {
    let mut words = prompt.trim().split_whitespace();
    let command = words.next()?;
    let argument = words.next().map(str::to_owned);
    let second_argument = words.next().map(str::to_owned);
    let trailing = words.next().is_some();
    let no_arguments =
        |name: &str| AgentLocalCommand::Invalid(format!("/{name} takes no arguments"));
    let parsed = match command {
        "/help" if argument.is_none() => AgentLocalCommand::Help,
        "/commands" if argument.is_none() => AgentLocalCommand::Commands,
        "/models" if argument.is_none() => AgentLocalCommand::Models,
        "/model" if argument.is_some() && second_argument.is_none() => {
            AgentLocalCommand::SetModel(argument.unwrap())
        }
        "/modes" if argument.is_none() => AgentLocalCommand::Modes,
        "/mode" if argument.is_some() && second_argument.is_none() => {
            AgentLocalCommand::SetMode(argument.unwrap())
        }
        "/config" if argument.is_none() => AgentLocalCommand::Config,
        "/config" if argument.is_some() && second_argument.is_some() && !trailing => {
            AgentLocalCommand::SetConfig {
                id: argument.unwrap(),
                value: second_argument.unwrap(),
            }
        }
        "/config" => AgentLocalCommand::Invalid("usage: /config <id> <value>".to_owned()),
        "/sessions" if argument.is_none() => AgentLocalCommand::Sessions,
        "/load" if argument.is_some() && second_argument.is_none() => {
            AgentLocalCommand::LoadSession(argument.unwrap())
        }
        "/close" if argument.is_some() && second_argument.is_none() => {
            AgentLocalCommand::CloseSession(argument.unwrap())
        }
        "/skills" if argument.is_none() => AgentLocalCommand::Skills,
        "/instructions" if argument.is_none() => AgentLocalCommand::Instructions,
        "/mcp" if argument.is_none() => AgentLocalCommand::Mcp,
        "/auth" if argument.is_none() => AgentLocalCommand::Auth,
        "/auth" if argument.is_some() && second_argument.is_none() => {
            AgentLocalCommand::Authenticate(argument.unwrap())
        }
        "/auth" => AgentLocalCommand::Invalid("usage: /auth [method-id]".to_owned()),
        "/logout" if argument.is_none() => AgentLocalCommand::Logout,
        "/clear" if argument.is_none() => AgentLocalCommand::Clear,
        "/new" if argument.is_none() => AgentLocalCommand::NewSession,
        "/cancel" if argument.is_none() => AgentLocalCommand::Cancel,
        "/help" => no_arguments("help"),
        "/commands" => no_arguments("commands"),
        "/models" => no_arguments("models"),
        "/model" => AgentLocalCommand::Invalid("usage: /model <id>".to_owned()),
        "/modes" => no_arguments("modes"),
        "/mode" => AgentLocalCommand::Invalid("usage: /mode <id>".to_owned()),
        "/sessions" => no_arguments("sessions"),
        "/load" => AgentLocalCommand::Invalid("usage: /load <session-id>".to_owned()),
        "/close" => AgentLocalCommand::Invalid("usage: /close <session-id>".to_owned()),
        "/skills" => no_arguments("skills"),
        "/instructions" => no_arguments("instructions"),
        "/mcp" => no_arguments("mcp"),
        "/logout" => no_arguments("logout"),
        "/clear" => no_arguments("clear"),
        "/new" => no_arguments("new"),
        "/cancel" => no_arguments("cancel"),
        _ => return None,
    };
    Some(parsed)
}

pub(crate) async fn execute_local_command(
    command: AgentLocalCommand,
    thread: Option<&Entity<AcpThread>>,
    project: Entity<Project>,
    file_system: Arc<dyn Fs>,
    cx: &mut AsyncApp,
) -> Result<AgentCommandEffect> {
    match command {
        AgentLocalCommand::Help => {
            return Ok(AgentCommandEffect::Output(
                "zec Agent commands\n\
                 /commands             ACP slash commands\n\
                 /models | /model ID   list/select language model\n\
                 /modes  | /mode ID    list/select session mode\n\
                 /config | /config ID VALUE\n\
                 /sessions | /load ID | /close ID\n\
                 /skills | /instructions | /mcp\n\
                 /auth [ID] | /logout  authentication\n\
                 /new | /cancel | /clear\n\
                 Unknown slash commands are sent to the Agent."
                    .to_owned(),
            ));
        }
        AgentLocalCommand::Invalid(message) => {
            return Ok(AgentCommandEffect::Output(format!("error: {message}")));
        }
        AgentLocalCommand::Clear => return Ok(AgentCommandEffect::Clear),
        AgentLocalCommand::NewSession => return Ok(AgentCommandEffect::NewSession),
        AgentLocalCommand::Cancel => return Ok(AgentCommandEffect::Cancel),
        AgentLocalCommand::Mcp => {
            let lines = project.read_with(cx, |project, cx| {
                let store = project.context_server_store().read(cx);
                let ids = store.configured_server_ids();
                if ids.is_empty() {
                    return vec!["MCP: no enabled servers configured".to_owned()];
                }
                let mut lines = vec![format!("MCP servers ({})", ids.len())];
                for id in ids {
                    let status = store
                        .status_for_server(&id)
                        .map_or_else(|| "configured".to_owned(), |status| format!("{status:?}"));
                    lines.push(format!("- {id}: {status}"));
                }
                lines
            });
            return Ok(AgentCommandEffect::Output(lines.join("\n")));
        }
        AgentLocalCommand::Instructions => {
            return Ok(AgentCommandEffect::Output(
                format_agent_instructions(project, file_system, cx).await?,
            ));
        }
        _ => {}
    }

    let thread = thread.context("agent is not connected")?;
    let (connection, session_id, work_dirs) = thread.read_with(cx, |thread, cx| {
        let work_dirs = thread
            .work_dirs()
            .cloned()
            .unwrap_or_else(|| project.read(cx).default_path_list(cx));
        (
            thread.connection().clone(),
            thread.session_id().clone(),
            work_dirs,
        )
    });

    match command {
        AgentLocalCommand::Commands => {
            let commands = thread.read_with(cx, |thread, _cx| thread.available_commands().to_vec());
            let output = if commands.is_empty() {
                "ACP commands: none advertised".to_owned()
            } else {
                let mut lines = vec![format!("ACP commands ({})", commands.len())];
                lines.extend(
                    commands
                        .into_iter()
                        .map(|command| format!("- /{} — {}", command.name, command.description)),
                );
                lines.join("\n")
            };
            Ok(AgentCommandEffect::Output(output))
        }
        AgentLocalCommand::Models => {
            if let Some(selector) = connection.model_selector(&session_id) {
                let task = cx.update(|cx| selector.list_models(cx));
                let models = task.await.context("list Agent models")?;
                Ok(AgentCommandEffect::Output(format_models(models)))
            } else {
                let provider = cx
                    .update(|cx| connection.session_config_options(&session_id, cx))
                    .context("this Agent does not expose model selection")?;
                let option = config_option_for_category(
                    &provider.config_options(),
                    acp::SessionConfigOptionCategory::Model,
                )
                .context("this Agent does not expose a model config option")?;
                Ok(AgentCommandEffect::Output(format_select_option(
                    "models", &option,
                )))
            }
        }
        AgentLocalCommand::SetModel(id) => {
            if let Some(selector) = connection.model_selector(&session_id) {
                let task = cx.update(|cx| selector.select_model(id.clone().into(), cx));
                task.await
                    .with_context(|| format!("select Agent model {id}"))?;
            } else {
                let provider = cx
                    .update(|cx| connection.session_config_options(&session_id, cx))
                    .context("this Agent does not expose model selection")?;
                let option = config_option_for_category(
                    &provider.config_options(),
                    acp::SessionConfigOptionCategory::Model,
                )
                .context("this Agent does not expose a model config option")?;
                ensure!(
                    select_option_values(&option)
                        .iter()
                        .any(|value| value == &id),
                    "unknown Agent model {id}"
                );
                let task = cx.update(|cx| {
                    provider.set_config_option(
                        option.id,
                        acp::SessionConfigOptionValue::value_id(id.clone()),
                        cx,
                    )
                });
                task.await
                    .with_context(|| format!("select Agent model {id}"))?;
            }
            Ok(AgentCommandEffect::Output(format!("model selected: {id}")))
        }
        AgentLocalCommand::Modes => {
            if let Some(modes) = cx.update(|cx| connection.session_modes(&session_id, cx)) {
                let current = modes.current_mode();
                let all = modes.all_modes();
                let mut lines = vec![format!("session modes ({})", all.len())];
                lines.extend(all.into_iter().map(|mode| {
                    let selected = if mode.id == current { "*" } else { " " };
                    let description = mode
                        .description
                        .as_deref()
                        .map_or(String::new(), |description| format!(" — {description}"));
                    format!("{selected} {} ({}){description}", mode.name, mode.id)
                }));
                Ok(AgentCommandEffect::Output(lines.join("\n")))
            } else {
                let provider = cx
                    .update(|cx| connection.session_config_options(&session_id, cx))
                    .context("this Agent does not expose session modes")?;
                let option = config_option_for_category(
                    &provider.config_options(),
                    acp::SessionConfigOptionCategory::Mode,
                )
                .context("this Agent does not expose a mode config option")?;
                Ok(AgentCommandEffect::Output(format_select_option(
                    "session modes",
                    &option,
                )))
            }
        }
        AgentLocalCommand::SetMode(id) => {
            if let Some(modes) = cx.update(|cx| connection.session_modes(&session_id, cx)) {
                let task = cx.update(|cx| modes.set_mode(acp::SessionModeId::new(id.clone()), cx));
                task.await
                    .with_context(|| format!("select Agent mode {id}"))?;
            } else {
                let provider = cx
                    .update(|cx| connection.session_config_options(&session_id, cx))
                    .context("this Agent does not expose session modes")?;
                let option = config_option_for_category(
                    &provider.config_options(),
                    acp::SessionConfigOptionCategory::Mode,
                )
                .context("this Agent does not expose a mode config option")?;
                ensure!(
                    select_option_values(&option)
                        .iter()
                        .any(|value| value == &id),
                    "unknown Agent mode {id}"
                );
                let task = cx.update(|cx| {
                    provider.set_config_option(
                        option.id,
                        acp::SessionConfigOptionValue::value_id(id.clone()),
                        cx,
                    )
                });
                task.await
                    .with_context(|| format!("select Agent mode {id}"))?;
            }
            Ok(AgentCommandEffect::Output(format!("mode selected: {id}")))
        }
        AgentLocalCommand::Config => {
            let provider = cx
                .update(|cx| connection.session_config_options(&session_id, cx))
                .context("this Agent does not expose session config options")?;
            Ok(AgentCommandEffect::Output(format_config_options(
                &provider.config_options(),
            )))
        }
        AgentLocalCommand::SetConfig { id, value } => {
            let provider = cx
                .update(|cx| connection.session_config_options(&session_id, cx))
                .context("this Agent does not expose session config options")?;
            let option = provider
                .config_options()
                .into_iter()
                .find(|option| option.id.0.as_ref() == id)
                .ok_or_else(|| anyhow!("unknown session config option {id}"))?;
            let value = match option.kind {
                acp::SessionConfigKind::Boolean(_) => {
                    acp::SessionConfigOptionValue::boolean(match value.as_str() {
                        "true" | "on" | "yes" | "1" => true,
                        "false" | "off" | "no" | "0" => false,
                        _ => return Err(anyhow!("{id} expects true/false")),
                    })
                }
                acp::SessionConfigKind::Select(_) => {
                    acp::SessionConfigOptionValue::value_id(value.clone())
                }
                _ => return Err(anyhow!("unsupported config shape for {id}")),
            };
            let task = cx.update(|cx| {
                provider.set_config_option(acp::SessionConfigId::new(id.clone()), value, cx)
            });
            let updated = task
                .await
                .with_context(|| format!("set Agent config {id}"))?;
            Ok(AgentCommandEffect::Output(format!(
                "config updated: {id}\n{}",
                format_config_options(&updated)
            )))
        }
        AgentLocalCommand::Sessions => {
            let list = cx
                .update(|cx| connection.session_list(cx))
                .context("this Agent does not expose session history")?;
            let task = cx.update(|cx| {
                list.list_sessions(
                    AgentSessionListRequest {
                        cwd: None,
                        ..Default::default()
                    },
                    cx,
                )
            });
            let response = task.await.context("list Agent sessions")?;
            let mut lines = vec![format!("saved sessions ({})", response.sessions.len())];
            lines.extend(response.sessions.into_iter().map(|session| {
                let title = session.title.as_deref().unwrap_or("untitled");
                format!("- {} — {title}", session.session_id)
            }));
            Ok(AgentCommandEffect::Output(lines.join("\n")))
        }
        AgentLocalCommand::LoadSession(id) => {
            ensure!(
                connection.supports_load_session(),
                "this Agent does not support loading sessions"
            );
            let task = cx.update(|cx| {
                connection.clone().load_session(
                    acp::SessionId::new(id.clone()),
                    project,
                    work_dirs,
                    None,
                    cx,
                )
            });
            let loaded = task
                .await
                .with_context(|| format!("load Agent session {id}"))?;
            Ok(AgentCommandEffect::ReplaceThread(loaded))
        }
        AgentLocalCommand::CloseSession(id) => {
            ensure!(
                connection.supports_close_session(),
                "this Agent does not support closing sessions"
            );
            let closing_current = session_id.0.as_ref() == id;
            let task = cx.update(|cx| {
                connection
                    .clone()
                    .close_session(&acp::SessionId::new(id.clone()), cx)
            });
            task.await
                .with_context(|| format!("close Agent session {id}"))?;
            if closing_current {
                Ok(AgentCommandEffect::NewSession)
            } else {
                Ok(AgentCommandEffect::Output(format!(
                    "Agent session closed: {id}"
                )))
            }
        }
        AgentLocalCommand::Skills => {
            let native = connection
                .clone()
                .downcast::<agent::NativeAgentConnection>()
                .context("skills catalog is available on the native Zed Agent")?;
            let mut skills = cx.update(|cx| {
                native.ensure_skills_scan_started(cx);
                native.refresh_skills_for_project(project.clone(), cx);
                native.available_skills(&session_id, cx)
            });
            // Project skill discovery opens buffers and may finish just after
            // the session is created. Yield briefly so one `/skills` command
            // returns the catalog instead of requiring a manual retry.
            for _ in 0..40 {
                if !skills.is_empty() {
                    break;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
                skills = cx.update(|cx| native.available_skills(&session_id, cx));
            }
            let mut lines = vec![format!("available skills ({})", skills.len())];
            if skills.is_empty() {
                lines.push("no global or trusted project skills discovered".to_owned());
            } else {
                lines.extend(skills.into_iter().map(|skill| {
                    let warning = skill
                        .warning
                        .as_deref()
                        .map_or(String::new(), |warning| format!(" [warning: {warning}]"));
                    format!(
                        "- /{} — {} ({}){warning}",
                        skill.name, skill.description, skill.source
                    )
                }));
            }
            Ok(AgentCommandEffect::Output(lines.join("\n")))
        }
        AgentLocalCommand::Auth => {
            let methods = connection.auth_methods();
            let output = if methods.is_empty() {
                "authentication: not required or no methods advertised".to_owned()
            } else {
                let mut lines = vec![format!("authentication methods ({})", methods.len())];
                lines.extend(methods.iter().map(|method| {
                    let description = method
                        .description()
                        .map_or(String::new(), |description| format!(" — {description}"));
                    format!("- {} ({}){description}", method.name(), method.id())
                }));
                lines.join("\n")
            };
            Ok(AgentCommandEffect::Output(output))
        }
        AgentLocalCommand::Authenticate(id) => {
            ensure!(
                connection
                    .auth_methods()
                    .iter()
                    .any(|method| method.id().0.as_ref() == id),
                "unknown authentication method {id}"
            );
            let method_id = acp::AuthMethodId::new(id.clone());
            if let Some(task) = cx.update(|cx| connection.terminal_auth_task(&method_id, cx)) {
                return Ok(AgentCommandEffect::TerminalAuthentication {
                    method_id,
                    task: task
                        .await
                        .with_context(|| format!("prepare terminal authentication {id}"))?,
                });
            }
            let task = cx.update(|cx| connection.authenticate(method_id, cx));
            task.await
                .with_context(|| format!("authenticate Agent with {id}"))?;
            Ok(AgentCommandEffect::Output(format!(
                "authentication completed: {id}"
            )))
        }
        AgentLocalCommand::Logout => {
            ensure!(
                connection.supports_logout(),
                "this Agent does not support logout"
            );
            let task = cx.update(|cx| connection.logout(cx));
            task.await.context("log out Agent")?;
            Ok(AgentCommandEffect::Output("Agent logged out".to_owned()))
        }
        AgentLocalCommand::Help
        | AgentLocalCommand::Invalid(_)
        | AgentLocalCommand::Mcp
        | AgentLocalCommand::Instructions
        | AgentLocalCommand::Clear
        | AgentLocalCommand::NewSession
        | AgentLocalCommand::Cancel => unreachable!("handled before connection lookup"),
    }
}

async fn format_agent_instructions(
    project: Entity<Project>,
    file_system: Arc<dyn Fs>,
    cx: &mut AsyncApp,
) -> Result<String> {
    const MAX_INSTRUCTION_PREVIEW_BYTES: usize = 16 * 1024;

    let mut sections = Vec::new();
    if let Ok(contents) = file_system.load(paths::agents_file()).await {
        let contents = contents.trim();
        if !contents.is_empty() {
            sections.push(format!(
                "personal {}\n{}",
                paths::agents_file().display(),
                bounded_text(contents, MAX_INSTRUCTION_PREVIEW_BYTES)
            ));
        }
    }

    let project_rules = project.read_with(cx, |project, cx| {
        project
            .visible_worktrees(cx)
            .filter_map(|worktree| {
                let worktree = worktree.read(cx);
                let (path, _) = prompt_store::RULES_FILE_NAMES.iter().find_map(|name| {
                    let path = RelPath::from_unix_str(name).ok()?.into_arc();
                    worktree
                        .entry_for_path(&path)
                        .filter(|entry| entry.is_file())
                        .map(|_| (path, *name))
                })?;
                Some((
                    worktree.root_name_str().to_owned(),
                    ProjectPath {
                        worktree_id: worktree.id(),
                        path,
                    },
                ))
            })
            .collect::<Vec<_>>()
    });
    for (root_name, path) in project_rules {
        let display_path = format!("{root_name}/{}", path.path.as_unix_str());
        let open = project.update(cx, |project, cx| project.open_buffer(path, cx));
        match open.await {
            Ok(buffer) => {
                let contents = buffer.read_with(cx, |buffer, _| buffer.as_rope().to_string());
                sections.push(format!(
                    "project {display_path}\n{}",
                    bounded_text(contents.trim(), MAX_INSTRUCTION_PREVIEW_BYTES)
                ));
            }
            Err(error) => sections.push(format!("project {display_path}\n[unreadable: {error}]")),
        }
    }

    if sections.is_empty() {
        Ok("effective Agent instructions: built-in Zed system prompt only".to_owned())
    } else {
        Ok(format!(
            "effective Agent instructions ({})\n\n{}",
            sections.len(),
            sections.join("\n\n")
        ))
    }
}

fn format_models(models: AgentModelList) -> String {
    let mut lines = Vec::new();
    match models {
        AgentModelList::Flat(models) => {
            lines.push(format!("models ({})", models.len()));
            lines.extend(models.into_iter().map(format_model));
        }
        AgentModelList::Grouped(groups) => {
            let count = groups.values().map(Vec::len).sum::<usize>();
            lines.push(format!("models ({count})"));
            for (group, models) in groups {
                lines.push(format!("[{}]", group.0));
                lines.extend(models.into_iter().map(format_model));
            }
        }
    }
    lines.join("\n")
}

fn format_model(model: acp_thread::AgentModelInfo) -> String {
    let disabled = model
        .disabled
        .as_ref()
        .map_or(String::new(), |reason| format!(" [disabled: {}]", reason.0));
    let description = model
        .description
        .as_deref()
        .map_or(String::new(), |description| format!(" — {description}"));
    format!("- {} ({}){disabled}{description}", model.name, model.id)
}

fn format_config_options(options: &[acp::SessionConfigOption]) -> String {
    if options.is_empty() {
        return "session config options: none".to_owned();
    }
    let mut lines = vec![format!("session config options ({})", options.len())];
    lines.extend(options.iter().map(|option| {
        let value = match &option.kind {
            acp::SessionConfigKind::Select(select) => select.current_value.to_string(),
            acp::SessionConfigKind::Boolean(boolean) => boolean.current_value.to_string(),
            _ => "unsupported".to_owned(),
        };
        format!("- {} ({}) = {value}", option.name, option.id)
    }));
    lines.join("\n")
}

fn config_option_for_category(
    options: &[acp::SessionConfigOption],
    category: acp::SessionConfigOptionCategory,
) -> Option<acp::SessionConfigOption> {
    options
        .iter()
        .find(|option| option.category.as_ref() == Some(&category))
        .cloned()
}

fn select_option_values(option: &acp::SessionConfigOption) -> Vec<String> {
    let acp::SessionConfigKind::Select(select) = &option.kind else {
        return Vec::new();
    };
    match &select.options {
        acp::SessionConfigSelectOptions::Ungrouped(options) => options
            .iter()
            .map(|option| option.value.to_string())
            .collect(),
        acp::SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| group.options.iter())
            .map(|option| option.value.to_string())
            .collect(),
        _ => Vec::new(),
    }
}

fn format_select_option(label: &str, option: &acp::SessionConfigOption) -> String {
    let acp::SessionConfigKind::Select(select) = &option.kind else {
        return format!("{label}: unsupported selector shape");
    };
    let values: Vec<_> = match &select.options {
        acp::SessionConfigSelectOptions::Ungrouped(options) => options
            .iter()
            .map(|option| (&option.value, option.name.as_str()))
            .collect(),
        acp::SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| group.options.iter())
            .map(|option| (&option.value, option.name.as_str()))
            .collect(),
        _ => Vec::new(),
    };
    let mut lines = vec![format!("{label} ({})", values.len())];
    lines.extend(values.into_iter().map(|(value, name)| {
        let selected = if value == &select.current_value {
            "*"
        } else {
            " "
        };
        format!("{selected} {name} ({value})")
    }));
    lines.join("\n")
}

#[derive(Clone, Debug)]
pub(crate) struct AgentPanelState {
    phase: AgentPhase,
    agent_name: String,
    prompt: String,
    cursor: usize,
    history: Vec<String>,
    history_index: Option<usize>,
    scroll_from_bottom: usize,
    notice: Option<String>,
    local_output: Vec<LocalAgentOutput>,
}

#[derive(Clone, Debug)]
struct LocalAgentOutput {
    /// Number of ACP thread entries that existed when this command completed.
    /// Keeping the anchor lets terminal-only command output retain its true
    /// position relative to later user and assistant messages.
    after_entry: usize,
    text: String,
}

impl AgentPanelState {
    pub(crate) fn new(
        config: Option<&AgentLaunchConfig>,
        configuration_error: Option<String>,
    ) -> Self {
        let (phase, agent_name, notice) = match (config, configuration_error) {
            (_, Some(error)) => (
                AgentPhase::Error(error.clone()),
                "external ACP".to_owned(),
                Some(error),
            ),
            (Some(config), None) => (
                AgentPhase::Disconnected,
                config.display_name().to_owned(),
                None,
            ),
            (None, None) => (AgentPhase::Disconnected, "Zed Agent".to_owned(), None),
        };
        Self {
            phase,
            agent_name,
            prompt: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_index: None,
            scroll_from_bottom: 0,
            notice,
            local_output: Vec::new(),
        }
    }

    pub(crate) fn phase(&self) -> &AgentPhase {
        &self.phase
    }

    pub(crate) fn set_phase(&mut self, phase: AgentPhase) {
        self.phase = phase;
    }

    pub(crate) fn set_notice(&mut self, notice: impl Into<String>) {
        self.notice = Some(bounded_text(&notice.into(), 4096));
    }

    pub(crate) fn clear_notice(&mut self) {
        self.notice = None;
    }

    pub(crate) fn push_local_output(&mut self, output: impl Into<String>, after_entry: usize) {
        let output = bounded_text(
            &sanitize_terminal_text(&output.into()),
            MAX_LOCAL_OUTPUT_BYTES,
        );
        self.local_output.push(LocalAgentOutput {
            after_entry,
            text: output,
        });
        while self.local_output.len() > MAX_LOCAL_OUTPUT_ENTRIES
            || self
                .local_output
                .iter()
                .map(|output| output.text.len())
                .sum::<usize>()
                > MAX_LOCAL_OUTPUT_BYTES
        {
            self.local_output.remove(0);
        }
        self.scroll_from_bottom = 0;
    }

    pub(crate) fn clear_local_output(&mut self) {
        self.local_output.clear();
        self.scroll_from_bottom = 0;
    }

    pub(crate) fn handle_key(&mut self, event: &KeyEvent, generating: bool) -> AgentInput {
        if event.kind == KeyEventKind::Release {
            return AgentInput::Consumed;
        }
        match (event.code, event.modifiers) {
            (KeyCode::Esc, KeyModifiers::NONE) => AgentInput::FocusEditor,
            (KeyCode::Char('c'), KeyModifiers::CONTROL) if generating => {
                AgentInput::CancelGeneration
            }
            (KeyCode::Char('n'), KeyModifiers::CONTROL) => AgentInput::NewSession,
            (KeyCode::PageUp, KeyModifiers::NONE) => {
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_add(8);
                AgentInput::Consumed
            }
            (KeyCode::PageDown, KeyModifiers::NONE) => {
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(8);
                AgentInput::Consumed
            }
            (KeyCode::Enter, KeyModifiers::NONE) => {
                let prompt = self.prompt.trim().to_owned();
                if prompt.is_empty() || generating {
                    return AgentInput::Consumed;
                }
                if self.history.last() != Some(&prompt) {
                    self.history.push(prompt.clone());
                    if self.history.len() > 100 {
                        self.history.remove(0);
                    }
                }
                self.prompt.clear();
                self.cursor = 0;
                self.history_index = None;
                self.scroll_from_bottom = 0;
                AgentInput::Submit(prompt)
            }
            (KeyCode::Enter, modifiers)
                if modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                self.insert("\n");
                AgentInput::Consumed
            }
            (KeyCode::Up, KeyModifiers::NONE) if self.prompt.is_empty() => {
                self.select_history(true);
                AgentInput::Consumed
            }
            (KeyCode::Down, KeyModifiers::NONE) if self.history_index.is_some() => {
                self.select_history(false);
                AgentInput::Consumed
            }
            (KeyCode::Backspace, KeyModifiers::NONE) => {
                if let Some(previous) = self.prompt[..self.cursor]
                    .char_indices()
                    .next_back()
                    .map(|(index, _)| index)
                {
                    self.prompt.drain(previous..self.cursor);
                    self.cursor = previous;
                }
                AgentInput::Consumed
            }
            (KeyCode::Delete, KeyModifiers::NONE) => {
                if let Some(character) = self.prompt[self.cursor..].chars().next() {
                    self.prompt
                        .drain(self.cursor..self.cursor + character.len_utf8());
                }
                AgentInput::Consumed
            }
            (KeyCode::Left, KeyModifiers::NONE) => {
                if let Some(previous) = self.prompt[..self.cursor]
                    .char_indices()
                    .next_back()
                    .map(|(index, _)| index)
                {
                    self.cursor = previous;
                }
                AgentInput::Consumed
            }
            (KeyCode::Right, KeyModifiers::NONE) => {
                if let Some(character) = self.prompt[self.cursor..].chars().next() {
                    self.cursor += character.len_utf8();
                }
                AgentInput::Consumed
            }
            (KeyCode::Home, KeyModifiers::NONE) => {
                self.cursor = self.prompt[..self.cursor]
                    .rfind('\n')
                    .map_or(0, |index| index + 1);
                AgentInput::Consumed
            }
            (KeyCode::End, KeyModifiers::NONE) => {
                self.cursor += self.prompt[self.cursor..]
                    .find('\n')
                    .unwrap_or(self.prompt.len() - self.cursor);
                AgentInput::Consumed
            }
            (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                self.prompt.clear();
                self.cursor = 0;
                self.history_index = None;
                AgentInput::Consumed
            }
            (KeyCode::Char(character), modifiers)
                if !character.is_control()
                    && !modifiers.intersects(
                        KeyModifiers::CONTROL
                            | KeyModifiers::ALT
                            | KeyModifiers::SUPER
                            | KeyModifiers::HYPER
                            | KeyModifiers::META,
                    ) =>
            {
                let mut encoded = [0; 4];
                self.insert(character.encode_utf8(&mut encoded));
                AgentInput::Consumed
            }
            _ => AgentInput::Consumed,
        }
    }

    pub(crate) fn handle_paste(&mut self, text: &str) {
        let mut normalized = String::with_capacity(text.len().min(MAX_PROMPT_BYTES));
        let mut previous_was_cr = false;
        for character in text.chars() {
            if character == '\r' {
                normalized.push('\n');
                previous_was_cr = true;
            } else if character == '\n' && previous_was_cr {
                previous_was_cr = false;
            } else {
                previous_was_cr = false;
                if character == '\n' || character == '\t' || !character.is_control() {
                    normalized.push(character);
                }
            }
            if normalized.len() >= MAX_PROMPT_BYTES {
                break;
            }
        }
        self.insert(&normalized);
    }

    pub(crate) fn scroll(&mut self, toward_history: bool, rows: usize) {
        if toward_history {
            self.scroll_from_bottom = self.scroll_from_bottom.saturating_add(rows);
        } else {
            self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(rows);
        }
    }

    fn insert(&mut self, text: &str) {
        let available = MAX_PROMPT_BYTES.saturating_sub(self.prompt.len());
        if available == 0 {
            self.notice = Some("agent prompt reached the 64 KiB limit".to_owned());
            return;
        }
        let mut end = text.len().min(available);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        self.prompt.insert_str(self.cursor, &text[..end]);
        self.cursor += end;
        self.history_index = None;
        if end < text.len() {
            self.notice = Some("agent prompt was truncated at 64 KiB".to_owned());
        }
    }

    fn select_history(&mut self, previous: bool) {
        if self.history.is_empty() {
            return;
        }
        self.history_index = if previous {
            Some(
                self.history_index
                    .map_or(self.history.len() - 1, |index| index.saturating_sub(1)),
            )
        } else {
            self.history_index
                .and_then(|index| (index + 1 < self.history.len()).then_some(index + 1))
        };
        self.prompt = self
            .history_index
            .and_then(|index| self.history.get(index).cloned())
            .unwrap_or_default();
        self.cursor = self.prompt.len();
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PendingPermission {
    pub(crate) tool_call_id: acp::ToolCallId,
    pub(crate) label: String,
    pub(crate) choices: Vec<String>,
}

fn permission_labels(options: &PermissionOptions) -> Vec<String> {
    match options {
        PermissionOptions::Flat(options) => {
            options.iter().map(|option| option.name.clone()).collect()
        }
        PermissionOptions::Dropdown(choices)
        | PermissionOptions::DropdownWithPatterns { choices, .. } => choices
            .iter()
            .flat_map(|choice| [choice.allow.name.clone(), choice.deny.name.clone()])
            .collect(),
    }
}

pub(crate) fn authorization_outcome(
    thread: &Entity<AcpThread>,
    allow: bool,
    cx: &gpui::App,
) -> Option<(acp::ToolCallId, SelectedPermissionOutcome)> {
    thread.read_with(cx, |thread, _cx| {
        thread.entries().iter().rev().find_map(|entry| {
            let AgentThreadEntry::ToolCall(call) = entry else {
                return None;
            };
            let ToolCallStatus::WaitingForConfirmation { options, .. } = &call.status else {
                return None;
            };
            let kinds = if allow {
                [
                    acp::PermissionOptionKind::AllowOnce,
                    acp::PermissionOptionKind::AllowAlways,
                ]
            } else {
                [
                    acp::PermissionOptionKind::RejectOnce,
                    acp::PermissionOptionKind::RejectAlways,
                ]
            };
            let option = kinds
                .into_iter()
                .find_map(|kind| options.first_option_of_kind(kind))?;
            Some((
                call.id.clone(),
                SelectedPermissionOutcome::new(option.option_id.clone(), option.kind),
            ))
        })
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ConversationLine {
    text: String,
    style: ConversationStyle,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConversationStyle {
    Plain,
    User,
    Assistant,
    Tool,
    Thought,
    Warning,
}

#[derive(Clone, Debug)]
pub(crate) struct AgentPanelSnapshot {
    title: String,
    conversation: Vec<ConversationLine>,
    prompt: String,
    prompt_cursor: usize,
    scroll_from_bottom: usize,
    permission: Option<PendingPermission>,
    notice: Option<String>,
}

impl AgentPanelSnapshot {
    pub(crate) fn capture(
        state: &AgentPanelState,
        thread: Option<&Entity<AcpThread>>,
        mcp_server_count: usize,
        cx: &gpui::App,
    ) -> Self {
        let mut conversation = Vec::new();
        let (thread_status, command_count, token_label, permission) = if let Some(thread) = thread {
            thread.read_with(cx, |thread, cx| {
                let mut remaining = MAX_CONVERSATION_BYTES;
                let start = thread
                    .entries()
                    .len()
                    .saturating_sub(MAX_CONVERSATION_ENTRIES);
                for output in state
                    .local_output
                    .iter()
                    .filter(|output| output.after_entry <= start)
                {
                    append_local_output(&mut conversation, &output.text);
                }
                for (index, entry) in thread.entries()[start..].iter().enumerate() {
                    if remaining == 0 {
                        break;
                    }
                    let markdown = bounded_text(&entry.to_markdown(cx), remaining);
                    remaining = remaining.saturating_sub(markdown.len());
                    append_markdown_lines(&mut conversation, &markdown);
                    let after_entry = start + index + 1;
                    for output in state
                        .local_output
                        .iter()
                        .filter(|output| output.after_entry == after_entry)
                    {
                        append_local_output(&mut conversation, &output.text);
                    }
                }
                let status = match thread.status() {
                    ThreadStatus::Idle => "idle",
                    ThreadStatus::Generating => "generating",
                };
                let tokens = thread.token_usage().map(|usage| {
                    if usage.max_tokens == 0 {
                        format!("{} tok", usage.used_tokens)
                    } else {
                        format!("{}/{} tok", usage.used_tokens, usage.max_tokens)
                    }
                });
                (
                    status,
                    thread.available_commands().len(),
                    tokens,
                    pending_permission_from_entries(thread.entries(), cx),
                )
            })
        } else {
            for output in &state.local_output {
                append_local_output(&mut conversation, &output.text);
            }
            (state.phase.label(), 0, None, None)
        };
        if conversation.is_empty() {
            conversation.push(ConversationLine {
                text: match state.phase() {
                    AgentPhase::Connecting => "Starting the ACP process and creating a Zed agent session…".to_owned(),
                    AgentPhase::Error(error) => format!("ACP connection failed: {error}"),
                    _ => "Type a prompt below. Zed owns the ACP session, project context, MCP hand-off, and tool authorization.".to_owned(),
                },
                style: if matches!(state.phase(), AgentPhase::Error(_)) {
                    ConversationStyle::Warning
                } else {
                    ConversationStyle::Plain
                },
            });
        }
        let mut title = format!(
            " Agent · {} · ACP {} · MCP {} · /cmd {}",
            state.agent_name, thread_status, mcp_server_count, command_count
        );
        if let Some(tokens) = token_label {
            title.push_str(" · ");
            title.push_str(&tokens);
        }
        title.push(' ');
        Self {
            title,
            conversation,
            prompt: state.prompt.clone(),
            prompt_cursor: state.cursor,
            scroll_from_bottom: state.scroll_from_bottom,
            permission,
            notice: state.notice.clone(),
        }
    }
}

fn append_local_output(conversation: &mut Vec<ConversationLine>, output: &str) {
    conversation.push(ConversationLine {
        text: "## zec".to_owned(),
        style: ConversationStyle::Thought,
    });
    for line in output.lines() {
        conversation.push(ConversationLine {
            text: line.to_owned(),
            style: ConversationStyle::Plain,
        });
    }
}

fn pending_permission_from_entries(
    entries: &[AgentThreadEntry],
    cx: &gpui::App,
) -> Option<PendingPermission> {
    entries.iter().rev().find_map(|entry| {
        let AgentThreadEntry::ToolCall(call) = entry else {
            return None;
        };
        let ToolCallStatus::WaitingForConfirmation { options, .. } = &call.status else {
            return None;
        };
        Some(PendingPermission {
            tool_call_id: call.id.clone(),
            label: bounded_text(call.label.read(cx).source(), 1024),
            choices: permission_labels(options),
        })
    })
}

fn append_markdown_lines(output: &mut Vec<ConversationLine>, markdown: &str) {
    for line in markdown.lines() {
        let trimmed = line.trim_start();
        let style = if trimmed == "## User" {
            ConversationStyle::User
        } else if trimmed == "## Assistant" {
            ConversationStyle::Assistant
        } else if trimmed.starts_with("**Tool Call:") || trimmed.starts_with("Status:") {
            ConversationStyle::Tool
        } else if trimmed.starts_with("<thinking>") || trimmed.ends_with("</thinking>") {
            ConversationStyle::Thought
        } else {
            ConversationStyle::Plain
        };
        output.push(ConversationLine {
            text: sanitize_terminal_text(line),
            style,
        });
    }
}

pub(crate) struct AgentPanelWidget<'a> {
    snapshot: &'a AgentPanelSnapshot,
    focused: bool,
}

impl<'a> AgentPanelWidget<'a> {
    pub(crate) fn new(snapshot: &'a AgentPanelSnapshot, focused: bool) -> Self {
        Self { snapshot, focused }
    }

    pub(crate) fn cursor_position(&self, area: Rect) -> Option<Position> {
        if !self.focused || self.snapshot.permission.is_some() {
            return None;
        }
        let inner = Block::default().borders(Borders::ALL).inner(area);
        let layout = panel_layout(self.snapshot, inner);
        let prompt = prompt_layout(
            &self.snapshot.prompt,
            self.snapshot.prompt_cursor,
            usize::from(inner.width),
        );
        let visible_start = prompt.lines.len().saturating_sub(layout.prompt_rows);
        let (row, column) = prompt.cursor?;
        if row < visible_start || row >= visible_start + layout.prompt_rows {
            return None;
        }
        Some(Position::new(
            inner.x.saturating_add(u16::try_from(column).ok()?),
            inner
                .y
                .saturating_add(u16::try_from(layout.prompt_y + row - visible_start).ok()?),
        ))
    }
}

impl Widget for AgentPanelWidget<'_> {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        if area.width < 3 || area.height < 3 {
            return;
        }
        Clear.render(area, buffer);
        let border_style = if self.focused {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title(self.snapshot.title.as_str())
            .border_style(border_style);
        let inner = block.inner(area);
        block.render(area, buffer);
        if inner.is_empty() {
            return;
        }
        let layout = panel_layout(self.snapshot, inner);
        let wrapped = wrap_conversation(&self.snapshot.conversation, usize::from(inner.width));
        let end = wrapped
            .len()
            .saturating_sub(self.snapshot.scroll_from_bottom.min(wrapped.len()));
        let start = end.saturating_sub(layout.conversation_rows);
        for (offset, line) in wrapped[start..end].iter().enumerate() {
            buffer.set_stringn(
                inner.x,
                inner
                    .y
                    .saturating_add(u16::try_from(offset).unwrap_or(u16::MAX)),
                &line.text,
                usize::from(inner.width),
                conversation_style(line.style),
            );
        }

        if let Some(permission) = &self.snapshot.permission {
            let y = inner
                .y
                .saturating_add(u16::try_from(layout.prompt_y).unwrap_or(u16::MAX));
            buffer.set_stringn(
                inner.x,
                y,
                format!("⚠ Permission: {}", permission.label),
                usize::from(inner.width),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            );
            if layout.prompt_rows > 1 {
                let choices = if permission.choices.is_empty() {
                    "y allow once · n reject once".to_owned()
                } else {
                    format!("y allow · n reject · {}", permission.choices.join(" / "))
                };
                buffer.set_stringn(
                    inner.x,
                    y.saturating_add(1),
                    choices,
                    usize::from(inner.width),
                    Style::default().fg(Color::Yellow),
                );
            }
        } else {
            let prompt = prompt_layout(
                &self.snapshot.prompt,
                self.snapshot.prompt_cursor,
                usize::from(inner.width),
            );
            let visible_start = prompt.lines.len().saturating_sub(layout.prompt_rows);
            for (offset, line) in prompt.lines[visible_start..].iter().enumerate() {
                buffer.set_stringn(
                    inner.x,
                    inner.y.saturating_add(
                        u16::try_from(layout.prompt_y + offset).unwrap_or(u16::MAX),
                    ),
                    line,
                    usize::from(inner.width),
                    Style::default().fg(Color::White),
                );
            }
        }

        if layout.footer_rows > 0 {
            let footer = self.snapshot.notice.as_deref().unwrap_or(
                "Enter send · Shift/Alt-Enter newline · PgUp/PgDn history · Ctrl-C cancel · Ctrl-N new · Esc editor",
            );
            buffer.set_stringn(
                inner.x,
                inner.bottom().saturating_sub(1),
                footer,
                usize::from(inner.width),
                Style::default().fg(Color::DarkGray),
            );
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct PanelLayout {
    conversation_rows: usize,
    prompt_y: usize,
    prompt_rows: usize,
    footer_rows: usize,
}

fn panel_layout(snapshot: &AgentPanelSnapshot, inner: Rect) -> PanelLayout {
    let height = usize::from(inner.height);
    let footer_rows = usize::from(height >= 3);
    let wanted_prompt_rows = if snapshot.permission.is_some() {
        2
    } else {
        prompt_layout(
            &snapshot.prompt,
            snapshot.prompt_cursor,
            usize::from(inner.width),
        )
        .lines
        .len()
        .clamp(1, MAX_PROMPT_ROWS)
    };
    let prompt_rows = wanted_prompt_rows.min(height.saturating_sub(footer_rows).max(1));
    let conversation_rows = height.saturating_sub(prompt_rows + footer_rows);
    PanelLayout {
        conversation_rows,
        prompt_y: conversation_rows,
        prompt_rows,
        footer_rows,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PromptLayout {
    lines: Vec<String>,
    cursor: Option<(usize, usize)>,
}

fn prompt_layout(text: &str, cursor: usize, width: usize) -> PromptLayout {
    let width = width.max(1);
    let prefix_width = 2.min(width);
    let content_width = width.saturating_sub(prefix_width).max(1);
    let mut lines = vec![String::from("› ")];
    let mut row = 0usize;
    let mut column = prefix_width;
    let mut cursor_position = None;
    for (byte, character) in text.char_indices() {
        if byte == cursor {
            cursor_position = Some((row, column.min(width.saturating_sub(1))));
        }
        if character == '\n' {
            lines.push(String::from("  "));
            row += 1;
            column = prefix_width;
            continue;
        }
        let character_width = character.width().unwrap_or(0);
        if column.saturating_sub(prefix_width) + character_width > content_width
            && column > prefix_width
        {
            lines.push(String::from("  "));
            row += 1;
            column = prefix_width;
        }
        lines[row].push(character);
        column = column.saturating_add(character_width);
    }
    if cursor == text.len() {
        cursor_position = Some((row, column.min(width.saturating_sub(1))));
    }
    PromptLayout {
        lines,
        cursor: cursor_position,
    }
}

fn wrap_conversation(lines: &[ConversationLine], width: usize) -> Vec<ConversationLine> {
    let width = width.max(1);
    let mut output = Vec::new();
    for line in lines {
        if line.text.is_empty() {
            output.push(line.clone());
            continue;
        }
        let mut current = String::new();
        let mut current_width = 0usize;
        for character in line.text.chars() {
            let character_width = character.width().unwrap_or(0);
            if current_width + character_width > width && !current.is_empty() {
                output.push(ConversationLine {
                    text: std::mem::take(&mut current),
                    style: line.style,
                });
                current_width = 0;
            }
            current.push(character);
            current_width += character_width;
        }
        output.push(ConversationLine {
            text: current,
            style: line.style,
        });
    }
    output
}

fn conversation_style(style: ConversationStyle) -> Style {
    match style {
        ConversationStyle::Plain => Style::default(),
        ConversationStyle::User => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        ConversationStyle::Assistant => Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
        ConversationStyle::Tool => Style::default().fg(Color::Yellow),
        ConversationStyle::Thought => Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::DIM),
        ConversationStyle::Warning => Style::default().fg(Color::LightRed),
    }
}

fn sanitize_terminal_text(text: &str) -> String {
    text.chars()
        .filter_map(|character| match character {
            '\t' => Some(' '),
            character if !character.is_control() => Some(character),
            _ => None,
        })
        .collect()
}

fn bounded_text(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit.saturating_sub('…'.len_utf8()).min(text.len());
    while !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr as _;

    #[test]
    fn launch_configuration_is_shell_free_and_bounded() {
        let config = AgentLaunchConfig::parse(
            r#"{"command":"/bin/agent","args":["--acp","a b"],"env":{"TOKEN":"secret"},"id":"fixture"}"#,
        )
        .unwrap();
        assert_eq!(config.command, PathBuf::from("/bin/agent"));
        assert_eq!(config.args, ["--acp", "a b"]);
        assert_eq!(config.id, "fixture");
        assert!(AgentLaunchConfig::parse(r#"{"command":"x","unknown":true}"#).is_err());
        assert!(AgentLaunchConfig::parse(r#"{"command":"x","env":{"BAD=NAME":"x"}}"#).is_err());
    }

    #[test]
    fn local_commands_are_strict_and_unknown_slash_commands_remain_agent_owned() {
        assert_eq!(
            parse_local_command("/model provider/model"),
            Some(AgentLocalCommand::SetModel("provider/model".to_owned()))
        );
        assert_eq!(
            parse_local_command("/config thinking true"),
            Some(AgentLocalCommand::SetConfig {
                id: "thinking".to_owned(),
                value: "true".to_owned(),
            })
        );
        assert!(matches!(
            parse_local_command("/mode"),
            Some(AgentLocalCommand::Invalid(_))
        ));
        assert_eq!(parse_local_command("/agent-owned argument"), None);
    }

    #[test]
    fn native_agent_is_the_zero_configuration_default() {
        let state = AgentPanelState::new(None, None);
        assert_eq!(state.phase(), &AgentPhase::Disconnected);
        assert_eq!(state.agent_name, "Zed Agent");
        assert!(state.notice.is_none());
    }

    #[test]
    fn prompt_supports_unicode_multiline_history_and_bounds() {
        let mut state = AgentPanelState::new(None, None);
        state.handle_paste("α\r\nβ");
        assert_eq!(state.prompt, "α\nβ");
        assert_eq!(
            state.handle_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), false),
            AgentInput::Submit("α\nβ".to_owned())
        );
        state.handle_key(&KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), false);
        assert_eq!(state.prompt, "α\nβ");
        state.handle_paste(&"x".repeat(MAX_PROMPT_BYTES + 100));
        assert!(state.prompt.len() <= MAX_PROMPT_BYTES);
    }

    #[test]
    fn prompt_layout_tracks_cursor_across_wide_cells_and_wraps() {
        let layout = prompt_layout("ab界c", "ab界".len(), 6);
        assert_eq!(layout.lines, ["› ab界", "  c"]);
        assert_eq!(layout.cursor, Some((0, 6 - 1)));
        let layout = prompt_layout("a\nb", 2, 10);
        assert_eq!(layout.lines, ["› a", "  b"]);
        assert_eq!(layout.cursor, Some((1, 2)));
    }

    #[test]
    fn conversation_wrap_is_bounded_by_terminal_cells() {
        let wrapped = wrap_conversation(
            &[ConversationLine {
                text: "ab界cd".to_owned(),
                style: ConversationStyle::Assistant,
            }],
            4,
        );
        assert_eq!(
            wrapped
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            ["ab界", "cd"]
        );
        assert!(wrapped.iter().all(|line| line.text.width() <= 4));
    }
}
