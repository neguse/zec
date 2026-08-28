#[path = "support/probe.rs"]
mod support;

use std::{
    fs,
    path::Path,
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;
use support::ProbeEnvironment;

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
    let mut probe_env = ProbeEnvironment::new("lsp-failure-fixture");
    let source = probe_env.source_dir.join("main.rs");
    fs::write(&source, "fn main() { let _value = stub_; }\n")
        .expect("write failure fixture source");
    let (format_on_save, formatter) = if scenario == "formatter-error" {
        ("on", ",\n              \"formatter\": \"language_server\"")
    } else {
        ("off", "")
    };
    fs::write(
        probe_env.zed_dir.join("settings.json"),
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
        probe_env.user_config.join("settings.json"),
        r#"{"session":{"trust_all_worktrees":true}}"#,
    )
    .expect("write isolated trusted user settings");
    fs::write(probe_env.user_config.join("global_settings.json"), "{}")
        .expect("write isolated global settings");
    fs::write(probe_env.user_config.join("keymap.json"), "[]").expect("write isolated keymap");

    let fixture_server = Path::new(env!("CARGO_BIN_EXE_fixture_lsp"));
    match scenario {
        "server-not-found" => {}
        "spawn-failure" => probe_env.install_unspawnable_server(),
        _ => probe_env.install_fixture_server(fixture_server),
    }

    let log = probe_env.log.clone();
    let restart_state = probe_env.temp.path().join("restart-state");
    let fixture_scenario = match scenario {
        "server-not-found" | "spawn-failure" => "normal",
        scenario => scenario,
    };
    probe_env.push_env("ZEC_FIXTURE_LSP_SCENARIO", fixture_scenario);
    probe_env.push_env("ZEC_FIXTURE_LSP_RESTART_STATE", restart_state.as_os_str());
    let report = probe_env.run_probe(
        Path::new(env!("CARGO_BIN_EXE_zec")),
        "lsp-failure",
        &source,
        &[scenario],
        Duration::from_secs(30),
        &format!("LSP failure scenario {scenario}"),
    );
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
                "fn main() { let _value = stub_; }\n"
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
        while support::process_exists(*pid) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !support::process_exists(*pid),
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
