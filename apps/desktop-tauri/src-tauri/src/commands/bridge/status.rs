use super::*;

/// Build a compact tray status label from a raw snapshot using the current language.
/// Localization is done at render time so cached snapshots stay language-neutral.
pub(crate) fn compact_tray_status_label(
    window: &RateWindowSnapshot,
    lang: codexbar::settings::Language,
) -> String {
    if window.is_informational {
        return window
            .reset_description
            .clone()
            .unwrap_or_else(|| "Unavailable".to_string());
    }

    let pct = format!("{:.0}%", window.used_percent);
    if let Some(reset) = compact_reset_description(window, lang) {
        format!("{pct} • {reset}")
    } else {
        pct
    }
}

fn compact_reset_description(
    window: &RateWindowSnapshot,
    lang: codexbar::settings::Language,
) -> Option<String> {
    if let Some(ref resets_at) = window.resets_at {
        let dt = chrono::DateTime::parse_from_rfc3339(resets_at)
            .ok()
            .map(|dt| dt.with_timezone(&chrono::Utc))?;
        return Some(format_compact_reset_countdown(dt, lang));
    }

    window
        .reset_description
        .as_deref()
        .map(|desc| normalize_reset_description(desc, lang))
        .filter(|desc| !desc.is_empty())
}

fn format_compact_reset_countdown(
    resets_at: chrono::DateTime<chrono::Utc>,
    lang: codexbar::settings::Language,
) -> String {
    let now = chrono::Utc::now();
    if resets_at <= now {
        return locale::get_text(lang, locale::LocaleKey::ResetInProgress);
    }

    let total_minutes = (resets_at - now).num_minutes().max(0);
    let days = total_minutes / 1440;
    let hours = (total_minutes % 1440) / 60;
    let minutes = total_minutes % 60;

    if days > 0 {
        locale::format_locale(
            lang,
            locale::LocaleKey::ResetsInDaysHours,
            &[&days.to_string(), &hours.to_string()],
        )
    } else {
        locale::format_locale(
            lang,
            locale::LocaleKey::ResetsInHoursMinutes,
            &[&hours.to_string(), &format!("{minutes:02}")],
        )
    }
}

fn normalize_reset_description(desc: &str, lang: codexbar::settings::Language) -> String {
    let trimmed = desc.trim();
    let lower = trimmed.to_ascii_lowercase();
    let prefix_len = ["resets in ", "reset in ", "in "]
        .iter()
        .find(|&&p| lower.starts_with(p))
        .map(|p| p.len())
        .unwrap_or(0);
    let body = trimmed[prefix_len..].trim_start();
    format!(
        "{} {body}",
        locale::get_text(lang, locale::LocaleKey::ResetsInShort)
    )
}

pub(crate) fn friendly_provider_error(id: ProviderId, error: &str) -> String {
    if id != ProviderId::Claude {
        return error.to_string();
    }

    let trimmed = error.trim();
    let lower = trimmed.to_lowercase();

    if lower.contains("swift.cancellationerror")
        || lower.contains("the operation couldn't be completed")
        || lower.contains("the operation could not be completed")
    {
        return "Claude usage fetch was cancelled before usage data was returned. Refresh Claude, or re-authenticate with Claude Code and try again.".to_string();
    }

    if lower.contains("claude oauth credentials not found") {
        return "Claude sign-in was not found. Run `claude` once to authenticate, then refresh Claude in Win-CodexBar.".to_string();
    }

    if lower.contains("oauth token expired") || lower.contains("token invalid or expired") {
        return "Claude sign-in expired. Run `claude` to refresh your Claude Code login, then refresh Claude in Win-CodexBar.".to_string();
    }

    if trimmed == "Authentication required" {
        return "Claude needs sign-in before Win-CodexBar can read usage. Run `claude` once, or add Claude cookies in Provider settings.".to_string();
    }

    if lower.starts_with("claude usage failed from all configured sources.") {
        return trimmed
            .replace(
                "OAuth: OAuth error: Claude OAuth credentials not found. Run `claude` to authenticate.",
                "OAuth: sign-in not found",
            )
            .replace(
                "Web: No cookies available for web API",
                "Web: no Claude cookies available",
            )
            .replace(
                "CLI: Provider not installed:",
                "CLI: not installed:",
            );
    }

    trimmed.to_string()
}
