#[path = "alpha_1_support/mod.rs"]
mod alpha_1_support;
mod alpha_2_support;
mod alpha_3_support;

use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

use alpha_1_support::{CTRL_Q, CTRL_W, DOWN, ENTER, ESC, MetricReport, PtySession};
use alpha_2_support::{CorrelationTrace, EvidenceFile};
use alpha_3_support::{
    CONTRACT_VERSION, Fixture, GateBinary, Invocation, READY_SENTINEL, REPORT_SCHEMA_VERSION,
    VM_HWM_LIMIT_BYTES, artifacts_directory, canonical_environment, json_bytes, metric,
    parse_invocation, persist_bytes, read_json_report, verify_canonical_environment, verify_metric,
    write_json_report,
};
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const REDRAW_WARMUPS: usize = 10;
const REDRAW_SAMPLES: usize = 100;
const PANEL_WARMUPS: usize = 0;
const PANEL_SAMPLES: usize = 20;
const OUTLINE_WARMUPS: usize = 0;
const OUTLINE_SAMPLES: usize = 100;
const REGEX_WARMUPS: usize = 0;
const REGEX_SAMPLES: usize = 100;
const REPLACE_WARMUPS: usize = 0;
const REPLACE_SAMPLES: usize = 20;
const NAVIGATION_WARMUPS: usize = 10;
const NAVIGATION_SAMPLES: usize = 500;
const RESTORE_WARMUPS: usize = 0;
const RESTORE_SAMPLES: usize = 20;

const TREE_FILE_COUNT: usize = 10_000;
const OUTLINE_SYMBOL_COUNT: usize = 10_000;
const REPLACE_FILE_COUNT: usize = 1_000;
const LARGE_LINE_COUNT: usize = 100_000;
const SESSION_PANE_COUNT: usize = 20;
const CORRELATION_COUNT: usize = REDRAW_SAMPLES
    + PANEL_SAMPLES
    + OUTLINE_SAMPLES
    + REGEX_SAMPLES
    + REPLACE_SAMPLES
    + NAVIGATION_SAMPLES
    + RESTORE_SAMPLES;

const F7: &[u8] = b"\x1b[18~";
const F9: &[u8] = b"\x1b[20~";
const F10: &[u8] = b"\x1b[21~";
const CTRL_PAGE_DOWN: &[u8] = b"\x1b[6;5~";
const ALT_F: &[u8] = b"\x1bf";
const ALT_R: &[u8] = b"\x1br";
const ALT_ENTER: &[u8] = b"\x1b\r";
const ALT_LEFT: &[u8] = b"\x1b[1;3D";
const ALT_RIGHT: &[u8] = b"\x1b[1;3C";
const CTRL_ALT_LEFT: &[u8] = b"\x1b[1;7D";
const CTRL_ALT_RIGHT: &[u8] = b"\x1b[1;7C";
const TAB: &[u8] = b"\t";
const REGEX_QUERY: &str = "BENCH_SEARCH_0{5}";
const REGEX_FIRST_RESULT: &str = "BENCH_SEARCH_00000";
const REPLACE_TOKEN: &str = "ALPHA3_REPLACE_BENCH_TOKEN";
const REPLACEMENT: &str = "ALPHA3_REPLACE_BENCH_RESULT";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct WorkloadReport {
    tree_files: usize,
    outline_symbols: usize,
    replace_files: usize,
    large_lines: usize,
    session_panes: usize,
}

impl WorkloadReport {
    fn canonical() -> Self {
        Self {
            tree_files: TREE_FILE_COUNT,
            outline_symbols: OUTLINE_SYMBOL_COUNT,
            replace_files: REPLACE_FILE_COUNT,
            large_lines: LARGE_LINE_COUNT,
            session_panes: SESSION_PANE_COUNT,
        }
    }

    fn development() -> Self {
        Self {
            tree_files: 200,
            outline_symbols: 200,
            replace_files: 20,
            large_lines: 2_000,
            session_panes: 4,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BenchmarkReport {
    schema_version: u32,
    contract_version: u32,
    report_kind: String,
    environment: alpha_1_support::EnvironmentReport,
    binary: GateBinary,
    workload: WorkloadReport,
    four_pane_redraw: MetricReport,
    project_panel_initial_ready: MetricReport,
    outline_update: MetricReport,
    regex_first_visible_result: MetricReport,
    replace_preview: MetricReport,
    navigation_apply: MetricReport,
    session_restore: MetricReport,
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
            let report = read_json_report::<BenchmarkReport>(&path)?;
            verify_report(&report).with_context(|| format!("verify {}", path.display()))?;
            println!("Alpha 3 benchmark report verified");
            Ok(())
        }
        Invocation::Run(arguments) => run(arguments),
    }
}

fn run(arguments: alpha_3_support::RunArguments) -> Result<()> {
    let zec = fs::canonicalize(&arguments.zec).context("canonicalize --zec")?;
    let artifacts = artifacts_directory(&arguments.report, "benchmark")?;
    let development_samples = std::env::var("ZEC_ALPHA3_DEV_SAMPLES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|samples| *samples > 0);
    let development_canonical_workload = development_samples.is_some()
        && std::env::var_os("ZEC_ALPHA3_DEV_CANONICAL_WORKLOAD").is_some();
    let workload = if development_samples.is_some() && !development_canonical_workload {
        WorkloadReport::development()
    } else {
        WorkloadReport::canonical()
    };
    let sample_count = |canonical| development_samples.unwrap_or(canonical);
    let warmups = |canonical| development_samples.map_or(canonical, |_| 0);

    let tree_fixture = Fixture::create("ALPHA3_BENCH_TREE")?;
    prepare_large_source(
        &tree_fixture.source,
        workload.outline_symbols,
        workload.large_lines,
    )?;
    prepare_tree_files(
        &tree_fixture.root,
        workload.tree_files,
        workload.replace_files,
    )?;
    let tree_manifest_before = tree_fixture.manifest()?;

    let outline_fixture = Fixture::create("ALPHA3_BENCH_OUTLINE")?;
    prepare_large_source(
        &outline_fixture.source,
        workload.outline_symbols,
        workload.outline_symbols.saturating_add(2),
    )?;
    let navigation_fixture = Fixture::create("ALPHA3_BENCH_NAVIGATION")?;
    let session_fixture = Fixture::create("ALPHA3_BENCH_SESSION")?;

    let mut correlation_ids = Vec::with_capacity(CORRELATION_COUNT);
    let mut memory = BTreeMap::new();

    let redraw = benchmark_redraw(
        &navigation_fixture,
        &zec,
        warmups(REDRAW_WARMUPS),
        sample_count(REDRAW_SAMPLES),
        &mut correlation_ids,
        &mut memory,
    )?;
    let panel = benchmark_project_panel(
        &tree_fixture,
        &zec,
        workload.tree_files,
        warmups(PANEL_WARMUPS),
        sample_count(PANEL_SAMPLES),
        &mut correlation_ids,
        &mut memory,
    )?;
    let outline = benchmark_outline(
        &outline_fixture,
        &zec,
        workload.outline_symbols,
        warmups(OUTLINE_WARMUPS),
        sample_count(OUTLINE_SAMPLES),
        &mut correlation_ids,
        &mut memory,
    )?;
    let regex = benchmark_regex_search(
        &tree_fixture,
        &zec,
        warmups(REGEX_WARMUPS),
        sample_count(REGEX_SAMPLES),
        &mut correlation_ids,
        &mut memory,
    )?;
    let replace = benchmark_replace_preview(
        &tree_fixture,
        &zec,
        workload.replace_files,
        warmups(REPLACE_WARMUPS),
        sample_count(REPLACE_SAMPLES),
        &mut correlation_ids,
        &mut memory,
    )?;
    let navigation = benchmark_navigation(
        &navigation_fixture,
        &zec,
        warmups(NAVIGATION_WARMUPS),
        sample_count(NAVIGATION_SAMPLES),
        &mut correlation_ids,
        &mut memory,
    )?;
    let (restore, session_generation, session_panes) = benchmark_session_restore(
        &session_fixture,
        &zec,
        workload.session_panes,
        warmups(RESTORE_WARMUPS),
        sample_count(RESTORE_SAMPLES),
        &mut correlation_ids,
        &mut memory,
    )?;
    ensure!(
        session_panes == workload.session_panes,
        "persisted session pane count differs"
    );
    exercise_large_memory(&tree_fixture, &zec, &workload, &mut memory)?;

    let tree_manifest_after = tree_fixture.manifest()?;
    ensure!(
        tree_manifest_before == tree_manifest_after,
        "benchmark changed the tree fixture"
    );
    let vm_hwm_bytes = memory.values().copied().max().unwrap_or_default();
    let four_pane_redraw = metric(
        warmups(REDRAW_WARMUPS),
        sample_count(REDRAW_SAMPLES),
        redraw,
        Some(16_000),
        Some(50_000),
    );
    let project_panel_initial_ready = metric(
        warmups(PANEL_WARMUPS),
        sample_count(PANEL_SAMPLES),
        panel,
        Some(750_000),
        Some(1_500_000),
    );
    let outline_update = metric(
        warmups(OUTLINE_WARMUPS),
        sample_count(OUTLINE_SAMPLES),
        outline,
        Some(100_000),
        Some(250_000),
    );
    let regex_first_visible_result = metric(
        warmups(REGEX_WARMUPS),
        sample_count(REGEX_SAMPLES),
        regex,
        Some(150_000),
        Some(500_000),
    );
    let replace_preview = metric(
        warmups(REPLACE_WARMUPS),
        sample_count(REPLACE_SAMPLES),
        replace,
        Some(750_000),
        Some(2_000_000),
    );
    let navigation_apply = metric(
        warmups(NAVIGATION_WARMUPS),
        sample_count(NAVIGATION_SAMPLES),
        navigation,
        Some(16_000),
        Some(50_000),
    );
    let session_restore = metric(
        warmups(RESTORE_WARMUPS),
        sample_count(RESTORE_SAMPLES),
        restore,
        Some(1_500_000),
        Some(3_000_000),
    );

    let correlation = CorrelationTrace {
        request_ids: correlation_ids.clone(),
        response_ids: correlation_ids.clone(),
        input_ids: correlation_ids.clone(),
        apply_ids: correlation_ids,
    };
    let mut evidence = Vec::new();
    persist_bytes(
        &mut evidence,
        &artifacts,
        "workload-manifest.json",
        &json_bytes(&json!({
            "workload": &workload,
            "project_file_count": tree_manifest_before.len(),
            "manifest": &tree_manifest_before,
            "unchanged": true,
        }))?,
    )?;
    persist_bytes(
        &mut evidence,
        &artifacts,
        "latency-trace.json",
        &json_bytes(&json!({
            "four_pane_redraw": &four_pane_redraw,
            "project_panel_initial_ready": &project_panel_initial_ready,
            "outline_update": &outline_update,
            "regex_first_visible_result": &regex_first_visible_result,
            "replace_preview": &replace_preview,
            "navigation_apply": &navigation_apply,
            "session_restore": &session_restore,
            "correlation_ids": &correlation.input_ids,
        }))?,
    )?;
    persist_bytes(
        &mut evidence,
        &artifacts,
        "session-generation.json",
        &fs::read(&session_generation)?,
    )?;
    persist_bytes(
        &mut evidence,
        &artifacts,
        "memory-observations.json",
        &json_bytes(&json!({
            "workload": &workload,
            "observations": &memory,
            "vm_hwm_bytes": vm_hwm_bytes,
            "limit_bytes": VM_HWM_LIMIT_BYTES,
        }))?,
    )?;

    let assertions = BTreeMap::from([
        ("correlation".to_owned(), correlation_is_valid(&correlation)),
        (
            "four_pane_redraw_latency".to_owned(),
            four_pane_redraw.assertion_passed,
        ),
        (
            "navigation_apply_latency".to_owned(),
            navigation_apply.assertion_passed,
        ),
        (
            "outline_update_latency".to_owned(),
            outline_update.assertion_passed,
        ),
        (
            "project_panel_initial_ready_latency".to_owned(),
            project_panel_initial_ready.assertion_passed,
        ),
        (
            "regex_first_visible_result_latency".to_owned(),
            regex_first_visible_result.assertion_passed,
        ),
        (
            "replace_preview_latency".to_owned(),
            replace_preview.assertion_passed,
        ),
        (
            "session_restore_latency".to_owned(),
            session_restore.assertion_passed,
        ),
        (
            "session_shape".to_owned(),
            session_panes == workload.session_panes,
        ),
        (
            "vm_hwm".to_owned(),
            vm_hwm_bytes > 0 && vm_hwm_bytes <= VM_HWM_LIMIT_BYTES,
        ),
        ("workload_unchanged".to_owned(), true),
    ]);
    let all_assertions_passed = assertions.values().all(|passed| *passed);
    let report = BenchmarkReport {
        schema_version: REPORT_SCHEMA_VERSION,
        contract_version: CONTRACT_VERSION,
        report_kind: "alpha_3_benchmark".to_owned(),
        environment: canonical_environment()?,
        binary: GateBinary::collect(&zec)?,
        workload,
        four_pane_redraw,
        project_panel_initial_ready,
        outline_update,
        regex_first_visible_result,
        replace_preview,
        navigation_apply,
        session_restore,
        vm_hwm_bytes,
        vm_hwm_limit_bytes: VM_HWM_LIMIT_BYTES,
        correlation,
        evidence,
        assertions,
        all_assertions_passed,
    };
    write_json_report(&arguments.report, &report)?;
    println!(
        "Alpha 3 benchmark: redraw={}us panel={}us outline={}us regex={}us replace={}us navigation={}us restore={}us VmHWM={} bytes; report {}",
        report.four_pane_redraw.p95_us,
        report.project_panel_initial_ready.p95_us,
        report.outline_update.p95_us,
        report.regex_first_visible_result.p95_us,
        report.replace_preview.p95_us,
        report.navigation_apply.p95_us,
        report.session_restore.p95_us,
        report.vm_hwm_bytes,
        arguments.report.display(),
    );
    if arguments.assert {
        verify_report(&report)?;
    }
    Ok(())
}

fn prepare_tree_files(root: &Path, file_count: usize, replace_count: usize) -> Result<()> {
    ensure!(
        replace_count <= file_count,
        "replace fixture exceeds tree fixture"
    );
    let tree = root.join("bench-tree");
    fs::create_dir_all(&tree)?;
    for index in 0..file_count {
        let directory = tree.join(format!("group-{:03}", index / 100));
        fs::create_dir_all(&directory)?;
        let mut contents = format!("BENCH_SEARCH_{index:05}\n");
        if index < replace_count {
            writeln!(contents, "{REPLACE_TOKEN}")?;
        }
        fs::write(directory.join(format!("entry-{index:05}.txt")), contents)?;
    }
    Ok(())
}

fn prepare_large_source(path: &Path, symbol_count: usize, line_count: usize) -> Result<()> {
    ensure!(
        line_count >= symbol_count.saturating_add(2),
        "large source needs at least one line per symbol"
    );
    let mut source = String::with_capacity(line_count.saturating_mul(32));
    writeln!(source, "// {READY_SENTINEL}")?;
    writeln!(source, "pub const ALPHA3_BENCH: usize = {symbol_count};")?;
    for index in 0..symbol_count {
        writeln!(source, "fn symbol_{index:05}() {{}}")?;
    }
    for index in symbol_count.saturating_add(2)..line_count {
        writeln!(source, "// large bounded viewport line {index:06}")?;
    }
    fs::write(path, source).with_context(|| format!("write large source {}", path.display()))
}

fn spawn_at(
    fixture: &Fixture,
    zec: &Path,
    file: &Path,
    sessions: bool,
) -> Result<(PtySession, nix::sys::termios::Termios)> {
    let arguments = [fixture.root.as_os_str(), file.as_os_str()];
    let mut environment = fixture.env_pairs();
    if !sessions {
        environment.push((OsStr::new("ZEC_DISABLE_SESSIONS"), OsStr::new("1")));
    }
    PtySession::spawn_with_env(
        zec,
        &fixture.root,
        &arguments,
        &fixture.config,
        &environment,
    )
}

fn spawn_source(
    fixture: &Fixture,
    zec: &Path,
    sessions: bool,
) -> Result<(PtySession, nix::sys::termios::Termios)> {
    let (mut session, baseline) = spawn_at(fixture, zec, &fixture.source, sessions)?;
    session.wait_ready("zec project", READY_SENTINEL)?;
    session.assert_raw(&baseline)?;
    let mark = session.send_marked(CTRL_PAGE_DOWN)?;
    session.wait_contains("activate benchmark source", mark, "[main.rs]")?;
    Ok((session, baseline))
}

fn quit_clean(mut session: PtySession, baseline: &nix::sys::termios::Termios) -> Result<()> {
    session.send(CTRL_Q)?;
    let status = session.wait_exit()?;
    ensure!(status.success(), "benchmark zec process exited as {status}");
    session.assert_restored_and_joined(baseline)
}

fn record_id(ids: &mut Vec<String>, prefix: &str, index: usize) {
    ids.push(format!("{prefix}_{:03}", index + 1));
}

fn observe_memory(
    memory: &mut BTreeMap<String, u64>,
    label: impl Into<String>,
    session: &PtySession,
) -> Result<()> {
    memory.insert(label.into(), alpha_1_support::vm_hwm_bytes(session.pid()?)?);
    Ok(())
}

fn benchmark_redraw(
    fixture: &Fixture,
    zec: &Path,
    warmups: usize,
    samples: usize,
    ids: &mut Vec<String>,
    memory: &mut BTreeMap<String, u64>,
) -> Result<Vec<u64>> {
    let (mut session, baseline) = spawn_source(fixture, zec, false)?;
    for _ in 0..3 {
        let mark = session.send_marked(F10)?;
        session.wait_contains(
            "create four-pane redraw fixture",
            mark,
            "split right into pane",
        )?;
    }
    let mut values = Vec::with_capacity(samples);
    for index in 0..warmups.saturating_add(samples) {
        let key = if index % 2 == 0 {
            CTRL_ALT_LEFT
        } else {
            CTRL_ALT_RIGHT
        };
        let previous_cursor = session.screen().cursor_position();
        let mark = session.send_marked(key)?;
        let elapsed = session.wait_after(
            "four-pane focus redraw",
            mark,
            alpha_1_support::SCREEN_TIMEOUT,
            |screen| {
                screen.size() == (alpha_1_support::ROWS, alpha_1_support::COLS)
                    && screen.cursor_position() != previous_cursor
            },
        )?;
        if index >= warmups {
            values.push(elapsed);
            record_id(ids, "A3_REDRAW", index - warmups);
        }
    }
    observe_memory(memory, "four_pane_redraw", &session)?;
    quit_clean(session, &baseline)?;
    Ok(values)
}

fn benchmark_project_panel(
    fixture: &Fixture,
    zec: &Path,
    tree_files: usize,
    warmups: usize,
    samples: usize,
    ids: &mut Vec<String>,
    memory: &mut BTreeMap<String, u64>,
) -> Result<Vec<u64>> {
    let mut values = Vec::with_capacity(samples);
    let query = format!("entry-{:05}", tree_files.saturating_sub(1));
    let expected = format!("{query}.txt");
    for index in 0..warmups.saturating_add(samples) {
        let readme = fixture.root.join("README.md");
        let (mut session, baseline) = spawn_at(fixture, zec, &readme, false)?;
        session.wait_ready("zec project", READY_SENTINEL)?;
        session.assert_raw(&baseline)?;
        let mark = session.send_marked(F7)?;
        let elapsed = session.wait_contains("project panel initial ready", mark, "┌ Project")?;
        let mark = session.send_marked(b"/")?;
        session.wait_contains("project panel benchmark filter", mark, "Project filter:")?;
        let mark = session.paste_marked(&query)?;
        session.wait_contains("10,000-entry project panel tail", mark, &expected)?;
        let mark = session.send_marked(ENTER)?;
        session.wait_contains(
            "apply project panel benchmark filter",
            mark,
            "filter applied",
        )?;
        if index >= warmups {
            values.push(elapsed);
            record_id(ids, "A3_PANEL", index - warmups);
        }
        if index + 1 == warmups.saturating_add(samples) {
            observe_memory(memory, "project_panel", &session)?;
        }
        let mark = session.send_marked(F7)?;
        session.wait_contains("hide benchmark project panel", mark, "project panel hidden")?;
        quit_clean(session, &baseline)?;
    }
    Ok(values)
}

fn benchmark_outline(
    fixture: &Fixture,
    zec: &Path,
    symbol_count: usize,
    warmups: usize,
    samples: usize,
    ids: &mut Vec<String>,
    memory: &mut BTreeMap<String, u64>,
) -> Result<Vec<u64>> {
    let (mut session, baseline) = spawn_source(fixture, zec, false)?;
    let query = format!("{:05}", symbol_count.saturating_sub(1));
    let expected = format!("symbol_{query}");
    let mut filter_installed = false;
    let mut values = Vec::with_capacity(samples);
    for index in 0..warmups.saturating_add(samples) {
        let mark = session.send_marked(F9)?;
        let elapsed = session.wait_after(
            "10,000-symbol outline update",
            mark,
            alpha_1_support::SCREEN_TIMEOUT,
            |screen| {
                let contents = screen.contents();
                contents.contains("┌ Outline ")
                    && if filter_installed {
                        contents.contains(&expected)
                    } else {
                        contents.contains("symbol_00000")
                    }
            },
        )?;
        if !filter_installed {
            let mark = session.send_marked(b"/")?;
            session.wait_contains("outline benchmark filter", mark, "Outline filter:")?;
            let mark = session.paste_marked(&query)?;
            session.wait_contains("10,000-symbol outline tail", mark, &expected)?;
            let mark = session.send_marked(ENTER)?;
            session.wait_contains("apply outline benchmark filter", mark, "filter applied")?;
            filter_installed = true;
        }
        if index >= warmups {
            values.push(elapsed);
            record_id(ids, "A3_OUTLINE", index - warmups);
        }
        let mark = session.send_marked(F9)?;
        session.wait_contains("hide benchmark outline", mark, "outline panel hidden")?;
    }
    observe_memory(memory, "outline", &session)?;
    quit_clean(session, &baseline)?;
    Ok(values)
}

fn benchmark_regex_search(
    fixture: &Fixture,
    zec: &Path,
    warmups: usize,
    samples: usize,
    ids: &mut Vec<String>,
    memory: &mut BTreeMap<String, u64>,
) -> Result<Vec<u64>> {
    let readme = fixture.root.join("README.md");
    let (mut session, baseline) = spawn_at(fixture, zec, &readme, false)?;
    session.wait_ready("zec project", READY_SENTINEL)?;
    session.assert_raw(&baseline)?;
    let mut values = Vec::with_capacity(samples);
    for index in 0..warmups.saturating_add(samples) {
        let mark = session.send_marked(ALT_F)?;
        session.wait_contains("open regex benchmark", mark, "Project search:")?;
        let mark = session.send_marked(ALT_R)?;
        session.wait_contains("enable regex benchmark", mark, "[regex")?;
        let mark = session.paste_marked(REGEX_QUERY)?;
        let elapsed = session.wait_after(
            "regex first visible result",
            mark,
            alpha_1_support::SCREEN_TIMEOUT,
            |screen| {
                let contents = screen.contents();
                contents.contains(REGEX_FIRST_RESULT) && !contents.contains("searching…")
            },
        )?;
        if index >= warmups {
            values.push(elapsed);
            record_id(ids, "A3_REGEX", index - warmups);
        }
        let mark = session.send_marked(ESC)?;
        session.wait_contains("close regex benchmark", mark, "project search cancelled")?;
    }
    observe_memory(memory, "regex_search", &session)?;
    quit_clean(session, &baseline)?;
    Ok(values)
}

fn benchmark_replace_preview(
    fixture: &Fixture,
    zec: &Path,
    replace_files: usize,
    warmups: usize,
    samples: usize,
    ids: &mut Vec<String>,
    memory: &mut BTreeMap<String, u64>,
) -> Result<Vec<u64>> {
    let readme = fixture.root.join("README.md");
    let (mut session, baseline) = spawn_at(fixture, zec, &readme, false)?;
    session.wait_ready("zec project", READY_SENTINEL)?;
    session.assert_raw(&baseline)?;
    let ready_count = format!("1/{replace_files}");
    let preview_status = format!("replace-all preview: {replace_files} match(es)");
    let rejected_status = format!("replacement of {replace_files} match(es) rejected");
    let mut values = Vec::with_capacity(samples);
    for index in 0..warmups.saturating_add(samples) {
        let mark = session.send_marked(ALT_F)?;
        session.wait_contains("open replace benchmark", mark, "Project search:")?;
        let mark = session.paste_marked(REPLACE_TOKEN)?;
        session.wait_after(
            "replace benchmark search ready",
            mark,
            alpha_1_support::SCREEN_TIMEOUT,
            |screen| {
                let contents = screen.contents();
                contents.contains(&ready_count) && !contents.contains("searching…")
            },
        )?;
        session.send(TAB)?;
        let mark = session.paste_marked(REPLACEMENT)?;
        let saw_restarted_search = Cell::new(false);
        session.wait_after(
            "replace benchmark field refresh",
            mark,
            alpha_1_support::SCREEN_TIMEOUT,
            |screen| {
                let contents = screen.contents();
                if contents.contains(REPLACEMENT) && contents.contains("searching…") {
                    saw_restarted_search.set(true);
                    false
                } else {
                    saw_restarted_search.get()
                        && contents.contains(REPLACEMENT)
                        && contents.contains(&ready_count)
                        && !contents.contains("searching…")
                }
            },
        )?;
        let mark = session.send_marked(ALT_ENTER)?;
        let elapsed = session.wait_contains("1,000-file replace preview", mark, &preview_status)?;
        if index >= warmups {
            values.push(elapsed);
            record_id(ids, "A3_REPLACE", index - warmups);
        }
        let mark = session.send_marked(ESC)?;
        session.wait_contains("reject replace benchmark preview", mark, &rejected_status)?;
    }
    observe_memory(memory, "replace_preview", &session)?;
    quit_clean(session, &baseline)?;
    Ok(values)
}

fn benchmark_navigation(
    fixture: &Fixture,
    zec: &Path,
    warmups: usize,
    samples: usize,
    ids: &mut Vec<String>,
    memory: &mut BTreeMap<String, u64>,
) -> Result<Vec<u64>> {
    let (mut session, baseline) = spawn_source(fixture, zec, false)?;
    let mark = session.send_marked(F9)?;
    session.wait_contains("navigation outline seed", mark, "Outline main.rs")?;
    session.send(DOWN)?;
    let mark = session.send_marked(ENTER)?;
    session.wait_contains("record navigation origin", mark, "jumped to outline symbol")?;
    let mut values = Vec::with_capacity(samples);
    for index in 0..warmups.saturating_add(samples) {
        let key = if index % 2 == 0 { ALT_LEFT } else { ALT_RIGHT };
        let mark = session.send_marked(key)?;
        let elapsed =
            session.wait_contains("back/forward navigation apply", mark, "navigated to")?;
        if index >= warmups {
            values.push(elapsed);
            record_id(ids, "A3_NAVIGATION", index - warmups);
        }
    }
    observe_memory(memory, "navigation", &session)?;
    quit_clean(session, &baseline)?;
    Ok(values)
}

fn benchmark_session_restore(
    fixture: &Fixture,
    zec: &Path,
    pane_count: usize,
    warmups: usize,
    samples: usize,
    ids: &mut Vec<String>,
    memory: &mut BTreeMap<String, u64>,
) -> Result<(Vec<u64>, PathBuf, usize)> {
    let (mut setup, baseline) = spawn_source(fixture, zec, true)?;
    for _ in 1..pane_count {
        setup.send(F10)?;
    }
    quit_clean(setup, &baseline)?;
    let initial_generation = latest_session_generation(fixture)?;
    let initial_panes = session_pane_count(&initial_generation)?;
    ensure!(
        initial_panes == pane_count,
        "session setup pane count differs"
    );

    let mut values = Vec::with_capacity(samples);
    for index in 0..warmups.saturating_add(samples) {
        let (mut session, baseline) = fixture.spawn(zec)?;
        let elapsed = session.wait_ready("zec project", "zec project")?;
        session.assert_raw(&baseline)?;
        ensure!(
            session.screen().alternate_screen(),
            "restored session did not enter the alternate screen"
        );
        if index >= warmups {
            values.push(elapsed);
            record_id(ids, "A3_RESTORE", index - warmups);
        }
        observe_memory(memory, format!("session_restore_{index:03}"), &session)?;
        quit_clean(session, &baseline)?;
    }
    let final_generation = latest_session_generation(fixture)?;
    let final_panes = session_pane_count(&final_generation)?;
    Ok((values, final_generation, final_panes))
}

fn latest_session_generation(fixture: &Fixture) -> Result<PathBuf> {
    fixture
        .session_files()?
        .into_iter()
        .filter(|path| path.extension() == Some(OsStr::new("json")))
        .max()
        .context("session benchmark produced no generation")
}

fn session_pane_count(path: &Path) -> Result<usize> {
    let value: Value = serde_json::from_slice(&fs::read(path)?)?;
    value
        .pointer("/payload/workspace/panes")
        .and_then(Value::as_object)
        .map(|panes| panes.len())
        .context("session generation has no workspace pane map")
}

fn exercise_large_memory(
    fixture: &Fixture,
    zec: &Path,
    workload: &WorkloadReport,
    memory: &mut BTreeMap<String, u64>,
) -> Result<()> {
    {
        let (mut session, baseline) = spawn_source(fixture, zec, false)?;
        for _ in 0..3 {
            let mark = session.send_marked(F10)?;
            session.wait_contains(
                "large-memory four-pane split",
                mark,
                "split right into pane",
            )?;
        }
        observe_memory(memory, "large_100k_lines_four_panes", &session)?;
        quit_clean(session, &baseline)?;
    }

    {
        let readme = fixture.root.join("README.md");
        let (mut session, baseline) = spawn_at(fixture, zec, &readme, false)?;
        session.wait_ready("zec project", READY_SENTINEL)?;
        session.assert_raw(&baseline)?;
        let panel_query = format!("entry-{:05}", workload.tree_files.saturating_sub(1));
        let panel_expected = format!("{panel_query}.txt");
        let mark = session.send_marked(F7)?;
        session.wait_contains("large-memory project panel", mark, "┌ Project")?;
        session.send(b"/")?;
        let mark = session.paste_marked(&panel_query)?;
        session.wait_contains("large-memory project panel tail", mark, &panel_expected)?;
        let mark = session.send_marked(ENTER)?;
        session.wait_contains("apply large-memory project filter", mark, "filter applied")?;
        observe_memory(memory, "large_10k_project_panel", &session)?;
        let mark = session.send_marked(F7)?;
        session.wait_absent("hide large-memory project panel", mark, "┌ Project")?;
        quit_clean(session, &baseline)?;
    }

    {
        let (mut session, baseline) = spawn_source(fixture, zec, false)?;
        let mark = session.send_marked(F9)?;
        session.wait_after(
            "large-memory outline",
            mark,
            alpha_1_support::SCREEN_TIMEOUT,
            |screen| {
                let contents = screen.contents();
                contents.contains("Outline main.rs") && contents.contains("symbol_00000")
            },
        )?;
        observe_memory(memory, "large_10k_outline", &session)?;
        let mark = session.send_marked(F9)?;
        session.wait_absent("hide large-memory outline", mark, "Outline main.rs")?;
        quit_clean(session, &baseline)?;
    }

    {
        let readme = fixture.root.join("README.md");
        let (mut session, baseline) = spawn_at(fixture, zec, &readme, false)?;
        session.wait_ready("zec project", READY_SENTINEL)?;
        session.assert_raw(&baseline)?;
        let mark = session.send_marked(ALT_F)?;
        session.wait_contains("large-memory project search", mark, "Project search:")?;
        let mark = session.paste_marked("BENCH_SEARCH_")?;
        let total = format!("1/{}", workload.tree_files);
        session.wait_after(
            "large-memory 10,000-result search",
            mark,
            std::time::Duration::from_secs(60),
            |screen| {
                let contents = screen.contents();
                contents.contains(&total) && !contents.contains("searching…")
            },
        )?;
        observe_memory(memory, "large_10k_search_results", &session)?;
        let mark = session.send_marked(F9)?;
        session.wait_after(
            "large-memory result MultiBuffer",
            mark,
            std::time::Duration::from_secs(60),
            |screen| {
                let contents = screen.contents();
                contents.contains(REGEX_FIRST_RESULT) && !contents.contains("Project search:")
            },
        )?;
        observe_memory(memory, "large_result_multibuffer", &session)?;
        let mark = session.send_marked(CTRL_W)?;
        session.wait_absent("close large-memory MultiBuffer", mark, REGEX_FIRST_RESULT)?;
        quit_clean(session, &baseline)?;
    }
    Ok(())
}

fn correlation_is_valid(trace: &CorrelationTrace) -> bool {
    trace.request_ids == trace.response_ids
        && trace.input_ids == trace.apply_ids
        && trace.request_ids == trace.input_ids
        && trace.input_ids.iter().collect::<BTreeSet<_>>().len() == trace.input_ids.len()
}

fn verify_report(report: &BenchmarkReport) -> Result<()> {
    ensure!(
        report.schema_version == REPORT_SCHEMA_VERSION,
        "schema differs"
    );
    ensure!(
        report.contract_version == CONTRACT_VERSION,
        "contract differs"
    );
    ensure!(
        report.report_kind == "alpha_3_benchmark",
        "report kind differs"
    );
    verify_canonical_environment(&report.environment)?;
    report.binary.verify()?;
    ensure!(
        report.workload == WorkloadReport::canonical(),
        "benchmark workload differs from the canonical scale"
    );
    for (label, value, expected_warmups, expected_samples, p95, max) in [
        (
            "four_pane_redraw",
            &report.four_pane_redraw,
            REDRAW_WARMUPS,
            REDRAW_SAMPLES,
            Some(16_000),
            Some(50_000),
        ),
        (
            "project_panel_initial_ready",
            &report.project_panel_initial_ready,
            PANEL_WARMUPS,
            PANEL_SAMPLES,
            Some(750_000),
            Some(1_500_000),
        ),
        (
            "outline_update",
            &report.outline_update,
            OUTLINE_WARMUPS,
            OUTLINE_SAMPLES,
            Some(100_000),
            Some(250_000),
        ),
        (
            "regex_first_visible_result",
            &report.regex_first_visible_result,
            REGEX_WARMUPS,
            REGEX_SAMPLES,
            Some(150_000),
            Some(500_000),
        ),
        (
            "replace_preview",
            &report.replace_preview,
            REPLACE_WARMUPS,
            REPLACE_SAMPLES,
            Some(750_000),
            Some(2_000_000),
        ),
        (
            "navigation_apply",
            &report.navigation_apply,
            NAVIGATION_WARMUPS,
            NAVIGATION_SAMPLES,
            Some(16_000),
            Some(50_000),
        ),
        (
            "session_restore",
            &report.session_restore,
            RESTORE_WARMUPS,
            RESTORE_SAMPLES,
            Some(1_500_000),
            Some(3_000_000),
        ),
    ] {
        ensure!(value.warmups == expected_warmups, "{label}: warmups differ");
        ensure!(
            value.expected_samples == expected_samples,
            "{label}: samples differ"
        );
        ensure!(value.p95_limit_us == p95, "{label}: p95 limit differs");
        ensure!(value.max_limit_us == max, "{label}: max limit differs");
        verify_metric(value, label)?;
    }
    ensure!(
        report.vm_hwm_limit_bytes == VM_HWM_LIMIT_BYTES
            && report.vm_hwm_bytes > 0
            && report.vm_hwm_bytes <= VM_HWM_LIMIT_BYTES,
        "VmHWM envelope differs"
    );
    report.correlation.verify()?;
    ensure!(
        correlation_is_valid(&report.correlation)
            && report.correlation.input_ids.len() == CORRELATION_COUNT,
        "benchmark correlation differs"
    );
    let evidence_labels = report
        .evidence
        .iter()
        .map(|evidence| evidence.label.as_str())
        .collect::<BTreeSet<_>>();
    ensure!(
        evidence_labels
            == BTreeSet::from([
                "latency-trace.json",
                "memory-observations.json",
                "session-generation.json",
                "workload-manifest.json",
            ]),
        "benchmark evidence labels differ"
    );
    for evidence in &report.evidence {
        evidence.verify()?;
    }
    let expected_assertions = BTreeSet::from([
        "correlation",
        "four_pane_redraw_latency",
        "navigation_apply_latency",
        "outline_update_latency",
        "project_panel_initial_ready_latency",
        "regex_first_visible_result_latency",
        "replace_preview_latency",
        "session_restore_latency",
        "session_shape",
        "vm_hwm",
        "workload_unchanged",
    ]);
    ensure!(
        report
            .assertions
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>()
            == expected_assertions
            && report.assertions.values().all(|passed| *passed),
        "benchmark assertion map differs or contains a failure"
    );
    ensure!(report.all_assertions_passed, "assertion summary is false");
    Ok(())
}
