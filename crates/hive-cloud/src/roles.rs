//! THE team-role matrix: who may do what, defined once and enforced at the one gate every admin
//! request passes ([`crate::auth::require_auth`] asks [`gate`]).
//!
//! A caller's role is the verified `role` claim of its JWT or dashboard API key. Roles are ordered
//! `viewer < member < admin < owner`:
//!
//! - **viewer** reads. It changes nothing, except the self-service routes in [`RULES`] (its own
//!   notifications, push and SMS delivery, browser node and profile sync).
//! - **member** does the day-to-day project work: deploys, env, domains, databases, storage,
//!   queues, sandboxes, cron, workflows, microfrontend groups, the conformance policy. It is the
//!   floor of EVERY mutation that no row below raises.
//! - **admin** runs the team: team settings and membership below its own rank, enterprise
//!   configuration, webhooks, integrations and their credentials, API-key revocation, project
//!   protection and ownership, project delete.
//! - **owner** does everything an admin may, plus deleting the team. A Clerk org's highest JWT role
//!   is `admin` (the dashboard mints `owner` only for a personal namespace), so an owner floor on a
//!   project action would lock every org out of it: project delete and transfer sit at admin.
//!
//! A platform operator (`platform_admin`) acts as an owner everywhere. A `service` claim (a fleet
//! delegation, a workload's queue client) is not a team role and is judged by its own handler. An
//! unenforced dev node has no verified roles and stays open: `require_auth` returns before it asks.
//! A role this table does not know is the lowest (viewer), never an implicit member.
//!
//! A route that needs more than member MUST be listed in [`RULES`]: the matrix is the only place a
//! role is decided, so a handler never re-decides one (it may still bind the tenant to a path slug,
//! `admin::require_team`, or refine WHAT a role sees, such as a webhook's signing secret).

use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::auth::{Claims, MESH_SERVICE_ROLE};
use crate::teams::Role;

/// Every method that changes state.
const WRITES: &[&str] = &["POST", "PUT", "PATCH", "DELETE"];
const GET: &[&str] = &["GET"];
const POST: &[&str] = &["POST"];
const PUT: &[&str] = &["PUT"];
const DELETE: &[&str] = &["DELETE"];
const POST_DELETE: &[&str] = &["POST", "DELETE"];

/// One row of the matrix: a request whose method is in `methods` and whose path matches `pattern`
/// needs at least `floor`. In a pattern `:name` matches one path segment and a trailing `*`
/// matches everything below the prefix.
struct Rule {
    methods: &'static [&'static str],
    pattern: &'static str,
    floor: Role,
}

const fn rule(methods: &'static [&'static str], pattern: &'static str, floor: Role) -> Rule {
    Rule {
        methods,
        pattern,
        floor,
    }
}

/// The matrix. The FIRST matching row wins; a request no row matches needs viewer when it reads and
/// member when it changes state.
const RULES: &[Rule] = &[
    // viewer: the caller's own state (notifications, push and SMS delivery, browser node, profile)
    rule(POST, "/v1/notifications/*", Role::Viewer),
    rule(POST_DELETE, "/v1/push/subscribe", Role::Viewer),
    rule(PUT, "/v1/push/sms", Role::Viewer),
    rule(POST, "/v1/push/sms/verify", Role::Viewer),
    rule(POST, "/v1/push/test", Role::Viewer),
    rule(POST, "/v1/identity/sync", Role::Viewer),
    rule(POST_DELETE, "/v1/browser/*", Role::Viewer),
    // viewer: a query that only reads
    rule(POST, "/v1/storage/vector/:index/query", Role::Viewer),
    // member: reads that hand out a way to write or to execute
    rule(GET, "/v1/databases/:id/credentials", Role::Member),
    rule(
        GET,
        "/v1/projects/:project/sandboxes/:sandbox/shell",
        Role::Member,
    ),
    rule(GET, "/v1/ws/realtime/:room", Role::Member),
    // admin: secrets, integrations, webhooks, project protection and ownership
    rule(GET, "/v1/integrations/:id/credentials", Role::Admin),
    rule(POST, "/v1/integrations", Role::Admin),
    rule(DELETE, "/v1/integrations/:id", Role::Admin),
    rule(POST, "/v1/webhooks", Role::Admin),
    rule(DELETE, "/v1/webhooks/:id", Role::Admin),
    rule(POST, "/v1/projects/:project/webhooks", Role::Admin),
    rule(PUT, "/v1/projects/:project/team", Role::Admin),
    rule(DELETE, "/v1/projects/:project", Role::Admin),
    rule(DELETE, "/v1/apikeys/:id", Role::Admin),
    // microfrontend groups and the conformance policy are project work (member), the rest of the
    // enterprise configuration (IP blocks, SIEM, SAML, SCIM, deployment protection) is admin
    rule(WRITES, "/v1/enterprise/microfrontends/*", Role::Member),
    rule(WRITES, "/v1/enterprise/conformance/*", Role::Member),
    rule(WRITES, "/v1/enterprise/*", Role::Admin),
    rule(POST, "/v1/saml/:team/acs", Role::Admin),
    // team settings and membership (`admin::require_team` still binds the path slug to the tenant)
    rule(POST, "/v1/teams/:slug/members", Role::Admin),
    rule(DELETE, "/v1/teams/:slug/members/:email", Role::Admin),
    rule(PUT, "/v1/teams/:slug/plan", Role::Admin),
    rule(PUT, "/v1/teams/:slug/sso", Role::Admin),
    rule(DELETE, "/v1/teams/:slug", Role::Owner),
];

/// Whether `path` matches `pattern` segment by segment (a trailing slash is ignored).
fn matches(pattern: &str, path: &str) -> bool {
    let mut want = pattern.trim_matches('/').split('/');
    let mut have = path.trim_matches('/').split('/');
    loop {
        match (want.next(), have.next()) {
            (Some("*"), _) => return true,
            (Some(w), Some(h)) if w.starts_with(':') => {
                if h.is_empty() {
                    return false;
                }
            }
            (Some(w), Some(h)) => {
                if w != h {
                    return false;
                }
            }
            (None, None) => return true,
            _ => return false,
        }
    }
}

fn is_read(method: &Method) -> bool {
    matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// The lowest role that may make this request.
pub fn floor(method: &Method, path: &str) -> Role {
    let verb = if *method == Method::HEAD {
        "GET"
    } else {
        method.as_str()
    };
    RULES
        .iter()
        .find(|row| row.methods.contains(&verb) && matches(row.pattern, path))
        .map(|row| row.floor)
        .unwrap_or(if is_read(method) {
            Role::Viewer
        } else {
            Role::Member
        })
}

/// A role claim as a [`Role`]; anything this table does not know is the lowest.
fn role_of(claim: &str) -> Role {
    match claim {
        "owner" => Role::Owner,
        "admin" => Role::Admin,
        "member" => Role::Member,
        _ => Role::Viewer,
    }
}

fn name(role: Role) -> &'static str {
    match role {
        Role::Owner => "owner",
        Role::Admin => "admin",
        Role::Member => "member",
        Role::Viewer => "viewer",
    }
}

/// Why [`gate`] refused a request: the caller's role claim is below the matrix's floor for it.
pub struct Refusal {
    role: Role,
    needs: Role,
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        let what = if self.role == Role::Viewer {
            "a viewer is read-only"
        } else {
            "your team role is too low for this change"
        };
        (
            StatusCode::FORBIDDEN,
            format!(
                "insufficient team role: {what}; this needs {} or above",
                name(self.needs)
            ),
        )
            .into_response()
    }
}

/// The matrix check for one authenticated request: `Ok` for an operator or a `service` claim, for
/// a role at or above the request's floor; otherwise the [`Refusal`]. Runs for every platform JWT
/// or API-key request on an enforced node (see `auth::require_auth`).
pub fn gate(claims: &Claims, method: &Method, path: &str) -> Result<(), Refusal> {
    if claims.platform_admin || claims.role == MESH_SERVICE_ROLE {
        return Ok(());
    }
    let needs = floor(method, path);
    let role = role_of(&claims.role);
    if role.rank() >= needs.rank() {
        Ok(())
    } else {
        Err(Refusal { role, needs })
    }
}

/// The role a request acts WITH: a platform operator is an owner, an unenforced dev node (no
/// verified roles) is open (owner), otherwise the verified role claim, an unknown one the lowest.
/// For the decisions the matrix leaves to a handler (`TeamStore::set_member_role` and
/// `remove_member_as` rank the actor, a webhook's signing secret is shown to an admin).
pub fn actor(claims: Option<&Claims>) -> Role {
    match claims {
        Some(cl) if cl.platform_admin => Role::Owner,
        Some(cl) => role_of(&cl.role),
        None if !crate::auth::enforced() => Role::Owner,
        None => Role::Viewer,
    }
}

/// [`actor`] is at least `floor`.
pub fn at_least(claims: Option<&Claims>, floor: Role) -> bool {
    actor(claims).rank() >= floor.rank()
}
