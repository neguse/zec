#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::{PermissionsExt as _, symlink},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;

const SCENARIOS: &[&str] = &[
    "server-not-found",
    "spawn-failure",
    "initialize-error",
    "malformed-frame",
    "unexpected-eof",
    "crash",
    "hang-initialize",
    "request-error",
    "malformed-response",
    "crash-request",
    "hang-request",
    "restart-once",
    "formatter-error",
    "large-payloads",
    "huge-stderr",
];

#[test]
fn lsp_failure_matrix_preserves_editor_and_reaps_processes() {
    for scenario in SCENARIOS {
        run_scenario(scenario);
    }
}

fn run_scenario(scenario: &str) {
    let temp = tempfile::tempdir().expect("create LSP failure fixture directory");
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
        fs::create_dir_all(directory).expect("create LSP failure fixture component");
    }
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"alpha-2-failure\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[workspace]\n",
    )
    .expect("write failure fixture Cargo.toml");
    let source = source_dir.join("main.rs");
    fs::write(&source, "fn main() { let _value = alpha_; }\n")
        .expect("write failure fixture source");
    let (format_on_save, formatter) = if scenario == "formatter-error" {
        ("on", ",\n              \"formatter\": \"language_server\"")
    } else {
        ("off", "")
    };
    fs::write(
        zed_dir.join("settings.json"),
        format!(
            r#"{{
              "format_on_save": "{format_on_save}"{formatter},
              "remove_trailing_whitespace_on_save": false,
              "ensure_final_newline_on_save": false
            }}"#
        ),
    )
    .expect("write failure fixture settings");
    fs::write(
        xdg_config.join("zed/settings.json"),
        r#"{"session":{"trust_all_worktrees":true}}"#,
    )
    .expect("write isolated trusted user settings");
    fs::write(xdg_config.join("zed/global_settings.json"), "{}")
        .expect("write isolated global settings");
    fs::write(xdg_config.join("zed/keymap.json"), "[]").expect("write isolated keymap");

    let fixture_server = Path::new(env!("CARGO_BIN_EXE_alpha_2_fixture_lsp"));
    match scenario {
        "server-not-found" => {}
        "spawn-failure" => {
            let wrapper = bin_dir.join("rust-analyzer");
            fs::write(
                &wrapper,
                "#!/bin/sh\nif [ \"${1:-}\" = \"--help\" ]; then\n  echo fixture\n  chmod 000 \"$0\"\n  exit 0\nfi\nexit 99\n",
            )
            .expect("write spawn-failure wrapper");
            let mut permissions = fs::metadata(&wrapper)
                .expect("read wrapper permissions")
                .permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&wrapper, permissions).expect("make wrapper executable");
        }
        _ => symlink(fixture_server, bin_dir.join("rust-analyzer"))
            .expect("link fixture as rust-analyzer"),
    }
    let fake_rustup = bin_dir.join("rustup");
    fs::write(&fake_rustup, "#!/bin/sh\nexit 1\n").expect("write fake rustup");
    let mut permissions = fs::metadata(&fake_rustup)
        .expect("read fake rustup permissions")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&fake_rustup, permissions).expect("make fake rustup executable");

    let log = temp.path().join("lsp.jsonl");
    let restart_state = temp.path().join("restart-state");
    let fixture_scenario = match scenario {
        "server-not-found" | "spawn-failure" => "normal",
        scenario => scenario,
    };
    let mut child = Command::new(env!("CARGO_BIN_EXE_zec"))
        .args([
            "--alpha-2-probe",
            "lsp-failure",
            root.to_str().expect("UTF-8 failure root"),
            source.to_str().expect("UTF-8 failure source"),
            scenario,
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
        .env("ZEC_ALPHA2_LSP_SCENARIO", fixture_scenario)
        .env("ZEC_ALPHA2_RESTART_STATE", &restart_state)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("spawn zec failure scenario {scenario}: {error}"));

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if child
            .try_wait()
            .unwrap_or_else(|error| panic!("poll {scenario}: {error}"))
            .is_some()
        {
            break;
        }
        if Instant::now() >= deadline {
            child
                .kill()
                .unwrap_or_else(|error| panic!("kill timed-out {scenario}: {error}"));
            let output = child
                .wait_with_output()
                .unwrap_or_else(|error| panic!("collect timed-out {scenario}: {error}"));
            panic!(
                "LSP failure scenario {scenario} exceeded 30 seconds\nstdout={}\nstderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(25));
    }
    let output = child
        .wait_with_output()
        .unwrap_or_else(|error| panic!("collect {scenario}: {error}"));
    assert!(
        output.status.success(),
        "LSP failure scenario {scenario} failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("parse {scenario} report: {error}"));
    assert_eq!(report["scenario"], scenario);
    for field in [
        "dirty_after_edit",
        "undo_restored",
        "redo_restored",
        "saved",
    ] {
        assert_eq!(
            report["editor"][field], true,
            "scenario {scenario} editor field {field}"
        );
    }
    assert!(
        report["editor"]["close_elapsed_ms"]
            .as_u64()
            .is_some_and(|elapsed| elapsed <= 250),
        "scenario {scenario} close latency: {}",
        report["editor"]["close_elapsed_ms"]
    );
    let service_notices = report["service_notices"]
        .as_array()
        .expect("service notice array");
    if service_notices.is_empty() {
        assert!(
            report["initially_ready"] == false
                && report["request"]["user_message"]
                    == "completion unavailable: no ready language server",
            "scenario {scenario} produced no user-facing language service status"
        );
    }
    assert!(service_notices.iter().all(|notice| {
        notice["level"]
            .as_str()
            .is_some_and(|level| !level.is_empty())
            && notice["message"]
                .as_str()
                .is_some_and(|message| message.len() <= 64 * 1024)
    }));

    let outcome = report["request"]["outcome"]
        .as_str()
        .expect("request outcome");
    match scenario {
        "server-not-found" | "spawn-failure" => {
            assert_eq!(report["initially_ready"], false);
            assert!(
                outcome == "ok" || outcome == "cancelled" || outcome.starts_with("error:"),
                "scenario {scenario} unexpected outcome {outcome}"
            );
            if outcome == "ok" {
                assert_eq!(report["request"]["completion_count"], 0);
            }
        }
        "request-error" => {
            assert!(
                outcome.contains("controlled completion failure")
                    || (outcome == "ok" && report["request"]["completion_count"] == 0),
                "request error outcome: {outcome}"
            );
            let trace = fs::read_to_string(&log).expect("read request-error trace");
            assert!(trace.contains("controlled completion failure"));
        }
        "hang-request" => {
            assert_eq!(outcome, "cancelled");
            assert!(report["request"]["elapsed_ms"].as_u64().unwrap() <= 350);
        }
        "hang-initialize" => {
            assert!(outcome == "cancelled" || outcome == "ok");
            if outcome == "cancelled" {
                assert!(report["request"]["elapsed_ms"].as_u64().unwrap() <= 350);
            } else {
                assert_eq!(report["request"]["completion_count"], 0);
            }
        }
        "restart-once" => {
            assert_eq!(report["restart_requested"], true);
            assert_eq!(outcome, "ok");
            assert_eq!(report["request"]["completion_count"], 2);
        }
        "large-payloads" => {
            assert_eq!(outcome, "ok");
            assert_eq!(report["request"]["completion_count"], 10_000);
            assert_eq!(report["request"]["max_documentation_bytes"], 64 * 1024);
            assert_eq!(report["request"]["overlay_rows"], 200);
            assert_eq!(report["diagnostics"]["warnings"], 10_000);
            assert_eq!(report["diagnostics"]["item_count"], 10_000);
            assert_eq!(report["diagnostics"]["overlay_rows"], 200);
            assert_eq!(report["limits"]["language_items"], 10_000);
            assert_eq!(report["limits"]["language_text_bytes"], 64 * 1024);
            assert_eq!(report["limits"]["overlay_rows"], 200);
        }
        "formatter-error" => {
            assert_eq!(outcome, "ok");
            assert_eq!(report["formatter_failure"]["dirty"], true);
            assert!(
                report["formatter_failure"]["error"]
                    .as_str()
                    .is_some_and(|error| error.contains("controlled formatter failure"))
            );
            assert_eq!(
                report["formatter_failure"]["disk"],
                "fn main() { let _value = alpha_; }\n"
            );
        }
        "huge-stderr" => {
            assert_eq!(outcome, "ok");
            assert_eq!(report["request"]["completion_count"], 2);
        }
        _ => {
            assert!(
                outcome == "ok" || outcome == "cancelled" || outcome.starts_with("error:"),
                "scenario {scenario} unexpected outcome {outcome}"
            );
            if outcome == "ok" {
                assert_eq!(
                    report["request"]["completion_count"], 0,
                    "failed server scenario {scenario} returned real completions"
                );
            }
        }
    }

    assert_fixture_processes_reaped(&log, scenario);
}

fn assert_fixture_processes_reaped(log: &Path, scenario: &str) {
    let pids = fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|entry| entry["direction"] == "lifecycle")
        .filter(|entry| entry["message"]["event"] == "start")
        .filter_map(|entry| entry["message"]["pid"].as_u64())
        .collect::<Vec<_>>();
    let deadline = Instant::now() + Duration::from_secs(5);
    for pid in &pids {
        let process_path = PathBuf::from(format!("/proc/{pid}"));
        while process_path.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !process_path.exists(),
            "fixture process {pid} from scenario {scenario} survived zec"
        );
    }
    if !matches!(scenario, "server-not-found" | "spawn-failure") {
        assert!(
            !pids.is_empty(),
            "scenario {scenario} did not start the fixture server"
        );
    }
    if scenario == "restart-once" {
        assert_eq!(
            pids.len(),
            2,
            "restart-once must launch exactly one replacement server"
        );
    }
}
