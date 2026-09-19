//! `get_safe_diagnostics` — a copy-friendly, secret-free diagnostics string
//! for bug reports. Contains only: app version/build, OS, update channel,
//! the log file's config-relative path, and the redacted log tail. The log
//! path is trimmed to its segments below the user profile so no username is
//! embedded, and the tail is redacted for both secrets and email addresses.
//! Never includes provider names, plans, account info, cookies, or tokens.

use super::*;

/// Last segments of `path` below the user profile (never the profile itself).
fn config_relative_path(path: &std::path::Path) -> String {
    let segments: Vec<std::ffi::OsString> =
        path.iter().map(std::borrow::ToOwned::to_owned).collect();
    let tail: Vec<String> = segments
        .iter()
        .rev()
        .take(3)
        .rev()
        .map(|s| s.to_string_lossy().to_string())
        .collect();
    if tail.len() == 3 {
        tail.join("/")
    } else {
        "unresolvable".to_string()
    }
}

#[tauri::command]
pub fn get_safe_diagnostics() -> String {
    let settings = Settings::load();
    let log_dir = codexbar::logging::log_file_path()
        .map(|p| config_relative_path(&p))
        .unwrap_or_else(|| "unresolvable".to_string());
    let log_tail =
        codexbar::logging::read_log_tail().unwrap_or_else(|| "log file unavailable".to_string());

    format!(
        "CodexBar diagnostics\n\
         -------------------\n\
         version: {} (build {})\n\
         os: {}\n\
         channel: {}\n\
         log file: {}\n\
         --- last log lines (redacted) ---\n\
         {}",
        env!("CARGO_PKG_VERSION"),
        option_env!("BUILD_NUMBER").unwrap_or("dev"),
        std::env::consts::OS,
        update_channel_label(settings.update_channel),
        log_dir,
        log_tail,
    )
}
