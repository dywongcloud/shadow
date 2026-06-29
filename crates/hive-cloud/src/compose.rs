//! docker-compose / `compose.yaml` support.
//!
//! Parses a Compose file into a set of container SERVICES that the build pipeline
//! turns into a MULTI-service manifest (one `__container__` `FunctionConfig` per
//! service) under a single project namespace. All of a deployment's services run on
//! the lease owner, joined to one per-project podman network so they reach each other
//! by service name (Compose semantics). Services that publish `ports:` are public
//! (routable); `expose`-only / portless services are internal (no public route) but
//! still run (kept warm via `min_instances`), e.g. a database a web service talks to.
//!
//! This module is PURE (no podman / IO beyond reading the file) so it's unit-tested;
//! the actual image builds + manifest assembly live in `git.rs::produce_manifest`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A build directive: `build: ./dir` or `build: { context: ., dockerfile: Foo }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposeBuild {
    pub context: String,
    pub dockerfile: Option<String>,
}

/// One parsed Compose service, normalized for the build pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedService {
    pub name: String,
    /// Explicit prebuilt image (`image:`), used when there's no `build:`.
    pub image: Option<String>,
    /// Build-from-source directive (`build:`), takes precedence over `image`.
    pub build: Option<ComposeBuild>,
    /// Port the container listens on (from `ports:` target, else `expose:`, else 8080).
    pub port: u16,
    /// True when the service publishes `ports:` — it's a PUBLIC (routable) service;
    /// false = internal-only (reachable on the shared network by name).
    pub published: bool,
    /// Service env (`environment:` as a map or `K=V` list).
    pub env: BTreeMap<String, String>,
}

/// Locate a Compose file in `dir` (the canonical names, newest spec first).
pub fn compose_file(dir: &Path) -> Option<PathBuf> {
    ["compose.yaml", "compose.yml", "docker-compose.yml", "docker-compose.yaml"]
        .iter()
        .map(|f| dir.join(f))
        .find(|p| p.exists())
}

/// Parse Compose YAML text into normalized services, sorted by name (deterministic).
/// Tolerant of the common Compose shapes; ignores keys we don't model (networks,
/// volumes, depends_on, healthcheck, …) rather than failing the build.
pub fn parse_compose(text: &str) -> anyhow::Result<Vec<ParsedService>> {
    let root: serde_yaml::Value = serde_yaml::from_str(text)?;
    let services = root
        .get("services")
        .and_then(|v| v.as_mapping())
        .ok_or_else(|| anyhow::anyhow!("compose file has no `services:` map"))?;

    let mut out = Vec::new();
    for (name_v, svc) in services {
        let name = match name_v.as_str() {
            Some(s) => s.to_string(),
            None => continue,
        };
        let image = svc.get("image").and_then(|v| v.as_str()).map(|s| s.to_string());
        let build = parse_build(svc.get("build"));
        let ports = svc.get("ports").and_then(|v| v.as_sequence()).cloned().unwrap_or_default();
        let published = !ports.is_empty();
        let port = container_port(&ports)
            .or_else(|| svc.get("expose").and_then(|v| v.as_sequence()).and_then(|s| first_port(s)))
            .unwrap_or(8080);
        let env = parse_env(svc.get("environment"));
        out.push(ParsedService { name, image, build, port, published, env });
    }
    anyhow::ensure!(!out.is_empty(), "compose file declares no services");
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// The single PUBLIC entrypoint service to route `/` to: a conventional web name if
/// present, else the first service that publishes ports, else the first service.
pub fn primary_service(services: &[ParsedService]) -> Option<&ParsedService> {
    const WEB_NAMES: &[&str] = &["web", "app", "frontend", "www", "gateway", "api", "server"];
    services
        .iter()
        .find(|s| s.published && WEB_NAMES.contains(&s.name.as_str()))
        .or_else(|| services.iter().find(|s| s.published))
        .or_else(|| services.first())
}

fn parse_build(v: Option<&serde_yaml::Value>) -> Option<ComposeBuild> {
    match v {
        Some(serde_yaml::Value::String(s)) => Some(ComposeBuild { context: s.clone(), dockerfile: None }),
        Some(serde_yaml::Value::Mapping(_)) => {
            let context = v?.get("context").and_then(|c| c.as_str()).unwrap_or(".").to_string();
            let dockerfile = v?.get("dockerfile").and_then(|c| c.as_str()).map(|s| s.to_string());
            Some(ComposeBuild { context, dockerfile })
        }
        _ => None,
    }
}

/// The CONTAINER-side port from a `ports:` sequence ("HOST:CONTAINER", "PORT",
/// "IP:HOST:CONTAINER", or `{ target: N }`). We want what the app listens on.
fn container_port(ports: &[serde_yaml::Value]) -> Option<u16> {
    for p in ports {
        if let Some(s) = p.as_str() {
            // Strip an optional /proto suffix, take the last colon-segment (container side).
            let bare = s.split('/').next().unwrap_or(s);
            if let Some(seg) = bare.rsplit(':').next() {
                if let Ok(n) = seg.trim().parse::<u16>() {
                    return Some(n);
                }
            }
        } else if let Some(n) = p.get("target").and_then(|t| t.as_u64()) {
            return u16::try_from(n).ok();
        } else if let Some(n) = p.as_u64() {
            return u16::try_from(n).ok();
        }
    }
    None
}

fn first_port(seq: &[serde_yaml::Value]) -> Option<u16> {
    seq.iter().find_map(|v| {
        v.as_u64()
            .and_then(|n| u16::try_from(n).ok())
            .or_else(|| v.as_str().and_then(|s| s.split('/').next()?.trim().parse::<u16>().ok()))
    })
}

fn parse_env(v: Option<&serde_yaml::Value>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    match v {
        Some(serde_yaml::Value::Mapping(m)) => {
            for (k, val) in m {
                if let Some(k) = k.as_str() {
                    let s = match val {
                        serde_yaml::Value::String(s) => s.clone(),
                        serde_yaml::Value::Bool(b) => b.to_string(),
                        serde_yaml::Value::Number(n) => n.to_string(),
                        _ => continue,
                    };
                    out.insert(k.to_string(), s);
                }
            }
        }
        Some(serde_yaml::Value::Sequence(seq)) => {
            for item in seq {
                if let Some(s) = item.as_str() {
                    if let Some((k, val)) = s.split_once('=') {
                        out.insert(k.trim().to_string(), val.to_string());
                    }
                }
            }
        }
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
services:
  web:
    build: .
    ports:
      - "8080:3000"
    environment:
      API_URL: http://api:9000
  api:
    build:
      context: ./api
      dockerfile: Dockerfile.api
    expose:
      - "9000"
    environment:
      - DATABASE_URL=postgres://db:5432/app
  db:
    image: postgres:16
    expose:
      - 5432
"#;

    #[test]
    fn parses_services_ports_build_env() {
        let svcs = parse_compose(SAMPLE).unwrap();
        assert_eq!(svcs.len(), 3);
        let web = svcs.iter().find(|s| s.name == "web").unwrap();
        assert_eq!(web.port, 3000); // container side of "8080:3000"
        assert!(web.published);
        assert_eq!(web.env.get("API_URL").unwrap(), "http://api:9000");
        assert_eq!(web.build.as_ref().unwrap().context, ".");

        let api = svcs.iter().find(|s| s.name == "api").unwrap();
        assert_eq!(api.port, 9000); // from `expose`
        assert!(!api.published); // expose-only = internal
        assert_eq!(api.build.as_ref().unwrap().dockerfile.as_deref(), Some("Dockerfile.api"));
        assert_eq!(api.env.get("DATABASE_URL").unwrap(), "postgres://db:5432/app");

        let db = svcs.iter().find(|s| s.name == "db").unwrap();
        assert_eq!(db.port, 5432);
        assert!(!db.published);
        assert_eq!(db.image.as_deref(), Some("postgres:16"));
        assert!(db.build.is_none());
    }

    #[test]
    fn primary_is_the_public_web_service() {
        let svcs = parse_compose(SAMPLE).unwrap();
        // `web` is published AND a conventional name → the routable entrypoint.
        assert_eq!(primary_service(&svcs).unwrap().name, "web");
    }

    #[test]
    fn primary_falls_back_to_first_published() {
        let yaml = r#"
services:
  zinternal:
    image: redis:7
    expose: [6379]
  payments:
    image: pay:1
    ports: ["443:8443"]
"#;
        let svcs = parse_compose(yaml).unwrap();
        // No conventional web name; the only PUBLISHED service wins (not the db).
        assert_eq!(primary_service(&svcs).unwrap().name, "payments");
        assert_eq!(primary_service(&svcs).unwrap().port, 8443);
    }

    #[test]
    fn no_services_is_an_error() {
        assert!(parse_compose("version: '3'\n").is_err());
    }
}
