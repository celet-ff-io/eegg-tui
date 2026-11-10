//! XDG resolution — the nvim convention mirrored (principal ruling):
//! config in `~/.config/eegg`, private data in `~/.local/share/eegg`,
//! logs in `~/.local/state/eegg`, scratch in `~/.cache/eegg`.
//!
//! This milestone creates ONLY the log file's directory (on demand);
//! the other roots are resolved but never created until a feature
//! needs them — nvim creates nothing until it writes.

pub type Result<T> = core::result::Result<T, Error>;

/// Errors of the path resolution and the log-file setup.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("no XDG state dir available for the log file")]
    NoStateDir,
    #[error("cannot create log dir {dir}: {source}")]
    CreateDir {
        dir: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// User config root (`~/.config/eegg`). The config face is a future
/// door; placement fixed here.
pub fn config_dir() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|d| d.join("eegg"))
}

/// Private data root (`~/.local/share/eegg`). Future home of the
/// private-key cache: stored PLAINTEXT by ruling (a compromised
/// system loses everything anyway), with 0600 file protection on
/// creation.
pub fn data_dir() -> Option<std::path::PathBuf> {
    dirs::data_local_dir().map(|d| d.join("eegg"))
}

/// Log root (`~/.local/state/eegg`) — the black-box log lives here.
pub fn state_dir() -> Option<std::path::PathBuf> {
    dirs::state_dir().map(|d| d.join("eegg"))
}

/// Scratch root (`~/.cache/eegg`). Unused for now.
pub fn cache_dir() -> Option<std::path::PathBuf> {
    dirs::cache_dir().map(|d| d.join("eegg"))
}

/// The black-box log file: `--log-file` override, else
/// `state_dir()/eegg.log`. Env failure is LOUD — the interface
/// refuses to start un-diagnosable sessions.
pub fn resolve_log_file(
    override_path: Option<&std::path::Path>,
) -> Result<std::path::PathBuf> {
    if let Some(p) = override_path {
        return Ok(p.to_owned());
    }
    let dir = state_dir().ok_or(Error::NoStateDir)?;
    std::fs::create_dir_all(&dir).map_err(|source| Error::CreateDir {
        dir: dir.clone(),
        source,
    })?;
    Ok(dir.join("eegg.log"))
}
