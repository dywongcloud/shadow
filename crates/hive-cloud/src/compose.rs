//! docker-compose / `compose.yaml` support.
//!
//! Parses a Compose file into a set of container SERVICES that the build pipeline
//! turns into a MULTI-service manifest (one `__container__` `FunctionConfig` per
//! service) under a single project namespace. All of a deployment's services run on
//! the lease owner, joined to one per-project podman network so they reach each other
//! by service name (Compose semantics). Only the PRIMARY service (the conventional web
//! entrypoint, or else the first service that publishes `ports:`) gets a public route
//! by default; every other service — published or not — stays internal-only (reachable
//! on the shared network by name, kept warm via `min_instances`), e.g. a database a web
//! service talks to. This is deliberate: a private DB sidecar should not become publicly
//! reachable just because it happens to declare `ports:`. A non-primary service that DOES
//! want external exposure (e.g. a standalone Postgres/Redis meant to be reachable from
//! outside) opts in explicitly via the `x-shadw-expose` custom-extension field — see
//! [`ComposeExpose`].
//!
//! This module is PURE (no podman / IO beyond reading the file) so it's unit-tested;
//! the actual image builds + manifest assembly live in `git.rs::build_compose_manifest`.

use fluid_core::ServiceProtocol;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// A build directive: `build: ./dir` or `build: { context: ., dockerfile: Foo }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposeBuild {
    pub context: String,
    pub dockerfile: Option<String>,
}

/// Explicit opt-in for routing a NON-PRIMARY service externally, read from an
/// `x-shadw-expose` field under the service definition — Compose's own `x-` prefix
/// convention for tool-specific extension data, silently ignored by real
/// `docker compose` (mirrors the `fluid.json` `container` override pattern used for
/// single-Dockerfile deploys; see `git.rs`'s `ContainerOverride`). Shorthand
/// `x-shadw-expose: true` exposes the service on its own already-parsed port +
/// protocol; the mapping form lets either be overridden explicitly, e.g. a standalone
/// Postgres service wanting a raw TCP proxy target:
/// ```yaml
/// services:
///   db:
///     image: postgres:16
///     ports: ["5432:5432"]
///     x-shadw-expose:
///       protocol: tcp
/// ```
/// Absent field (the default) keeps the service internal-only — unchanged behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ComposeExpose {
    pub enabled: bool,
    /// Overrides the service's own parsed `protocol` for the exposed target, when set.
    pub protocol: Option<ServiceProtocol>,
    /// Overrides the service's own parsed `port` for the exposed target, when set.
    pub port: Option<u16>,
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
    /// Transport/application protocol for `port`, parsed from the compose port's
    /// `/tcp` or `/udp` short-syntax suffix (or the long-syntax `protocol:` key), e.g.
    /// `"5432:5432/udp"` -> `Udp`. No suffix, or an explicit `/tcp`, both normalize to
    /// `Http` — Compose's implicit-default transport, and the pre-existing behavior for
    /// the overwhelming common case (a web service listening HTTP-over-TCP). Only an
    /// explicit `udp` component is unambiguous signal for the raw-splice proxy path
    /// (`FunctionConfig::needs_raw_proxy`): a bare `/tcp` carries no extra information
    /// since TCP is already Compose's default transport for an ordinary HTTP service.
    pub protocol: ServiceProtocol,
    /// Explicit opt-in for external routing when this service is NOT the primary
    /// entrypoint (`x-shadw-expose`). See [`ComposeExpose`]; default is internal-only.
    pub expose: ComposeExpose,
}

/// Locate a Compose file in `dir` (the canonical names, newest spec first).
pub fn compose_file(dir: &Path) -> Option<PathBuf> {
    [
        "compose.yaml",
        "compose.yml",
        "docker-compose.yml",
        "docker-compose.yaml",
    ]
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
        let image = svc
            .get("image")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let build = parse_build(svc.get("build"));
        let ports = svc
            .get("ports")
            .and_then(|v| v.as_sequence())
            .cloned()
            .unwrap_or_default();
        let published = !ports.is_empty();
        let (port, protocol) = container_port(&ports)
            .or_else(|| {
                svc.get("expose")
                    .and_then(|v| v.as_sequence())
                    .and_then(|s| first_port(s))
            })
            .unwrap_or((8080, ServiceProtocol::Http));
        let env = parse_env(svc.get("environment"));
        let expose = parse_expose_ext(svc.get("x-shadw-expose"))
            .map_err(|e| anyhow::anyhow!("service '{name}': {e}"))?;
        out.push(ParsedService {
            name,
            image,
            build,
            port,
            published,
            env,
            protocol,
            expose,
        });
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
        Some(serde_yaml::Value::String(s)) => Some(ComposeBuild {
            context: s.clone(),
            dockerfile: None,
        }),
        Some(serde_yaml::Value::Mapping(_)) => {
            let context = v?
                .get("context")
                .and_then(|c| c.as_str())
                .unwrap_or(".")
                .to_string();
            let dockerfile = v?
                .get("dockerfile")
                .and_then(|c| c.as_str())
                .map(|s| s.to_string());
            Some(ComposeBuild {
                context,
                dockerfile,
            })
        }
        _ => None,
    }
}

/// The CONTAINER-side port + transport protocol from a `ports:` sequence
/// ("HOST:CONTAINER", "PORT", "IP:HOST:CONTAINER", "…/udp", or the long-syntax
/// `{ target: N, protocol: "udp" }`). We want what the app listens on, plus whether
/// the mapping declared a real transport change — see [`ParsedService::protocol`].
fn container_port(ports: &[serde_yaml::Value]) -> Option<(u16, ServiceProtocol)> {
    for p in ports {
        if let Some(s) = p.as_str() {
            // Split off an optional /proto suffix, take the last colon-segment (container side).
            let (bare, proto) = split_proto_suffix(s);
            if let Some(seg) = bare.rsplit(':').next() {
                if let Ok(n) = seg.trim().parse::<u16>() {
                    return Some((n, proto));
                }
            }
        } else if let Some(n) = p.get("target").and_then(|t| t.as_u64()) {
            let proto = p
                .get("protocol")
                .and_then(|v| v.as_str())
                .map(|s| {
                    if s.eq_ignore_ascii_case("udp") {
                        ServiceProtocol::Udp
                    } else {
                        ServiceProtocol::Http
                    }
                })
                .unwrap_or(ServiceProtocol::Http);
            return u16::try_from(n).ok().map(|n| (n, proto));
        } else if let Some(n) = p.as_u64() {
            return u16::try_from(n).ok().map(|n| (n, ServiceProtocol::Http));
        }
    }
    None
}

/// Split a compose port string's optional transport suffix, e.g. `"5432:5432/udp"`
/// -> (`"5432:5432"`, `Udp`). Mirrors the pre-existing `s.split('/').next()`
/// stripping (any single `/`-delimited suffix), now CLASSIFYING it instead of
/// silently discarding it: `udp` -> `Udp`; anything else (`tcp`, absent, or an
/// unrecognized value) -> `Http`, Compose's implicit-default transport.
fn split_proto_suffix(s: &str) -> (&str, ServiceProtocol) {
    match s.split_once('/') {
        Some((bare, proto)) if proto.eq_ignore_ascii_case("udp") => (bare, ServiceProtocol::Udp),
        Some((bare, _)) => (bare, ServiceProtocol::Http),
        None => (s, ServiceProtocol::Http),
    }
}

fn first_port(seq: &[serde_yaml::Value]) -> Option<(u16, ServiceProtocol)> {
    seq.iter().find_map(|v| {
        v.as_u64()
            .and_then(|n| u16::try_from(n).ok())
            .map(|n| (n, ServiceProtocol::Http))
            .or_else(|| {
                let s = v.as_str()?;
                let (bare, proto) = split_proto_suffix(s);
                bare.trim().parse::<u16>().ok().map(|n| (n, proto))
            })
    })
}

/// Parse the `x-shadw-expose` extension field, if present. Tolerant of both the
/// boolean shorthand and the full mapping form (any other shape, e.g. a bare
/// string/number, is ignored — stays internal, matching this module's tolerant-of-
/// unmodeled-keys convention). An unparseable `protocol` string IS a hard error: this
/// field toggles PUBLIC exposure, so silently ignoring a typo could leak a service the
/// user meant to keep locked down, or fail to expose one they meant to open up.
fn parse_expose_ext(v: Option<&serde_yaml::Value>) -> anyhow::Result<ComposeExpose> {
    match v {
        None => Ok(ComposeExpose::default()),
        Some(serde_yaml::Value::Bool(b)) => Ok(ComposeExpose {
            enabled: *b,
            protocol: None,
            port: None,
        }),
        Some(m @ serde_yaml::Value::Mapping(_)) => {
            let enabled = m.get("expose").and_then(|e| e.as_bool()).unwrap_or(true);
            let protocol = match m.get("protocol").and_then(|p| p.as_str()) {
                Some(s) => Some(
                    ServiceProtocol::from_str(s)
                        .map_err(|e| anyhow::anyhow!("x-shadw-expose.protocol: {e}"))?,
                ),
                None => None,
            };
            let port = match m.get("port").and_then(|p| p.as_u64()) {
                Some(n) => Some(
                    u16::try_from(n)
                        .map_err(|_| anyhow::anyhow!("x-shadw-expose.port out of range"))?,
                ),
                None => None,
            };
            Ok(ComposeExpose {
                enabled,
                protocol,
                port,
            })
        }
        Some(_) => Ok(ComposeExpose::default()),
    }
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
        assert_eq!(
            api.build.as_ref().unwrap().dockerfile.as_deref(),
            Some("Dockerfile.api")
        );
        assert_eq!(
            api.env.get("DATABASE_URL").unwrap(),
            "postgres://db:5432/app"
        );

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
