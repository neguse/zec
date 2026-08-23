#[path = "../../tests/alpha_1/fixture.rs"]
mod fixture;

use std::env;
use std::path::{Path, PathBuf};

fn usage() -> &'static str {
    "usage:\n  alpha_1_fixture write-oracles [--repo PATH]\n  alpha_1_fixture verify-oracles [--repo PATH]\n  alpha_1_fixture generate --root PATH\n  alpha_1_fixture verify --root PATH [--after-run 1..20]\n  alpha_1_fixture print-hashes [--repo PATH]"
}

fn take_value(args: &mut Vec<String>, name: &str) -> Result<Option<String>, String> {
    let Some(index) = args.iter().position(|arg| arg == name) else {
        return Ok(None);
    };
    if index + 1 >= args.len() {
        return Err(format!("{name} requires a value"));
    }
    args.remove(index);
    Ok(Some(args.remove(index)))
}

fn repo_arg(args: &mut Vec<String>) -> Result<PathBuf, String> {
    Ok(take_value(args, "--repo")?
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".")))
}

fn ensure_empty(args: &[String]) -> Result<(), String> {
    if args.is_empty() {
        Ok(())
    } else {
        Err(format!("unexpected arguments: {}", args.join(" ")))
    }
}

fn run() -> Result<(), String> {
    let mut args = env::args().skip(1).collect::<Vec<_>>();
    if args.is_empty() {
        return Err(usage().to_owned());
    }
    let command = args.remove(0);
    match command.as_str() {
        "write-oracles" => {
            let repo = repo_arg(&mut args)?;
            ensure_empty(&args)?;
            fixture::write_oracles(&repo)
        }
        "verify-oracles" => {
            let repo = repo_arg(&mut args)?;
            ensure_empty(&args)?;
            fixture::verify_oracles(&repo)
        }
        "generate" => {
            let root = take_value(&mut args, "--root")?
                .ok_or_else(|| "generate requires --root PATH".to_owned())?;
            ensure_empty(&args)?;
            let generated = fixture::generate(Path::new(&root))?;
            println!("{}", generated.summary_json());
            Ok(())
        }
        "verify" => {
            let root = take_value(&mut args, "--root")?
                .ok_or_else(|| "verify requires --root PATH".to_owned())?;
            let after_run = take_value(&mut args, "--after-run")?
                .map(|value| {
                    value
                        .parse::<u8>()
                        .map_err(|_| "--after-run must be 1..20".to_owned())
                })
                .transpose()?;
            ensure_empty(&args)?;
            fixture::verify_fixture(Path::new(&root), after_run)
        }
        "print-hashes" => {
            let repo = repo_arg(&mut args)?;
            ensure_empty(&args)?;
            println!("{}", fixture::oracle_hashes_json(&repo)?);
            Ok(())
        }
        "-h" | "--help" | "help" => {
            println!("{}", usage());
            Ok(())
        }
        _ => Err(format!("unknown command {command:?}\n{}", usage())),
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("alpha_1_fixture: {error}");
        std::process::exit(2);
    }
}
