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

/// One declared container port of a compose service, with its publish request.
///
/// Docker-compose's own `ports:` grammar distinguishes a PUBLISH request from a
/// bare container-side declaration: `"9000:9000"` (or long-syntax `published:`)
/// says "make this reachable from outside on host port 9000", while a bare
/// `"9000"` only documents the container-side listen port. The platform honors
/// the same split — a published entry asks the raw-port allocator for its
/// literal host port; a bare entry stays internal-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComposePort {
    /// Container-side listen port (`CONTAINER` in `HOST:CONTAINER`).
    pub container: u16,
    /// The declared HOST side, when the entry is a publish request.
    pub host: Option<u16>,
    pub protocol: ServiceProtocol,
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
    /// `"5432:5432/udp"` -> `Udp`, `"25565:25565/tcp"` -> `Tcp`. No suffix normalizes to
    /// `Http` — Compose's implicit-default transport, and the pre-existing behavior for
    /// the overwhelming common case (a web service listening HTTP-over-TCP). An explicit
    /// `/tcp` is the ONLY way to declare a raw non-HTTP TCP service (Minecraft,
    /// Postgres-wire, …) through plain compose `ports:` syntax — it must NOT collapse
    /// into the same `Http` bucket as "unspecified", or nothing could ever opt into the
    /// raw-splice proxy path (`FunctionConfig::needs_raw_proxy`) without the separate
    /// `x-shadw-expose` extension.
    pub protocol: ServiceProtocol,
    /// Explicit opt-in for external routing when this service is NOT the primary
    /// entrypoint (`x-shadw-expose`). See [`ComposeExpose`]; default is internal-only.
    pub expose: ComposeExpose,
    /// EVERY container-side port the service declares, in declaration order —
    /// `port`/`protocol` above are just the first of these.
    ///
    /// Only the first was ever parsed, so a service publishing more than one port
    /// had the rest discarded without a word. MinIO — the exact case that broke
    /// `compose-yaml.shadw.app` — declares `["9000:9000", "9001:9001"]`: the S3 API
    /// and the web console. The console simply did not exist as far as the platform
    /// was concerned, and nothing in the build output said so, which is why the
    /// symptom presented as "the port closes the connection" with no diagnosis
    /// available anywhere. Carrying the full list lets the manifest DOCUMENT the
    /// extra ports (and allocate public ones for raw-protocol extras) exactly the
    /// way the single-image path already does from the image's `ExposedPorts`.
    pub all_ports: Vec<ComposePort>,
    /// `command:` — the argv appended AFTER the image, exactly as Docker/Compose
    /// treat it (it overrides the image's `CMD`, not its `ENTRYPOINT`).
    ///
    /// Dropping this silently makes a large class of real compose files
    /// unstartable, because many official images ship an ENTRYPOINT that is
    /// useless without arguments. The canonical MinIO compose — the exact case
    /// that broke `compose-yaml.shadw.app` — is `image: minio/minio` plus
    /// `command: server /data --console-address ":9001"`; run bare, `minio`
    /// prints usage and exits non-zero in well under a second, so the container
    /// never listens, every cold start fails, and the deployment's circuit
    /// opens. Same for `postgres` with a custom `-c` flag, `redis-server` with a
    /// config path, and so on.
    pub command: Option<Vec<String>>,
    /// `entrypoint:` — replaces the image's own ENTRYPOINT (podman
    /// `--entrypoint`). Parsed alongside `command` for the same reason.
    pub entrypoint: Option<Vec<String>>,
}

/// Parse a Compose `command:`/`entrypoint:` value, which may be either a YAML
/// sequence (already argv) or a single string (shell-style, split on
/// whitespace honoring quotes — Compose's own "shell form").
fn parse_argv(v: Option<&serde_yaml::Value>) -> Option<Vec<String>> {
    match v {
        Some(serde_yaml::Value::Sequence(seq)) => {
            let argv: Vec<String> = seq
                .iter()
                .filter_map(|x| match x {
                    serde_yaml::Value::String(s) => Some(s.clone()),
                    serde_yaml::Value::Number(n) => Some(n.to_string()),
                    serde_yaml::Value::Bool(b) => Some(b.to_string()),
                    _ => None,
                })
                .collect();
            (!argv.is_empty()).then_some(argv)
        }
        Some(serde_yaml::Value::String(s)) => {
            let argv = shell_split(s);
            (!argv.is_empty()).then_some(argv)
        }
        _ => None,
    }
}

/// Split a shell-form command string into argv, honoring single and double
/// quotes. MinIO's `--console-address ":9001"` is the motivating case: a naive
/// `split_whitespace` would hand podman a literal `":9001"` INCLUDING the
/// quote characters, which the app then fails to parse as an address.
fn shell_split(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut any = false;
    for c in s.chars() {
        match quote {
            Some(q) if c == q => {
                quote = None;
            }
            Some(_) => cur.push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                any = true; // `""` is a real, empty argument
            }
            None if c.is_whitespace() => {
                if !cur.is_empty() || any {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            None => cur.push(c),
        }
    }
    if !cur.is_empty() || any {
        out.push(cur);
    }
    out
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
        let all_ports = container_ports(&ports);
        let (port, protocol) = all_ports
            .first()
            .map(|p| (p.container, p.protocol))
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
            command: parse_argv(svc.get("command")),
            entrypoint: parse_argv(svc.get("entrypoint")),
            all_ports,
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
    container_ports(ports)
        .first()
        .map(|p| (p.container, p.protocol))
}

/// EVERY container-side port in a `ports:` sequence, in declaration order, deduped
/// on (port, protocol). Same per-entry grammar as [`container_port`], which is now
/// just "the first of these" — see [`ParsedService::all_ports`] for why the rest
/// must not be dropped on the floor.
fn container_ports(ports: &[serde_yaml::Value]) -> Vec<ComposePort> {
    let mut out: Vec<ComposePort> = Vec::new();
    for p in ports {
        if let Some(e) = one_container_port(p) {
            // Dedup on the container side; declaration order wins, same as the
            // primary-port rule.
            if !out
                .iter()
                .any(|x| x.container == e.container && x.protocol == e.protocol)
            {
                out.push(e);
            }
        }
    }
    out
}

fn one_container_port(p: &serde_yaml::Value) -> Option<ComposePort> {
    if let Some(s) = p.as_str() {
        // Short syntax: "PORT", "HOST:CONTAINER", "IP:HOST:CONTAINER", each with
        // an optional "/udp"/"/tcp" suffix. The LAST colon-segment is the
        // container side; the one before it (when numeric) is the publish
        // request's host port. An IP prefix ("127.0.0.1:8080:80") is accepted
        // and the IP itself ignored — the platform's raw ingress binds fleet
        // addresses, not the compose author's loopback.
        let (bare, protocol) = split_proto_suffix(s);
        let segs: Vec<&str> = bare.split(':').collect();
        let container = segs.last()?.trim().parse::<u16>().ok()?;
        // Host `0` is docker's "pick an ephemeral port" — a publish with no
        // specific number, so it maps to "published, no preference"… which the
        // platform has no lane for (publish implies a concrete public port), so
        // it normalizes to internal-only rather than a literal preference of 0.
        let host = if segs.len() >= 2 {
            segs[segs.len() - 2]
                .trim()
                .parse::<u16>()
                .ok()
                .filter(|&h| h != 0)
        } else {
            None
        };
        return Some(ComposePort {
            container,
            host,
            protocol,
        });
    }
    if let Some(n) = p.get("target").and_then(|t| t.as_u64()) {
        let protocol = p
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
        // Long syntax carries the publish request as `published:` (number, or a
        // string for ranges — only a plain number can be honored per-entry).
        let host = p
            .get("published")
            .and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            })
            .and_then(|n| u16::try_from(n).ok())
            .filter(|&h| h != 0);
        return u16::try_from(n).ok().map(|container| ComposePort {
            container,
            host,
            protocol,
        });
    }
    if let Some(n) = p.as_u64() {
        return u16::try_from(n).ok().map(|container| ComposePort {
            container,
            host: None,
            protocol: ServiceProtocol::Http,
        });
    }
    None
}

/// Split a compose port string's optional transport suffix, e.g. `"5432:5432/udp"`
/// -> (`"5432:5432"`, `Udp`). Mirrors the pre-existing `s.split('/').next()`
/// stripping (any single `/`-delimited suffix), now CLASSIFYING it instead of
/// silently discarding it: `udp` -> `Udp`, `tcp` -> `Tcp` (both explicit
/// transport markers are equally unambiguous signal for the raw-splice proxy
/// path — there is no other way to declare a non-HTTP TCP service, e.g.
/// Minecraft/Postgres-wire, through plain compose `ports:` syntax, so an
/// explicit `/tcp` must not collapse into the same bucket as "unspecified");
/// no suffix (or any other unrecognized value) -> `Http`, Compose's
/// implicit-default transport, unchanged.
fn split_proto_suffix(s: &str) -> (&str, ServiceProtocol) {
    match s.split_once('/') {
        Some((bare, proto)) if proto.eq_ignore_ascii_case("udp") => (bare, ServiceProtocol::Udp),
        Some((bare, proto)) if proto.eq_ignore_ascii_case("tcp") => (bare, ServiceProtocol::Tcp),
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
