//! The clipboard seam: a text clipboard that may be UNAVAILABLE (a
//! headless session, a locked-down compositor) — every call reports
//! its failure as a message, and the file channel (save/load) is the
//! always-works fallback.

/// Set the clipboard text. `Err` = a human-readable reason.
pub fn set(text: &str) -> Result<(), String> {
    arboard::Clipboard::new()
        .and_then(|mut c| c.set_text(text))
        .map_err(|e| format!("clipboard unavailable: {e}"))
}

/// Get the clipboard text. `Err` = a human-readable reason.
pub fn get() -> Result<String, String> {
    arboard::Clipboard::new()
        .and_then(|mut c| c.get_text())
        .map_err(|e| format!("clipboard unavailable: {e}"))
}
