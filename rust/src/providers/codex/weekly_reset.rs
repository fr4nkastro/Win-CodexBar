use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::api::{ResetCredit, ResetCredits};
use crate::core::{RateWindow, UsageSnapshot};

const STATE_VERSION: u32 = 1;
const EVIDENCE_VERSION: u32 = 1;
const RESET_THRESHOLD: f64 = 1.0;
const RESET_TOLERANCE_SECONDS: i64 = 2 * 60;
const STABLE_BOUNDARY_TOLERANCE_SECONDS: i64 = 1;
const CANDIDATE_MINIMUM_AGE_SECONDS: i64 = 60;
const CANDIDATE_MAXIMUM_AGE_SECONDS: i64 = 30 * 60;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct StateFile {
    version: u32,
    accounts: HashMap<String, AccountState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AccountState {
    published_weekly: Option<RateWindow>,
    published_at: DateTime<Utc>,
    plan: Option<String>,
    credit_inventory: Option<CreditInventory>,
    candidate: Option<DelayedCandidate>,
}

impl Default for AccountState {
    fn default() -> Self {
        Self {
            published_weekly: None,
            published_at: DateTime::<Utc>::UNIX_EPOCH,
            plan: None,
            credit_inventory: None,
            candidate: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CreditInventory {
    available_count: u32,
    credits: Vec<CreditIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreditIdentity {
    id: String,
    reset_type: String,
    status: String,
    expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DelayedCandidate {
    evidence_version: u32,
    first_observed_at: DateTime<Utc>,
    created_at: DateTime<Utc>,
    snapshot_updated_at: DateTime<Utc>,
    weekly: RateWindow,
    plan: Option<String>,
    inventory: CreditInventory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InitialDecision {
    Publish,
    Preserve,
    RequiresConfirmation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ConfirmationDecision {
    Publish,
    Preserve,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DelayedDecision {
    Publish,
    Retain,
    Discard,
}

mod diagnostics;
use diagnostics::{ResetDiagnosticReason, log_reset_diagnostic};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResetCreditEvidence {
    None,
    Consumed,
    NoAvailableCredits,
}

pub(super) fn scope_key(account_id: Option<&str>, auth_path: &Path) -> String {
    let raw = account_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| format!("account:{value}"))
        .unwrap_or_else(|| format!("home:{}", auth_path.to_string_lossy().to_lowercase()));
    let digest = Sha256::digest(raw.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(super) fn load(scope: &str) -> AccountState {
    let Some(path) = state_path() else {
        return AccountState::default();
    };
    let Ok(raw) = crate::secure_file::read_string(&path) else {
        return AccountState::default();
    };
    let Ok(file) = serde_json::from_str::<StateFile>(&raw) else {
        return AccountState::default();
    };
    if file.version != STATE_VERSION {
        return AccountState::default();
    }
    file.accounts.get(scope).cloned().unwrap_or_default()
}

pub(super) fn save(scope: &str, state: &AccountState) {
    let Some(path) = state_path() else {
        log_reset_diagnostic(
            "candidatePersistence",
            "skipped",
            ResetDiagnosticReason::StoreUnavailable,
        );
        return;
    };
    let mut file = crate::secure_file::read_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<StateFile>(&raw).ok())
        .filter(|file| file.version == STATE_VERSION)
        .unwrap_or_else(|| StateFile {
            version: STATE_VERSION,
            accounts: HashMap::new(),
        });
    file.accounts.insert(scope.to_string(), state.clone());
    let Some(parent) = path.parent() else {
        log_reset_diagnostic(
            "candidatePersistence",
            "skipped",
            ResetDiagnosticReason::StoreUnavailable,
        );
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        log_reset_diagnostic(
            "candidatePersistence",
            "skipped",
            ResetDiagnosticReason::StoreUnavailable,
        );
        return;
    }
    if let Ok(raw) = serde_json::to_string_pretty(&file) {
        let _written = crate::secure_file::write_string(&path, &raw);
        log_reset_diagnostic(
            "candidatePersistence",
            "requested",
            ResetDiagnosticReason::StoreRequested,
        );
    }
}

fn state_path() -> Option<PathBuf> {
    dirs::data_local_dir().map(|root| root.join("CodexBar").join("codex-weekly-reset-v1.json"))
}

pub(super) fn inventory(
    reset: Option<&ResetCredits>,
    observed_at: DateTime<Utc>,
) -> Option<CreditInventory> {
    let reset = reset?;
    let mut credits = reset
        .credits
        .iter()
        .filter_map(|credit| credit_identity(credit, observed_at))
        .collect::<Vec<_>>();
    credits.sort_by(|left, right| {
        left.id
            .cmp(&right.id)
            .then_with(|| left.reset_type.cmp(&right.reset_type))
            .then_with(|| left.status.cmp(&right.status))
            .then_with(|| left.expires_at.cmp(&right.expires_at))
    });
    let available = credits
        .iter()
        .filter(|credit| credit_identity_is_available(credit))
        .count();
    if available != usize::try_from(reset.available_count).ok()? {
        return None;
    }
    Some(CreditInventory {
        available_count: reset.available_count,
        credits,
    })
}

fn credit_identity_is_available(credit: &CreditIdentity) -> bool {
    credit.status.is_empty() || credit.status.eq_ignore_ascii_case("available")
}

fn credit_identity(credit: &ResetCredit, observed_at: DateTime<Utc>) -> Option<CreditIdentity> {
    let id = credit.id.as_deref()?.trim();
    if id.is_empty() {
        return None;
    }
    let expires_at = credit
        .expires_at
        .as_deref()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc));
    if expires_at.is_some_and(|value| value <= observed_at) {
        return None;
    }
    Some(CreditIdentity {
        id: id.to_string(),
        reset_type: credit.reset_type.clone().unwrap_or_default(),
        status: credit
            .status
            .clone()
            .unwrap_or_else(|| "available".to_string()),
        expires_at,
    })
}

pub(super) fn initial_decision(
    state: &mut AccountState,
    current: &UsageSnapshot,
    current_inventory: Option<&CreditInventory>,
    exact_oauth: bool,
    observed_at: DateTime<Utc>,
) -> InitialDecision {
    if let Some(candidate) = state.candidate.clone() {
        match delayed_candidate_decision(
            state,
            &candidate,
            current,
            current_inventory,
            exact_oauth,
            observed_at,
        ) {
            DelayedDecision::Publish => {
                state.candidate = None;
                return InitialDecision::Publish;
            }
            DelayedDecision::Retain => return InitialDecision::Preserve,
            DelayedDecision::Discard => state.candidate = None,
        }
    }

    let Some(current_weekly) = weekly(current) else {
        return if state.published_weekly.is_some() {
            InitialDecision::Preserve
        } else {
            InitialDecision::Publish
        };
    };
    if !current_weekly.used_percent.is_finite()
        || !is_valid_boundary(current_weekly, current.updated_at)
    {
        return InitialDecision::Preserve;
    }

    let Some(previous_weekly) = state.published_weekly.as_ref() else {
        return if current_weekly.used_percent <= RESET_THRESHOLD {
            InitialDecision::RequiresConfirmation
        } else {
            InitialDecision::Publish
        };
    };
    if current.updated_at <= state.published_at || !previous_weekly.used_percent.is_finite() {
        return InitialDecision::Preserve;
    }
    if boundary_moves_backward(previous_weekly, current_weekly) {
        return InitialDecision::Preserve;
    }
    if previous_weekly.used_percent > RESET_THRESHOLD
        && current_weekly.used_percent <= RESET_THRESHOLD
    {
        InitialDecision::RequiresConfirmation
    } else {
        InitialDecision::Publish
    }
}

pub(super) fn confirmation_decision(
    state: &mut AccountState,
    initial: &UsageSnapshot,
    initial_inventory: Option<&CreditInventory>,
    confirmation: &UsageSnapshot,
    confirmation_inventory: Option<&CreditInventory>,
    exact_oauth: bool,
    observed_at: DateTime<Utc>,
) -> ConfirmationDecision {
    let Some(initial_weekly) = weekly(initial) else {
        return ConfirmationDecision::Preserve;
    };
    let Some(confirmation_weekly) = weekly(confirmation) else {
        return ConfirmationDecision::Preserve;
    };
    if confirmation.updated_at <= initial.updated_at
        || !initial_weekly.used_percent.is_finite()
        || !confirmation_weekly.used_percent.is_finite()
        || !is_valid_boundary(initial_weekly, initial.updated_at)
        || !is_valid_boundary(confirmation_weekly, confirmation.updated_at)
        || boundary_distance_seconds(initial_weekly, confirmation_weekly).abs()
            >= RESET_TOLERANCE_SECONDS
    {
        return ConfirmationDecision::Preserve;
    }
    if confirmation_weekly.used_percent > RESET_THRESHOLD {
        return ConfirmationDecision::Publish;
    }
    if initial_weekly.used_percent > RESET_THRESHOLD {
        return ConfirmationDecision::Preserve;
    }

    let Some(previous_weekly) = state.published_weekly.as_ref() else {
        return ConfirmationDecision::Publish;
    };
    let credit_evidence = reset_credit_evidence(
        state.credit_inventory.as_ref(),
        initial_inventory,
        confirmation_inventory,
        observed_at,
    );
    if let Some(previous_boundary) = previous_weekly.resets_at {
        if confirmation.updated_at
            < previous_boundary - chrono::Duration::seconds(RESET_TOLERANCE_SECONDS)
            && credit_evidence == ResetCreditEvidence::None
        {
            maybe_store_delayed_candidate(
                state,
                initial,
                confirmation,
                confirmation_inventory,
                exact_oauth,
                observed_at,
            );
            return ConfirmationDecision::Preserve;
        }
        let initial_advance = boundary_distance_seconds(previous_weekly, initial_weekly);
        let confirmation_advance = boundary_distance_seconds(previous_weekly, confirmation_weekly);
        if initial_advance < RESET_TOLERANCE_SECONDS
            || confirmation_advance < RESET_TOLERANCE_SECONDS
        {
            return if credit_evidence == ResetCreditEvidence::Consumed {
                ConfirmationDecision::Publish
            } else {
                maybe_store_delayed_candidate(
                    state,
                    initial,
                    confirmation,
                    confirmation_inventory,
                    exact_oauth,
                    observed_at,
                );
                ConfirmationDecision::Preserve
            };
        }
    }
    ConfirmationDecision::Publish
}

fn maybe_store_delayed_candidate(
    state: &mut AccountState,
    initial: &UsageSnapshot,
    confirmation: &UsageSnapshot,
    confirmation_inventory: Option<&CreditInventory>,
    exact_oauth: bool,
    observed_at: DateTime<Utc>,
) {
    if !exact_oauth {
        log_reset_diagnostic(
            "candidateCreation",
            "rejected",
            ResetDiagnosticReason::SourceNotExactOAuth,
        );
        return;
    }
    if !plans_match(state.plan.as_deref(), initial, confirmation) {
        log_reset_diagnostic(
            "candidateCreation",
            "rejected",
            ResetDiagnosticReason::PlanMismatch,
        );
        return;
    }
    let Some(previous_weekly) = state.published_weekly.as_ref() else {
        log_reset_diagnostic(
            "candidateCreation",
            "rejected",
            ResetDiagnosticReason::MissingPreviousSnapshot,
        );
        return;
    };
    let Some(initial_weekly) = weekly(initial) else {
        log_reset_diagnostic(
            "candidateCreation",
            "rejected",
            ResetDiagnosticReason::MissingWeeklyWindow,
        );
        return;
    };
    let Some(confirmation_weekly) = weekly(confirmation) else {
        log_reset_diagnostic(
            "candidateCreation",
            "rejected",
            ResetDiagnosticReason::MissingWeeklyWindow,
        );
        return;
    };
    let Some(previous_inventory) = state.credit_inventory.as_ref() else {
        log_reset_diagnostic(
            "candidateCreation",
            "rejected",
            ResetDiagnosticReason::MissingCreditInventory,
        );
        return;
    };
    let Some(confirmation_inventory) = confirmation_inventory else {
        log_reset_diagnostic(
            "candidateCreation",
            "rejected",
            ResetDiagnosticReason::MissingCreditInventory,
        );
        return;
    };
    if previous_inventory.available_count == 0 || previous_inventory != confirmation_inventory {
        log_reset_diagnostic(
            "candidateCreation",
            "rejected",
            ResetDiagnosticReason::ChangedCreditInventory,
        );
        return;
    }
    if !supported_delayed_boundary(previous_weekly, initial_weekly)
        || !supported_delayed_boundary(previous_weekly, confirmation_weekly)
    {
        log_reset_diagnostic(
            "candidateCreation",
            "rejected",
            ResetDiagnosticReason::UnsupportedResetBoundary,
        );
        return;
    }
    if boundary_distance_seconds(initial_weekly, confirmation_weekly).abs()
        >= RESET_TOLERANCE_SECONDS
    {
        log_reset_diagnostic(
            "candidateCreation",
            "rejected",
            ResetDiagnosticReason::InconsistentResetBoundary,
        );
        return;
    }
    state.candidate = Some(DelayedCandidate {
        evidence_version: EVIDENCE_VERSION,
        first_observed_at: initial.updated_at,
        created_at: observed_at,
        snapshot_updated_at: confirmation.updated_at,
        weekly: confirmation_weekly.clone(),
        plan: confirmation.login_method.clone(),
        inventory: confirmation_inventory.clone(),
    });
    log_reset_diagnostic(
        "candidateCreation",
        "created",
        ResetDiagnosticReason::CandidateCreated,
    );
}

fn delayed_candidate_decision(
    state: &AccountState,
    candidate: &DelayedCandidate,
    current: &UsageSnapshot,
    current_inventory: Option<&CreditInventory>,
    exact_oauth: bool,
    observed_at: DateTime<Utc>,
) -> DelayedDecision {
    let age = observed_at
        .signed_duration_since(candidate.created_at)
        .num_seconds();
    if candidate.evidence_version != EVIDENCE_VERSION {
        log_reset_diagnostic(
            "delayedCandidate",
            "discard",
            ResetDiagnosticReason::EvidenceVersionMismatch,
        );
        return DelayedDecision::Discard;
    }
    if age < 0 {
        log_reset_diagnostic(
            "delayedCandidate",
            "discard",
            ResetDiagnosticReason::FutureCandidate,
        );
        return DelayedDecision::Discard;
    }
    if age > CANDIDATE_MAXIMUM_AGE_SECONDS {
        log_reset_diagnostic(
            "delayedCandidate",
            "discard",
            ResetDiagnosticReason::ExpiredCandidate,
        );
        return DelayedDecision::Discard;
    }
    if !exact_oauth {
        log_reset_diagnostic(
            "delayedCandidate",
            "discard",
            ResetDiagnosticReason::SourceNotExactOAuth,
        );
        return DelayedDecision::Discard;
    }
    let Some(previous_weekly) = state.published_weekly.as_ref() else {
        log_reset_diagnostic(
            "delayedCandidate",
            "discard",
            ResetDiagnosticReason::MissingPreviousSnapshot,
        );
        return DelayedDecision::Discard;
    };
    let Some(current_weekly) = weekly(current) else {
        // Credits-only refreshes do not carry the weekly window (or necessarily
        // the plan/inventory fields). Preserve the candidate and let the next
        // complete usage observation validate it.
        log_reset_diagnostic(
            "delayedCandidate",
            "retain",
            ResetDiagnosticReason::MissingWeeklyWindow,
        );
        return DelayedDecision::Retain;
    };
    if !plans_match(state.plan.as_deref(), current, current) {
        log_reset_diagnostic(
            "delayedCandidate",
            "discard",
            ResetDiagnosticReason::PlanMismatch,
        );
        return DelayedDecision::Discard;
    }
    if previous_weekly.used_percent <= RESET_THRESHOLD
        || current_weekly.used_percent > RESET_THRESHOLD
    {
        log_reset_diagnostic(
            "delayedCandidate",
            "discard",
            ResetDiagnosticReason::ResetThresholdMismatch,
        );
        return DelayedDecision::Discard;
    }
    if current.updated_at <= candidate.snapshot_updated_at {
        log_reset_diagnostic(
            "delayedCandidate",
            "discard",
            ResetDiagnosticReason::StaleObservation,
        );
        return DelayedDecision::Discard;
    }
    if !is_valid_boundary(current_weekly, current.updated_at) {
        log_reset_diagnostic(
            "delayedCandidate",
            "discard",
            ResetDiagnosticReason::InvalidResetBoundary,
        );
        return DelayedDecision::Discard;
    }
    if boundary_distance_seconds(&candidate.weekly, current_weekly).abs() >= RESET_TOLERANCE_SECONDS
    {
        log_reset_diagnostic(
            "delayedCandidate",
            "discard",
            ResetDiagnosticReason::InconsistentResetBoundary,
        );
        return DelayedDecision::Discard;
    }
    if !supported_delayed_boundary(previous_weekly, current_weekly) {
        log_reset_diagnostic(
            "delayedCandidate",
            "discard",
            ResetDiagnosticReason::UnsupportedResetBoundary,
        );
        return DelayedDecision::Discard;
    }
    if current_inventory != Some(&candidate.inventory) {
        log_reset_diagnostic(
            "delayedCandidate",
            "discard",
            ResetDiagnosticReason::ChangedCreditInventory,
        );
        return DelayedDecision::Discard;
    }
    if age >= CANDIDATE_MINIMUM_AGE_SECONDS {
        log_reset_diagnostic(
            "delayedCandidate",
            "publish",
            ResetDiagnosticReason::ConfirmedObservation,
        );
        DelayedDecision::Publish
    } else {
        log_reset_diagnostic(
            "delayedCandidate",
            "retain",
            ResetDiagnosticReason::MinimumDelay,
        );
        DelayedDecision::Retain
    }
}

pub(super) fn commit_publication(
    state: &mut AccountState,
    snapshot: &UsageSnapshot,
    inventory: Option<CreditInventory>,
) {
    if let Some(weekly) = weekly(snapshot) {
        state.published_weekly = Some(weekly.clone());
        state.published_at = snapshot.updated_at;
        state.plan = snapshot.login_method.clone();
        state.credit_inventory = inventory;
        state.candidate = None;
    }
}

pub(super) fn preserve_weekly(state: &AccountState, mut current: UsageSnapshot) -> UsageSnapshot {
    if let Some(previous) = state.published_weekly.clone() {
        current.secondary = Some(previous);
    }
    current
}

fn weekly(snapshot: &UsageSnapshot) -> Option<&RateWindow> {
    snapshot.secondary.as_ref()
}

fn is_valid_boundary(window: &RateWindow, captured_at: DateTime<Utc>) -> bool {
    window
        .resets_at
        .is_some_and(|boundary| boundary > captured_at)
}

fn boundary_distance_seconds(left: &RateWindow, right: &RateWindow) -> i64 {
    match (left.resets_at, right.resets_at) {
        (Some(left), Some(right)) => right.signed_duration_since(left).num_seconds(),
        _ => i64::MIN / 2,
    }
}

fn boundary_moves_backward(previous: &RateWindow, current: &RateWindow) -> bool {
    boundary_distance_seconds(previous, current) < -RESET_TOLERANCE_SECONDS
}

fn supported_delayed_boundary(previous: &RateWindow, current: &RateWindow) -> bool {
    let distance = boundary_distance_seconds(previous, current);
    distance.abs() < STABLE_BOUNDARY_TOLERANCE_SECONDS || distance >= RESET_TOLERANCE_SECONDS
}

fn plans_match(previous: Option<&str>, left: &UsageSnapshot, right: &UsageSnapshot) -> bool {
    let normalize = |value: Option<&str>| {
        value
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_lowercase)
    };
    let previous = normalize(previous);
    let left = normalize(left.login_method.as_deref());
    let right = normalize(right.login_method.as_deref());
    previous.is_some() && previous == left && left == right
}

fn reset_credit_evidence(
    previous: Option<&CreditInventory>,
    initial: Option<&CreditInventory>,
    confirmation: Option<&CreditInventory>,
    observed_at: DateTime<Utc>,
) -> ResetCreditEvidence {
    let (Some(previous), Some(initial), Some(confirmation)) = (previous, initial, confirmation)
    else {
        return ResetCreditEvidence::None;
    };
    if previous.available_count == 0 {
        return ResetCreditEvidence::NoAvailableCredits;
    }
    for prior in &previous.credits {
        let consumed_in = |current: &CreditInventory| {
            if let Some(credit) = current.credits.iter().find(|credit| credit.id == prior.id) {
                return credit.status.eq_ignore_ascii_case("redeeming")
                    || credit.status.eq_ignore_ascii_case("redeemed");
            }
            let still_valid = prior.expires_at.is_none_or(|expiry| expiry > observed_at);
            still_valid && current.available_count < previous.available_count
        };
        if consumed_in(initial) && consumed_in(confirmation) {
            return ResetCreditEvidence::Consumed;
        }
    }
    ResetCreditEvidence::None
}

#[cfg(test)]
mod tests;
