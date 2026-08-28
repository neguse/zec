#[path = "e2e_support/mod.rs"]
mod e2e_support;
mod language_support;

use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, ensure};
use e2e_support::{CTRL_Q, CTRL_W, CTRL_Z, END, ENTER, ESC, PtySession};
use language_support::{
    CONTRACT_VERSION, CaseResult, EvidenceFile, FAILURE_SCENARIOS, FRESH_PROCESS_RUNS, Fixture,
    FixtureMode, Invocation, REPORT_SCHEMA_VERSION, SuiteBinaries, acceptance_case_ids,
    artifacts_directory, assert_failure_report, assert_language_report, assert_lsp_trace,
    assert_settings_report, canonical_environment, duration_us, json_bytes, manifest_evidence_json,
    parse_invocation, persist_bytes, read_report, run_probe, verify_canonical_environment,
    verify_cases, wait_for_lsp_trace_method, write_report,
};
use serde::{Deserialize, Serialize};

const CTRL_PAGE_DOWN: &[u8] = b"\x1b[6;5~";
const ALT_SLASH: &[u8] = b"\x1b/";
const F2: &[u8] = b"\x1bOQ";
const F8: &[u8] = b"\x1b[19~";
const SHIFT_F12: &[u8] = b"\x1b[24;2~";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AcceptanceReport {
    schema_version: u32,
    contract_version: u32,
    report_kind: String,
    environment: e2e_support::EnvironmentReport,
    binaries: SuiteBinaries,
    fresh_process_runs: usize,
    failure_scenario_count: usize,
    required_case_count: usize,
    passed: usize,
    failed: usize,
    cases: Vec<CaseResult>,
    evidence: Vec<EvidenceFile>,
    all_assertions_passed: bool,
}

fn main() -> Result<()> {
    match parse_invocation()? {
        Invocation::Verify(path) => {
            let report = read_report::<AcceptanceReport>(&path)?;
            verify_report(&report).with_context(|| format!("verify {}", path.display()))?;
            println!(
                "Language acceptance report verified: {}/{} cases",
                report.passed, report.required_case_count
            );
            Ok(())
        }
        Invocation::Run(arguments) => run(arguments),
    }
}

fn run(arguments: language_support::RunArguments) -> Result<()> {
    let zec = fs::canonicalize(&arguments.zec).context("canonicalize --zec")?;
    let lsp = fs::canonicalize(&arguments.lsp).context("canonicalize --lsp")?;
    let artifacts = artifacts_directory(&arguments.report, "acceptance")?;
    let process_runs = std::env::var("ZEC_E2E_DEV_RUNS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|runs| *runs > 0)
        .unwrap_or(FRESH_PROCESS_RUNS);
    let mut evidence = Vec::new();
    let mut cases = Vec::with_capacity(acceptance_case_ids().len());

    for run in 1..=process_runs {
        record_case(&mut cases, &format!("B1_B5_LANGUAGE_{run:02}"), || {
            let fixture = Fixture::create(&lsp, FixtureMode::Normal)?;
            let before = fixture.manifest()?;
            let report = run_probe(&zec, &fixture, "language-service", None)?;
            assert_language_report(&report)?;
            let pids = assert_lsp_trace(&fixture.log, None)?;
            let after = fixture.manifest()?;
            if run == 1 {
                persist_representative(
                    &mut evidence,
                    &artifacts,
                    "language",
                    &fixture,
                    &before,
                    &after,
                )?;
            }
            Ok(format!(
                "Project/LSP/MultiBuffer/editing oracle passed; {} server process(es) reaped",
                pids.len()
            ))
        });
    }

    record_case(&mut cases, "B2_SETTINGS_RELOAD", || {
        let fixture = Fixture::create(&lsp, FixtureMode::Settings)?;
        let before = fixture.manifest()?;
        let report = run_probe(&zec, &fixture, "settings-reload", None)?;
        assert_settings_report(&report)?;
        let pids = assert_lsp_trace(&fixture.log, Some("settings"))?;
        let after = fixture.manifest()?;
        persist_representative(
            &mut evidence,
            &artifacts,
            "settings",
            &fixture,
            &before,
            &after,
        )?;
        Ok(format!(
            "settings/keymap precedence and live reload passed; {} process(es) reaped",
            pids.len()
        ))
    });

    for run in 1..=process_runs {
        record_case(&mut cases, &format!("B3_B6_B7_PTY_{run:02}"), || {
            pty_language_workflow(
                &zec,
                &lsp,
                (run == 1).then_some((&artifacts, &mut evidence)),
            )
        });
    }

    for scenario in FAILURE_SCENARIOS {
        let scenario_id = scenario.replace('-', "_").to_ascii_uppercase();
        for run in 1..=process_runs {
            record_case(&mut cases, &format!("B7_{scenario_id}_{run:02}"), || {
                let fixture = Fixture::create(&lsp, FixtureMode::Failure(scenario))?;
                let before = fixture.manifest()?;
                let report = run_probe(&zec, &fixture, "lsp-failure", Some(scenario))?;
                assert_failure_report(&report, scenario)?;
                let pids = assert_lsp_trace(&fixture.log, Some(scenario))?;
                let after = fixture.manifest()?;
                if run == 1 {
                    persist_representative(
                        &mut evidence,
                        &artifacts,
                        &format!("failure-{scenario}"),
                        &fixture,
                        &before,
                        &after,
                    )?;
                }
                Ok(format!(
                    "{scenario}: editor remained usable and {} process(es) were reaped",
                    pids.len()
                ))
            });
        }
    }

    let required = acceptance_case_ids();
    let passed = cases.iter().filter(|case| case.passed).count();
    let failed = cases.len().saturating_sub(passed);
    let all_assertions_passed = process_runs == FRESH_PROCESS_RUNS
        && failed == 0
        && cases.len() == required.len()
        && cases.iter().map(|case| &case.id).eq(required.iter());
    let report = AcceptanceReport {
        schema_version: REPORT_SCHEMA_VERSION,
        contract_version: CONTRACT_VERSION,
        report_kind: "e2e_language".to_owned(),
        environment: canonical_environment()?,
        binaries: SuiteBinaries::collect(&zec, &lsp)?,
        fresh_process_runs: process_runs,
        failure_scenario_count: FAILURE_SCENARIOS.len(),
        required_case_count: if process_runs == FRESH_PROCESS_RUNS {
            required.len()
        } else {
            cases.len()
        },
        passed,
        failed,
        cases,
        evidence,
        all_assertions_passed,
    };
    write_report(&arguments.report, &report)?;
    println!(
        "Language acceptance: {}/{} passed; report {}",
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
    let duration_us = duration_us(started.elapsed());
    match result {
        Ok(detail) if duration_us <= 120_000_000 => {
            println!("PASS {id}: {detail}");
            cases.push(CaseResult {
                id: id.to_owned(),
                passed: true,
                duration_us,
                detail,
            });
        }
        Ok(_) => {
            let detail = "case exceeded 120 seconds".to_owned();
            eprintln!("FAIL {id}: {detail}");
            cases.push(CaseResult {
                id: id.to_owned(),
                passed: false,
                duration_us,
                detail,
            });
        }
        Err(error) => {
            let detail = format!("{error:#}");
            eprintln!("FAIL {id}: {detail}");
            cases.push(CaseResult {
                id: id.to_owned(),
                passed: false,
                duration_us,
                detail,
            });
        }
    }
}

fn pty_language_workflow(
    zec: &Path,
    lsp: &Path,
    representative: Option<(&Path, &mut Vec<EvidenceFile>)>,
) -> Result<String> {
    let fixture = Fixture::create(lsp, FixtureMode::PtyRestricted)?;
    let before = fixture.manifest()?;
    let arguments = [fixture.root.as_os_str(), fixture.source.as_os_str()];
    let environment = fixture.env_pairs();
    let (mut session, baseline) = PtySession::spawn_with_env(
        zec,
        &fixture.root,
        &arguments,
        &fixture.xdg_config,
        &environment,
    )?;
    session.wait_ready("zec project", "language e2e fixture")?;
    session.assert_raw(&baseline)?;

    let trust_mark = session.mark();
    session.wait_contains("worktree trust prompt", trust_mark, "Worktree Trust")?;
    ensure!(
        !fixture.log.exists() || fs::read(&fixture.log)?.is_empty(),
        "language server started before trust"
    );
    let trust_mark = session.send_marked(ENTER)?;
    session.wait_absent("worktree trust dismissal", trust_mark, "Worktree Trust")?;
    wait_for_lsp_trace_method(&fixture.log, "initialized", Duration::from_secs(15))?;

    let tab_mark = session.send_marked(CTRL_PAGE_DOWN)?;
    session.wait_contains("Rust tab", tab_mark, "2/2 README.md [main.rs]")?;
    session.send(END)?;
    let completion_mark = session.send_marked(ALT_SLASH)?;
    session.wait_after(
        "completed completion picker",
        completion_mark,
        e2e_support::SCREEN_TIMEOUT,
        |screen| {
            let contents = screen.contents();
            contents.contains("Completions") && contents.contains("alpha_completion")
        },
    )?;
    let commit_mark = session.send_marked(ENTER)?;
    session.wait_contains("completion commit", commit_mark, "alpha_completion()")?;
    let undo_mark = session.send_marked(CTRL_Z)?;
    session.wait_contains("completion undo", undo_mark, "alpha_")?;

    let hover_mark = session.send_marked(F2)?;
    session.wait_after(
        "completed hover overlay",
        hover_mark,
        e2e_support::SCREEN_TIMEOUT,
        |screen| {
            let contents = screen.contents();
            contents.contains("Hover") && contents.contains("Fixture hover")
        },
    )?;
    let dismiss_hover = session.send_marked(ESC)?;
    session.wait_absent("hover dismissal", dismiss_hover, "Fixture hover")?;

    let diagnostics_mark = session.send_marked(F8)?;
    session.wait_after(
        "completed diagnostics",
        diagnostics_mark,
        e2e_support::SCREEN_TIMEOUT,
        |screen| {
            let contents = screen.contents();
            contents.contains("Diagnostics") && contents.contains("deterministic fixture warning")
        },
    )?;
    let return_to_editor = session.send_marked(ESC)?;
    session.wait_after(
        "diagnostics focus return",
        return_to_editor,
        e2e_support::SCREEN_TIMEOUT,
        |screen| {
            let contents = screen.contents();
            contents.contains("editor focused; diagnostics dock remains open")
                && contents.contains("deterministic fixture warning")
        },
    )?;
    let dismiss_diagnostics = session.send_marked(F8)?;
    session.wait_absent("diagnostics dismissal", dismiss_diagnostics, "Diagnostics")?;

    let references_mark = session.send_marked(SHIFT_F12)?;
    session.wait_after(
        "completed references MultiBuffer",
        references_mark,
        e2e_support::SCREEN_TIMEOUT,
        |screen| {
            let contents = screen.contents();
            contents.contains("editable MultiBuffer (2 target(s))")
                && contents.contains("fixture_peer")
        },
    )?;
    let close_mark = session.send_marked(CTRL_W)?;
    session.wait_contains("return from references", close_mark, "[main.rs]")?;

    let quit_mark = session.send_marked(CTRL_Q)?;
    let status = session.wait_exit()?;
    ensure!(status.success(), "PTY language workflow exited as {status}");
    session.assert_restored_and_joined(&baseline)?;
    let pids = assert_lsp_trace(&fixture.log, Some("pty"))?;
    ensure!(
        quit_mark.elapsed_us() <= 5_000_000,
        "quit input timestamp overflowed lifecycle bound"
    );
    let after = fixture.manifest()?;
    ensure!(
        before == after,
        "cancelled/undone PTY workflow changed disk"
    );
    if let Some((artifacts, evidence)) = representative {
        persist_representative(
            evidence,
            artifacts,
            "pty-language",
            &fixture,
            &before,
            &after,
        )?;
    }
    Ok(format!(
        "trust, completion, undo, hover, diagnostics, references and cleanup passed; {} server process(es) reaped",
        pids.len()
    ))
}

fn persist_representative(
    evidence: &mut Vec<EvidenceFile>,
    artifacts: &Path,
    label: &str,
    fixture: &Fixture,
    before: &std::collections::BTreeMap<String, String>,
    after: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    let safe_label = label.replace('/', "-");
    if fixture.log.is_file() {
        persist_bytes(
            evidence,
            artifacts,
            &format!("{safe_label}-lsp.jsonl"),
            &fs::read(&fixture.log)?,
        )?;
    }
    let manifests = manifest_evidence_json(before, after);
    persist_bytes(
        evidence,
        artifacts,
        &format!("{safe_label}-manifest.json"),
        &json_bytes(&manifests)?,
    )
}

fn verify_report(report: &AcceptanceReport) -> Result<()> {
    ensure!(
        report.schema_version == REPORT_SCHEMA_VERSION,
        "schema mismatch"
    );
    ensure!(
        report.contract_version == CONTRACT_VERSION,
        "contract mismatch"
    );
    ensure!(report.report_kind == "e2e_language", "report kind mismatch");
    verify_canonical_environment(&report.environment)?;
    report.binaries.verify()?;
    ensure!(
        report.fresh_process_runs == FRESH_PROCESS_RUNS,
        "fresh process count differs"
    );
    ensure!(
        report.failure_scenario_count == FAILURE_SCENARIOS.len(),
        "failure scenario count differs"
    );
    let required = acceptance_case_ids();
    ensure!(
        report.required_case_count == required.len(),
        "required case count differs"
    );
    verify_cases(&report.cases, &required)?;
    let passed = report.cases.iter().filter(|case| case.passed).count();
    ensure!(
        (report.passed, report.failed) == (passed, report.cases.len() - passed),
        "stored pass/fail counts differ"
    );
    ensure!(report.failed == 0, "acceptance contains failed cases");
    ensure!(!report.evidence.is_empty(), "acceptance evidence is empty");
    let labels = report
        .evidence
        .iter()
        .map(|file| file.label.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    ensure!(
        labels.len() == report.evidence.len(),
        "duplicate evidence labels"
    );
    for file in &report.evidence {
        file.verify()?;
    }
    ensure!(report.all_assertions_passed, "assertion summary is false");
    Ok(())
}
