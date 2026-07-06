//! EXPERIMENT (feature `zkauth`): anonymous team/role membership for preview
//! deployments — an **opt-in** access path layered onto `preview_gate`.
//!
//! Goals (no manual steps for the end user):
//! 1. **Roster is maintained automatically.** As signed-in members use the
//!    dashboard, the client calls [`register`] in the background; the node lazily
//!    creates a per-member ZK keypair (secret sealed at rest via
//!    [`crate::secrets`]) and adds the public key to the team roster.
//! 2. **Proofs are minted automatically.** When a member opens a protected
//!    preview, the dashboard asks the member's **home node** ([`preview_proof`])
//!    to mint an anonymous membership proof; the browser is redirected through a
//!    bootstrap endpoint ([`bootstrap`]) on the preview host, which verifies the
//!    proof and drops a short-lived access cookie. `preview_gate` then honours
//!    that cookie.
//!
//! Trust model: the proof is **anonymous against the node that SERVES the preview**
//! (a peer you may not control) — that node only sees "some member with role ≥
//! viewer", never which one, and never a replayable identity token. The member's
//! own home node (which mints the proof) naturally knows the member; that's the
//! party the user already trusts.
//!
//! **Safety:** this whole module is `#![cfg(feature = "zkauth")]`. With the
//! feature off (the default) none of it compiles in, and `preview_gate` /
//! `edge_pipeline` behave exactly as before. Frontend wiring is separately gated
//! by `NEXT_PUBLIC_ZKAUTH`.

#![cfg(feature = "zkauth")]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use axum::{
    body::Body,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use hive_core::now_ms;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use hive_zkauth::{prove, verify, Proof, PublicKey, Role, Roster, SecretKey};

const SCOPE_PREFIX: &str = "preview:";
const COOKIE: &str = "shadw_zk";
const BUCKET_MS: u64 = 300_000; // 5-minute proof validity window
const COOKIE_TTL_MS: u64 = 3_600_000; // 1-hour preview session

// ----------------------------- persistent store -----------------------------

#[derive(Serialize, Deserialize, Default)]
struct Store {
    teams: HashMap<String, Vec<MemberRec>>,
}
#[derive(Serialize, Deserialize, Clone)]
struct MemberRec {
    user_id: String,
    pubkey: String,      // hex
    role: String,        // viewer|member|admin|owner
    secret_sealed: String, // crate::secrets::encrypt(hex(secret))
}

fn store_path() -> PathBuf {
    crate::persist::data_dir().join("zkauth.json")
}
fn store() -> &'static Mutex<Store> {
    static S: OnceLock<Mutex<Store>> = OnceLock::new();
    S.get_or_init(|| {
        let s = std::fs::read_to_string(store_path())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        Mutex::new(s)
    })
}
fn save(s: &Store) {
    if let Ok(json) = serde_json::to_string(s) {
        let _ = std::fs::create_dir_all(crate::persist::data_dir());
        let _ = std::fs::write(store_path(), json);
    }
}

// ----------------------------- helpers -----------------------------

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len() / 2).map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()).collect()
}
fn arr32(v: &[u8]) -> Option<[u8; 32]> {
    if v.len() != 32 {
        return None;
    }
    let mut a = [0u8; 32];
    a.copy_from_slice(v);
    Some(a)
}
fn parse_role(s: &str) -> Role {
    match s.to_ascii_lowercase().as_str() {
        "viewer" => Role::Viewer,
        "admin" => Role::Admin,
        "owner" => Role::Owner,
        _ => Role::Member,
    }
}
fn role_name(r: Role) -> &'static str {
    match r {
        Role::Viewer => "viewer",
        Role::Member => "member",
        Role::Admin => "admin",
        Role::Owner => "owner",
    }
}
fn team_of(headers: &HeaderMap) -> String {
    headers
        .get("x-hive-team")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .unwrap_or("personal")
        .to_string()
}

/// The team roster used for BOTH minting and verifying — the union of this node's
/// local members and the public members replicated from peers via gossip. Because
/// a preview can be PLACED on a peer (the placement scheduler), the node that
/// serves it must rebuild the *identical* ring the home node minted against;
/// replicating the public roster makes that ring match across the mesh.
/// `Roster::enroll` dedups by key and `Roster::ring` sorts by bytes, so the ring
/// is identical regardless of merge order.
fn roster_of(team: &str, store: &Store) -> Roster {
    let mut r = Roster::new();
    if let Some(members) = store.teams.get(team) {
        for m in members {
            if let Some(pk) = unhex(&m.pubkey).and_then(|b| arr32(&b)).and_then(|a| PublicKey::from_bytes(&a).ok()) {
                r.enroll(pk, parse_role(&m.role));
            }
        }
    }
    if let Some(members) = peer_cache().lock().unwrap().get(team) {
        for (pubkey, role) in members {
            if let Some(pk) = unhex(pubkey).and_then(|b| arr32(&b)).and_then(|a| PublicKey::from_bytes(&a).ok()) {
                r.enroll(pk, parse_role(role));
            }
        }
    }
    r
}

// --------------------- mesh roster replication (public keys only) ------------

/// Public rosters learned from peers: team -> [(pubkey hex, role)]. Rebuilt each
/// gossip cycle so revocations converge. Holds only public info — never secrets.
fn peer_cache() -> &'static Mutex<HashMap<String, Vec<(String, String)>>> {
    static P: OnceLock<Mutex<HashMap<String, Vec<(String, String)>>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(HashMap::new()))
}

/// This node's LOCAL roster as public JSON (no secrets), for peers to replicate.
/// Exports only local members (never the peer cache) so revocations can't echo.
pub fn export_json() -> Value {
    let s = store().lock().unwrap();
    let teams: serde_json::Map<String, Value> = s
        .teams
        .iter()
        .map(|(t, ms)| {
            let members: Vec<Value> = ms.iter().map(|m| json!({ "pubkey": m.pubkey, "role": m.role })).collect();
            (t.clone(), json!(members))
        })
        .collect();
    json!({ "teams": teams })
}

/// Drop all replicated peer rosters (called at the start of each gossip cycle).
pub fn clear_peer_cache() {
    peer_cache().lock().unwrap().clear();
}

/// Merge one peer's [`export_json`] output into the peer roster cache.
pub fn ingest_peer_export(v: &Value) {
    let Some(teams) = v.get("teams").and_then(|x| x.as_object()) else {
        return;
    };
    let mut pc = peer_cache().lock().unwrap();
    for (team, members) in teams {
        let Some(arr) = members.as_array() else { continue };
        let entry = pc.entry(team.clone()).or_default();
        for m in arr {
            let pk = m.get("pubkey").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let role = m.get("role").and_then(|x| x.as_str()).unwrap_or("member").to_string();
            if !pk.is_empty() && !entry.iter().any(|(p, _)| p == &pk) {
                entry.push((pk, role));
            }
        }
    }
}

// ----------------------------- core ops -----------------------------

/// Idempotently ensure a member has a ZK key enrolled in the team roster.
fn ensure_member(team: &str, user_id: &str, role: Role) -> PublicKey {
    let mut s = store().lock().unwrap();
    let members = s.teams.entry(team.to_string()).or_default();
    if let Some(m) = members.iter().find(|m| m.user_id == user_id) {
        if let Some(pk) = unhex(&m.pubkey).and_then(|b| arr32(&b)).and_then(|a| PublicKey::from_bytes(&a).ok()) {
            return pk;
        }
    }
    let sk = SecretKey::generate();
    let pk = sk.public();
    members.retain(|m| m.user_id != user_id);
    members.push(MemberRec {
        user_id: user_id.to_string(),
        pubkey: hex(&pk.to_bytes()),
        role: role_name(role).to_string(),
        secret_sealed: crate::secrets::encrypt(&hex(&sk.to_bytes())),
    });
    save(&s);
    pk
}

/// Current proof-validity time bucket.
fn bucket() -> u64 {
    now_ms() / BUCKET_MS
}

/// Mint an anonymous membership proof for `project` on behalf of `user_id`'s key.
/// Uses the role-≥-viewer ring (everyone), so the proof reveals only "a member".
fn mint(team: &str, user_id: &str, project: &str) -> Option<(String, String)> {
    let s = store().lock().unwrap();
    let rec = s.teams.get(team)?.iter().find(|m| m.user_id == user_id)?.clone();
    let secret_hex = crate::secrets::decrypt(&rec.secret_sealed);
    let sk = SecretKey::from_bytes(&arr32(&unhex(&secret_hex)?)?).ok()?;
    let ring = roster_of(team, &s).ring(Role::Viewer);
    let scope = format!("{SCOPE_PREFIX}{project}");
    let message = bucket().to_string();
    let proof = prove(&sk, &ring, scope.as_bytes(), message.as_bytes()).ok()?;
    Some((hex(&proof.to_bytes()), message))
}

/// Verify a bootstrap proof for `project`: correct ring, fresh time bucket.
fn verify_access(team: &str, project: &str, proof_hex: &str, message: &str) -> bool {
    let Some(proof) = unhex(proof_hex).and_then(|b| Proof::from_bytes(&b).ok()) else {
        return false;
    };
    // Accept the current or previous bucket (clock skew / window edge).
    let now = bucket();
    let Ok(m) = message.parse::<u64>() else { return false };
    if m != now && m + 1 != now {
        return false;
    }
    let s = store().lock().unwrap();
    let ring = roster_of(team, &s).ring(Role::Viewer);
    let scope = format!("{SCOPE_PREFIX}{project}");
    verify(&ring, scope.as_bytes(), message.as_bytes(), &proof)
}

// ----------------------------- cookie (access session) -----------------------------

fn issue_cookie(project: &str) -> String {
    let exp = now_ms() + COOKIE_TTL_MS;
    let token = crate::secrets::encrypt(&format!("{project}|{exp}"));
    format!("{COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}", COOKIE_TTL_MS / 1000)
}

/// True if the request carries a valid, unexpired access cookie for `project`.
pub fn cookie_access(project: &str, headers: &[(String, String)]) -> bool {
    let Some(cookie_hdr) = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("cookie")).map(|(_, v)| v.clone()) else {
        return false;
    };
    for kv in cookie_hdr.split(';') {
        let kv = kv.trim();
        if let Some(tok) = kv.strip_prefix(&format!("{COOKIE}=")) {
            let plain = crate::secrets::decrypt(tok);
            if let Some((p, exp)) = plain.split_once('|') {
                if p == project {
                    if let Ok(e) = exp.parse::<u64>() {
                        return now_ms() < e;
                    }
                }
            }
        }
    }
    false
}

// ----------------------------- gateway bootstrap -----------------------------

fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| v.to_string())
    })
}

/// Handle `GET /_shadw/zk?p=<proof>&m=<message>&t=<team>&next=<path>` on a preview
/// host: verify the anonymous proof, set a short-lived access cookie, then redirect
/// to `next` (default `/`) — completing the one-and-done unlock. `next` is forced
/// to a same-site path (must start with a single `/`) to prevent open redirects.
pub fn bootstrap(cloud: &std::sync::Arc<crate::state::CloudState>, host: &str, query: &str) -> Response {
    let Some(project) = cloud.gw.project_for_host(host) else {
        return (StatusCode::NOT_FOUND, "unknown deployment").into_response();
    };
    let team = query_param(query, "t").unwrap_or_else(|| "personal".into());
    let proof = query_param(query, "p").unwrap_or_default();
    let message = query_param(query, "m").unwrap_or_default();
    let next = query_param(query, "next")
        .map(|n| urldecode(&n))
        .filter(|n| n.starts_with('/') && !n.starts_with("//"))
        .unwrap_or_else(|| "/".into());
    if verify_access(&team, &project, &proof, &message) {
        Response::builder()
            .status(StatusCode::FOUND)
            .header(header::LOCATION, next)
            .header(header::SET_COOKIE, issue_cookie(&project))
            .body(Body::empty())
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
    } else {
        (StatusCode::UNAUTHORIZED, "invalid membership proof").into_response()
    }
}

/// Minimal percent-decoding for the `next` redirect target.
fn urldecode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ----------------------------- admin routes -----------------------------

pub fn routes() -> Router {
    Router::new()
        .route("/v1/zkauth/register", post(register))
        .route("/v1/zkauth/preview-proof", post(preview_proof))
        .route("/v1/zkauth/roster", get(roster))
        .route("/v1/zkauth/roster-export", get(roster_export))
}

/// Node-to-node: this node's local public roster, for mesh replication so peers
/// that serve previews can rebuild the identical verification ring.
pub(crate) async fn roster_export() -> Json<Value> {
    Json(export_json())
}

#[derive(Deserialize)]
struct RegisterReq {
    user_id: String,
    #[serde(default)]
    role: Option<String>,
}
/// Enrollment + proof-minting are SECURITY-SENSITIVE: whoever can call them can
/// join a team's roster and mint a membership proof. The node can't verify Clerk
/// org membership itself, so these endpoints must only be reachable from the
/// trusted dashboard server (which DOES verify membership) — proven by a shared
/// `HIVE_INTERNAL_TOKEN` in `x-hive-internal`. When the token is unset (dev) the
/// guard is open; set it in any multi-user/production deployment.
fn internal_ok(headers: &HeaderMap) -> bool {
    // Fail CLOSED when JWT auth is enforced but HIVE_INTERNAL_TOKEN is
    // missing — mirrors `admin::mint_allowed`'s pattern exactly. This used to
    // fail OPEN (`_ => true`) whenever no token was configured, so a single
    // fleet node missing this one env var silently let ANY external caller
    // self-enroll into (and mint a preview-access proof for) any tenant's
    // zkauth roster — and because rosters are gossiped mesh-wide, that bogus
    // membership then verified on every peer node too.
    if !crate::auth::enforced() {
        return true; // dev mode (no HIVE_JWT_SECRET) — unchanged, open like the rest of the dev-mode API
    }
    match std::env::var("HIVE_INTERNAL_TOKEN") {
        Ok(t) if !t.trim().is_empty() => headers
            .get("x-hive-internal")
            .and_then(|v| v.to_str().ok())
            .map(|v| crate::admin::ct_eq(v, &t))
            .unwrap_or(false),
        _ => false,
    }
}

async fn register(headers: HeaderMap, Json(req): Json<RegisterReq>) -> Result<Json<Value>, (StatusCode, String)> {
    if !internal_ok(&headers) {
        return Err((StatusCode::FORBIDDEN, "enrollment is server-only".into()));
    }
    let team = team_of(&headers);
    let role = req.role.as_deref().map(parse_role).unwrap_or(Role::Member);
    let pk = ensure_member(&team, &req.user_id, role);
    Ok(Json(json!({ "ok": true, "team": team, "public_key": hex(&pk.to_bytes()), "role": role_name(role) })))
}

#[derive(Deserialize)]
struct ProveReq {
    user_id: String,
    project: String,
}
async fn preview_proof(headers: HeaderMap, Json(req): Json<ProveReq>) -> Result<Json<Value>, (StatusCode, String)> {
    if !internal_ok(&headers) {
        return Err((StatusCode::FORBIDDEN, "proof minting is server-only".into()));
    }
    let team = team_of(&headers);
    match mint(&team, &req.user_id, &req.project) {
        Some((proof, message)) => Ok(Json(json!({ "proof": proof, "message": message, "team": team }))),
        None => Err((StatusCode::NOT_FOUND, "no membership key for this user/team".into())),
    }
}

async fn roster(headers: HeaderMap) -> Json<Value> {
    let team = team_of(&headers);
    let s = store().lock().unwrap();
    let members: Vec<Value> = s
        .teams
        .get(&team)
        .map(|m| m.iter().map(|r| json!({ "user_id": r.user_id, "public_key": r.pubkey, "role": r.role })).collect())
        .unwrap_or_default();
    Json(json!({ "team": team, "members": members }))
}
