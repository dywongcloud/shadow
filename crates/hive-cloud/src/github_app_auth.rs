//! GitHub App installation-token minting: server-to-server, no user session required.
//!
//! A webhook-triggered build (`admin::git_webhook`) runs with nobody logged in, so it has no
//! per-user token (`ui/lib/github-app.ts`'s user-to-server OAuth flow exists only inside a
//! signed-in browser request) to attach to `GitDeployRequest.git_token`. Before this module the
//! only private-repo credential available to a webhook delivery was a node-wide `GITHUB_TOKEN` PAT
//! (`git.rs`'s clone path), so a private repo connected only through a user's interactive GitHub
//! App session could be imported manually but never auto-deployed from a push.
//!
//! A GitHub App can mint a short-lived "installation access token" for any repo or org it is
//! installed on, authenticated purely by the App's own private key (see
//! <https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/authenticating-as-a-github-app>):
//! 1. Sign a short-lived JWT (RS256) with the App's private key (claims `iat`/`exp`/`iss`, `iss` =
//! the App's numeric id).
//! 2. `GET /repos/{owner}/{repo}/installation` (Bearer: that JWT) finds the installation, if any,
//! covering this repo.
//! 3. `POST /app/installations/{id}/access_tokens` (Bearer: that JWT) mints a token scoped to that
//! installation, usable exactly like a PAT for cloning and valid ~1 hour.
//!
//! Configured via `GITHUB_APP_ID` (the App's numeric id) and `GITHUB_APP_PRIVATE_KEY` (the PEM from
//! the App's settings page), both DISTINCT from `GITHUB_APP_CLIENT_ID`/`GITHUB_APP_CLIENT_SECRET`
//! (the OAuth client credentials of the interactive user-to-server flow in `ui/lib/github-app.ts`).
//! Entirely optional: [`configured`] returns `false` when either is absent, and every caller falls
//! back to the node-wide `GITHUB_TOKEN` path unchanged, so this module is never a hard dependency.
//!
//! SECURITY: never log or print the private key, the signed JWT or a minted installation token
//! anywhere (including `Debug` and error text); only whether the mint succeeded and, on failure,
//! GitHub's HTTP status.

use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// GitHub caps an App JWT's lifetime at 10 minutes; stay comfortably under that. `iat` is backdated
/// 60 s to tolerate clock drift, as GitHub recommends.
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

/// Accepts the PEM either with real newlines (a multi-line env value) or as a single line with
/// literal `\n` escapes (the common shape of a multi-line secret carried through a systemd
/// `Environment=` line or an ansible-vault scalar), unescaping it here so callers never care which
/// form ops pasted.
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

/// True when both `GITHUB_APP_ID` and `GITHUB_APP_PRIVATE_KEY` are set: the gate every caller
/// checks before relying on this module. Absent either, callers fall back to `GITHUB_TOKEN` as
/// before.
pub fn configured() -> bool {
    app_id().is_some() && private_key_pem().is_some()
}

/// Signs a fresh App-level JWT (RS256, ~9 minutes valid). Never logged: callers use it only as the
/// bearer of the two GitHub API calls below.
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

/// Looks up the installation covering `owner/repo` and returns its numeric id. `Ok(None)`, not an
/// error, when the App is simply not installed on this repo or org: the normal "not applicable
/// here" case, distinct from a transport or auth failure.
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
        return Ok(None);
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

/// Mints a fresh installation access token scoped to `owner/repo`, or `Ok(None)` when the App
/// (while configured) is simply not installed on that repo: the caller's cue to fall back to
/// `GITHUB_TOKEN` rather than treat it as a hard failure. Every other failure (bad credentials,
/// malformed key, GitHub unreachable) is `Err`, so the caller can log a reason without a token
/// value ever entering the log; the error text carries GitHub's HTTP status only, never the
/// response body.
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
