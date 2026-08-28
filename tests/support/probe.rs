//! Shared cross-platform fixture plumbing for the Alpha 2 actual-binary tests.
//!
//! Isolation works through `ZEC_DATA_DIR` (identical on every platform); the
//! XDG variables additionally fence off Unix-only side channels. The fixture
//! language server is exposed to PATH discovery as `rust-analyzer` via a
//! symlink on Unix and a hard link/copy named `rust-analyzer.exe` on Windows.

#![allow(dead_code)]

use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;
use tempfile::TempDir;

pub struct ProbeEnvironment {
    pub temp: TempDir,
    pub root: PathBuf,
    pub source_dir: PathBuf,
    pub zed_dir: PathBuf,
    /// User-level Zed config directory: `<ZEC_DATA_DIR>/config`.
    pub user_config: PathBuf,
    pub bin_dir: PathBuf,
    pub log: PathBuf,
    environment: Vec<(&'static str, OsString)>,
}

impl ProbeEnvironment {
    pub fn new(package_name: &str) -> Self {
        let temp = tempfile::tempdir().expect("create Alpha 2 test directory");
        let root = temp.path().join("project");
        let source_dir = root.join("src");
        let zed_dir = root.join(".zed");
        let bin_dir = temp.path().join("bin");
        let home = temp.path().join("home");
        let data_root = temp.path().join("zec-data");
        let user_config = data_root.join("config");
        let xdg_data = temp.path().join("xdg-data");
        let xdg_cache = temp.path().join("xdg-cache");
        let xdg_state = temp.path().join("xdg-state");
        let rustup_home = temp.path().join("rustup");
        let cargo_home = temp.path().join("cargo");
        for directory in [
            &source_dir,
            &zed_dir,
            &bin_dir,
            &home,
            &user_config,
            &xdg_data,
            &xdg_cache,
            &xdg_state,
            &rustup_home,
            &cargo_home,
        ] {
            fs::create_dir_all(directory).expect("create Alpha 2 fixture directory");
        }
        fs::write(
            root.join("Cargo.toml"),
            format!(
                "[package]\nname = \"{package_name}\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[workspace]\n"
            ),
        )
        .expect("write fixture Cargo.toml");
        install_failing_rustup(&bin_dir);

        let log = temp.path().join("lsp.jsonl");
        let environment = vec![
            ("PATH", restricted_path(&bin_dir)),
            ("HOME", home.into_os_string()),
            ("ZEC_DATA_DIR", data_root.into_os_string()),
            (
                "XDG_CONFIG_HOME",
                temp.path().join("xdg-config").into_os_string(),
            ),
            ("XDG_DATA_HOME", xdg_data.into_os_string()),
            ("XDG_CACHE_HOME", xdg_cache.into_os_string()),
            ("XDG_STATE_HOME", xdg_state.into_os_string()),
            ("RUSTUP_HOME", rustup_home.into_os_string()),
            ("CARGO_HOME", cargo_home.into_os_string()),
            ("ZEC_FIXTURE_LSP_LOG", log.clone().into_os_string()),
        ];

        Self {
            temp,
            root,
            source_dir,
            zed_dir,
            user_config,
            bin_dir,
            log,
            environment,
        }
    }

    pub fn push_env(&mut self, name: &'static str, value: impl Into<OsString>) {
        self.environment.push((name, value.into()));
    }

    /// Exposes the fixture language server to PATH discovery as rust-analyzer.
    pub fn install_fixture_server(&self, fixture_server: &Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(fixture_server, self.bin_dir.join("rust-analyzer"))
            .expect("link fixture as rust-analyzer");
        #[cfg(windows)]
        {
            let target = self.bin_dir.join("rust-analyzer.exe");
            if fs::hard_link(fixture_server, &target).is_err() {
                fs::copy(fixture_server, &target).expect("copy fixture as rust-analyzer.exe");
            }
            // First execution of a fresh binary can stall on hosted runners
            // (on-access antivirus scan); absorb that outside the probe's
            // discovery window.
            let _ = Command::new(&target).arg("--help").output();
        }
    }

    /// Installs a rust-analyzer that PATH discovery finds but the OS refuses
    /// to spawn.
    pub fn install_unspawnable_server(&self) {
        #[cfg(unix)]
        {
            let wrapper = self.bin_dir.join("rust-analyzer");
            fs::write(
                &wrapper,
                "#!/bin/sh\nif [ \"${1:-}\" = \"--help\" ]; then\n  echo fixture\n  chmod 000 \"$0\"\n  exit 0\nfi\nexit 99\n",
            )
            .expect("write spawn-failure wrapper");
            make_executable(&wrapper);
        }
        #[cfg(windows)]
        fs::write(self.bin_dir.join("rust-analyzer.exe"), b"")
            .expect("write invalid rust-analyzer image");
    }

    /// Runs one `zec probe` invocation and returns its JSON report.
    pub fn run_probe(
        &self,
        zec: &Path,
        probe: &str,
        source: &Path,
        extra_arguments: &[&str],
        timeout: Duration,
        label: &str,
    ) -> Value {
        let mut command = Command::new(zec);
        command.args([
            "probe",
            probe,
            self.root.to_str().expect("UTF-8 fixture root"),
            source.to_str().expect("UTF-8 fixture source"),
        ]);
        command.args(extra_arguments);
        for (name, value) in &self.environment {
            command.env(name, value);
        }
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|error| panic!("spawn {label}: {error}"));

        let deadline = Instant::now() + timeout;
        loop {
            if child
                .try_wait()
                .unwrap_or_else(|error| panic!("poll {label}: {error}"))
                .is_some()
            {
                break;
            }
            if Instant::now() >= deadline {
                child
                    .kill()
                    .unwrap_or_else(|error| panic!("kill timed-out {label}: {error}"));
                let output = child
                    .wait_with_output()
                    .unwrap_or_else(|error| panic!("collect timed-out {label}: {error}"));
                panic!(
                    "{label} exceeded {} seconds\nstdout={}\nstderr={}",
                    timeout.as_secs(),
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            thread::sleep(Duration::from_millis(25));
        }
        let output = child
            .wait_with_output()
            .unwrap_or_else(|error| panic!("collect {label}: {error}"));
        assert!(
            output.status.success(),
            "{label} failed\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|error| panic!("parse {label} JSON output: {error}"))
    }
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;

    let mut permissions = fs::metadata(path)
        .expect("read fixture permissions")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("make fixture executable");
}

/// Masks any real rustup so toolchain discovery falls back to PATH lookup.
fn install_failing_rustup(bin_dir: &Path) {
    #[cfg(unix)]
    {
        let fake_rustup = bin_dir.join("rustup");
        fs::write(&fake_rustup, "#!/bin/sh\nexit 1\n").expect("write fake rustup");
        make_executable(&fake_rustup);
    }
    #[cfg(windows)]
    {
        // The restricted PATH simply omits rustup.
        let _ = bin_dir;
    }
}

/// PATH that exposes only the fixture bin directory plus the system
/// directories needed for processes to start at all.
fn restricted_path(bin_dir: &Path) -> OsString {
    #[cfg(unix)]
    {
        OsString::from(format!(
            "{}:/usr/local/bin:/usr/bin:/bin",
            bin_dir.display()
        ))
    }
    #[cfg(windows)]
    {
        let mut entries = vec![bin_dir.to_path_buf()];
        if let Some(system_root) = std::env::var_os("SystemRoot").map(PathBuf::from) {
            entries.push(system_root.join("System32"));
            entries.push(system_root);
        }
        std::env::join_paths(entries).expect("join restricted PATH entries")
    }
}

pub fn process_exists(pid: u64) -> bool {
    #[cfg(unix)]
    {
        PathBuf::from(format!("/proc/{pid}")).exists()
    }
    #[cfg(windows)]
    {
        let Ok(pid) = u32::try_from(pid) else {
            return false;
        };
        let pid = sysinfo::Pid::from_u32(pid);
        let refresh = sysinfo::ProcessRefreshKind::nothing();
        let mut system = sysinfo::System::new();
        system.refresh_processes_specifics(sysinfo::ProcessesToUpdate::Some(&[pid]), true, refresh);
        system.process(pid).is_some()
    }
}
