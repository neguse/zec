#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::{PermissionsExt as _, symlink},
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;

#[test]
fn zed_project_discovers_and_uses_fixture_rust_analyzer() {
    let temp = tempfile::tempdir().expect("create Alpha 2 test directory");
    let root = temp.path().join("project");
    let source_dir = root.join("src");
    let zed_dir = root.join(".zed");
    let bin_dir = temp.path().join("bin");
    let home = temp.path().join("home");
    let xdg_config = temp.path().join("xdg-config");
    let xdg_data = temp.path().join("xdg-data");
    let xdg_cache = temp.path().join("xdg-cache");
    let xdg_state = temp.path().join("xdg-state");
    let rustup_home = temp.path().join("rustup");
    let cargo_home = temp.path().join("cargo");
    for directory in [
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
        fs::create_dir_all(directory).expect("create Alpha 2 fixture directory");
    }
    fs::write(
        xdg_config.join("zed/settings.json"),
        r#"{"session":{"trust_all_worktrees":true}}"#,
    )
    .expect("write isolated trusted user settings");
    fs::write(xdg_config.join("zed/global_settings.json"), "{}")
        .expect("write isolated global settings");
    fs::write(xdg_config.join("zed/keymap.json"), "[]").expect("write isolated keymap");
    fs::write(
        zed_dir.join("settings.json"),
        r#"{
          "format_on_save": "off",
          "remove_trailing_whitespace_on_save": false,
          "ensure_final_newline_on_save": false
        }"#,
    )
    .expect("write Alpha 2 fixture project settings");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"alpha-2-test\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[workspace]\n",
    )
    .expect("write fixture Cargo.toml");
    let source = source_dir.join("main.rs");
    fs::write(&source, "fn main() {\n    let _value = alpha_;\n}   \n")
        .expect("write fixture Rust source");
    fs::write(
        source_dir.join("lib.rs"),
        "pub fn fixture_peer() {\n    let _peer = alpha_;\n}  \n",
    )
    .expect("write fixture peer source");

    let fixture_server = Path::new(env!("CARGO_BIN_EXE_alpha_2_fixture_lsp"));
    symlink(fixture_server, bin_dir.join("rust-analyzer")).expect("link fixture as rust-analyzer");
    let fake_rustup = bin_dir.join("rustup");
    fs::write(&fake_rustup, "#!/bin/sh\nexit 1\n").expect("write fake rustup");
    let mut permissions = fs::metadata(&fake_rustup)
        .expect("read fake rustup metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&fake_rustup, permissions).expect("make fake rustup executable");

    let log = temp.path().join("lsp.jsonl");
    let mut child = Command::new(env!("CARGO_BIN_EXE_zec"))
        .args([
            "--alpha-2-probe",
            "language-service",
            root.to_str().expect("UTF-8 fixture root"),
            source.to_str().expect("UTF-8 fixture source"),
        ])
        .env(
            "PATH",
            format!("{}:/usr/local/bin:/usr/bin:/bin", bin_dir.display()),
        )
        .env("RUSTUP_HOME", &rustup_home)
        .env("CARGO_HOME", &cargo_home)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("XDG_CACHE_HOME", &xdg_cache)
        .env("XDG_STATE_HOME", &xdg_state)
        .env("ZEC_ALPHA2_LSP_LOG", &log)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn zec Alpha 2 probe");

    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        if child.try_wait().expect("poll zec Alpha 2 probe").is_some() {
            break;
        }
        if Instant::now() >= deadline {
            child.kill().expect("kill timed-out zec Alpha 2 probe");
            let output = child.wait_with_output().expect("collect timed-out probe");
            panic!(
                "Alpha 2 probe exceeded 25 seconds\nstdout={}\nstderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(25));
    }
    let output = child.wait_with_output().expect("collect Alpha 2 probe");
    assert!(
        output.status.success(),
        "Alpha 2 probe failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let report: Value =
        serde_json::from_slice(&output.stdout).expect("parse Alpha 2 probe JSON output");
    assert_eq!(report["language"], "Rust");
    assert_eq!(report["servers"][0]["name"], "rust-analyzer");
    assert!(report["servers"][0]["process_id"].is_number());
    assert_eq!(report["completions"][0]["new_text"], "alpha_completion()");
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
        "fn main() {\n    let _value = alpha_completion();\n}   \n"
    );
    assert_eq!(
        report["terminal_completion"]["text_after_undo"],
        "fn main() {\n    let _value = alpha_;\n}   \n"
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
            .contains("alpha_completion")
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
        "alpha_"
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
        "fn main() {\n    let _value = alpha_;\n}   \n"
    );
    assert_eq!(
        report["terminal_edits"]["rename"]["peer_after_undo"],
        "pub fn fixture_peer() {\n    let _peer = alpha_;\n}  \n"
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
        "fn main() {\n    let _value = alpha_;\n}   \n"
    );
    for format_scope in ["format_document", "format_range"] {
        assert_eq!(
            report["terminal_edits"][format_scope]["after"],
            "fn main() {\n    let _value = alpha_;\n}\n"
        );
        assert_eq!(
            report["terminal_edits"][format_scope]["after_undo"],
            "fn main() {\n    let _value = alpha_;\n}   \n"
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
