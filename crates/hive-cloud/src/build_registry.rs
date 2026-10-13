//! Host-lane package-registry guard: when the host user's npm registry (a
//! provider mirror answerable only by the provider's resolvers) is dark while
//! the public registry answers, a build runs with `npm_config_userconfig`
//! pointing at a copy of the host npmrc whose registry is the public one.
//! The user-config layer sits BELOW a repository's own `.npmrc`, so a tenant
//! registry wins by npm's layering with no detection, and npm 10/11, pnpm
//! 9-12 and yarn 1 all honour it (pnpm 11+ ignores `npm_config_registry`).
//! History: CHANGELOG 2026-10-13.

use std::path::{Path, PathBuf};
use std::time::Duration;

pub const PUBLIC_REGISTRY: &str = "https://registry.npmjs.org/";
const PUBLIC_REGISTRY_HOST: &str = "registry.npmjs.org";
pub const GLOBAL_NPMRC: &str = "/etc/npmrc";
pub const FALLBACK_ENV_KEY: &str = "npm_config_userconfig";
const FALLBACK_FILE: &str = "build-registry-fallback.npmrc";

/// Exact hosts that serve a copy of the PUBLIC npm registry; a registry on
/// any other host may be private and is never replaced (dependency confusion).
/// `HIVE_BUILD_REGISTRY_MIRRORS` (comma-separated hosts) extends the list.
const PUBLIC_MIRROR_HOSTS: [&str; 10] = [
    "mirrors.tencentyun.com",
    "mirrors.tencent.com",
    "mirrors.cloud.tencent.com",
    "registry.npmmirror.com",
    "registry.npm.taobao.org",
    "r.cnpmjs.org",
    "registry.yarnpkg.com",
    "mirrors.aliyun.com",
    "mirrors.huaweicloud.com",
    "repo.huaweicloud.com",
];
const MAX_CONFIG_BYTES: u64 = 64 * 1024;
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, PartialEq, Eq)]
pub enum Plan {
    Unchanged,
    /// `userconfig` is the file to hand the build as `npm_config_userconfig`.
    Fallback { configured: String, reason: String, userconfig: PathBuf },
    Dark { configured: String, reason: String },
}

#[derive(Debug, PartialEq, Eq)]
struct HostRegistry {
    display: String,
    host: String,
    port: u16,
}

fn is_registry_line(line: &str) -> bool {
    line.split_once('=')
        .is_some_and(|(key, _)| key.trim() == "registry")
}

/// The last top-level `registry=` in an npmrc (last assignment wins; keys
/// under an ini `[section]` are ignored, as npm ignores them).
fn npmrc_registry(text: &str) -> Option<String> {
    let mut found = None;
    let mut in_section = false;
    for line in text.trim_start_matches('\u{feff}').lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_section = true;
        }
        if in_section || line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if is_registry_line(line) {
            let value = line.split_once('=').map(|(_, v)| v).unwrap_or("");
            let value = value.trim().trim_matches(|c| c == '"' || c == '\'').trim();
            if !value.is_empty() {
                found = Some(value.to_string());
            }
        }
    }
    found
}

/// `http(s)://[user@]host[:port]/...`; `display` carries only scheme, host and
/// port because it reaches the tenant-visible build log.
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
    let authority = rest.split(['/', '?', '#']).next()?;
    let host_port = authority.rsplit('@').next()?;
    if host_port.starts_with('[') {
        return None;
    }
    let (host, port) = match host_port.rsplit_once(':') {
        Some((host, port)) => (host, port.parse().ok()?),
        None => (host_port, default_port),
    };
    let host = host.trim().to_ascii_lowercase();
    if host.is_empty() || !host.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-') {
        return None;
    }
    let display = if port == default_port {
        format!("{scheme}://{host}")
    } else {
        format!("{scheme}://{host}:{port}")
    };
    Some(HostRegistry { display, host, port })
}

fn is_public_mirror(host: &str, extra_hosts: &str) -> bool {
    PUBLIC_MIRROR_HOSTS.contains(&host)
        || extra_hosts
            .split(',')
            .map(|h| h.trim().to_ascii_lowercase())
            .any(|h| !h.is_empty() && h == host)
}

async fn read_small(path: &Path) -> Option<String> {
    let meta = tokio::fs::symlink_metadata(path).await.ok()?;
    if !meta.is_file() || meta.len() > MAX_CONFIG_BYTES {
        return None;
    }
    tokio::fs::read_to_string(path).await.ok()
}

/// Every address of the host is dialed at once; the first connect wins.
async fn reachable(host: &str, port: u16) -> Result<(), String> {
    let race = async {
        let addrs: Vec<_> = tokio::net::lookup_host((host, port))
            .await
            .map_err(|e| format!("{host}:{port}: {e}"))?
            .collect();
        let mut dials = tokio::task::JoinSet::new();
        for addr in addrs {
            dials.spawn(async move { tokio::net::TcpStream::connect(addr).await.map(drop) });
        }
        let mut last = format!("{host}:{port}: no address");
        while let Some(result) = dials.join_next().await {
            match result {
                Ok(Ok(())) => return Ok(()),
                Ok(Err(e)) => last = format!("{host}:{port}: {e}"),
                Err(e) => last = format!("{host}:{port}: {e}"),
            }
        }
        Err(last)
    };
    match tokio::time::timeout(PROBE_TIMEOUT, race).await {
        Ok(result) => result,
        Err(_) => Err(format!("{host}:{port}: no answer within {}s", PROBE_TIMEOUT.as_secs())),
    }
}

/// The host npmrc with its registry replaced by the public one, written
/// atomically and readable only by this user (the original may hold tokens).
fn write_fallback_userconfig(dir: &Path, host_npmrc: &str) -> std::io::Result<PathBuf> {
    let mut text: String = host_npmrc
        .lines()
        .filter(|line| !is_registry_line(line.trim()))
        .map(|line| format!("{line}\n"))
        .collect();
    text.push_str(&format!("registry={PUBLIC_REGISTRY}\n"));
    std::fs::create_dir_all(dir)?;
    let path = dir.join(FALLBACK_FILE);
    let tmp = dir.join(format!("{FALLBACK_FILE}.{}.tmp", std::process::id()));
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        use std::io::Write;
        options.open(&tmp)?.write_all(text.as_bytes())?;
    }
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Probe the host registry once. `HIVE_BUILD_REGISTRY_GUARD=0` disables it.
pub async fn assess(home: &Path, global_npmrc: &Path, fallback_dir: &Path) -> Plan {
    if std::env::var("HIVE_BUILD_REGISTRY_GUARD").is_ok_and(|value| value == "0") {
        return Plan::Unchanged;
    }
    let user_npmrc = read_small(&home.join(".npmrc")).await;
    let configured = match user_npmrc.as_deref().and_then(npmrc_registry) {
        Some(value) => Some(value),
        None => read_small(global_npmrc).await.as_deref().and_then(npmrc_registry),
    };
    let Some(registry) = configured.as_deref().and_then(parse_registry) else {
        return Plan::Unchanged;
    };
    let extra = std::env::var("HIVE_BUILD_REGISTRY_MIRRORS").unwrap_or_default();
    if !is_public_mirror(&registry.host, &extra) {
        return Plan::Unchanged;
    }
    let Err(reason) = reachable(&registry.host, registry.port).await else {
        return Plan::Unchanged;
    };
    if let Err(public) = reachable(PUBLIC_REGISTRY_HOST, 443).await {
        return Plan::Dark {
            configured: registry.display,
            reason: format!("{reason}; {public}"),
        };
    }
    match write_fallback_userconfig(fallback_dir, user_npmrc.as_deref().unwrap_or("")) {
        Ok(userconfig) => Plan::Fallback {
            configured: registry.display,
            reason,
            userconfig,
        },
        Err(error) => Plan::Dark {
            configured: registry.display,
            reason: format!("{reason}; fallback npmrc could not be written: {error}"),
        },
    }
}
