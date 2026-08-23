//! The debug log Fero writes while scraping.
//!
//! Apart from the browser code because everything writes to it: the
//! subscription checks, the migration and the window handling alike.

use super::*;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WebnovelDebugLogResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    content: String,
}

/// Returns the debug-log path and its (tail) content for the UI.
pub(super) fn build_webnovel_debug_log_response() -> WebnovelDebugLogResponse {
    let path = debug_log_path();
    let content = path
        .as_ref()
        .and_then(|p| fs::read_to_string(p).ok())
        .unwrap_or_default();
    // Only the last ~400 lines are useful and keep the payload small.
    let tail: Vec<&str> = content.lines().rev().take(400).collect();
    let content = tail.into_iter().rev().collect::<Vec<_>>().join("\n");
    WebnovelDebugLogResponse {
        path: path.map(|p| p.to_string_lossy().to_string()),
        content,
    }
}

/// Opens the debug-log file in the OS default application.
pub(super) fn build_open_debug_log_response() -> WebnovelSimpleResponse {
    let Some(path) = debug_log_path() else {
        return WebnovelSimpleResponse::error("Log-Pfad nicht ermittelbar.");
    };
    // Make sure the file exists so the OS has something to open.
    if !path.exists() {
        debug_log("open-debug-log: Datei angelegt (war leer)");
    }
    let path_str = path.to_string_lossy().to_string();

    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(&path_str).spawn();
    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("cmd")
        .args(["/C", "start", "", &path_str])
        .spawn();
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let result = std::process::Command::new("xdg-open")
        .arg(&path_str)
        .spawn();

    match result {
        Ok(_) => WebnovelSimpleResponse::ok(),
        Err(error) => WebnovelSimpleResponse::error(format!("Öffnen fehlgeschlagen: {error}")),
    }
}

/// Debug-log path (`~/.fero/webnovel_debug.log`).
pub(super) fn debug_log_path() -> Option<PathBuf> {
    let home = env::var_os("HOME").or_else(|| env::var_os("USERPROFILE"))?;
    Some(PathBuf::from(home).join(".fero").join("webnovel_debug.log"))
}

/// Size at which the debug log is rotated to `<name>.1`.
const DEBUG_LOG_MAX_BYTES: u64 = 2 * 1024 * 1024;

/// The log records every URL the fetcher visits, i.e. a complete reading
/// history — it is therefore owner-readable only and rotated at
/// [`DEBUG_LOG_MAX_BYTES`] so it cannot grow without bound.
pub(crate) fn debug_log(message: &str) {
    let Some(path) = debug_log_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent);
    }

    // Rotate before appending: one previous generation is kept.
    if fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0) >= DEBUG_LOG_MAX_BYTES {
        let rotated = path.with_extension("log.1");
        let _ = fs::remove_file(&rotated);
        let _ = fs::rename(&path, &rotated);
    }

    let line = format!("[{}] {message}\n", unix_now());
    use std::io::Write;
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        restrict_to_owner(&path, false);
        let _ = file.write_all(line.as_bytes());
    }
}
