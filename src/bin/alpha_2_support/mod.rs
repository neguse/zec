#![allow(dead_code)]

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fs,
    os::unix::fs::{PermissionsExt as _, symlink},
    path::{Path, PathBuf},
    process::{Command, Output},
    thread,
    time::{Duration, Instant},
};

use crate::alpha_1_support::{
    BinaryReport, EnvironmentReport, MetricReport, binary_report, command_output_with_timeout,
    environment_report, statistics,
};
use anyhow::{Context as _, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;

pub const REPORT_SCHEMA_VERSION: u32 = 1;
pub const CONTRACT_VERSION: u32 = 2;
pub const FRESH_PROCESS_RUNS: usize = 20;
pub const LANGUAGE_ITEM_LIMIT: usize = 10_000;
pub const LANGUAGE_TEXT_LIMIT_BYTES: usize = 64 * 1024;
pub const OVERLAY_ROW_LIMIT: usize = 200;
pub const VM_HWM_LIMIT_BYTES: u64 = 1_610_612_736;
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(35);

pub const FAILURE_SCENARIOS: &[&str] = &[
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

#[derive(Clone, Debug)]
pub struct RunArguments {
    pub zec: PathBuf,
    pub lsp: PathBuf,
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
    let mut lsp = None;
    let mut report = None;
    let mut verify = None;
    let mut assert = false;
    while let Some(argument) = arguments.next() {
        match argument.to_str() {
            Some("--zec") => zec = Some(value(&mut arguments, "--zec")?.into()),
            Some("--lsp") => lsp = Some(value(&mut arguments, "--lsp")?.into()),
            Some("--report") => report = Some(value(&mut arguments, "--report")?.into()),
            Some("--verify-report") => {
                verify = Some(value(&mut arguments, "--verify-report")?.into())
            }
            Some("--assert") => assert = true,
            Some("--help" | "-h") => {
                let program = std::env::args()
                    .next()
                    .unwrap_or_else(|| "alpha_2_gate".to_owned());
                println!(
                    "Usage: {program} --zec PATH --lsp PATH --assert --report PATH\n       {program} --verify-report PATH"
                );
                std::process::exit(0);
            }
            _ => bail!("unknown or non-UTF-8 argument: {argument:?}"),
        }
    }
    if let Some(path) = verify {
        ensure!(
            zec.is_none() && lsp.is_none() && report.is_none() && !assert,
            "--verify-report cannot be combined with run arguments"
        );
        return Ok(Invocation::Verify(path));
    }
    Ok(Invocation::Run(RunArguments {
        zec: zec.context("--zec PATH is required")?,
        lsp: lsp.context("--lsp PATH is required")?,
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
pub struct GateBinaries {
    pub zec: BinaryReport,
    pub fixture_lsp: GateBinary,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GateBinary {
    pub path: String,
    pub content_sha256: String,
    pub release_profile: bool,
}

impl GateBinaries {
    pub fn collect(zec: &Path, lsp: &Path) -> Result<Self> {
        Ok(Self {
            zec: binary_report(zec)?,
            fixture_lsp: GateBinary::collect(lsp, "alpha_2_fixture_lsp")?,
        })
    }

    pub fn verify(&self) -> Result<()> {
        ensure!(
            self.zec.release_profile,
            "zec was not built in release profile"
        );
        ensure!(
            binary_report(Path::new(&self.zec.path))?.content_sha256 == self.zec.content_sha256,
            "zec binary digest differs from report"
        );
        self.fixture_lsp.verify("alpha_2_fixture_lsp")
    }
}

impl GateBinary {
    fn collect(path: &Path, expected_name: &str) -> Result<Self> {
        let path = fs::canonicalize(path)
            .with_context(|| format!("canonicalize binary {}", path.display()))?;
        ensure!(
            path.file_name() == Some(OsStr::new(expected_name)),
            "expected {expected_name}, found {}",
            path.display()
        );
        let bytes = fs::read(&path).with_context(|| format!("read binary {}", path.display()))?;
        Ok(Self {
            release_profile: path.parent().and_then(Path::file_name) == Some(OsStr::new("release")),
            path: path.display().to_string(),
            content_sha256: sha256(&bytes),
        })
    }

    fn verify(&self, expected_name: &str) -> Result<()> {
        ensure!(
            self.release_profile,
            "{expected_name} was not built in release profile"
        );
        let current = Self::collect(Path::new(&self.path), expected_name)?;
        ensure!(
            current.content_sha256 == self.content_sha256,
            "{expected_name} binary digest differs from report"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CaseResult {
    pub id: String,
    pub passed: bool,
    pub duration_us: u64,
    pub detail: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceFile {
    pub label: String,
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

impl EvidenceFile {
    pub fn collect(label: impl Into<String>, path: &Path) -> Result<Self> {
        let bytes =
            fs::read(path).with_context(|| format!("read evidence file {}", path.display()))?;
        Ok(Self {
            label: label.into(),
            path: fs::canonicalize(path)
                .with_context(|| format!("canonicalize evidence {}", path.display()))?
                .display()
                .to_string(),
            bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            sha256: sha256(&bytes),
        })
    }

    pub fn verify(&self) -> Result<()> {
        let bytes = fs::read(&self.path)
            .with_context(|| format!("read reported evidence {}", self.path))?;
        ensure!(
            u64::try_from(bytes.len()).unwrap_or(u64::MAX) == self.bytes,
            "evidence size differs for {}",
            self.label
        );
        ensure!(
            sha256(&bytes) == self.sha256,
            "evidence digest differs for {}",
            self.label
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CorrelationTrace {
    pub request_ids: Vec<String>,
    pub response_ids: Vec<String>,
    pub input_ids: Vec<String>,
    pub apply_ids: Vec<String>,
}

impl CorrelationTrace {
    pub fn verify(&self) -> Result<()> {
        ensure!(
            self.request_ids == self.response_ids,
            "LSP request/response ID sequences differ"
        );
        ensure!(
            self.input_ids == self.apply_ids,
            "input/apply ID sequences differ"
        );
        ensure!(
            self.request_ids.iter().collect::<BTreeSet<_>>().len() == self.request_ids.len(),
            "duplicate request IDs"
        );
        ensure!(
            self.input_ids.iter().collect::<BTreeSet<_>>().len() == self.input_ids.len(),
            "duplicate input IDs"
        );
        Ok(())
    }
}

pub fn acceptance_case_ids() -> Vec<String> {
    let mut ids = Vec::new();
    for run in 1..=FRESH_PROCESS_RUNS {
        ids.push(format!("B1_B5_LANGUAGE_{run:02}"));
    }
    ids.push("B2_SETTINGS_RELOAD".to_owned());
    for run in 1..=FRESH_PROCESS_RUNS {
        ids.push(format!("B3_B6_B7_PTY_{run:02}"));
    }
    for scenario in FAILURE_SCENARIOS {
        let scenario = scenario.replace('-', "_").to_ascii_uppercase();
        for run in 1..=FRESH_PROCESS_RUNS {
            ids.push(format!("B7_{scenario}_{run:02}"));
        }
    }
    ids
}

pub fn verify_cases(cases: &[CaseResult], required: &[String]) -> Result<()> {
    ensure!(cases.len() == required.len(), "case count differs");
    let actual = cases.iter().map(|case| case.id.clone()).collect::<Vec<_>>();
    ensure!(actual == required, "case IDs or order differ from contract");
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

pub fn canonical_environment() -> Result<EnvironmentReport> {
    environment_report()
}

pub fn verify_canonical_environment(environment: &EnvironmentReport) -> Result<()> {
    crate::alpha_1_support::verify_environment(environment)
}

pub fn write_report(path: &Path, report: &impl Serialize) -> Result<()> {
    crate::alpha_1_support::write_report(path, report)
}

pub fn read_report<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    crate::alpha_1_support::read_report(path)
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FixtureMode<'a> {
    Normal,
    Settings,
    Failure(&'a str),
    PtyRestricted,
    LargePayload,
}

pub struct Fixture {
    _temp: TempDir,
    pub root: PathBuf,
    pub source: PathBuf,
    pub peer: PathBuf,
    pub xdg_config: PathBuf,
    pub log: PathBuf,
    pub restart_state: PathBuf,
    environment: Vec<(OsString, OsString)>,
}

impl Fixture {
    pub fn create(lsp: &Path, mode: FixtureMode<'_>) -> Result<Self> {
        let temp = tempfile::tempdir().context("create Alpha 2 fixture directory")?;
        let workspace = temp.path();
        let root = workspace.join("project");
        let source_dir = root.join("src");
        let zed_dir = root.join(".zed");
        let bin_dir = workspace.join("bin");
        let home = workspace.join("home");
        let xdg_config = workspace.join("xdg-config");
        let xdg_data = workspace.join("xdg-data");
        let xdg_cache = workspace.join("xdg-cache");
        let xdg_state = workspace.join("xdg-state");
        let rustup_home = workspace.join("rustup");
        let cargo_home = workspace.join("cargo");
        for directory in [
            &source_dir,
            &source_dir.join("nested"),
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
            fs::create_dir_all(directory)
                .with_context(|| format!("create fixture component {}", directory.display()))?;
        }

        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"alpha-2-gate\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[workspace]\n",
        )?;
        fs::write(root.join("README.md"), "# Alpha 2 gate fixture\n")?;
        fs::write(root.join(".gitignore"), "src/ignored.rs\n")?;
        let source = source_dir.join("main.rs");
        let peer = source_dir.join("lib.rs");
        fs::write(&source, "fn main() {\n    let _value = alpha_;\n}   \n")?;
        fs::write(
            &peer,
            "pub fn fixture_peer() {\n    let _peer = alpha_;\n}  \n",
        )?;
        fs::write(source_dir.join("unicode.rs"), "pub fn 日本語_🧪() {}\n")?;
        fs::write(source_dir.join("crlf.rs"), b"pub fn crlf() {}\r\n")?;
        fs::write(source_dir.join("same_a.rs"), "pub fn same_name() {}\n")?;
        fs::write(source_dir.join("same_b.rs"), "pub fn same_name() {}\n")?;
        fs::write(source_dir.join("ignored.rs"), "pub fn ignored() {}\n")?;
        fs::write(source_dir.join("nested/mod.rs"), "pub fn nested() {}\n")?;

        let (project_settings, user_settings, keymap) = match mode {
            FixtureMode::Settings => (
                r#"{
                  "tab_size": 4,
                  "format_on_save": "off",
                  "completions": { "lsp": false },
                  "show_completions_on_input": false,
                  "languages": { "Rust": {
                    "tab_size": 5,
                    "format_on_save": "off",
                    "completions": { "lsp": true },
                    "show_completions_on_input": true
                  }}
                }"#,
                r#"{
                  "session": { "trust_all_worktrees": true },
                  "tab_size": 3,
                  "completions": { "lsp": true },
                  "show_completions_on_input": false
                }"#,
                r#"[{"context":"Editor","bindings":{"f1":null,"ctrl-k ctrl-p":"command_palette::Toggle"}}]"#,
            ),
            FixtureMode::Failure("formatter-error") => (
                r#"{
                  "format_on_save": "on",
                  "formatter": "language_server",
                  "remove_trailing_whitespace_on_save": false,
                  "ensure_final_newline_on_save": false
                }"#,
                r#"{"session":{"trust_all_worktrees":true}}"#,
                "[]",
            ),
            FixtureMode::PtyRestricted => (
                r#"{
                  "format_on_save": "off",
                  "remove_trailing_whitespace_on_save": false,
                  "ensure_final_newline_on_save": false
                }"#,
                "{}",
                "[]",
            ),
            _ => (
                r#"{
                  "format_on_save": "off",
                  "remove_trailing_whitespace_on_save": false,
                  "ensure_final_newline_on_save": false
                }"#,
                r#"{"session":{"trust_all_worktrees":true}}"#,
                "[]",
            ),
        };
        fs::write(zed_dir.join("settings.json"), project_settings)?;
        fs::write(xdg_config.join("zed/settings.json"), user_settings)?;
        fs::write(
            xdg_config.join("zed/global_settings.json"),
            if mode == FixtureMode::Settings {
                r#"{
                  "tab_size": 2,
                  "format_on_save": "off",
                  "completions": { "lsp": false },
                  "show_completions_on_input": false
                }"#
            } else {
                "{}"
            },
        )?;
        fs::write(xdg_config.join("zed/keymap.json"), keymap)?;

        let scenario = match mode {
            FixtureMode::Failure(scenario) => scenario,
            FixtureMode::LargePayload => "large-payloads",
            _ => "normal",
        };
        match mode {
            FixtureMode::Failure("server-not-found") => {}
            FixtureMode::Failure("spawn-failure") => {
                let wrapper = bin_dir.join("rust-analyzer");
                fs::write(
                    &wrapper,
                    "#!/bin/sh\nif [ \"${1:-}\" = \"--help\" ]; then\n  echo fixture\n  chmod 000 \"$0\"\n  exit 0\nfi\nexit 99\n",
                )?;
                make_executable(&wrapper)?;
            }
            _ => symlink(lsp, bin_dir.join("rust-analyzer"))
                .context("link fixture language server as rust-analyzer")?,
        }
        let fake_rustup = bin_dir.join("rustup");
        fs::write(&fake_rustup, "#!/bin/sh\nexit 1\n")?;
        make_executable(&fake_rustup)?;

        let log = workspace.join("lsp.jsonl");
        let restart_state = workspace.join("restart-state");
        let path = format!("{}:/usr/local/bin:/usr/bin:/bin", bin_dir.display());
        let environment = vec![
            (OsString::from("PATH"), OsString::from(path)),
            (OsString::from("HOME"), home.into_os_string()),
            (
                OsString::from("XDG_CONFIG_HOME"),
                xdg_config.clone().into_os_string(),
            ),
            (OsString::from("XDG_DATA_HOME"), xdg_data.into_os_string()),
            (OsString::from("XDG_CACHE_HOME"), xdg_cache.into_os_string()),
            (OsString::from("XDG_STATE_HOME"), xdg_state.into_os_string()),
            (OsString::from("RUSTUP_HOME"), rustup_home.into_os_string()),
            (OsString::from("CARGO_HOME"), cargo_home.into_os_string()),
            (
                OsString::from("ZEC_ALPHA2_LSP_LOG"),
                log.clone().into_os_string(),
            ),
            (
                OsString::from("ZEC_ALPHA2_LSP_SCENARIO"),
                OsString::from(match mode {
                    FixtureMode::Failure("server-not-found" | "spawn-failure") => "normal",
                    _ => scenario,
                }),
            ),
            (
                OsString::from("ZEC_ALPHA2_RESTART_STATE"),
                restart_state.clone().into_os_string(),
            ),
        ];

        Ok(Self {
            _temp: temp,
            root,
            source,
            peer,
            xdg_config,
            log,
            restart_state,
            environment,
        })
    }

    pub fn env_pairs(&self) -> Vec<(&OsStr, &OsStr)> {
        self.environment
            .iter()
            .map(|(name, value)| (name.as_os_str(), value.as_os_str()))
            .collect()
    }

    pub fn apply_environment(&self, command: &mut Command) {
        for (name, value) in &self.environment {
            command.env(name, value);
        }
    }

    pub fn manifest(&self) -> Result<BTreeMap<String, String>> {
        manifest(&self.root)
    }
}

fn make_executable(path: &Path) -> Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

pub fn run_probe(
    zec: &Path,
    fixture: &Fixture,
    probe: &str,
    scenario: Option<&str>,
) -> Result<Value> {
    let mut command = Command::new(zec);
    command.args([
        OsStr::new("--alpha-2-probe"),
        OsStr::new(probe),
        fixture.root.as_os_str(),
        fixture.source.as_os_str(),
    ]);
    if let Some(scenario) = scenario {
        command.arg(scenario);
    }
    fixture.apply_environment(&mut command);
    let output = command_output_with_timeout(&mut command, PROBE_TIMEOUT)?;
    parse_probe_output(probe, scenario, output)
}

fn parse_probe_output(probe: &str, scenario: Option<&str>, output: Output) -> Result<Value> {
    ensure!(
        output.status.success(),
        "{probe}{} failed as {}\nstdout={}\nstderr={}",
        scenario
            .map(|scenario| format!("/{scenario}"))
            .unwrap_or_default(),
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).with_context(|| {
        format!(
            "parse {probe} output as JSON; stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

pub fn assert_language_report(report: &Value) -> Result<()> {
    ensure!(
        report["language"] == "Rust",
        "Rust language was not resolved"
    );
    ensure!(
        report["servers"][0]["name"] == "rust-analyzer"
            && report["servers"][0]["process_id"].is_number(),
        "fixture language server is absent"
    );
    ensure!(
        report["completions"][0]["new_text"] == "alpha_completion()"
            && report["completions"][1]["new_text"] == "beta_completion",
        "completion oracle differs"
    );
    ensure!(
        report["terminal_completion"]["text_after_apply"]
            .as_str()
            .is_some_and(|text| text.contains("alpha_completion()"))
            && report["terminal_completion"]["text_after_undo"]
                .as_str()
                .is_some_and(|text| text.contains("alpha_")),
        "completion transaction/undo oracle differs"
    );
    ensure!(
        report["hover"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("Fixture hover")),
        "hover oracle differs"
    );
    ensure!(
        report["diagnostics"]["errors"] == 0 && report["diagnostics"]["warnings"] == 1,
        "diagnostic oracle differs"
    );
    ensure!(
        report["terminal_locations"]["definitions"]["items"]
            .as_array()
            .is_some_and(|items| items.len() == 1)
            && report["terminal_locations"]["type_definitions"]["items"]
                .as_array()
                .is_some_and(|items| items.len() == 1)
            && report["terminal_locations"]["references"]["items"]
                .as_array()
                .is_some_and(|items| items.len() == 2),
        "semantic location oracle differs"
    );
    ensure!(
        report["terminal_multibuffer"]["source_count"] == 2
            && report["terminal_multibuffer"]["disk_after_save"]
                == report["terminal_multibuffer"]["source_after_edit"]
            && report["terminal_multibuffer"]["disk_after_restore"]
                == report["terminal_multibuffer"]["source_before"],
        "editable MultiBuffer oracle differs"
    );
    let preview = &report["terminal_edits"]["rename"]["preview"];
    for field in [
        "read_only",
        "confirmation_signature_matches",
        "contains_main",
        "contains_peer",
        "contains_old_text",
        "contains_new_text",
        "rejected_contains_new_text",
        "rejection_unchanged",
    ] {
        ensure!(
            preview[field] == true,
            "rename preview field {field} differs"
        );
    }
    ensure!(
        report["terminal_edits"]["rename"]["buffer_count"] == 2
            && report["terminal_edits"]["rename"]["undo_buffer_count"] == 2
            && report["terminal_edits"]["rename"]["redo_buffer_count"] == 2,
        "rename transaction oracle differs"
    );
    ensure!(
        report["terminal_edits"]["code_action"]["preferred"] == true
            && report["terminal_edits"]["code_action"]["main_after"]
                .as_str()
                .is_some_and(|text| text.contains("fixture_fixed"))
            && report["terminal_edits"]["code_action"]["main_after_undo"]
                .as_str()
                .is_some_and(|text| text.contains("alpha_")),
        "code action oracle differs"
    );
    for scope in ["format_document", "format_range"] {
        ensure!(
            report["terminal_edits"][scope]["after"]
                .as_str()
                .is_some_and(|text| text.ends_with("}\n") && !text.contains("}   \n"))
                && report["terminal_edits"][scope]["after_undo"]
                    .as_str()
                    .is_some_and(|text| text.contains("}   \n")),
            "{scope} transaction oracle differs"
        );
    }
    Ok(())
}

pub fn assert_settings_report(report: &Value) -> Result<()> {
    ensure!(
        report["initial"]["tab_size"] == 5,
        "initial precedence differs"
    );
    ensure!(
        report["updated"]["tab_size"] == 7,
        "live settings update differs"
    );
    ensure!(
        report["updated"]["completion_lsp"] == false
            && report["retained_after_invalid"] == report["updated"],
        "invalid settings did not retain last good state"
    );
    ensure!(
        report["recovered"]["tab_size"] == 9 && report["recovered"]["completion_lsp"] == true,
        "settings recovery differs"
    );
    ensure!(
        report["format_on_save_disk"] == "fn main() { let value = 1; }\n",
        "format-on-save disk oracle differs"
    );
    for field in [
        "multi_chord_rebind",
        "unbind",
        "last_good_retained",
        "replacement_rebind",
    ] {
        ensure!(
            report["keymap"][field] == true,
            "keymap field {field} differs"
        );
    }
    Ok(())
}

pub fn assert_failure_report(report: &Value, scenario: &str) -> Result<()> {
    ensure!(
        report["scenario"] == scenario,
        "failure scenario label differs"
    );
    for field in [
        "dirty_after_edit",
        "undo_restored",
        "redo_restored",
        "saved",
    ] {
        ensure!(
            report["editor"][field] == true,
            "editor field {field} differs"
        );
    }
    ensure!(
        report["editor"]["close_elapsed_ms"]
            .as_u64()
            .is_some_and(|elapsed| elapsed <= 250),
        "editor close exceeded 250 ms"
    );
    let outcome = report["request"]["outcome"]
        .as_str()
        .context("failure report has no request outcome")?;
    match scenario {
        "request-error" => ensure!(
            outcome.contains("controlled completion failure")
                || (outcome == "ok" && report["request"]["completion_count"] == 0),
            "request error was not surfaced"
        ),
        "hang-request" => ensure!(
            outcome == "cancelled"
                && report["request"]["elapsed_ms"]
                    .as_u64()
                    .is_some_and(|elapsed| elapsed <= 350),
            "hung request was not cancelled within 350 ms"
        ),
        "restart-once" => ensure!(
            report["restart_requested"] == true
                && outcome == "ok"
                && report["request"]["completion_count"] == 2,
            "restart recovery differs"
        ),
        "formatter-error" => ensure!(
            report["formatter_failure"]["dirty"] == true
                && report["formatter_failure"]["error"]
                    .as_str()
                    .is_some_and(|error| error.contains("controlled formatter failure")),
            "formatter error did not preserve dirty text"
        ),
        "large-payloads" => ensure!(
            outcome == "ok"
                && report["request"]["completion_count"] == LANGUAGE_ITEM_LIMIT
                && report["request"]["max_documentation_bytes"] == LANGUAGE_TEXT_LIMIT_BYTES
                && report["request"]["overlay_rows"] == OVERLAY_ROW_LIMIT
                && report["diagnostics"]["item_count"] == LANGUAGE_ITEM_LIMIT
                && report["diagnostics"]["overlay_rows"] == OVERLAY_ROW_LIMIT,
            "large payload bounds differ"
        ),
        "huge-stderr" => ensure!(
            outcome == "ok" && report["request"]["completion_count"] == 2,
            "huge stderr prevented language service use"
        ),
        _ => ensure!(
            outcome == "ok" || outcome == "cancelled" || outcome.starts_with("error:"),
            "unexpected failure outcome {outcome}"
        ),
    }
    let notices = report["service_notices"]
        .as_array()
        .context("failure report has no service notices")?;
    ensure!(
        !notices.is_empty()
            || report["request"]["user_message"]
                == "completion unavailable: no ready language server",
        "failure had no user-facing status"
    );
    ensure!(
        notices.iter().all(|notice| notice["message"]
            .as_str()
            .is_some_and(|message| message.len() <= LANGUAGE_TEXT_LIMIT_BYTES)),
        "a service notice exceeded the text bound"
    );
    Ok(())
}

pub fn assert_lsp_trace(log: &Path, scenario: Option<&str>) -> Result<Vec<u64>> {
    if !log.exists() && matches!(scenario, Some("server-not-found" | "spawn-failure")) {
        return Ok(Vec::new());
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let initial_messages = read_complete_lsp_trace(log, deadline)?;
    let pids = initial_messages
        .iter()
        .filter(|entry| entry["direction"] == "lifecycle")
        .filter(|entry| entry["message"]["event"] == "start")
        .filter_map(|entry| entry["message"]["pid"].as_u64())
        .collect::<Vec<_>>();
    if !matches!(scenario, Some("server-not-found" | "spawn-failure")) {
        ensure!(!pids.is_empty(), "fixture language server did not start");
    }
    if scenario == Some("restart-once") {
        ensure!(pids.len() == 2, "restart must launch exactly two processes");
    }
    assert_processes_reaped(&pids)?;
    let messages = read_complete_lsp_trace(log, deadline)?;
    if scenario.is_none() {
        let methods = messages
            .iter()
            .filter(|entry| entry["direction"] == "client")
            .filter_map(|entry| entry.pointer("/message/method").and_then(Value::as_str))
            .collect::<Vec<_>>();
        for method in [
            "initialize",
            "initialized",
            "textDocument/didOpen",
            "textDocument/completion",
            "textDocument/hover",
            "textDocument/definition",
            "textDocument/typeDefinition",
            "textDocument/references",
            "workspace/symbol",
            "textDocument/prepareRename",
            "textDocument/rename",
            "textDocument/codeAction",
            "textDocument/formatting",
            "textDocument/rangeFormatting",
            "textDocument/didClose",
            "shutdown",
        ] {
            ensure!(methods.contains(&method), "LSP trace is missing {method}");
        }
    }
    Ok(pids)
}

pub fn wait_for_lsp_trace_method(log: &Path, method: &str, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let messages = read_complete_lsp_trace(log, deadline)?;
        if messages
            .iter()
            .any(|entry| entry["direction"] == "client" && entry["message"]["method"] == method)
        {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "fixture LSP did not record {method} within {} ms",
            timeout.as_millis()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn read_complete_lsp_trace(log: &Path, deadline: Instant) -> Result<Vec<Value>> {
    loop {
        let result = fs::read(log)
            .with_context(|| format!("read LSP trace {}", log.display()))
            .and_then(|bytes| {
                ensure!(
                    bytes.is_empty() || bytes.ends_with(b"\n"),
                    "LSP JSONL has an incomplete final line"
                );
                let entries = String::from_utf8(bytes).context("LSP trace is not UTF-8")?;
                entries
                    .lines()
                    .map(|line| {
                        serde_json::from_str::<Value>(line).context("parse LSP JSONL entry")
                    })
                    .collect::<Result<Vec<_>>>()
            });
        match result {
            Ok(messages) => return Ok(messages),
            Err(error) if Instant::now() < deadline => {
                let _ = error;
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    }
}

pub fn assert_processes_reaped(pids: &[u64]) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    for pid in pids {
        let process = PathBuf::from(format!("/proc/{pid}"));
        while process.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        ensure!(
            !process.exists(),
            "fixture process {pid} survived zec shutdown"
        );
    }
    Ok(())
}

pub fn trace_request_response_ids(log: &Path) -> Result<(Vec<String>, Vec<String>)> {
    let entries = fs::read_to_string(log)?;
    let mut requests = Vec::new();
    let mut responses = Vec::new();
    for line in entries.lines() {
        let entry: Value = serde_json::from_str(line)?;
        let direction = entry["direction"].as_str();
        let message = &entry["message"];
        let id = message.get("id").and_then(json_id);
        if message.get("method").is_some() && direction == Some("client") {
            if let Some(id) = id {
                requests.push(id);
            }
        } else if message.get("method").is_none() && direction == Some("server") {
            if let Some(id) = id {
                responses.push(id);
            }
        }
    }
    Ok((requests, responses))
}

fn json_id(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_i64().map(|id| id.to_string()))
        .or_else(|| value.as_u64().map(|id| id.to_string()))
}

pub fn manifest(root: &Path) -> Result<BTreeMap<String, String>> {
    let mut files = Vec::new();
    collect_files(root, root, &mut files)?;
    files.sort();
    files
        .into_iter()
        .map(|path| {
            let relative = path
                .strip_prefix(root)
                .expect("collected path is rooted")
                .to_string_lossy()
                .replace('\\', "/");
            let bytes = fs::read(&path)
                .with_context(|| format!("read fixture manifest path {}", path.display()))?;
            Ok((relative, sha256(&bytes)))
        })
        .collect()
}

fn collect_files(root: &Path, directory: &Path, output: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(directory)
        .with_context(|| format!("read fixture directory {}", directory.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_dir() {
            collect_files(root, &path, output)?;
        } else if metadata.is_file() {
            ensure!(path.starts_with(root), "manifest path escaped fixture root");
            output.push(path);
        }
    }
    Ok(())
}

pub fn json_bytes(value: &impl Serialize) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn duration_us(duration: Duration) -> u64 {
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

pub fn manifest_evidence_json(
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
) -> Value {
    json!({ "before": before, "after": after })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_acceptance_plan_is_complete_ordered_and_unique() {
        let ids = acceptance_case_ids();
        assert_eq!(ids.len(), 341);
        assert_eq!(ids.first().map(String::as_str), Some("B1_B5_LANGUAGE_01"));
        assert_eq!(ids.get(19).map(String::as_str), Some("B1_B5_LANGUAGE_20"));
        assert_eq!(ids.get(20).map(String::as_str), Some("B2_SETTINGS_RELOAD"));
        assert_eq!(ids.get(21).map(String::as_str), Some("B3_B6_B7_PTY_01"));
        assert_eq!(ids.last().map(String::as_str), Some("B7_HUGE_STDERR_20"));
        assert_eq!(ids.iter().collect::<BTreeSet<_>>().len(), ids.len());
    }

    #[test]
    fn metric_verifier_recomputes_nearest_rank_and_rejects_tampering() {
        let values = (1..=20).map(|value| value * 1_000).collect::<Vec<_>>();
        let report = metric(2, 20, values, Some(19_000), Some(20_000));
        verify_metric(&report, "fixture metric").expect("valid metric");

        let mut tampered = report.clone();
        tampered.p95_us += 1;
        assert!(verify_metric(&tampered, "fixture metric").is_err());

        let exceeded = metric(0, 2, vec![1, 3], Some(2), Some(3));
        assert!(verify_metric(&exceeded, "fixture metric").is_err());
    }

    #[test]
    fn correlation_requires_exact_unique_request_and_input_pairs() {
        let valid = CorrelationTrace {
            request_ids: vec!["request-1".to_owned(), "request-2".to_owned()],
            response_ids: vec!["request-1".to_owned(), "request-2".to_owned()],
            input_ids: vec!["input-1".to_owned()],
            apply_ids: vec!["input-1".to_owned()],
        };
        valid.verify().expect("valid correlation");

        let mut reordered = valid.clone();
        reordered.response_ids.reverse();
        assert!(reordered.verify().is_err());

        let duplicated = CorrelationTrace {
            request_ids: vec!["request-1".to_owned(), "request-1".to_owned()],
            response_ids: vec!["request-1".to_owned(), "request-1".to_owned()],
            input_ids: Vec::new(),
            apply_ids: Vec::new(),
        };
        assert!(duplicated.verify().is_err());
    }
}
