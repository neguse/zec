mod app;
mod cli;
mod features;
mod terminal;
mod zed;

use anyhow::Result;

fn main() -> Result<()> {
    cli::apply_data_dir_override()?;
    match cli::parse(std::env::args_os().skip(1))? {
        cli::Command::Edit(paths) => app::run(paths),
        cli::Command::Smoke => {
            zed::smoke();
            Ok(())
        }
        cli::Command::Version => {
            println!("zec {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        cli::Command::Help => {
            println!("{}", cli::USAGE);
            Ok(())
        }
    }
}
