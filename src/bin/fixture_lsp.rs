use std::{
    collections::BTreeMap,
    env,
    fs::{self, OpenOptions},
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, bail, ensure};
use serde_json::{Value, json};

const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

fn main() -> Result<()> {
    if env::args_os().skip(1).any(|argument| argument == "--help") {
        println!("fixture_lsp: deterministic LSP 3.17 fixture server");
        return Ok(());
    }

    let log_path = env::var_os("ZEC_FIXTURE_LSP_LOG").map(PathBuf::from);
    let scenario = env::var("ZEC_FIXTURE_LSP_SCENARIO").unwrap_or_else(|_| "normal".to_owned());
    let restart_attempt = if scenario == "restart-once" {
        let state_path = env::var_os("ZEC_FIXTURE_LSP_RESTART_STATE")
            .map(PathBuf::from)
            .context("restart-once requires ZEC_FIXTURE_LSP_RESTART_STATE")?;
        let previous = fs::read_to_string(&state_path)
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok())
            .unwrap_or(0);
        let attempt = previous.saturating_add(1);
        fs::write(&state_path, attempt.to_string())
            .with_context(|| format!("write restart state {}", state_path.display()))?;
        attempt
    } else {
        1
    };
    append_log(
        log_path.as_deref(),
        "lifecycle",
        &json!({
            "event": "start",
            "pid": std::process::id(),
            "scenario": scenario.as_str(),
            "attempt": restart_attempt,
        }),
    )?;
    if scenario == "huge-stderr" {
        let mut stderr = io::stderr().lock();
        let chunk = vec![b'e'; 64 * 1024];
        for _ in 0..128 {
            stderr
                .write_all(&chunk)
                .context("write bounded stderr fixture")?;
        }
        stderr.flush().context("flush bounded stderr fixture")?;
    }
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = stdout.lock();
    let mut documents = BTreeMap::<String, String>::new();
    let mut shutdown_requested = false;

    while let Some(message) = read_message(&mut reader)? {
        append_log(log_path.as_deref(), "client", &message)?;
        let method = message.get("method").and_then(Value::as_str);
        let id = message.get("id").cloned();
        let params = message.get("params").cloned().unwrap_or(Value::Null);

        match method {
            Some("initialize") => match scenario.as_str() {
                "initialize-error" => error_response(
                    &mut writer,
                    log_path.as_deref(),
                    id,
                    -32002,
                    "controlled initialize failure".to_owned(),
                )?,
                "malformed-frame" => {
                    writer
                        .write_all(b"Content-Length: 7\r\n\r\n{broken")
                        .context("write malformed fixture frame")?;
                    writer.flush().context("flush malformed fixture frame")?;
                    break;
                }
                "unexpected-eof" | "crash" => {
                    append_log(
                        log_path.as_deref(),
                        "lifecycle",
                        &json!({ "event": scenario.as_str(), "pid": std::process::id() }),
                    )?;
                    if scenario == "crash" {
                        std::process::exit(42);
                    }
                    break;
                }
                "hang-initialize" => {}
                _ => respond(&mut writer, log_path.as_deref(), id, initialize_result())?,
            },
            Some("initialized") => {
                if scenario == "restart-once" && restart_attempt == 1 {
                    append_log(
                        log_path.as_deref(),
                        "lifecycle",
                        &json!({
                            "event": "controlled-restart-crash",
                            "pid": std::process::id()
                        }),
                    )?;
                    std::process::exit(43);
                }
            }
            Some("textDocument/didOpen") => {
                if let (Some(uri), Some(text)) = (
                    params.pointer("/textDocument/uri").and_then(Value::as_str),
                    params.pointer("/textDocument/text").and_then(Value::as_str),
                ) {
                    documents.insert(uri.to_owned(), text.to_owned());
                    if uri.ends_with("/main.rs") {
                        let diagnostics = if scenario == "large-payloads" {
                            (0..10_000)
                                .map(|index| {
                                    json!({
                                        "range": {
                                            "start": { "line": 0, "character": 0 },
                                            "end": { "line": 0, "character": 1 }
                                        },
                                        "severity": 2,
                                        "code": format!("fixture-{index:05}"),
                                        "source": "zec-fixture",
                                        "message": format!("bounded diagnostic {index:05}")
                                    })
                                })
                                .collect::<Vec<_>>()
                        } else {
                            vec![json!({
                                "range": {
                                    "start": { "line": 0, "character": 3 },
                                    "end": { "line": 0, "character": 8 }
                                },
                                "severity": 2,
                                "code": "fixture-warning",
                                "source": "zec-fixture",
                                "message": "deterministic fixture warning"
                            })]
                        };
                        notify(
                            &mut writer,
                            log_path.as_deref(),
                            "textDocument/publishDiagnostics",
                            json!({
                                "uri": uri,
                                "version": params.pointer("/textDocument/version"),
                                "diagnostics": diagnostics
                            }),
                        )?;
                    }
                }
            }
            Some("textDocument/didChange") => {
                if let Some(uri) = params.pointer("/textDocument/uri").and_then(Value::as_str)
                    && let Some(text) = params
                        .pointer("/contentChanges")
                        .and_then(Value::as_array)
                        .and_then(|changes| changes.last())
                        .and_then(|change| change.get("text"))
                        .and_then(Value::as_str)
                {
                    documents.insert(uri.to_owned(), text.to_owned());
                }
            }
            Some("textDocument/didClose") => {
                if let Some(uri) = params.pointer("/textDocument/uri").and_then(Value::as_str) {
                    documents.remove(uri);
                    notify(
                        &mut writer,
                        log_path.as_deref(),
                        "textDocument/publishDiagnostics",
                        json!({ "uri": uri, "diagnostics": [] }),
                    )?;
                }
            }
            Some("textDocument/completion") => match scenario.as_str() {
                "request-error" => error_response(
                    &mut writer,
                    log_path.as_deref(),
                    id,
                    -32003,
                    "controlled completion failure".to_owned(),
                )?,
                "hang-request" => {}
                "crash-request" => std::process::exit(44),
                "malformed-response" => {
                    writer
                        .write_all(b"Content-Length: 4\r\n\r\nnope")
                        .context("write malformed completion response")?;
                    writer
                        .flush()
                        .context("flush malformed completion response")?;
                    break;
                }
                "large-payloads" => respond(
                    &mut writer,
                    log_path.as_deref(),
                    id,
                    large_completion_result(),
                )?,
                _ => respond(&mut writer, log_path.as_deref(), id, completion_result())?,
            },
            Some("completionItem/resolve") => {
                let mut item = params;
                if let Some(object) = item.as_object_mut() {
                    object.insert(
                        "documentation".to_owned(),
                        json!({
                            "kind": "markdown",
                            "value": "Resolved fixture documentation with [link](https://example.invalid/fixture)."
                        }),
                    );
                }
                respond(&mut writer, log_path.as_deref(), id, item)?;
            }
            Some("textDocument/hover") => {
                let value = if scenario == "large-payloads" {
                    "H".repeat(64 * 1024)
                } else {
                    "`stub_completion: fn()`\n\nFixture hover with [docs](https://example.invalid/hover)."
                        .to_owned()
                };
                respond(
                    &mut writer,
                    log_path.as_deref(),
                    id,
                    json!({
                        "contents": { "kind": "markdown", "value": value },
                        "range": request_range(&params)
                    }),
                )?;
            }
            Some("textDocument/inlayHint") => respond(
                &mut writer,
                log_path.as_deref(),
                id,
                json!([
                    {
                        "position": { "line": 0, "character": 18 },
                        "label": ": fixture_type",
                        "kind": 1,
                        "paddingLeft": false,
                        "paddingRight": false,
                        "tooltip": "Deterministic fixture inlay hint"
                    }
                ]),
            )?,
            Some("textDocument/definition") | Some("textDocument/typeDefinition") => respond(
                &mut writer,
                log_path.as_deref(),
                id,
                location_for_request(&params),
            )?,
            Some("textDocument/references") => {
                let location = location_for_request(&params);
                let peer = documents
                    .iter()
                    .find_map(|(uri, text)| {
                        uri.ends_with("/lib.rs").then(|| {
                            json!({
                                "uri": uri,
                                "range": stub_range(text).unwrap_or_else(fixture_range)
                            })
                        })
                    })
                    .or_else(|| {
                        request_uri(&params).and_then(|uri| {
                            uri.strip_suffix("/main.rs").map(|root| {
                                json!({
                                    "uri": format!("{root}/lib.rs"),
                                    "range": {
                                        "start": { "line": 1, "character": 16 },
                                        "end": { "line": 1, "character": 22 }
                                    }
                                })
                            })
                        })
                    });
                let mut locations = vec![location.clone(), location];
                if let Some(peer) = peer {
                    locations.push(peer);
                }
                respond(&mut writer, log_path.as_deref(), id, json!(locations))?;
            }
            Some("textDocument/prepareRename") => respond(
                &mut writer,
                log_path.as_deref(),
                id,
                json!({
                    "range": stub_range_for_request(&params, &documents),
                    "placeholder": "stub_"
                }),
            )?,
            Some("textDocument/rename") => {
                let new_name = params
                    .get("newName")
                    .and_then(Value::as_str)
                    .unwrap_or("renamed_fixture");
                let changes = documents
                    .iter()
                    .filter_map(|(uri, text)| {
                        stub_range(text).map(|range| {
                            (
                                uri.clone(),
                                json!([{
                                    "range": range,
                                    "newText": new_name
                                }]),
                            )
                        })
                    })
                    .collect::<BTreeMap<_, _>>();
                respond(
                    &mut writer,
                    log_path.as_deref(),
                    id,
                    json!({ "changes": changes }),
                )?;
            }
            Some("textDocument/codeAction") => respond(
                &mut writer,
                log_path.as_deref(),
                id,
                json!([{
                    "title": "Apply fixture quick fix",
                    "kind": "quickfix",
                    "isPreferred": true,
                    "edit": {
                        "changes": {
                            request_uri(&params).unwrap_or_default(): [{
                                "range": stub_range_for_request(&params, &documents),
                                "newText": "fixture_fixed"
                            }]
                        }
                    }
                }]),
            )?,
            Some("textDocument/formatting") | Some("textDocument/rangeFormatting") => {
                if scenario == "formatter-error" {
                    error_response(
                        &mut writer,
                        log_path.as_deref(),
                        id,
                        -32004,
                        "controlled formatter failure".to_owned(),
                    )?;
                    continue;
                }
                let uri = request_uri(&params).unwrap_or_default();
                let text = documents.get(&uri).cloned().unwrap_or_default();
                let formatted = text
                    .lines()
                    .map(str::trim_end)
                    .collect::<Vec<_>>()
                    .join("\n");
                respond(
                    &mut writer,
                    log_path.as_deref(),
                    id,
                    json!([{
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 2147483647u32, "character": 0 }
                        },
                        "newText": format!("{formatted}\n")
                    }]),
                )?;
            }
            Some("workspace/symbol") => {
                let uri = documents
                    .keys()
                    .find(|uri| uri.ends_with("/main.rs"))
                    .or_else(|| documents.keys().next())
                    .cloned()
                    .unwrap_or_default();
                respond(
                    &mut writer,
                    log_path.as_deref(),
                    id,
                    json!([{
                        "name": "stub_completion",
                        "kind": 12,
                        "location": {
                            "uri": uri,
                            "range": fixture_range()
                        },
                        "containerName": "fixture"
                    }]),
                )?;
            }
            Some("shutdown") => {
                shutdown_requested = true;
                respond(&mut writer, log_path.as_deref(), id, Value::Null)?;
            }
            Some("exit") => {
                ensure!(shutdown_requested, "exit arrived before shutdown");
                break;
            }
            Some("$/cancelRequest")
            | Some("textDocument/didSave")
            | Some("workspace/didChangeConfiguration")
            | Some("workspace/didChangeWorkspaceFolders")
            | Some("window/workDoneProgress/cancel") => {}
            Some(method) if id.is_some() => error_response(
                &mut writer,
                log_path.as_deref(),
                id,
                -32601,
                format!("fixture method not implemented: {method}"),
            )?,
            Some(_) => {}
            None => bail!("JSON-RPC message has neither method nor a recognized response"),
        }
    }

    Ok(())
}

fn initialize_result() -> Value {
    json!({
        "capabilities": {
            "positionEncoding": "utf-16",
            "textDocumentSync": {
                "openClose": true,
                "change": 1,
                "save": { "includeText": true }
            },
            "completionProvider": {
                "resolveProvider": true,
                "triggerCharacters": ["."]
            },
            "hoverProvider": true,
            "inlayHintProvider": { "resolveProvider": false },
            "definitionProvider": true,
            "typeDefinitionProvider": true,
            "referencesProvider": true,
            "renameProvider": { "prepareProvider": true },
            "codeActionProvider": {
                "codeActionKinds": ["quickfix", "refactor.rename"]
            },
            "documentFormattingProvider": true,
            "documentRangeFormattingProvider": true,
            "workspaceSymbolProvider": true
        },
        "serverInfo": {
            "name": "zec-fixture-lsp",
            "version": "1.0.0"
        }
    })
}

fn completion_result() -> Value {
    json!({
        "isIncomplete": false,
        "items": [
            {
                "label": "stub_completion",
                "kind": 3,
                "detail": "fn stub_completion()",
                "documentation": {
                    "kind": "markdown",
                    "value": "Fixture **completion** documentation."
                },
                "insertText": "stub_completion()",
                "insertTextFormat": 1,
                "sortText": "001",
                "data": { "fixture": "stub" }
            },
            {
                "label": "beta_completion",
                "kind": 6,
                "detail": "u32",
                "documentation": "Fixture beta documentation.",
                "insertText": "beta_completion",
                "sortText": "002",
                "data": { "fixture": "beta" }
            }
        ]
    })
}

fn large_completion_result() -> Value {
    let documentation = "D".repeat(64 * 1024);
    let items = (0..10_000)
        .map(|index| {
            json!({
                "label": format!("completion_{index:05}"),
                "kind": 6,
                "detail": "bounded fixture item",
                "documentation": if index == 0 {
                    documentation.clone()
                } else {
                    String::new()
                },
                "insertText": format!("completion_{index:05}"),
                "sortText": format!("{index:05}")
            })
        })
        .collect::<Vec<_>>();
    json!({ "isIncomplete": false, "items": items })
}

fn read_message(reader: &mut impl BufRead) -> Result<Option<Value>> {
    let mut content_length = None;
    loop {
        let mut header = String::new();
        let bytes = reader.read_line(&mut header).context("read LSP header")?;
        if bytes == 0 {
            if content_length.is_none() {
                return Ok(None);
            }
            bail!("unexpected EOF in LSP headers");
        }
        if header == "\r\n" || header == "\n" {
            break;
        }
        if let Some((name, value)) = header.trim_end().split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .context("parse Content-Length")?,
            );
        }
    }

    let content_length = content_length.context("LSP message omitted Content-Length")?;
    ensure!(
        content_length <= MAX_MESSAGE_BYTES,
        "LSP message exceeded {MAX_MESSAGE_BYTES} bytes"
    );
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body).context("read LSP body")?;
    serde_json::from_slice(&body)
        .context("parse LSP JSON")
        .map(Some)
}

fn respond(
    writer: &mut impl Write,
    log_path: Option<&Path>,
    id: Option<Value>,
    result: Value,
) -> Result<()> {
    let id = id.context("fixture received request without id")?;
    write_message(
        writer,
        log_path,
        json!({ "jsonrpc": "2.0", "id": id, "result": result }),
    )
}

fn error_response(
    writer: &mut impl Write,
    log_path: Option<&Path>,
    id: Option<Value>,
    code: i64,
    message: String,
) -> Result<()> {
    let id = id.context("fixture received request without id")?;
    write_message(
        writer,
        log_path,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message }
        }),
    )
}

fn notify(
    writer: &mut impl Write,
    log_path: Option<&Path>,
    method: &str,
    params: Value,
) -> Result<()> {
    write_message(
        writer,
        log_path,
        json!({ "jsonrpc": "2.0", "method": method, "params": params }),
    )
}

fn write_message(writer: &mut impl Write, log_path: Option<&Path>, message: Value) -> Result<()> {
    append_log(log_path, "server", &message)?;
    let body = serde_json::to_vec(&message).context("serialize LSP JSON")?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len()).context("write LSP header")?;
    writer.write_all(&body).context("write LSP body")?;
    writer.flush().context("flush LSP response")
}

fn append_log(path: Option<&Path>, direction: &str, message: &Value) -> Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open fixture log {}", path.display()))?;
    let mut entry = serde_json::to_vec(&json!({ "direction": direction, "message": message }))
        .context("serialize fixture log entry")?;
    entry.push(b'\n');
    file.write_all(&entry)
        .context("append one complete fixture log entry")
}

fn request_uri(params: &Value) -> Option<String> {
    params
        .pointer("/textDocument/uri")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn fixture_range() -> Value {
    json!({
        "start": { "line": 0, "character": 3 },
        "end": { "line": 0, "character": 19 }
    })
}

fn request_range(params: &Value) -> Value {
    let position = params
        .get("position")
        .cloned()
        .unwrap_or_else(|| json!({ "line": 0, "character": 3 }));
    json!({ "start": position, "end": position })
}

fn stub_range_for_request(params: &Value, documents: &BTreeMap<String, String>) -> Value {
    request_uri(params)
        .and_then(|uri| documents.get(&uri))
        .and_then(|text| stub_range(text))
        .unwrap_or_else(|| request_range(params))
}

fn stub_range(text: &str) -> Option<Value> {
    let start = text.find("stub_")?;
    let end = start + "stub_".len();
    let point = |offset: usize| {
        let prefix = &text[..offset];
        let line = prefix.bytes().filter(|byte| *byte == b'\n').count();
        let line_start = prefix.rfind('\n').map_or(0, |newline| newline + 1);
        let character = text[line_start..offset].encode_utf16().count();
        json!({ "line": line, "character": character })
    };
    Some(json!({ "start": point(start), "end": point(end) }))
}

fn location_for_request(params: &Value) -> Value {
    json!({
        "uri": request_uri(params).unwrap_or_default(),
        "range": fixture_range()
    })
}
