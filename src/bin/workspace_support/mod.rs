#![allow(dead_code)]

use std::{
    collections::BTreeSet,
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
};

use crate::{
    e2e_support::{
        BinaryReport, EnvironmentReport, MetricReport, PtySession, TerminalBaseline, binary_report,
        environment_report, read_report, statistics, verify_environment, write_report,
    },
    language_support::{CaseResult, CorrelationTrace, EvidenceFile},
};
use anyhow::{Context as _, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;

pub const REPORT_SCHEMA_VERSION: u32 = 1;
pub const CONTRACT_VERSION: u32 = 3;
pub const FRESH_PROCESS_RUNS: usize = 20;
pub const REQUIRED_CASE_COUNT: usize = 361;
pub const VM_HWM_LIMIT_BYTES: u64 = 1_879_048_192;
pub const READY_SENTINEL: &str = "E2E_WORKSPACE_READY";
pub const SEARCH_TOKEN: &str = "E2E_WORKSPACE_SEARCH_TOKEN";

pub const CAPABILITY_PREFIXES: &[&str] = &[
    "C2_LAYOUT",
    "C2_TABS",
    "C3_PROJECT_PANEL",
    "C4_SEARCH_REPLACE",
    "C5_NAV_OUTLINE",
    "C6_ADVANCED_EDITOR",
    "C7_SESSION_RESTORE",
    "C7_CRASH_RECOVERY",
    "C8_CAPABILITY_FALLBACK",
];

pub const FAILURE_SCENARIOS: &[&str] = &[
    "CORRUPT_TRUNCATED_SESSION",
    "PROJECT_PANEL_SYMLINK_ESCAPE",
    "FILE_OPERATION_PERMISSION_DENIED",
    "DIRTY_DELETE_RENAME_CONFLICT",
    "REPLACE_FINGERPRINT_CONFLICT",
    "TERMINAL_1X1_RESIZE_STORM",
    "WATCHER_OVERFLOW_RESCAN_REORDER",
    "STALE_OUTLINE_PANEL_SEARCH_GENERATION",
    "CRASH_DURING_RECOVERY_JOURNAL_COMMIT",
];

#[derive(Clone, Debug)]
pub struct RunArguments {
    pub zec: PathBuf,
    pub assert: bool,
    pub report: PathBuf,
}

#[derive(Clone, Debug)]
pub enum Invocation {
    Run(RunArguments),
    Verify(PathBuf),
}

pub fn parse_invocation() -> Result<Invocation> {
    let mut arguments = std::env::args_os().skip(1);
    let mut zec = None;
    let mut report = None;
    let mut verify = None;
    let mut assert = false;
    while let Some(argument) = arguments.next() {
        match argument.to_str() {
            Some("--zec") => zec = Some(value(&mut arguments, "--zec")?.into()),
            Some("--report") => report = Some(value(&mut arguments, "--report")?.into()),
            Some("--verify-report") => {
                verify = Some(value(&mut arguments, "--verify-report")?.into())
            }
            Some("--assert") => assert = true,
            Some("--help" | "-h") => {
                let program = std::env::args()
                    .next()
                    .unwrap_or_else(|| "workspace_e2e".to_owned());
                println!(
                    "Usage: {program} --zec PATH --assert --report PATH\n       {program} --verify-report PATH"
                );
                std::process::exit(0);
            }
            _ => bail!("unknown or non-UTF-8 argument: {argument:?}"),
        }
    }
    if let Some(path) = verify {
        ensure!(
            zec.is_none() && report.is_none() && !assert,
            "--verify-report cannot be combined with run arguments"
        );
        return Ok(Invocation::Verify(path));
    }
    Ok(Invocation::Run(RunArguments {
        zec: zec.context("--zec PATH is required")?,
        assert,
        report: report.context("--report PATH is required")?,
    }))
}

fn value(arguments: &mut impl Iterator<Item = OsString>, flag: &str) -> Result<OsString> {
    arguments
        .next()
        .with_context(|| format!("{flag} requires a value"))
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SuiteBinary {
    pub zec: BinaryReport,
}

impl SuiteBinary {
    pub fn collect(zec: &Path) -> Result<Self> {
        Ok(Self {
            zec: binary_report(zec)?,
        })
    }

    pub fn verify(&self) -> Result<()> {
        ensure!(
            self.zec.release_profile,
            "zec was not built in release profile"
        );
        let current = binary_report(Path::new(&self.zec.path))?;
        ensure!(
            current.content_sha256 == self.zec.content_sha256,
            "zec binary digest differs from report"
        );
        Ok(())
    }
}

pub fn acceptance_case_ids(runs: usize) -> Vec<String> {
    let mut ids = vec!["C1_WORKSPACE_MODEL".to_owned()];
    for prefix in CAPABILITY_PREFIXES {
        for run in 1..=runs {
            ids.push(format!("{prefix}_{run:02}"));
        }
    }
    for scenario in FAILURE_SCENARIOS {
        for run in 1..=runs {
            ids.push(format!("C8_{scenario}_{run:02}"));
        }
    }
    ids
}

pub fn canonical_acceptance_case_ids() -> Vec<String> {
    acceptance_case_ids(FRESH_PROCESS_RUNS)
}

pub fn verify_cases(cases: &[CaseResult], required: &[String]) -> Result<()> {
    ensure!(cases.len() == required.len(), "case count differs");
    ensure!(
        cases.iter().map(|case| &case.id).eq(required.iter()),
        "case IDs or order differ from contract"
    );
    ensure!(
        cases.iter().all(|case| case.passed),
        "one or more acceptance cases failed"
    );
    ensure!(
        cases.iter().all(|case| case.duration_us <= 120_000_000),
        "an acceptance case exceeded 120 seconds"
    );
    Ok(())
}

pub fn verify_correlation(trace: &CorrelationTrace, case_ids: &[String]) -> Result<()> {
    trace.verify()?;
    ensure!(
        trace.input_ids == case_ids,
        "input/apply correlation does not cover every canonical case"
    );
    Ok(())
}

pub fn canonical_environment() -> Result<EnvironmentReport> {
    environment_report()
}

pub fn verify_canonical_environment(environment: &EnvironmentReport) -> Result<()> {
    verify_environment(environment)
}

pub fn artifacts_directory(report: &Path, kind: &str) -> Result<PathBuf> {
    let parent = report.parent().unwrap_or_else(|| Path::new("."));
    let directory = parent.join(format!("{kind}-artifacts"));
    fs::create_dir_all(&directory)
        .with_context(|| format!("create artifact directory {}", directory.display()))?;
    Ok(directory)
}

pub fn persist_bytes(
    evidence: &mut Vec<EvidenceFile>,
    directory: &Path,
    label: &str,
    bytes: &[u8],
) -> Result<()> {
    let path = directory.join(label);
    fs::write(&path, bytes).with_context(|| format!("write evidence {}", path.display()))?;
    evidence.push(EvidenceFile::collect(label, &path)?);
    Ok(())
}

pub fn json_bytes(value: &impl Serialize) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn duration_us(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

pub fn metric(
    warmups: usize,
    samples: usize,
    values: Vec<u64>,
    p95_limit_us: Option<u64>,
    max_limit_us: Option<u64>,
) -> MetricReport {
    MetricReport::new(warmups, samples, values, p95_limit_us, max_limit_us)
}

pub fn verify_metric(metric: &MetricReport, label: &str) -> Result<()> {
    ensure!(
        metric.raw_samples_us.len() == metric.expected_samples,
        "{label}: sample count differs"
    );
    ensure!(!metric.raw_samples_us.is_empty(), "{label}: no samples");
    let (p50, p95, max) = statistics(&metric.raw_samples_us);
    ensure!(
        (metric.p50_us, metric.p95_us, metric.max_us) == (p50, p95, max),
        "{label}: stored nearest-rank statistics differ"
    );
    let passed = metric.p95_limit_us.is_none_or(|limit| p95 <= limit)
        && metric.max_limit_us.is_none_or(|limit| max <= limit);
    ensure!(
        metric.assertion_passed == passed && passed,
        "{label}: limit exceeded"
    );
    Ok(())
}

pub fn write_json_report(path: &Path, report: &impl Serialize) -> Result<()> {
    write_report(path, report)
}

pub fn read_json_report<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    read_report(path)
}

pub struct Fixture {
    _temp: TempDir,
    pub root: PathBuf,
    pub source: PathBuf,
    pub peer: PathBuf,
    pub outside: PathBuf,
    pub config: PathBuf,
    pub session: PathBuf,
    environment: Vec<(OsString, OsString)>,
}

impl Fixture {
    pub fn create(case_id: &str) -> Result<Self> {
        let temp = tempfile::Builder::new()
            .prefix("zec-workspace-e2e-")
            .tempdir()
            .context("create Alpha 3 fixture")?;
        let root = temp.path().join("project");
        let source_directory = root.join("src");
        let locked = root.join("locked");
        let outside = temp.path().join("outside");
        let config = temp.path().join("config");
        let session = temp.path().join("session");
        for directory in [
            &source_directory,
            &locked,
            &outside,
            &config.join("config"),
            &session,
            &temp.path().join("home"),
        ] {
            fs::create_dir_all(directory)
                .with_context(|| format!("create fixture directory {}", directory.display()))?;
        }

        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"workspace-e2e\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[workspace]\n",
        )?;
        fs::write(
            root.join("README.md"),
            format!("{READY_SENTINEL} {case_id}\n"),
        )?;
        fs::write(root.join(".gitignore"), "target/\n")?;
        let source = source_directory.join("main.rs");
        let peer = source_directory.join("peer.rs");
        fs::write(
            &source,
            format!(
                "// {READY_SENTINEL}\nfn folded_fixture() {{\n    let value = \"{SEARCH_TOKEN}\";\n    println!(\"{{value}}\");\n}}\n// {}E2E_WRAP_TAIL\n",
                "w".repeat(180)
            ),
        )?;
        fs::write(
            &peer,
            format!("// peer {READY_SENTINEL}\npub const TOKEN: &str = \"{SEARCH_TOKEN}\";\n"),
        )?;
        fs::write(outside.join("outside.txt"), "must remain outside\n")?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, root.join("escape"))
            .context("create project-panel escape symlink")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o555))?;
        }

        fs::write(
            config.join("config/settings.json"),
            r#"{
              "session": { "trust_all_worktrees": true },
              "format_on_save": "off",
              "remove_trailing_whitespace_on_save": false,
              "ensure_final_newline_on_save": false,
              "show_completions_on_input": false
            }"#,
        )?;
        fs::write(config.join("config/global_settings.json"), "{}")?;
        fs::write(config.join("config/keymap.json"), "[]")?;

        let environment = vec![
            (OsString::from("ZEC_DISABLE_SESSIONS"), OsString::from("0")),
            (
                OsString::from("ZEC_SESSION_DIR"),
                session.clone().into_os_string(),
            ),
            (
                OsString::from("HOME"),
                temp.path().join("home").into_os_string(),
            ),
        ];
        Ok(Self {
            _temp: temp,
            root,
            source,
            peer,
            outside,
            config,
            session,
            environment,
        })
    }

    pub fn env_pairs(&self) -> Vec<(&OsStr, &OsStr)> {
        self.environment
            .iter()
            .map(|(name, value)| (name.as_os_str(), value.as_os_str()))
            .collect()
    }

    pub fn spawn(&self, zec: &Path) -> Result<(PtySession, TerminalBaseline)> {
        self.spawn_with_extra(zec, &[])
    }

    pub fn spawn_with_extra(
        &self,
        zec: &Path,
        extra: &[(&OsStr, &OsStr)],
    ) -> Result<(PtySession, TerminalBaseline)> {
        let arguments = [self.root.as_os_str(), self.source.as_os_str()];
        let mut environment = self.env_pairs();
        environment.extend_from_slice(extra);
        PtySession::spawn_with_env(zec, &self.root, &arguments, &self.config, &environment)
    }

    pub fn manifest(&self) -> Result<std::collections::BTreeMap<String, String>> {
        crate::language_support::manifest(&self.root)
    }

    pub fn session_files(&self) -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        collect_files(&self.session, &mut files)?;
        files.sort();
        Ok(files)
    }
}

fn collect_files(directory: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    if !directory.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(directory)
        .with_context(|| format!("read artifact directory {}", directory.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_dir() {
            collect_files(&path, files)?;
        } else if metadata.is_file() {
            files.push(path);
        }
    }
    Ok(())
}

pub fn assert_unique_strings(values: &[String], label: &str) -> Result<()> {
    ensure!(
        values.iter().collect::<BTreeSet<_>>().len() == values.len(),
        "{label} contains duplicates"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_case_plan_is_exact_ordered_and_unique() {
        let ids = canonical_acceptance_case_ids();
        assert_eq!(ids.len(), REQUIRED_CASE_COUNT);
        assert_eq!(ids.first().map(String::as_str), Some("C1_WORKSPACE_MODEL"));
        assert_eq!(ids.get(1).map(String::as_str), Some("C2_LAYOUT_01"));
        assert_eq!(
            ids.get(180).map(String::as_str),
            Some("C8_CAPABILITY_FALLBACK_20")
        );
        assert_eq!(
            ids.last().map(String::as_str),
            Some("C8_CRASH_DURING_RECOVERY_JOURNAL_COMMIT_20")
        );
        assert_eq!(ids.iter().collect::<BTreeSet<_>>().len(), ids.len());
    }

    #[test]
    fn metric_verifier_recomputes_nearest_rank() {
        let values = (1..=20).map(|value| value * 1_000).collect::<Vec<_>>();
        let report = metric(0, 20, values, Some(19_000), Some(20_000));
        verify_metric(&report, "fixture").unwrap();
    }
}
