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

fn roster_of(team: &str, store: &Store) -> Roster {
    let mut r = Roster::new();
    if let Some(members) = store.teams.get(team) {
        for m in members {
            if let Some(pk) = unhex(&m.pubkey).and_then(|b| arr32(&b)).and_then(|a| PublicKey::from_bytes(&a).ok()) {
                r.enroll(pk, parse_role(&m.role));
            }
        }
    }
    r
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
}

#[derive(Deserialize)]
struct RegisterReq {
    user_id: String,
    #[serde(default)]
    role: Option<String>,
}
async fn register(headers: HeaderMap, Json(req): Json<RegisterReq>) -> Json<Value> {
    let team = team_of(&headers);
    let role = req.role.as_deref().map(parse_role).unwrap_or(Role::Member);
    let pk = ensure_member(&team, &req.user_id, role);
    Json(json!({ "ok": true, "team": team, "public_key": hex(&pk.to_bytes()), "role": role_name(role) }))
}

#[derive(Deserialize)]
struct ProveReq {
    user_id: String,
    project: String,
}
async fn preview_proof(headers: HeaderMap, Json(req): Json<ProveReq>) -> Result<Json<Value>, (StatusCode, String)> {
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
