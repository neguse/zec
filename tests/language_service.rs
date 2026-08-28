#[path = "support/probe.rs"]
mod support;

use std::{fs, path::Path, time::Duration};

use serde_json::Value;
use support::ProbeEnvironment;

#[test]
fn zed_project_discovers_and_uses_fixture_rust_analyzer() {
    let probe_env = ProbeEnvironment::new("language-service-fixture");
    fs::write(
        probe_env.user_config.join("settings.json"),
        r#"{"session":{"trust_all_worktrees":true},"edit_predictions":{"provider":"none"}}"#,
    )
    .expect("write isolated trusted user settings");
    fs::write(probe_env.user_config.join("global_settings.json"), "{}")
        .expect("write isolated global settings");
    fs::write(probe_env.user_config.join("keymap.json"), "[]").expect("write isolated keymap");
    fs::write(
        probe_env.zed_dir.join("settings.json"),
        r#"{
          "edit_predictions": { "provider": "none" },
          "format_on_save": "off",
          "remove_trailing_whitespace_on_save": false,
          "ensure_final_newline_on_save": false
        }"#,
    )
    .expect("write language fixture project settings");
    let source = probe_env.source_dir.join("main.rs");
    fs::write(&source, "fn main() {\n    let _value = stub_;\n}   \n")
        .expect("write fixture Rust source");
    fs::write(
        probe_env.source_dir.join("lib.rs"),
        "pub fn fixture_peer() {\n    let _peer = stub_;\n}  \n",
    )
    .expect("write fixture peer source");
    probe_env.install_fixture_server(Path::new(env!("CARGO_BIN_EXE_fixture_lsp")));

    let log = probe_env.log.clone();
    let report = probe_env.run_probe(
        Path::new(env!("CARGO_BIN_EXE_zec")),
        "language-service",
        &source,
        &[],
        Duration::from_secs(25),
        "language probe",
    );
    assert_eq!(report["language"], "Rust");
    assert_eq!(report["servers"][0]["name"], "rust-analyzer");
    assert!(report["servers"][0]["process_id"].is_number());
    assert_eq!(report["completions"][0]["new_text"], "stub_completion()");
    assert_eq!(report["completions"][1]["new_text"], "beta_completion");
    assert_eq!(report["hover"][0]["kind"], "Markdown");
    assert!(
        report["hover"][0]["text"]
            .as_str()
            .expect("hover text")
            .contains("Fixture hover")
    );
    assert_eq!(report["diagnostics"]["errors"], 0);
    assert_eq!(report["diagnostics"]["warnings"], 1);
    assert_eq!(
        report["terminal_completion"]["items"][0]["documentation"],
        "Fixture **completion** documentation."
    );
    assert_eq!(
        report["terminal_completion"]["text_after_apply"],
        "fn main() {\n    let _value = stub_completion();\n}   \n"
    );
    assert_eq!(
        report["terminal_completion"]["text_after_undo"],
        "fn main() {\n    let _value = stub_;\n}   \n"
    );
    assert_eq!(report["terminal_hover"]["items"][0]["kind"], "Markdown");
    assert!(
        report["terminal_hover"]["items"][0]["text"]
            .as_str()
            .expect("terminal hover text")
            .contains("Fixture hover")
    );
    assert!(
        report["terminal_hover"]["overlay_rows"]
            .as_array()
            .expect("terminal hover overlay rows")
            .iter()
            .any(|row| row
                .as_str()
                .is_some_and(|row| row.contains("Fixture hover")))
    );
    assert_eq!(
        report["terminal_diagnostics"]["items"][0]["label"],
        "src/main.rs"
    );
    assert_eq!(report["terminal_diagnostics"]["items"][0]["severity"], "W");
    assert_eq!(report["terminal_diagnostics"]["items"][0]["row"], 0);
    assert_eq!(report["terminal_diagnostics"]["items"][0]["column"], 3);
    assert_eq!(
        report["terminal_diagnostics"]["items"][0]["source"],
        "zec-fixture"
    );
    assert_eq!(report["terminal_diagnostics"]["cursor"]["row"], 0);
    assert_eq!(report["terminal_diagnostics"]["cursor"]["column"], 3);
    assert_eq!(
        report["terminal_locations"]["definitions"]["items"][0]["label"],
        "src/main.rs"
    );
    assert_eq!(
        report["terminal_locations"]["definitions"]["items"][0]["row"],
        0
    );
    assert_eq!(
        report["terminal_locations"]["definitions"]["items"][0]["column"],
        3
    );
    assert_eq!(
        report["terminal_locations"]["type_definitions"]["items"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    assert_eq!(
        report["terminal_locations"]["references"]["items"]
            .as_array()
            .map(Vec::len),
        Some(2),
        "Zed must deduplicate the repeated main location and retain the peer location"
    );
    assert_eq!(
        report["terminal_locations"]["project_symbols"]["items"][0]["label"],
        "src/main.rs"
    );
    assert!(
        report["terminal_locations"]["project_symbols"]["items"][0]["snippet"]
            .as_str()
            .expect("project symbol snippet")
            .contains("stub_completion")
    );
    assert_eq!(report["semantic_navigation"]["cursor"]["row"], 0);
    assert_eq!(report["semantic_navigation"]["cursor"]["column"], 3);
    assert_eq!(report["terminal_multibuffer"]["title"], "References");
    assert_eq!(report["terminal_multibuffer"]["target_count"], 2);
    assert_eq!(report["terminal_multibuffer"]["source_count"], 2);
    assert_eq!(
        report["terminal_multibuffer"]["snapshot_contains_main"],
        true
    );
    assert_eq!(
        report["terminal_multibuffer"]["snapshot_contains_peer"],
        true
    );
    assert_ne!(
        report["terminal_multibuffer"]["source_after_edit"],
        report["terminal_multibuffer"]["source_before"],
        "editing the MultiBuffer must mutate its source Buffer"
    );
    assert_eq!(
        report["terminal_multibuffer"]["disk_after_save"],
        report["terminal_multibuffer"]["source_after_edit"]
    );
    assert_eq!(
        report["terminal_multibuffer"]["source_after_undo"],
        report["terminal_multibuffer"]["source_before"]
    );
    assert_eq!(
        report["terminal_multibuffer"]["disk_after_restore"],
        report["terminal_multibuffer"]["source_before"]
    );
    assert_eq!(
        report["terminal_edits"]["rename"]["preparation"]["placeholder"],
        "stub_"
    );
    assert_eq!(
        report["terminal_edits"]["rename"]["preview"]["read_only"],
        true
    );
    assert_eq!(
        report["terminal_edits"]["rename"]["preview"]["source_count"],
        2
    );
    assert_eq!(
        report["terminal_edits"]["rename"]["preview"]["buffer_count"],
        2
    );
    assert_eq!(
        report["terminal_edits"]["rename"]["preview"]["edit_count"],
        2
    );
    assert_eq!(
        report["terminal_edits"]["rename"]["preview"]["file_operation_count"],
        0
    );
    for field in [
        "confirmation_signature_matches",
        "contains_main",
        "contains_peer",
        "contains_old_text",
        "contains_new_text",
        "rejected_contains_new_text",
        "rejection_unchanged",
    ] {
        assert_eq!(
            report["terminal_edits"]["rename"]["preview"][field], true,
            "rename preview field {field}"
        );
    }
    assert_eq!(report["terminal_edits"]["rename"]["buffer_count"], 2);
    assert_eq!(report["terminal_edits"]["rename"]["undo_buffer_count"], 2);
    assert_eq!(report["terminal_edits"]["rename"]["redo_buffer_count"], 2);
    assert_eq!(
        report["terminal_edits"]["rename"]["main_after"],
        "fn main() {\n    let _value = renamed_fixture;\n}   \n"
    );
    assert_eq!(
        report["terminal_edits"]["rename"]["peer_after"],
        "pub fn fixture_peer() {\n    let _peer = renamed_fixture;\n}  \n"
    );
    assert_eq!(
        report["terminal_edits"]["rename"]["main_after_undo"],
        "fn main() {\n    let _value = stub_;\n}   \n"
    );
    assert_eq!(
        report["terminal_edits"]["rename"]["peer_after_undo"],
        "pub fn fixture_peer() {\n    let _peer = stub_;\n}  \n"
    );
    assert_eq!(
        report["terminal_edits"]["rename"]["main_after_redo"],
        "fn main() {\n    let _value = renamed_fixture;\n}   \n"
    );
    assert_eq!(
        report["terminal_edits"]["code_action"]["title"],
        "Apply fixture quick fix"
    );
    assert_eq!(report["terminal_edits"]["code_action"]["kind"], "quickfix");
    assert_eq!(report["terminal_edits"]["code_action"]["preferred"], true);
    assert_eq!(
        report["terminal_edits"]["code_action"]["main_after"],
        "fn main() {\n    let _value = fixture_fixed;\n}   \n"
    );
    assert_eq!(
        report["terminal_edits"]["code_action"]["main_after_undo"],
        "fn main() {\n    let _value = stub_;\n}   \n"
    );
    for format_scope in ["format_document", "format_range"] {
        assert_eq!(
            report["terminal_edits"][format_scope]["after"],
            "fn main() {\n    let _value = stub_;\n}\n"
        );
        assert_eq!(
            report["terminal_edits"][format_scope]["after_undo"],
            "fn main() {\n    let _value = stub_;\n}   \n"
        );
        assert_eq!(
            report["terminal_edits"][format_scope]["undo_buffer_count"],
            1
        );
    }

    let entries = fs::read_to_string(&log).expect("read fixture LSP trace");
    let messages = entries
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("parse fixture JSONL entry"))
        .collect::<Vec<_>>();
    let client_methods = messages
        .iter()
        .filter(|entry| entry["direction"] == "client")
        .filter_map(|entry| entry.pointer("/message/method").and_then(Value::as_str))
        .collect::<Vec<_>>();
    for required in [
        "initialize",
        "initialized",
        "textDocument/didOpen",
        "textDocument/completion",
        "textDocument/hover",
        "textDocument/didClose",
        "shutdown",
    ] {
        assert!(
            client_methods.contains(&required),
            "missing {required:?} in {client_methods:?}"
        );
    }
    assert_eq!(
        client_methods
            .iter()
            .filter(|method| **method == "initialize")
            .count(),
        1,
        "language server was unexpectedly restarted"
    );
    assert_eq!(
        client_methods
            .iter()
            .filter(|method| **method == "textDocument/completion")
            .count(),
        2,
        "direct and terminal-projected completion requests must both run"
    );
    assert_eq!(
        client_methods
            .iter()
            .filter(|method| **method == "textDocument/hover")
            .count(),
        2,
        "direct and terminal-projected hover requests must both run"
    );
    for required in [
        "textDocument/definition",
        "textDocument/typeDefinition",
        "textDocument/references",
        "workspace/symbol",
    ] {
        assert_eq!(
            client_methods
                .iter()
                .filter(|method| **method == required)
                .count(),
            1,
            "semantic request {required} must run exactly once"
        );
    }
    for required in [
        "textDocument/prepareRename",
        "textDocument/codeAction",
        "textDocument/formatting",
        "textDocument/rangeFormatting",
    ] {
        assert!(
            client_methods
                .iter()
                .filter(|method| **method == required)
                .count()
                >= 1,
            "editing request {required} must run"
        );
    }
    assert!(
        client_methods
            .iter()
            .filter(|method| **method == "textDocument/rename")
            .count()
            >= 4,
        "rename must run for rejected preview, accepted preview, drift check, and Project apply"
    );
    assert!(messages.iter().any(|entry| {
        entry["direction"] == "server"
            && entry.pointer("/message/method")
                == Some(&Value::String("textDocument/publishDiagnostics".to_owned()))
    }));
}
