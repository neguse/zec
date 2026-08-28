#[path = "e2e_support/mod.rs"]
mod e2e_support;
mod language_support;

use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, ensure};
use e2e_support::{CTRL_Q, CTRL_S, CTRL_W, CTRL_Z, END, ENTER, ESC, MetricReport, PtySession};
use language_support::{
    CONTRACT_VERSION, CorrelationTrace, EvidenceFile, Fixture, FixtureMode, Invocation,
    REPORT_SCHEMA_VERSION, SuiteBinaries, VM_HWM_LIMIT_BYTES, artifacts_directory,
    assert_lsp_trace, canonical_environment, manifest_evidence_json, metric, parse_invocation,
    persist_bytes, read_report, trace_request_response_ids, verify_canonical_environment,
    verify_metric, write_report,
};
use serde::{Deserialize, Serialize};

const STARTUP_WARMUPS: usize = 2;
const STARTUP_SAMPLES: usize = 20;
const LANGUAGE_WARMUPS: usize = 0;
const LANGUAGE_SAMPLES: usize = 20;
const COMPLETION_WARMUPS: usize = 10;
const COMPLETION_SAMPLES: usize = 100;
const DIAGNOSTIC_WARMUPS: usize = 0;
const DIAGNOSTIC_SAMPLES: usize = 100;
const DEFINITION_WARMUPS: usize = 0;
const DEFINITION_SAMPLES: usize = 100;
const REFERENCES_WARMUPS: usize = 0;
const REFERENCES_SAMPLES: usize = 100;
const RENAME_WARMUPS: usize = 0;
const RENAME_SAMPLES: usize = 20;

const CTRL_PAGE_DOWN: &[u8] = b"\x1b[6;5~";
const ALT_SLASH: &[u8] = b"\x1b/";
const F6: &[u8] = b"\x1b[17~";
const F8: &[u8] = b"\x1b[19~";
const F12: &[u8] = b"\x1b[24~";
const SHIFT_F12: &[u8] = b"\x1b[24;2~";
const BACKSPACE: &[u8] = b"\x7f";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BenchmarkReport {
    schema_version: u32,
    contract_version: u32,
    report_kind: String,
    environment: e2e_support::EnvironmentReport,
    binaries: SuiteBinaries,
    startup: MetricReport,
    language_ready: MetricReport,
    completion: MetricReport,
    diagnostics: MetricReport,
    definition: MetricReport,
    references: MetricReport,
    rename_apply_save: MetricReport,
    vm_hwm_bytes: u64,
    vm_hwm_limit_bytes: u64,
    correlation: CorrelationTrace,
    evidence: Vec<EvidenceFile>,
    assertions: BTreeMap<String, bool>,
    all_assertions_passed: bool,
}

fn main() -> Result<()> {
    match parse_invocation()? {
        Invocation::Verify(path) => {
            let report = read_report::<BenchmarkReport>(&path)?;
            verify_report(&report).with_context(|| format!("verify {}", path.display()))?;
            println!("Language benchmark report verified");
            Ok(())
        }
        Invocation::Run(arguments) => run(arguments),
    }
}

fn run(arguments: language_support::RunArguments) -> Result<()> {
    let zec = fs::canonicalize(&arguments.zec).context("canonicalize --zec")?;
    let lsp = fs::canonicalize(&arguments.lsp).context("canonicalize --lsp")?;
    let artifacts = artifacts_directory(&arguments.report, "benchmark")?;
    let mut evidence = Vec::new();
    let development_samples = std::env::var("ZEC_E2E_DEV_SAMPLES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|samples| *samples > 0);
    let startup_warmups = development_samples.map_or(STARTUP_WARMUPS, |_| 0);
    let startup_sample_count = development_samples.unwrap_or(STARTUP_SAMPLES);
    let language_warmups = development_samples.map_or(LANGUAGE_WARMUPS, |_| 0);
    let language_sample_count = development_samples.unwrap_or(LANGUAGE_SAMPLES);
    let completion_warmups = development_samples.map_or(COMPLETION_WARMUPS, |_| 0);
    let completion_sample_count = development_samples.unwrap_or(COMPLETION_SAMPLES);
    let diagnostic_warmups = development_samples.map_or(DIAGNOSTIC_WARMUPS, |_| 0);
    let diagnostic_sample_count = development_samples.unwrap_or(DIAGNOSTIC_SAMPLES);
    let definition_warmups = development_samples.map_or(DEFINITION_WARMUPS, |_| 0);
    let definition_sample_count = development_samples.unwrap_or(DEFINITION_SAMPLES);
    let references_warmups = development_samples.map_or(REFERENCES_WARMUPS, |_| 0);
    let references_sample_count = development_samples.unwrap_or(REFERENCES_SAMPLES);
    let rename_warmups = development_samples.map_or(RENAME_WARMUPS, |_| 0);
    let rename_sample_count = development_samples.unwrap_or(RENAME_SAMPLES);

    let mut startup_samples = Vec::with_capacity(startup_sample_count);
    let mut language_samples = Vec::with_capacity(language_sample_count);
    for index in 0..startup_warmups + startup_sample_count {
        let fixture = Fixture::create(&lsp, FixtureMode::Normal)?;
        let pty_arguments = [fixture.root.as_os_str(), fixture.source.as_os_str()];
        let environment = fixture.env_pairs();
        let spawned = Instant::now();
        let (mut session, baseline) = PtySession::spawn_with_env(
            &zec,
            &fixture.root,
            &pty_arguments,
            &fixture.xdg_config,
            &environment,
        )?;
        let startup_us = session.wait_ready("zec project", "language e2e fixture")?;
        let language_us = wait_for_lsp_initialized(&fixture.log, spawned)?;
        if index >= startup_warmups {
            startup_samples.push(startup_us);
            language_samples.push(language_us);
        }
        session.send(CTRL_Q)?;
        ensure!(
            session.wait_exit()?.success(),
            "startup sample did not quit cleanly"
        );
        session.assert_restored_and_joined(&baseline)?;
        assert_lsp_trace(&fixture.log, Some("benchmark-startup"))?;
    }
    let startup = metric(
        startup_warmups,
        startup_sample_count,
        startup_samples,
        Some(3_000_000),
        None,
    );
    let language_ready = metric(
        language_warmups,
        language_sample_count,
        language_samples,
        Some(1_500_000),
        Some(3_000_000),
    );

    let fixture = Fixture::create(&lsp, FixtureMode::Normal)?;
    let before = fixture.manifest()?;
    let pty_arguments = [fixture.root.as_os_str(), fixture.source.as_os_str()];
    let environment = fixture.env_pairs();
    let (mut session, baseline) = PtySession::spawn_with_env(
        &zec,
        &fixture.root,
        &pty_arguments,
        &fixture.xdg_config,
        &environment,
    )?;
    session.wait_ready("zec project", "language e2e fixture")?;
    wait_for_lsp_initialized(&fixture.log, Instant::now())?;
    let tab = session.send_marked(CTRL_PAGE_DOWN)?;
    session.wait_contains("benchmark Rust tab", tab, "2/2 README.md [main.rs]")?;
    session.send(END)?;

    let mut input_ids = Vec::new();
    let mut apply_ids = Vec::new();
    let mut completion_samples = Vec::with_capacity(completion_sample_count);
    for index in 0..completion_warmups + completion_sample_count {
        let mark = session.send_marked(ALT_SLASH)?;
        let elapsed = session.wait_after(
            "completed completion popup",
            mark,
            e2e_support::SCREEN_TIMEOUT,
            |screen| {
                let contents = screen.contents();
                contents.contains("Completions") && contents.contains("alpha_completion")
            },
        )?;
        let dismiss = session.send_marked(ESC)?;
        session.wait_absent("completion popup dismissal", dismiss, "Completions")?;
        if index >= completion_warmups {
            let id = format!("B8_COMPLETION_{:03}", index - completion_warmups + 1);
            input_ids.push(id.clone());
            apply_ids.push(id);
            completion_samples.push(elapsed);
        }
    }

    let mut diagnostic_samples = Vec::with_capacity(diagnostic_sample_count);
    for index in 0..diagnostic_warmups + diagnostic_sample_count {
        let mark = session.send_marked(F8)?;
        let elapsed = session.wait_after(
            "completed diagnostics popup",
            mark,
            e2e_support::SCREEN_TIMEOUT,
            |screen| {
                let contents = screen.contents();
                contents.contains("Diagnostics")
                    && contents.contains("deterministic fixture warning")
            },
        )?;
        let return_to_editor = session.send_marked(ESC)?;
        session.wait_contains(
            "diagnostics focus return",
            return_to_editor,
            "editor focused; diagnostics dock remains open",
        )?;
        let dismiss = session.send_marked(F8)?;
        session.wait_absent("diagnostics dismissal", dismiss, "Diagnostics")?;
        if index >= diagnostic_warmups {
            let id = format!("B8_DIAGNOSTICS_{:03}", index - diagnostic_warmups + 1);
            input_ids.push(id.clone());
            apply_ids.push(id);
            diagnostic_samples.push(elapsed);
        }
    }

    let mut definition_samples = Vec::with_capacity(definition_sample_count);
    for index in 0..definition_warmups + definition_sample_count {
        let mark = session.send_marked(F12)?;
        let saw_request = Cell::new(false);
        let elapsed = session.wait_after(
            "completed definition navigation",
            mark,
            e2e_support::SCREEN_TIMEOUT,
            |screen| {
                let contents = screen.contents();
                if contents.contains("Requesting definitions") {
                    saw_request.set(true);
                    false
                } else {
                    saw_request.get() && contents.contains("opened src/main.rs:1:4")
                }
            },
        )?;
        session.send(END)?;
        if index >= definition_warmups {
            let id = format!("B8_DEFINITION_{:03}", index - definition_warmups + 1);
            input_ids.push(id.clone());
            apply_ids.push(id);
            definition_samples.push(elapsed);
        }
    }

    let mut reference_samples = Vec::with_capacity(references_sample_count);
    for index in 0..references_warmups + references_sample_count {
        let mark = session.send_marked(SHIFT_F12)?;
        let saw_request = Cell::new(false);
        let elapsed = session.wait_after(
            "completed references MultiBuffer",
            mark,
            e2e_support::SCREEN_TIMEOUT,
            |screen| {
                let contents = screen.contents();
                if contents.contains("Requesting references") {
                    saw_request.set(true);
                    false
                } else {
                    saw_request.get() && contents.contains("editable MultiBuffer (2 target(s))")
                }
            },
        )?;
        let close = session.send_marked(CTRL_W)?;
        session.wait_contains("return to source tab", close, "[main.rs]")?;
        session.send(END)?;
        if index >= references_warmups {
            let id = format!("B8_REFERENCES_{:03}", index - references_warmups + 1);
            input_ids.push(id.clone());
            apply_ids.push(id);
            reference_samples.push(elapsed);
        }
    }
    let interactive_vm_hwm = e2e_support::vm_hwm_bytes(session.pid()?)?;
    session.send(CTRL_Q)?;
    ensure!(
        session.wait_exit()?.success(),
        "interactive benchmark did not quit cleanly"
    );
    session.assert_restored_and_joined(&baseline)?;
    assert_lsp_trace(&fixture.log, Some("benchmark-interactive"))?;
    let (request_ids, response_ids) = trace_request_response_ids(&fixture.log)?;
    let after = fixture.manifest()?;
    ensure!(before == after, "read-only benchmark changed fixture disk");
    persist_bytes(
        &mut evidence,
        &artifacts,
        "interactive-lsp.jsonl",
        &fs::read(&fixture.log)?,
    )?;
    persist_bytes(
        &mut evidence,
        &artifacts,
        "interactive-manifest.json",
        &language_support::json_bytes(&manifest_evidence_json(&before, &after))?,
    )?;

    let completion = metric(
        completion_warmups,
        completion_sample_count,
        completion_samples,
        Some(100_000),
        Some(250_000),
    );
    let diagnostics = metric(
        diagnostic_warmups,
        diagnostic_sample_count,
        diagnostic_samples,
        Some(100_000),
        Some(250_000),
    );
    let definition = metric(
        definition_warmups,
        definition_sample_count,
        definition_samples,
        Some(150_000),
        Some(500_000),
    );
    let references = metric(
        references_warmups,
        references_sample_count,
        reference_samples,
        Some(150_000),
        Some(500_000),
    );

    let mut rename_samples = Vec::with_capacity(rename_sample_count);
    for index in 0..rename_warmups + rename_sample_count {
        let fixture = Fixture::create(&lsp, FixtureMode::Normal)?;
        let before = fixture.manifest()?;
        let pty_arguments = [fixture.root.as_os_str(), fixture.source.as_os_str()];
        let environment = fixture.env_pairs();
        let (mut session, baseline) = PtySession::spawn_with_env(
            &zec,
            &fixture.root,
            &pty_arguments,
            &fixture.xdg_config,
            &environment,
        )?;
        session.wait_ready("zec project", "language e2e fixture")?;
        wait_for_lsp_initialized(&fixture.log, Instant::now())?;
        let tab = session.send_marked(CTRL_PAGE_DOWN)?;
        session.wait_contains("rename Rust tab", tab, "2/2 README.md [main.rs]")?;
        session.send(END)?;
        let mark = session.send_marked(F6)?;
        session.wait_after(
            "ready rename prompt",
            mark,
            e2e_support::SCREEN_TIMEOUT,
            |screen| {
                let contents = screen.contents();
                contents.contains("Rename Symbol") && contents.contains("Enter preview")
            },
        )?;
        for _ in 0.."alpha_".len() {
            session.send(BACKSPACE)?;
        }
        let new_name = format!("renamed_bench_{index:02}");
        let name_mark = session.paste_marked(&new_name)?;
        session.wait_contains("renamed symbol input", name_mark, &new_name)?;
        session.send(ENTER)?;
        session.wait_contains("rename preview", mark, "rename preview:")?;
        session.send(ENTER)?;
        session.wait_contains("rename apply", mark, "renamed to renamed_bench_")?;
        session.send(CTRL_S)?;
        let elapsed = session.wait_contains("rename save", mark, "saved  |  zec")?;
        if index >= rename_warmups {
            let id = format!("B8_RENAME_{:02}", index - rename_warmups + 1);
            input_ids.push(id.clone());
            apply_ids.push(id);
            rename_samples.push(elapsed);
        }
        session.send(CTRL_Z)?;
        let restore_save = session.send_marked(CTRL_S)?;
        session.wait_contains("rename restoration save", restore_save, "saved  |  zec")?;
        session.send(CTRL_Q)?;
        ensure!(
            session.wait_exit()?.success(),
            "rename benchmark did not quit cleanly"
        );
        session.assert_restored_and_joined(&baseline)?;
        assert_lsp_trace(&fixture.log, Some("benchmark-rename"))?;
        let after = fixture.manifest()?;
        ensure!(
            before == after,
            "rename benchmark did not restore fixture disk"
        );
        if index == 0 {
            persist_bytes(
                &mut evidence,
                &artifacts,
                "rename-lsp.jsonl",
                &fs::read(&fixture.log)?,
            )?;
            persist_bytes(
                &mut evidence,
                &artifacts,
                "rename-manifest.json",
                &language_support::json_bytes(&manifest_evidence_json(&before, &after))?,
            )?;
        }
    }
    let rename_apply_save = metric(
        rename_warmups,
        rename_sample_count,
        rename_samples,
        Some(500_000),
        Some(1_500_000),
    );

    let large_fixture = Fixture::create(&lsp, FixtureMode::LargePayload)?;
    let pty_arguments = [
        large_fixture.root.as_os_str(),
        large_fixture.source.as_os_str(),
    ];
    let environment = large_fixture.env_pairs();
    let (mut large_session, large_baseline) = PtySession::spawn_with_env(
        &zec,
        &large_fixture.root,
        &pty_arguments,
        &large_fixture.xdg_config,
        &environment,
    )?;
    large_session.wait_ready("zec project", "language e2e fixture")?;
    wait_for_lsp_initialized(&large_fixture.log, Instant::now())?;
    let tab = large_session.send_marked(CTRL_PAGE_DOWN)?;
    large_session.wait_contains("large payload Rust tab", tab, "2/2 README.md [main.rs]")?;
    large_session.send(END)?;
    let completion_mark = large_session.send_marked(ALT_SLASH)?;
    large_session.wait_contains("large completion popup", completion_mark, "Completions")?;
    let completion_vm = e2e_support::vm_hwm_bytes(large_session.pid()?)?;
    let dismiss_completion = large_session.send_marked(ESC)?;
    large_session.wait_absent(
        "large completion dismissal",
        dismiss_completion,
        "Completions",
    )?;
    let diagnostics_deadline = Instant::now() + e2e_support::SCREEN_TIMEOUT;
    loop {
        let diagnostics_mark = large_session.send_marked(F8)?;
        large_session.wait_after(
            "large diagnostics collection attempt",
            diagnostics_mark,
            e2e_support::SCREEN_TIMEOUT,
            |screen| {
                let contents = screen.contents();
                contents.contains("bounded diagnostic")
                    || contents.contains("No matching diagnostics")
            },
        )?;
        if large_session
            .screen()
            .contents()
            .contains("bounded diagnostic")
        {
            break;
        }
        ensure!(
            Instant::now() < diagnostics_deadline,
            "10,000 diagnostics did not reach the project within 15 seconds"
        );
        let return_to_editor = large_session.send_marked(ESC)?;
        large_session.wait_contains(
            "empty diagnostics focus return",
            return_to_editor,
            "editor focused; diagnostics dock remains open",
        )?;
        let dismiss = large_session.send_marked(F8)?;
        large_session.wait_absent("empty diagnostics dismissal", dismiss, "Diagnostics")?;
        thread::sleep(Duration::from_millis(25));
    }
    let diagnostics_vm = e2e_support::vm_hwm_bytes(large_session.pid()?)?;
    let return_to_editor = large_session.send_marked(ESC)?;
    large_session.wait_contains(
        "large diagnostics focus return",
        return_to_editor,
        "editor focused; diagnostics dock remains open",
    )?;
    let dismiss_diagnostics = large_session.send_marked(F8)?;
    large_session.wait_absent(
        "large diagnostics dismissal",
        dismiss_diagnostics,
        "Diagnostics",
    )?;
    large_session.send(CTRL_Q)?;
    ensure!(
        large_session.wait_exit()?.success(),
        "large payload benchmark did not quit cleanly"
    );
    large_session.assert_restored_and_joined(&large_baseline)?;
    assert_lsp_trace(&large_fixture.log, Some("large-payloads"))?;
    let vm_hwm_bytes = interactive_vm_hwm.max(completion_vm).max(diagnostics_vm);
    persist_bytes(
        &mut evidence,
        &artifacts,
        "large-payload-lsp.jsonl",
        &fs::read(&large_fixture.log)?,
    )?;

    let correlation = CorrelationTrace {
        request_ids,
        response_ids,
        input_ids,
        apply_ids,
    };
    let assertions = BTreeMap::from([
        ("startup_latency".to_owned(), startup.assertion_passed),
        (
            "language_ready_latency".to_owned(),
            language_ready.assertion_passed,
        ),
        ("completion_latency".to_owned(), completion.assertion_passed),
        (
            "diagnostics_latency".to_owned(),
            diagnostics.assertion_passed,
        ),
        ("definition_latency".to_owned(), definition.assertion_passed),
        ("references_latency".to_owned(), references.assertion_passed),
        (
            "rename_apply_save_latency".to_owned(),
            rename_apply_save.assertion_passed,
        ),
        (
            "vm_hwm".to_owned(),
            vm_hwm_bytes > 0 && vm_hwm_bytes <= VM_HWM_LIMIT_BYTES,
        ),
        (
            "correlation".to_owned(),
            correlation.request_ids == correlation.response_ids
                && correlation.input_ids == correlation.apply_ids,
        ),
    ]);
    let all_assertions_passed = assertions.values().all(|passed| *passed);
    let report = BenchmarkReport {
        schema_version: REPORT_SCHEMA_VERSION,
        contract_version: CONTRACT_VERSION,
        report_kind: "e2e_language_benchmark".to_owned(),
        environment: canonical_environment()?,
        binaries: SuiteBinaries::collect(&zec, &lsp)?,
        startup,
        language_ready,
        completion,
        diagnostics,
        definition,
        references,
        rename_apply_save,
        vm_hwm_bytes,
        vm_hwm_limit_bytes: VM_HWM_LIMIT_BYTES,
        correlation,
        evidence,
        assertions,
        all_assertions_passed,
    };
    write_report(&arguments.report, &report)?;
    println!(
        "Language benchmark: completion p95={}us, diagnostics p95={}us, definition p95={}us, references p95={}us, rename p95={}us, VmHWM={} bytes; report {}",
        report.completion.p95_us,
        report.diagnostics.p95_us,
        report.definition.p95_us,
        report.references.p95_us,
        report.rename_apply_save.p95_us,
        report.vm_hwm_bytes,
        arguments.report.display()
    );
    if arguments.assert {
        verify_report(&report)?;
    }
    Ok(())
}

fn wait_for_lsp_initialized(log: &Path, started: Instant) -> Result<u64> {
    let deadline = started + Duration::from_secs(15);
    loop {
        if fs::read_to_string(log).is_ok_and(|trace| trace.contains("\"method\":\"initialized\"")) {
            return Ok(language_support::duration_us(started.elapsed()));
        }
        ensure!(
            Instant::now() < deadline,
            "fixture LSP initialization timed out"
        );
        thread::sleep(Duration::from_millis(2));
    }
}

fn verify_report(report: &BenchmarkReport) -> Result<()> {
    ensure!(
        report.schema_version == REPORT_SCHEMA_VERSION,
        "schema mismatch"
    );
    ensure!(
        report.contract_version == CONTRACT_VERSION,
        "contract mismatch"
    );
    ensure!(
        report.report_kind == "e2e_language_benchmark",
        "report kind mismatch"
    );
    verify_canonical_environment(&report.environment)?;
    report.binaries.verify()?;
    for (label, metric, warmups, samples, p95, max) in [
        (
            "startup",
            &report.startup,
            STARTUP_WARMUPS,
            STARTUP_SAMPLES,
            Some(3_000_000),
            None,
        ),
        (
            "language_ready",
            &report.language_ready,
            LANGUAGE_WARMUPS,
            LANGUAGE_SAMPLES,
            Some(1_500_000),
            Some(3_000_000),
        ),
        (
            "completion",
            &report.completion,
            COMPLETION_WARMUPS,
            COMPLETION_SAMPLES,
            Some(100_000),
            Some(250_000),
        ),
        (
            "diagnostics",
            &report.diagnostics,
            DIAGNOSTIC_WARMUPS,
            DIAGNOSTIC_SAMPLES,
            Some(100_000),
            Some(250_000),
        ),
        (
            "definition",
            &report.definition,
            DEFINITION_WARMUPS,
            DEFINITION_SAMPLES,
            Some(150_000),
            Some(500_000),
        ),
        (
            "references",
            &report.references,
            REFERENCES_WARMUPS,
            REFERENCES_SAMPLES,
            Some(150_000),
            Some(500_000),
        ),
        (
            "rename_apply_save",
            &report.rename_apply_save,
            RENAME_WARMUPS,
            RENAME_SAMPLES,
            Some(500_000),
            Some(1_500_000),
        ),
    ] {
        ensure!(metric.warmups == warmups, "{label}: warmup count differs");
        ensure!(
            metric.expected_samples == samples,
            "{label}: sample count differs"
        );
        ensure!(metric.p95_limit_us == p95, "{label}: p95 limit differs");
        ensure!(metric.max_limit_us == max, "{label}: max limit differs");
        verify_metric(metric, label)?;
    }
    ensure!(
        report.vm_hwm_limit_bytes == VM_HWM_LIMIT_BYTES
            && report.vm_hwm_bytes > 0
            && report.vm_hwm_bytes <= VM_HWM_LIMIT_BYTES,
        "VmHWM envelope differs"
    );
    report.correlation.verify()?;
    ensure!(!report.evidence.is_empty(), "benchmark evidence is empty");
    let labels = report
        .evidence
        .iter()
        .map(|file| file.label.as_str())
        .collect::<BTreeSet<_>>();
    ensure!(
        labels.len() == report.evidence.len(),
        "duplicate evidence labels"
    );
    for evidence in &report.evidence {
        evidence.verify()?;
    }
    ensure!(
        !report.assertions.is_empty() && report.assertions.values().all(|passed| *passed),
        "benchmark assertion map contains failure"
    );
    ensure!(report.all_assertions_passed, "assertion summary is false");
    Ok(())
}
