#![allow(dead_code)]

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fs,
    io::{self, Read as _, Write as _},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail, ensure};
use nix::{
    sys::{
        signal::{Signal, killpg},
        termios::{LocalFlags, Termios},
        wait::{WaitPidFlag, WaitStatus, waitpid},
    },
    unistd::Pid,
};
use portable_pty::{
    Child, CommandBuilder, ExitStatus, MasterPty, PtyPair, PtySize, native_pty_system,
};
use serde::{Deserialize, Serialize};
use vt100::{MouseProtocolEncoding, MouseProtocolMode, Parser};

#[path = "../../../tests/alpha_1/fixture.rs"]
pub mod fixture;

pub const REPORT_SCHEMA_VERSION: u32 = 1;
pub const COLS: u16 = 120;
pub const ROWS: u16 = 40;
pub const SCREEN_TIMEOUT: Duration = Duration::from_secs(15);
pub const STARTUP_TIMEOUT: Duration = SCREEN_TIMEOUT;
pub const CHILD_TIMEOUT: Duration = Duration::from_secs(5);
pub const SCENARIO_TIMEOUT: Duration = Duration::from_secs(120);
const EVENT_POLL: Duration = Duration::from_millis(25);
const TRANSCRIPT_LIMIT: usize = 256 * 1024;
const DIAGNOSTIC_TAIL: usize = 8 * 1024;
const EXPECTED_POC_IDS: &str = include_str!("../../../tests/alpha_1/poc-test-ids-v1.txt");

pub const CTRL_A: &[u8] = b"\x01";
pub const CTRL_G: &[u8] = b"\x07";
pub const CTRL_N: &[u8] = b"\x0e";
pub const CTRL_P: &[u8] = b"\x10";
pub const CTRL_Q: &[u8] = b"\x11";
pub const CTRL_S: &[u8] = b"\x13";
pub const CTRL_W: &[u8] = b"\x17";
pub const CTRL_Z: &[u8] = b"\x1a";
pub const ALT_F: &[u8] = b"\x1bf";
pub const ESC: &[u8] = b"\x1b";
pub const ENTER: &[u8] = b"\r";
pub const TAB: &[u8] = b"\t";
pub const END: &[u8] = b"\x1b[F";
pub const DELETE: &[u8] = b"\x1b[3~";
pub const DOWN: &[u8] = b"\x1b[B";

const CLEANUP_ESCAPES: &[u8] = concat!(
    "\x1b[?25h",
    "\x1b[?1006l",
    "\x1b[?1015l",
    "\x1b[?1003l",
    "\x1b[?1002l",
    "\x1b[?1000l",
    "\x1b[?2004l",
    "\x1b[?1049l",
)
.as_bytes();

#[derive(Clone, Debug)]
pub struct RunArguments {
    pub zec: PathBuf,
    pub repo: Option<PathBuf>,
    pub assert: bool,
    pub report: PathBuf,
}

#[derive(Clone, Debug)]
pub enum Invocation {
    Run(RunArguments),
    Verify(PathBuf),
}

pub fn parse_invocation(requires_repo: bool) -> Result<Invocation> {
    let mut arguments = std::env::args_os().skip(1);
    let mut zec = None;
    let mut repo = None;
    let mut report = None;
    let mut verify = None;
    let mut assert = false;
    while let Some(argument) = arguments.next() {
        match argument.to_str() {
            Some("--zec") => zec = Some(value(&mut arguments, "--zec")?.into()),
            Some("--repo") => repo = Some(value(&mut arguments, "--repo")?.into()),
            Some("--report") => report = Some(value(&mut arguments, "--report")?.into()),
            Some("--verify-report") => {
                verify = Some(value(&mut arguments, "--verify-report")?.into())
            }
            Some("--assert") => assert = true,
            Some("--help" | "-h") => {
                println!(
                    "Usage: {} --zec PATH {}--assert --report PATH\n       {} --verify-report PATH",
                    std::env::args()
                        .next()
                        .unwrap_or_else(|| "alpha_1".to_owned()),
                    if requires_repo { "--repo PATH " } else { "" },
                    std::env::args()
                        .next()
                        .unwrap_or_else(|| "alpha_1".to_owned()),
                );
                std::process::exit(0);
            }
            _ => bail!("unknown or non-UTF-8 argument: {:?}", argument),
        }
    }
    if let Some(path) = verify {
        ensure!(
            zec.is_none() && repo.is_none() && report.is_none() && !assert,
            "--verify-report cannot be combined with run arguments"
        );
        return Ok(Invocation::Verify(path));
    }
    let zec = zec.context("--zec PATH is required")?;
    let report = report.context("--report PATH is required")?;
    if requires_repo {
        ensure!(repo.is_some(), "--repo PATH is required");
    } else {
        ensure!(repo.is_none(), "--repo is not accepted by this binary");
    }
    Ok(Invocation::Run(RunArguments {
        zec,
        repo,
        assert,
        report,
    }))
}

fn value(arguments: &mut impl Iterator<Item = OsString>, flag: &str) -> Result<OsString> {
    arguments
        .next()
        .with_context(|| format!("{flag} requires a value"))
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentReport {
    pub runner_class: String,
    pub cpu_model: String,
    pub cpu_core_count: u64,
    pub ram_bytes: u64,
    pub kernel: String,
    pub runner_image_version: String,
    pub architecture: String,
    pub locale: String,
    pub term: String,
    pub columns: u16,
    pub rows: u16,
    pub pty_parser: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OracleHashes {
    pub generator_source_sha256: String,
    pub spec_sha256: String,
    pub before_manifest_sha256: String,
    pub after_manifest_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BinaryReport {
    pub path: String,
    pub content_sha256: String,
    pub release_profile: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CaseResult {
    pub id: String,
    pub passed: bool,
    pub duration_us: u64,
    pub detail: String,
    pub input_trace: Option<InputTrace>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceReport {
    pub schema_version: u32,
    pub contract_version: u32,
    pub report_kind: String,
    pub environment: EnvironmentReport,
    pub binary: BinaryReport,
    pub oracles: OracleHashes,
    pub poc_expected_count: usize,
    pub poc_observed_count: usize,
    pub poc_missing_ids: Vec<String>,
    pub required_case_count: usize,
    pub passed: usize,
    pub failed: usize,
    pub cases: Vec<CaseResult>,
    pub all_assertions_passed: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MetricReport {
    pub warmups: usize,
    pub expected_samples: usize,
    pub raw_samples_us: Vec<u64>,
    pub p50_us: u64,
    pub p95_us: u64,
    pub max_us: u64,
    pub p95_limit_us: Option<u64>,
    pub max_limit_us: Option<u64>,
    pub assertion_passed: bool,
}

impl MetricReport {
    pub fn new(
        warmups: usize,
        expected_samples: usize,
        raw_samples_us: Vec<u64>,
        p95_limit_us: Option<u64>,
        max_limit_us: Option<u64>,
    ) -> Self {
        let (p50_us, p95_us, max_us) = statistics(&raw_samples_us);
        let assertion_passed = raw_samples_us.len() == expected_samples
            && p95_limit_us.is_none_or(|limit| p95_us <= limit)
            && max_limit_us.is_none_or(|limit| max_us <= limit);
        Self {
            warmups,
            expected_samples,
            raw_samples_us,
            p50_us,
            p95_us,
            max_us,
            p95_limit_us,
            max_limit_us,
            assertion_passed,
        }
    }

    fn verify(&self, label: &str) -> Result<()> {
        ensure!(
            self.raw_samples_us.len() == self.expected_samples,
            "{label}: expected {} raw samples, found {}",
            self.expected_samples,
            self.raw_samples_us.len()
        );
        ensure!(!self.raw_samples_us.is_empty(), "{label}: no samples");
        let (p50, p95, max) = statistics(&self.raw_samples_us);
        ensure!(
            (self.p50_us, self.p95_us, self.max_us) == (p50, p95, max),
            "{label}: stored statistics do not match raw samples"
        );
        let passed = self.p95_limit_us.is_none_or(|limit| self.p95_us <= limit)
            && self.max_limit_us.is_none_or(|limit| self.max_us <= limit);
        ensure!(
            self.assertion_passed == passed,
            "{label}: assertion_passed is inconsistent"
        );
        ensure!(passed, "{label}: latency limit exceeded");
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InputTrace {
    pub sent_input_ids: Vec<String>,
    pub applied_input_ids: Vec<String>,
    pub expected_input_ids: Vec<String>,
    pub dropped_count: usize,
    pub reordered: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SearchResultReport {
    pub path: String,
    pub line: u32,
    pub column: u32,
    pub preview: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct QuickOpenQuery {
    pub query: String,
    pub expected_selected_path: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkOracle {
    pub report_schema_version: u32,
    pub startup: P95Oracle,
    pub quick_open: QuickOpenBenchmarkOracle,
    pub project_search: ProjectSearchBenchmarkOracle,
    pub in_flight_search: InFlightBenchmarkOracle,
    pub editing: EditingBenchmarkOracle,
    pub save: SaveBenchmarkOracle,
    pub vm_hwm_max_bytes: u64,
    pub descendant_process_count: usize,
    pub nearest_rank_p95: String,
    pub input_invariants: InputInvariantOracle,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct P95Oracle {
    pub warmups: usize,
    pub samples: usize,
    pub p95_max_us: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct QuickOpenBenchmarkOracle {
    pub warmups: usize,
    pub samples: usize,
    pub p95_max_us: u64,
    pub max_us: u64,
    pub queries: Vec<QuickOpenQuery>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProjectSearchBenchmarkOracle {
    pub warmups: usize,
    pub samples: usize,
    pub p95_max_us: u64,
    pub expected_hits: usize,
    pub visible_results: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct InFlightBenchmarkOracle {
    pub attempts_each: usize,
    pub max_us: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EditingBenchmarkOracle {
    pub path: String,
    pub warmups: usize,
    pub samples: usize,
    pub p95_max_us: u64,
    pub max_us: u64,
    pub input_id_template: String,
    pub payload_suffix: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SaveBenchmarkOracle {
    pub path: String,
    pub size: u64,
    pub warmups: usize,
    pub samples: usize,
    pub max_us: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct InputInvariantOracle {
    pub sent_equals_applied_equals_expected: bool,
    pub dropped_count: usize,
    pub reordered: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkReport {
    pub schema_version: u32,
    pub contract_version: u32,
    pub report_kind: String,
    pub environment: EnvironmentReport,
    pub binary: BinaryReport,
    pub oracles: OracleHashes,
    pub startup: MetricReport,
    pub quick_open: MetricReport,
    pub project_search: MetricReport,
    pub replace_query: MetricReport,
    pub cancel_search: MetricReport,
    pub quit_in_flight_search: MetricReport,
    pub editing: MetricReport,
    pub save: MetricReport,
    pub project_search_total_hits: usize,
    pub project_search_visible_results: usize,
    pub project_search_rows: Vec<SearchResultReport>,
    pub vm_hwm_bytes: u64,
    pub vm_hwm_limit_bytes: u64,
    pub descendant_process_count: usize,
    pub input_trace: InputTrace,
    pub assertions: BTreeMap<String, bool>,
    pub all_assertions_passed: bool,
}

pub fn statistics(samples: &[u64]) -> (u64, u64, u64) {
    if samples.is_empty() {
        return (0, 0, 0);
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let rank = |percent: usize| {
        let index = (percent * sorted.len()).div_ceil(100).saturating_sub(1);
        sorted[index]
    };
    (rank(50), rank(95), sorted[sorted.len() - 1])
}

pub fn environment_report() -> Result<EnvironmentReport> {
    let cpuinfo = fs::read_to_string("/proc/cpuinfo").context("read /proc/cpuinfo")?;
    let cpu_model = cpuinfo
        .lines()
        .find_map(|line| line.strip_prefix("model name\t: "))
        .unwrap_or("unknown")
        .to_owned();
    let cpu_core_count = std::thread::available_parallelism()
        .map(|count| count.get() as u64)
        .unwrap_or(0);
    let meminfo = fs::read_to_string("/proc/meminfo").context("read /proc/meminfo")?;
    let ram_bytes = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))
        .and_then(|value| value.split_whitespace().next())
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0)
        .saturating_mul(1024);
    let kernel = fs::read_to_string("/proc/sys/kernel/osrelease")
        .context("read kernel release")?
        .trim()
        .to_owned();
    let runner_image_version = std::env::var("ImageVersion")
        .or_else(|_| std::env::var("RUNNER_IMAGE_VERSION"))
        .unwrap_or_else(|_| os_release_value("VERSION_ID").unwrap_or_else(|| "unknown".to_owned()));
    let runner_environment =
        std::env::var("RUNNER_ENVIRONMENT").unwrap_or_else(|_| "local".to_owned());
    let image_os = std::env::var("ImageOS").unwrap_or_else(|_| "linux".to_owned());
    let runner_class = if runner_environment == "github-hosted"
        && image_os == "ubuntu24"
        && std::env::consts::ARCH == "x86_64"
    {
        "github-hosted-ubuntu-24.04-x86_64".to_owned()
    } else {
        format!("{runner_environment}-{image_os}-{}", std::env::consts::ARCH)
    };
    Ok(EnvironmentReport {
        runner_class,
        cpu_model,
        cpu_core_count,
        ram_bytes,
        kernel,
        runner_image_version,
        architecture: std::env::consts::ARCH.to_owned(),
        locale: std::env::var("LC_ALL")
            .or_else(|_| std::env::var("LANG"))
            .unwrap_or_else(|_| "unknown".to_owned()),
        term: std::env::var("TERM").unwrap_or_else(|_| "unknown".to_owned()),
        columns: COLS,
        rows: ROWS,
        pty_parser: "vt100 0.16.2".to_owned(),
    })
}

fn os_release_value(key: &str) -> Option<String> {
    fs::read_to_string("/etc/os-release")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{key}=")))
        .map(|value| value.trim_matches('"').to_owned())
}

pub fn verify_environment(environment: &EnvironmentReport) -> Result<()> {
    ensure!(
        environment.runner_class == "github-hosted-ubuntu-24.04-x86_64",
        "unexpected runner class"
    );
    ensure!(!environment.cpu_model.is_empty(), "cpu_model is empty");
    ensure!(environment.cpu_core_count > 0, "cpu_core_count is zero");
    ensure!(environment.ram_bytes > 0, "ram_bytes is zero");
    ensure!(!environment.kernel.is_empty(), "kernel is empty");
    ensure!(
        !environment.runner_image_version.is_empty(),
        "runner_image_version is empty"
    );
    ensure!(
        environment.architecture == "x86_64",
        "architecture is not x86_64"
    );
    ensure!(environment.locale == "C.UTF-8", "locale mismatch");
    ensure!(environment.term == "xterm-256color", "TERM mismatch");
    ensure!(
        (environment.columns, environment.rows) == (COLS, ROWS),
        "terminal dimensions mismatch"
    );
    ensure!(environment.pty_parser == "vt100 0.16.2", "parser mismatch");
    ensure!(
        environment == &environment_report()?,
        "reported environment differs from the verifier environment"
    );
    Ok(())
}

pub fn oracle_hashes() -> OracleHashes {
    OracleHashes {
        generator_source_sha256: fixture::generator_source_sha256(),
        spec_sha256: fixture::sha256_hex(&fixture::spec_bytes()),
        before_manifest_sha256: fixture::sha256_hex(&fixture::expected_before_manifest()),
        after_manifest_sha256: fixture::sha256_hex(
            &fixture::expected_after_manifest(1).expect("workflow 1 is valid"),
        ),
    }
}

pub fn binary_report(path: &Path) -> Result<BinaryReport> {
    let canonical = fs::canonicalize(path)
        .with_context(|| format!("canonicalize zec binary {}", path.display()))?;
    let bytes =
        fs::read(&canonical).with_context(|| format!("read zec binary {}", canonical.display()))?;
    Ok(BinaryReport {
        release_profile: is_release_zec(&canonical),
        path: canonical.display().to_string(),
        content_sha256: fixture::sha256_hex(&bytes),
    })
}

fn is_release_zec(path: &Path) -> bool {
    path.file_name() == Some(OsStr::new("zec"))
        && path.parent().and_then(Path::file_name) == Some(OsStr::new("release"))
}
pub fn write_report(path: &Path, report: &impl Serialize) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("create report directory {}", parent.display()))?;
    }
    let mut bytes = serde_json::to_vec_pretty(report).context("serialize report")?;
    bytes.push(b'\n');
    fs::write(path, bytes).with_context(|| format!("write report {}", path.display()))
}

pub fn command_output_with_timeout(command: &mut Command, timeout: Duration) -> Result<Output> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn bounded command")?;
    let mut stdout = child
        .stdout
        .take()
        .context("bounded command has no stdout")?;
    let mut stderr = child
        .stderr
        .take()
        .context("bounded command has no stderr")?;
    let stdout_reader = thread::Builder::new()
        .name("alpha-1-command-stdout".to_owned())
        .spawn(move || {
            let mut bytes = Vec::new();
            stdout.read_to_end(&mut bytes)?;
            Ok::<_, io::Error>(bytes)
        })
        .context("spawn bounded stdout reader")?;
    let stderr_reader = thread::Builder::new()
        .name("alpha-1-command-stderr".to_owned())
        .spawn(move || {
            let mut bytes = Vec::new();
            stderr.read_to_end(&mut bytes)?;
            Ok::<_, io::Error>(bytes)
        })
        .context("spawn bounded stderr reader")?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().context("poll bounded command")? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let cleanup_deadline = Instant::now() + CHILD_TIMEOUT;
            let _ = reap_std_child(&mut child, cleanup_deadline);
            let _ = join_output_reader(stdout_reader, cleanup_deadline, "stdout");
            let _ = join_output_reader(stderr_reader, cleanup_deadline, "stderr");
            bail!("command exceeded {} seconds", timeout.as_secs());
        }
        thread::sleep(Duration::from_millis(10));
    };
    let cleanup_deadline = Instant::now() + CHILD_TIMEOUT;
    let stdout = join_output_reader(stdout_reader, cleanup_deadline, "stdout")?;
    let stderr = join_output_reader(stderr_reader, cleanup_deadline, "stderr")?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn reap_std_child(
    child: &mut std::process::Child,
    deadline: Instant,
) -> Result<std::process::ExitStatus> {
    loop {
        if let Some(status) = child.try_wait().context("poll killed command")? {
            return Ok(status);
        }
        let now = Instant::now();
        ensure!(
            now < deadline,
            "killed command was not reaped within 5 seconds"
        );
        thread::sleep((deadline - now).min(Duration::from_millis(10)));
    }
}

fn join_output_reader(
    handle: JoinHandle<io::Result<Vec<u8>>>,
    deadline: Instant,
    stream: &str,
) -> Result<Vec<u8>> {
    while !handle.is_finished() {
        let now = Instant::now();
        ensure!(
            now < deadline,
            "{stream} reader did not finish within 5 seconds"
        );
        thread::sleep((deadline - now).min(Duration::from_millis(10)));
    }
    handle
        .join()
        .map_err(|_| anyhow::anyhow!("bounded {stream} reader panicked"))?
        .with_context(|| format!("read bounded command {stream}"))
}

pub fn read_report<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let bytes = fs::read(path).with_context(|| format!("read report {}", path.display()))?;
    ensure!(bytes.ends_with(b"\n"), "report must end with LF");
    serde_json::from_slice(&bytes).with_context(|| format!("parse report {}", path.display()))
}

pub fn expected_poc_ids() -> Vec<String> {
    EXPECTED_POC_IDS.lines().map(str::to_owned).collect()
}

pub fn benchmark_oracle() -> Result<BenchmarkOracle> {
    let spec: serde_json::Value =
        serde_json::from_slice(&fixture::spec_bytes()).context("parse generated Alpha 1 spec")?;
    serde_json::from_value(
        spec.get("benchmark")
            .cloned()
            .context("Alpha 1 spec has no benchmark object")?,
    )
    .context("parse complete benchmark oracle")
}

pub fn verify_benchmark_oracle(oracle: &BenchmarkOracle) -> Result<()> {
    ensure!(
        oracle.report_schema_version == REPORT_SCHEMA_VERSION,
        "benchmark report schema oracle differs"
    );
    ensure!(
        (
            oracle.startup.warmups,
            oracle.startup.samples,
            oracle.startup.p95_max_us
        ) == (2, 20, 3_000_000),
        "startup benchmark oracle differs"
    );
    ensure!(
        (
            oracle.quick_open.warmups,
            oracle.quick_open.samples,
            oracle.quick_open.p95_max_us,
            oracle.quick_open.max_us,
        ) == (10, 100, 150_000, 500_000),
        "quick-open benchmark oracle differs"
    );
    ensure!(
        oracle.quick_open.queries.len() == fixture::BENCH_QUICK_OPEN_QUERIES,
        "quick-open query oracle count differs"
    );
    for (index, query) in oracle.quick_open.queries.iter().enumerate() {
        let expected = format!("bench/search-{index:04}.txt");
        ensure!(
            query.query == expected && query.expected_selected_path == expected,
            "quick-open query oracle {index} differs"
        );
    }
    ensure!(
        (
            oracle.project_search.warmups,
            oracle.project_search.samples,
            oracle.project_search.p95_max_us,
            oracle.project_search.expected_hits,
            oracle.project_search.visible_results,
        ) == (
            2,
            10,
            5_000_000,
            fixture::BENCH_SEARCH_HITS,
            fixture::SEARCH_RESULT_LIMIT
        ),
        "project-search benchmark oracle differs"
    );
    ensure!(
        (
            oracle.in_flight_search.attempts_each,
            oracle.in_flight_search.max_us
        ) == (20, 250_000),
        "in-flight benchmark oracle differs"
    );
    ensure!(
        (
            oracle.editing.path.as_str(),
            oracle.editing.warmups,
            oracle.editing.samples,
            oracle.editing.p95_max_us,
            oracle.editing.max_us,
            oracle.editing.input_id_template.as_str(),
            oracle.editing.payload_suffix.as_str(),
        ) == (
            "bench/large-100000-lines.txt",
            10,
            500,
            100_000,
            500_000,
            "EDIT_{SEQUENCE_4}",
            "LF",
        ),
        "editing benchmark oracle differs"
    );
    ensure!(
        (
            oracle.save.path.as_str(),
            oracle.save.size,
            oracle.save.warmups,
            oracle.save.samples,
            oracle.save.max_us,
        ) == ("bench/save-5mib.txt", 5 * 1024 * 1024, 2, 10, 2_000_000),
        "save benchmark oracle differs"
    );
    ensure!(
        oracle.vm_hwm_max_bytes == 1_073_741_824
            && oracle.descendant_process_count == 0
            && oracle.nearest_rank_p95 == "sorted_samples[ceil(0.95*N)-1]"
            && oracle.input_invariants.sent_equals_applied_equals_expected
            && oracle.input_invariants.dropped_count == 0
            && !oracle.input_invariants.reordered,
        "benchmark resource/statistics/input invariant oracle differs"
    );
    Ok(())
}

pub fn expected_benchmark_search_rows() -> Vec<SearchResultReport> {
    let spec: serde_json::Value =
        serde_json::from_slice(&fixture::spec_bytes()).expect("generated spec is valid JSON");
    serde_json::from_value(
        spec["project_search"]["queries"]
            .as_array()
            .and_then(|queries| queries.iter().find(|query| query["id"] == "benchmark"))
            .and_then(|query| query.get("expected_results"))
            .cloned()
            .expect("benchmark expected_results are fixed in the spec"),
    )
    .expect("benchmark expected_results match SearchResultReport")
}

pub fn expected_benchmark_quick_open_queries() -> Vec<QuickOpenQuery> {
    benchmark_oracle()
        .expect("generated benchmark oracle is valid")
        .quick_open
        .queries
}

pub fn observe_poc_tests(repo: &Path) -> Result<(usize, Vec<String>)> {
    let mut observed = BTreeSet::new();
    let mut ignored = BTreeSet::new();
    for arguments in [
        vec!["test", "--locked", "--bin", "zec", "--", "--list"],
        vec![
            "test",
            "--locked",
            "--test",
            "pty_acceptance",
            "--",
            "--list",
        ],
    ] {
        let mut command = Command::new("cargo");
        command.args(&arguments).current_dir(repo);
        let output = command_output_with_timeout(&mut command, SCENARIO_TIMEOUT)
            .context("run cargo test --list for PoC baseline")?;
        ensure!(
            output.status.success(),
            "cargo test --list failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if let Some(id) = line.strip_suffix(": test") {
                observed.insert(id.to_owned());
            }
        }

        let mut ignored_arguments = arguments;
        ignored_arguments.push("--ignored");
        let mut command = Command::new("cargo");
        command.args(ignored_arguments).current_dir(repo);
        let output = command_output_with_timeout(&mut command, SCENARIO_TIMEOUT)
            .context("run cargo test --list --ignored for PoC baseline")?;
        ensure!(
            output.status.success(),
            "cargo test --list --ignored failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if let Some(id) = line.strip_suffix(": test") {
                ignored.insert(id.to_owned());
            }
        }
    }
    let expected = expected_poc_ids();
    let expected_set = expected.iter().collect::<BTreeSet<_>>();
    ensure!(
        expected.len() == 94
            && expected_set.len() == 94
            && expected.iter().all(|id| !id.is_empty()),
        "fixed PoC ID baseline is not exactly 94 unique non-empty IDs"
    );
    let missing = expected
        .iter()
        .filter(|id| !observed.contains(*id) || ignored.contains(*id))
        .cloned()
        .collect();
    Ok((observed.len(), missing))
}

pub fn verify_acceptance_report(report: &AcceptanceReport) -> Result<()> {
    ensure!(
        report.schema_version == REPORT_SCHEMA_VERSION,
        "schema mismatch"
    );
    ensure!(
        report.contract_version == fixture::CONTRACT_VERSION,
        "contract mismatch"
    );
    ensure!(
        report.report_kind == "alpha_1_acceptance",
        "report kind mismatch"
    );
    verify_environment(&report.environment)?;
    verify_binary(&report.binary)?;
    ensure!(report.oracles == oracle_hashes(), "oracle hashes mismatch");
    let required = fixture::required_case_ids();
    ensure!(
        required.len() == 186,
        "compiled required case count is not 186"
    );
    ensure!(
        report.poc_expected_count == 94,
        "PoC expected count mismatch"
    );
    ensure!(
        report.poc_observed_count >= report.poc_expected_count,
        "observed PoC test count regressed"
    );
    ensure!(
        report.poc_missing_ids.is_empty(),
        "PoC test IDs are missing"
    );
    ensure!(
        report.required_case_count == required.len(),
        "case count mismatch"
    );
    ensure!(
        report.cases.len() == required.len(),
        "case result count mismatch"
    );
    let ids = report
        .cases
        .iter()
        .map(|case| case.id.clone())
        .collect::<BTreeSet<_>>();
    ensure!(ids.len() == report.cases.len(), "duplicate case IDs");
    ensure!(
        ids == required.into_iter().collect(),
        "required case IDs differ"
    );
    let passed = report.cases.iter().filter(|case| case.passed).count();
    let failed = report.cases.len() - passed;
    ensure!(
        (report.passed, report.failed) == (passed, failed),
        "stored pass/fail counts are inconsistent"
    );
    ensure!(failed == 0, "{failed} acceptance case(s) failed");
    for case in &report.cases {
        ensure!(
            u128::from(case.duration_us) <= SCENARIO_TIMEOUT.as_micros(),
            "{} exceeded the 120-second scenario limit",
            case.id
        );
        if let Some(run) = case.id.strip_prefix("A3_WORKFLOW_") {
            let run = run.parse::<u8>().context("invalid A3 workflow ID")?;
            let expected = (1..=4)
                .map(|sequence| format!("A3_{run:02}_{sequence:04}"))
                .collect::<Vec<_>>();
            let trace = case
                .input_trace
                .as_ref()
                .with_context(|| format!("{} has no input trace", case.id))?;
            ensure!(
                trace.expected_input_ids == expected
                    && trace.sent_input_ids == expected
                    && trace.applied_input_ids == expected,
                "{} input ID sequence differs",
                case.id
            );
            ensure!(
                trace.dropped_count == 0 && !trace.reordered,
                "{} input trace reports drop/reorder",
                case.id
            );
        }
    }
    ensure!(
        report.all_assertions_passed,
        "acceptance assertion summary is false"
    );
    Ok(())
}

pub fn verify_benchmark_report(report: &BenchmarkReport) -> Result<()> {
    ensure!(
        report.schema_version == REPORT_SCHEMA_VERSION,
        "schema mismatch"
    );
    ensure!(
        report.contract_version == fixture::CONTRACT_VERSION,
        "contract mismatch"
    );
    ensure!(
        report.report_kind == "alpha_1_benchmark",
        "report kind mismatch"
    );
    verify_environment(&report.environment)?;
    verify_binary(&report.binary)?;
    ensure!(report.oracles == oracle_hashes(), "oracle hashes mismatch");
    let oracle = benchmark_oracle()?;
    verify_benchmark_oracle(&oracle)?;
    for (label, metric, warmups, samples, p95_limit, max_limit) in [
        (
            "startup",
            &report.startup,
            oracle.startup.warmups,
            oracle.startup.samples,
            Some(oracle.startup.p95_max_us),
            None,
        ),
        (
            "quick_open",
            &report.quick_open,
            oracle.quick_open.warmups,
            oracle.quick_open.samples,
            Some(oracle.quick_open.p95_max_us),
            Some(oracle.quick_open.max_us),
        ),
        (
            "project_search",
            &report.project_search,
            oracle.project_search.warmups,
            oracle.project_search.samples,
            Some(oracle.project_search.p95_max_us),
            None,
        ),
        (
            "replace_query",
            &report.replace_query,
            0,
            oracle.in_flight_search.attempts_each,
            None,
            Some(oracle.in_flight_search.max_us),
        ),
        (
            "cancel_search",
            &report.cancel_search,
            0,
            oracle.in_flight_search.attempts_each,
            None,
            Some(oracle.in_flight_search.max_us),
        ),
        (
            "quit_in_flight_search",
            &report.quit_in_flight_search,
            0,
            oracle.in_flight_search.attempts_each,
            None,
            Some(oracle.in_flight_search.max_us),
        ),
        (
            "editing",
            &report.editing,
            oracle.editing.warmups,
            oracle.editing.samples,
            Some(oracle.editing.p95_max_us),
            Some(oracle.editing.max_us),
        ),
        (
            "save",
            &report.save,
            oracle.save.warmups,
            oracle.save.samples,
            None,
            Some(oracle.save.max_us),
        ),
    ] {
        ensure!(
            metric.warmups == warmups
                && metric.expected_samples == samples
                && metric.p95_limit_us == p95_limit
                && metric.max_limit_us == max_limit,
            "{label}: fixed metric contract differs"
        );
    }
    for (label, metric) in [
        ("startup", &report.startup),
        ("quick_open", &report.quick_open),
        ("project_search", &report.project_search),
        ("replace_query", &report.replace_query),
        ("cancel_search", &report.cancel_search),
        ("quit_in_flight_search", &report.quit_in_flight_search),
        ("editing", &report.editing),
        ("save", &report.save),
    ] {
        metric.verify(label)?;
    }
    ensure!(
        report.project_search_total_hits == oracle.project_search.expected_hits,
        "project search hit count mismatch"
    );
    ensure!(
        report.project_search_visible_results == oracle.project_search.visible_results,
        "visible project search count mismatch"
    );
    ensure!(
        report.project_search_rows == expected_benchmark_search_rows(),
        "ordered visible project search rows differ from spec"
    );
    ensure!(
        report.vm_hwm_bytes > 0
            && report.vm_hwm_bytes <= report.vm_hwm_limit_bytes
            && report.vm_hwm_limit_bytes == oracle.vm_hwm_max_bytes,
        "VmHWM limit failed"
    );
    ensure!(
        report.descendant_process_count == oracle.descendant_process_count,
        "zec spawned descendant processes"
    );
    let trace = &report.input_trace;
    ensure!(
        trace.sent_input_ids == trace.expected_input_ids
            && trace.applied_input_ids == trace.expected_input_ids,
        "input ID sequences differ"
    );
    ensure!(
        oracle.input_invariants.sent_equals_applied_equals_expected,
        "input equality invariant is disabled"
    );
    ensure!(
        trace.dropped_count == oracle.input_invariants.dropped_count,
        "input IDs were dropped"
    );
    ensure!(
        trace.reordered == oracle.input_invariants.reordered,
        "input IDs were reordered"
    );
    let expected_edit_ids = (1..=oracle.editing.samples)
        .map(|sequence| format!("EDIT_{sequence:04}"))
        .collect::<Vec<_>>();
    ensure!(
        trace.expected_input_ids == expected_edit_ids,
        "editing expected ID sequence differs from the fixed contract"
    );
    let expected_assertion_keys = BTreeSet::from([
        "startup_latency",
        "quick_open_latency",
        "project_search_latency",
        "replace_query_latency",
        "cancel_search_latency",
        "quit_in_flight_latency",
        "editing_latency",
        "save_latency",
        "project_search_hits",
        "vm_hwm",
        "no_descendant_processes",
        "input_ids",
    ]);
    ensure!(
        report
            .assertions
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>()
            == expected_assertion_keys
            && report.assertions.values().all(|passed| *passed),
        "one or more benchmark assertions failed"
    );
    ensure!(
        report.all_assertions_passed,
        "benchmark assertion summary is false"
    );
    Ok(())
}

fn verify_binary(binary: &BinaryReport) -> Result<()> {
    ensure!(
        binary.release_profile,
        "zec binary was not built in release profile"
    );
    ensure!(!binary.path.is_empty(), "zec binary path is empty");
    let path = Path::new(&binary.path);
    let canonical = fs::canonicalize(path)
        .with_context(|| format!("canonicalize reported zec binary {}", path.display()))?;
    ensure!(
        is_release_zec(&canonical),
        "reported binary is not canonical target/release/zec"
    );
    let actual = fs::read(&canonical)
        .with_context(|| format!("read reported zec binary {}", canonical.display()))?;
    ensure!(
        binary.content_sha256.len() == 64
            && binary
                .content_sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "zec binary SHA-256 is invalid"
    );
    ensure!(
        fixture::sha256_hex(&actual) == binary.content_sha256,
        "reported zec binary SHA-256 differs from disk"
    );
    Ok(())
}

pub fn reset_fixed_fixture() -> Result<fixture::GeneratedFixture> {
    let workspace = Path::new(fixture::FIXED_WORKSPACE);
    ensure!(
        workspace == Path::new("/tmp/zec-alpha-1-v1"),
        "refusing to reset unexpected fixture path {}",
        workspace.display()
    );
    if fs::symlink_metadata(workspace).is_ok() {
        fs::remove_dir_all(workspace)
            .with_context(|| format!("remove old fixture {}", workspace.display()))?;
    }
    fixture::generate(Path::new(fixture::FIXED_ROOT)).map_err(anyhow::Error::msg)
}

pub fn fresh_config_dir(case_id: &str) -> Result<PathBuf> {
    let path = Path::new(fixture::FIXED_WORKSPACE)
        .join("configs")
        .join(case_id);
    if fs::symlink_metadata(&path).is_ok() {
        fs::remove_dir_all(&path)
            .with_context(|| format!("remove old config {}", path.display()))?;
    }
    fs::create_dir_all(&path).with_context(|| format!("create fresh config {}", path.display()))?;
    Ok(path)
}

enum ReaderEvent {
    Bytes {
        bytes: Vec<u8>,
        completed_at: Instant,
    },
    Eof,
    Error(io::Error),
}

#[derive(Clone, Copy, Debug)]
pub struct OperationMark {
    at: Instant,
    generation: u64,
}

impl OperationMark {
    pub fn elapsed_us(self) -> u64 {
        duration_us(self.at.elapsed())
    }
}

pub struct PtySession {
    master: Option<Box<dyn MasterPty + Send>>,
    child: Option<Box<dyn Child + Send + Sync>>,
    process_id: i32,
    writer: Option<Box<dyn io::Write + Send>>,
    reader: Option<JoinHandle<()>>,
    events: Option<Receiver<ReaderEvent>>,
    reader_eof: bool,
    parser: Parser,
    generation: u64,
    last_read_at: Option<Instant>,
    transcript: Vec<u8>,
    spawn_mark: OperationMark,
    scenario_deadline: Instant,
}

impl PtySession {
    pub fn spawn(
        zec: &Path,
        cwd: &Path,
        arguments: &[&OsStr],
        config_dir: &Path,
    ) -> Result<(Self, Termios)> {
        Self::spawn_with_env(zec, cwd, arguments, config_dir, &[])
    }

    pub fn spawn_with_env(
        zec: &Path,
        cwd: &Path,
        arguments: &[&OsStr],
        config_dir: &Path,
        environment: &[(&OsStr, &OsStr)],
    ) -> Result<(Self, Termios)> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: ROWS,
                cols: COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("open native PTY")?;
        let baseline = pair
            .master
            .get_termios()
            .context("PTY does not expose initial termios")?;
        Self::from_pair(pair, zec, cwd, arguments, config_dir, environment)
            .map(|session| (session, baseline))
    }

    fn from_pair(
        pair: PtyPair,
        zec: &Path,
        cwd: &Path,
        arguments: &[&OsStr],
        config_dir: &Path,
        environment: &[(&OsStr, &OsStr)],
    ) -> Result<Self> {
        let PtyPair { slave, master } = pair;
        let mut reader = master.try_clone_reader().context("clone PTY reader")?;
        let writer = master.take_writer().context("take PTY writer")?;
        let (sender, events) = mpsc::sync_channel(64);
        let reader_thread = thread::Builder::new()
            .name("alpha-1-pty-reader".to_owned())
            .spawn(move || {
                let mut buffer = [0_u8; 8192];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => {
                            let _ = sender.send(ReaderEvent::Eof);
                            break;
                        }
                        Ok(count) => {
                            let event = ReaderEvent::Bytes {
                                bytes: buffer[..count].to_vec(),
                                completed_at: Instant::now(),
                            };
                            if sender.send(event).is_err() {
                                break;
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error) if error.raw_os_error() == Some(nix::libc::EIO) => {
                            let _ = sender.send(ReaderEvent::Eof);
                            break;
                        }
                        Err(error) => {
                            let _ = sender.send(ReaderEvent::Error(error));
                            break;
                        }
                    }
                }
            })
            .context("spawn PTY reader")?;
        let mut command = CommandBuilder::new(zec);
        command.args(arguments);
        command.cwd(cwd);
        command.env("TERM", "xterm-256color");
        command.env("LANG", "C.UTF-8");
        command.env("LC_ALL", "C.UTF-8");
        command.env("XDG_CONFIG_HOME", config_dir);
        command.env("XDG_CACHE_HOME", config_dir);
        for (name, value) in environment {
            command.env(name, value);
        }
        command.env("XDG_DATA_HOME", config_dir);
        let spawn_started = Instant::now();
        let child = slave
            .spawn_command(command)
            .with_context(|| format!("spawn actual zec binary {}", zec.display()))?;
        let process_id = child
            .process_id()
            .context("zec child has no process ID")
            .and_then(|pid| i32::try_from(pid).context("zec PID does not fit pid_t"))?;
        drop(slave);
        Ok(Self {
            master: Some(master),
            child: Some(child),
            process_id,
            writer: Some(writer),
            reader: Some(reader_thread),
            events: Some(events),
            reader_eof: false,
            parser: Parser::new(ROWS, COLS, 0),
            generation: 0,
            last_read_at: None,
            transcript: Vec::new(),
            spawn_mark: OperationMark {
                at: spawn_started,
                generation: 0,
            },
            scenario_deadline: spawn_started + SCENARIO_TIMEOUT,
        })
    }

    pub fn pid(&self) -> Result<i32> {
        Ok(self.process_id)
    }

    pub fn wait_ready(&mut self, root_label: &str, sentinel: &str) -> Result<u64> {
        let mark = self.spawn_mark;
        self.wait_after("Alpha 1 Ready frame", mark, STARTUP_TIMEOUT, |screen| {
            let (cursor_row, cursor_col) = screen.cursor_position();
            let (rows, cols) = screen.size();
            screen.alternate_screen()
                && (rows, cols) == (ROWS, COLS)
                && screen.contents().contains(root_label)
                && screen.contents().contains(sentinel)
                && !screen.hide_cursor()
                && cursor_row < ROWS.saturating_sub(1)
                && cursor_col < COLS
        })
    }

    pub fn mark(&self) -> OperationMark {
        OperationMark {
            at: Instant::now(),
            generation: self.generation,
        }
    }

    pub fn send(&mut self, bytes: &[u8]) -> Result<()> {
        ensure!(
            Instant::now() < self.scenario_deadline,
            "PTY scenario exceeded 120 seconds"
        );
        let writer = self.writer.as_mut().context("PTY writer is closed")?;
        writer.write_all(bytes).context("write PTY input")?;
        writer.flush().context("flush PTY input")
    }

    pub fn send_marked(&mut self, bytes: &[u8]) -> Result<OperationMark> {
        self.send(bytes)?;
        Ok(self.mark())
    }

    pub fn paste(&mut self, text: &str) -> Result<()> {
        let mut bytes = Vec::with_capacity(text.len() + 12);
        bytes.extend_from_slice(b"\x1b[200~");
        bytes.extend_from_slice(text.as_bytes());
        bytes.extend_from_slice(b"\x1b[201~");
        self.send(&bytes)
    }

    pub fn paste_marked(&mut self, text: &str) -> Result<OperationMark> {
        self.paste(text)?;
        Ok(self.mark())
    }

    pub fn wait_contains(
        &mut self,
        description: &str,
        mark: OperationMark,
        needle: &str,
    ) -> Result<u64> {
        self.wait_after(description, mark, SCREEN_TIMEOUT, |screen| {
            screen.contents().contains(needle)
        })
    }

    pub fn wait_absent(
        &mut self,
        description: &str,
        mark: OperationMark,
        needle: &str,
    ) -> Result<u64> {
        self.wait_after(description, mark, SCREEN_TIMEOUT, |screen| {
            !screen.contents().contains(needle)
        })
    }

    pub fn wait_after(
        &mut self,
        description: &str,
        mark: OperationMark,
        timeout: Duration,
        predicate: impl Fn(&vt100::Screen) -> bool,
    ) -> Result<u64> {
        let deadline = (mark.at + timeout).min(self.scenario_deadline);
        loop {
            while let Some(event) = self.try_event()? {
                if let Some(elapsed) = self.consume_matching(event, mark, deadline, &predicate)? {
                    return Ok(elapsed);
                }
            }
            if let Some(child) = self.child.as_mut()
                && let Some(status) = child.try_wait().context("poll zec while waiting")?
            {
                self.child.take();
                bail!(
                    "zec exited as {status} before {description}\n{}",
                    self.diagnostic()
                );
            }
            if self.reader_eof {
                bail!("PTY EOF before {description}\n{}", self.diagnostic());
            }
            let now = Instant::now();
            if now >= deadline {
                bail!("timed out waiting for {description}\n{}", self.diagnostic());
            }
            if let Some(event) = self.receive_event((deadline - now).min(EVENT_POLL))?
                && let Some(elapsed) = self.consume_matching(event, mark, deadline, &predicate)?
            {
                return Ok(elapsed);
            }
        }
    }

    fn consume_matching(
        &mut self,
        event: ReaderEvent,
        mark: OperationMark,
        deadline: Instant,
        predicate: &impl Fn(&vt100::Screen) -> bool,
    ) -> Result<Option<u64>> {
        let completed_at = match &event {
            ReaderEvent::Bytes { completed_at, .. } => Some(*completed_at),
            _ => None,
        };
        self.consume(event)?;
        Ok(completed_at
            .filter(|completed| {
                *completed <= deadline
                    && *completed >= mark.at
                    && self.generation > mark.generation
                    && predicate(self.parser.screen())
            })
            .map(|completed| duration_us(completed.saturating_duration_since(mark.at))))
    }

    pub fn send_signal(&self, signal: Signal) -> Result<()> {
        ensure!(
            Instant::now() < self.scenario_deadline,
            "PTY scenario exceeded 120 seconds"
        );
        let pid = self.pid()?;
        killpg(Pid::from_raw(pid), signal)
            .with_context(|| format!("send {signal:?} to zec process group {pid}"))
    }

    pub fn wait_stopped(&mut self) -> Result<()> {
        let pid = Pid::from_raw(self.pid()?);
        let deadline = (Instant::now() + CHILD_TIMEOUT).min(self.scenario_deadline);
        loop {
            let now = Instant::now();
            if now >= deadline {
                bail!("timed out waiting for stopped zec\n{}", self.diagnostic());
            }
            self.drain_available()?;
            ensure!(
                Instant::now() < deadline,
                "timed out waiting for stopped zec\n{}",
                self.diagnostic()
            );
            match waitpid(pid, Some(WaitPidFlag::WNOHANG | WaitPidFlag::WUNTRACED))
                .context("wait for zec stop")?
            {
                WaitStatus::Stopped(_, Signal::SIGSTOP | Signal::SIGTSTP) => return Ok(()),
                WaitStatus::StillAlive | WaitStatus::Continued(_) => {}
                status => bail!("zec exited instead of stopping: {status:?}"),
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            self.receive_one(remaining.min(EVENT_POLL))?;
        }
    }

    pub fn wait_exit(&mut self) -> Result<ExitStatus> {
        let deadline = (Instant::now() + CHILD_TIMEOUT).min(self.scenario_deadline);
        loop {
            let now = Instant::now();
            if now >= deadline {
                bail!("timed out waiting for zec exit\n{}", self.diagnostic());
            }
            self.drain_available()?;
            ensure!(
                Instant::now() < deadline,
                "timed out waiting for zec exit\n{}",
                self.diagnostic()
            );
            let status = self
                .child
                .as_mut()
                .context("zec child was already reaped")?
                .try_wait()
                .context("poll zec exit")?;
            if let Some(status) = status {
                self.child.take();
                return Ok(status);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            self.receive_one(remaining.min(EVENT_POLL))?;
        }
    }

    pub fn assert_raw(&self, baseline: &Termios) -> Result<()> {
        let current = self.termios()?;
        ensure!(&current != baseline, "zec did not enable raw mode");
        ensure!(
            !current
                .local_flags
                .contains(LocalFlags::ICANON | LocalFlags::ECHO),
            "canonical input or echo remained enabled"
        );
        Ok(())
    }

    pub fn assert_restored(&mut self, baseline: &Termios) -> Result<()> {
        let deadline = (Instant::now() + CHILD_TIMEOUT).min(self.scenario_deadline);
        loop {
            self.drain_available()?;
            let now = Instant::now();
            if contains_bytes(&self.transcript, CLEANUP_ESCAPES) {
                ensure!(
                    now <= deadline,
                    "terminal cleanup escapes arrived after 5 seconds"
                );
                break;
            }
            if now >= deadline {
                bail!(
                    "terminal cleanup escapes were not observed\n{}",
                    self.diagnostic()
                );
            }
            self.receive_one((deadline - now).min(EVENT_POLL))?;
        }
        self.drain_available()?;
        ensure!(&self.termios()? == baseline, "tcgetattr baseline differs");
        let screen = self.parser.screen();
        ensure!(
            !screen.alternate_screen(),
            "alternate screen remains active"
        );
        ensure!(!screen.hide_cursor(), "cursor remains hidden");
        ensure!(!screen.bracketed_paste(), "bracketed paste remains active");
        ensure!(
            !screen.application_keypad(),
            "application keypad remains active"
        );
        ensure!(
            !screen.application_cursor(),
            "application cursor remains active"
        );
        ensure!(
            screen.mouse_protocol_mode() == MouseProtocolMode::None,
            "mouse mode remains active"
        );
        ensure!(
            screen.mouse_protocol_encoding() == MouseProtocolEncoding::Default,
            "mouse encoding remains active"
        );
        Ok(())
    }

    pub fn assert_restored_and_joined(&mut self, baseline: &Termios) -> Result<()> {
        ensure!(
            self.child.is_none(),
            "zec child was not reaped before reader join"
        );
        self.assert_restored(baseline)?;
        self.writer.take();
        self.master.take();

        let deadline = (Instant::now() + CHILD_TIMEOUT).min(self.scenario_deadline);
        loop {
            self.drain_available()?;
            let now = Instant::now();
            let reader_finished = self.reader.as_ref().is_none_or(JoinHandle::is_finished);
            if self.reader_eof && reader_finished {
                ensure!(now <= deadline, "PTY reader reached EOF after 5 seconds");
                break;
            }
            if now >= deadline {
                bail!("PTY reader did not reach EOF and finish within 5 seconds");
            }
            self.receive_one((deadline - now).min(EVENT_POLL))?;
        }
        self.reader
            .take()
            .context("PTY reader handle is absent")?
            .join()
            .map_err(|_| anyhow::anyhow!("PTY reader thread panicked"))?;
        self.events.take();
        assert_process_group_absent(self.process_id)
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    pub fn diagnostic(&self) -> String {
        let start = self.transcript.len().saturating_sub(DIAGNOSTIC_TAIL);
        format!(
            "generation={}\nscreen:\n{}\nraw tail:\n{:?}",
            self.generation,
            self.parser.screen().contents(),
            String::from_utf8_lossy(&self.transcript[start..])
        )
    }

    fn termios(&self) -> Result<Termios> {
        self.master
            .as_ref()
            .context("PTY master is closed")?
            .get_termios()
            .context("PTY no longer exposes termios")
    }

    fn try_event(&self) -> Result<Option<ReaderEvent>> {
        match self
            .events
            .as_ref()
            .context("PTY events are closed")?
            .try_recv()
        {
            Ok(event) => Ok(Some(event)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) if !self.reader_eof => {
                bail!("PTY reader disconnected")
            }
            Err(TryRecvError::Disconnected) => Ok(None),
        }
    }

    fn receive_event(&self, timeout: Duration) -> Result<Option<ReaderEvent>> {
        match self
            .events
            .as_ref()
            .context("PTY events are closed")?
            .recv_timeout(timeout)
        {
            Ok(event) => Ok(Some(event)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) if !self.reader_eof => {
                bail!("PTY reader disconnected")
            }
            Err(RecvTimeoutError::Disconnected) => Ok(None),
        }
    }

    fn drain_available(&mut self) -> Result<()> {
        loop {
            let event = match self
                .events
                .as_ref()
                .context("PTY events are closed")?
                .try_recv()
            {
                Ok(event) => event,
                Err(TryRecvError::Empty) => return Ok(()),
                Err(TryRecvError::Disconnected) if !self.reader_eof => {
                    bail!("PTY reader disconnected")
                }
                Err(TryRecvError::Disconnected) => return Ok(()),
            };
            self.consume(event)?;
        }
    }

    fn receive_one(&mut self, timeout: Duration) -> Result<()> {
        let event = match self
            .events
            .as_ref()
            .context("PTY events are closed")?
            .recv_timeout(timeout)
        {
            Ok(event) => event,
            Err(RecvTimeoutError::Timeout) => return Ok(()),
            Err(RecvTimeoutError::Disconnected) if !self.reader_eof => {
                bail!("PTY reader disconnected")
            }
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        };
        self.consume(event)
    }

    fn consume(&mut self, event: ReaderEvent) -> Result<()> {
        match event {
            ReaderEvent::Bytes {
                bytes,
                completed_at,
            } => {
                self.parser.process(&bytes);
                self.generation = self.generation.wrapping_add(1);
                self.last_read_at = Some(completed_at);
                append_bounded(&mut self.transcript, &bytes);
                Ok(())
            }
            ReaderEvent::Eof => {
                self.reader_eof = true;
                Ok(())
            }
            ReaderEvent::Error(error) => Err(error).context("read actual zec PTY output"),
        }
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        let deadline = Instant::now() + CHILD_TIMEOUT;
        if self.child.is_some() {
            let _ = killpg(Pid::from_raw(self.process_id), Signal::SIGKILL);
        }
        while let Some(child) = self.child.as_mut() {
            match child.try_wait() {
                Ok(Some(_)) => {
                    self.child.take();
                }
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(
                        deadline
                            .saturating_duration_since(Instant::now())
                            .min(EVENT_POLL),
                    );
                }
                Ok(None) | Err(_) => {
                    self.child.take();
                }
            }
        }
        self.writer.take();
        self.master.take();
        self.events.take();
        if let Some(reader) = self.reader.take() {
            while !reader.is_finished() && Instant::now() < deadline {
                thread::sleep(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(EVENT_POLL),
                );
            }
            if reader.is_finished() {
                let _ = reader.join();
            }
        }
    }
}

pub fn assert_process_group_absent(process_group: i32) -> Result<()> {
    let result = unsafe { nix::libc::kill(-process_group, 0) };
    if result == 0 {
        bail!("process group {process_group} still exists after child exit");
    }
    let error = io::Error::last_os_error();
    ensure!(
        error.raw_os_error() == Some(nix::libc::ESRCH),
        "cannot prove process group {process_group} is absent: {error}"
    );
    Ok(())
}

pub fn vm_hwm_bytes(pid: i32) -> Result<u64> {
    vm_hwm_bytes_if_present(pid)?.context("VmHWM is absent from zec status")
}

pub fn vm_hwm_bytes_if_present(pid: i32) -> Result<Option<u64>> {
    let status = fs::read_to_string(format!("/proc/{pid}/status"))
        .with_context(|| format!("read /proc/{pid}/status"))?;

    Ok(status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .and_then(|value| value.split_whitespace().next())
        .and_then(|value| value.parse::<u64>().ok())
        .map(|kib| kib.saturating_mul(1024)))
}

pub fn descendant_process_count(pid: i32) -> Result<usize> {
    let mut descendants = BTreeSet::new();
    let mut pending = vec![pid];
    while let Some(parent) = pending.pop() {
        let task_path = format!("/proc/{parent}/task");
        let tasks = match fs::read_dir(&task_path) {
            Ok(tasks) => tasks,
            Err(error) if error.kind() == io::ErrorKind::NotFound && parent != pid => continue,
            Err(error) => return Err(error).with_context(|| format!("read {task_path}")),
        };
        for task in tasks {
            let task = task.with_context(|| format!("iterate {task_path}"))?;
            let tid = match task.file_name().to_string_lossy().parse::<i32>() {
                Ok(tid) => tid,
                Err(_) => continue,
            };
            let children_path = format!("/proc/{parent}/task/{tid}/children");
            let children = match fs::read_to_string(&children_path) {
                Ok(children) => children,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error).with_context(|| format!("read {children_path}"));
                }
            };
            for child in children.split_whitespace() {
                let child = child
                    .parse::<i32>()
                    .with_context(|| format!("parse child PID in {children_path}"))?;
                if child != pid && descendants.insert(child) {
                    pending.push(child);
                }
            }
        }
    }
    Ok(descendants.len())
}

pub fn open_fd_count() -> Result<usize> {
    Ok(fs::read_dir("/proc/self/fd")
        .context("read /proc/self/fd")?
        .count())
}

fn duration_us(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn append_bounded(transcript: &mut Vec<u8>, bytes: &[u8]) {
    if bytes.len() >= TRANSCRIPT_LIMIT {
        transcript.clear();
        transcript.extend_from_slice(&bytes[bytes.len() - TRANSCRIPT_LIMIT..]);
        return;
    }
    let overflow = transcript
        .len()
        .saturating_add(bytes.len())
        .saturating_sub(TRANSCRIPT_LIMIT);
    if overflow > 0 {
        transcript.drain(..overflow);
    }
    transcript.extend_from_slice(bytes);
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty()
        || haystack
            .windows(needle.len())
            .any(|window| window == needle)
}
