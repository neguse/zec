//! An optional log file. `ZEC_LOG=path` captures the `log` records that
//! Zed's crates emit about language servers, worktrees, and settings, which
//! the terminal UI cannot show.

use std::{fs::File, io::Write as _, path::PathBuf, sync::Mutex};

use anyhow::{Context as _, Result};

struct FileLogger {
    file: Mutex<File>,
}

impl log::Log for FileLogger {
    /// Everything at info and above; debug records only from zec and
    /// the Zed crates that run language servers and settings.
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Info
            || ["zec", "project", "lsp", "language", "settings"]
                .iter()
                .any(|prefix| metadata.target().starts_with(prefix))
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        if let Ok(mut file) = self.file.lock() {
            let _ = writeln!(
                file,
                "{} {} {}",
                record.level(),
                record.target(),
                record.args()
            );
        }
    }

    fn flush(&self) {}
}

/// Installs the file logger when `ZEC_LOG` names a file.
pub fn init_from_env() -> Result<()> {
    let Some(path) = std::env::var_os("ZEC_LOG").map(PathBuf::from) else {
        return Ok(());
    };
    let file = File::options()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("could not open the log file {}", path.display()))?;
    log::set_boxed_logger(Box::new(FileLogger {
        file: Mutex::new(file),
    }))
    .context("a logger is already installed")?;
    log::set_max_level(log::LevelFilter::Debug);
    Ok(())
}
