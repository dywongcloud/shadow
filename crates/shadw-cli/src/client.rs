//! HTTP client + config resolution for the shadw CLI.
//!
//! Auth is the platform API key issued in the dashboard (Settings → API Keys):
//! every request carries `Authorization: Bearer <key>`, which scopes it to the
//! key's team server-side. Config is resolved (highest priority first) from CLI
//! flags → environment → `~/.shadw/config.json` → built-in defaults.

use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::path::PathBuf;

pub const DEFAULT_API: &str = "http://127.0.0.1:8786";

#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct Config {
    #[serde(default)]
    pub api: Option<String>,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub team: Option<String>,
}

/// Path to the CLI config file (`~/.shadw/config.json`).
pub fn config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".shadw").join("config.json")
}

pub fn load_config() -> Config {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_config(cfg: &Config) -> Result<()> {
    let path = config_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(cfg)?)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// A configured client: resolved base URL + optional token/team, over reqwest.
pub struct Client {
    pub api: String,
    pub token: Option<String>,
    pub team: Option<String>,
    http: reqwest::Client,
}

/// Pure resolution of (api, token, team) from flags → env → config → default.
/// Extracted so it can be unit-tested deterministically with a fake env lookup.
pub(crate) fn resolve_settings(
    api: Option<String>,
    token: Option<String>,
    team: Option<String>,
    cfg: &Config,
    env: impl Fn(&str) -> Option<String>,
) -> (String, Option<String>, Option<String>) {
    let api = api
        .or_else(|| env("SHADW_API_URL"))
        .or_else(|| env("SHADW_API"))
        .or_else(|| cfg.api.clone())
        .unwrap_or_else(|| DEFAULT_API.into());
    let token = token
        .or_else(|| env("SHADW_TOKEN"))
        .or_else(|| env("SHADW_API_KEY"))
        .or_else(|| cfg.token.clone())
        .filter(|s| !s.is_empty());
    let team = team
        .or_else(|| env("SHADW_TEAM"))
        .or_else(|| cfg.team.clone())
        .filter(|s| !s.is_empty());
    (api.trim_end_matches('/').to_string(), token, team)
}

/// Join a base URL with a path, tolerating a leading slash or not.
pub(crate) fn join_url(base: &str, path: &str) -> String {
    if path.starts_with('/') {
        format!("{base}{path}")
    } else {
        format!("{base}/{path}")
    }
}

impl Client {
    /// Resolve config from flags (highest priority) → env → config file → default.
    pub fn resolve(api: Option<String>, token: Option<String>, team: Option<String>) -> Client {
        let cfg = load_config();
        let (api, token, team) = resolve_settings(api, token, team, &cfg, |k| std::env::var(k).ok());
        Client {
            api,
            token,
            team,
            http: reqwest::Client::new(),
        }
    }

    fn url(&self, path: &str) -> String {
        join_url(&self.api, path)
    }

    fn apply_auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let mut rb = rb;
        if let Some(t) = &self.token {
            rb = rb.bearer_auth(t);
        }
        if let Some(team) = &self.team {
            rb = rb.header("x-hive-team", team);
        }
        rb
    }

    /// Perform a request; returns parsed JSON (or a JSON string for non-JSON bodies).
    /// Errors on non-2xx with the status + body for clear diagnostics.
    pub async fn request(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
        let m = reqwest::Method::from_bytes(method.to_uppercase().as_bytes())
            .map_err(|_| anyhow!("invalid HTTP method: {method}"))?;
        let mut rb = self.http.request(m, self.url(path));
        rb = self.apply_auth(rb);
        if let Some(b) = body {
            rb = rb.json(&b);
        }
        let resp = rb
            .send()
            .await
            .with_context(|| format!("requesting {} {}", method, self.url(path)))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!(
                "{} {} → HTTP {}{}",
                method,
                path,
                status.as_u16(),
                if text.is_empty() { String::new() } else { format!(": {}", text.trim()) }
            ));
        }
        if text.is_empty() {
            return Ok(Value::Null);
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }

    pub async fn get(&self, path: &str) -> Result<Value> {
        self.request("GET", path, None).await
    }
    pub async fn post(&self, path: &str, body: Value) -> Result<Value> {
        self.request("POST", path, Some(body)).await
    }
    pub async fn put(&self, path: &str, body: Value) -> Result<Value> {
        self.request("PUT", path, Some(body)).await
    }
    pub async fn delete(&self, path: &str) -> Result<Value> {
        self.request("DELETE", path, None).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k: &str| m.get(k).cloned()
    }

    #[test]
    fn precedence_flag_over_env_over_config_over_default() {
        let cfg = Config { api: Some("http://cfg".into()), token: Some("cfg_tok".into()), team: Some("cfg_team".into()) };
        // Flags win over everything.
        let (api, tok, team) = resolve_settings(
            Some("http://flag".into()), Some("flag_tok".into()), Some("flag_team".into()),
            &cfg, env_of(&[("SHADW_API_URL", "http://env"), ("SHADW_TOKEN", "env_tok")]),
        );
        assert_eq!(api, "http://flag");
        assert_eq!(tok.as_deref(), Some("flag_tok"));
        assert_eq!(team.as_deref(), Some("flag_team"));
    }

    #[test]
    fn env_wins_over_config_when_no_flag() {
        let cfg = Config { api: Some("http://cfg".into()), token: Some("cfg_tok".into()), team: None };
        let (api, tok, _) = resolve_settings(None, None, None, &cfg, env_of(&[("SHADW_API_URL", "http://env"), ("SHADW_TOKEN", "env_tok")]));
        assert_eq!(api, "http://env");
        assert_eq!(tok.as_deref(), Some("env_tok"));
    }

    #[test]
    fn config_used_when_no_flag_or_env() {
        let cfg = Config { api: Some("http://cfg".into()), token: Some("cfg_tok".into()), team: Some("cfg_team".into()) };
        let (api, tok, team) = resolve_settings(None, None, None, &cfg, env_of(&[]));
        assert_eq!(api, "http://cfg");
        assert_eq!(tok.as_deref(), Some("cfg_tok"));
        assert_eq!(team.as_deref(), Some("cfg_team"));
    }

    #[test]
    fn default_api_and_no_token_when_nothing_set() {
        let (api, tok, team) = resolve_settings(None, None, None, &Config::default(), env_of(&[]));
        assert_eq!(api, DEFAULT_API);
        assert!(tok.is_none());
        assert!(team.is_none());
    }

    #[test]
    fn legacy_env_aliases_and_empty_strings_ignored() {
        // SHADW_API / SHADW_API_KEY are the legacy aliases.
        let (api, tok, _) = resolve_settings(None, None, None, &Config::default(), env_of(&[("SHADW_API", "http://legacy"), ("SHADW_API_KEY", "legacy_key")]));
        assert_eq!(api, "http://legacy");
        assert_eq!(tok.as_deref(), Some("legacy_key"));
        // An empty token is treated as absent.
        let (_, tok2, _) = resolve_settings(None, Some(String::new()), None, &Config::default(), env_of(&[]));
        assert!(tok2.is_none());
    }

    #[test]
    fn trailing_slash_trimmed_and_url_join() {
        let (api, _, _) = resolve_settings(Some("http://x:8786/".into()), None, None, &Config::default(), env_of(&[]));
        assert_eq!(api, "http://x:8786");
        assert_eq!(join_url(&api, "/v1/overview"), "http://x:8786/v1/overview");
        assert_eq!(join_url(&api, "v1/overview"), "http://x:8786/v1/overview");
    }
}
