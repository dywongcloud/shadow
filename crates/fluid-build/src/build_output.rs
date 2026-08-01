//! Typed model of the **Build Output API v3** — the format every framework build
//! is normalized into (`.vercel/output/`):
//!
//! ```text
//! .vercel/output/
//!   config.json                 # routes, images, crons, framework metadata
//!   static/**                   # static assets served directly by the CDN
//!   functions/
//!     <name>.func/
//!       .vc-config.json         # serverless OR edge function descriptor
//!       index.js / handler ...  # the function bundle
//!     <name>.prerender-config.json   # ISR/prerender metadata (optional)
//! ```
//!
//! These types let us read a build output produced by `vercel build` (or by our
//! own framework adapters) and provision the right primitives from it.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// The Build Output API version we target.
pub const BUILD_OUTPUT_VERSION: u32 = 3;

/// `.vercel/output/config.json`
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BuildOutputConfig {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<Route>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<ImagesConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wildcard: Option<Vec<WildcardEntry>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overrides: Option<HashMap<String, PathOverride>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub framework: Option<FrameworkInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crons: Option<Vec<Cron>>,
}

impl Default for BuildOutputConfig {
    fn default() -> Self {
        BuildOutputConfig {
            version: BUILD_OUTPUT_VERSION,
            routes: Vec::new(),
            images: None,
            wildcard: None,
            overrides: None,
            framework: None,
            crons: None,
        }
    }
}

/// A routing rule. Vercel routes are either *source* rules (`src` -> `dest`,
/// headers, status) or *phase handles* (`handle`: filesystem / rewrite / miss /
/// hit / error / resource). Edge Middleware appears as a route with
/// `middlewarePath`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Route {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub src: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,
    #[serde(rename = "continue", skip_serializing_if = "Option::is_none")]
    pub cont: Option<bool>,
    #[serde(rename = "middlewarePath", skip_serializing_if = "Option::is_none")]
    pub middleware_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub methods: Option<Vec<String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WildcardEntry {
    pub domain: String,
    pub value: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PathOverride {
    #[serde(rename = "contentType", skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FrameworkInfo {
    pub version: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Cron {
    pub path: String,
    pub schedule: String,
}

/// Image Optimization configuration (`config.json` `images`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImagesConfig {
    pub sizes: Vec<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub domains: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub formats: Vec<String>,
    #[serde(rename = "minimumCacheTTL", skip_serializing_if = "Option::is_none")]
    pub minimum_cache_ttl: Option<u64>,
}

/// `.vc-config.json` for a **serverless** function (`functions/<name>.func/`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerlessConfig {
    /// e.g. "nodejs20.x", "python3.12", "go1.x".
    pub runtime: String,
    pub handler: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<u32>,
    #[serde(rename = "maxDuration", skip_serializing_if = "Option::is_none")]
    pub max_duration: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub regions: Vec<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub environment: HashMap<String, String>,
    #[serde(rename = "launcherType", skip_serializing_if = "Option::is_none")]
    pub launcher_type: Option<String>,
    #[serde(
        rename = "supportsResponseStreaming",
        skip_serializing_if = "Option::is_none"
    )]
    pub supports_response_streaming: Option<bool>,
}

/// `.vc-config.json` for an **edge** function (runs in the lightweight edge
/// runtime, close to the user — Edge Middleware compiles to this too).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EdgeConfig {
    /// Always "edge".
    pub runtime: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<String>,
    #[serde(
        rename = "envVarsInUse",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub env_vars_in_use: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub regions: Vec<String>,
}

/// A function descriptor, classified by runtime.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FunctionConfig {
    Serverless(ServerlessConfig),
    Edge(EdgeConfig),
}

impl FunctionConfig {
    pub fn is_edge(&self) -> bool {
        matches!(self, FunctionConfig::Edge(_))
    }
    pub fn runtime(&self) -> &str {
        match self {
            FunctionConfig::Serverless(s) => &s.runtime,
            FunctionConfig::Edge(e) => &e.runtime,
        }
    }
}

/// Parse a `.vc-config.json` value, branching on `runtime` to pick edge vs
/// serverless. Returns an error if `runtime` is missing.
pub fn parse_vc_config(v: &serde_json::Value) -> anyhow::Result<FunctionConfig> {
    let runtime = v
        .get("runtime")
        .and_then(|r| r.as_str())
        .ok_or_else(|| anyhow::anyhow!(".vc-config.json missing `runtime`"))?;
    if runtime == "edge" {
        Ok(FunctionConfig::Edge(serde_json::from_value(v.clone())?))
    } else {
        Ok(FunctionConfig::Serverless(serde_json::from_value(
            v.clone(),
        )?))
    }
}

/// `.prerender-config.json` — ISR / on-demand revalidation metadata.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PrerenderConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expiration: Option<serde_json::Value>, // number | false
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<u64>,
    #[serde(rename = "bypassToken", skip_serializing_if = "Option::is_none")]
    pub bypass_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
}
