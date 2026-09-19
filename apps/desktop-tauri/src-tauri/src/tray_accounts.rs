//! Account-specific native tray menu construction and dispatch.
//!
//! Keep provider/account workflows out of the generic tray shell so adding a
//! new account action does not grow `tray_bridge.rs` into another controller.

use codexbar::claude_accounts::ClaudeAccount;
use codexbar::codex_accounts::{CodexAccount, ordinals_by_id};
use codexbar::locale::{self, LocaleKey};
use codexbar::settings::{Language, Settings};
use tauri::AppHandle;
use uuid::Uuid;

use crate::tray_menu::TrayMenuEntry;

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(
    clippy::enum_variant_names,
    reason = "every tray action targets one provider account; the suffix is the domain noun"
)]
pub(crate) enum AccountMenuAction {
    AddCodexAccount,
    AddClaudeAccount,
    CancelClaudeLogin,
    SwitchClaudeAccount(String),
    SwitchCodexAccount(String),
}

pub(crate) fn prepend_account_menus(spec: &mut Vec<TrayMenuEntry>, settings: &Settings) {
    let accounts = crate::commands::load_codex_accounts().unwrap_or_default();
    let active =
        codexbar::codex_accounts::CodexAccountManager::new().discover_ambient_account(&accounts);
    spec.insert(
        0,
        codex_accounts_menu(
            &accounts,
            active.as_ref(),
            settings.ui_language,
            settings.hide_personal_info,
        ),
    );

    // Claude accounts use this fork's `claude_accounts` store and its consent
    // gate: the submenu is only offered once account management is allowed.
    if crate::commands::claude_accounts_management_enabled() {
        let claude_accounts = crate::commands::claude_accounts_list().unwrap_or_default();
        let active = crate::commands::claude_active_account_id(&claude_accounts);
        spec.insert(
            1,
            claude_accounts_menu(
                &claude_accounts,
                active,
                crate::commands::claude_login_in_progress(),
                settings.ui_language,
                settings.hide_personal_info,
            ),
        );
    }
}

pub(crate) fn resolve_action(id: &str) -> Option<AccountMenuAction> {
    match id {
        "add_codex_account" => Some(AccountMenuAction::AddCodexAccount),
        "add_claude_account" => Some(AccountMenuAction::AddClaudeAccount),
        "cancel_claude_login" => Some(AccountMenuAction::CancelClaudeLogin),
        _ if id.starts_with("switch_claude_account:") => {
            let id = id.strip_prefix("switch_claude_account:")?;
            uuid::Uuid::parse_str(id).ok()?;
            Some(AccountMenuAction::SwitchClaudeAccount(id.to_string()))
        }
        _ if id.starts_with("switch_codex_account:") => {
            let id = id.strip_prefix("switch_codex_account:")?;
            uuid::Uuid::parse_str(id).ok()?;
            Some(AccountMenuAction::SwitchCodexAccount(id.to_string()))
        }
        _ => None,
    }
}

pub(crate) fn handle_action(app: &AppHandle, action: AccountMenuAction) {
    match action {
        AccountMenuAction::AddCodexAccount => {
            let handle = app.clone();
            tauri::async_runtime::spawn(async move {
                match crate::commands::codex_account_add(handle.clone()).await {
                    Ok(_) => show_codex_message(&handle, "Codex account added."),
                    Err(error) => show_codex_message(&handle, &error),
                }
            });
        }
        AccountMenuAction::SwitchCodexAccount(id) => {
            let handle = app.clone();
            tauri::async_runtime::spawn(async move {
                match crate::commands::codex_account_switch(handle.clone(), id).await {
                    Ok(result) => {
                        use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};
                        if result.desktop_session_restore_path.is_some() {
                            let dialog_handle = handle.clone();
                            let restart = tauri::async_runtime::spawn_blocking(move || {
                                dialog_handle
                                    .dialog()
                                    .message("Account switched. Restart Codex Desktop to use it? This stops running desktop tasks.")
                                    .title("Codex Accounts")
                                    .buttons(MessageDialogButtons::OkCancelCustom(
                                        "Restart".into(),
                                        "Later".into(),
                                    ))
                                    .blocking_show()
                            })
                            .await
                            .unwrap_or(false);
                            if restart {
                                let restart_result =
                                    crate::commands::codex_account_restart_desktop(
                                        handle.clone(),
                                        result.switch_id.to_string(),
                                    )
                                    .await;
                                if let Err(error) = restart_result {
                                    show_codex_message(&handle, &error);
                                }
                            }
                        } else {
                            show_codex_message(&handle, "Codex account switched.");
                        }
                    }
                    Err(error) => show_codex_message(&handle, &error),
                }
            });
        }
        AccountMenuAction::CancelClaudeLogin => {
            // The pending add call reports the cancellation; nothing to show.
            let _cancelled = crate::commands::claude_account_cancel_login();
        }
        action @ (AccountMenuAction::AddClaudeAccount
        | AccountMenuAction::SwitchClaudeAccount(_)) => {
            let handle = app.clone();
            tauri::async_runtime::spawn(async move {
                use tauri_plugin_dialog::DialogExt;
                let result = match action {
                    AccountMenuAction::AddClaudeAccount => {
                        crate::commands::claude_account_add(handle.clone())
                            .await
                            .map(|_| "Claude Code account added. Select it to switch.")
                    }
                    AccountMenuAction::SwitchClaudeAccount(id) => {
                        crate::commands::claude_account_switch(handle.clone(), id)
                            .await
                            .map(|_| {
                                "Claude Code account switched. Reopen the Claude Code CLI to use it."
                            })
                    }
                    _ => return,
                };
                // A user-initiated cancel is not an error worth a dialog.
                if result.as_ref().is_err_and(|error| {
                    error == codexbar::claude_accounts::SIGN_IN_CANCELLED_MESSAGE
                }) {
                    return;
                }
                handle
                    .dialog()
                    .message(result.map_or_else(|error| error, str::to_string))
                    .title("Claude Code accounts")
                    .show(|_| {});
            });
        }
    }
}

fn show_codex_message(app: &AppHandle, message: &str) {
    use tauri_plugin_dialog::DialogExt;
    app.dialog()
        .message(message)
        .title("Codex Accounts")
        .show(|_| {});
}

fn codex_accounts_menu(
    accounts: &[CodexAccount],
    active: Option<&CodexAccount>,
    lang: Language,
    hide_personal_info: bool,
) -> TrayMenuEntry {
    let text = |key| locale::get_text(lang, key);
    let ordinals = ordinals_by_id(accounts);
    let mut children: Vec<_> = accounts
        .iter()
        .map(|account| {
            let is_active = active.is_some_and(|current| current.matches(account));
            let mut entry = TrayMenuEntry::check_item(
                format!("switch_codex_account:{}", account.id),
                codex_account_menu_label(account, lang, hide_personal_info, ordinals[&account.id]),
                is_active,
            );
            entry.disabled = is_active;
            entry
        })
        .collect();
    if children.is_empty() {
        children.push(TrayMenuEntry::status_row(
            "codex_accounts_empty",
            text(LocaleKey::CodexAccountsEmpty),
        ));
    }
    children.push(TrayMenuEntry::separator());
    children.push(TrayMenuEntry::item(
        "add_codex_account",
        text(LocaleKey::CodexAccountsAddButton),
    ));
    TrayMenuEntry::submenu(
        "codex_accounts",
        text(LocaleKey::CodexAccountsTitle),
        children,
    )
}

fn codex_account_menu_label(
    account: &CodexAccount,
    lang: Language,
    hide_personal_info: bool,
    ordinal: usize,
) -> String {
    if hide_personal_info {
        return format!("{} {ordinal}", locale::get_text(lang, LocaleKey::Account));
    }
    account.display_name()
}

fn claude_accounts_menu(
    accounts: &[ClaudeAccount],
    active: Option<Uuid>,
    login_in_progress: bool,
    lang: Language,
    hide_personal_info: bool,
) -> TrayMenuEntry {
    let text = |key| locale::get_text(lang, key);
    let mut children: Vec<_> = accounts
        .iter()
        .enumerate()
        .map(|(index, account)| {
            let is_active = active == Some(account.id);
            let mut entry = TrayMenuEntry::check_item(
                format!("switch_claude_account:{}", account.id),
                claude_account_menu_label(account, lang, hide_personal_info, index + 1),
                is_active,
            );
            entry.disabled = is_active;
            entry
        })
        .collect();
    if children.is_empty() {
        children.push(TrayMenuEntry::status_row(
            "claude_accounts_empty",
            text(LocaleKey::ClaudeAccountsEmpty),
        ));
    }
    children.push(TrayMenuEntry::separator());
    // While a sign-in runs, Add is replaced by Cancel: only one sign-in can
    // be in flight, and the browser flow may be abandoned at any time.
    children.push(if login_in_progress {
        TrayMenuEntry::item(
            "cancel_claude_login",
            text(LocaleKey::ClaudeAccountsCancelLogin),
        )
    } else {
        TrayMenuEntry::item(
            "add_claude_account",
            text(LocaleKey::ClaudeAccountsAddButton),
        )
    });
    TrayMenuEntry::submenu(
        "claude_accounts",
        text(LocaleKey::ClaudeAccountsTitle),
        children,
    )
}

fn claude_account_menu_label(
    account: &ClaudeAccount,
    lang: Language,
    hide_personal_info: bool,
    ordinal: usize,
) -> String {
    if hide_personal_info {
        return format!("{} {ordinal}", locale::get_text(lang, LocaleKey::Account));
    }
    match account.org_name.as_deref().map(str::trim) {
        Some(org) if !org.is_empty() => format!("{} ({org})", account.display_name()),
        _ => account.display_name(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn menu_contains(menu: &[TrayMenuEntry], id: &str) -> bool {
        menu.iter().any(|entry| {
            entry.id.as_deref() == Some(id)
                || (!entry.children.is_empty() && menu_contains(&entry.children, id))
        })
    }

    #[test]
    fn account_action_ids_are_typed_and_validated() {
        assert_eq!(
            resolve_action("add_claude_account"),
            Some(AccountMenuAction::AddClaudeAccount)
        );
        assert_eq!(
            resolve_action("cancel_claude_login"),
            Some(AccountMenuAction::CancelClaudeLogin)
        );
        let claude_id = "00000000-0000-0000-0000-00000000000a";
        assert_eq!(
            resolve_action(&format!("switch_claude_account:{claude_id}")),
            Some(AccountMenuAction::SwitchClaudeAccount(claude_id.into()))
        );
        assert!(resolve_action("save_claude_account").is_none());
        assert!(resolve_action("switch_claude_account:").is_none());
        assert!(resolve_action("switch_claude_account:a:org").is_none());
        assert!(resolve_action("switch_codex_account:not-a-uuid").is_none());
    }

    #[test]
    fn codex_accounts_can_be_switched_or_added_from_tray() {
        use codexbar::codex_accounts::{CodexAccountSource, utc_now};
        let make = |name: &str| {
            CodexAccount::new(
                uuid::Uuid::new_v4(),
                Some(name.into()),
                None,
                None,
                Some(name.into()),
                std::path::PathBuf::from(name),
                CodexAccountSource::ManagedByApp,
                utc_now(),
                utc_now(),
                None,
            )
        };
        let first = make("Personal");
        let second = make("Work");
        let menu = codex_accounts_menu(
            &[first.clone(), second.clone()],
            Some(&first),
            Language::English,
            false,
        );
        assert_eq!(menu.children[0].checked, Some(true));
        assert!(menu.children[0].disabled);
        assert_eq!(
            menu.children[1].id.as_deref(),
            Some(format!("switch_codex_account:{}", second.id).as_str())
        );
        assert_eq!(menu.children[1].checked, Some(false));
        assert!(!menu.children[1].disabled);
        assert!(menu_contains(&menu.children, "add_codex_account"));
        let empty = codex_accounts_menu(&[], None, Language::English, false);
        assert!(menu_contains(&empty.children, "add_codex_account"));
        let mut email_account = second;
        email_account.nickname = None;
        email_account.email_hint = Some("private@example.com".into());
        let private = codex_accounts_menu(&[email_account.clone()], None, Language::English, true);
        assert!(!private.children[0].label.contains("private@example.com"));
        let visible = codex_accounts_menu(&[email_account], None, Language::English, false);
        assert_eq!(visible.children[0].label, "private@example.com");
    }

    #[test]
    fn hidden_codex_tray_labels_are_opaque_and_stable() {
        use codexbar::codex_accounts::{CodexAccountSource, utc_now};

        let make = |id: &str, nickname: Option<&str>, email: &str| {
            CodexAccount::new(
                Uuid::parse_str(id).unwrap(),
                nickname.map(str::to_string),
                Some(email.to_string()),
                None,
                None,
                std::path::PathBuf::from("C:/private-home"),
                CodexAccountSource::ManagedByApp,
                utc_now(),
                utc_now(),
                None,
            )
        };
        let with_nickname = make(
            "00000000-0000-0000-0000-000000000002",
            Some("Work"),
            "user@example.com",
        );
        let without_nickname = make(
            "00000000-0000-0000-0000-000000000001",
            None,
            "personal@example.com",
        );

        let accounts = [with_nickname.clone(), without_nickname.clone()];
        let ordinals = ordinals_by_id(&accounts);
        assert_eq!(ordinals[&without_nickname.id], 1);
        assert_eq!(ordinals[&with_nickname.id], 2);
        assert_eq!(
            codex_account_menu_label(
                &with_nickname,
                Language::English,
                true,
                ordinals[&with_nickname.id],
            ),
            "Account 2"
        );
        assert_eq!(
            codex_account_menu_label(
                &without_nickname,
                Language::English,
                true,
                ordinals[&without_nickname.id],
            ),
            "Account 1"
        );

        let hidden = codex_accounts_menu(&accounts, None, Language::English, true);
        assert_eq!(hidden.children[0].label, "Account 2");
        assert_eq!(hidden.children[1].label, "Account 1");
        for entry in hidden.children.iter().take(2) {
            assert!(!entry.label.contains('@'));
            assert!(!entry.label.contains("example.com"));
            assert!(!entry.label.contains("Work"));
        }

        let reversed = codex_accounts_menu(
            &[without_nickname.clone(), with_nickname.clone()],
            None,
            Language::English,
            true,
        );
        assert_eq!(reversed.children[0].label, "Account 1");
        assert_eq!(reversed.children[1].label, "Account 2");

        let visible = codex_accounts_menu(&[with_nickname], None, Language::English, false);
        assert_eq!(visible.children[0].label, "user@example.com — Work");
    }

    #[test]
    fn claude_menu_checks_current_account_and_routes_saved_accounts() {
        use codexbar::claude_accounts::ClaudeAccountSource;
        use codexbar::codex_accounts::utc_now;
        let make = |email: &str, source: ClaudeAccountSource| {
            ClaudeAccount::new(
                Uuid::new_v4(),
                None,
                Some(email.into()),
                None,
                Some("Org".into()),
                None,
                std::path::PathBuf::from(email),
                source,
                utc_now(),
                utc_now(),
                None,
            )
        };
        let current = make("a@example.com", ClaudeAccountSource::Ambient);
        let saved = make("b@example.com", ClaudeAccountSource::ManagedByApp);
        let menu = claude_accounts_menu(
            &[current.clone(), saved.clone()],
            Some(current.id),
            false,
            Language::English,
            false,
        );
        assert_eq!(menu.id.as_deref(), Some("claude_accounts"));
        assert_eq!(menu.children[0].checked, Some(true));
        assert!(menu.children[0].disabled);
        assert_eq!(menu.children[0].label, "a@example.com (Org)");
        assert_eq!(
            menu.children[1].id.as_deref(),
            Some(format!("switch_claude_account:{}", saved.id).as_str())
        );
        assert_eq!(menu.children[1].checked, Some(false));
        assert!(!menu.children[1].disabled);
        assert!(menu_contains(&menu.children, "add_claude_account"));
        assert!(!menu_contains(&menu.children, "cancel_claude_login"));
        assert!(menu_contains(
            &claude_accounts_menu(&[], None, false, Language::English, false).children,
            "add_claude_account"
        ));
        let signing_in = claude_accounts_menu(
            std::slice::from_ref(&saved),
            None,
            true,
            Language::English,
            false,
        );
        assert!(menu_contains(&signing_in.children, "cancel_claude_login"));
        assert!(!menu_contains(&signing_in.children, "add_claude_account"));
        let hidden = claude_accounts_menu(&[saved], None, false, Language::English, true);
        assert_eq!(hidden.children[0].label, "Account 1");
    }
}
