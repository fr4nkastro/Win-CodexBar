//! Per-account usage fetch: read a managed account's own credentials, then
//! reuse the already-public `ClaudeOAuthFetcher::fetch_with_access_token`
//! (design decision D5 / divergence #3 from the proposal) — no CLI probe, no
//! `CLAUDE_CONFIG_DIR` subprocess for usage.
//!
//! Deliberately does not refresh an expired access token itself: rotating a
//! managed account's refresh token from an ad hoc usage poll — independent of
//! the app's normal refresh-lane scheduling — is exactly the race the
//! proposal's Risk 1 (refresh-token rotation) warns against. Per design
//! decision D5, refresh scheduling for managed accounts belongs to the
//! `ProviderId::Claude` refresh lane wired in PR2
//! (`commands/providers.rs::refresh_claude_account_lanes`), not here.

use std::path::Path;

use crate::core::{ProviderError, ProviderFetchResult, RateWindow};
use crate::providers::claude::ClaudeOAuthFetcher;

use super::models::{ClaudeAccountUsageSnapshot, UsageWindowSnapshot};

/// Fetch a usage snapshot for the managed (or ambient) account whose
/// credentials live at `dir`.
pub async fn fetch_snapshot(dir: &Path) -> Result<ClaudeAccountUsageSnapshot, ProviderError> {
    let credentials = crate::providers::claude::read_credentials_in(dir)?;
    let fetcher = ClaudeOAuthFetcher::new();
    let result = fetcher
        .fetch_with_access_token(&credentials.access_token)
        .await?;
    Ok(usage_snapshot_from_fetch_result(&result))
}

fn window_snapshot(window: &RateWindow) -> UsageWindowSnapshot {
    UsageWindowSnapshot::new(
        window.used_percent,
        window.resets_at,
        window
            .window_minutes
            .map(|minutes| i64::from(minutes) * 60)
            .unwrap_or(0),
    )
}

/// Pure mapping from the generic provider `ProviderFetchResult` into the
/// per-account stored shape (`window_minutes * 60` -> `limitWindowSeconds`,
/// per design).
fn usage_snapshot_from_fetch_result(result: &ProviderFetchResult) -> ClaudeAccountUsageSnapshot {
    ClaudeAccountUsageSnapshot {
        email: result.usage.account_email.clone(),
        org_id: None,
        plan: result.usage.login_method.clone(),
        primary_window: Some(window_snapshot(&result.usage.primary)),
        secondary_window: result.usage.secondary.as_ref().map(window_snapshot),
        updated_at: result.usage.updated_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::UsageSnapshot;

    fn fetch_result(primary_pct: f64, secondary: Option<(f64, u32)>) -> ProviderFetchResult {
        let primary = RateWindow::with_details(primary_pct, Some(300), None, None);
        let mut usage = UsageSnapshot::new(primary)
            .with_email("a@example.com")
            .with_login_method("Claude Pro");
        if let Some((pct, minutes)) = secondary {
            usage = usage.with_secondary(RateWindow::with_details(pct, Some(minutes), None, None));
        }
        ProviderFetchResult::new(usage, "oauth")
    }

    #[test]
    fn maps_window_minutes_to_limit_window_seconds() {
        let result = fetch_result(10.0, Some((20.0, 10_080)));
        let snapshot = usage_snapshot_from_fetch_result(&result);
        assert_eq!(
            snapshot
                .primary_window
                .as_ref()
                .unwrap()
                .limit_window_seconds,
            300 * 60
        );
        assert_eq!(
            snapshot
                .secondary_window
                .as_ref()
                .unwrap()
                .limit_window_seconds,
            10_080 * 60
        );
        assert_eq!(snapshot.email.as_deref(), Some("a@example.com"));
        assert_eq!(snapshot.plan.as_deref(), Some("Claude Pro"));
    }

    #[test]
    fn missing_window_minutes_maps_to_zero_limit_window_seconds() {
        let primary = RateWindow::new(5.0);
        let usage = UsageSnapshot::new(primary);
        let result = ProviderFetchResult::new(usage, "oauth");
        let snapshot = usage_snapshot_from_fetch_result(&result);
        assert_eq!(
            snapshot
                .primary_window
                .as_ref()
                .unwrap()
                .limit_window_seconds,
            0
        );
        assert!(snapshot.secondary_window.is_none());
    }

    /// Two managed accounts with different subscriptions each report their
    /// own usage values independently (spec: "Usage renders per account").
    /// Exercised at the pure-mapping level; the real per-account HTTP/refresh
    /// path is covered by the manual Windows checklist (design "Testing
    /// Strategy").
    #[test]
    fn two_accounts_map_to_independent_snapshots() {
        let a = usage_snapshot_from_fetch_result(&fetch_result(10.0, Some((20.0, 10_080))));
        let b = usage_snapshot_from_fetch_result(&fetch_result(90.0, Some((5.0, 10_080))));
        assert_ne!(
            a.primary_window.unwrap().used_percent,
            b.primary_window.unwrap().used_percent
        );
    }
}
