#[path = "support/probe.rs"]
mod support;

use std::{fs, path::Path, time::Duration};

use serde_json::Value;
use support::ProbeEnvironment;

#[test]
fn settings_keymap_and_format_on_save_reload_through_zed() {
    let probe_env = ProbeEnvironment::new("settings-reload-fixture");
    let config_dir = probe_env.user_config.clone();

    let source = probe_env.source_dir.join("main.rs");
    fs::write(&source, "fn main() { let _value = stub_; }   \n")
        .expect("write fixture Rust source");
    fs::write(
        probe_env.zed_dir.join("settings.json"),
        r#"{
          "tab_size": 4,
          "format_on_save": "off",
          "completions": { "lsp": false },
          "show_completions_on_input": false,
          "languages": {
            "Rust": {
              "tab_size": 5,
              "format_on_save": "off",
              "completions": { "lsp": true },
              "show_completions_on_input": true
            }
          }
        }"#,
    )
    .expect("write project settings");
    fs::write(
        config_dir.join("global_settings.json"),
        r#"{
          "tab_size": 2,
          "format_on_save": "off",
          "completions": { "lsp": false },
          "show_completions_on_input": false
        }"#,
    )
    .expect("write global settings");
    fs::write(
        config_dir.join("settings.json"),
        r#"{
          "session": { "trust_all_worktrees": true },
          "tab_size": 3,
          "completions": { "lsp": true },
          "show_completions_on_input": false
        }"#,
    )
    .expect("write user settings");
    fs::write(
        config_dir.join("keymap.json"),
        r#"[
          {
            "context": "Editor",
            "bindings": {
              "f1": null,
              "ctrl-k ctrl-p": "command_palette::Toggle"
            }
          }
        ]"#,
    )
    .expect("write user keymap");

    probe_env.install_fixture_server(Path::new(env!("CARGO_BIN_EXE_fixture_lsp")));

    let log = probe_env.log.clone();
    let report = probe_env.run_probe(
        Path::new(env!("CARGO_BIN_EXE_zec")),
        "settings-reload",
        &source,
        &[],
        Duration::from_secs(25),
        "language settings probe",
    );
    assert_eq!(report["initial"]["tab_size"], 5);
    assert_eq!(report["initial"]["format_on_save"], "off");
    assert_eq!(report["initial"]["completion_lsp"], true);
    assert_eq!(report["initial"]["show_completions_on_input"], true);
    assert_eq!(report["updated"]["tab_size"], 7);
    assert_eq!(report["updated"]["format_on_save"], "on");
    assert_eq!(report["updated"]["completion_lsp"], false);
    assert_eq!(report["retained_after_invalid"], report["updated"]);
    assert_eq!(report["recovered"]["tab_size"], 9);
    assert_eq!(report["recovered"]["format_on_save"], "off");
    assert_eq!(report["recovered"]["completion_lsp"], true);
    assert_eq!(
        report["format_on_save_disk"],
        "fn main() { let value = 1; }\n"
    );
    assert_eq!(report["keymap"]["multi_chord_rebind"], true);
    assert_eq!(report["keymap"]["unbind"], true);
    assert_eq!(report["keymap"]["last_good_retained"], true);
    assert_eq!(report["keymap"]["replacement_rebind"], true);
    assert!(
        report["settings"]["invalid_error"]
            .as_str()
            .is_some_and(|error| error.contains("Failed to set local settings"))
    );
    assert!(
        report["keymap"]["invalid_error"]
            .as_str()
            .is_some_and(|error| error.contains("retained last valid bindings"))
    );

    let formatting_requests = fs::read_to_string(&log)
        .expect("read fixture LSP log")
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|entry| entry["direction"] == "client")
        .filter(|entry| entry["message"]["method"] == "textDocument/formatting")
        .count();
    assert_eq!(
        formatting_requests, 1,
        "one save with format-on-save enabled must produce exactly one formatting request; the later disabled save must produce none"
    );
}
