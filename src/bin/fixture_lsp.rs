//! A deterministic LSP 3.17 server for the PTY tests.
//!
//! It answers every request with fixed content: two completions, one hover,
//! one definition, references in every open `lib.rs`, a rename and a quick
//! fix over the first `stub_` in the document, and one warning published
//! for every `main.rs` that opens. Nothing depends on the request position
//! except the ranges echoed back, so a test can assert exact screen text.

use std::{
    collections::BTreeMap,
    env,
    io::{self, BufRead, BufReader, Write},
};

use anyhow::{Context as _, Result, bail, ensure};
use serde_json::{Value, json};

const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

fn main() -> Result<()> {
    if env::args_os().skip(1).any(|argument| argument == "--help") {
        println!("fixture_lsp: deterministic LSP 3.17 fixture server");
        return Ok(());
    }
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = stdout.lock();
    let mut documents = BTreeMap::<String, String>::new();
    let mut shutdown_requested = false;

    while let Some(message) = read_message(&mut reader)? {
        let method = message.get("method").and_then(Value::as_str);
        let id = message.get("id").cloned();
        let params = message.get("params").cloned().unwrap_or(Value::Null);

        match method {
            Some("initialize") => respond(&mut writer, id, initialize_result())?,
            Some("textDocument/didOpen") => {
                if let (Some(uri), Some(text)) = (
                    params.pointer("/textDocument/uri").and_then(Value::as_str),
                    params.pointer("/textDocument/text").and_then(Value::as_str),
                ) {
                    documents.insert(uri.to_owned(), text.to_owned());
                    if uri.ends_with("/main.rs") {
                        notify(
                            &mut writer,
                            "textDocument/publishDiagnostics",
                            json!({
                                "uri": uri,
                                "version": params.pointer("/textDocument/version"),
                                "diagnostics": [{
                                    "range": {
                                        "start": { "line": 0, "character": 3 },
                                        "end": { "line": 0, "character": 8 }
                                    },
                                    "severity": 2,
                                    "code": "fixture-warning",
                                    "source": "zec-fixture",
                                    "message": "deterministic fixture warning"
                                }]
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
                        "textDocument/publishDiagnostics",
                        json!({ "uri": uri, "diagnostics": [] }),
                    )?;
                }
            }
            Some("textDocument/completion") => respond(&mut writer, id, completion_result())?,
            Some("completionItem/resolve") => respond(&mut writer, id, params)?,
            Some("textDocument/hover") => respond(
                &mut writer,
                id,
                json!({
                    "contents": {
                        "kind": "markdown",
                        "value": "`stub_completion: fn()`\n\nFixture hover with [docs](https://example.invalid/hover)."
                    },
                    "range": request_range(&params)
                }),
            )?,
            Some("textDocument/definition") | Some("textDocument/typeDefinition") => {
                respond(&mut writer, id, location_for_request(&params))?
            }
            Some("textDocument/references") => {
                let location = location_for_request(&params);
                let peers = documents
                    .iter()
                    .filter(|(uri, _)| uri.ends_with("/lib.rs"))
                    .map(|(uri, text)| {
                        json!({
                            "uri": uri,
                            "range": stub_range(text).unwrap_or_else(fixture_range)
                        })
                    });
                let locations = [location].into_iter().chain(peers).collect::<Vec<_>>();
                respond(&mut writer, id, json!(locations))?;
            }
            Some("textDocument/prepareRename") => respond(
                &mut writer,
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
                                json!([{ "range": range, "newText": new_name }]),
                            )
                        })
                    })
                    .collect::<BTreeMap<_, _>>();
                respond(&mut writer, id, json!({ "changes": changes }))?;
            }
            Some("textDocument/codeAction") => respond(
                &mut writer,
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
            Some("shutdown") => {
                shutdown_requested = true;
                respond(&mut writer, id, Value::Null)?;
            }
            Some("exit") => {
                ensure!(shutdown_requested, "exit arrived before shutdown");
                break;
            }
            Some(method) if id.is_some() => error_response(
                &mut writer,
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
            "definitionProvider": true,
            "typeDefinitionProvider": true,
            "referencesProvider": true,
            "renameProvider": { "prepareProvider": true },
            "codeActionProvider": {
                "codeActionKinds": ["quickfix"]
            }
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
                "sortText": "001"
            },
            {
                "label": "beta_completion",
                "kind": 6,
                "detail": "u32",
                "documentation": "Fixture beta documentation.",
                "insertText": "beta_completion",
                "sortText": "002"
            }
        ]
    })
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

fn respond(writer: &mut impl Write, id: Option<Value>, result: Value) -> Result<()> {
    let id = id.context("fixture received request without id")?;
    write_message(
        writer,
        json!({ "jsonrpc": "2.0", "id": id, "result": result }),
    )
}

fn error_response(
    writer: &mut impl Write,
    id: Option<Value>,
    code: i64,
    message: String,
) -> Result<()> {
    let id = id.context("fixture received request without id")?;
    write_message(
        writer,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message }
        }),
    )
}

fn notify(writer: &mut impl Write, method: &str, params: Value) -> Result<()> {
    write_message(
        writer,
        json!({ "jsonrpc": "2.0", "method": method, "params": params }),
    )
}

fn write_message(writer: &mut impl Write, message: Value) -> Result<()> {
    let body = serde_json::to_vec(&message).context("serialize LSP JSON")?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len()).context("write LSP header")?;
    writer.write_all(&body).context("write LSP body")?;
    writer.flush().context("flush LSP response")
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

/// The range of the first `stub_` in `text`, in UTF-16 columns.
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
