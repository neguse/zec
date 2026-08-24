mod alpha_1_support;

use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    sync::mpsc::{self, Sender, TryRecvError},
    thread::{self, JoinHandle},
    time::Duration,
};

use alpha_1_support::{
    ALT_F, BenchmarkReport, CTRL_G, CTRL_P, CTRL_Q, CTRL_S, DELETE, DOWN, ENTER, ESC, InputTrace,
    Invocation, MetricReport, PtySession, REPORT_SCHEMA_VERSION, binary_report,
    descendant_process_count, environment_report, fixture, oracle_hashes, parse_invocation,
    reset_fixed_fixture, verify_benchmark_report, vm_hwm_bytes, write_report,
};
use alpha_1_support::{
    SearchResultReport, expected_benchmark_quick_open_queries, expected_benchmark_search_rows,
};
use anyhow::{Context as _, Result, ensure};
use nix::sys::termios::Termios;
use serde::Deserialize;

const STARTUP_WARMUPS: usize = 2;
const STARTUP_SAMPLES: usize = 20;
const QUICK_WARMUPS: usize = 10;
const QUICK_SAMPLES: usize = 100;
const PROJECT_WARMUPS: usize = 2;
const PROJECT_SAMPLES: usize = 10;
const IN_FLIGHT_ATTEMPTS: usize = 20;
const EDIT_WARMUPS: usize = 10;
const EDIT_SAMPLES: usize = 500;
const SAVE_WARMUPS: usize = 2;
const SAVE_SAMPLES: usize = 10;

fn main() -> Result<()> {
    match parse_invocation(false)? {
        Invocation::Verify(path) => {
            let report = alpha_1_support::read_report::<BenchmarkReport>(&path)?;
            verify_benchmark_report(&report)
                .with_context(|| format!("verify {}", path.display()))?;
            println!("Alpha 1 benchmark report verified");
            Ok(())
        }
        Invocation::Run(arguments) => run(arguments),
    }
}

fn run(arguments: alpha_1_support::RunArguments) -> Result<()> {
    let zec = fs::canonicalize(&arguments.zec).context("canonicalize --zec")?;
    let generated = reset_fixed_fixture()?;
    let mut resources = ResourceTracker::default();

    let startup = startup_metric(&zec, &generated.root, &mut resources)?;
    let quick_open = quick_open_metric(&zec, &generated.root, &mut resources)?;
    let (project_search, search_observation) =
        project_search_metric(&zec, &generated.root, &mut resources)?;
    let (replace_query, cancel_search, quit_in_flight_search) =
        in_flight_metrics(&zec, &generated.root, &mut resources)?;
    let (editing, input_trace) = editing_metric(&zec, &generated.root, &mut resources)?;
    let save = save_metric(&zec, &generated.root, &mut resources)?;

    let assertions = BTreeMap::from([
        ("startup_latency".to_owned(), startup.assertion_passed),
        ("quick_open_latency".to_owned(), quick_open.assertion_passed),
        (
            "project_search_latency".to_owned(),
            project_search.assertion_passed,
        ),
        (
            "replace_query_latency".to_owned(),
            replace_query.assertion_passed,
        ),
        (
            "cancel_search_latency".to_owned(),
            cancel_search.assertion_passed,
        ),
        (
            "quit_in_flight_latency".to_owned(),
            quit_in_flight_search.assertion_passed,
        ),
        ("editing_latency".to_owned(), editing.assertion_passed),
        ("save_latency".to_owned(), save.assertion_passed),
        (
            "project_search_hits".to_owned(),
            search_observation.total_hits == fixture::BENCH_SEARCH_HITS
                && search_observation.visible_results.len() == fixture::SEARCH_RESULT_LIMIT
                && search_observation.visible_results == expected_benchmark_search_rows(),
        ),
        (
            "vm_hwm".to_owned(),
            resources.max_vm_hwm_bytes <= 1_073_741_824,
        ),
        (
            "no_descendant_processes".to_owned(),
            resources.max_descendant_count == 0,
        ),
        (
            "input_ids".to_owned(),
            input_trace.sent_input_ids == input_trace.expected_input_ids
                && input_trace.applied_input_ids == input_trace.expected_input_ids
                && input_trace.dropped_count == 0
                && !input_trace.reordered,
        ),
    ]);
    let all_assertions_passed = assertions.values().all(|passed| *passed);

    let report = BenchmarkReport {
        schema_version: REPORT_SCHEMA_VERSION,
        contract_version: fixture::CONTRACT_VERSION,
        report_kind: "alpha_1_benchmark".to_owned(),
        environment: environment_report()?,
        binary: binary_report(&zec)?,
        oracles: oracle_hashes(),
        startup,
        quick_open,
        project_search,
        replace_query,
        cancel_search,
        quit_in_flight_search,
        editing,
        save,
        project_search_total_hits: search_observation.total_hits,
        project_search_visible_results: search_observation.visible_results.len(),
        project_search_rows: search_observation.visible_results,
        vm_hwm_bytes: resources.max_vm_hwm_bytes,
        vm_hwm_limit_bytes: 1_073_741_824,
        descendant_process_count: resources.max_descendant_count,
        input_trace,
        assertions,
        all_assertions_passed,
    };
    write_report(&arguments.report, &report)?;
    println!(
        "Alpha 1 benchmark: startup p95={}us, quick-open p95={}us, search p95={}us, edit p95={}us, save max={}us; report {}",
        report.startup.p95_us,
        report.quick_open.p95_us,
        report.project_search.p95_us,
        report.editing.p95_us,
        report.save.max_us,
        arguments.report.display()
    );
    if arguments.assert {
        verify_benchmark_report(&report)?;
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct ProjectSearchProbe {
    query: String,
    total_hits: usize,
    visible_results: Vec<SearchResultReport>,
}

#[derive(Default)]
struct ResourceTracker {
    max_vm_hwm_bytes: u64,
    max_descendant_count: usize,
}

impl ResourceTracker {
    fn finish(&mut self, monitor: ResourceMonitor) -> Result<()> {
        let observation = monitor.finish()?;
        self.max_vm_hwm_bytes = self.max_vm_hwm_bytes.max(observation.max_vm_hwm_bytes);
        self.max_descendant_count = self
            .max_descendant_count
            .max(observation.max_descendant_count);
        Ok(())
    }
}

#[derive(Default)]
struct ResourceObservation {
    max_vm_hwm_bytes: u64,
    max_descendant_count: usize,
}

struct ResourceMonitor {
    stop: Sender<()>,
    handle: Option<JoinHandle<Result<ResourceObservation>>>,
}

impl ResourceMonitor {
    fn start(pid: i32) -> Result<Self> {
        let initial_vm_hwm_bytes = vm_hwm_bytes(pid)?;
        let initial_descendant_count = descendant_process_count(pid)?;
        let (stop, receiver) = mpsc::channel();
        let handle = thread::Builder::new()
            .name(format!("alpha-1-resource-{pid}"))
            .spawn(move || {
                let mut observation = ResourceObservation {
                    max_vm_hwm_bytes: initial_vm_hwm_bytes,
                    max_descendant_count: initial_descendant_count,
                };
                loop {
                    let proc_path = format!("/proc/{pid}");
                    match vm_hwm_bytes(pid) {
                        Ok(bytes) => {
                            observation.max_vm_hwm_bytes = observation.max_vm_hwm_bytes.max(bytes);
                        }
                        Err(_) if !Path::new(&proc_path).exists() => {}
                        Err(error) => return Err(error),
                    }
                    match descendant_process_count(pid) {
                        Ok(count) => {
                            observation.max_descendant_count =
                                observation.max_descendant_count.max(count);
                        }
                        Err(_) if !Path::new(&proc_path).exists() => {}
                        Err(error) => return Err(error),
                    }
                    match receiver.try_recv() {
                        Ok(()) | Err(TryRecvError::Disconnected) => break,
                        Err(TryRecvError::Empty) => {
                            thread::sleep(Duration::from_millis(1));
                        }
                    }
                }
                Ok(observation)
            })
            .context("spawn continuous resource monitor")?;
        Ok(Self {
            stop,
            handle: Some(handle),
        })
    }

    fn finish(mut self) -> Result<ResourceObservation> {
        let _ = self.stop.send(());
        let handle = self
            .handle
            .take()
            .context("resource monitor handle is absent")?;
        join_resource_monitor(handle)
    }
}

impl Drop for ResourceMonitor {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(handle) = self.handle.take() {
            let deadline = std::time::Instant::now() + alpha_1_support::CHILD_TIMEOUT;
            while !handle.is_finished() && std::time::Instant::now() < deadline {
                thread::sleep(Duration::from_millis(1));
            }
            if handle.is_finished() {
                let _ = handle.join();
            }
        }
    }
}

fn join_resource_monitor(
    handle: JoinHandle<Result<ResourceObservation>>,
) -> Result<ResourceObservation> {
    let deadline = std::time::Instant::now() + alpha_1_support::CHILD_TIMEOUT;
    while !handle.is_finished() {
        let now = std::time::Instant::now();
        ensure!(
            now < deadline,
            "resource monitor did not finish within 5 seconds"
        );
        thread::sleep((deadline - now).min(Duration::from_millis(1)));
    }
    handle
        .join()
        .map_err(|_| anyhow::anyhow!("resource monitor thread panicked"))?
}

fn spawn_ready(
    zec: &Path,
    root: &Path,
    config_name: &str,
) -> Result<(PtySession, Termios, u64, ResourceMonitor)> {
    spawn_ready_with_env(zec, root, config_name, &[])
}

fn spawn_ready_with_env(
    zec: &Path,
    root: &Path,
    config_name: &str,
    environment: &[(&std::ffi::OsStr, &std::ffi::OsStr)],
) -> Result<(PtySession, Termios, u64, ResourceMonitor)> {
    let config = alpha_1_support::fresh_config_dir(config_name)?;
    let (mut session, baseline) =
        PtySession::spawn_with_env(zec, root, &[root.as_os_str()], &config, environment)?;
    let monitor = ResourceMonitor::start(session.pid()?)?;
    let startup_us = session.wait_ready("repo", fixture::READY_SENTINEL)?;
    session.assert_raw(&baseline)?;
    Ok((session, baseline, startup_us, monitor))
}

fn startup_metric(
    zec: &Path,
    root: &Path,
    resources: &mut ResourceTracker,
) -> Result<MetricReport> {
    let mut samples = Vec::with_capacity(STARTUP_SAMPLES);
    for index in 0..STARTUP_WARMUPS + STARTUP_SAMPLES {
        let (mut session, baseline, elapsed, monitor) =
            spawn_ready(zec, root, &format!("bench-startup-{index:02}"))?;
        if index >= STARTUP_WARMUPS {
            samples.push(elapsed);
        }
        quit_clean(&mut session, &baseline, resources, monitor)?;
    }
    Ok(MetricReport::new(
        STARTUP_WARMUPS,
        STARTUP_SAMPLES,
        samples,
        Some(3_000_000),
        None,
    ))
}

fn quick_open_metric(
    zec: &Path,
    root: &Path,
    resources: &mut ResourceTracker,
) -> Result<MetricReport> {
    let (mut session, baseline, _, monitor) = spawn_ready(zec, root, "bench-quick-open")?;
    let queries = expected_benchmark_quick_open_queries();
    ensure!(
        queries.len() == QUICK_SAMPLES,
        "quick-open spec query count differs"
    );
    let mut samples = Vec::with_capacity(QUICK_SAMPLES);
    let attempts = queries
        .iter()
        .take(QUICK_WARMUPS)
        .map(|query| (false, query))
        .chain(queries.iter().map(|query| (true, query)));
    for (measured, query) in attempts {
        session.send(CTRL_P)?;
        let prompt = session.mark();
        session.wait_contains("benchmark quick-open prompt", prompt, "Quick open:")?;
        let operation = session.paste_marked(&query.query)?;
        let expected_status = format!(
            "Quick open: {}  1/1  {}",
            query.query, query.expected_selected_path
        );
        let elapsed = session.wait_contains(
            "benchmark exact quick-open result",
            operation,
            &expected_status,
        )?;
        if measured {
            samples.push(elapsed);
        }
        let cancel = session.send_marked(ESC)?;
        session.wait_absent("benchmark quick-open cancel", cancel, "Quick open:")?;
    }
    quit_clean(&mut session, &baseline, resources, monitor)?;
    Ok(MetricReport::new(
        QUICK_WARMUPS,
        QUICK_SAMPLES,
        samples,
        Some(150_000),
        Some(500_000),
    ))
}

fn project_search_metric(
    zec: &Path,
    root: &Path,
    resources: &mut ResourceTracker,
) -> Result<(MetricReport, ProjectSearchProbe)> {
    let expected_rows = expected_benchmark_search_rows();
    ensure!(
        expected_rows.len() == fixture::SEARCH_RESULT_LIMIT,
        "project-search row oracle count differs"
    );
    let mut samples = Vec::with_capacity(PROJECT_SAMPLES);
    let mut observation = None;
    for index in 0..PROJECT_WARMUPS + PROJECT_SAMPLES {
        let observation_path = root
            .parent()
            .context("fixture root has no workspace parent")?
            .join(format!("project-search-observation-{index:02}.json"));
        ensure!(
            !observation_path.exists(),
            "project-search observation path was not fresh"
        );
        let environment = [(
            std::ffi::OsStr::new("ZEC_ALPHA1_SEARCH_OBSERVATION"),
            observation_path.as_os_str(),
        )];
        let (mut session, baseline, _, monitor) = spawn_ready_with_env(
            zec,
            root,
            &format!("bench-project-{index:02}"),
            &environment,
        )?;
        session.send(ALT_F)?;
        let prompt = session.mark();
        session.wait_contains("benchmark project-search prompt", prompt, "Project search:")?;
        let operation = session.paste_marked("ALPHA1_BENCH_SEARCH")?;
        let first = &expected_rows[0];
        let expected_status = format!(
            "Project search: ALPHA1_BENCH_SEARCH  1/{}  {}:{}:{}  {}",
            fixture::BENCH_SEARCH_HITS,
            first.path,
            first.line,
            first.column,
            first.preview
        );
        let elapsed = session.wait_contains(
            "exact 1,000-hit completed project-search generation",
            operation,
            &expected_status,
        )?;
        let bytes =
            fs::read(&observation_path).context("read same-process project-search observation")?;
        let probe: ProjectSearchProbe =
            serde_json::from_slice(&bytes).context("parse project-search observation JSON")?;
        ensure!(
            probe.query == "ALPHA1_BENCH_SEARCH"
                && probe.total_hits == fixture::BENCH_SEARCH_HITS
                && probe.visible_results == expected_rows,
            "same-process project-search observation differs from spec"
        );
        if let Some(previous) = &observation {
            ensure!(previous == &probe, "project-search observations differ");
        } else {
            observation = Some(probe.clone());
        }
        if index == PROJECT_WARMUPS {
            assert_ordered_project_search_rows(&mut session, &expected_rows)?;
        }
        if index >= PROJECT_WARMUPS {
            samples.push(elapsed);
        }
        quit_clean(&mut session, &baseline, resources, monitor)?;
    }
    Ok((
        MetricReport::new(
            PROJECT_WARMUPS,
            PROJECT_SAMPLES,
            samples,
            Some(5_000_000),
            None,
        ),
        observation.context("project-search produced no observation")?,
    ))
}

fn assert_ordered_project_search_rows(
    session: &mut PtySession,
    expected_rows: &[SearchResultReport],
) -> Result<()> {
    for (index, row) in expected_rows.iter().enumerate().skip(1) {
        let moved = session.send_marked(DOWN)?;
        let expected_status = format!(
            "Project search: ALPHA1_BENCH_SEARCH  {}/{}  {}:{}:{}  {}",
            index + 1,
            fixture::BENCH_SEARCH_HITS,
            row.path,
            row.line,
            row.column,
            row.preview
        );
        session.wait_contains(
            &format!("ordered project-search row {}", index + 1),
            moved,
            &expected_status,
        )?;
    }
    Ok(())
}

fn in_flight_metrics(
    zec: &Path,
    root: &Path,
    resources: &mut ResourceTracker,
) -> Result<(MetricReport, MetricReport, MetricReport)> {
    let mut replace = Vec::with_capacity(IN_FLIGHT_ATTEMPTS);
    let mut cancel = Vec::with_capacity(IN_FLIGHT_ATTEMPTS);
    let mut quit = Vec::with_capacity(IN_FLIGHT_ATTEMPTS);

    for index in 0..IN_FLIGHT_ATTEMPTS {
        let (mut session, baseline, _, monitor) =
            spawn_ready(zec, root, &format!("bench-replace-{index:02}"))?;
        begin_in_flight_search(&mut session, "ALPHA1_STALE_A")?;
        for _ in 0.."ALPHA1_STALE_A".len() {
            session.send(b"\x7f")?;
        }
        let operation = session.paste_marked("ALPHA1_STALE_B")?;
        replace.push(session.wait_contains(
            "exact replacement query result",
            operation,
            "Project search: ALPHA1_STALE_B  1/1  src/stale-b.txt:1:1  ALPHA1_STALE_B current query result",
        )?);
        quit_clean(&mut session, &baseline, resources, monitor)?;
    }

    for index in 0..IN_FLIGHT_ATTEMPTS {
        let (mut session, baseline, _, monitor) =
            spawn_ready(zec, root, &format!("bench-cancel-{index:02}"))?;
        begin_in_flight_search(&mut session, "ALPHA1_BENCH_SEARCH")?;
        let operation = session.send_marked(ESC)?;
        cancel.push(session.wait_absent(
            "cancel in-flight project search",
            operation,
            "Project search:",
        )?);
        quit_clean(&mut session, &baseline, resources, monitor)?;
    }

    for index in 0..IN_FLIGHT_ATTEMPTS {
        let (mut session, baseline, _, monitor) =
            spawn_ready(zec, root, &format!("bench-quit-{index:02}"))?;
        begin_in_flight_search(&mut session, "ALPHA1_BENCH_SEARCH")?;
        let operation = session.send_marked(CTRL_Q)?;
        let status = session.wait_exit()?;
        ensure!(status.success(), "in-flight Ctrl-Q failed: {status}");
        quit.push(operation.elapsed_us());
        session.assert_restored_and_joined(&baseline)?;
        resources.finish(monitor)?;
    }

    Ok((
        MetricReport::new(0, IN_FLIGHT_ATTEMPTS, replace, None, Some(250_000)),
        MetricReport::new(0, IN_FLIGHT_ATTEMPTS, cancel, None, Some(250_000)),
        MetricReport::new(0, IN_FLIGHT_ATTEMPTS, quit, None, Some(250_000)),
    ))
}

fn editing_metric(
    zec: &Path,
    root: &Path,
    resources: &mut ResourceTracker,
) -> Result<(MetricReport, InputTrace)> {
    let (mut session, baseline, _, monitor) = spawn_ready(zec, root, "bench-editing")?;
    let path = root.join("bench/large-100000-lines.txt");
    let original = fs::read(&path).context("read original 100,000-line file")?;
    quick_open(
        &mut session,
        "large-100000-lines.txt",
        "bench/large-100000-lines.txt",
    )?;
    session.send(CTRL_G)?;
    session.paste("50000:1")?;
    session.send(ENTER)?;
    let positioned = session.mark();
    session.wait_contains(
        "100,000-line edit position",
        positioned,
        "large-line-049999",
    )?;

    let expected_ids = (1..=EDIT_SAMPLES)
        .map(|sequence| format!("EDIT_{sequence:04}"))
        .collect::<Vec<_>>();
    let warmup_ids = (0..EDIT_WARMUPS)
        .map(|sequence| format!("WARMUP_{sequence:02}"))
        .collect::<Vec<_>>();
    let mut sent = Vec::with_capacity(EDIT_SAMPLES);
    let mut applied = Vec::with_capacity(EDIT_SAMPLES);
    let mut samples = Vec::with_capacity(EDIT_SAMPLES);
    for index in 0..EDIT_WARMUPS + EDIT_SAMPLES {
        let id = if index < EDIT_WARMUPS {
            warmup_ids[index].clone()
        } else {
            expected_ids[index - EDIT_WARMUPS].clone()
        };
        let operation = session.paste_marked(&id)?;
        if index >= EDIT_WARMUPS {
            sent.push(id.clone());
        }
        let elapsed = session.wait_after(
            "sequential edit at cursor-relative expected cells",
            operation,
            alpha_1_support::SCREEN_TIMEOUT,
            |screen| {
                token_immediately_before_cursor(screen, &id)
                    && screen.contents().contains("large-100000-lines.txt+]")
            },
        )?;
        if index >= EDIT_WARMUPS {
            applied.push(id);
            samples.push(elapsed);
        }
    }

    let saved = session.send_marked(CTRL_S)?;
    session.wait_after(
        "editing buffer saved with dirty marker cleared",
        saved,
        alpha_1_support::SCREEN_TIMEOUT,
        |screen| {
            let contents = screen.contents();
            contents.contains("saved")
                && contents.contains("large-100000-lines.txt]")
                && !contents.contains("large-100000-lines.txt+]")
        },
    )?;
    let marker = b"large-line-049999";
    let insertion_offset = original
        .windows(marker.len())
        .position(|window| window == marker)
        .context("line 50,000 marker is absent")?;
    let warmup_payload = warmup_ids.concat();
    let edit_payload = expected_ids.concat();
    let mut expected_disk =
        Vec::with_capacity(original.len() + warmup_payload.len() + edit_payload.len());
    expected_disk.extend_from_slice(&original[..insertion_offset]);
    expected_disk.extend_from_slice(warmup_payload.as_bytes());
    expected_disk.extend_from_slice(edit_payload.as_bytes());
    expected_disk.extend_from_slice(&original[insertion_offset..]);
    let disk = fs::read(&path).context("read saved editing buffer")?;
    ensure!(
        disk == expected_disk,
        "final 100,000-line Zed buffer bytes differ"
    );

    let observed_edit = &disk[insertion_offset + warmup_payload.len()
        ..insertion_offset + warmup_payload.len() + edit_payload.len()];
    let mut file_ids = Vec::with_capacity(expected_ids.len());
    let mut offset = 0;
    for expected in &expected_ids {
        let end = offset + expected.len();
        let observed = std::str::from_utf8(&observed_edit[offset..end])
            .context("saved edit ID is not UTF-8")?;
        ensure!(observed == expected, "saved edit ID sequence differs");
        file_ids.push(observed.to_owned());
        offset = end;
    }
    quit_clean(&mut session, &baseline, resources, monitor)?;

    let dropped_count = expected_ids.len().saturating_sub(applied.len());
    let reordered = sent != applied || applied != file_ids;
    let trace = InputTrace {
        sent_input_ids: sent,
        applied_input_ids: applied,
        expected_input_ids: file_ids,
        dropped_count,
        reordered,
    };
    Ok((
        MetricReport::new(
            EDIT_WARMUPS,
            EDIT_SAMPLES,
            samples,
            Some(100_000),
            Some(500_000),
        ),
        trace,
    ))
}

fn save_metric(zec: &Path, root: &Path, resources: &mut ResourceTracker) -> Result<MetricReport> {
    let (mut session, baseline, _, monitor) = spawn_ready(zec, root, "bench-save")?;
    quick_open(&mut session, "save-5mib.txt", "bench/save-5mib.txt")?;
    session.send(CTRL_G)?;
    session.paste("1:1")?;
    session.send(ENTER)?;
    let positioned = session.mark();
    session.wait_contains("5 MiB save position", positioned, "Ln 1, Col 1")?;

    let path = root.join("bench/save-5mib.txt");
    let mut expected_disk = fs::read(&path).context("read original 5 MiB save file")?;
    ensure!(
        expected_disk.len() == 5 * 1024 * 1024,
        "save file size differs"
    );
    let mut samples = Vec::with_capacity(SAVE_SAMPLES);
    for index in 0..SAVE_WARMUPS + SAVE_SAMPLES {
        session.send(b"\x1b[H")?;
        session.send(DELETE)?;
        let replacement = if index % 2 == 0 { "a" } else { "b" };
        let edited = session.paste_marked(replacement)?;
        session.wait_after(
            "5 MiB buffer became dirty at the fixed cell",
            edited,
            alpha_1_support::SCREEN_TIMEOUT,
            |screen| {
                token_immediately_before_cursor(screen, replacement)
                    && screen.contents().contains("save-5mib.txt+]")
            },
        )?;
        ensure!(
            fs::read(&path)? == expected_disk,
            "disk bytes changed before Ctrl-S"
        );
        expected_disk[0] = replacement.as_bytes()[0];
        let operation = session.send_marked(CTRL_S)?;
        let ui_elapsed = session.wait_after(
            "5 MiB save completion with dirty marker cleared",
            operation,
            alpha_1_support::SCREEN_TIMEOUT,
            |screen| {
                let contents = screen.contents();
                contents.contains("saved")
                    && contents.contains("save-5mib.txt]")
                    && !contents.contains("save-5mib.txt+]")
            },
        )?;
        let disk = fs::read(&path).context("read 5 MiB save result")?;
        ensure!(disk == expected_disk, "full saved 5 MiB bytes differ");
        let elapsed = ui_elapsed.max(operation.elapsed_us());
        if index >= SAVE_WARMUPS {
            samples.push(elapsed);
        }
    }
    quit_clean(&mut session, &baseline, resources, monitor)?;
    Ok(MetricReport::new(
        SAVE_WARMUPS,
        SAVE_SAMPLES,
        samples,
        None,
        Some(2_000_000),
    ))
}

fn token_immediately_before_cursor(screen: &vt100::Screen, token: &str) -> bool {
    let (row, column) = screen.cursor_position();
    let Ok(width) = u16::try_from(token.len()) else {
        return false;
    };
    if column < width {
        return false;
    }
    screen.contents_between(row, column - width, row, column) == token
}
fn quick_open(session: &mut PtySession, query: &str, expected_path: &str) -> Result<()> {
    session.send(CTRL_P)?;
    let prompt = session.mark();
    session.wait_contains("benchmark quick-open prompt", prompt, "Quick open:")?;
    let queried = session.paste_marked(query)?;
    session.wait_contains("benchmark quick-open path", queried, expected_path)?;
    session.send(ENTER)?;
    let opened = session.mark();
    session.wait_contains("benchmark quick-open target", opened, expected_path)?;
    Ok(())
}

fn begin_in_flight_search(session: &mut PtySession, query: &str) -> Result<()> {
    session.send(ALT_F)?;
    let prompt = session.mark();
    session.wait_contains("in-flight project-search prompt", prompt, "Project search:")?;
    let started = session.paste_marked(query)?;
    session.wait_contains(
        "project search entered Running state",
        started,
        &format!("Project search: {query}  searching…"),
    )?;
    Ok(())
}

fn quit_clean(
    session: &mut PtySession,
    baseline: &Termios,
    resources: &mut ResourceTracker,
    monitor: ResourceMonitor,
) -> Result<()> {
    session.send(CTRL_Q)?;
    let status = session.wait_exit()?;
    ensure!(status.success(), "benchmark Ctrl-Q failed: {status}");
    session.assert_restored_and_joined(baseline)?;
    resources.finish(monitor)
}
