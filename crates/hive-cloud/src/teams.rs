//! Teams — the top-level ownership/tenancy unit. Every project belongs to a
//! team; access to previews and dashboards is gated by team membership. This
//! mirrors Vercel/Railway's team model (owner > admin > member > viewer).
//!
//! Membership is keyed by email so it lines up with Clerk identities in the UI.
//! The store is in-memory + persisted via the platform snapshot.

use hive_core::now_ms;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

/// A team member's role. Roles are ordered: Owner > Admin > Member > Viewer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Owner,
    Admin,
    Member,
    Viewer,
}
impl Role {
    pub fn rank(&self) -> u8 {
        match self {
            Role::Owner => 3,
            Role::Admin => 2,
            Role::Member => 1,
            Role::Viewer => 0,
        }
    }
    /// Can this role see protected previews? (everyone who is a member can.)
    pub fn can_view(&self) -> bool {
        true
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Member {
    pub email: String,
    pub role: Role,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub added_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Team {
    /// URL-safe slug, e.g. "dylans-projects" — also the tenant identifier.
    pub slug: String,
    pub name: String,
    /// hobby | pro | enterprise
    #[serde(default = "default_plan")]
    pub plan: String,
    #[serde(default)]
    pub created_ms: u64,
    #[serde(default)]
    pub members: Vec<Member>,
    /// Optional team/org SSO (SAML/OIDC) — an Enterprise capability.
    #[serde(default)]
    pub sso_enabled: bool,
    /// Causal version of the whole aggregate, including its member vector.
    /// `0` is a legacy row and is never promoted merely by loading it.
    #[serde(default)]
    pub updated_ms: u64,
    /// A locally-created availability fallback, not authoritative tenant state.
    /// It never crosses a persistence or replication boundary, so an older
    /// binary cannot strip provenance and relay the placeholder as real.
    #[serde(skip)]
    pub(crate) synthetic_seed: bool,
}
fn default_plan() -> String {
    "pro".into()
}

impl Team {
    pub fn member(&self, email: &str) -> Option<&Member> {
        let e = email.to_lowercase();
        self.members.iter().find(|m| m.email.to_lowercase() == e)
    }
}

/// Replication payload: live aggregates plus permanent deletion generations.
/// A bare legacy map is still accepted by `store_sync` as tombstone-free input.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SyncedTeams {
    pub rows: BTreeMap<String, Team>,
    #[serde(default)]
    pub tombstones: BTreeMap<String, u64>,
}

#[derive(Default)]
struct TeamState {
    teams: HashMap<String, Team>,
    tombstones: BTreeMap<String, u64>,
}

pub struct TeamStore {
    // Rows and tombstones share one lock so no snapshot can observe the gap
    // between removing a row and recording why it is absent.
    state: RwLock<TeamState>,
}

fn time_ceiling(now: u64) -> u64 {
    now.saturating_add(hive_edge::region::MAX_GOSSIP_FUTURE_SKEW_MS * 2)
}

fn next_version(previous: u64, tombstone: u64) -> u64 {
    // This is a Lamport floor with wall-clock readability, not a wall-clock
    // timestamp that may move backward. Remote future values are rejected at
    // ingress; once a generation is held locally it must never be rebased down.
    now_ms()
        .max(previous.saturating_add(1))
        .max(tombstone.saturating_add(1))
}

fn tie_break_take(local: &Team, remote: &Team) -> bool {
    match (serde_json::to_vec(local), serde_json::to_vec(remote)) {
        (Ok(local), Ok(remote)) => remote > local,
        (Err(_), Ok(_)) => true,
        _ => false,
    }
}

/// Synthetic boot availability must never outrank real tenant state, including
/// a legacy real row whose causal version is still zero. Within one provenance
/// class, generation then canonical whole-row bytes form the deterministic
/// total order used by every observer.
fn row_take(local: &Team, remote: &Team) -> bool {
    match (local.synthetic_seed, remote.synthetic_seed) {
        (true, false) => true,
        (false, true) => false,
        _ if remote.updated_ms > local.updated_ms => true,
        _ if remote.updated_ms < local.updated_ms => false,
        _ => tie_break_take(local, remote),
    }
}

fn survives_tombstone(team: &Team, tombstone: Option<u64>) -> bool {
    match tombstone {
        None => true,
        Some(_) if team.synthetic_seed => false,
        Some(deleted_ms) => team.updated_ms > deleted_ms,
    }
}

impl TeamStore {
    pub fn new() -> TeamStore {
        TeamStore {
            state: RwLock::new(TeamState::default()),
        }
    }

    /// Seed the initial personal team only when the store has never held one.
    /// A retained tombstone means an explicit deletion and must dominate boot.
    pub fn ensure_seed(&self, owner_email: &str) {
        let mut state = self.state.write();
        let slug = "personal".to_string();
        if state.teams.is_empty() && !state.tombstones.contains_key(&slug) {
            let version = next_version(0, 0);
            state.teams.insert(
                slug.clone(),
                Team {
                    slug,
                    name: "Personal".into(),
                    plan: "pro".into(),
                    created_ms: now_ms(),
                    members: vec![Member {
                        email: owner_email.to_string(),
                        role: Role::Owner,
                        name: "Owner".into(),
                        added_ms: now_ms(),
                    }],
                    sso_enabled: false,
                    updated_ms: version,
                    synthetic_seed: true,
                },
            );
        }
    }

    pub fn list(&self) -> Vec<Team> {
        let mut v: Vec<Team> = self.state.read().teams.values().cloned().collect();
        v.sort_by(|a, b| {
            a.created_ms
                .cmp(&b.created_ms)
                .then_with(|| a.slug.cmp(&b.slug))
        });
        v
    }

    /// Tenant state safe to project into shared durable stores. The synthetic
    /// personal fallback is visible locally but is not an authoritative row.
    pub fn list_authoritative(&self) -> Vec<Team> {
        let mut v: Vec<Team> = self
            .state
            .read()
            .teams
            .values()
            .filter(|team| !team.synthetic_seed)
            .cloned()
            .collect();
        v.sort_by(|a, b| {
            a.created_ms
                .cmp(&b.created_ms)
                .then_with(|| a.slug.cmp(&b.slug))
        });
        v
    }

    pub fn get(&self, slug: &str) -> Option<Team> {
        self.state.read().teams.get(slug).cloned()
    }

    pub fn snapshot(&self) -> HashMap<String, Team> {
        self.state.read().teams.clone()
    }

    pub fn snapshot_synced(&self) -> SyncedTeams {
        let state = self.state.read();
        SyncedTeams {
            // Synthetic personal is a node-local availability placeholder. Never
            // publish it: an older peer would ignore the provenance field and
            // could relay it back as authoritative real tenant state.
            rows: state
                .teams
                .iter()
                .filter(|(_, team)| !team.synthetic_seed)
                .map(|(slug, team)| (slug.clone(), team.clone()))
                .collect(),
            tombstones: state.tombstones.clone(),
        }
    }

    /// Load legacy/persisted live rows. Tombstones load separately immediately
    /// afterward; `tombstones_load` performs the final dominance filter.
    pub fn load(&self, mut data: HashMap<String, Team>) {
        for (slug, team) in &mut data {
            if team.slug != *slug {
                tracing::warn!(team = %slug, embedded_slug = %team.slug, "normalizing mismatched persisted team slug");
                team.slug = slug.clone();
            }
        }
        let mut state = self.state.write();
        state.teams = data;
        let tombstones = state.tombstones.clone();
        state
            .teams
            .retain(|slug, team| survives_tombstone(team, tombstones.get(slug).copied()));
    }

    pub fn tombstones_snapshot(&self) -> BTreeMap<String, u64> {
        self.state.read().tombstones.clone()
    }

    pub fn tombstones_load(&self, data: BTreeMap<String, u64>) {
        let mut state = self.state.write();
        state.tombstones = data;
        let tombstones = state.tombstones.clone();
        state
            .teams
            .retain(|slug, team| survives_tombstone(team, tombstones.get(slug).copied()));
    }

    /// Per-team generation merge for live gossip. Absence never deletes; a
    /// tombstone at or after a row does. Implausibly future live inputs are
    /// refused at this ingress boundary.
    pub fn merge_synced(&self, remote: SyncedTeams) -> usize {
        self.merge(remote, false)
    }

    /// Merge a previously durable snapshot. Already-held Lamport generations
    /// remain valid across a local wall-clock rollback and must not be rebased
    /// or discarded merely because they now look future-dated.
    pub fn merge_recovered(&self, remote: SyncedTeams) -> usize {
        self.merge(remote, true)
    }

    fn merge(&self, remote: SyncedTeams, recovered: bool) -> usize {
        let now = now_ms();
        let ceiling = time_ceiling(now);
        let SyncedTeams { rows, tombstones } = remote;
        let mut state = self.state.write();
        let mut changed = 0usize;

        for (slug, deleted_ms) in tombstones {
            if !recovered && deleted_ms > ceiling {
                tracing::warn!(team = %slug, deleted_ms, ceiling_ms = ceiling, "dropping relayed team tombstone with implausibly future generation");
                continue;
            }
            match state.tombstones.get_mut(&slug) {
                Some(current) if deleted_ms > *current => {
                    *current = deleted_ms;
                    changed += 1;
                }
                None => {
                    state.tombstones.insert(slug, deleted_ms);
                    changed += 1;
                }
                _ => {}
            }
        }

        for (slug, mut remote_team) in rows {
            if remote_team.slug != slug {
                tracing::warn!(team = %slug, embedded_slug = %remote_team.slug, "normalizing mismatched relayed team slug");
                remote_team.slug = slug.clone();
            }
            if !recovered && remote_team.updated_ms > ceiling {
                tracing::warn!(team = %slug, updated_ms = remote_team.updated_ms, ceiling_ms = ceiling, "dropping relayed team row with implausibly future generation");
                continue;
            }
            if !survives_tombstone(&remote_team, state.tombstones.get(&slug).copied()) {
                continue;
            }
            let take = match state.teams.get(&slug) {
                None => true,
                Some(local) => row_take(local, &remote_team),
            };
            if take {
                state.teams.insert(slug, remote_team);
                changed += 1;
            }
        }
        let tombstones = state.tombstones.clone();
        let rows_before_tombstones = state.teams.len();
        state
            .teams
            .retain(|slug, team| survives_tombstone(team, tombstones.get(slug).copied()));
        changed += rows_before_tombstones - state.teams.len();
        changed
    }

    pub fn create(&self, name: &str, plan: &str, owner_email: &str) -> Team {
        let base = slugify(name);
        let mut slug = base.clone();
        let mut state = self.state.write();
        // Ensure unique among LIVE rows. The old one-shot `% 10000` suffix
        // could collide and overwrite an existing aggregate. Keep extending a
        // deterministic candidate until insertion is provably non-destructive.
        if state.teams.contains_key(&slug) {
            let nonce = now_ms() % 10000;
            slug = format!("{base}-{nonce}");
            let mut sequence = 2u64;
            while state.teams.contains_key(&slug) {
                slug = format!("{base}-{nonce}-{sequence}");
                sequence = sequence.saturating_add(1);
            }
        }
        let version = next_version(0, state.tombstones.get(&slug).copied().unwrap_or_default());
        let team = Team {
            slug: slug.clone(),
            name: name.to_string(),
            plan: plan.to_string(),
            created_ms: now_ms(),
            members: vec![Member {
                email: owner_email.to_string(),
                role: Role::Owner,
                name: String::new(),
                added_ms: now_ms(),
            }],
            sso_enabled: false,
            updated_ms: version,
            synthetic_seed: false,
        };
        state.teams.insert(slug, team.clone());
        team
    }

    /// Create a team at an EXACT, caller-supplied slug — never re-derived or
    /// suffixed. Returns `None` if that exact LIVE slug is already taken.
    pub fn create_with_slug(
        &self,
        slug: &str,
        name: &str,
        plan: &str,
        owner_email: &str,
    ) -> Option<Team> {
        let mut state = self.state.write();
        if state.teams.contains_key(slug) {
            return None;
        }
        let version = next_version(0, state.tombstones.get(slug).copied().unwrap_or_default());
        let team = Team {
            slug: slug.to_string(),
            name: name.to_string(),
            plan: plan.to_string(),
            created_ms: now_ms(),
            members: vec![Member {
                email: owner_email.to_string(),
                role: Role::Owner,
                name: String::new(),
                added_ms: now_ms(),
            }],
            sso_enabled: false,
            updated_ms: version,
            synthetic_seed: false,
        };
        state.teams.insert(slug.to_string(), team.clone());
        Some(team)
    }

    /// Toggle team/org SSO (Enterprise-only — caller enforces the plan gate).
    pub fn set_sso(&self, slug: &str, enabled: bool) -> Option<Team> {
        let mut state = self.state.write();
        let tombstone = state.tombstones.get(slug).copied().unwrap_or_default();
        let team = state.teams.get_mut(slug)?;
        if team.sso_enabled != enabled {
            team.sso_enabled = enabled;
            team.synthetic_seed = false;
            team.updated_ms = next_version(team.updated_ms, tombstone);
        }
        Some(team.clone())
    }

    /// Update a team's plan/tier (hobby | pro | enterprise).
    pub fn set_plan(&self, slug: &str, plan: &str) -> Option<Team> {
        let mut state = self.state.write();
        let tombstone = state.tombstones.get(slug).copied().unwrap_or_default();
        let team = state.teams.get_mut(slug)?;
        if team.plan != plan {
            team.plan = plan.to_string();
            team.synthetic_seed = false;
            team.updated_ms = next_version(team.updated_ms, tombstone);
        }
        Some(team.clone())
    }

    /// Delete a team and atomically retain its causal tombstone. Returns the
    /// removed record, or `None` if there was no such live team.
    pub fn remove(&self, slug: &str) -> Option<Team> {
        let mut state = self.state.write();
        let team = state.teams.remove(slug)?;
        let prior = state.tombstones.get(slug).copied().unwrap_or_default();
        let deleted_ms = next_version(team.updated_ms, prior);
        state.tombstones.insert(slug.to_string(), deleted_ms);
        Some(team)
    }

    pub fn add_member(&self, slug: &str, email: &str, role: Role) -> Option<Team> {
        let mut state = self.state.write();
        let tombstone = state.tombstones.get(slug).copied().unwrap_or_default();
        let team = state.teams.get_mut(slug)?;
        let email_lower = email.to_lowercase();
        let mut changed = false;
        if let Some(existing) = team
            .members
            .iter_mut()
            .find(|member| member.email.to_lowercase() == email_lower)
        {
            if existing.role != role {
                existing.role = role;
                changed = true;
            }
        } else {
            team.members.push(Member {
                email: email.to_string(),
                role,
                name: String::new(),
                added_ms: now_ms(),
            });
            changed = true;
        }
        if changed {
            team.synthetic_seed = false;
            team.updated_ms = next_version(team.updated_ms, tombstone);
        }
        Some(team.clone())
    }

    pub fn remove_member(&self, slug: &str, email: &str) -> Option<Team> {
        let mut state = self.state.write();
        let tombstone = state.tombstones.get(slug).copied().unwrap_or_default();
        let team = state.teams.get_mut(slug)?;
        let email_lower = email.to_lowercase();
        let before = team.members.len();
        team.members
            .retain(|member| member.email.to_lowercase() != email_lower);
        if team.members.len() != before {
            team.synthetic_seed = false;
            team.updated_ms = next_version(team.updated_ms, tombstone);
        }
        Some(team.clone())
    }

    /// Is `email` a member of `slug` (for preview access control)?
    pub fn is_member(&self, slug: &str, email: &str) -> bool {
        self.state
            .read()
            .teams
            .get(slug)
            .is_some_and(|team| team.member(email).is_some())
    }

    pub fn count(&self) -> usize {
        self.state.read().teams.len()
    }
}

impl Default for TeamStore {
    fn default() -> Self {
        Self::new()
    }
}

pub fn slugify(s: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in s.trim().to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "team".into()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_makes_url_safe_slugs() {
        assert_eq!(slugify("My Team!"), "my-team");
        assert_eq!(slugify("  Acme   Corp  "), "acme-corp");
        assert_eq!(slugify("---"), "team");
        assert_eq!(slugify(""), "team");
        assert_eq!(slugify("Block/Offsets"), "block-offsets");
    }

    #[test]
    fn team_create_dedupes_slugs() {
        let store = TeamStore::new();
        let a = store.create("Acme", "pro", "owner@x.com");
        let b = store.create("Acme", "pro", "owner@x.com");
        assert_eq!(a.slug, "acme");
        assert_ne!(a.slug, b.slug, "duplicate name must get a unique slug");
        // Owner is a member with the Owner role.
        assert!(store.is_member("acme", "owner@x.com"));
    }

    #[test]
    fn ensure_seed_creates_personal_once() {
        let store = TeamStore::new();
        store.ensure_seed("owner@x.com");
        store.ensure_seed("owner@x.com");
        assert_eq!(store.count(), 1);
        assert!(store.get("personal").is_some());
    }
}
