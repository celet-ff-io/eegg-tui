//! eegg — bin entry (thin; the luatalk-cli model: logging here, logic
//! in the library).

use clap::Parser as _;

use eegg_tui::{cli::Cli, run};

fn main() -> miette::Result<()> {
    let cli = Cli::parse();
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or(cli.verbose.log_level_filter().to_string()),
    )
    .init();
    run(&cli)
}
