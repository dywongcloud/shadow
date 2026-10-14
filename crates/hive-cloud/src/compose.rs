//! docker-compose / `compose.yaml` support.
//!
//! Parses a Compose file into normalized container SERVICES. This module is PURE (no podman, no IO
//! beyond reading the file); the deploy path is `git::compose_container_manifest`, which deploys
//! ONLY the PRIMARY service ([`primary_service`]: the first published service with a conventional
//! web name, else the first published service, else the first service) as the project's single
//! routable container function, because the unit of deployment is one project namespace with one
//! public entrypoint. It is built from source when it declares `build:`, pulled when it declares
//! `image:` (`build` wins when both are present, as in Compose), and of the parsed data that path
//! reads only `build`, `image`, `port` and `protocol`.
//!
//! Everything else is modeled and validated here but not applied to the deployment: the other
//! services, `environment`, `command`, `entrypoint`, every declared port beyond the first
//! (`all_ports`) and the `x-shadw-expose` extension ([`ComposeExpose`]), the opt-in meant to expose
//! a NON-PRIMARY service. `hive-cloud --compose-probe` prints what the parser read.

use fluid_core::ServiceProtocol;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// A build directive: `build: ./dir` or `build: { context: ., dockerfile: Foo }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposeBuild {
    pub context: String,
    pub dockerfile: Option<String>,
    /// Build stage to stop at (`target:`). None = build the Dockerfile's final stage.
    /// The deploy path passes it to `podman build --target`, so the stage the compose file asked
    /// for is the one built, not the Dockerfile's last.
    pub target: Option<String>,
}

/// The `x-shadw-expose` extension field of a service (Compose's `x-` prefix convention for
/// tool-specific data, ignored by real `docker compose`): the explicit opt-in for exposing a
/// NON-PRIMARY service externally, so a private sidecar does not become public just because it
/// declares `ports:`. The shorthand `x-shadw-expose: true` exposes the service on its own parsed
/// port and protocol; the mapping form overrides either, e.g. a standalone Postgres wanting a raw
/// TCP target:
/// ```yaml
/// services:
///   db:
///     image: postgres:16
///     ports: ["5432:5432"]
///     x-shadw-expose:
///       protocol: tcp
/// ```
/// An absent field (the default) means internal-only. The field is parsed and validated
/// ([`parse_expose_ext`]), but the deploy path builds only the primary service, so nothing reads it
/// yet.
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
/// Compose's `ports:` grammar distinguishes a PUBLISH request from a bare container-side
/// declaration: `"9000:9000"` (or long-syntax `published:`) says 'make this reachable from outside
/// on host port 9000', while a bare `"9000"` only documents the container-side listen port. `host`
/// carries that split (`None` = internal-only). The deploy path does not read it; `--compose-probe`
/// reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComposePort {
    /// Container-side listen port (`CONTAINER` in `HOST:CONTAINER`).
    pub container: u16,
    /// The declared HOST side, when the entry is a publish request.
    pub host: Option<u16>,
    pub protocol: ServiceProtocol,
}

/// One parsed Compose service. The deploy path (`git::compose_container_manifest`) reads only
/// `build`, `image`, `port` and `protocol` of the PRIMARY service; the other fields are parsed and
/// validated but not applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedService {
    pub name: String,
    /// Explicit prebuilt image (`image:`), used when there's no `build:`.
    pub image: Option<String>,
    /// Build-from-source directive (`build:`), takes precedence over `image`.
    pub build: Option<ComposeBuild>,
    /// Port the container listens on (from `ports:` target, else `expose:`, else 8080).
    pub port: u16,
    /// True when the service declares any `ports:` entry; it only steers [`primary_service`].
    pub published: bool,
    /// Service env (`environment:` as a map or `K=V` list).
    pub env: BTreeMap<String, String>,
    /// Transport/application protocol for `port`, parsed from the compose port's `/tcp` or `/udp`
    /// short-syntax suffix (the long-syntax `protocol:` key recognizes only `udp`: `tcp` there
    /// still reads as `Http`), e.g. `"5432:5432/udp"` -> `Udp`, `"25565:25565/tcp"` -> `Tcp`. No
    /// suffix is `Http`, Compose's implicit default and the overwhelmingly common case (a web
    /// service listening HTTP-over-TCP). An explicit `/tcp` is the only way to declare a raw
    /// non-HTTP TCP service (Minecraft, Postgres wire, ...) through plain compose `ports:` syntax,
    /// so it must NOT collapse into the same `Http` bucket as 'unspecified': the primary's protocol
    /// decides whether the deployment is spliced as a raw connection
    /// (`ServiceProtocol::needs_raw_proxy`).
    pub protocol: ServiceProtocol,
    /// Explicit opt-in for external routing when this service is NOT the primary
    /// entrypoint (`x-shadw-expose`). See [`ComposeExpose`]; default is internal-only.
    pub expose: ComposeExpose,
    /// EVERY container-side port the service declares, in declaration order; `port`/`protocol`
    /// above are just the first of these. A service can declare several (MinIO declares
    /// `["9000:9000", "9001:9001"]`: the S3 API and the web console); the deploy path routes only
    /// the first.
    pub all_ports: Vec<ComposePort>,
    /// `command:`: the argv appended AFTER the image, as Docker/Compose treat it (it overrides the
    /// image's `CMD`, not its `ENTRYPOINT`). Parsed here but not applied by the deploy path, which
    /// starts the primary with its image's own `CMD`.
    pub command: Option<Vec<String>>,
    /// `entrypoint:`: replaces the image's own ENTRYPOINT (podman `--entrypoint`). Parsed with
    /// `command`, and likewise not applied by the deploy path.
    pub entrypoint: Option<Vec<String>>,
}

/// Parses a Compose `command:`/`entrypoint:` value, which may be either a YAML sequence (already
/// argv) or a single string (shell form: split on whitespace honoring quotes, Compose's own rule).
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

/// Splits a shell-form command string into argv, honoring single and double quotes; an empty quoted
/// argument (`""`) is a real, empty argument. MinIO's `--console-address ":9001"` is the motivating
/// case: a naive `split_whitespace` would hand podman a literal `":9001"` INCLUDING the quote
/// characters, which the app then fails to parse as an address.
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
                any = true;
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

/// Parses Compose YAML text into normalized services, sorted by name (deterministic). Tolerant of
/// the common Compose shapes: keys this module does not model (networks, volumes, depends_on,
/// healthcheck, ...) are ignored rather than failing the build.
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

/// The single PUBLIC entrypoint service to route `/` to: the first published service with a
/// conventional web name (`WEB_NAMES`), else the first published service, else the first service.
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
            target: None,
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

            let target = v?.get("target").and_then(|c| c.as_str()).map(|s| s.to_string());
            Some(ComposeBuild {
                context,
                dockerfile,
                target,
            })
        }
        _ => None,
    }
}

/// The CONTAINER-side port + transport protocol from a `ports:` sequence (`"HOST:CONTAINER"`,
/// `"PORT"`, `"IP:HOST:CONTAINER"`, `".../udp"`, or the long syntax `{ target: N, protocol: "udp"
/// }`): what the app listens on, plus whether the mapping declared a real transport change (see
/// [`ParsedService::protocol`]).
fn container_port(ports: &[serde_yaml::Value]) -> Option<(u16, ServiceProtocol)> {
    container_ports(ports)
        .first()
        .map(|p| (p.container, p.protocol))
}

/// EVERY container-side port in a `ports:` sequence, in declaration order, deduped on (port,
/// protocol) with declaration order winning, as in the primary-port rule. Same per-entry grammar as
/// [`container_port`], which is just 'the first of these'; see [`ParsedService::all_ports`].
fn container_ports(ports: &[serde_yaml::Value]) -> Vec<ComposePort> {
    let mut out: Vec<ComposePort> = Vec::new();
    for p in ports {
        if let Some(e) = one_container_port(p) {
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

/// Parses one `ports:` entry. Short syntax is `PORT`, `HOST:CONTAINER` or `IP:HOST:CONTAINER`, each
/// with an optional `/udp` or `/tcp` suffix: the LAST colon segment is the container side and the
/// one before it (when numeric) is the publish request's host port. An IP prefix
/// (`127.0.0.1:8080:80`) is accepted and ignored. Host `0` is docker's 'pick an ephemeral port', a
/// publish with no specific number, so it normalizes to no publish request (`None`). Long syntax
/// carries the publish request as `published:` (a number, or a string for ranges, of which only a
/// plain number is honored per entry).
fn one_container_port(p: &serde_yaml::Value) -> Option<ComposePort> {
    if let Some(s) = p.as_str() {
        let (bare, protocol) = split_proto_suffix(s);
        let segs: Vec<&str> = bare.split(':').collect();
        let container = segs.last()?.trim().parse::<u16>().ok()?;
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

/// Splits a compose port string's optional transport suffix, e.g. `"5432:5432/udp"` ->
/// (`"5432:5432"`, `Udp`). The suffix is CLASSIFIED rather than discarded: `udp` -> `Udp` and `tcp`
/// -> `Tcp` (an explicit `/tcp` is the only way to declare a non-HTTP TCP service, e.g. Minecraft
/// or Postgres wire, through plain compose `ports:` syntax, so it must not collapse into the bucket
/// of 'unspecified'), while no suffix (or any other unrecognized value) is `Http`, Compose's
/// implicit default.
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

/// Parses the `x-shadw-expose` extension field, if present. Tolerant of both the boolean shorthand
/// and the full mapping form (any other shape, e.g. a bare string or number, is ignored and the
/// service stays internal, matching this module's tolerance of unmodeled keys). An unparseable
/// `protocol` string IS a hard error: this field toggles PUBLIC exposure, so silently ignoring a
/// typo could leak a service the user meant to keep locked down, or fail to expose one they meant
/// to open up.
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
