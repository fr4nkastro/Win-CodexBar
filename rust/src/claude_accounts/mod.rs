//! Multi-account Claude Code support for CodexBar.
//!
//! Mirrors `codex_accounts`' shape (see its module doc comment) minus
//! `codex_desktop.rs` — Claude Code has no MSIX session state to preserve.
//!
//! A `ClaudeAccount` lives in either the ambient config directory
//! (`CLAUDE_CONFIG_DIR`, default `~/.claude`) or an app-managed directory
//! under `managed-configs/`. Switching swaps only the `claudeAiOauth` key of
//! the ambient `.credentials.json`, preserving `mcpOAuth` byte-for-byte
//! (design decision D2 — the key structural divergence from `codex_accounts`,
//! which copies the whole `auth.json`).

pub mod credentials_merge;
pub mod file_locations;
pub mod identity;
pub mod login_runner;
pub mod models;

pub use identity::{
    AmbientClaudeIdentity, AmbientOauthAccount, ClaudeIdentity, ClaudeIdentityError,
    active_account_id, derive_account_uuid, parse_auth_status_json, stable_discovered_id,
};
pub use login_runner::{ClaudeLoginOutcome, ClaudeLoginResult, ManagedLoginProcess};
pub use models::{
    ClaudeAccount, ClaudeAccountSource, ClaudeAccountUsageSnapshot, RemovedAccountIdentity,
    UsageWindowSnapshot, utc_now,
};
