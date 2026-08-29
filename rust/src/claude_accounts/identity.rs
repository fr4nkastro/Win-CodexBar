//! Identity capture and active-account detection for Claude accounts.
//!
//! `claude auth status --json` is invoked exactly once, at add-account time
//! (see `account_manager::add_managed_account`), to capture a newly added
//! account's identity. Active-account detection at every other call site is
//! file-based only (spec: "Active-account detection" requirement) — no
//! `claude` subprocess is spawned on menu open, polling, or
//! `claude-accounts-updated` events.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;
use thiserror::Error;
use uuid::Uuid;

use super::models::ClaudeAccount;

/// Identity captured from `claude auth status --json` at add-time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaudeIdentity {
    pub email: Option<String>,
    pub org_id: Option<String>,
    pub org_name: Option<String>,
    pub subscription_type: Option<String>,
}

#[derive(Debug, Error)]
pub enum ClaudeIdentityError {
    #[error("failed to parse `claude auth status --json` output: {0}")]
    Parse(String),
}

/// Raw shape accepted from `claude auth status --json`. Field names are
/// deliberately flexible (top-level or nested under `organization`/`account`,
/// camelCase or snake_case) since the exact CLI schema was not verified
/// against a real Windows host as of this design (see design Open Questions);
/// this must be confirmed before shipping past PR1.
#[derive(Debug, Deserialize, Default)]
struct AuthStatusPayload {
    email: Option<String>,
    #[serde(rename = "emailAddress")]
    email_address: Option<String>,
    #[serde(rename = "organizationUuid")]
    organization_uuid: Option<String>,
    #[serde(rename = "orgId")]
    org_id: Option<String>,
    #[serde(rename = "organizationName")]
    organization_name: Option<String>,
    #[serde(rename = "orgName")]
    org_name: Option<String>,
    #[serde(rename = "subscriptionType")]
    subscription_type: Option<String>,
    plan: Option<String>,
    organization: Option<OrganizationPayload>,
    account: Option<AccountPayload>,
}

#[derive(Debug, Deserialize, Default)]
struct OrganizationPayload {
    uuid: Option<String>,
    id: Option<String>,
    name: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct AccountPayload {
    email: Option<String>,
    #[serde(rename = "emailAddress")]
    email_address: Option<String>,
}

fn normalized(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Parse `claude auth status --json` output into a [`ClaudeIdentity`]. Pure
/// computation: malformed JSON returns a typed error, never panics.
pub fn parse_auth_status_json(content: &str) -> Result<ClaudeIdentity, ClaudeIdentityError> {
    let payload: AuthStatusPayload =
        serde_json::from_str(content).map_err(|e| ClaudeIdentityError::Parse(e.to_string()))?;

    let email = normalized(payload.email.as_deref())
        .or_else(|| normalized(payload.email_address.as_deref()))
        .or_else(|| {
            payload
                .account
                .as_ref()
                .and_then(|a| normalized(a.email.as_deref()))
        })
        .or_else(|| {
            payload
                .account
                .as_ref()
                .and_then(|a| normalized(a.email_address.as_deref()))
        });

    let org_id = normalized(payload.organization_uuid.as_deref())
        .or_else(|| normalized(payload.org_id.as_deref()))
        .or_else(|| {
            payload
                .organization
                .as_ref()
                .and_then(|o| normalized(o.uuid.as_deref()))
        })
        .or_else(|| {
            payload
                .organization
                .as_ref()
                .and_then(|o| normalized(o.id.as_deref()))
        });

    let org_name = normalized(payload.organization_name.as_deref())
        .or_else(|| normalized(payload.org_name.as_deref()))
        .or_else(|| {
            payload
                .organization
                .as_ref()
                .and_then(|o| normalized(o.name.as_deref()))
        });

    let subscription_type = normalized(payload.subscription_type.as_deref())
        .or_else(|| normalized(payload.plan.as_deref()));

    Ok(ClaudeIdentity {
        email,
        org_id,
        org_name,
        subscription_type,
    })
}

/// Namespace for deterministically derived discovered-account ids.
///
/// Never change this string: changing it re-keys every derived id and would
/// make already-listed discovered accounts unresolvable on a later IPC call.
/// Deliberately not shared with `codex_accounts`' own namespace constant
/// (design decision D6: copy the id-derivation pattern, do not extract a
/// shared helper).
const DISCOVERED_ACCOUNT_ID_NAMESPACE: &str = "codexbar:claude-account:v1";

/// Fold a stable key into a well-formed, deterministic `Uuid` via SHA-256.
///
/// The version nibble is set to `5` to denote the name-based family (not the
/// literal hash algorithm); the RFC 4122 variant bits are set so the value is
/// a round-trippable `Uuid`. Copies `codex_accounts::account_manager`'s
/// `derive_account_uuid` verbatim (same rationale: existing `sha2` dependency,
/// no new crate, `uuid`'s `v5` feature deliberately not enabled).
pub fn derive_account_uuid(key: &str) -> Uuid {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(DISCOVERED_ACCOUNT_ID_NAMESPACE.as_bytes());
    hasher.update([0x1f_u8]); // unit separator: removes namespace/key concat ambiguity
    hasher.update(key.as_bytes());
    let digest = hasher.finalize();

    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

/// First-available stable key: `org_id` -> `email` -> config dir.
fn stable_identity_key(config_dir: &Path, identity: &ClaudeIdentity) -> String {
    if let Some(value) = normalized(identity.org_id.as_deref()) {
        return format!("org:{}", value.to_lowercase());
    }
    if let Some(value) = normalized(identity.email.as_deref()) {
        return format!("email:{}", value.to_lowercase());
    }
    format!("dir:{}", standardized_dir_key(config_dir))
}

fn standardized_dir_key(config_dir: &Path) -> String {
    std::path::absolute(config_dir)
        .unwrap_or_else(|_| config_dir.to_path_buf())
        .to_string_lossy()
        .to_lowercase()
}

/// Deterministic id for an account discovered on disk.
///
/// Managed config directories created by the app (and externally created
/// ones) are named `<uuid>`; that name IS the id. Otherwise the id is derived
/// from the strongest available stable identity key. Pure computation.
pub fn stable_discovered_id(config_dir: &Path, identity: &ClaudeIdentity) -> Uuid {
    if let Some(id) = config_dir
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| Uuid::parse_str(name.trim()).ok())
    {
        return id;
    }
    derive_account_uuid(&stable_identity_key(config_dir, identity))
}

/// Ambient `~/.claude.json`'s `oauthAccount` object, matched against stored
/// accounts' `org_id`/`email_hint` (spec: "Primary match via oauthAccount").
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AmbientOauthAccount {
    pub email_address: Option<String>,
    pub organization_uuid: Option<String>,
}

/// The full file-based ambient state consulted by [`active_account_id`].
#[derive(Debug, Clone, Default)]
pub struct AmbientClaudeIdentity {
    pub oauth_account: Option<AmbientOauthAccount>,
    /// Ambient `.credentials.json`'s `claudeAiOauth.accessToken`, compared by
    /// exact string equality against each managed account's own token (spec:
    /// "Fallback match via access-token equality").
    pub access_token: Option<String>,
}

fn match_by_oauth_account(accounts: &[ClaudeAccount], oauth: &AmbientOauthAccount) -> Option<Uuid> {
    let org = normalized(oauth.organization_uuid.as_deref()).map(|v| v.to_lowercase());
    if let Some(org) = org.as_deref() {
        for account in accounts {
            if account.normalized_org_id().as_deref() == Some(org) {
                return Some(account.id);
            }
        }
    }
    let email = normalized(oauth.email_address.as_deref()).map(|v| v.to_lowercase());
    if let Some(email) = email.as_deref() {
        for account in accounts {
            if account.normalized_email_hint().as_deref() == Some(email) {
                return Some(account.id);
            }
        }
    }
    None
}

/// Id of the listed account whose identity matches the live ambient state, or
/// `None` when nothing matches. Pure computation: no filesystem, network, or
/// subprocess — callers pre-load `managed_access_tokens` from disk.
///
/// Order (spec "Active-account detection"):
/// 1. `oauthAccount` match: `organization_uuid` then `email_address`.
/// 2. Fallback: exact `claudeAiOauth.accessToken` string equality against a
///    stored managed account's own token.
/// 3. Otherwise `None`.
pub fn active_account_id(
    accounts: &[ClaudeAccount],
    ambient: &AmbientClaudeIdentity,
    managed_access_tokens: &HashMap<Uuid, String>,
) -> Option<Uuid> {
    if let Some(oauth) = &ambient.oauth_account
        && let Some(id) = match_by_oauth_account(accounts, oauth)
    {
        return Some(id);
    }

    let token = ambient.access_token.as_deref()?.trim();
    if token.is_empty() {
        return None;
    }
    for account in accounts {
        if managed_access_tokens.get(&account.id).map(|t| t.as_str()) == Some(token) {
            return Some(account.id);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_accounts::models::{ClaudeAccountSource, utc_now};
    use std::path::PathBuf;

    fn account(id: &str, org_id: Option<&str>, email: Option<&str>) -> ClaudeAccount {
        ClaudeAccount::new(
            Uuid::parse_str(id).unwrap(),
            None,
            email.map(str::to_string),
            org_id.map(str::to_string),
            None,
            None,
            PathBuf::from(format!("/managed/{id}")),
            ClaudeAccountSource::ManagedByApp,
            utc_now(),
            utc_now(),
            Some(utc_now()),
        )
    }

    // ── parse_auth_status_json ───────────────────────────────────────────

    #[test]
    fn parses_flat_auth_status_payload() {
        let identity = parse_auth_status_json(
            r#"{"email":"a@example.com","organizationUuid":"org-1","organizationName":"Acme","subscriptionType":"pro"}"#,
        )
        .unwrap();
        assert_eq!(identity.email.as_deref(), Some("a@example.com"));
        assert_eq!(identity.org_id.as_deref(), Some("org-1"));
        assert_eq!(identity.org_name.as_deref(), Some("Acme"));
        assert_eq!(identity.subscription_type.as_deref(), Some("pro"));
    }

    #[test]
    fn parses_nested_auth_status_payload() {
        let identity = parse_auth_status_json(
            r#"{"account":{"emailAddress":"b@example.com"},"organization":{"uuid":"org-2","name":"Beta"},"plan":"max"}"#,
        )
        .unwrap();
        assert_eq!(identity.email.as_deref(), Some("b@example.com"));
        assert_eq!(identity.org_id.as_deref(), Some("org-2"));
        assert_eq!(identity.org_name.as_deref(), Some("Beta"));
        assert_eq!(identity.subscription_type.as_deref(), Some("max"));
    }

    #[test]
    fn malformed_auth_status_json_returns_typed_error_not_panic() {
        let err = parse_auth_status_json("not json").expect_err("malformed JSON must error");
        assert!(matches!(err, ClaudeIdentityError::Parse(_)));
    }

    // ── derive_account_uuid / stable_discovered_id ──────────────────────

    #[test]
    fn derive_account_uuid_is_deterministic() {
        let a = derive_account_uuid("org:acme");
        let b = derive_account_uuid("org:acme");
        assert_eq!(a, b);
    }

    #[test]
    fn derive_account_uuid_differs_for_distinct_keys() {
        assert_ne!(
            derive_account_uuid("org:acme"),
            derive_account_uuid("org:beta")
        );
    }

    #[test]
    fn stable_discovered_id_uses_uuid_directory_name() {
        let uuid_name = "f47ac10b-58cc-4372-a567-0e02b2c3d479";
        let dir = PathBuf::from("/managed-configs").join(uuid_name);
        let identity = ClaudeIdentity::default();
        assert_eq!(
            stable_discovered_id(&dir, &identity),
            Uuid::parse_str(uuid_name).unwrap()
        );
    }

    #[test]
    fn stable_discovered_id_key_precedence_org_then_email_then_dir() {
        let dir = PathBuf::from("/managed-configs/not-a-uuid");

        let org_identity = ClaudeIdentity {
            org_id: Some("org-1".to_string()),
            email: Some("ignored@example.com".to_string()),
            ..Default::default()
        };
        assert_eq!(
            stable_discovered_id(&dir, &org_identity),
            derive_account_uuid("org:org-1")
        );

        let email_identity = ClaudeIdentity {
            email: Some("e@example.com".to_string()),
            ..Default::default()
        };
        assert_eq!(
            stable_discovered_id(&dir, &email_identity),
            derive_account_uuid("email:e@example.com")
        );

        let path_identity = ClaudeIdentity::default();
        assert_eq!(
            stable_discovered_id(&dir, &path_identity),
            derive_account_uuid(&format!("dir:{}", standardized_dir_key(&dir)))
        );
    }

    #[test]
    fn stable_discovered_id_stable_across_passes() {
        let dir = PathBuf::from("/managed-configs/not-a-uuid");
        let identity = ClaudeIdentity {
            org_id: Some("org-1".to_string()),
            ..Default::default()
        };
        assert_eq!(
            stable_discovered_id(&dir, &identity),
            stable_discovered_id(&dir, &identity)
        );
    }

    // ── active_account_id ────────────────────────────────────────────────

    #[test]
    fn active_account_id_matches_by_oauth_account_org() {
        let a = account(
            "11111111-1111-1111-1111-111111111111",
            Some("org-a"),
            Some("a@x.com"),
        );
        let b = account(
            "22222222-2222-2222-2222-222222222222",
            Some("org-b"),
            Some("b@x.com"),
        );
        let accounts = vec![a, b.clone()];
        let ambient = AmbientClaudeIdentity {
            oauth_account: Some(AmbientOauthAccount {
                email_address: Some("nomatch@x.com".to_string()),
                organization_uuid: Some("ORG-B".to_string()),
            }),
            access_token: None,
        };
        assert_eq!(
            active_account_id(&accounts, &ambient, &HashMap::new()),
            Some(b.id)
        );
    }

    #[test]
    fn active_account_id_falls_back_to_oauth_account_email() {
        let a = account(
            "11111111-1111-1111-1111-111111111111",
            None,
            Some("a@x.com"),
        );
        let b = account(
            "22222222-2222-2222-2222-222222222222",
            None,
            Some("b@x.com"),
        );
        let accounts = vec![a, b.clone()];
        let ambient = AmbientClaudeIdentity {
            oauth_account: Some(AmbientOauthAccount {
                email_address: Some("B@X.COM".to_string()),
                organization_uuid: None,
            }),
            access_token: None,
        };
        assert_eq!(
            active_account_id(&accounts, &ambient, &HashMap::new()),
            Some(b.id)
        );
    }

    #[test]
    fn active_account_id_falls_back_to_access_token_when_oauth_account_absent() {
        let a = account("11111111-1111-1111-1111-111111111111", None, None);
        let b = account("22222222-2222-2222-2222-222222222222", None, None);
        let accounts = vec![a.clone(), b.clone()];
        let mut tokens = HashMap::new();
        tokens.insert(a.id, "token-a".to_string());
        tokens.insert(b.id, "token-b".to_string());

        let ambient = AmbientClaudeIdentity {
            oauth_account: None,
            access_token: Some("token-b".to_string()),
        };
        assert_eq!(active_account_id(&accounts, &ambient, &tokens), Some(b.id));
    }

    #[test]
    fn active_account_id_falls_back_to_access_token_when_oauth_account_unmatched() {
        let a = account("11111111-1111-1111-1111-111111111111", Some("org-a"), None);
        let accounts = vec![a.clone()];
        let mut tokens = HashMap::new();
        tokens.insert(a.id, "token-a".to_string());

        let ambient = AmbientClaudeIdentity {
            oauth_account: Some(AmbientOauthAccount {
                email_address: None,
                organization_uuid: Some("org-does-not-exist".to_string()),
            }),
            access_token: Some("token-a".to_string()),
        };
        assert_eq!(active_account_id(&accounts, &ambient, &tokens), Some(a.id));
    }

    #[test]
    fn active_account_id_none_when_nothing_matches() {
        let a = account(
            "11111111-1111-1111-1111-111111111111",
            Some("org-a"),
            Some("a@x.com"),
        );
        let accounts = vec![a];
        let ambient = AmbientClaudeIdentity {
            oauth_account: Some(AmbientOauthAccount {
                email_address: Some("z@x.com".to_string()),
                organization_uuid: Some("org-z".to_string()),
            }),
            access_token: Some("unrelated-token".to_string()),
        };
        assert_eq!(
            active_account_id(&accounts, &ambient, &HashMap::new()),
            None
        );
    }

    #[test]
    fn active_account_id_oauth_account_wins_over_access_token() {
        let a = account("11111111-1111-1111-1111-111111111111", Some("org-a"), None);
        let b = account("22222222-2222-2222-2222-222222222222", None, None);
        let accounts = vec![a.clone(), b.clone()];
        let mut tokens = HashMap::new();
        tokens.insert(b.id, "shared-token".to_string());

        // oauthAccount matches `a`, while the access token would match `b` —
        // the oauthAccount match must win.
        let ambient = AmbientClaudeIdentity {
            oauth_account: Some(AmbientOauthAccount {
                email_address: None,
                organization_uuid: Some("org-a".to_string()),
            }),
            access_token: Some("shared-token".to_string()),
        };
        assert_eq!(active_account_id(&accounts, &ambient, &tokens), Some(a.id));
    }
}
