//! eegg — bin entry (thin; the luatalk-cli model: logging here, logic
//! in the library).

use clap::Parser as _;

use eegg_tui::{cli::Cli, run};

fn main() -> miette::Result<()> {
    let cli = Cli::parse();
    run(&cli)
}
