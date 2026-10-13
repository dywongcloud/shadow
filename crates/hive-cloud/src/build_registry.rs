//! **Host-lane package-registry guard.**
//!
//! `IsolatedBuild::run_host` clears the environment but forwards `HOME`, so the
//! host user's `~/.npmrc` silently configures every host-lane install. On the
//! Tencent images that file is `registry=https://mirrors.tencentyun.com/npm` — a
//! name only the provider's own resolvers can answer. The isolated (microVM)
//! lane runs with `HOME=/workspace/home` and never sees it, so this coupling is
//! an accident of the host lane, not a design.
//!
//! Witnessed 2026-10-13 on fc-sanjose: `/etc/resolv.conf` had been overwritten
//! four days earlier with `nameserver 1.1.1.1`, so `mirrors.tencentyun.com` was
//! `ENOTFOUND`. Every install that needed an uncached package died — `npm ci`
//! with npm 10's `Exit handler never called!`, `npm install` with ENOTFOUND —
//! and npm spent 70 s retrying before it gave up (measured). Installs whose
//! whole dependency set was warm in `~/.npm/_cacache` kept working, which is
//! why four days passed before a tenant hit it.
//!
//! The guard probes the host registry once per build and, only when it is
//! unreachable while the public registry answers, points that build's commands
//! at the public registry. Three limits keep it from ever being the surprise:
//!
//!  * **Only a known public mirror is stood in for.** Falling back from a
//!    private registry would let a same-named public package satisfy an
//!    internal one (dependency confusion), so an unknown host is left alone.
//!  * **Repository-owned registry settings always win.** npm gives an
//!    environment variable priority over a project `.npmrc` (measured), so
//!    injecting over a tenant's registry would silently break their install.
//!  * **Never silent.** A fallback or a dark registry is a build-log line and a
//!    greppable `BUILD_REGISTRY_*` warning, never a quiet substitution.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

/// Default registry every supported package manager understands.
pub const PUBLIC_REGISTRY: &str = "https://registry.npmjs.org/";
const PUBLIC_REGISTRY_HOST: &str = "registry.npmjs.org";
/// npm's distro-level config on the fleet's Rocky/TencentOS images
/// (`/usr/lib/node_modules/npm/npmrc` sets `globalconfig=/etc/npmrc`).
pub const GLOBAL_NPMRC: &str = "/etc/npmrc";

/// Environment applied to a build that falls back. `npm_config_registry` is
/// honoured over a user `~/.npmrc` by npm 10, pnpm 10 and yarn 1.22 (each
/// measured against a dead mirror on fc-sanjose 2026-10-13). Yarn Berry never
/// reads `.npmrc`, and bun is provisioned on no build host, so neither is set.
pub const FALLBACK_ENV: (&str, &str) = ("npm_config_registry", PUBLIC_REGISTRY);

/// Hosts that serve a copy of the PUBLIC npm registry. Extend per node with
/// `HIVE_BUILD_REGISTRY_MIRRORS` (comma-separated host suffixes).
const PUBLIC_MIRROR_SUFFIXES: [&str; 8] = [
    "tencentyun.com",
    "tencent.com",
    "npmmirror.com",
    "taobao.org",
    "cnpmjs.org",
    "aliyun.com",
    "huaweicloud.com",
    "yarnpkg.com",
];

/// Files whose mention of a registry means the repository configures its own.
/// Substring match, deliberately over-broad: a false "tenant owns it" only
/// withholds the fallback (today's behaviour), never overrides a tenant.
const TENANT_CONFIG_FILES: [&str; 6] = [
    ".npmrc",
    ".yarnrc",
    ".yarnrc.yml",
    ".pnpmrc",
    "bunfig.toml",
    "pnpm-workspace.yaml",
];
const MAX_CONFIG_BYTES: u64 = 64 * 1024;
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// What one build should do about the host's package registry.
#[derive(Debug, PartialEq, Eq)]
pub enum Plan {
    /// Nothing host-configured, not a known public mirror, or it answers.
    Unchanged,
    /// The host mirror is dark and the public registry answers.
    Fallback { configured: String, reason: String },
    /// Neither answers: leave the environment alone (a warm cache may serve).
    Dark { configured: String, reason: String },
}

#[derive(Debug, PartialEq, Eq)]
struct HostRegistry {
    url: String,
    host: String,
    port: u16,
}

/// The last `registry=` in an npmrc body (npm: last assignment wins). Scoped
/// `@scope:registry` keys are different keys and are not matched.
fn npmrc_registry(text: &str) -> Option<String> {
    let mut found = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "registry" {
            continue;
        }
        let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
        if !value.is_empty() {
            found = Some(value.to_string());
        }
    }
    found
}

/// `http(s)://[user@]host[:port]/path`. An env-interpolated value or an IPv6
/// literal cannot be resolved here, so it is `None` (= leave alone). `url` is
/// REDACTED — scheme, host[:port] and path only — because it is written to the
/// tenant-visible build log: credentials embedded as `user:token@` and any
/// query string must never reach it.
fn parse_registry(raw: &str) -> Option<HostRegistry> {
    if raw.contains("${") {
        return None;
    }
    let (scheme, rest) = raw.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "https" => 443,
        "http" => 80,
        _ => return None,
    };
    let (authority, path) = match rest.find('/') {
        Some(slash) => rest.split_at(slash),
        None => (rest, ""),
    };
    let authority = authority.split(['?', '#']).next()?;
    let host_port = authority.rsplit('@').next()?;
    if host_port.starts_with('[') {
        return None;
    }
    let (host, port) = match host_port.rsplit_once(':') {
        Some((host, port)) => (host, port.parse().ok()?),
        None => (host_port, default_port),
    };
    if host.is_empty() {
        return None;
    }
    let path = path.split(['?', '#']).next().unwrap_or("");
    Some(HostRegistry {
        url: format!("{scheme}://{host_port}{path}"),
        host: host.to_ascii_lowercase(),
        port,
    })
}

fn is_public_mirror(host: &str, extra_suffixes: &str) -> bool {
    let under = |suffix: &str| {
        let suffix = suffix.trim().trim_start_matches('.').to_ascii_lowercase();
        !suffix.is_empty() && (host == suffix || host.ends_with(&format!(".{suffix}")))
    };
    PUBLIC_MIRROR_SUFFIXES.iter().any(|suffix| under(suffix)) || extra_suffixes.split(',').any(under)
}

async fn read_small(path: &Path) -> Option<String> {
    let meta = tokio::fs::symlink_metadata(path).await.ok()?;
    if !meta.is_file() || meta.len() > MAX_CONFIG_BYTES {
        return None;
    }
    tokio::fs::read_to_string(path).await.ok()
}

/// The registry npm will use by default for the daemon's `HOME`: the user file
/// beats the global one, exactly npm's own precedence.
async fn host_registry(home: &Path, global_npmrc: &Path) -> Option<HostRegistry> {
    for file in [home.join(".npmrc"), global_npmrc.to_path_buf()] {
        if let Some(value) = read_small(&file).await.as_deref().and_then(npmrc_registry) {
            return parse_registry(&value);
        }
    }
    None
}

/// DNS + TCP in one step: `ENOTFOUND` and a black-holed address both fail it.
async fn reachable(host: &str, port: u16) -> Result<(), String> {
    match tokio::time::timeout(PROBE_TIMEOUT, tokio::net::TcpStream::connect((host, port))).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(format!("{host}:{port}: {error}")),
        Err(_) => Err(format!(
            "{host}:{port}: no answer within {}s",
            PROBE_TIMEOUT.as_secs()
        )),
    }
}

/// Probe the host registry once. `HIVE_BUILD_REGISTRY_GUARD=0` disables it.
pub async fn assess(home: &Path, global_npmrc: &Path) -> Plan {
    if std::env::var("HIVE_BUILD_REGISTRY_GUARD").is_ok_and(|value| value == "0") {
        return Plan::Unchanged;
    }
    let Some(registry) = host_registry(home, global_npmrc).await else {
        return Plan::Unchanged;
    };
    let extra = std::env::var("HIVE_BUILD_REGISTRY_MIRRORS").unwrap_or_default();
    if !is_public_mirror(&registry.host, &extra) {
        return Plan::Unchanged;
    }
    let Err(reason) = reachable(&registry.host, registry.port).await else {
        return Plan::Unchanged;
    };
    match reachable(PUBLIC_REGISTRY_HOST, 443).await {
        Ok(()) => Plan::Fallback {
            configured: registry.url,
            reason,
        },
        Err(public) => Plan::Dark {
            configured: registry.url,
            reason: format!("{reason}; {public}"),
        },
    }
}

async fn mentions_registry(path: &Path) -> bool {
    read_small(path)
        .await
        .is_some_and(|text| text.to_ascii_lowercase().contains("registry"))
}

/// Whether this command already has a registry chosen by the tenant: a build
/// environment variable, the command text itself, or any repository config
/// file from the command's directory up to the checkout root.
pub async fn tenant_owns_registry(
    command: &str,
    dir: &Path,
    root: &Path,
    tenant_env: &BTreeMap<String, String>,
) -> bool {
    let chosen = |key: &String| {
        let key = key.to_ascii_lowercase();
        key.contains("registry") || key.contains("userconfig")
    };
    if tenant_env.keys().any(chosen) || command.to_ascii_lowercase().contains("registry") {
        return true;
    }
    let mut current = Some(dir);
    while let Some(directory) = current {
        for name in TENANT_CONFIG_FILES {
            if mentions_registry(&directory.join(name)).await {
                return true;
            }
        }
        if directory == root {
            break;
        }
        current = directory.parent().filter(|parent| parent.starts_with(root));
    }
    false
}
