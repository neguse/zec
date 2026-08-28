//! Command-line surface: the argument grammar, including `zec probe`.

use std::{env, ffi::OsString, path::PathBuf};

use anyhow::{Context as _, Result, bail, ensure};
use serde::Deserialize;

use crate::{remote_session, update};

pub(crate) const USAGE: &str = "Usage: zec [DIRECTORY | FILE ...]\n       zec --smoke\n       zec --version\n       zec remote ssh HOST [--user USER] [--port PORT] [--arg ARG]... [--timeout SECONDS] [--nickname NAME] [--no-upload] ABSOLUTE_PATH...\n       zec remote wsl DISTRO [--user USER] ABSOLUTE_PATH...\n       zec remote container NAME [--id ID] [--user USER] [--podman|--docker] [--env NAME=VALUE]... [--no-upload] ABSOLUTE_PATH...\n       zec update check [--manifest PATH|HTTPS_URL]\n       zec update download --output PATH [--manifest PATH|HTTPS_URL]\n       zec update apply [--manifest PATH|HTTPS_URL]\n       zec update verify --binary PATH [--manifest PATH|HTTPS_URL]\n\nKeys: F1/Ctrl-Shift-P commands, Ctrl-Shift-A Agent, Ctrl-Alt-C collaboration, Ctrl-Enter inline assistant, Alt-\\ show prediction, Alt-L/K/J accept prediction/all/word/line, Ctrl-Alt-Shift-E toggle predictions, Ctrl-Shift-V Markdown preview, Ctrl-Shift-X extensions, Ctrl-Alt-T/I theme/icon theme, Ctrl-, settings, Ctrl-Alt-, keymap, F3/Ctrl-` terminal, Ctrl-Shift-` new terminal, Ctrl-Shift-G Git, Ctrl-Shift-B tasks, Ctrl-Alt-B rerun task, F5 debug, Ctrl-Shift-D debugger, Ctrl-F9 breakpoint, Ctrl/Shift-F5 continue/stop, Ctrl-F6 pause, Alt-F10/F11 step over/in, Alt-Shift-F11 step out, Ctrl-Shift-R debug REPL, .ipynb Notebook: Up/Down cells, Enter edit, Ctrl/Shift-Enter run, b/m add, dd delete, i interrupt, r restart, R run all, F4 terminal capabilities, F7 project panel, F9 outline panel, F10/Shift-F10 split right/down, Ctrl-Alt-Arrows focus panes, Ctrl-Alt-Shift-Arrows move tabs, F11/Ctrl-F11/Shift-F11 fold/fold all/unfold all, Alt-Z soft wrap, Ctrl-: inlay hints, Shift-Alt-Up/Down add cursors, Ctrl-D/Ctrl-Shift-L select next/all occurrences, Ctrl-Space/Alt-/ completion, F2 hover, F6 rename, F8 diagnostics, F12 definition, Alt-F12 type definition, Shift-F12 references, Ctrl-. code actions, Shift-Alt-F format, Ctrl-Alt-F format selection, Ctrl-T symbols, Alt-Left/Right history, Ctrl-N new, Ctrl-O open, Ctrl-P quick open, Alt-F project search, Ctrl-W close tab, Ctrl-PgUp/PgDn tabs, Alt-PgUp/PgDn scroll, Ctrl-C copy, Ctrl-X cut, Ctrl-F find, Ctrl-H replace, Ctrl-G line, Ctrl-R reload, Ctrl-S save, Ctrl-Q quit, Ctrl-Z/Y undo/redo";

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Command {
    Edit(Vec<PathBuf>),
    Remote(remote_session::RemoteRequest),
    RepositoryProbe(RepositoryProbe),
    LanguageProbe(LanguageProbe),
    Update(update::UpdateCommand),
    Smoke,
    Version,
    Help,
}
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum RepositoryProbe {
    RootIdentity {
        root: PathBuf,
        inputs: Vec<RepositoryRootInput>,
    },
    OutsideTrace(PathBuf),
    ProjectSearch {
        root: PathBuf,
        query: String,
    },
    StaleResult(PathBuf),
    SearchFailure(PathBuf),
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum LanguageProbe {
    LanguageService {
        root: PathBuf,
        file: PathBuf,
    },
    SettingsReload {
        root: PathBuf,
        file: PathBuf,
    },
    LspFailure {
        root: PathBuf,
        file: PathBuf,
        scenario: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepositoryRootInput {
    pub(crate) id: String,
    pub(crate) cwd: PathBuf,
    pub(crate) argument: Option<PathBuf>,
}

/// Redirects every Zed-side user directory (config, data, logs, databases)
/// under `$ZEC_DATA_DIR` before anything resolves them. This is the one
/// isolation mechanism that behaves identically on every platform; the XDG
/// variables only cover Unix.
pub(crate) fn apply_data_dir_override() -> Result<()> {
    if let Some(directory) = env::var_os("ZEC_DATA_DIR") {
        let directory = directory
            .to_str()
            .context("ZEC_DATA_DIR must be valid UTF-8")?;
        paths::set_custom_data_dir(directory);
    }
    Ok(())
}

pub(crate) fn parse_command(arguments: impl IntoIterator<Item = OsString>) -> Result<Command> {
    let arguments = arguments.into_iter().collect::<Vec<_>>();
    let Some(first) = arguments.first() else {
        return Ok(Command::Edit(Vec::new()));
    };

    if first == "--help" || first == "-h" {
        if arguments.len() > 1 {
            bail!("--help does not accept arguments");
        }
        return Ok(Command::Help);
    }
    if first == "--version" || first == "-V" {
        if arguments.len() > 1 {
            bail!("--version does not accept arguments");
        }
        return Ok(Command::Version);
    }
    if first == "update" {
        return Ok(Command::Update(update::parse_update_command(
            &arguments[1..],
        )?));
    }
    if first == "remote" {
        return Ok(Command::Remote(remote_session::parse_remote_command(
            &arguments[1..],
        )?));
    }
    if first == "--smoke" {
        if arguments.len() > 1 {
            bail!("--smoke does not accept arguments");
        }
        return Ok(Command::Smoke);
    }
    if first == "probe" {
        let case = arguments
            .get(1)
            .and_then(|case| case.to_str())
            .context("probe requires a UTF-8 case name")?;
        let one_path = |name: &str| -> Result<PathBuf> {
            ensure!(
                arguments.len() == 3,
                "probe {name} requires exactly one path"
            );
            Ok(arguments[2].clone().into())
        };
        return Ok(match case {
            "root-identity" => {
                ensure!(
                    arguments.len() == 4,
                    "probe root-identity requires ROOT INPUTS_JSON"
                );
                let encoded = arguments[3]
                    .to_str()
                    .context("root-identity inputs must be UTF-8 JSON")?;
                let inputs =
                    serde_json::from_str(encoded).context("parse root-identity inputs JSON")?;
                Command::RepositoryProbe(RepositoryProbe::RootIdentity {
                    root: arguments[2].clone().into(),
                    inputs,
                })
            }
            "outside-trace" => {
                Command::RepositoryProbe(RepositoryProbe::OutsideTrace(one_path(case)?))
            }
            "stale-result" => {
                Command::RepositoryProbe(RepositoryProbe::StaleResult(one_path(case)?))
            }
            "search-failure" => {
                Command::RepositoryProbe(RepositoryProbe::SearchFailure(one_path(case)?))
            }
            "project-search" => {
                ensure!(
                    arguments.len() == 4,
                    "probe project-search requires ROOT QUERY"
                );
                let query = arguments[3]
                    .clone()
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("project-search query must be UTF-8"))?;
                Command::RepositoryProbe(RepositoryProbe::ProjectSearch {
                    root: arguments[2].clone().into(),
                    query,
                })
            }
            "language-service" => {
                ensure!(
                    arguments.len() == 4,
                    "probe language-service requires ROOT FILE"
                );
                Command::LanguageProbe(LanguageProbe::LanguageService {
                    root: arguments[2].clone().into(),
                    file: arguments[3].clone().into(),
                })
            }
            "settings-reload" => {
                ensure!(
                    arguments.len() == 4,
                    "probe settings-reload requires ROOT FILE"
                );
                Command::LanguageProbe(LanguageProbe::SettingsReload {
                    root: arguments[2].clone().into(),
                    file: arguments[3].clone().into(),
                })
            }
            "lsp-failure" => {
                ensure!(
                    arguments.len() == 5,
                    "probe lsp-failure requires ROOT FILE SCENARIO"
                );
                Command::LanguageProbe(LanguageProbe::LspFailure {
                    root: arguments[2].clone().into(),
                    file: arguments[3].clone().into(),
                    scenario: arguments[4]
                        .clone()
                        .into_string()
                        .map_err(|_| anyhow::anyhow!("lsp-failure scenario must be UTF-8"))?,
                })
            }
            _ => bail!("unknown probe case: {case}"),
        });
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
