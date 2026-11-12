//! The license surface: `eegg --license` prints the third-party
//! license bundle to stdout and exits (a terminal citizen prints
//! text; the TUI carries no license mode).
//!
//! The bundle is the committed artifact `third-party-licenses.txt`
//! at the repo root, rendered by `cargo about` from `about.txt.hbs`
//! over the dependency graph. Regenerate it whenever the dependency
//! set changes (the command is in the template header and the
//! README); embedding is compile-time (`include_str!`), so a stale
//! artifact ships only if nobody regenerates — the same keep-green
//! discipline as the rest of the licensing tooling.

/// The full bundle, verbatim.
pub const THIRD_PARTY_LICENSES: &str = include_str!("../../../third-party-licenses.txt");
