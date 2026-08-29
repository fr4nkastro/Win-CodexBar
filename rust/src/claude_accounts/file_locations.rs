//! Path resolution for Claude account storage and the ambient Claude Code
//! state files.
//!
//! Mirrors `codex_accounts::file_locations`'s shape (app-support root +
//! thread_local test overrides), adapted to CodexBar's `%config%/CodexBar`
//! convention and to Claude Code's two ambient files: the config directory
//! (`CLAUDE_CONFIG_DIR`, default `~/.claude`) holding `.credentials.json`, and
//! the separate home-relative `~/.claude.json` holding onboarding/account
//! state (including `oauthAccount`, per the corrected active-detection spec).

use std::path::PathBuf;

thread_local! {
    static APP_SUPPORT_OVERRIDE: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
    static AMBIENT_CONFIG_DIR_OVERRIDE: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
    static AMBIENT_CLAUDE_JSON_OVERRIDE: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Base directory holding the Claude account store (accounts.json,
/// managed-configs).
pub fn app_support_directory() -> PathBuf {
    APP_SUPPORT_OVERRIDE
        .with(|cell| cell.borrow().clone())
        .unwrap_or_else(|| {
            dirs::config_dir()
                .map(|dir| dir.join("CodexBar").join("claude-accounts"))
                .unwrap_or_else(|| PathBuf::from(".").join("claude-accounts"))
        })
}

/// Override the app support root (tests / shell). Returns the previous value.
pub fn with_app_support_directory(path: PathBuf) -> Option<PathBuf> {
    APP_SUPPORT_OVERRIDE.with(|cell| {
        let previous = cell.borrow().clone();
        *cell.borrow_mut() = Some(path);
        previous
    })
}

pub fn clear_app_support_directory_override() {
    APP_SUPPORT_OVERRIDE.with(|cell| *cell.borrow_mut() = None);
}

pub fn accounts_file() -> PathBuf {
    app_support_directory().join("accounts.json")
}

pub fn snapshots_file() -> PathBuf {
    app_support_directory().join("snapshots.json")
}

pub fn managed_configs_directory() -> PathBuf {
    app_support_directory().join("managed-configs")
}

pub fn credentials_backups_directory() -> PathBuf {
    app_support_directory().join("credentials-backups")
}

/// The environment (ambient) Claude Code config directory: `CLAUDE_CONFIG_DIR`
/// when set (mirrors `cost_scanner.rs::get_claude_projects_dir`), else
/// `~/.claude`.
pub fn ambient_claude_config_dir() -> PathBuf {
    if let Some(overridden) = AMBIENT_CONFIG_DIR_OVERRIDE.with(|cell| cell.borrow().clone()) {
        return overridden;
    }
    if let Ok(claude_config) = std::env::var("CLAUDE_CONFIG_DIR") {
        let trimmed = claude_config.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
}

/// Override the ambient config dir (tests / shell).
pub fn with_ambient_claude_config_dir(path: PathBuf) {
    AMBIENT_CONFIG_DIR_OVERRIDE.with(|cell| *cell.borrow_mut() = Some(path));
}

pub fn clear_ambient_claude_config_dir_override() {
    AMBIENT_CONFIG_DIR_OVERRIDE.with(|cell| *cell.borrow_mut() = None);
}

/// Claude Code's top-level onboarding/account state file. Unlike
/// `.credentials.json`, this file is not relocated by `CLAUDE_CONFIG_DIR` — it
/// always lives at the home directory root.
pub fn ambient_claude_json_path() -> PathBuf {
    if let Some(overridden) = AMBIENT_CLAUDE_JSON_OVERRIDE.with(|cell| cell.borrow().clone()) {
        return overridden;
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude.json")
}

/// Override the ambient `.claude.json` path (tests / shell).
pub fn with_ambient_claude_json_path(path: PathBuf) {
    AMBIENT_CLAUDE_JSON_OVERRIDE.with(|cell| *cell.borrow_mut() = Some(path));
}

pub fn clear_ambient_claude_json_path_override() {
    AMBIENT_CLAUDE_JSON_OVERRIDE.with(|cell| *cell.borrow_mut() = None);
}

/// Ensure required directories exist.
pub fn ensure_directories() -> std::io::Result<()> {
    std::fs::create_dir_all(app_support_directory())?;
    std::fs::create_dir_all(managed_configs_directory())?;
    std::fs::create_dir_all(credentials_backups_directory())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_support_default_resolves_to_config_dir() {
        clear_app_support_directory_override();
        let dir = app_support_directory();
        assert!(dir.to_string_lossy().contains("claude-accounts"));
    }

    #[test]
    fn app_support_override_isolates_from_default() {
        let dir = tempfile::tempdir().unwrap();
        with_app_support_directory(dir.path().to_path_buf());
        assert_eq!(app_support_directory(), dir.path());
        assert_eq!(accounts_file(), dir.path().join("accounts.json"));
        assert_eq!(snapshots_file(), dir.path().join("snapshots.json"));
        assert_eq!(
            managed_configs_directory(),
            dir.path().join("managed-configs")
        );
        assert_eq!(
            credentials_backups_directory(),
            dir.path().join("credentials-backups")
        );
        clear_app_support_directory_override();
    }

    #[test]
    fn ambient_config_dir_override_wins_over_default() {
        let dir = tempfile::tempdir().unwrap();
        with_ambient_claude_config_dir(dir.path().to_path_buf());
        assert_eq!(ambient_claude_config_dir(), dir.path());
        clear_ambient_claude_config_dir_override();
    }

    #[test]
    fn ambient_claude_json_override_wins_over_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude.json");
        with_ambient_claude_json_path(path.clone());
        assert_eq!(ambient_claude_json_path(), path);
        clear_ambient_claude_json_path_override();
    }

    #[test]
    fn overrides_do_not_bleed_into_each_other() {
        let app_dir = tempfile::tempdir().unwrap();
        let ambient_dir = tempfile::tempdir().unwrap();
        with_app_support_directory(app_dir.path().to_path_buf());
        with_ambient_claude_config_dir(ambient_dir.path().to_path_buf());

        assert_eq!(app_support_directory(), app_dir.path());
        assert_eq!(ambient_claude_config_dir(), ambient_dir.path());
        assert_ne!(app_support_directory(), ambient_claude_config_dir());

        clear_app_support_directory_override();
        clear_ambient_claude_config_dir_override();
    }
}
