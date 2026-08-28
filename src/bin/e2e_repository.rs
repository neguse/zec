mod e2e_support;

use std::{
    ffi::OsStr,
    fs,
    path::Path,
    process::{Command, Output},
    time::Instant,
};

use anyhow::{Context as _, Result, ensure};
use e2e_support::{
    ALT_F, AcceptanceReport, CTRL_A, CTRL_N, CTRL_P, CTRL_Q, CTRL_S, CTRL_W, END, ENTER, ESC,
    InputTrace, Invocation, PtySession, REPORT_SCHEMA_VERSION, TerminalBaseline, binary_report,
    command_output_with_timeout, environment_report, fixture, observe_poc_tests, open_fd_count,
    oracle_hashes, parse_invocation, reset_fixed_fixture, verify_acceptance_report, write_report,
};
#[cfg(unix)]
use nix::sys::signal::Signal;
use serde::{Deserialize, Serialize};
use unicode_width::UnicodeWidthStr;

fn main() -> Result<()> {
    match parse_invocation(true)? {
        Invocation::Verify(path) => {
            let report = e2e_support::read_report::<AcceptanceReport>(&path)?;
            verify_acceptance_report(&report)
                .with_context(|| format!("verify {}", path.display()))?;
            let repo = std::env::current_dir().context("resolve verifier repository cwd")?;
            let (observed, missing) = observe_poc_tests(&repo)?;
            ensure!(
                observed == report.poc_observed_count,
                "current PoC test count {observed} differs from report {}",
                report.poc_observed_count
            );
            ensure!(
                missing.is_empty(),
                "current checkout is missing/ignoring fixed PoC IDs: {missing:?}"
            );
            println!(
                "Repository acceptance report verified: {}/{} cases",
                report.passed, report.required_case_count
            );
            Ok(())
        }
        Invocation::Run(arguments) => run(arguments),
    }
}

fn run(arguments: e2e_support::RunArguments) -> Result<()> {
    let repo = fs::canonicalize(arguments.repo.as_ref().expect("parser requires repo"))
        .context("canonicalize --repo")?;
    fixture::verify_oracles(&repo).map_err(anyhow::Error::msg)?;
    let zec = fs::canonicalize(&arguments.zec).context("canonicalize --zec")?;
    let (poc_observed_count, poc_missing_ids) = observe_poc_tests(&repo)?;

    let mut runner = AcceptanceRunner {
        zec: &zec,
        cases: Vec::with_capacity(186),
    };
    runner.case("A1_ROOT_IDENTITY", root_identity);
    runner.case("A1_OUTSIDE_TRACE", outside_trace);
    runner.case("A1_PARTIAL_STARTUP", partial_startup);
    runner.case("A2_QUICK_OPEN", quick_open);
    runner.case("A2_PROJECT_SEARCH", project_search);
    runner.case("A2_STALE_RESULT", stale_result);

    for run in 1..=fixture::WORKFLOW_RUNS {
        runner.traced_case(&format!("A3_WORKFLOW_{run:02}"), |zec| workflow(zec, run));
    }
    for run in 1..=fixture::WORKFLOW_RUNS {
        runner.case(&format!("A5_OPEN_{run:02}"), open_failure);
    }
    for run in 1..=fixture::WORKFLOW_RUNS {
        runner.case(&format!("A5_SEARCH_{run:02}"), controlled_search_failure);
    }
    for run in 1..=fixture::WORKFLOW_RUNS {
        runner.case(&format!("A5_SAVE_{run:02}"), save_failure);
    }
    #[cfg(unix)]
    {
        for (name, signal) in [
            ("INT", Signal::SIGINT),
            ("QUIT", Signal::SIGQUIT),
            ("TERM", Signal::SIGTERM),
            ("HUP", Signal::SIGHUP),
        ] {
            for run in 1..=fixture::WORKFLOW_RUNS {
                runner.case(&format!("A5_{name}_{run:02}"), |zec| {
                    signal_exit(zec, signal)
                });
            }
        }
        for run in 1..=fixture::WORKFLOW_RUNS {
            runner.case(&format!("A5_TSTP_CONT_{run:02}"), suspend_resume);
        }
    }

    let required = fixture::platform_case_ids();
    ensure!(
        runner.cases.len() == required.len(),
        "internal case plan is incomplete"
    );
    let passed = runner.cases.iter().filter(|case| case.passed).count();
    let failed = runner.cases.len() - passed;
    let all_assertions_passed = failed == 0 && poc_missing_ids.is_empty();
    let report = AcceptanceReport {
        schema_version: REPORT_SCHEMA_VERSION,
        contract_version: fixture::CONTRACT_VERSION,
        report_kind: "e2e_repository".to_owned(),
        environment: environment_report()?,
        binary: binary_report(&zec)?,
        oracles: oracle_hashes(),
        poc_expected_count: 94,
        poc_observed_count,
        poc_missing_ids,
        required_case_count: required.len(),
        passed,
        failed,
        cases: runner.cases,
        all_assertions_passed,
    };
    write_report(&arguments.report, &report)?;
    println!(
        "Repository acceptance: {}/{} passed; report {}",
        report.passed,
        report.required_case_count,
        arguments.report.display()
    );
    if arguments.assert {
        verify_acceptance_report(&report)?;
    }
    Ok(())
}

struct AcceptanceRunner<'a> {
    zec: &'a Path,
    cases: Vec<e2e_support::CaseResult>,
}

impl AcceptanceRunner<'_> {
    fn case<F>(&mut self, id: &str, function: F)
    where
        F: FnOnce(&Path) -> Result<String>,
    {
        let started = Instant::now();
        let result = function(self.zec).map(|detail| (detail, None));
        self.record(id, started, result);
    }

    fn traced_case<F>(&mut self, id: &str, function: F)
    where
        F: FnOnce(&Path) -> Result<(String, InputTrace)>,
    {
        let started = Instant::now();
        let result = function(self.zec).map(|(detail, trace)| (detail, Some(trace)));
        self.record(id, started, result);
    }

    fn record(&mut self, id: &str, started: Instant, result: Result<(String, Option<InputTrace>)>) {
        let elapsed = started.elapsed();
        let duration_us = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        let result = if elapsed > e2e_support::SCENARIO_TIMEOUT {
            Err(anyhow::anyhow!(
                "PTY scenario exceeded {} seconds",
                e2e_support::SCENARIO_TIMEOUT.as_secs()
            ))
        } else {
            result
        };
        match result {
            Ok((detail, input_trace)) => {
                println!("PASS {id}: {detail}");
                self.cases.push(e2e_support::CaseResult {
                    id: id.to_owned(),
                    passed: true,
                    duration_us,
                    detail,
                    input_trace,
                });
            }
            Err(error) => {
                eprintln!("FAIL {id}: {error:#}");
                self.cases.push(e2e_support::CaseResult {
                    id: id.to_owned(),
                    passed: false,
                    duration_us,
                    detail: format!("{error:#}"),
                    input_trace: None,
                });
            }
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RootIdentityProbe {
    repository_root_count: usize,
    worktree_root_count: usize,
    directory_opened_as_file: bool,
    root_inputs: Vec<RootInputProbe>,
    aliases: Vec<AliasIdentity>,
    quick_open_excluded_results: Vec<QuickOpenExclusionProbe>,
    project_search_excluded_results: Vec<SearchSummary>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RootInputProbe {
    id: String,
    cwd: String,
    argument: Option<String>,
    resolved_path: String,
    repository_root: String,
    worktree_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AliasIdentity {
    path: String,
    buffer_id: String,
    tab_id: String,
}

#[derive(Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct SearchSummary {
    path: String,
    line: u32,
    column: u32,
    preview: String,
}

#[derive(Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct QuickOpenExclusionProbe {
    query: String,
    results: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RepositoryOracle {
    root_inputs: Vec<RootInputOracle>,
    identity: IdentityOracle,
    exclusions: ExclusionsOracle,
    quick_open: QuickOpenOracle,
    project_search: ProjectSearchOracle,
    outside_trace: OutsideTraceOracle,
    partial_startup: PartialStartupOracle,
}

#[derive(Debug, Deserialize, Serialize)]
struct RootInputOracle {
    id: String,
    cwd: String,
    argument: Option<String>,
}

#[derive(Debug, Deserialize)]
struct IdentityOracle {
    aliases: Vec<String>,
    expected_repository_root_count: usize,
    expected_worktree_root_count: usize,
    expected_buffer_id_count: usize,
    expected_tab_id_count: usize,
}

#[derive(Debug, Deserialize)]
struct ExclusionsOracle {
    quick_open_exclusion_queries: Vec<QuickOpenExclusionOracle>,
    project_search_expected_results: Vec<SearchSummary>,
}
#[derive(Debug, Deserialize)]
struct QuickOpenOracle {
    shortcut_bytes_hex: String,
    query: String,
    expected_selected_path: String,
    expected_open_path: String,
    alias_reopen_path: String,
    expected_tab_count_after_alias_reopen: usize,
}

#[derive(Debug, Deserialize)]
struct ProjectSearchOracle {
    queries: Vec<ProjectSearchQueryOracle>,
}

#[derive(Debug, Deserialize)]
struct ProjectSearchQueryOracle {
    id: String,
    text: String,
    expected_results: Vec<SearchSummary>,
    #[serde(default)]
    expected_total_hits: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct QuickOpenExclusionOracle {
    query: String,
    expected_results: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct OutsideTraceOracle {
    path: String,
    allowed_operations: Vec<String>,
    outside_parent_read_dir_count: usize,
    outside_sibling_read_dir_count: usize,
}

#[derive(Debug, Deserialize)]
struct PartialStartupOracle {
    normal_path: String,
    eloop_path: String,
    expected_error: String,
    editable_token: String,
    expected_exit_code: i32,
}

fn repository_oracle() -> Result<RepositoryOracle> {
    serde_json::from_slice(&fixture::spec_bytes()).context("parse compiled repository spec")
}

fn run_probe(zec: &Path, case: &str, path: &Path) -> Result<Output> {
    let mut command = Command::new(zec);
    command
        .args([OsStr::new("probe"), OsStr::new(case), path.as_os_str()])
        .env("LC_ALL", "C.UTF-8")
        .env("LANG", "C.UTF-8");
    command_output_with_timeout(&mut command, e2e_support::SCENARIO_TIMEOUT)
        .with_context(|| format!("run production {case} probe"))
}

fn root_identity(zec: &Path) -> Result<String> {
    let generated = reset_fixed_fixture()?;
    let oracle = repository_oracle()?;
    let root_inputs =
        serde_json::to_string(&oracle.root_inputs).context("serialize normative root inputs")?;
    let mut command = Command::new(zec);
    command
        .args([
            OsStr::new("probe"),
            OsStr::new("root-identity"),
            generated.root.as_os_str(),
            OsStr::new(&root_inputs),
        ])
        .env("LC_ALL", "C.UTF-8")
        .env("LANG", "C.UTF-8");
    let output = command_output_with_timeout(&mut command, e2e_support::SCENARIO_TIMEOUT)
        .context("run production root-identity probe")?;
    ensure!(
        output.status.success(),
        "root-identity probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let probe: RootIdentityProbe =
        serde_json::from_slice(&output.stdout).context("parse root-identity probe JSON")?;
    ensure!(
        !probe.directory_opened_as_file,
        "repository directory was opened as a file"
    );
    ensure!(
        probe.repository_root_count == oracle.identity.expected_repository_root_count,
        "repository root count differs"
    );
    ensure!(
        probe.worktree_root_count == oracle.identity.expected_worktree_root_count,
        "worktree root count differs"
    );
    ensure!(
        probe.root_inputs.len() == oracle.root_inputs.len()
            && probe
                .root_inputs
                .iter()
                .zip(&oracle.root_inputs)
                .all(|(actual, expected)| {
                    actual.id == expected.id
                        && actual.cwd == expected.cwd
                        && actual.argument == expected.argument
                }),
        "root input rows/order differ from spec"
    );
    let expected_resolved_root = generated.root.display().to_string();
    ensure!(
        probe
            .root_inputs
            .iter()
            .all(|input| input.resolved_path == expected_resolved_root
                && input.repository_root == expected_resolved_root),
        "a root input resolved to or reported a repository outside the fixed root"
    );
    let input_repository_roots = probe
        .root_inputs
        .iter()
        .map(|input| input.repository_root.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let input_worktree_ids = probe
        .root_inputs
        .iter()
        .map(|input| input.worktree_id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    ensure!(
        input_repository_roots.len() == oracle.identity.expected_repository_root_count
            && input_worktree_ids.len() == oracle.identity.expected_worktree_root_count
            && probe.repository_root_count == input_repository_roots.len()
            && probe.worktree_root_count == input_worktree_ids.len(),
        "root inputs did not converge to one repository/worktree identity"
    );
    ensure!(
        probe
            .aliases
            .iter()
            .map(|alias| alias.path.as_str())
            .eq(oracle.identity.aliases.iter().map(String::as_str)),
        "probe aliases differ from spec"
    );
    let buffer_ids = probe
        .aliases
        .iter()
        .map(|alias| &alias.buffer_id)
        .collect::<std::collections::BTreeSet<_>>();
    let tab_ids = probe
        .aliases
        .iter()
        .map(|alias| &alias.tab_id)
        .collect::<std::collections::BTreeSet<_>>();
    ensure!(
        buffer_ids.len() == oracle.identity.expected_buffer_id_count,
        "aliases produced the wrong buffer identity count"
    );
    ensure!(
        tab_ids.len() == oracle.identity.expected_tab_id_count,
        "aliases produced the wrong tab identity count"
    );
    ensure!(
        probe.quick_open_excluded_results.len()
            == oracle.exclusions.quick_open_exclusion_queries.len()
            && probe
                .quick_open_excluded_results
                .iter()
                .zip(&oracle.exclusions.quick_open_exclusion_queries)
                .all(|(actual, expected)| {
                    actual.query == expected.query && actual.results == expected.expected_results
                }),
        "quick-open exclusion query/results differ from spec"
    );
    ensure!(
        probe.project_search_excluded_results == oracle.exclusions.project_search_expected_results,
        "excluded sentinel project results differ"
    );
    Ok("one root/worktree/buffer/tab identity and exact exclusion oracle".to_owned())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OutsideTraceProbe {
    opened_path: String,
    outside_parent_read_dir_count: usize,
    outside_sibling_read_dir_count: usize,
    operations: Vec<String>,
}

fn outside_trace(zec: &Path) -> Result<String> {
    let generated = reset_fixed_fixture()?;
    let oracle = repository_oracle()?;
    let output = run_probe(zec, "outside-trace", &generated.outside_control)?;
    ensure!(
        output.status.success(),
        "outside-trace probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let probe: OutsideTraceProbe =
        serde_json::from_slice(&output.stdout).context("parse outside-trace probe JSON")?;
    ensure!(
        probe.opened_path == oracle.outside_trace.path,
        "outside probe opened a different path"
    );
    ensure!(
        probe.outside_parent_read_dir_count == oracle.outside_trace.outside_parent_read_dir_count
            && probe.outside_sibling_read_dir_count
                == oracle.outside_trace.outside_sibling_read_dir_count,
        "outside directory traversal was observed"
    );
    let actual_operations = probe
        .operations
        .iter()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    let expected_operations = oracle
        .outside_trace
        .allowed_operations
        .iter()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    ensure!(
        actual_operations == expected_operations
            && actual_operations.len() == probe.operations.len()
            && expected_operations.len() == oracle.outside_trace.allowed_operations.len(),
        "outside probe operation set differs from spec"
    );
    Ok("outside file used exactly the allowed filesystem operation set".to_owned())
}

fn partial_startup(zec: &Path) -> Result<String> {
    let generated = reset_fixed_fixture()?;
    let oracle = repository_oracle()?;
    let partial = &oracle.partial_startup;
    ensure!(
        partial.expected_error == "ELOOP",
        "partial-startup expected error must be ELOOP"
    );
    ensure!(
        partial.expected_exit_code == 0,
        "partial-startup expected exit code must be zero"
    );
    let config = e2e_support::fresh_config_dir("A1_PARTIAL_STARTUP")?;
    let normal = generated.root.join(&partial.normal_path);
    let loop_path = generated.root.join(&partial.eloop_path);
    let (mut session, baseline) = PtySession::spawn(
        zec,
        &generated.root,
        &[normal.as_os_str(), loop_path.as_os_str()],
        &config,
    )?;
    session.wait_ready("repo", fixture::READY_SENTINEL)?;
    let expected_error = format!("ELOOP opening {}", loop_path.display());
    ensure!(
        session.screen().contents().contains(&expected_error),
        "partial startup did not render exact error prefix {expected_error:?}"
    );
    session.send(CTRL_A)?;
    session.paste(&partial.editable_token)?;
    let mark = session.send_marked(CTRL_S)?;
    session.wait_contains("partial startup save", mark, "saved")?;
    ensure!(
        fs::read(&normal)? == partial.editable_token.as_bytes(),
        "partial startup edit did not reach disk"
    );
    clean_quit(&mut session, &baseline)?;
    Ok("ELOOP was isolated and the valid tab remained editable".to_owned())
}

fn quick_open(zec: &Path) -> Result<String> {
    let generated = reset_fixed_fixture()?;
    let oracle = repository_oracle()?;
    let quick = &oracle.quick_open;
    ensure!(
        quick.shortcut_bytes_hex == "10",
        "quick-open shortcut oracle differs from Ctrl-P"
    );
    let config = e2e_support::fresh_config_dir("A2_QUICK_OPEN")?;
    let (mut session, baseline) =
        PtySession::spawn(zec, &generated.root, &[generated.root.as_os_str()], &config)?;
    session.wait_ready("repo", fixture::READY_SENTINEL)?;
    open_exact_quick(
        &mut session,
        &quick.query,
        &quick.expected_selected_path,
        &quick.expected_open_path,
        "E2E_EDIT_A_OLD",
    )?;
    open_exact_quick(
        &mut session,
        &quick.alias_reopen_path,
        &quick.expected_selected_path,
        &quick.expected_open_path,
        "E2E_EDIT_A_OLD",
    )?;
    let displayed_tab_count = quick.expected_tab_count_after_alias_reopen + 1;
    let expected_name = Path::new(&quick.expected_open_path)
        .file_name()
        .context("quick-open expected path has no file name")?
        .to_string_lossy();
    let exact_tab_status =
        format!("{displayed_tab_count}/{displayed_tab_count} README.md [{expected_name}]");
    ensure!(
        session.screen().contents().contains(&exact_tab_status),
        "alias reopen increased the tab count or lost the README tab"
    );
    clean_quit(&mut session, &baseline)?;
    Ok("Ctrl-P selected canonical path and alias reopen preserved tab identity".to_owned())
}
fn open_exact_quick(
    session: &mut PtySession,
    query: &str,
    selected_path: &str,
    opened_path: &str,
    body: &str,
) -> Result<()> {
    let prompt = session.send_marked(CTRL_P)?;
    session.wait_contains("exact quick-open prompt", prompt, "Quick open:")?;
    let queried = session.paste_marked(query)?;
    let exact_result = format!("Quick open: {query}  1/1  {selected_path}");
    session.wait_contains("exact quick-open selected result", queried, &exact_result)?;
    let opened = session.send_marked(ENTER)?;
    session.wait_after(
        "exact quick-open target",
        opened,
        e2e_support::SCREEN_TIMEOUT,
        |screen| {
            let contents = screen.contents();
            contents.contains(opened_path)
                && contents.contains(body)
                && !contents.contains("Quick open:")
        },
    )?;
    Ok(())
}

fn project_query<'a>(
    oracle: &'a RepositoryOracle,
    id: &str,
) -> Result<&'a ProjectSearchQueryOracle> {
    oracle
        .project_search
        .queries
        .iter()
        .find(|query| query.id == id)
        .with_context(|| format!("repository project-search oracle is missing {id}"))
}

fn wait_project_query(session: &mut PtySession, query: &ProjectSearchQueryOracle) -> Result<()> {
    let prompt = session.send_marked(ALT_F)?;
    session.wait_contains("project-search prompt", prompt, "Project search:")?;
    let queried = session.paste_marked(&query.text)?;
    let total = query
        .expected_total_hits
        .unwrap_or(query.expected_results.len());
    let expected = match query.expected_results.first() {
        Some(result) => format!(
            "Project search: {}  1/{total}  {}:{}:{}  {}",
            query.text, result.path, result.line, result.column, result.preview
        ),
        None => format!("Project search: {}  0/{total}  no matches", query.text),
    };
    session.wait_contains(
        &format!("exact project-search result {}", query.id),
        queried,
        &expected,
    )?;
    Ok(())
}

fn close_project_query(session: &mut PtySession, id: &str) -> Result<()> {
    let closed = session.send_marked(ESC)?;
    session.wait_absent(
        &format!("close project-search query {id}"),
        closed,
        "Project search:",
    )?;
    Ok(())
}

fn open_project_query(session: &mut PtySession, query: &ProjectSearchQueryOracle) -> Result<()> {
    ensure!(
        query.expected_results.len() == 1,
        "{} must have exactly one result to open",
        query.id
    );
    wait_project_query(session, query)?;
    let expected = &query.expected_results[0];
    let terminal_column = terminal_cell_column(&expected.preview, expected.column)?;
    let opened_label = format!(
        "opened {}:{}:{}",
        expected.path, expected.line, expected.column
    );
    let opened = session.send_marked(ENTER)?;
    session.wait_after(
        &format!("open exact project-search result {}", query.id),
        opened,
        e2e_support::SCREEN_TIMEOUT,
        |screen| {
            let contents = screen.contents();
            let expected_row = u16::try_from(expected.line.saturating_sub(1)).ok();
            let cursor_matches = expected_row.is_some_and(|expected_row| {
                let (cursor_row, cursor_column) = screen.cursor_position();
                if cursor_row != expected_row {
                    return false;
                }
                let row = screen.contents_between(cursor_row, 0, cursor_row, screen.size().1);
                row.find(&expected.preview).is_some_and(|preview_byte| {
                    let preview_start = row[..preview_byte].width();
                    usize::from(cursor_column)
                        == preview_start.saturating_add(terminal_column.saturating_sub(1))
                })
            });
            contents.contains(&opened_label)
                && contents.contains(&expected.preview)
                && cursor_matches
                && !contents.contains("Project search:")
        },
    )?;
    Ok(())
}

fn terminal_cell_column(line: &str, scalar_column: u32) -> Result<usize> {
    ensure!(scalar_column > 0, "project-search columns are 1-based");
    let scalar_offset = usize::try_from(scalar_column.saturating_sub(1))
        .context("project-search scalar column does not fit usize")?;
    let prefix = line
        .char_indices()
        .nth(scalar_offset)
        .map_or(line, |(byte_offset, _)| &line[..byte_offset]);
    ensure!(
        scalar_offset <= line.chars().count(),
        "project-search scalar column {scalar_column} exceeds preview {line:?}"
    );
    Ok(prefix.width().saturating_add(1))
}

fn project_search(zec: &Path) -> Result<String> {
    let generated = reset_fixed_fixture()?;
    let oracle = repository_oracle()?;
    let config = e2e_support::fresh_config_dir("A2_PROJECT_SEARCH")?;
    let (mut session, baseline) =
        PtySession::spawn(zec, &generated.root, &[generated.root.as_os_str()], &config)?;
    session.wait_ready("repo", fixture::READY_SENTINEL)?;

    for id in ["case_variant", "nfc_no_normalization"] {
        wait_project_query(&mut session, project_query(&oracle, id)?)?;
        close_project_query(&mut session, id)?;
    }

    for id in ["nfd_scalar_column", "bom_scalar_column", "excluded"] {
        open_project_query(&mut session, project_query(&oracle, id)?)?;
    }

    let stale_a = project_query(&oracle, "stale_a")?;
    let prompt = session.send_marked(ALT_F)?;
    session.wait_contains("stale-A project-search prompt", prompt, "Project search:")?;
    let started_a = session.paste_marked(&stale_a.text)?;
    session.wait_contains(
        "stale-A production search entered Running state",
        started_a,
        &format!("Project search: {}  searching…", stale_a.text),
    )?;
    let cancelled = session.send_marked(ESC)?;
    session.wait_after(
        "cancel in-flight stale-A search",
        cancelled,
        e2e_support::SCREEN_TIMEOUT,
        |screen| {
            let contents = screen.contents();
            !contents.contains("Project search:")
                && !contents.contains(&stale_a.expected_results[0].path)
        },
    )?;

    let stale_b = project_query(&oracle, "stale_b")?;
    let prompt = session.send_marked(ALT_F)?;
    session.wait_contains("stale-B project-search prompt", prompt, "Project search:")?;
    let current = session.paste_marked(&stale_b.text)?;
    let expected_b = &stale_b.expected_results[0];
    let stale_b_status = format!(
        "Project search: {}  1/1  {}:{}:{}  {}",
        stale_b.text, expected_b.path, expected_b.line, expected_b.column, expected_b.preview
    );
    session.wait_after(
        "new project query only",
        current,
        e2e_support::SCREEN_TIMEOUT,
        |screen| {
            let contents = screen.contents();
            contents.contains(&stale_b_status)
                && !contents.contains(&stale_a.expected_results[0].path)
        },
    )?;
    close_project_query(&mut session, "stale_b")?;

    let quit_query = project_query(&oracle, "benchmark")?;
    let prompt = session.send_marked(ALT_F)?;
    session.wait_contains("quit project-search prompt", prompt, "Project search:")?;
    let started_quit = session.paste_marked(&quit_query.text)?;
    session.wait_contains(
        "quit production search entered Running state",
        started_quit,
        &format!("Project search: {}  searching…", quit_query.text),
    )?;
    session.send(CTRL_Q)?;
    let status = session.wait_exit()?;
    ensure!(
        status.success(),
        "Ctrl-Q during project search failed: {status}"
    );
    session.assert_restored_and_joined(&baseline)?;
    Ok(
        "exact case-sensitive/non-normalizing/BOM scalar coordinates, selection and stale-screen suppression passed"
            .to_owned(),
    )
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StaleResultProbe {
    publish_log: Vec<String>,
    final_query: String,
    final_path: String,
}

fn stale_result(zec: &Path) -> Result<String> {
    let generated = reset_fixed_fixture()?;
    let output = run_probe(zec, "stale-result", &generated.root)?;
    ensure!(
        output.status.success(),
        "stale-result probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let probe: StaleResultProbe =
        serde_json::from_slice(&output.stdout).context("parse stale-result probe JSON")?;
    ensure!(probe.publish_log == ["B"], "stale completion was published");
    ensure!(probe.final_query == "E2E_STALE_B", "final query is not B");
    ensure!(probe.final_path == "src/stale-b.txt", "final path is not B");
    Ok("production command/reducer published B once and discarded A".to_owned())
}

fn workflow(zec: &Path, run: u8) -> Result<(String, InputTrace)> {
    let generated = reset_fixed_fixture()?;
    let config = e2e_support::fresh_config_dir(&format!("A3_WORKFLOW_{run:02}"))?;
    let (mut session, baseline) =
        PtySession::spawn(zec, &generated.root, &[generated.root.as_os_str()], &config)?;
    session.wait_ready("repo", fixture::READY_SENTINEL)?;
    let expected_ids = (1..=4)
        .map(|sequence| format!("A3_{run:02}_{sequence:04}"))
        .collect::<Vec<_>>();
    let mut sent_ids = Vec::with_capacity(expected_ids.len());
    let mut applied_ids = Vec::with_capacity(expected_ids.len());
    let mut file_ids = Vec::with_capacity(expected_ids.len());

    open_quick(
        &mut session,
        "日本 語.rs",
        fixture::EDIT_A_PATH,
        "E2E_EDIT_A_OLD",
        None,
    )?;
    select_current_match(&mut session, "E2E_EDIT_A_OLD")?;
    let input_id_a = expected_ids[0].clone();
    let replacement_a = format!("E2E_EDIT_A_{run:02} {input_id_a}");
    let replaced_a = session.paste_marked(&replacement_a)?;
    sent_ids.push(input_id_a.clone());
    session.wait_contains("buffer A input ID", replaced_a, &input_id_a)?;
    applied_ids.push(input_id_a);
    save(&mut session)?;

    open_project(
        &mut session,
        "E2E_FIND_B_OLD",
        fixture::EDIT_B_PATH,
        "E2E_FIND_B_OLD",
        None,
    )?;
    select_current_match(&mut session, "E2E_FIND_B_OLD")?;
    let input_id_b = expected_ids[1].clone();
    let replacement_b = format!("E2E_EDIT_B_{run:02} {input_id_b}");
    let replaced_b = session.paste_marked(&replacement_b)?;
    sent_ids.push(input_id_b.clone());
    session.wait_contains("buffer B input ID", replaced_b, &input_id_b)?;
    applied_ids.push(input_id_b);
    save(&mut session)?;

    open_quick(
        &mut session,
        "no-final-newline.txt",
        fixture::EDIT_C_PATH,
        "E2E_EDIT_C_OLD",
        None,
    )?;
    session.send(END)?;
    let input_id_c = expected_ids[2].clone();
    let appended = session.paste_marked(&format!(" :: E2E_EDIT_C_{run:02} {input_id_c}"))?;
    sent_ids.push(input_id_c.clone());
    session.wait_contains("buffer C input ID", appended, &input_id_c)?;
    applied_ids.push(input_id_c);
    save(&mut session)?;

    session.send(CTRL_N)?;
    let input_id_d = expected_ids[3].clone();
    let scratch = format!("zec scratch 日本語\nworkflow token E2E_EDIT_D_{run:02} {input_id_d}\n");
    let inserted_d = session.paste_marked(&scratch)?;
    sent_ids.push(input_id_d.clone());
    session.wait_contains("scratch D input ID", inserted_d, &input_id_d)?;
    applied_ids.push(input_id_d);
    let prompt = session.send_marked(CTRL_S)?;
    session.wait_contains("workflow Save As prompt", prompt, "Save as:")?;
    session.paste(fixture::EDIT_D_PATH)?;
    let saved = session.send_marked(ENTER)?;
    session.wait_after(
        "workflow Save As completion and clean four-tab status",
        saved,
        e2e_support::SCREEN_TIMEOUT,
        |screen| {
            let contents = screen.contents();
            contents.contains("saved")
                && [
                    "日本 語.rs",
                    "crlf-edit.rs",
                    "no-final-newline.txt",
                    "新規 メモ.txt",
                ]
                .iter()
                .all(|name| {
                    !contents.contains(&format!("{name}+"))
                        && !contents.contains(&format!("{name}!"))
                })
        },
    )?;
    clean_quit(&mut session, &baseline)?;

    fixture::verify_fixture(&generated.root, Some(run)).map_err(anyhow::Error::msg)?;
    for (index, path) in [
        fixture::EDIT_A_PATH,
        fixture::EDIT_B_PATH,
        fixture::EDIT_C_PATH,
        fixture::EDIT_D_PATH,
    ]
    .into_iter()
    .enumerate()
    {
        let actual = fs::read(generated.root.join(path))?;
        let expected = fixture::expected_workflow_file(path, run).map_err(anyhow::Error::msg)?;
        ensure!(
            actual == expected,
            "workflow file oracle differs for {path}"
        );
        let expected_id = &expected_ids[index];
        let position = actual
            .windows(expected_id.len())
            .position(|window| window == expected_id.as_bytes())
            .with_context(|| format!("input ID is absent from exact file oracle for {path}"))?;
        let observed_id = std::str::from_utf8(&actual[position..position + expected_id.len()])
            .context("workflow input ID is not UTF-8")?;
        file_ids.push(observed_id.to_owned());
    }

    let reopen_config = e2e_support::fresh_config_dir(&format!("A3_WORKFLOW_{run:02}_REOPEN"))?;
    let (mut reopen, reopen_baseline) = PtySession::spawn(
        zec,
        &generated.root,
        &[generated.root.as_os_str()],
        &reopen_config,
    )?;
    reopen.wait_ready("repo", fixture::READY_SENTINEL)?;
    for (path, token, expected_cursor) in [
        (fixture::EDIT_A_PATH, format!("E2E_EDIT_A_{run:02}"), (0, 2)),
        (fixture::EDIT_C_PATH, format!("E2E_EDIT_C_{run:02}"), (0, 2)),
        (fixture::EDIT_D_PATH, format!("E2E_EDIT_D_{run:02}"), (0, 2)),
    ] {
        open_quick(&mut reopen, path, path, &token, Some(expected_cursor))?;
        ensure!(
            reopen.screen().cursor_position() == expected_cursor,
            "reopen VT caret differs for {path}: expected {expected_cursor:?}, got {:?}",
            reopen.screen().cursor_position()
        );
        ensure!(
            fs::read(generated.root.join(path))?
                == fixture::expected_workflow_file(path, run).map_err(anyhow::Error::msg)?,
            "reopened workflow bytes differ for {path}"
        );
    }
    let b_token = format!("E2E_EDIT_B_{run:02}");
    open_project(
        &mut reopen,
        &b_token,
        fixture::EDIT_B_PATH,
        &b_token,
        Some((1, 27)),
    )?;
    ensure!(
        reopen.screen().cursor_position() == (1, 27),
        "project-search reopen VT caret differs for {}: expected (1, 27), got {:?}",
        fixture::EDIT_B_PATH,
        reopen.screen().cursor_position()
    );
    ensure!(
        fs::read(generated.root.join(fixture::EDIT_B_PATH))?
            == fixture::expected_workflow_file(fixture::EDIT_B_PATH, run)
                .map_err(anyhow::Error::msg)?,
        "reopened workflow bytes differ for {}",
        fixture::EDIT_B_PATH
    );
    clean_quit(&mut reopen, &reopen_baseline)?;
    let dropped_count = expected_ids.len().saturating_sub(applied_ids.len());
    let reordered = sent_ids != applied_ids || applied_ids != file_ids;
    let trace = InputTrace {
        sent_input_ids: sent_ids,
        applied_input_ids: applied_ids,
        expected_input_ids: file_ids,
        dropped_count,
        reordered,
    };
    Ok((
        format!("exact four-file workflow and fresh-process reopen {run:02}"),
        trace,
    ))
}

fn open_failure(zec: &Path) -> Result<String> {
    let fd_before = open_fd_count()?;
    let detail = partial_startup(zec)?;
    ensure!(
        open_fd_count()? == fd_before,
        "harness file descriptor leak"
    );
    Ok(detail)
}

#[derive(Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct SearchFailureTrace {
    tab_count: usize,
    body_sha256: String,
    dirty: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchFailureProbe {
    error: String,
    before: SearchFailureTrace,
    after: SearchFailureTrace,
    continued_edit_saved: bool,
    control_disk_token: String,
}

fn controlled_search_failure(zec: &Path) -> Result<String> {
    let fd_before = open_fd_count()?;
    let generated = reset_fixed_fixture()?;
    let output = run_probe(zec, "search-failure", &generated.root)?;
    ensure!(
        output.status.success(),
        "search-failure probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let probe: SearchFailureProbe =
        serde_json::from_slice(&output.stdout).context("parse search-failure probe JSON")?;
    ensure!(probe.error == "EIO", "controlled provider error differs");
    ensure!(
        probe.before == probe.after,
        "search EIO changed tab/body/dirty trace"
    );
    ensure!(
        probe.continued_edit_saved,
        "control tab was not editable/saveable after EIO"
    );
    ensure!(
        probe.control_disk_token == "E2E_SEARCH_FAILURE_CONTINUED",
        "continued edit disk token differs"
    );
    ensure!(
        open_fd_count()? == fd_before,
        "harness file descriptor leak"
    );
    Ok("production EIO preserved state and control editing continued".to_owned())
}

fn save_failure(zec: &Path) -> Result<String> {
    let fd_before = open_fd_count()?;
    let generated = reset_fixed_fixture()?;
    let failed_relative = "src/control.txt/child.txt";
    let failed_path = generated.root.join(failed_relative);
    ensure!(
        fs::metadata(generated.root.join("src/control.txt"))?.is_file(),
        "ENOTDIR parent is not a regular file"
    );
    let target_error = fs::write(&failed_path, b"must not be written")
        .err()
        .context("ENOTDIR target was unexpectedly writable")?;
    // Unix reports ENOTDIR for a path whose parent is a regular file;
    // Windows reports ERROR_PATH_NOT_FOUND or ERROR_DIRECTORY.
    #[cfg(unix)]
    let parent_is_file_errors = [nix::libc::ENOTDIR];
    #[cfg(windows)]
    let parent_is_file_errors = [3, 267];
    ensure!(
        target_error
            .raw_os_error()
            .is_some_and(|code| parent_is_file_errors.contains(&code)),
        "fixed save-failure target does not report a file parent: {target_error}"
    );
    let config = e2e_support::fresh_config_dir("A5_SAVE")?;
    let (mut session, baseline) =
        PtySession::spawn(zec, &generated.root, &[generated.root.as_os_str()], &config)?;
    session.wait_ready("repo", fixture::READY_SENTINEL)?;
    session.send(CTRL_N)?;
    session.paste("E2E_UNSAVED_ENOTDIR")?;
    let prompt = session.send_marked(CTRL_S)?;
    session.wait_contains("ENOTDIR Save As prompt", prompt, "Save as:")?;
    session.paste(failed_relative)?;
    let failed = session.send_marked(ENTER)?;
    session.wait_after(
        "ENOTDIR save failure",
        failed,
        e2e_support::SCREEN_TIMEOUT,
        |screen| {
            let contents = screen.contents();
            contents.contains("save failed:")
                && contents.contains("E2E_UNSAVED_ENOTDIR")
                && contents.contains(failed_relative)
        },
    )?;
    ensure!(
        !failed_path.exists(),
        "ENOTDIR path was unexpectedly created"
    );
    let guarded = session.send_marked(CTRL_Q)?;
    session.wait_contains("dirty guard after ENOTDIR", guarded, "unsaved or deleted")?;
    let close_guard = session.send_marked(CTRL_W)?;
    session.wait_contains(
        "dirty close guard after ENOTDIR",
        close_guard,
        "press Ctrl-W again to discard this tab",
    )?;
    let back = session.send_marked(CTRL_W)?;
    session.wait_contains(
        "control tab after failed save",
        back,
        fixture::READY_SENTINEL,
    )?;
    session.send(CTRL_A)?;
    session.paste(fixture::READY_SENTINEL)?;
    save(&mut session)?;
    ensure!(
        fs::read(generated.root.join(fixture::READY_PATH))? == fixture::READY_SENTINEL.as_bytes(),
        "control tab bytes were not saved after ENOTDIR"
    );
    clean_quit(&mut session, &baseline)?;
    ensure!(
        open_fd_count()? == fd_before,
        "harness file descriptor leak"
    );
    Ok("ENOTDIR preserved dirty text and control tab remained saveable".to_owned())
}

#[cfg(unix)]
fn signal_exit(zec: &Path, signal: Signal) -> Result<String> {
    let generated = reset_fixed_fixture()?;
    let config = e2e_support::fresh_config_dir(&format!("signal-{}", signal as i32))?;
    let fd_before = open_fd_count()?;
    {
        let (mut session, baseline) =
            PtySession::spawn(zec, &generated.root, &[generated.root.as_os_str()], &config)?;
        session.wait_ready("repo", fixture::READY_SENTINEL)?;
        session.assert_raw(&baseline)?;
        e2e_support::wait_for_no_descendant_processes(session.pid()?)?;
        session.send_signal(signal)?;
        let status = session.wait_exit()?;
        ensure!(status.success(), "{signal:?} exit was not normal: {status}");
        session.assert_restored_and_joined(&baseline)?;
    }
    ensure!(
        open_fd_count()? == fd_before,
        "harness file descriptor leak"
    );
    Ok(format!(
        "{signal:?} cleanly restored and reaped the foreground group"
    ))
}

#[cfg(unix)]
fn suspend_resume(zec: &Path) -> Result<String> {
    let generated = reset_fixed_fixture()?;
    let config = e2e_support::fresh_config_dir("A5_TSTP_CONT")?;
    let fd_before = open_fd_count()?;
    {
        let (mut session, baseline) =
            PtySession::spawn(zec, &generated.root, &[generated.root.as_os_str()], &config)?;
        session.wait_ready("repo", fixture::READY_SENTINEL)?;
        session.send_signal(Signal::SIGTSTP)?;
        session.wait_stopped()?;
        session.assert_restored(&baseline)?;
        let resumed = session.send_signal_marked(Signal::SIGCONT)?;
        session.wait_after(
            "Ready after SIGCONT",
            resumed,
            e2e_support::SCREEN_TIMEOUT,
            |screen| {
                screen.alternate_screen()
                    && screen.contents().contains(fixture::READY_SENTINEL)
                    && screen.contents().contains("repo")
            },
        )?;
        session.assert_raw(&baseline)?;
        session.send(CTRL_A)?;
        session.paste("E2E_TSTP_CONT_EDIT")?;
        save(&mut session)?;
        ensure!(
            fs::read(generated.root.join(fixture::READY_PATH))? == b"E2E_TSTP_CONT_EDIT",
            "post-SIGCONT edit did not reach disk"
        );
        clean_quit(&mut session, &baseline)?;
    }
    ensure!(
        open_fd_count()? == fd_before,
        "harness file descriptor leak"
    );
    Ok("SIGTSTP restored, SIGCONT re-entered, edited, saved and exited".to_owned())
}

fn opened_editor_frame_matches(
    contents: &str,
    cursor: (u16, u16),
    cursor_visible: bool,
    path: &str,
    body: &str,
    prompt: &str,
    expected_cursor: Option<(u16, u16)>,
) -> bool {
    cursor_visible
        && contents.contains(path)
        && contents.contains(body)
        && !contents.contains(prompt)
        && expected_cursor.is_none_or(|expected| cursor == expected)
}

fn open_quick(
    session: &mut PtySession,
    query: &str,
    path: &str,
    body: &str,
    expected_cursor: Option<(u16, u16)>,
) -> Result<()> {
    let prompt = session.send_marked(CTRL_P)?;
    session.wait_contains("quick-open prompt", prompt, "Quick open:")?;
    let queried = session.paste_marked(query)?;
    session.wait_contains("quick-open selected path", queried, path)?;
    let opened = session.send_marked(ENTER)?;
    session.wait_after(
        "quick-open target editor frame",
        opened,
        e2e_support::SCREEN_TIMEOUT,
        |screen| {
            let contents = screen.contents();
            opened_editor_frame_matches(
                &contents,
                screen.cursor_position(),
                !screen.hide_cursor(),
                path,
                body,
                "Quick open:",
                expected_cursor,
            )
        },
    )?;
    Ok(())
}

fn open_project(
    session: &mut PtySession,
    query: &str,
    path: &str,
    body: &str,
    expected_cursor: Option<(u16, u16)>,
) -> Result<()> {
    let prompt = session.send_marked(ALT_F)?;
    session.wait_contains("project-search prompt", prompt, "Project search:")?;
    let queried = session.paste_marked(query)?;
    session.wait_contains("project-search selected path", queried, path)?;
    let opened = session.send_marked(ENTER)?;
    session.wait_after(
        "project-search target editor frame",
        opened,
        e2e_support::SCREEN_TIMEOUT,
        |screen| {
            let contents = screen.contents();
            opened_editor_frame_matches(
                &contents,
                screen.cursor_position(),
                !screen.hide_cursor(),
                path,
                body,
                "Project search:",
                expected_cursor,
            )
        },
    )?;
    Ok(())
}

fn select_current_match(session: &mut PtySession, search: &str) -> Result<()> {
    let prompt = session.send_marked(b"\x06")?;
    session.wait_contains("buffer find prompt", prompt, "Find:")?;
    let queried = session.paste_marked(search)?;
    session.wait_contains("buffer find match", queried, "1/1")?;
    let closed = session.send_marked(ESC)?;
    session.wait_absent("close buffer find", closed, "Find:")?;
    Ok(())
}

fn save(session: &mut PtySession) -> Result<()> {
    let mark = session.send_marked(CTRL_S)?;
    session.wait_contains("save completion", mark, "saved")?;
    Ok(())
}

fn clean_quit(session: &mut PtySession, baseline: &TerminalBaseline) -> Result<()> {
    session.send(CTRL_Q)?;
    let status = session.wait_exit()?;
    ensure!(status.success(), "Ctrl-Q exit failed: {status}");
    session.assert_restored_and_joined(baseline)
}

#[cfg(test)]
mod tests {
    use super::{opened_editor_frame_matches, terminal_cell_column};

    #[test]
    fn opened_editor_frame_rejects_partial_prompt_cursor_and_accepts_exact_caret() {
        let path = "src/reopened.txt";
        let body = "E2E_REOPENED_BODY";
        let partial = format!("{path}\n{body}\nQuick open: reopened");
        assert!(!opened_editor_frame_matches(
            &partial,
            (39, 120),
            true,
            path,
            body,
            "Quick open:",
            Some((0, 2)),
        ));
        assert!(!opened_editor_frame_matches(
            &format!("{path}\n{body}"),
            (39, 32),
            true,
            path,
            body,
            "Project search:",
            Some((1, 27)),
        ));
        assert!(opened_editor_frame_matches(
            &format!("{path}\n{body}"),
            (0, 2),
            true,
            path,
            body,
            "Quick open:",
            Some((0, 2)),
        ));
        assert!(opened_editor_frame_matches(
            &format!("{path}\n{body}"),
            (1, 27),
            true,
            path,
            body,
            "Project search:",
            Some((1, 27)),
        ));
        assert!(!opened_editor_frame_matches(
            &format!("{path}\n{body}"),
            (1, 27),
            false,
            path,
            body,
            "Project search:",
            Some((1, 27)),
        ));
    }

    #[test]
    fn converts_scalar_columns_to_terminal_cell_columns() {
        assert_eq!(
            terminal_cell_column("Unicode path fixture: 日本語 e\u{301}", 27).unwrap(),
            30
        );
        assert_eq!(terminal_cell_column("// UTF-8 BOM fixture", 4).unwrap(), 4);
        assert_eq!(terminal_cell_column("control", 1).unwrap(), 1);
        assert!(terminal_cell_column("control", 0).is_err());
        assert!(terminal_cell_column("control", 9).is_err());
    }
}
