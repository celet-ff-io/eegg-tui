//! eegg — bin entry (thin; the luatalk-cli model: logging here, logic
//! in the library).

use clap::Parser as _;

use eegg_tui::{cli::Cli, license::THIRD_PARTY_LICENSES, run};

fn main() -> miette::Result<()> {
    let cli = Cli::parse();
    // `--license` prints and exits before anything else (no core, no
    // TUI, no logging setup) — stdout is the whole product here.
    if cli.license {
        use std::io::Write as _;
        let mut out = std::io::stdout().lock();
        let done = out
            .write_all(THIRD_PARTY_LICENSES.as_bytes())
            .and_then(|()| out.flush());
        // A reader exiting early (`| head`) closes the pipe: die
        // quietly with the shell convention, not a panic report.
        match done {
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
            Err(e) => return Err(miette::miette!("failed printing to stdout: {e}")),
            Ok(()) => {}
        }
        return Ok(());
    }
    run(&cli)
}
