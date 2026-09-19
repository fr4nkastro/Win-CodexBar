//! Codex credential-file parsing, identity derivation, and persistence.

use std::path::Path;

use base64::Engine;
use chrono::{DateTime, Utc};

use super::api::CodexApiError;

/// Identity derived from a Codex account's credentials.
#[derive(Debug, Clone)]
pub struct AuthBackedIdentity {
    pub email: Option<String>,
    pub auth_subject: Option<String>,
    pub plan: Option<String>,
    pub provider_account_id: Option<String>,
}

/// Raw auth.json credentials.
#[derive(Debug, Clone)]
pub struct AuthCredentials {
    pub access_token: String,
    pub refresh_token: String,
    pub id_token: Option<String>,
    pub account_id: Option<String>,
    pub last_refresh: Option<DateTime<Utc>>,
}

impl AuthCredentials {
    pub fn needs_refresh(&self) -> bool {
        self.last_refresh
            .is_none_or(|last| Utc::now() - last > chrono::TimeDelta::days(8))
    }
}

/// Load the account identity from a Codex home's `auth.json`.
pub fn load_identity(codex_home_path: &Path) -> Result<AuthBackedIdentity, CodexApiError> {
    Ok(identity_from_credentials(&load_credentials(
        codex_home_path,
    )?))
}

/// Read and parse `auth.json`.
pub fn load_credentials(codex_home_path: &Path) -> Result<AuthCredentials, CodexApiError> {
    let auth_path = codex_home_path.join("auth.json");
    let content = std::fs::read_to_string(&auth_path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            CodexApiError::Message("No `auth.json` was found for this account.".to_string())
        } else {
            CodexApiError::Parse(format!("Failed to read the auth file: {e}"))
        }
    })?;
    parse_credentials_json(&content)
}

/// Parse `auth.json` contents, accepting `OPENAI_API_KEY` or a `tokens` object.
pub fn parse_credentials_json(content: &str) -> Result<AuthCredentials, CodexApiError> {
    let json: serde_json::Value = serde_json::from_str(content)
        .map_err(|e| CodexApiError::Parse(format!("Failed to parse the auth file: {e}")))?;

    if let Some(api_key) = json
        .get("OPENAI_API_KEY")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Ok(AuthCredentials {
            access_token: api_key.to_string(),
            refresh_token: String::new(),
            id_token: None,
            account_id: None,
            last_refresh: None,
        });
    }

    let tokens = json
        .get("tokens")
        .and_then(|v| v.as_object())
        .ok_or_else(|| {
            CodexApiError::Message(
                "The required token fields are missing from `auth.json`.".to_string(),
            )
        })?;

    let access_token = tokens
        .get("access_token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            CodexApiError::Message(
                "The required token fields are missing from `auth.json`.".to_string(),
            )
        })?
        .to_string();

    let id_token = tokens
        .get("id_token")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let account_id = tokens
        .get("account_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| account_id_from_id_token(id_token.as_deref()));

    Ok(AuthCredentials {
        access_token,
        refresh_token: tokens
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        id_token,
        account_id,
        last_refresh: json
            .get("last_refresh")
            .and_then(|v| v.as_str())
            .and_then(super::models::parse_datetime),
    })
}

/// Save (possibly refreshed) credentials back to `auth.json`.
pub fn save_credentials(
    codex_home_path: &Path,
    credentials: &AuthCredentials,
) -> std::io::Result<()> {
    let auth_path = codex_home_path.join("auth.json");
    let mut payload: serde_json::Value = std::fs::read_to_string(&auth_path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_else(|| serde_json::json!({}));

    let mut tokens = serde_json::Map::new();
    tokens.insert(
        "access_token".to_string(),
        serde_json::json!(credentials.access_token),
    );
    tokens.insert(
        "refresh_token".to_string(),
        serde_json::json!(credentials.refresh_token),
    );
    if let Some(id_token) = &credentials.id_token {
        tokens.insert("id_token".to_string(), serde_json::json!(id_token));
    }
    if let Some(account_id) = &credentials.account_id {
        tokens.insert("account_id".to_string(), serde_json::json!(account_id));
    }
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("tokens".to_string(), serde_json::Value::Object(tokens));
        obj.insert(
            "last_refresh".to_string(),
            serde_json::json!(Utc::now().to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)),
        );
    }
    write_auth_contents(codex_home_path, &serde_json::to_vec_pretty(&payload)?)
}

pub(super) fn write_auth_contents(home: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let auth_path = home.join("auth.json");
    // Preserve an existing auth-file symlink by replacing its resolved target.
    let destination = auth_path.canonicalize().unwrap_or(auth_path);
    let staged = destination.with_file_name(format!(".auth-{}.tmp", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&staged)?;
    let written = file.write_all(contents);
    drop(file);
    let result = written.and_then(|()| std::fs::rename(&staged, &destination));
    if result.is_err() {
        let _cleanup = std::fs::remove_file(staged);
    }
    result
}

pub(super) fn identity_from_credentials(credentials: &AuthCredentials) -> AuthBackedIdentity {
    let payload = credentials
        .id_token
        .as_deref()
        .and_then(jwt_payload)
        .unwrap_or_default();
    let auth = payload
        .get("https://api.openai.com/auth")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    let profile = payload
        .get("https://api.openai.com/profile")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();

    let email = normalize_string(payload.get("email").and_then(|v| v.as_str()))
        .or_else(|| normalize_string(profile.get("email").and_then(|v| v.as_str())));
    let auth_subject = normalize_string(payload.get("sub").and_then(|v| v.as_str()));
    let plan = normalize_string(auth.get("chatgpt_plan_type").and_then(|v| v.as_str()))
        .or_else(|| normalize_string(payload.get("chatgpt_plan_type").and_then(|v| v.as_str())));
    let provider_account_id = normalize_string(credentials.account_id.as_deref())
        .or_else(|| normalize_string(auth.get("chatgpt_account_id").and_then(|v| v.as_str())))
        .or_else(|| normalize_string(payload.get("chatgpt_account_id").and_then(|v| v.as_str())));

    AuthBackedIdentity {
        email,
        auth_subject,
        plan,
        provider_account_id,
    }
}

/// Minimal JWT payload extraction (base64url payload, no signature verification).
pub fn jwt_payload(token: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let mut padded = payload.to_string();
    while padded.len() % 4 != 0 {
        padded.push('=');
    }
    let decoded = base64::engine::general_purpose::URL_SAFE
        .decode(padded.as_bytes())
        .ok()?;
    serde_json::from_slice::<serde_json::Value>(&decoded)
        .ok()?
        .as_object()
        .cloned()
}

pub(super) fn account_id_from_id_token(id_token: Option<&str>) -> Option<String> {
    let payload = id_token.and_then(jwt_payload)?;
    let auth = payload
        .get("https://api.openai.com/auth")
        .and_then(|v| v.as_object())?;
    normalize_string(auth.get("chatgpt_account_id").and_then(|v| v.as_str()))
        .or_else(|| normalize_string(payload.get("chatgpt_account_id").and_then(|v| v.as_str())))
}

pub(super) fn normalize_string(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

pub(super) fn string_value(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}
