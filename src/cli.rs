//! Command-line surface: the argument grammar and process-level overrides.

use std::{
    collections::HashSet,
    env,
    ffi::OsString,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, bail, ensure};

pub const USAGE: &str =
    "Usage: zec [DIRECTORY | FILE ...]\n       zec --smoke\n       zec --version";

#[derive(Debug, Eq, PartialEq)]
pub enum Command {
    Edit(Vec<PathBuf>),
    Smoke,
    Version,
    Help,
}

/// Redirects every Zed-side user directory (config, data, logs, databases)
/// under `$ZEC_DATA_DIR` before anything resolves them. This is the one
/// isolation mechanism that behaves identically on every platform; the XDG
/// variables only cover Unix.
pub fn apply_data_dir_override() -> Result<()> {
    if let Some(directory) = env::var_os("ZEC_DATA_DIR") {
        let directory = directory
            .to_str()
            .context("ZEC_DATA_DIR must be valid UTF-8")?;
        paths::set_custom_data_dir(directory);
    }
    Ok(())
}

pub fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<Command> {
    let arguments = arguments.into_iter().collect::<Vec<_>>();
    let Some(first) = arguments.first() else {
        return Ok(Command::Edit(Vec::new()));
    };

    let only = |name: &str| -> Result<()> {
        ensure!(arguments.len() == 1, "{name} does not accept arguments");
        Ok(())
    };
    if first == "--help" || first == "-h" {
        only("--help")?;
        return Ok(Command::Help);
    }
    if first == "--version" || first == "-V" {
        only("--version")?;
        return Ok(Command::Version);
    }
    if first == "--smoke" {
        only("--smoke")?;
        return Ok(Command::Smoke);
    }

    let mut paths = Vec::new();
    let mut positional_only = false;
    for argument in arguments {
        if !positional_only && argument == "--" {
            positional_only = true;
            continue;
        }
        if !positional_only && argument.to_string_lossy().starts_with('-') {
            bail!("unknown option: {}", argument.to_string_lossy());
        }
        paths.push(argument.into());
    }
    if positional_only && paths.is_empty() {
        bail!("expected a file path after --");
    }

    Ok(Command::Edit(paths))
}

/// Makes every path absolute against `cwd` and drops exact duplicates while
/// keeping the first occurrence's order.
pub fn absolute_unique_paths(cwd: &Path, paths: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    ensure!(
        cwd.is_absolute(),
        "startup cwd must be absolute: {}",
        cwd.display()
    );
    let mut seen = HashSet::new();
    paths
        .into_iter()
        .map(|input| {
            let joined = if input.is_absolute() {
                input.clone()
            } else {
                cwd.join(&input)
            };
            std::path::absolute(&joined).with_context(|| {
                format!(
                    "failed to make {} absolute from {}",
                    input.display(),
                    cwd.display()
                )
            })
        })
        .filter(|path| match path {
            Ok(path) => seen.insert(path.clone()),
            Err(_) => true,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_strings(arguments: &[&str]) -> Result<Command> {
        parse(arguments.iter().map(OsString::from))
    }

    #[test]
    fn parses_modes_and_paths() {
        assert_eq!(parse_strings(&[]).unwrap(), Command::Edit(Vec::new()));
        assert_eq!(parse_strings(&["--smoke"]).unwrap(), Command::Smoke);
        assert_eq!(parse_strings(&["--version"]).unwrap(), Command::Version);
        assert_eq!(parse_strings(&["-h"]).unwrap(), Command::Help);
        assert_eq!(
            parse_strings(&["a.txt", "--", "-dash.txt"]).unwrap(),
            Command::Edit(vec![PathBuf::from("a.txt"), PathBuf::from("-dash.txt")])
        );
    }

    #[test]
    fn rejects_unknown_options() {
        assert!(parse_strings(&["--nope"]).is_err());
        assert!(parse_strings(&["--smoke", "x"]).is_err());
        assert!(parse_strings(&["--"]).is_err());
    }

    #[test]
    fn makes_paths_absolute_and_removes_exact_duplicates() {
        let cwd = std::env::temp_dir();
        let paths = absolute_unique_paths(
            &cwd,
            vec![
                PathBuf::from("a.txt"),
                cwd.join("a.txt"),
                PathBuf::from("b/../a.txt"),
            ],
        )
        .unwrap();
        assert_eq!(paths, vec![cwd.join("a.txt"), cwd.join("b/../a.txt")]);
    }
}
