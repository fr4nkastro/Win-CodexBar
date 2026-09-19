use super::*;
use chrono::TimeZone;

#[test]
fn reset_diagnostic_codes_are_fixed_and_redacted() {
    let codes = [
        ResetDiagnosticReason::CandidateCreated.code(),
        ResetDiagnosticReason::SourceNotExactOAuth.code(),
        ResetDiagnosticReason::ExpiredCandidate.code(),
        ResetDiagnosticReason::ChangedCreditInventory.code(),
        ResetDiagnosticReason::StoreRequested.code(),
    ];
    assert_eq!(
        codes,
        [
            "candidateCreated",
            "sourceNotExactOAuth",
            "expiredCandidate",
            "changedCreditInventory",
            "storeRequested",
        ]
    );
    assert!(codes.iter().all(|code| {
        !code.contains('@') && !code.contains(':') && !code.contains('/') && !code.contains('\\')
    }));
}

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 25, 12, 0, 0).unwrap()
}

fn snapshot(used: f64, reset_days: i64, captured_minutes: i64) -> UsageSnapshot {
    let captured = now() + chrono::Duration::minutes(captured_minutes);
    let weekly = RateWindow::with_details(
        used,
        Some(7 * 24 * 60),
        Some(now() + chrono::Duration::days(reset_days)),
        None,
    );
    let mut snapshot = UsageSnapshot::new(RateWindow::new(20.0)).with_secondary(weekly);
    snapshot.updated_at = captured;
    snapshot.login_method = Some("ChatGPT Pro".to_string());
    snapshot
}

fn inventory(id: &str) -> CreditInventory {
    CreditInventory {
        available_count: 1,
        credits: vec![CreditIdentity {
            id: id.to_string(),
            reset_type: "weekly".to_string(),
            status: "available".to_string(),
            expires_at: Some(now() + chrono::Duration::days(3)),
        }],
    }
}

fn baseline() -> AccountState {
    let previous = snapshot(45.0, 2, 0);
    AccountState {
        published_weekly: previous.secondary.clone(),
        published_at: previous.updated_at,
        plan: previous.login_method.clone(),
        credit_inventory: Some(inventory("credit-a")),
        candidate: None,
    }
}

#[test]
fn inventory_retains_consumed_status_rows_but_counts_only_available_credits() {
    let reset = ResetCredits {
        available_count: 1,
        credits: vec![
            ResetCredit {
                id: Some("available-a".into()),
                reset_type: Some("weekly".into()),
                status: Some("available".into()),
                expires_at: None,
            },
            ResetCredit {
                id: Some("redeeming-b".into()),
                reset_type: Some("weekly".into()),
                status: Some("redeeming".into()),
                expires_at: None,
            },
            ResetCredit {
                id: Some("redeemed-c".into()),
                reset_type: Some("weekly".into()),
                status: Some("redeemed".into()),
                expires_at: None,
            },
        ],
    };
    let inventory = super::inventory(Some(&reset), now()).expect("credit inventory");
    assert_eq!(inventory.available_count, 1);
    assert_eq!(inventory.credits.len(), 3);
    assert!(
        inventory
            .credits
            .iter()
            .any(|credit| credit.status == "redeeming")
    );
    assert!(
        inventory
            .credits
            .iter()
            .any(|credit| credit.status == "redeemed")
    );
}
#[test]
fn early_low_usage_requires_confirmation_without_spending_credit() {
    let mut state = baseline();
    let initial = snapshot(0.0, 9, 1);
    let inv = inventory("credit-a");
    assert_eq!(
        initial_decision(&mut state, &initial, Some(&inv), true, now()),
        InitialDecision::RequiresConfirmation
    );
    let confirmation = snapshot(0.0, 9, 2);
    assert_eq!(
        confirmation_decision(
            &mut state,
            &initial,
            Some(&inv),
            &confirmation,
            Some(&inv),
            true,
            now(),
        ),
        ConfirmationDecision::Preserve
    );
    assert!(state.candidate.is_some());
    assert_eq!(state.credit_inventory.as_ref().unwrap().available_count, 1);
}

#[test]
fn delayed_candidate_publishes_after_sixty_seconds_and_expires_after_thirty_minutes() {
    let mut state = baseline();
    let initial = snapshot(0.0, 9, 1);
    let confirmation = snapshot(0.0, 9, 2);
    let inv = inventory("credit-a");
    assert_eq!(
        confirmation_decision(
            &mut state,
            &initial,
            Some(&inv),
            &confirmation,
            Some(&inv),
            true,
            now(),
        ),
        ConfirmationDecision::Preserve
    );
    let current = snapshot(0.0, 9, 3);
    let candidate = state.candidate.clone().unwrap();
    assert_eq!(
        delayed_candidate_decision(
            &state,
            &candidate,
            &current,
            Some(&inv),
            true,
            now() + chrono::Duration::seconds(59),
        ),
        DelayedDecision::Retain
    );
    assert_eq!(
        delayed_candidate_decision(
            &state,
            &candidate,
            &current,
            Some(&inv),
            true,
            now() + chrono::Duration::seconds(60),
        ),
        DelayedDecision::Publish
    );
    assert_eq!(
        delayed_candidate_decision(
            &state,
            &candidate,
            &current,
            Some(&inv),
            true,
            now() + chrono::Duration::minutes(31),
        ),
        DelayedDecision::Discard
    );
}

#[test]
fn credits_only_refresh_retains_candidate_and_account_scope_hashes_differ() {
    let mut state = baseline();
    state.candidate = Some(DelayedCandidate {
        evidence_version: EVIDENCE_VERSION,
        first_observed_at: now(),
        created_at: now(),
        snapshot_updated_at: now(),
        weekly: snapshot(0.0, 9, 1).secondary.unwrap(),
        plan: Some("ChatGPT Pro".to_string()),
        inventory: inventory("credit-a"),
    });
    let mut credits_only = UsageSnapshot::new(RateWindow::new(20.0));
    credits_only.updated_at = now() + chrono::Duration::minutes(1);
    // A credits-only refresh has no weekly window and may omit both plan and
    // reset-credit inventory. It must not consume the pending evidence.
    let candidate_before = serde_json::to_value(&state.candidate).unwrap();
    assert_eq!(
        initial_decision(
            &mut state,
            &credits_only,
            None,
            true,
            now() + chrono::Duration::minutes(1),
        ),
        InitialDecision::Preserve
    );
    assert_eq!(
        serde_json::to_value(&state.candidate).unwrap(),
        candidate_before
    );
    assert_ne!(
        scope_key(Some("account-a"), Path::new("C:/a/auth.json")),
        scope_key(Some("account-b"), Path::new("C:/b/auth.json"))
    );
}

#[test]
fn credits_only_refresh_candidate_survives_state_reload_until_full_usage() {
    let mut state = baseline();
    state.candidate = Some(DelayedCandidate {
        evidence_version: EVIDENCE_VERSION,
        first_observed_at: now(),
        created_at: now(),
        snapshot_updated_at: now(),
        weekly: snapshot(0.0, 9, 1).secondary.unwrap(),
        plan: Some("ChatGPT Pro".to_string()),
        inventory: inventory("credit-a"),
    });
    let candidate_before = serde_json::to_value(&state.candidate).unwrap();
    let mut credits_only = UsageSnapshot::new(RateWindow::new(20.0));
    credits_only.updated_at = now() + chrono::Duration::minutes(1);

    assert_eq!(
        initial_decision(
            &mut state,
            &credits_only,
            None,
            true,
            now() + chrono::Duration::minutes(1),
        ),
        InitialDecision::Preserve
    );

    // Model the StateFile envelope used by save/load without touching the
    // user's real LocalAppData during a unit test.
    let encoded = serde_json::to_vec(&StateFile {
        version: STATE_VERSION,
        accounts: HashMap::from([(String::from("scope"), state)]),
    })
    .unwrap();
    let mut reloaded_file: StateFile = serde_json::from_slice(&encoded).unwrap();
    let mut reloaded = reloaded_file.accounts.remove("scope").unwrap();
    assert_eq!(
        serde_json::to_value(&reloaded.candidate).unwrap(),
        candidate_before
    );

    let mut incompatible = reloaded.clone();
    let mut incompatible_usage = snapshot(0.0, 9, 3);
    incompatible_usage.login_method = Some("ChatGPT Plus".to_string());
    assert_eq!(
        initial_decision(
            &mut incompatible,
            &incompatible_usage,
            Some(&inventory("credit-a")),
            true,
            now() + chrono::Duration::seconds(60),
        ),
        InitialDecision::RequiresConfirmation
    );
    assert!(incompatible.candidate.is_none());

    let full_usage = snapshot(0.0, 9, 3);
    assert_eq!(
        initial_decision(
            &mut reloaded,
            &full_usage,
            Some(&inventory("credit-a")),
            true,
            now() + chrono::Duration::seconds(60),
        ),
        InitialDecision::Publish
    );
    assert!(reloaded.candidate.is_none());
}

#[test]
fn consumed_credit_allows_immediate_confirmation() {
    let mut state = baseline();
    let initial = snapshot(0.0, 2, 1);
    let confirmation = snapshot(0.0, 2, 2);
    let consumed = CreditInventory {
        available_count: 0,
        credits: Vec::new(),
    };
    assert_eq!(
        confirmation_decision(
            &mut state,
            &initial,
            Some(&consumed),
            &confirmation,
            Some(&consumed),
            true,
            now(),
        ),
        ConfirmationDecision::Publish
    );
}
