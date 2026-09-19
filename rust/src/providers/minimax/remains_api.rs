//! Bearer-authenticated MiniMax coding-plan quota fetch.
//!
//! Kept separate from the legacy billing client so the client-rendered console
//! workaround (#425) does not further grow the already-large provider module.

use chrono::Utc;

use crate::core::{FetchContext, ProviderError, ProviderFetchResult};

use super::{MiniMaxRegion, coding_plan, coding_plan_html};

fn resolve_plain_api_key(explicit: Option<&str>, environment: Option<&str>) -> Option<String> {
    explicit
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .or_else(|| environment.map(str::trim).filter(|key| !key.is_empty()))
        .map(str::to_string)
}

pub(super) fn read_plain_api_key(ctx: &FetchContext) -> Option<String> {
    let environment = std::env::var("MINIMAX_API_KEY").ok();
    resolve_plain_api_key(ctx.api_key.as_deref(), environment.as_deref())
}

pub(super) async fn fetch_remains_via_api_key(
    api_key: &str,
    region: MiniMaxRegion,
) -> Result<ProviderFetchResult, ProviderError> {
    let now = Utc::now();
    let urls = [region.coding_plan_remains_url(), region.www_remains_url()];
    let mut last_err: Option<ProviderError> = None;
    for url in urls {
        match fetch_remains_once_via_api_key(api_key, &url).await {
            Ok(snapshot) => {
                let usage = coding_plan_html::to_usage_snapshot(&snapshot, now)?;
                return Ok(ProviderFetchResult::new(usage, "api"));
            }
            Err(err @ ProviderError::Parse(_)) => last_err = Some(err),
            Err(err) => return Err(err),
        }
    }
    Err(last_err.unwrap_or_else(|| ProviderError::Parse("Missing MiniMax remains URL.".into())))
}

async fn fetch_remains_once_via_api_key(
    api_key: &str,
    url: &str,
) -> Result<coding_plan::MiniMaxCodingPlanSnapshot, ProviderError> {
    let client = crate::core::credentialed_http_client_builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| ProviderError::Other(e.to_string()))?;

    let response = client
        .get(url)
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Accept", "application/json, text/plain, */*")
        .send()
        .await?;

    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(ProviderError::AuthRequired);
    }
    if !status.is_success() {
        let message = format!("MiniMax remains (api key) returned status {status}");
        if status == reqwest::StatusCode::NOT_FOUND
            || status == reqwest::StatusCode::METHOD_NOT_ALLOWED
        {
            return Err(ProviderError::Parse(message));
        }
        return Err(ProviderError::Other(message));
    }

    let json: serde_json::Value = response
        .json()
        .await
        .map_err(|e| ProviderError::Parse(format!("Failed to parse remains JSON: {e}")))?;
    coding_plan::parse_coding_plan_value(&json, Utc::now())
}

#[cfg(test)]
mod tests {
    use super::resolve_plain_api_key;

    #[test]
    fn plain_api_key_prefers_explicit_then_environment_without_mutating_process_env() {
        assert_eq!(
            resolve_plain_api_key(Some("  ctx-key  "), Some("env-key")).as_deref(),
            Some("ctx-key")
        );
        assert_eq!(
            resolve_plain_api_key(Some("   "), Some("  env-key  ")).as_deref(),
            Some("env-key")
        );
        assert_eq!(resolve_plain_api_key(Some("   "), Some("   ")), None);
        assert_eq!(resolve_plain_api_key(None, None), None);
    }
}
