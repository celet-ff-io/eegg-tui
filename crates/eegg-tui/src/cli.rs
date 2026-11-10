//! Command-line surface (English-only help; the luatalk-cli bin model).

use clap::{
    Parser,
    builder::{
        Styles,
        styling::{AnsiColor, Effects},
    },
};
use clap_verbosity_flag::{Verbosity, WarnLevel};

static STYLES: Styles = Styles::styled()
    .header(AnsiColor::Yellow.on_default().effects(Effects::BOLD))
    .usage(AnsiColor::Yellow.on_default().effects(Effects::BOLD))
    .literal(AnsiColor::Green.on_default().effects(Effects::BOLD))
    .placeholder(AnsiColor::Red.on_default());

/// eegg - the zconn terminal interface. Spawns a core process over
/// stdio protobuf (one interface owns exactly one core); quitting
/// tears the core down.
#[derive(Debug, Parser)]
#[command(version)]
#[command(styles = STYLES.clone())]
pub struct Cli {
    /// Core command line to spawn, shlex-split.
    ///
    /// Resolution order is this flag first, then the `EEGG__CORE`
    /// environment variable, then `"zconned"` looked up through PATH.
    #[arg(long, value_name = "CMDLINE")]
    pub core: Option<String>,

    /// Relay URL, handed to the spawned core as `--relay <URL>` on its
    /// argv (the single pass-through form). Absent = the core runs
    /// stdio-only (records stay local; nothing reaches a relay).
    #[arg(long, value_name = "URL")]
    pub relay: Option<String>,

    /// Log file (the black-box). Defaults to `eegg.log` under the XDG
    /// state dir (`~/.local/state/eegg/`).
    #[arg(long, value_name = "PATH")]
    pub log_file: Option<std::path::PathBuf>,

    #[command(flatten)]
    pub verbose: Verbosity<WarnLevel>,
}
