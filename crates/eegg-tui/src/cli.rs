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
/// stdio protobuf (strictly 1:1); quitting tears the core down.
#[derive(Debug, Parser)]
#[command(version)]
#[command(styles = STYLES.clone())]
pub struct Cli {
    /// Core command line to spawn, shlex-split.
    ///
    /// Resolution order is this flag first, then the `EEGG__CORE`
    /// environment variable, then `"fake-core"` looked up through PATH.
    #[arg(long, value_name = "CMDLINE")]
    pub core: Option<String>,

    #[command(flatten)]
    pub verbose: Verbosity<WarnLevel>,
}
