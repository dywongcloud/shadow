//! Typed model of inert **`vercel.json`** configuration —
//! the *user-facing* project configuration format, as documented at
//! <https://vercel.com/docs/project-configuration/vercel-json>.
//!
//! This is distinct from [`crate::build_output`] (the Build Output API v3, which
//! is the *compiled* artifact a framework emits). `vercel.json` is what a user
//! writes by hand to override build behavior, routing, functions, crons, and
//! image optimization. Vercel's precedence is:
//!
//! ```text
//! vercel.json  >  Project Settings (dashboard)  >  framework auto-detection
//! ```
//!
//! Model parsing remains tolerant for read-only analysis callers: every field is
//! optional and unknown keys are ignored. Build admission uses the checked loader,
//! which rejects malformed JSON and executable JS/TS configuration loudly.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

/// A parsed `vercel.json`. All fields optional — see the module docs.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct VercelConfig {
    // ---- build overrides ----
    pub build_command: Option<String>,
    pub dev_command: Option<String>,
    pub install_command: Option<String>,
    pub output_directory: Option<String>,
    /// Framework slug, or `null` to mean "Other" (both deserialize to `None`).
    pub framework: Option<String>,
    pub ignore_command: Option<String>,
    /// Vercel's Bun beta selector — a MAJOR-version-only string (e.g. `"1.x"`),
    /// never a specific patch. Presence alone means "use Bun" but does not by
    /// itself pin an exact version (Vercel's own semantics); see [`Self::runtime`]
    /// for this platform's more explicit native selector.
    pub bun_version: Option<String>,
    /// This platform's native, explicit runtime selector — `{"runtime":"bun"}` or
    /// `{"runtime":"nodejs"}` in `vercel.json`. Deliberately SEPARATE from
    /// `bun_version`/package-manager detection: selecting Bun as a package
    /// manager (a `bun.lock` in the repo) must NOT by itself select Bun as the
    /// RUNTIME — these are orthogonal axes. Precedence when resolving the
    /// effective runtime (see `hive-cloud/src/git.rs`): this field wins if set,
    /// else `bun_version` presence means Bun, else Project Settings' explicit
    /// override, else infer from the detected start command (today's behavior,
    /// unchanged for every existing Node deployment).
    pub runtime: Option<String>,

    // ---- routing ----
    pub clean_urls: Option<bool>,
    pub trailing_slash: Option<bool>,
    pub redirects: Vec<VercelRedirect>,
    pub rewrites: Vec<VercelRewrite>,
    pub headers: Vec<VercelHeaderRule>,
    /// Legacy low-level routes — captured for completeness; advanced semantics
    /// (transforms/mitigate) are deferred. Kept as raw JSON.
    pub routes: Vec<serde_json::Value>,

    // ---- functions / compute ----
    /// Glob pattern -> per-function overrides.
    pub functions: BTreeMap<String, VercelFunction>,
    pub regions: Vec<String>,
    pub function_failover_regions: Vec<String>,
    pub fluid: Option<bool>,

    // ---- scheduled + images ----
    pub crons: Vec<VercelCron>,
    pub images: Option<VercelImages>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct VercelRedirect {
    pub source: String,
    pub destination: String,
    /// `true` => 308, `false` => 307. Ignored if `status_code` is set.
    pub permanent: Option<bool>,
    pub status_code: Option<u16>,
    pub has: Vec<VercelCondition>,
    pub missing: Vec<VercelCondition>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct VercelRewrite {
    pub source: String,
    pub destination: String,
    pub has: Vec<VercelCondition>,
    pub missing: Vec<VercelCondition>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct VercelHeaderRule {
    pub source: String,
    pub headers: Vec<VercelHeader>,
    pub has: Vec<VercelCondition>,
    pub missing: Vec<VercelCondition>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct VercelHeader {
    pub key: String,
    pub value: String,
}

/// A `has`/`missing` condition: match on a request `host`, `header`, `cookie`,
/// or `query`. `value` may be a plain string or an expressive `{pre,suf}` object.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct VercelCondition {
    #[serde(rename = "type")]
    pub kind: String,
    pub key: Option<String>,
    pub value: Option<ConditionValue>,
}

/// `value` in a `has`/`missing` condition — either a literal or a prefix/suffix
/// matcher.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum ConditionValue {
    Text(String),
    Expr {
        #[serde(default)]
        pre: Option<String>,
        #[serde(default)]
        suf: Option<String>,
    },
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct VercelFunction {
    /// MB of memory for the matched function(s).
    pub memory: Option<u32>,
    /// Max wall-clock seconds per invocation.
    pub max_duration: Option<u64>,
    /// npm runtime package (community runtimes), e.g. `vercel-php@0.5.2`.
    pub runtime: Option<String>,
    pub regions: Vec<String>,
    pub function_failover_regions: Vec<String>,
    pub include_files: Option<String>,
    pub exclude_files: Option<String>,
    pub supports_cancellation: Option<bool>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct VercelCron {
    pub path: String,
    pub schedule: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct VercelImages {
    pub sizes: Vec<u32>,
    pub qualities: Vec<u32>,
    /// e.g. `["image/avif","image/webp"]`.
    pub formats: Vec<String>,
    pub minimum_cache_ttl: Option<u64>,
    /// Legacy allow-list of external hostnames.
    pub domains: Vec<String>,
    pub remote_patterns: Vec<RemotePattern>,
    pub local_patterns: Vec<LocalPattern>,
    pub dangerously_allow_svg: Option<bool>,
    pub content_security_policy: Option<String>,
    /// `"inline"` | `"attachment"`.
    pub content_disposition_type: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RemotePattern {
    pub protocol: Option<String>,
    pub hostname: String,
    pub port: Option<String>,
    pub pathname: Option<String>,
    pub search: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LocalPattern {
    pub pathname: Option<String>,
    pub search: Option<String>,
}

impl VercelConfig {
    /// True if nothing meaningful is configured (so callers can skip work).
    pub fn is_empty(&self) -> bool {
        self.build_command.is_none()
            && self.dev_command.is_none()
            && self.install_command.is_none()
            && self.output_directory.is_none()
            && self.framework.is_none()
            && self.ignore_command.is_none()
            && self.bun_version.is_none()
            && self.runtime.is_none()
            && self.clean_urls.is_none()
            && self.trailing_slash.is_none()
            && self.redirects.is_empty()
            && self.rewrites.is_empty()
            && self.headers.is_empty()
            && self.routes.is_empty()
            && self.functions.is_empty()
            && self.regions.is_empty()
            && self.function_failover_regions.is_empty()
            && self.fluid.is_none()
            && self.crons.is_empty()
            && self.images.is_none()
    }

    pub fn from_json(s: &str) -> Result<VercelConfig, serde_json::Error> {
        serde_json::from_str(s)
    }
}

impl ConditionValue {
    /// Match a candidate string against this value (literal equality, or the
    /// `{pre,suf}` prefix/suffix matcher). `None`-valued conditions match on
    /// mere presence, so this is only called when a value is specified.
    pub fn matches(&self, candidate: &str) -> bool {
        match self {
            ConditionValue::Text(t) => candidate == t,
            ConditionValue::Expr { pre, suf } => {
                pre.as_deref()
                    .map(|p| candidate.starts_with(p))
                    .unwrap_or(true)
                    && suf
                        .as_deref()
                        .map(|s| candidate.ends_with(s))
                        .unwrap_or(true)
            }
        }
    }
}

const MAX_VERCEL_CONFIG_BYTES: u64 = 1024 * 1024;

/// Load the inert JSON configuration without executing repository modules.
/// Malformed, oversized, symlinked, or dynamic JS/TS configuration is a loud
/// build-input error for callers that execute repository builds.
pub fn load_vercel_config_checked(repo: &Path) -> anyhow::Result<Option<VercelConfig>> {
    use anyhow::{ensure, Context};
    use std::io::Read;

    for name in ["vercel.ts", "vercel.js", "vercel.mjs"] {
        match std::fs::symlink_metadata(repo.join(name)) {
            Ok(_) => anyhow::bail!(
                "BUILD_ISOLATION_UNSUPPORTED_CONFIG: {name} is executable repository configuration; use inert vercel.json"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("inspect {name}")),
        }
    }

    let json = repo.join("vercel.json");
    match std::fs::symlink_metadata(&json) {
        Ok(initial) => {
            ensure!(
                initial.file_type().is_file() && !initial.file_type().is_symlink(),
                "vercel.json must be a regular file, not a symlink or directory"
            );
            ensure!(
                initial.len() <= MAX_VERCEL_CONFIG_BYTES,
                "vercel.json exceeds the {}-byte limit",
                MAX_VERCEL_CONFIG_BYTES
            );
            let mut file = std::fs::File::open(&json).context("open vercel.json")?;
            let opened = file.metadata().context("inspect opened vercel.json")?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                ensure!(
                    initial.dev() == opened.dev() && initial.ino() == opened.ino(),
                    "vercel.json changed identity while it was opened"
                );
            }
            ensure!(
                opened.is_file() && opened.len() <= MAX_VERCEL_CONFIG_BYTES,
                "opened vercel.json is not a bounded regular file"
            );
            let mut bytes = Vec::with_capacity(opened.len() as usize);
            (&mut file)
                .take(MAX_VERCEL_CONFIG_BYTES + 1)
                .read_to_end(&mut bytes)
                .context("read vercel.json")?;
            ensure!(
                bytes.len() as u64 <= MAX_VERCEL_CONFIG_BYTES,
                "vercel.json grew beyond the {}-byte limit while reading",
                MAX_VERCEL_CONFIG_BYTES
            );
            let after = file.metadata().context("reinspect vercel.json")?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                ensure!(
                    opened.dev() == after.dev()
                        && opened.ino() == after.ino()
                        && opened.len() == after.len()
                        && opened.mtime() == after.mtime()
                        && opened.mtime_nsec() == after.mtime_nsec(),
                    "vercel.json changed while it was read"
                );
            }
            ensure!(
                after.len() == bytes.len() as u64,
                "vercel.json length changed while it was read"
            );
            let text = std::str::from_utf8(&bytes).context("vercel.json is not UTF-8")?;
            let config = VercelConfig::from_json(text).context("parse vercel.json")?;
            return Ok(Some(config));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("inspect vercel.json"),
    }

    Ok(None)
}

/// Compatibility wrapper for read-only analysis callers. Production build
/// admission uses [`load_vercel_config_checked`] so invalid input stays loud.
pub fn load_vercel_config(repo: &Path) -> Option<VercelConfig> {
    load_vercel_config_checked(repo).ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_config() {
        let c = VercelConfig::from_json(
            r#"{
              "$schema": "https://openapi.vercel.sh/vercel.json",
              "buildCommand": "next build",
              "installCommand": "npm ci",
              "outputDirectory": "dist",
              "framework": "nextjs",
              "ignoreCommand": "git diff --quiet HEAD^ HEAD ./",
              "cleanUrls": true,
              "trailingSlash": false,
              "redirects": [
                { "source": "/me", "destination": "/profile.html", "permanent": false },
                { "source": "/user", "destination": "/api/user", "statusCode": 301 },
                { "source": "/:path((?!uk/).*)", "destination": "/uk/:path*",
                  "has": [{ "type": "header", "key": "x-vercel-ip-country", "value": "GB" }] }
              ],
              "rewrites": [
                { "source": "/dashboard", "destination": "/login",
                  "missing": [{ "type": "cookie", "key": "auth_token" }] }
              ],
              "headers": [
                { "source": "/(.*)", "headers": [{ "key": "X-Frame-Options", "value": "DENY" }] }
              ],
              "crons": [{ "path": "/api/cron", "schedule": "0 0 * * *" }],
              "functions": {
                "api/*.js": { "memory": 3009, "maxDuration": 30 },
                "api/eu.js": { "regions": ["cdg1"], "functionFailoverRegions": ["lhr1"] }
              },
              "regions": ["iad1"],
              "fluid": true,
              "images": {
                "sizes": [256, 640, 1080],
                "qualities": [25, 50, 75],
                "formats": ["image/webp"],
                "minimumCacheTTL": 60,
                "remotePatterns": [{ "protocol": "https", "hostname": "example.com", "pathname": "^/a/.*$" }],
                "dangerouslyAllowSVG": false,
                "contentDispositionType": "inline"
              }
            }"#,
        )
        .unwrap();

        assert_eq!(c.build_command.as_deref(), Some("next build"));
        assert_eq!(c.install_command.as_deref(), Some("npm ci"));
        assert_eq!(c.output_directory.as_deref(), Some("dist"));
        assert_eq!(c.framework.as_deref(), Some("nextjs"));
        assert!(c.ignore_command.is_some());
        assert_eq!(c.clean_urls, Some(true));
        assert_eq!(c.trailing_slash, Some(false));
        assert_eq!(c.redirects.len(), 3);
        assert_eq!(c.redirects[0].permanent, Some(false));
        assert_eq!(c.redirects[1].status_code, Some(301));
        assert_eq!(c.redirects[2].has.len(), 1);
        assert_eq!(c.redirects[2].has[0].kind, "header");
        assert_eq!(c.rewrites[0].missing[0].kind, "cookie");
        assert_eq!(c.headers[0].headers[0].key, "X-Frame-Options");
        assert_eq!(c.crons[0].schedule, "0 0 * * *");
        assert_eq!(c.functions.len(), 2);
        assert_eq!(c.functions["api/*.js"].memory, Some(3009));
        assert_eq!(c.functions["api/eu.js"].regions, vec!["cdg1"]);
        assert_eq!(c.regions, vec!["iad1"]);
        assert_eq!(c.fluid, Some(true));
        let img = c.images.clone().unwrap();
        assert_eq!(img.sizes, vec![256, 640, 1080]);
        assert_eq!(img.qualities, vec![25, 50, 75]);
        assert_eq!(img.remote_patterns[0].hostname, "example.com");
        assert!(!c.is_empty());
    }

    #[test]
    fn empty_and_partial_are_tolerant() {
        assert!(VercelConfig::from_json("{}").unwrap().is_empty());
        // unknown keys ignored
        let c = VercelConfig::from_json(r#"{ "wat": 1, "cleanUrls": true }"#).unwrap();
        assert_eq!(c.clean_urls, Some(true));
    }

    #[test]
    fn condition_value_expr_matches() {
        let v: VercelCondition = serde_json::from_str(
            r#"{ "type":"header","key":"X","value":{"pre":"valid","suf":"value"} }"#,
        )
        .unwrap();
        let cv = v.value.unwrap();
        assert!(cv.matches("valid-some-value"));
        assert!(!cv.matches("invalid"));
        let lit: ConditionValue = serde_json::from_str(r#""GB""#).unwrap();
        assert!(lit.matches("GB"));
        assert!(!lit.matches("US"));
    }
}
