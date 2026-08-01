//! GitHub App installation-token minting — server-to-server, no user session
//! required.
//!
//! A webhook-triggered build (`admin::git_webhook`) runs with nobody logged
//! in, so it has no per-user token (see `ui/lib/github-app.ts`'s
//! user-to-server OAuth flow, which only exists inside a request from a
//! signed-in browser) to attach to `GitDeployRequest.git_token`. Until this
//! module existed the ONLY private-repo credential available to a webhook
//! delivery was a node-wide `GITHUB_TOKEN` PAT (see `git.rs`'s clone path) —
//! so a private repo that was only ever connected via a user's interactive
//! GitHub App session (no node-wide PAT configured) could be imported
//! manually but never auto-deployed from a push.
//!
//! GitHub Apps solve this without a user present at all: the App itself can
//! mint a short-lived "installation access token" for any repo/org it is
//! installed on, authenticated purely with the App's own private key. Flow
//! (per <https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/authenticating-as-a-github-app>):
//!
//!   1. Sign a short-lived JWT (RS256) with the App's private key — claims
//!      `iat`/`exp`/`iss` (`iss` = the App's numeric ID).
//!   2. `GET /repos/{owner}/{repo}/installation` (Bearer: that JWT) to find
//!      which installation (if any) covers this repo.
//!   3. `POST /app/installations/{id}/access_tokens` (Bearer: that JWT) to
//!      mint a token scoped to that installation — usable exactly like a PAT
//!      for cloning, valid ~1 hour.
//!
//! Configured via `GITHUB_APP_ID` (the App's numeric ID, from the App's
//! settings page) and `GITHUB_APP_PRIVATE_KEY` (the PEM the same page lets
//! you generate/download) — BOTH distinct from `GITHUB_APP_CLIENT_ID` /
//! `GITHUB_APP_CLIENT_SECRET` (`ui/lib/github-app.ts`'s OAuth client
//! credentials, used only for the interactive user-to-server flow). Entirely
//! optional: [`configured`] returns `false` when either is absent, and every
//! caller in this codebase falls back to the pre-existing node-wide
//! `GITHUB_TOKEN` path unchanged — this module never becomes a hard
//! dependency.
//!
//! SECURITY: never log or print the private key, the signed JWT, or a minted
//! installation token anywhere (including `Debug`/error text) — only whether
//! the mint succeeded, and on failure GitHub's HTTP status (its error bodies
//! don't echo credentials back).

use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Installation JWTs are capped at 10 minutes by GitHub; stay comfortably
/// under that. `iat` is backdated 60s to tolerate clock drift, matching
/// GitHub's own documented recommendation.
const JWT_BACKDATE_SECS: i64 = 60;
const JWT_TTL_SECS: i64 = 9 * 60;

const API_BASE: &str = "https://api.github.com";
const USER_AGENT: &str = "shadw-hive-cloud";
const API_VERSION: &str = "2022-11-28";

#[derive(Serialize)]
struct AppClaims {
    iat: i64,
    exp: i64,
    iss: String,
}

#[derive(Deserialize)]
struct InstallationLookup {
    id: u64,
}

#[derive(Deserialize)]
struct InstallationTokenResp {
    token: String,
}

fn app_id() -> Option<String> {
    std::env::var("GITHUB_APP_ID")
        .ok()
        .filter(|s| !s.is_empty())
}

/// Accepts the PEM either as real newlines (multi-line env value) or as a
/// single-line value with literal `\n` escapes (the common shape for a
/// multi-line secret carried through a systemd `Environment=` line /
/// ansible-vault scalar) — unescaped here so callers never have to care
/// which form ops pasted it in.
fn private_key_pem() -> Option<String> {
    std::env::var("GITHUB_APP_PRIVATE_KEY")
        .ok()
        .filter(|s| !s.is_empty())
        .map(|raw| {
            if raw.contains("\\n") && !raw.contains('\n') {
                raw.replace("\\n", "\n")
            } else {
                raw
            }
        })
}

/// True when both `GITHUB_APP_ID` and `GITHUB_APP_PRIVATE_KEY` are set — the
/// gate every caller checks before relying on this module. Absent either,
/// callers fall back to `GITHUB_TOKEN` exactly as before this module existed.
pub fn configured() -> bool {
    app_id().is_some() && private_key_pem().is_some()
}

/// Sign a fresh App-level JWT (RS256, ~9 minutes valid). Never logged —
/// callers use the returned string only as a bearer credential for the two
/// GitHub API calls below, never printed.
fn signed_app_jwt() -> anyhow::Result<String> {
    let id = app_id().ok_or_else(|| anyhow::anyhow!("GITHUB_APP_ID not set"))?;
    let pem = private_key_pem().ok_or_else(|| anyhow::anyhow!("GITHUB_APP_PRIVATE_KEY not set"))?;
    let key = EncodingKey::from_rsa_pem(pem.as_bytes())
        .map_err(|e| anyhow::anyhow!("GITHUB_APP_PRIVATE_KEY is not a valid RSA PEM: {e}"))?;
    let now = chrono::Utc::now().timestamp();
    let claims = AppClaims {
        iat: now - JWT_BACKDATE_SECS,
        exp: now + JWT_TTL_SECS,
        iss: id,
    };
    let token = encode(&Header::new(Algorithm::RS256), &claims, &key)?;
    Ok(token)
}

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

/// Look up the installation covering `owner/repo` and return its numeric id.
/// `Ok(None)` (not an error) when the App simply isn't installed on this
/// repo — the normal "not applicable here" case, distinct from a transport/
/// auth failure.
async fn installation_id_for_repo(
    app_jwt: &str,
    owner: &str,
    repo: &str,
) -> anyhow::Result<Option<u64>> {
    let url = format!("{API_BASE}/repos/{owner}/{repo}/installation");
    let resp = client()
        .get(&url)
        .bearer_auth(app_jwt)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", API_VERSION)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("installation lookup request failed: {e}"))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None); // App not installed on this repo/org — not an error.
    }
    let status = resp.status();
    if !status.is_success() {
        return Err(anyhow::anyhow!(
            "installation lookup returned HTTP {status}"
        ));
    }
    let body: InstallationLookup = resp
        .json()
        .await
        .map_err(|e| anyhow::anyhow!("installation lookup returned unparseable JSON: {e}"))?;
    Ok(Some(body.id))
}

/// Exchange the App JWT for a short-lived (~1h) installation access token.
async fn mint_installation_token(app_jwt: &str, installation_id: u64) -> anyhow::Result<String> {
    let url = format!("{API_BASE}/app/installations/{installation_id}/access_tokens");
    let resp = client()
        .post(&url)
        .bearer_auth(app_jwt)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", API_VERSION)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("installation-token mint request failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        // Deliberately not including the response body: GitHub error bodies
        // are safe (no credential echo) but keeping this conservative costs
        // nothing and the status code alone is enough to diagnose from logs.
        return Err(anyhow::anyhow!(
            "installation-token mint returned HTTP {status}"
        ));
    }
    let body: InstallationTokenResp = resp
        .json()
        .await
        .map_err(|e| anyhow::anyhow!("installation-token mint returned unparseable JSON: {e}"))?;
    Ok(body.token)
}

/// Mint a fresh installation access token scoped to `owner/repo`, or `Ok(None)`
/// when the App (while configured) simply isn't installed on that repo — the
/// caller's cue to fall back to `GITHUB_TOKEN` rather than treating it as a
/// hard failure. Every other failure (bad credentials, malformed key, GitHub
/// unreachable) is `Err` so the caller can log a reason without a token value
/// ever entering the log.
pub async fn installation_token_for_repo(
    owner: &str,
    repo: &str,
) -> anyhow::Result<Option<String>> {
    let jwt = signed_app_jwt()?;
    let Some(installation_id) = installation_id_for_repo(&jwt, owner, repo).await? else {
        return Ok(None);
    };
    let token = mint_installation_token(&jwt, installation_id).await?;
    Ok(Some(token))
}
