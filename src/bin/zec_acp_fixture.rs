//! Deterministic stdio ACP agent used by the real-binary acceptance gate.
//!
//! This deliberately implements the wire peer as a tiny line-oriented
//! JSON-RPC loop. The production side of the gate is still Zed's full
//! `agent_servers::AcpConnection` and `acp_thread::AcpThread` stack.

use std::io::{self, BufRead as _, Write};

use agent_client_protocol::schema::v1 as acp;
use anyhow::{Context as _, Result};
use serde::Serialize;
use serde_json::{Value, json};

const SESSION_ID: &str = "zec-acp-fixture-session";
const SAVED_SESSION_ID: &str = "zec-acp-fixture-saved";
const TOOL_CALL_ID: &str = "zec-acp-fixture-tool";
const PERMISSION_REQUEST_ID: &str = "zec-acp-fixture-permission";

fn main() -> Result<()> {
    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    let stdout = io::stdout();
    let mut output = stdout.lock();
    let mut mcp_server_count = 0usize;
    let mut session_cwd = std::env::current_dir().context("read fixture cwd")?;
    let mut selected_model = "fixture-balanced".to_owned();
    let mut selected_mode = "ask".to_owned();
    let mut thinking = false;

    while let Some(line) = lines.next() {
        let line = line.context("read ACP request")?;
        let message: Value = serde_json::from_str(&line).context("parse ACP JSON-RPC request")?;
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            continue;
        };
        let id = message.get("id").cloned();
        match method {
            "initialize" => {
                let request: acp::InitializeRequest = params(&message)?;
                respond(
                    &mut output,
                    id.context("initialize request missing id")?,
                    &acp::InitializeResponse::new(request.protocol_version)
                        .agent_capabilities(
                            acp::AgentCapabilities::new()
                                .load_session(true)
                                .session_capabilities(
                                    acp::SessionCapabilities::new()
                                        .list(acp::SessionListCapabilities::new())
                                        .close(acp::SessionCloseCapabilities::new()),
                                )
                                .auth(
                                    acp::AgentAuthCapabilities::new()
                                        .logout(acp::LogoutCapabilities::new()),
                                ),
                        )
                        .auth_methods(auth_methods())
                        .agent_info(acp::Implementation::new("zec-acp-fixture", "1.0.0")),
                )?;
            }
            "session/new" => {
                let request: acp::NewSessionRequest = params(&message)?;
                mcp_server_count = request.mcp_servers.len();
                session_cwd = request.cwd;
                respond(
                    &mut output,
                    id.context("session/new request missing id")?,
                    &acp::NewSessionResponse::new(SESSION_ID).config_options(config_options(
                        &selected_model,
                        &selected_mode,
                        thinking,
                    )),
                )?;
                notify(
                    &mut output,
                    "session/update",
                    &acp::SessionNotification::new(
                        SESSION_ID,
                        acp::SessionUpdate::AvailableCommandsUpdate(
                            acp::AvailableCommandsUpdate::new(vec![acp::AvailableCommand::new(
                                "fixture_command",
                                "Fixture-advertised ACP slash command",
                            )]),
                        ),
                    ),
                )?;
            }
            "session/set_config_option" => {
                let request: acp::SetSessionConfigOptionRequest = params(&message)?;
                match request.config_id.0.as_ref() {
                    "model" => {
                        selected_model = request
                            .value
                            .as_value_id()
                            .context("model config requires a value id")?
                            .to_string();
                    }
                    "mode" => {
                        selected_mode = request
                            .value
                            .as_value_id()
                            .context("mode config requires a value id")?
                            .to_string();
                    }
                    "thinking" => match request.value {
                        acp::SessionConfigOptionValue::Boolean { value } => thinking = value,
                        _ => anyhow::bail!("thinking config requires a boolean"),
                    },
                    other => anyhow::bail!("unknown fixture config option {other}"),
                }
                respond(
                    &mut output,
                    id.context("session/set_config_option request missing id")?,
                    &acp::SetSessionConfigOptionResponse::new(config_options(
                        &selected_model,
                        &selected_mode,
                        thinking,
                    )),
                )?;
            }
            "session/list" => {
                let _: acp::ListSessionsRequest = params(&message)?;
                respond(
                    &mut output,
                    id.context("session/list request missing id")?,
                    &acp::ListSessionsResponse::new(vec![
                        acp::SessionInfo::new(SAVED_SESSION_ID, session_cwd.clone())
                            .title("Fixture saved session"),
                    ]),
                )?;
            }
            "session/load" => {
                let request: acp::LoadSessionRequest = params(&message)?;
                anyhow::ensure!(request.session_id.0.as_ref() == SAVED_SESSION_ID);
                respond(
                    &mut output,
                    id.context("session/load request missing id")?,
                    &acp::LoadSessionResponse::new().config_options(config_options(
                        &selected_model,
                        &selected_mode,
                        thinking,
                    )),
                )?;
            }
            "session/close" => {
                let request: acp::CloseSessionRequest = params(&message)?;
                anyhow::ensure!(
                    request.session_id.0.as_ref() == SAVED_SESSION_ID
                        || request.session_id.0.as_ref() == SESSION_ID
                );
                respond(
                    &mut output,
                    id.context("session/close request missing id")?,
                    &acp::CloseSessionResponse::new(),
                )?;
            }
            "authenticate" => {
                let request: acp::AuthenticateRequest = params(&message)?;
                anyhow::ensure!(request.method_id.0.as_ref() == "fixture-login");
                respond(
                    &mut output,
                    id.context("authenticate request missing id")?,
                    &acp::AuthenticateResponse::new(),
                )?;
            }
            "logout" => {
                let _: acp::LogoutRequest = params(&message)?;
                respond(
                    &mut output,
                    id.context("logout request missing id")?,
                    &acp::LogoutResponse::new(),
                )?;
            }
            "session/prompt" => {
                let request: acp::PromptRequest = params(&message)?;
                let prompt = request
                    .prompt
                    .iter()
                    .filter_map(|block| match block {
                        acp::ContentBlock::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                run_prompt(
                    &mut lines,
                    &mut output,
                    &request.session_id,
                    &prompt,
                    mcp_server_count,
                )?;
                respond(
                    &mut output,
                    id.context("session/prompt request missing id")?,
                    &acp::PromptResponse::new(acp::StopReason::EndTurn),
                )?;
            }
            "session/cancel" => {}
            _ if id.is_some() => {
                write_json(
                    &mut output,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {"code": -32601, "message": format!("unsupported fixture method: {method}")}
                    }),
                )?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn auth_methods() -> Vec<acp::AuthMethod> {
    let auth_log = std::env::var("ZEC_ACP_FIXTURE_AUTH_LOG")
        .unwrap_or_else(|_| "/tmp/zec-acp-fixture-auth.log".to_owned());
    let terminal_meta: acp::Meta = serde_json::from_value(json!({
        "terminal-auth": {
            "label": "Fixture terminal login",
            "command": "/bin/sh",
            "args": [
                "-c",
                "printf 'BETA3_TERMINAL_AUTH_READY\\n' | tee \"$ZEC_ACP_FIXTURE_AUTH_LOG\""
            ],
            "env": {"ZEC_ACP_FIXTURE_AUTH_LOG": auth_log}
        }
    }))
    .expect("terminal auth metadata is an object");
    vec![
        acp::AuthMethod::Agent(acp::AuthMethodAgent::new("fixture-login", "Fixture login")),
        acp::AuthMethod::Agent(
            acp::AuthMethodAgent::new("fixture-terminal", "Fixture terminal login")
                .description("Interactive login through the Zed terminal")
                .meta(terminal_meta),
        ),
    ]
}

fn config_options(
    selected_model: &str,
    selected_mode: &str,
    thinking: bool,
) -> Vec<acp::SessionConfigOption> {
    vec![
        acp::SessionConfigOption::select(
            "model",
            "Model",
            selected_model.to_owned(),
            vec![
                acp::SessionConfigSelectOption::new("fixture-balanced", "Fixture Balanced"),
                acp::SessionConfigSelectOption::new("fixture-fast", "Fixture Fast"),
            ],
        )
        .category(acp::SessionConfigOptionCategory::Model),
        acp::SessionConfigOption::select(
            "mode",
            "Mode",
            selected_mode.to_owned(),
            vec![
                acp::SessionConfigSelectOption::new("ask", "Ask"),
                acp::SessionConfigSelectOption::new("plan", "Plan"),
            ],
        )
        .category(acp::SessionConfigOptionCategory::Mode),
        acp::SessionConfigOption::boolean("thinking", "Thinking", thinking)
            .category(acp::SessionConfigOptionCategory::ThoughtLevel),
    ]
}

fn run_prompt(
    lines: &mut impl Iterator<Item = io::Result<String>>,
    output: &mut impl Write,
    session_id: &acp::SessionId,
    prompt: &str,
    mcp_server_count: usize,
) -> Result<()> {
    let tool = acp::ToolCall::new(TOOL_CALL_ID, "Fixture approval gate")
        .kind(acp::ToolKind::Execute)
        .status(acp::ToolCallStatus::Pending)
        .raw_input(json!({"prompt": prompt}));
    notify(
        output,
        "session/update",
        &acp::SessionNotification::new(
            session_id.clone(),
            acp::SessionUpdate::ToolCall(tool.clone()),
        ),
    )?;
    request(
        output,
        Value::String(PERMISSION_REQUEST_ID.to_owned()),
        "session/request_permission",
        &acp::RequestPermissionRequest::new(
            session_id.clone(),
            tool.into(),
            vec![
                acp::PermissionOption::new(
                    "allow",
                    "Allow once",
                    acp::PermissionOptionKind::AllowOnce,
                ),
                acp::PermissionOption::new(
                    "deny",
                    "Reject once",
                    acp::PermissionOptionKind::RejectOnce,
                ),
            ],
        ),
    )?;

    let allowed = loop {
        let line = lines
            .next()
            .context("ACP client closed while permission was pending")??;
        let message: Value = serde_json::from_str(&line).context("parse permission response")?;
        if message.get("id") == Some(&Value::String(PERMISSION_REQUEST_ID.to_owned())) {
            let response: acp::RequestPermissionResponse = serde_json::from_value(
                message
                    .get("result")
                    .cloned()
                    .context("permission response missing result")?,
            )
            .context("decode permission response")?;
            break matches!(
                response.outcome,
                acp::RequestPermissionOutcome::Selected(ref selected)
                    if selected.option_id.0.as_ref() == "allow"
            );
        }
        if message.get("method").and_then(Value::as_str) == Some("session/cancel") {
            break false;
        }
    };

    let (status, tool_text, assistant_text) = if allowed {
        (
            acp::ToolCallStatus::Completed,
            "fixture authorization accepted",
            format!(
                "fixture approved: {prompt}\n\nMCP servers received from Zed Project: {mcp_server_count}"
            ),
        )
    } else {
        (
            acp::ToolCallStatus::Failed,
            "fixture authorization rejected",
            format!("fixture rejected: {prompt}"),
        )
    };
    notify(
        output,
        "session/update",
        &acp::SessionNotification::new(
            session_id.clone(),
            acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                TOOL_CALL_ID,
                acp::ToolCallUpdateFields::new()
                    .status(status)
                    .content(vec![tool_text.into()]),
            )),
        ),
    )?;
    notify(
        output,
        "session/update",
        &acp::SessionNotification::new(
            session_id.clone(),
            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(assistant_text.into())),
        ),
    )
}

fn params<T: serde::de::DeserializeOwned>(message: &Value) -> Result<T> {
    serde_json::from_value(
        message
            .get("params")
            .cloned()
            .context("JSON-RPC request missing params")?,
    )
    .context("decode ACP request params")
}

fn respond(output: &mut impl Write, id: Value, result: &impl Serialize) -> Result<()> {
    write_json(
        output,
        &json!({"jsonrpc": "2.0", "id": id, "result": result}),
    )
}

fn request(
    output: &mut impl Write,
    id: Value,
    method: &str,
    params: &impl Serialize,
) -> Result<()> {
    write_json(
        output,
        &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
    )
}

fn notify(output: &mut impl Write, method: &str, params: &impl Serialize) -> Result<()> {
    write_json(
        output,
        &json!({"jsonrpc": "2.0", "method": method, "params": params}),
    )
}

fn write_json(output: &mut impl Write, value: &Value) -> Result<()> {
    serde_json::to_writer(&mut *output, value).context("encode ACP JSON-RPC message")?;
    output.write_all(b"\n").context("write ACP line")?;
    output.flush().context("flush ACP line")
}
