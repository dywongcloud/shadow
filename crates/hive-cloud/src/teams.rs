//! Teams — the top-level ownership/tenancy unit. Every project belongs to a
//! team; access to previews and dashboards is gated by team membership. This
//! mirrors Vercel/Railway's team model (owner > admin > member > viewer).
//!
//! Membership is keyed by email so it lines up with Clerk identities in the UI.
//! The store is in-memory + persisted via the platform snapshot.

use hive_core::now_ms;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Member {
    pub email: String,
    pub role: Role,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub added_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
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

pub struct TeamStore {
    teams: RwLock<HashMap<String, Team>>,
}

impl TeamStore {
    pub fn new() -> TeamStore {
        TeamStore { teams: RwLock::new(HashMap::new()) }
    }

    /// Seed a default personal team owned by the platform owner if empty.
    pub fn ensure_seed(&self, owner_email: &str) {
        let mut m = self.teams.write();
        if m.is_empty() {
            let slug = "personal".to_string();
            m.insert(
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
                },
            );
        }
    }

    pub fn list(&self) -> Vec<Team> {
        let mut v: Vec<Team> = self.teams.read().values().cloned().collect();
        v.sort_by(|a, b| a.created_ms.cmp(&b.created_ms));
        v
    }

    pub fn get(&self, slug: &str) -> Option<Team> {
        self.teams.read().get(slug).cloned()
    }

    pub fn snapshot(&self) -> HashMap<String, Team> {
        self.teams.read().clone()
    }

    pub fn load(&self, data: HashMap<String, Team>) {
        *self.teams.write() = data;
    }

    pub fn create(&self, name: &str, plan: &str, owner_email: &str) -> Team {
        let slug = slugify(name);
        let mut slug = slug;
        let mut m = self.teams.write();
        // Ensure unique slug.
        if m.contains_key(&slug) {
            slug = format!("{slug}-{}", (now_ms() % 10000));
        }
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
        };
        m.insert(slug, team.clone());
        team
    }

    /// Create a team at an EXACT, caller-supplied slug — never re-derived or
    /// suffixed. For reconciling a team row with tenant-scoped data that already
    /// exists under a specific external id (e.g. a Clerk-org-derived tenant string
    /// with its own uniqueness suffix, which `create`'s slugify(name) scheme can
    /// never reproduce). Returns `None` if that exact slug is already taken —
    /// callers must not silently collide with or overwrite an existing team.
    pub fn create_with_slug(&self, slug: &str, name: &str, plan: &str, owner_email: &str) -> Option<Team> {
        let mut m = self.teams.write();
        if m.contains_key(slug) {
            return None;
        }
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
        };
        m.insert(slug.to_string(), team.clone());
        Some(team)
    }

    /// Toggle team/org SSO (Enterprise-only — caller enforces the plan gate).
    pub fn set_sso(&self, slug: &str, enabled: bool) -> Option<Team> {
        let mut m = self.teams.write();
        let t = m.get_mut(slug)?;
        t.sso_enabled = enabled;
        Some(t.clone())
    }

    /// Update a team's plan/tier (hobby | pro | enterprise).
    pub fn set_plan(&self, slug: &str, plan: &str) -> Option<Team> {
        let mut m = self.teams.write();
        let t = m.get_mut(slug)?;
        t.plan = plan.to_string();
        Some(t.clone())
    }

    pub fn add_member(&self, slug: &str, email: &str, role: Role) -> Option<Team> {
        let mut m = self.teams.write();
        let t = m.get_mut(slug)?;
        let el = email.to_lowercase();
        if let Some(existing) = t.members.iter_mut().find(|x| x.email.to_lowercase() == el) {
            existing.role = role;
        } else {
            t.members.push(Member {
                email: email.to_string(),
                role,
                name: String::new(),
                added_ms: now_ms(),
            });
        }
        Some(t.clone())
    }

    pub fn remove_member(&self, slug: &str, email: &str) -> Option<Team> {
        let mut m = self.teams.write();
        let t = m.get_mut(slug)?;
        let el = email.to_lowercase();
        t.members.retain(|x| x.email.to_lowercase() != el);
        Some(t.clone())
    }

    /// Is `email` a member of `slug` (for preview access control)?
    pub fn is_member(&self, slug: &str, email: &str) -> bool {
        self.teams
            .read()
            .get(slug)
            .map(|t| t.member(email).is_some())
            .unwrap_or(false)
    }

    pub fn count(&self) -> usize {
        self.teams.read().len()
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
        assert_eq!(store.count(), 2);
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
