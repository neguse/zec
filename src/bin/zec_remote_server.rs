//! The pinned Zed remote server, packaged under a zec-specific executable name.

use std::{io::Write as _, path::PathBuf};

use clap::Parser;
use remote_server::Commands;

#[derive(Parser)]
#[command(disable_version_flag = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
    /// Internal SSH/Git askpass bridge used by the Zed remote project.
    #[arg(long, hide = true)]
    askpass: Option<String>,
    /// Internal crash-handler process socket.
    #[arg(long, hide = true)]
    crash_handler: Option<PathBuf>,
    /// Print the project shell environment for remote process activation.
    #[arg(long, hide = true)]
    printenv: bool,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if let Some(socket_path) = &cli.askpass {
        askpass::main(socket_path);
        return Ok(());
    }
    if let Some(socket) = &cli.crash_handler {
        crashes::crash_server(socket.as_path(), paths::logs_dir().clone());
        return Ok(());
    }
    if cli.printenv {
        util::shell_env::print_env();
        return Ok(());
    }

    let Some(command) = cli.command else {
        std::io::stderr()
            .write_all(b"usage: zec-remote-server <run|proxy|version>\n")
            .ok();
        std::process::exit(1);
    };
    let result = remote_server::run(command);
    if let Err(error) = &result
        && let Some(error) = error.downcast_ref::<remote_server::ExecuteProxyError>()
    {
        std::io::stderr()
            .write_fmt(format_args!("{error:#}\n"))
            .ok();
        std::process::exit(error.to_exit_code());
    }
    result
}
