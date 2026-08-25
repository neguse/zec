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
fn settings_keymap_and_format_on_save_reload_through_zed() {
    let temp = tempfile::tempdir().expect("create Alpha 2 settings test directory");
    let root = temp.path().join("project");
    let source_dir = root.join("src");
    let zed_dir = root.join(".zed");
    let bin_dir = temp.path().join("bin");
    let home = temp.path().join("home");
    let xdg_config = temp.path().join("xdg-config");
    let xdg_data = temp.path().join("xdg-data");
    let xdg_cache = temp.path().join("xdg-cache");
    let xdg_state = temp.path().join("xdg-state");
    let config_dir = xdg_config.join("zed");
    let rustup_home = temp.path().join("rustup");
    let cargo_home = temp.path().join("cargo");
    for directory in [
        &source_dir,
        &zed_dir,
        &bin_dir,
        &home,
        &config_dir,
        &xdg_data,
        &xdg_cache,
        &xdg_state,
        &rustup_home,
        &cargo_home,
    ] {
        fs::create_dir_all(directory).expect("create settings fixture directory");
    }

    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"alpha-2-settings\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[workspace]\n",
    )
    .expect("write fixture Cargo.toml");
    let source = source_dir.join("main.rs");
    fs::write(&source, "fn main() { let _value = alpha_; }   \n")
        .expect("write fixture Rust source");
    fs::write(
        zed_dir.join("settings.json"),
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
            "settings-reload",
            root.to_str().expect("UTF-8 fixture root"),
            source.to_str().expect("UTF-8 fixture source"),
        ])
        .env(
            "PATH",
            format!("{}:/usr/local/bin:/usr/bin:/bin", bin_dir.display()),
        )
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("XDG_CACHE_HOME", &xdg_cache)
        .env("XDG_STATE_HOME", &xdg_state)
        .env("RUSTUP_HOME", &rustup_home)
        .env("CARGO_HOME", &cargo_home)
        .env("ZEC_ALPHA2_LSP_LOG", &log)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn zec settings probe");

    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        if child.try_wait().expect("poll settings probe").is_some() {
            break;
        }
        if Instant::now() >= deadline {
            child.kill().expect("kill timed-out settings probe");
            let output = child.wait_with_output().expect("collect timed-out probe");
            panic!(
                "Alpha 2 settings probe exceeded 25 seconds\nstdout={}\nstderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(25));
    }
    let output = child.wait_with_output().expect("collect settings probe");
    assert!(
        output.status.success(),
        "Alpha 2 settings probe failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let report: Value =
        serde_json::from_slice(&output.stdout).expect("parse settings probe JSON output");
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
