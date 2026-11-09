//! XDG resolution — the nvim convention mirrored (principal ruling):
//! config in `~/.config/eegg`, private data in `~/.local/share/eegg`,
//! logs in `~/.local/state/eegg`, scratch in `~/.cache/eegg`.
//!
//! M0 creates ONLY the log file's directory (on demand); the other
//! roots are resolved but never created until a feature needs them —
//! nvim creates nothing until it writes.

/// User config root (`~/.config/eegg`). The config face is a future
/// door; placement fixed here.
pub fn config_dir() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|d| d.join("eegg"))
}

/// Private data root (`~/.local/share/eegg`). Future home of the
/// private-key cache: PLAINTEXT by ruling (a compromised system loses
/// everything anyway), file protection 0600 (interface-side note, B11).
pub fn data_dir() -> Option<std::path::PathBuf> {
    dirs::data_local_dir().map(|d| d.join("eegg"))
}

/// Log root (`~/.local/state/eegg`) — the black-box log lives here.
pub fn state_dir() -> Option<std::path::PathBuf> {
    dirs::state_dir().map(|d| d.join("eegg"))
}

/// Scratch root (`~/.cache/eegg`). Unused in M0.
pub fn cache_dir() -> Option<std::path::PathBuf> {
    dirs::cache_dir().map(|d| d.join("eegg"))
}

/// The black-box log file: `--log-file` override, else
/// `state_dir()/eegg.log`. Env failure is LOUD (C21) — M0 refuses to
/// start un-diagnosable sessions.
pub fn resolve_log_file(
    override_path: Option<&std::path::Path>,
) -> miette::Result<std::path::PathBuf> {
    if let Some(p) = override_path {
        return Ok(p.to_owned());
    }
    let dir = state_dir()
        .ok_or_else(|| miette::miette!("no XDG state dir available for the log file"))?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| miette::miette!("cannot create log dir {}: {e}", dir.display()))?;
    Ok(dir.join("eegg.log"))
}
