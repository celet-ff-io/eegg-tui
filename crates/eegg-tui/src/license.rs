//! The license surface: `eegg --license` prints the third-party
//! license page to stdout and exits (the TUI carries no license
//! mode; redirect stdout to a file and open it in a browser).
//!
//! The page is the committed artifact `license.html` at the repo
//! root, rendered by `cargo about` from the HTML template
//! `about.hbs` over the dependency graph. Regenerate it whenever
//! the dependency set changes (the command is in the README);
//! embedding is compile-time (`include_str!`), so a stale page
//! ships only if nobody regenerates — the same keep-green
//! discipline as the rest of the licensing tooling.

/// The full page, verbatim.
pub const LICENSE_HTML: &str = include_str!("../../../license.html");
