//! Identity-based grouping of Claude accounts for usage-probe coalescing.
//!
//! A single refresh cycle must issue at most one `GET /api/oauth/usage` per
//! distinct Claude identity (spec `claude-usage-refresh`). Ambient and managed
//! accounts that resolve to the same identity share one usage result; the
//! ambient lane always leads its group so only it can rotate the ambient
//! refresh token (design decision D3/D5).
//!
//! This lives in the `codexbar` crate (not the Tauri command layer) so it is
//! unit-testable with `cargo test -p codexbar` — the Tauri crate cannot link
//! under WSL2 (design decision D2).

use std::collections::HashMap;

use super::models::{ClaudeAccount, ClaudeAccountSource};

/// One identity lane: the account whose fetch runs for the group (`leader`)
/// plus every account sharing that identity (`members`, including the leader),
/// all as indices into the slice passed to [`group_lanes_by_identity`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneGroup {
    /// Index of the account whose lane performs the single usage fetch.
    pub leader: usize,
    /// Indices of every account in this lane, in input order.
    pub members: Vec<usize>,
}

/// Normalized identity key for one account.
///
/// Precedence, matching `ClaudeAccount`'s existing normalization: normalized
/// `org_id` → normalized `email_hint` → standardized (absolute, lowercased)
/// config directory. The prefixes keep the three key spaces from colliding.
fn identity_key(account: &ClaudeAccount) -> String {
    if let Some(org_id) = account.normalized_org_id() {
        return format!("org:{org_id}");
    }
    if let Some(email) = account.normalized_email_hint() {
        return format!("email:{email}");
    }
    format!("dir:{}", account.standardized_config_dir())
}

/// Group accounts that share one Claude identity into lanes.
///
/// Groups are returned in first-seen order. Within a group the leader is the
/// ambient member if one exists, otherwise the member with the lowest `id`
/// (deterministic). `members` preserves input order.
pub fn group_lanes_by_identity(accounts: &[ClaudeAccount]) -> Vec<LaneGroup> {
    let mut order: Vec<String> = Vec::new();
    let mut members_by_key: HashMap<String, Vec<usize>> = HashMap::new();

    for (idx, account) in accounts.iter().enumerate() {
        let key = identity_key(account);
        members_by_key
            .entry(key.clone())
            .or_insert_with(|| {
                order.push(key.clone());
                Vec::new()
            })
            .push(idx);
    }

    order
        .into_iter()
        .map(|key| {
            let members = members_by_key.remove(&key).expect("key was inserted above");
            let leader = members
                .iter()
                .copied()
                .min_by(|&a, &b| {
                    let a_ambient = accounts[a].source == ClaudeAccountSource::Ambient;
                    let b_ambient = accounts[b].source == ClaudeAccountSource::Ambient;
                    // Ambient sorts first, then lowest id.
                    b_ambient
                        .cmp(&a_ambient)
                        .then_with(|| accounts[a].id.cmp(&accounts[b].id))
                })
                .expect("group is never empty");
            LaneGroup { leader, members }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_accounts::models::utc_now;
    use std::path::PathBuf;
    use uuid::Uuid;

    fn account(
        id: &str,
        dir: &str,
        org_id: Option<&str>,
        email: Option<&str>,
        source: ClaudeAccountSource,
    ) -> ClaudeAccount {
        ClaudeAccount::new(
            Uuid::parse_str(id).unwrap(),
            None,
            email.map(str::to_string),
            org_id.map(str::to_string),
            None,
            None,
            PathBuf::from(dir),
            source,
            utc_now(),
            utc_now(),
            None,
        )
    }

    const ID_A: &str = "11111111-1111-1111-1111-111111111111";
    const ID_B: &str = "22222222-2222-2222-2222-222222222222";
    const ID_C: &str = "33333333-3333-3333-3333-333333333333";

    #[test]
    fn single_account_is_its_own_group() {
        let accounts = vec![account(
            ID_A,
            "/managed/a",
            Some("org-x"),
            Some("a@x.com"),
            ClaudeAccountSource::Ambient,
        )];
        let groups = group_lanes_by_identity(&accounts);
        assert_eq!(
            groups,
            vec![LaneGroup {
                leader: 0,
                members: vec![0]
            }]
        );
    }

    #[test]
    fn ambient_and_managed_twin_collapse_with_ambient_leading() {
        // Managed twin listed first, ambient second — ambient must still lead.
        let accounts = vec![
            account(
                ID_A,
                "/managed/twin",
                Some("ORG-X"),
                Some("twin@x.com"),
                ClaudeAccountSource::ManagedByApp,
            ),
            account(
                ID_B,
                "/home/.claude",
                Some("org-x"),
                Some("ambient@x.com"),
                ClaudeAccountSource::Ambient,
            ),
        ];
        let groups = group_lanes_by_identity(&accounts);
        assert_eq!(groups.len(), 1, "same org_id collapses to one group");
        assert_eq!(groups[0].leader, 1, "ambient member leads");
        assert_eq!(groups[0].members, vec![0, 1]);
    }

    #[test]
    fn distinct_orgs_stay_separate() {
        let accounts = vec![
            account(
                ID_A,
                "/home/.claude",
                Some("org-x"),
                Some("shared@x.com"),
                ClaudeAccountSource::Ambient,
            ),
            account(
                ID_B,
                "/managed/y",
                Some("org-y"),
                Some("shared@x.com"),
                ClaudeAccountSource::ManagedByApp,
            ),
        ];
        let groups = group_lanes_by_identity(&accounts);
        assert_eq!(groups.len(), 2, "different org_id => two groups");
        assert_eq!(groups[0].members, vec![0]);
        assert_eq!(groups[1].members, vec![1]);
    }

    #[test]
    fn missing_org_id_falls_back_to_email_key() {
        let accounts = vec![
            account(
                ID_A,
                "/managed/a",
                None,
                Some("dupe@x.com"),
                ClaudeAccountSource::ManagedByApp,
            ),
            account(
                ID_B,
                "/managed/b",
                None,
                Some("DUPE@X.COM"),
                ClaudeAccountSource::ManagedByApp,
            ),
        ];
        let groups = group_lanes_by_identity(&accounts);
        assert_eq!(groups.len(), 1, "same normalized email => one group");
        // No ambient member: lowest id (ID_A at index 0) leads.
        assert_eq!(groups[0].leader, 0);
        assert_eq!(groups[0].members, vec![0, 1]);
    }

    #[test]
    fn missing_org_and_email_falls_back_to_dir_key() {
        let accounts = vec![
            account(
                ID_A,
                "/managed/only-a",
                None,
                None,
                ClaudeAccountSource::ManagedByApp,
            ),
            account(
                ID_B,
                "/managed/only-b",
                None,
                None,
                ClaudeAccountSource::ManagedByApp,
            ),
        ];
        let groups = group_lanes_by_identity(&accounts);
        assert_eq!(groups.len(), 2, "different dirs, no identity => two groups");
    }

    #[test]
    fn two_managed_dirs_one_identity_keep_both_member_ids() {
        let accounts = vec![
            account(
                ID_C,
                "/managed/dir-1",
                Some("org-shared"),
                None,
                ClaudeAccountSource::ManagedByApp,
            ),
            account(
                ID_B,
                "/managed/dir-2",
                Some("org-shared"),
                None,
                ClaudeAccountSource::ManagedByApp,
            ),
        ];
        let groups = group_lanes_by_identity(&accounts);
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].members,
            vec![0, 1],
            "both dirs render from one lane"
        );
        // No ambient: lowest id wins. ID_B < ID_C, ID_B is at index 1.
        assert_eq!(groups[0].leader, 1);
    }
}
