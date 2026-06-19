//! EXPERIMENT (feature `zkauth`): HTTP demo for anonymous team/role membership.
//!
//! Behind the `zkauth` cargo feature, this mounts a handful of `/v1/zkauth/*`
//! routes onto the admin API so the [`hive_zkauth`] primitive can be exercised
//! over HTTP. It is **completely isolated**: its own in-process demo state, no
//! `CloudState`, and it is NOT part of the real auth / preview-gate path. With
//! the feature off (the default), none of this compiles in.
//!
//! Flow:
//!   1. `POST /v1/zkauth/enroll {role}`      → a member keypair (secret returned
//!                                             for the demo; normally client-only)
//!   2. `POST /v1/zkauth/prove  {secret_key, min_role, scope, message}` → proof
//!   3. `POST /v1/zkauth/verify {proof, min_role, scope, message}`      → verdict
//!                                             + nullifier reuse check
//!   4. `GET  /v1/zkauth/roster`             → public roster + ring sizes
//!   5. `POST /v1/zkauth/reset`              → clear demo state

#![cfg(feature = "zkauth")]

use std::sync::{Mutex, OnceLock};

use axum::{
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use hive_zkauth::{prove, verify, NullifierSet, Proof, PublicKey, Role, Roster, SecretKey};

struct Demo {
    entries: Vec<(PublicKey, Role)>,
    nullifiers: NullifierSet,
}
impl Demo {
    fn roster(&self) -> Roster {
        let mut r = Roster::new();
        for (p, role) in &self.entries {
            r.enroll(*p, *role);
        }
        r
    }
}

fn state() -> &'static Mutex<Demo> {
    static S: OnceLock<Mutex<Demo>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(Demo { entries: Vec::new(), nullifiers: NullifierSet::new() }))
}

/// Feature-gated demo routes, merged onto the admin router.
pub fn routes() -> Router {
    Router::new()
        .route("/v1/zkauth/enroll", post(enroll))
        .route("/v1/zkauth/roster", get(roster))
        .route("/v1/zkauth/prove", post(prove_h))
        .route("/v1/zkauth/verify", post(verify_h))
        .route("/v1/zkauth/reset", post(reset))
}

type Resp = Result<Json<Value>, (StatusCode, String)>;
fn bad(m: impl Into<String>) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, m.into())
}

fn parse_role(s: &str) -> Result<Role, (StatusCode, String)> {
    match s.to_ascii_lowercase().as_str() {
        "viewer" => Ok(Role::Viewer),
        "member" => Ok(Role::Member),
        "admin" => Ok(Role::Admin),
        "owner" => Ok(Role::Owner),
        other => Err(bad(format!("unknown role '{other}' (viewer|member|admin|owner)"))),
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

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
fn unhex(s: &str) -> Result<Vec<u8>, (StatusCode, String)> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return Err(bad("odd-length hex"));
    }
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(|_| bad("invalid hex")))
        .collect()
}
fn arr32(v: &[u8]) -> Result<[u8; 32], (StatusCode, String)> {
    if v.len() != 32 {
        return Err(bad("expected 32 bytes"));
    }
    let mut a = [0u8; 32];
    a.copy_from_slice(v);
    Ok(a)
}

#[derive(Deserialize)]
struct EnrollReq {
    #[serde(default = "default_role")]
    role: String,
}
fn default_role() -> String {
    "member".into()
}

async fn enroll(Json(req): Json<EnrollReq>) -> Resp {
    let role = parse_role(&req.role)?;
    let sk = SecretKey::generate();
    let pk = sk.public();
    state().lock().unwrap().entries.push((pk, role));
    Ok(Json(json!({
        "secret_key": hex(&sk.to_bytes()),
        "public_key": hex(&pk.to_bytes()),
        "role": role_name(role),
        "note": "demo only — in real use the secret never leaves the member",
    })))
}

async fn roster() -> Json<Value> {
    let s = state().lock().unwrap();
    let r = s.roster();
    let members: Vec<Value> = s
        .entries
        .iter()
        .map(|(p, role)| json!({ "public_key": hex(&p.to_bytes()), "role": role_name(*role) }))
        .collect();
    Json(json!({
        "members": members,
        "rings": {
            "viewer": r.ring(Role::Viewer).len(),
            "member": r.ring(Role::Member).len(),
            "admin": r.ring(Role::Admin).len(),
            "owner": r.ring(Role::Owner).len(),
        },
    }))
}

#[derive(Deserialize)]
struct ProveReq {
    secret_key: String,
    #[serde(default = "default_role")]
    min_role: String,
    #[serde(default)]
    scope: String,
    #[serde(default)]
    message: String,
}

async fn prove_h(Json(req): Json<ProveReq>) -> Resp {
    let min = parse_role(&req.min_role)?;
    let sk = SecretKey::from_bytes(&arr32(&unhex(&req.secret_key)?)?).map_err(|e| bad(e.to_string()))?;
    let ring = state().lock().unwrap().roster().ring(min);
    let proof = prove(&sk, &ring, req.scope.as_bytes(), req.message.as_bytes()).map_err(|e| bad(e.to_string()))?;
    Ok(Json(json!({
        "proof": hex(&proof.to_bytes()),
        "nullifier": hex(&proof.nullifier()),
        "ring_size": ring.len(),
        "min_role": role_name(min),
    })))
}

#[derive(Deserialize)]
struct VerifyReq {
    proof: String,
    #[serde(default = "default_role")]
    min_role: String,
    #[serde(default)]
    scope: String,
    #[serde(default)]
    message: String,
}

async fn verify_h(Json(req): Json<VerifyReq>) -> Resp {
    let min = parse_role(&req.min_role)?;
    let proof = Proof::from_bytes(&unhex(&req.proof)?).map_err(|e| bad(e.to_string()))?;
    let mut s = state().lock().unwrap();
    let ring = s.roster().ring(min);
    let valid = verify(&ring, req.scope.as_bytes(), req.message.as_bytes(), &proof);
    // Only spend the nullifier if the proof actually verifies.
    let fresh = if valid { s.nullifiers.redeem(&proof) } else { false };
    Ok(Json(json!({
        "valid": valid,
        "fresh": fresh,
        "nullifier": hex(&proof.nullifier()),
        "min_role": role_name(min),
        "ring_size": ring.len(),
        "note": if valid && !fresh { "valid but nullifier already used (replay/rate-limited)" } else { "" },
    })))
}

async fn reset() -> Json<Value> {
    *state().lock().unwrap() = Demo { entries: Vec::new(), nullifiers: NullifierSet::new() };
    Json(json!({ "ok": true }))
}
