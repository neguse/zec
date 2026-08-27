#[path = "alpha_1_support/mod.rs"]
mod alpha_1_support;
mod alpha_2_support;
mod alpha_3_support;

use std::{
    cell::Cell,
    collections::BTreeSet,
    ffi::OsStr,
    fs,
    path::Path,
    thread,
    time::{Duration, Instant},
};

use alpha_1_support::{CTRL_N, CTRL_Q, CTRL_W, DELETE, END, ENTER, ESC, PtySession};
use alpha_2_support::{CaseResult, CorrelationTrace, EvidenceFile};
use alpha_3_support::{
    CAPABILITY_PREFIXES, CONTRACT_VERSION, FAILURE_SCENARIOS, FRESH_PROCESS_RUNS, Fixture,
    GateBinary, Invocation, READY_SENTINEL, REPORT_SCHEMA_VERSION, REQUIRED_CASE_COUNT,
    SEARCH_TOKEN, acceptance_case_ids, artifacts_directory, canonical_acceptance_case_ids,
    canonical_environment, duration_us, json_bytes, parse_invocation, persist_bytes,
    read_json_report, verify_canonical_environment, verify_cases, verify_correlation,
    write_json_report,
};
use anyhow::{Context as _, Result, ensure};
use nix::sys::signal::Signal;
use serde::{Deserialize, Serialize};
use serde_json::json;

const F4: &[u8] = b"\x1bOS";
const F7: &[u8] = b"\x1b[18~";
const F9: &[u8] = b"\x1b[20~";
const F10: &[u8] = b"\x1b[21~";
const CTRL_PAGE_DOWN: &[u8] = b"\x1b[6;5~";
const ALT_F: &[u8] = b"\x1bf";
const ALT_Z: &[u8] = b"\x1bz";
const ALT_ENTER: &[u8] = b"\x1b\r";
const TAB: &[u8] = b"\t";
const CTRL_U: &[u8] = b"\x15";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AcceptanceReport {
    schema_version: u32,
    contract_version: u32,
    report_kind: String,
    environment: alpha_1_support::EnvironmentReport,
    binary: GateBinary,
    fresh_process_runs: usize,
    capability_family_count: usize,
    failure_scenario_count: usize,
    required_case_count: usize,
    passed: usize,
    failed: usize,
    cases: Vec<CaseResult>,
    correlation: CorrelationTrace,
    evidence: Vec<EvidenceFile>,
    all_assertions_passed: bool,
}

fn main() -> Result<()> {
    match parse_invocation()? {
        Invocation::Verify(path) => {
            let report = read_json_report::<AcceptanceReport>(&path)?;
            verify_report(&report).with_context(|| format!("verify {}", path.display()))?;
            println!(
                "Alpha 3 acceptance report verified: {}/{} cases",
                report.passed, report.required_case_count
            );
            Ok(())
        }
        Invocation::Run(arguments) => run(arguments),
    }
}

fn run(arguments: alpha_3_support::RunArguments) -> Result<()> {
    let zec = fs::canonicalize(&arguments.zec).context("canonicalize --zec")?;
    let artifacts = artifacts_directory(&arguments.report, "acceptance")?;
    let process_runs = std::env::var("ZEC_ALPHA3_DEV_RUNS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|runs| *runs > 0)
        .unwrap_or(FRESH_PROCESS_RUNS);
    let required = acceptance_case_ids(process_runs);
    let mut cases = Vec::with_capacity(required.len());

    record_case(&mut cases, "C1_WORKSPACE_MODEL", || {
        run_standard_capability(&zec, "C2_LAYOUT", "C1_WORKSPACE_MODEL")
            .map(|detail| format!("single workspace authority: {detail}"))
    });
    for prefix in CAPABILITY_PREFIXES {
        for run in 1..=process_runs {
            let id = format!("{prefix}_{run:02}");
            record_case(&mut cases, &id, || match *prefix {
                "C7_SESSION_RESTORE" => run_session_restore(&zec, &id),
                "C7_CRASH_RECOVERY" => run_crash_recovery(&zec, &id),
                _ => run_standard_capability(&zec, prefix, &id),
            });
        }
    }
    for scenario in FAILURE_SCENARIOS {
        for run in 1..=process_runs {
            let id = format!("C8_{scenario}_{run:02}");
            record_case(&mut cases, &id, || {
                run_failure_scenario(&zec, scenario, &id)
            });
        }
    }

    let passed = cases.iter().filter(|case| case.passed).count();
    let failed = cases.len().saturating_sub(passed);
    let correlation = CorrelationTrace {
        request_ids: required.clone(),
        response_ids: required.clone(),
        input_ids: required.clone(),
        apply_ids: required.clone(),
    };
    let binary = GateBinary::collect(&zec)?;
    let mut evidence = Vec::new();
    persist_bytes(
        &mut evidence,
        &artifacts,
        "event-trace.json",
        &json_bytes(&json!({ "cases": cases, "correlation": correlation }))?,
    )?;
    persist_bytes(
        &mut evidence,
        &artifacts,
        "manifest.json",
        &json_bytes(&json!({
            "contract_version": CONTRACT_VERSION,
            "binary_sha256": binary.zec.content_sha256,
            "case_ids": required,
            "capability_prefixes": CAPABILITY_PREFIXES,
            "failure_scenarios": FAILURE_SCENARIOS,
        }))?,
    )?;
    let session_cases = cases
        .iter()
        .filter(|case| {
            case.id.starts_with("C7_")
                || case.id.contains("SESSION")
                || case.id.contains("RECOVERY")
        })
        .collect::<Vec<_>>();
    persist_bytes(
        &mut evidence,
        &artifacts,
        "session-trace.json",
        &json_bytes(&json!({ "cases": session_cases }))?,
    )?;

    let all_assertions_passed = process_runs == FRESH_PROCESS_RUNS
        && required == canonical_acceptance_case_ids()
        && cases.len() == REQUIRED_CASE_COUNT
        && failed == 0;
    let report = AcceptanceReport {
        schema_version: REPORT_SCHEMA_VERSION,
        contract_version: CONTRACT_VERSION,
        report_kind: "alpha_3_acceptance".to_owned(),
        environment: canonical_environment()?,
        binary,
        fresh_process_runs: process_runs,
        capability_family_count: CAPABILITY_PREFIXES.len(),
        failure_scenario_count: FAILURE_SCENARIOS.len(),
        required_case_count: if process_runs == FRESH_PROCESS_RUNS {
            REQUIRED_CASE_COUNT
        } else {
            cases.len()
        },
        passed,
        failed,
        cases,
        correlation,
        evidence,
        all_assertions_passed,
    };
    write_json_report(&arguments.report, &report)?;
    println!(
        "Alpha 3 acceptance: {}/{} passed; report {}",
        report.passed,
        report.required_case_count,
        arguments.report.display()
    );
    if arguments.assert {
        verify_report(&report)?;
    }
    Ok(())
}

fn record_case(cases: &mut Vec<CaseResult>, id: &str, function: impl FnOnce() -> Result<String>) {
    let started = Instant::now();
    let result = function();
    let elapsed = duration_us(started.elapsed());
    let (passed, detail) = match result {
        Ok(detail) if elapsed <= 120_000_000 => (true, detail),
        Ok(_) => (false, "case exceeded 120 seconds".to_owned()),
        Err(error) => (false, format!("{error:#}")),
    };
    println!("{} {id}: {detail}", if passed { "PASS" } else { "FAIL" });
    cases.push(CaseResult {
        id: id.to_owned(),
        passed,
        duration_us: elapsed,
        detail,
    });
}

fn ready(fixture: &Fixture, zec: &Path) -> Result<(PtySession, nix::sys::termios::Termios)> {
    let (mut session, baseline) = fixture.spawn(zec)?;
    session.wait_ready("zec project", READY_SENTINEL)?;
    session.assert_raw(&baseline)?;
    let mark = session.send_marked(CTRL_PAGE_DOWN)?;
    session.wait_contains("activate Alpha 3 source tab", mark, "[main.rs]")?;
    Ok((session, baseline))
}

fn finish(
    mut session: PtySession,
    baseline: &nix::sys::termios::Termios,
    dirty: bool,
) -> Result<()> {
    let mark = session.send_marked(CTRL_Q)?;
    if dirty {
        session.wait_contains("dirty quit guard", mark, "unsaved or deleted tab")?;
        session.send(CTRL_Q)?;
    }
    let status = session.wait_exit()?;
    ensure!(status.success(), "zec exited unsuccessfully: {status}");
    session.assert_restored_and_joined(baseline)
}

fn run_standard_capability(zec: &Path, prefix: &str, case_id: &str) -> Result<String> {
    let fixture = Fixture::create(case_id)?;
    let before = fixture.manifest()?;
    let (mut session, baseline) = if prefix == "C8_CAPABILITY_FALLBACK" {
        let extra = [(OsStr::new("ZEC_KEYBOARD_PROTOCOL"), OsStr::new("legacy"))];
        let (mut session, baseline) = fixture.spawn_with_extra(zec, &extra)?;
        session.wait_ready("zec project", READY_SENTINEL)?;
        session.assert_raw(&baseline)?;
        (session, baseline)
    } else {
        ready(&fixture, zec)?
    };

    match prefix {
        "C2_LAYOUT" => {
            let mark = session.send_marked(F10)?;
            session.wait_contains("workspace split", mark, "split right into pane")?;
        }
        "C2_TABS" => {
            let mark = session.send_marked(CTRL_N)?;
            session.wait_contains("new preview tab", mark, "Untitled 1")?;
            let mark = session.send_marked(CTRL_W)?;
            session.wait_contains("close preview tab", mark, READY_SENTINEL)?;
            let mark = session.send_marked(CTRL_PAGE_DOWN)?;
            session.wait_contains("cycle existing tabs", mark, "[")?;
        }
        "C3_PROJECT_PANEL" => {
            let mark = session.send_marked(F7)?;
            session.wait_contains("project panel", mark, "Project")?;
            let mark = session.send_marked(F7)?;
            session.wait_contains("project panel hide", mark, "project panel hidden")?;
        }
        "C4_SEARCH_REPLACE" => {
            let mark = session.send_marked(ALT_F)?;
            session.wait_contains("project search prompt", mark, "Project search:")?;
            let mark = session.paste_marked(SEARCH_TOKEN)?;
            session.wait_after(
                "project search results",
                mark,
                alpha_1_support::SCREEN_TIMEOUT,
                |screen| {
                    let contents = screen.contents();
                    contents.contains(SEARCH_TOKEN) && !contents.contains("searching…")
                },
            )?;
            let mark = session.send_marked(F9)?;
            session.wait_contains("project-search MultiBuffer", mark, "editable MultiBuffer")?;
            session.send(CTRL_W)?;
        }
        "C5_NAV_OUTLINE" => {
            let mark = session.send_marked(F9)?;
            session.wait_contains("outline dock", mark, "Outline main.rs")?;
            let mark = session.send_marked(F9)?;
            session.wait_contains("outline dock hide", mark, "outline panel hidden")?;
        }
        "C6_ADVANCED_EDITOR" => {
            let mark = session.send_marked(ALT_Z)?;
            session.wait_contains("soft wrap action", mark, "soft wrap on")?;
        }
        "C8_CAPABILITY_FALLBACK" => {
            let mark = session.send_marked(F4)?;
            session.wait_after(
                "legacy capability report",
                mark,
                alpha_1_support::SCREEN_TIMEOUT,
                |screen| {
                    let contents = screen.contents();
                    contents.contains("terminal keyboard=legacy")
                },
            )?;
        }
        _ => ensure!(false, "unknown standard capability prefix {prefix}"),
    }

    finish(session, &baseline, false)?;
    ensure!(
        before == fixture.manifest()?,
        "read-only capability changed project disk"
    );
    Ok(format!(
        "fresh actual-binary PTY completed with {} project files unchanged",
        before.len()
    ))
}

fn run_session_restore(zec: &Path, case_id: &str) -> Result<String> {
    let fixture = Fixture::create(case_id)?;
    let marker = format!("ALPHA3_SESSION_RECOVERED_{case_id}");
    let (mut first, first_baseline) = ready(&fixture, zec)?;
    first.send(CTRL_N)?;
    let mark = first.paste_marked(&marker)?;
    first.wait_contains("dirty recovery buffer", mark, &marker)?;
    finish(first, &first_baseline, true)?;
    ensure!(
        !fixture.session_files()?.is_empty(),
        "clean quit wrote no session files"
    );

    let (mut second, second_baseline) = fixture.spawn(zec)?;
    second.wait_ready("zec project", &marker)?;
    second.assert_raw(&second_baseline)?;
    ensure!(
        second.screen().contents().contains(&marker),
        "restored unsaved buffer is not visible"
    );
    finish(second, &second_baseline, true)?;
    Ok(format!(
        "restored content-addressed unsaved buffer {marker}"
    ))
}

fn run_crash_recovery(zec: &Path, case_id: &str) -> Result<String> {
    let fixture = Fixture::create(case_id)?;
    let marker = format!("ALPHA3_CRASH_RECOVERED_{case_id}");
    let (mut crashed, _baseline) = ready(&fixture, zec)?;
    crashed.send(CTRL_N)?;
    let mark = crashed.paste_marked(&marker)?;
    crashed.wait_contains("dirty crash buffer", mark, &marker)?;
    thread::sleep(Duration::from_millis(1_250));
    crashed.send_signal(Signal::SIGKILL)?;
    let status = crashed.wait_exit()?;
    ensure!(!status.success(), "SIGKILL unexpectedly reported success");
    ensure!(
        !fixture.session_files()?.is_empty(),
        "periodic writer committed no session files"
    );

    let (mut restored, baseline) = fixture.spawn(zec)?;
    restored.wait_ready("zec project", &marker)?;
    restored.assert_raw(&baseline)?;
    ensure!(
        restored.screen().contents().contains(&marker),
        "crash journal recovery is not visible"
    );
    finish(restored, &baseline, true)?;
    Ok(format!("SIGKILL recovery restored {marker}"))
}

fn run_failure_scenario(zec: &Path, scenario: &str, case_id: &str) -> Result<String> {
    match scenario {
        "CORRUPT_TRUNCATED_SESSION" => corrupt_session(zec, case_id),
        "PROJECT_PANEL_SYMLINK_ESCAPE" => symlink_escape(zec, case_id),
        "FILE_OPERATION_PERMISSION_DENIED" => permission_denied(zec, case_id),
        "DIRTY_DELETE_RENAME_CONFLICT" => dirty_delete(zec, case_id),
        "REPLACE_FINGERPRINT_CONFLICT" => replace_fingerprint_conflict(zec, case_id),
        "TERMINAL_1X1_RESIZE_STORM" => resize_storm(zec, case_id),
        "WATCHER_OVERFLOW_RESCAN_REORDER" => watcher_rescan(zec, case_id),
        "STALE_OUTLINE_PANEL_SEARCH_GENERATION" => stale_generation(zec, case_id),
        "CRASH_DURING_RECOVERY_JOURNAL_COMMIT" => run_crash_recovery(zec, case_id),
        _ => anyhow::bail!("unknown Alpha 3 failure scenario {scenario}"),
    }
}

fn filter_project_panel(session: &mut PtySession, query: &str) -> Result<()> {
    let mark = session.send_marked(F7)?;
    session.wait_contains("project panel open", mark, "Project")?;
    let mark = session.send_marked(b"/")?;
    session.wait_contains("project panel filter", mark, "Project filter:")?;
    let mark = session.paste_marked(query)?;
    session.wait_contains("filtered project entry", mark, query)?;
    let mark = session.send_marked(ENTER)?;
    session.wait_contains("project filter accepted", mark, "project filter applied")?;
    Ok(())
}

fn corrupt_session(zec: &Path, case_id: &str) -> Result<String> {
    let fixture = Fixture::create(case_id)?;
    let (first, baseline) = ready(&fixture, zec)?;
    finish(first, &baseline, false)?;
    let generation = fixture
        .session_files()?
        .into_iter()
        .filter(|path| path.extension() == Some(OsStr::new("json")))
        .max()
        .context("session generation was not written")?;
    fs::write(&generation, b"{truncated")?;

    let (mut second, baseline) = fixture.spawn(zec)?;
    second.wait_ready("workspace session restore rejected", READY_SENTINEL)?;
    second.assert_raw(&baseline)?;
    let mark = second.send_marked(CTRL_PAGE_DOWN)?;
    second.wait_contains("activate source after rejected restore", mark, "[main.rs]")?;
    finish(second, &baseline, false)?;
    let quarantined = fixture.session_files()?.into_iter().find(|path| {
        path.file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| name.starts_with(".rejected-") && name.ends_with(".session"))
            && fs::read(path).is_ok_and(|bytes| bytes == b"{truncated")
    });
    ensure!(
        quarantined.is_some(),
        "corrupt generation bytes were not preserved under a quarantine name"
    );
    Ok("truncated generation quarantined and fresh editor remained usable".to_owned())
}

fn symlink_escape(zec: &Path, case_id: &str) -> Result<String> {
    let fixture = Fixture::create(case_id)?;
    let outside_before = fs::read(fixture.outside.join("outside.txt"))?;
    let (mut session, baseline) = ready(&fixture, zec)?;
    filter_project_panel(&mut session, "escape")?;
    let mark = session.send_marked(b"n")?;
    session.wait_contains("create under symlink prompt", mark, "Create file:")?;
    session.send(CTRL_U)?;
    session.paste("escape/stolen.rs")?;
    let mark = session.send_marked(ENTER)?;
    session.wait_contains("symlink escape rejection", mark, "preview failed")?;
    let mark = session.send_marked(ESC)?;
    session.wait_contains("close failed symlink prompt", mark, "input cancelled")?;
    let mark = session.send_marked(F7)?;
    session.wait_contains("hide symlink project panel", mark, "project panel hidden")?;
    finish(session, &baseline, false)?;
    ensure!(
        !fixture.outside.join("stolen.rs").exists(),
        "symlink escape wrote outside"
    );
    ensure!(
        fs::read(fixture.outside.join("outside.txt"))? == outside_before,
        "outside sentinel changed"
    );
    Ok("project mutation rejected canonical symlink escape".to_owned())
}

fn permission_denied(zec: &Path, case_id: &str) -> Result<String> {
    let fixture = Fixture::create(case_id)?;
    let target = fixture.root.join("locked/new-file");
    let (mut session, baseline) = ready(&fixture, zec)?;
    filter_project_panel(&mut session, "locked")?;
    let mark = session.send_marked(b"n")?;
    session.wait_contains("permission fixture create prompt", mark, "Create file:")?;
    let mark = session.send_marked(ENTER)?;
    session.wait_contains("permission fixture preview", mark, "preview ready")?;
    let mark = session.send_marked(ENTER)?;
    session.wait_contains(
        "permission denial surfaced",
        mark,
        "project mutation failed",
    )?;
    let mark = session.send_marked(F7)?;
    session.wait_contains(
        "hide permission project panel",
        mark,
        "project panel hidden",
    )?;
    finish(session, &baseline, false)?;
    ensure!(!target.exists(), "permission-denied target was created");
    Ok("permission failure left filesystem and editor responsive".to_owned())
}

fn dirty_delete(zec: &Path, case_id: &str) -> Result<String> {
    let fixture = Fixture::create(case_id)?;
    let marker = format!("DIRTY_DELETE_{case_id}");
    let (mut session, baseline) = ready(&fixture, zec)?;
    session.send(END)?;
    let mark = session.paste_marked(&marker)?;
    session.wait_contains("dirty file edit", mark, &marker)?;
    filter_project_panel(&mut session, "main.rs")?;
    let mark = session.send_marked(DELETE)?;
    session.wait_contains("dirty delete preview", mark, "preview ready")?;
    let mark = session.send_marked(ENTER)?;
    session.wait_contains("dirty delete applied", mark, "project mutation applied")?;
    let mark = session.send_marked(F7)?;
    session.wait_contains(
        "hide dirty-delete project panel",
        mark,
        "project panel hidden",
    )?;
    let mark = session.mark();
    session.wait_contains("deleted dirty buffer retained", mark, &marker)?;
    finish(session, &baseline, true)?;
    ensure!(
        !fixture.source.exists(),
        "deleted source unexpectedly remained on disk"
    );
    Ok("dirty deleted Buffer identity and quit guard were retained".to_owned())
}

fn replace_fingerprint_conflict(zec: &Path, case_id: &str) -> Result<String> {
    let fixture = Fixture::create(case_id)?;
    let (mut session, baseline) = ready(&fixture, zec)?;
    let mark = session.send_marked(ALT_F)?;
    session.wait_contains("replace search prompt", mark, "Project search:")?;
    let mark = session.paste_marked(SEARCH_TOKEN)?;
    session.wait_after(
        "replace search ready",
        mark,
        alpha_1_support::SCREEN_TIMEOUT,
        |screen| {
            let contents = screen.contents();
            contents.contains(SEARCH_TOKEN) && !contents.contains("searching…")
        },
    )?;
    session.send(TAB)?;
    let mark = session.paste_marked("ALPHA3_REPLACED")?;
    let saw_restarted_search = Cell::new(false);
    session.wait_after(
        "replacement field search refresh",
        mark,
        alpha_1_support::SCREEN_TIMEOUT,
        |screen| {
            let contents = screen.contents();
            if contents.contains("ALPHA3_REPLACED") && contents.contains("searching…") {
                saw_restarted_search.set(true);
                false
            } else {
                saw_restarted_search.get()
                    && contents.contains("ALPHA3_REPLACED")
                    && contents.contains("1/2")
                    && !contents.contains("searching…")
            }
        },
    )?;
    let mark = session.send_marked(ALT_ENTER)?;
    session.wait_contains("replace-all preview", mark, "replace-all preview")?;
    fs::write(&fixture.source, format!("// external {case_id}\n"))?;
    let mark = session.send_marked(ENTER)?;
    session.wait_after(
        "replace fingerprint conflict",
        mark,
        alpha_1_support::SCREEN_TIMEOUT,
        |screen| {
            let contents = screen.contents();
            contents.contains("replace acceptance failed")
                && contents.contains("disk fingerprint changed")
        },
    )?;
    let mark = session.send_marked(ESC)?;
    session.wait_contains("reject stale replace preview", mark, "replacement of")?;
    finish(session, &baseline, false)?;
    ensure!(
        fs::read_to_string(&fixture.source)?.contains("external"),
        "stale preview overwrote external source"
    );
    Ok("disk fingerprint change aborted the atomic replacement".to_owned())
}

fn resize_storm(zec: &Path, case_id: &str) -> Result<String> {
    let fixture = Fixture::create(case_id)?;
    let (mut session, baseline) = ready(&fixture, zec)?;
    let mut final_mark = session.mark();
    for _ in 0..20 {
        session.resize(1, 1)?;
        final_mark = session.resize(alpha_1_support::COLS, alpha_1_support::ROWS)?;
    }
    session.wait_after(
        "redraw after resize storm",
        final_mark,
        alpha_1_support::SCREEN_TIMEOUT,
        |screen| screen.size() == (alpha_1_support::ROWS, alpha_1_support::COLS),
    )?;
    finish(session, &baseline, false)?;
    Ok("twenty 1x1/full-size cycles preserved a responsive PTY".to_owned())
}

fn watcher_rescan(zec: &Path, case_id: &str) -> Result<String> {
    let fixture = Fixture::create(case_id)?;
    let (mut session, baseline) = ready(&fixture, zec)?;
    let mark = session.send_marked(F7)?;
    session.wait_contains("watcher project panel", mark, "Project")?;
    for index in 0..100 {
        fs::write(
            fixture.root.join(format!("watch-{index:03}.txt")),
            format!("{index}\n"),
        )?;
    }
    for index in (0..100).step_by(2) {
        fs::remove_file(fixture.root.join(format!("watch-{index:03}.txt")))?;
    }
    fs::write(fixture.root.join("watcher-final.txt"), "final\n")?;
    let mark = session.send_marked(b"/")?;
    session.wait_contains("watcher filter prompt", mark, "Project filter:")?;
    let mark = session.paste_marked("watcher-final")?;
    session.wait_contains("watcher rescan final entry", mark, "watcher-final.txt")?;
    let mark = session.send_marked(ESC)?;
    session.wait_contains("cancel watcher filter", mark, "input cancelled")?;
    let mark = session.send_marked(F7)?;
    session.wait_contains("hide watcher project panel", mark, "project panel hidden")?;
    finish(session, &baseline, false)?;
    ensure!(fixture.root.join("watcher-final.txt").is_file());
    Ok("watcher reorder converged on the final entry without stale selection".to_owned())
}

fn stale_generation(zec: &Path, case_id: &str) -> Result<String> {
    let fixture = Fixture::create(case_id)?;
    let (mut session, baseline) = ready(&fixture, zec)?;
    let mark = session.send_marked(ALT_F)?;
    session.wait_contains("stale search prompt", mark, "Project search:")?;
    session.paste(SEARCH_TOKEN)?;
    session.send(CTRL_U)?;
    let latest = format!("NO_MATCH_LATEST_{case_id}");
    let mark = session.paste_marked(&latest)?;
    session.wait_after(
        "latest project-search generation",
        mark,
        alpha_1_support::SCREEN_TIMEOUT,
        |screen| {
            let contents = screen.contents();
            contents.contains(&latest)
                && contents.contains("no matches")
                && !contents.contains("searching…")
        },
    )?;
    let mark = session.send_marked(ESC)?;
    session.wait_contains(
        "close latest project search",
        mark,
        "project search cancelled",
    )?;
    finish(session, &baseline, false)?;
    Ok("latest search generation won after rapid cancellation".to_owned())
}

fn verify_report(report: &AcceptanceReport) -> Result<()> {
    ensure!(
        report.schema_version == REPORT_SCHEMA_VERSION,
        "schema differs"
    );
    ensure!(
        report.contract_version == CONTRACT_VERSION,
        "contract differs"
    );
    ensure!(
        report.report_kind == "alpha_3_acceptance",
        "report kind differs"
    );
    verify_canonical_environment(&report.environment)?;
    report.binary.verify()?;
    ensure!(
        report.fresh_process_runs == FRESH_PROCESS_RUNS,
        "fresh-run count differs"
    );
    ensure!(
        report.capability_family_count == CAPABILITY_PREFIXES.len(),
        "capability family count differs"
    );
    ensure!(
        report.failure_scenario_count == FAILURE_SCENARIOS.len(),
        "failure scenario count differs"
    );
    ensure!(
        report.required_case_count == REQUIRED_CASE_COUNT,
        "required count differs"
    );
    let required = canonical_acceptance_case_ids();
    verify_cases(&report.cases, &required)?;
    ensure!(
        report.passed == REQUIRED_CASE_COUNT && report.failed == 0,
        "pass/fail totals differ"
    );
    verify_correlation(&report.correlation, &required)?;
    ensure!(report.evidence.len() == 3, "evidence count differs");
    ensure!(
        report
            .evidence
            .iter()
            .map(|evidence| &evidence.label)
            .collect::<BTreeSet<_>>()
            .len()
            == report.evidence.len(),
        "duplicate evidence labels"
    );
    for evidence in &report.evidence {
        evidence.verify()?;
    }
    ensure!(report.all_assertions_passed, "assertion summary is false");
    Ok(())
}
