//! THE team-role matrix: who may do what, defined once and enforced at the one gate every admin
//! request passes ([`crate::auth::require_auth`] asks [`gate`]).
//!
//! A caller's role is the verified `role` claim of its JWT or dashboard API key. Roles are ordered
//! `viewer < member < admin < owner`:
//!
//! - **viewer** reads. It changes nothing, except the self-service routes in [`RULES`] (its own
//!   notifications, push and SMS delivery, browser node and profile sync).
//! - **member** does the day-to-day project work: deploys, env, attaching and verifying domains,
//!   databases (create and connect), storage, queues, sandboxes, cron, workflows, microfrontend
//!   groups, the conformance policy, its own API keys. Every route it may call is NAMED in
//!   [`RULES`].
//! - **admin** runs the team and is the floor of EVERY other change, a route nobody listed
//!   included: team settings and membership below its own rank, enterprise configuration,
//!   webhooks, integrations and their credentials, API-key revocation, billing, DNS records and
//!   nameservers, the destructive database operations (delete, credential rotation), the edge
//!   security rules (WAF, rate limits, bot rules), the repository link, project protection and
//!   ownership, project delete.
//! - **owner** does everything an admin may, plus deleting the team. A Clerk org's highest JWT role
//!   is `admin` (the dashboard mints `owner` only for a personal namespace), so an owner floor on a
//!   project action would lock every org out of it: project delete and transfer sit at admin.
//!
//! A platform operator (`platform_admin`) acts as an owner everywhere. A `service` claim is not a
//! team role: a FLEET token (`auth::is_fleet_service`, subject AND role) passes and is judged by its
//! own handler; the workflow World's queue client token (`auth::is_queue_client`), which sits in a
//! tenant's project env, may call only [`QUEUE_CLIENT_ROUTES`]; any other `service` claim is refused
//! (`auth::verify` never yields one). An unenforced dev node has no verified roles and stays open:
//! `require_auth` returns before it asks. A role this table does not know is the lowest (viewer),
//! never an implicit member.
//!
//! A new route that changes state is ADMIN until someone lists it: a member's route is listed
//! deliberately, never inherited from a default. The matrix is the only place a role is decided,
//! so a handler never re-decides one (it may still bind the tenant to a path slug,
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
const PUT_DELETE: &[&str] = &["PUT", "DELETE"];

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
/// ADMIN when it changes state, so a route is only ever open to a member because it is listed.
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
    // admin: money, DNS and the destructive database operations
    rule(POST, "/v1/billing/checkout", Role::Admin),
    rule(POST, "/v1/billing/confirm", Role::Admin),
    rule(POST, "/v1/billing/charge", Role::Admin),
    rule(POST, "/v1/domains/:domain/import", Role::Admin),
    rule(PUT, "/v1/domains/:domain/nameservers", Role::Admin),
    rule(POST, "/v1/domains/:domain/records", Role::Admin),
    rule(PUT_DELETE, "/v1/domains/:domain/records/:id", Role::Admin),
    rule(POST, "/v1/domains/:domain/ssl/renew", Role::Admin),
    rule(DELETE, "/v1/databases/:id", Role::Admin),
    rule(POST, "/v1/databases/:id/rotate", Role::Admin),
    // admin: the team's edge security, its SMS delivery provider and its repository link
    rule(WRITES, "/v1/waf/*", Role::Admin),
    rule(WRITES, "/v1/ratelimit", Role::Admin),
    rule(WRITES, "/v1/bot", Role::Admin),
    rule(WRITES, "/v1/push/sms-direct-mx", Role::Admin),
    rule(WRITES, "/v1/push/sms-key", Role::Admin),
    rule(WRITES, "/v1/push/sms-relay", Role::Admin),
    rule(WRITES, "/v1/gitops", Role::Admin),
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
    // member: the day-to-day project work, each route (or the lifecycle of one resource) named
    // deploy and build
    rule(POST, "/deployments", Role::Member),
    rule(POST, "/v1/deploy/image", Role::Member),
    rule(POST, "/v1/deploy/zip", Role::Member),
    rule(POST, "/v1/git/deploy", Role::Member),
    rule(POST, "/v1/runtime-artifact/v1/git/deploy", Role::Member),
    rule(POST, "/v1/builds/:id/cancel", Role::Member),
    rule(DELETE, "/v1/deployments/:id", Role::Member),
    rule(POST, "/v1/deployments/:id/promote", Role::Member),
    rule(POST, "/v1/projects/:project/redeploy", Role::Member),
    rule(POST, "/v1/projects/:project/rename", Role::Member),
    // environment
    rule(POST, "/v1/projects/:project/env", Role::Member),
    rule(DELETE, "/v1/projects/:project/env/:key", Role::Member),
    // domains: attaching and verifying one (DNS records, nameservers and certificates are admin)
    rule(POST, "/v1/projects/:project/domains", Role::Member),
    rule(
        DELETE,
        "/v1/projects/:project/domains/:domain",
        Role::Member,
    ),
    rule(
        POST,
        "/v1/projects/:project/domains/:domain/activate",
        Role::Member,
    ),
    rule(
        POST,
        "/v1/projects/:project/domains/:domain/detach",
        Role::Member,
    ),
    rule(POST, "/v1/domains/:domain/verify", Role::Member),
    // project settings and snapshots
    rule(PUT, "/v1/projects/:project/build", Role::Member),
    rule(WRITES, "/v1/projects/:project/container", Role::Member),
    rule(PUT, "/v1/projects/:project/cron-enabled", Role::Member),
    rule(PUT, "/v1/projects/:project/functions", Role::Member),
    rule(PUT, "/v1/projects/:project/git-ci", Role::Member),
    rule(PUT, "/v1/projects/:project/network", Role::Member),
    rule(
        PUT,
        "/v1/projects/:project/settings/microfrontends",
        Role::Member,
    ),
    rule(WRITES, "/v1/projects/:project/browser-db", Role::Member),
    rule(
        POST,
        "/v1/projects/:project/browser-db/rest-token",
        Role::Member,
    ),
    rule(
        WRITES,
        "/v1/projects/:project/deployments/:deployment_id/snapshots/*",
        Role::Member,
    ),
    // databases: create and connect (delete and credential rotation are admin)
    rule(POST, "/v1/databases", Role::Member),
    rule(POST, "/v1/databases/replica", Role::Member),
    rule(POST, "/v1/databases/:id/sqlite-engine", Role::Member),
    rule(POST, "/v1/databases/:id/studio-open", Role::Member),
    // resources a project owns: sandboxes, cron, queues, storage, workflows, drive, routing, cache,
    // microfrontend groups, secure links, incidents
    rule(WRITES, "/v1/projects/:project/sandboxes/*", Role::Member),
    rule(POST, "/v1/sandbox", Role::Member),
    rule(WRITES, "/v1/cron/*", Role::Member),
    rule(WRITES, "/v1/queues/*", Role::Member),
    rule(WRITES, "/v1/storage/*", Role::Member),
    rule(WRITES, "/v1/workflows/*", Role::Member),
    rule(WRITES, "/v1/microfrontends/groups/*", Role::Member),
    rule(WRITES, "/v1/drive/:project/*", Role::Member),
    rule(WRITES, "/v1/routing/*", Role::Member),
    rule(WRITES, "/v1/runtime-cache/*", Role::Member),
    rule(POST, "/v1/cdn/purge", Role::Member),
    rule(WRITES, "/v1/securelinks/*", Role::Member),
    rule(WRITES, "/v1/incidents/*", Role::Member),
    rule(POST, "/v1/gitops/synced", Role::Member),
    rule(POST, "/v1/wqueue/enqueue", Role::Member),
    // the caller's own: an API key (never above the creator's role) and a team of its own
    rule(POST, "/v1/apikeys", Role::Member),
    rule(POST, "/v1/teams", Role::Member),
];

/// The routes the workflow World's queue client token may call: enqueue a delayed callback for its
/// own team, and read the queue's counters. Nothing else.
const QUEUE_CLIENT_ROUTES: &[Rule] = &[
    rule(POST, "/v1/wqueue/enqueue", Role::Member),
    rule(GET, "/v1/wqueue/stats", Role::Viewer),
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
            Role::Admin
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

/// Why [`gate`] refused a request.
pub enum Refusal {
    /// The caller's role claim is below the matrix's floor for it.
    Role { role: Role, needs: Role },
    /// A `service` claim that is not a fleet token asked for more than its own routes.
    Service,
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        let message = match self {
            Refusal::Role { role, needs } => {
                let what = if role == Role::Viewer {
                    "a viewer is read-only"
                } else {
                    "your team role is too low for this change"
                };
                format!(
                    "insufficient team role: {what}; this needs {} or above",
                    name(needs)
                )
            }
            Refusal::Service => {
                "a service token may call only its own routes (the workflow queue client: POST /v1/wqueue/enqueue)"
                    .to_string()
            }
        };
        (StatusCode::FORBIDDEN, message).into_response()
    }
}

/// The matrix check for one authenticated request: `Ok` for an operator or a fleet token, for the
/// queue client on its own routes, for a role at or above the request's floor; otherwise the
/// [`Refusal`]. Runs for every authenticated request on an enforced node (see `auth::require_auth`).
pub fn gate(claims: &Claims, method: &Method, path: &str) -> Result<(), Refusal> {
    if claims.platform_admin || crate::auth::is_fleet_service(claims) {
        return Ok(());
    }
    if claims.role == MESH_SERVICE_ROLE {
        return if crate::auth::is_queue_client(claims) && queue_client_may(method, path) {
            Ok(())
        } else {
            Err(Refusal::Service)
        };
    }
    let needs = floor(method, path);
    let role = role_of(&claims.role);
    if role.rank() >= needs.rank() {
        Ok(())
    } else {
        Err(Refusal::Role { role, needs })
    }
}

/// Whether the queue client token may make this request ([`QUEUE_CLIENT_ROUTES`]).
fn queue_client_may(method: &Method, path: &str) -> bool {
    let verb = if *method == Method::HEAD {
        "GET"
    } else {
        method.as_str()
    };
    QUEUE_CLIENT_ROUTES
        .iter()
        .any(|row| row.methods.contains(&verb) && matches(row.pattern, path))
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
