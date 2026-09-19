//! Process-wide coordination for Codex account mutations and Desktop restart handoff.
//!
//! The Tauri layer should issue commands, not own the account-switch state machine.

use std::sync::Mutex;

use super::{CodexAccount, CodexAccountManagerError, CodexSwitchResult};

static ACCOUNT_MUTATION: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static PENDING_RESTART: Mutex<Option<CodexSwitchResult>> = Mutex::new(None);

#[derive(Debug, Default, Clone, Copy)]
pub struct CodexAccountRuntime;

impl CodexAccountRuntime {
    pub fn new() -> Self {
        Self
    }

    pub fn try_begin_mutation(
        &self,
    ) -> Result<tokio::sync::MutexGuard<'static, ()>, CodexAccountManagerError> {
        ACCOUNT_MUTATION.try_lock().map_err(|_| {
            CodexAccountManagerError::Message(
                "A Codex account operation is already in progress.".to_string(),
            )
        })
    }

    pub fn remember_restart(
        &self,
        result: &CodexSwitchResult,
    ) -> Result<(), CodexAccountManagerError> {
        let pending = result
            .desktop_session_restore_path
            .as_ref()
            .map(|_| result.clone());
        *PENDING_RESTART
            .lock()
            .map_err(|error| CodexAccountManagerError::Message(error.to_string()))? = pending;
        Ok(())
    }

    pub fn invalidate_restart_for_removed_account(
        &self,
        removed: &CodexAccount,
    ) -> Result<(), CodexAccountManagerError> {
        let mut pending = PENDING_RESTART
            .lock()
            .map_err(|error| CodexAccountManagerError::Message(error.to_string()))?;
        invalidate_restart_for_removed_account(&mut pending, removed);
        Ok(())
    }

    pub fn pending_restart_for(
        &self,
        switch_id: &str,
        active: Option<&CodexAccount>,
    ) -> Result<CodexSwitchResult, CodexAccountManagerError> {
        let pending = PENDING_RESTART
            .lock()
            .map_err(|error| CodexAccountManagerError::Message(error.to_string()))?;
        validate_pending_restart(pending.as_ref(), switch_id, active).cloned()
    }

    pub fn clear_pending_restart(&self) -> Result<(), CodexAccountManagerError> {
        *PENDING_RESTART
            .lock()
            .map_err(|error| CodexAccountManagerError::Message(error.to_string()))? = None;
        Ok(())
    }
}

fn invalidate_restart_for_removed_account(
    pending: &mut Option<CodexSwitchResult>,
    removed: &CodexAccount,
) {
    if pending.as_ref().is_some_and(|result| {
        result
            .materialized_account
            .as_ref()
            .is_some_and(|account| account.matches(removed))
            || result
                .ambient_account
                .as_ref()
                .is_some_and(|account| account.matches(removed))
            || [
                &result.desktop_session_backup_path,
                &result.desktop_session_restore_path,
            ]
            .into_iter()
            .flatten()
            .any(|path| path.parent() == Some(removed.codex_home_path.as_path()))
    }) {
        *pending = None;
    }
}

fn validate_pending_restart<'a>(
    pending: Option<&'a CodexSwitchResult>,
    switch_id: &str,
    active: Option<&CodexAccount>,
) -> Result<&'a CodexSwitchResult, CodexAccountManagerError> {
    pending
        .filter(|result| {
            result.switch_id.to_string() == switch_id
                && result
                    .ambient_account
                    .as_ref()
                    .zip(active)
                    .is_some_and(|(expected, current)| expected.matches(current))
        })
        .ok_or_else(|| {
            CodexAccountManagerError::Message(
                "The selected account changed after this restart prompt opened. Switch to the intended account again before restarting Codex Desktop."
                    .to_string(),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn sample_account() -> CodexAccount {
        CodexAccount::new(
            Uuid::new_v4(),
            None,
            Some("user@example.com".to_string()),
            Some("auth0|acct".to_string()),
            Some("acct".to_string()),
            std::path::PathBuf::from("/tmp/fake-home"),
            super::super::models::CodexAccountSource::ManagedByApp,
            super::super::models::utc_now(),
            super::super::models::utc_now(),
            Some(super::super::models::utc_now()),
        )
    }

    fn switch_result(outgoing: &CodexAccount, active: &CodexAccount) -> CodexSwitchResult {
        CodexSwitchResult {
            switch_id: Uuid::new_v4(),
            materialized_account: Some(outgoing.clone()),
            ambient_account: Some(active.clone()),
            backup_path: None,
            desktop_session_backup_path: Some(outgoing.codex_home_path.join("desktop-session")),
            desktop_session_restore_path: Some(active.codex_home_path.join("desktop-session")),
            desktop_session_restore_exists: false,
        }
    }

    #[test]
    fn removing_involved_account_revokes_pending_restart() {
        let mut outgoing = sample_account();
        outgoing.provider_account_id = Some("outgoing".into());
        outgoing.codex_home_path = "/tmp/outgoing".into();
        let active = sample_account();
        let mut unrelated = sample_account();
        unrelated.provider_account_id = Some("unrelated".into());
        unrelated.codex_home_path = "/tmp/unrelated".into();
        let result = switch_result(&outgoing, &active);
        let mut pending = Some(result.clone());

        invalidate_restart_for_removed_account(&mut pending, &unrelated);
        assert!(
            validate_pending_restart(
                pending.as_ref(),
                &result.switch_id.to_string(),
                Some(&active)
            )
            .is_ok()
        );

        invalidate_restart_for_removed_account(&mut pending, &outgoing);
        assert!(
            validate_pending_restart(
                pending.as_ref(),
                &result.switch_id.to_string(),
                Some(&active)
            )
            .is_err()
        );
    }

    #[test]
    fn restart_rejects_stale_prompt_and_external_identity_change() {
        let outgoing = sample_account();
        let active = sample_account();
        let result = switch_result(&outgoing, &active);
        let id = result.switch_id.to_string();

        assert!(validate_pending_restart(Some(&result), &id, Some(&active)).is_ok());
        assert!(
            validate_pending_restart(Some(&result), &Uuid::new_v4().to_string(), Some(&active))
                .is_err()
        );
        let mut other = active;
        other.provider_account_id = Some("different-account".into());
        assert!(validate_pending_restart(Some(&result), &id, Some(&other)).is_err());
        assert!(validate_pending_restart(None, &id, Some(&other)).is_err());
    }
}
