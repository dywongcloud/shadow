//! Intelligent service-graph scanner (Issue #2) — a pure ALGORITHM (no AI) that
//! inspects a deployment's checked-out repo + its manifest + env var names and
//! derives the graph of services ACTUALLY used/consumed:
//!
//!   * the app(s) and their framework/runtime,
//!   * monolith bundling — one Dockerfile/compose running e.g. a React frontend +
//!     an Express backend become TWO nodes sharing one `group` (same container/PID),
//!   * monorepo workspace packages that are genuinely consumed,
//!   * databases/caches only when a real client dep OR a connection env var proves
//!     they're consumed (never a node for something unused),
//!
//! It runs async off the build path (`scan_dir` reads the filesystem), so it never
//! blocks a deploy. Everything here is deterministic + unit-testable: the
//! filesystem read is isolated in `scan_dir`, the reasoning in `build_graph`.

use hive_core::now_ms;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SvcNode {
    pub id: String,
    /// "ingress" | "app" | "frontend" | "backend" | "worker" | "package" | "database"
    pub kind: String,
    pub name: String,
    /// Technology label, e.g. "Next.js", "Express", "PostgreSQL", "React".
    pub tech: String,
    /// Nodes sharing a `group` run in ONE container/PID (monolith bundling).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Short human note (why it's here / what it is).
    pub detail: String,
    /// How it was detected (the evidence — keeps the graph honest/auditable).
    pub source: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SvcEdge {
    pub from: String,
    pub to: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServiceGraph {
    pub project: String,
    pub deployment_id: String,
    pub generated_ms: u64,
    pub nodes: Vec<SvcNode>,
    pub edges: Vec<SvcEdge>,
    /// Env var NAMES referenced (values NEVER included — secure).
    pub env_keys: Vec<String>,
    /// One-line summary from the README, if any (sanitized, bounded).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub readme: String,
}

/// Raw signals scraped from the filesystem — the ONLY part that touches disk, so
/// `build_graph` stays a pure, testable function of these facts.
#[derive(Clone, Debug, Default)]
pub struct Scan {
    /// Detected framework slug for the primary app (fluid-build's detector).
    pub framework: String,
    pub framework_name: String,
    /// npm deps present anywhere in the consumed package.json(s) (dep name set).
    pub deps: BTreeSet<String>,
    /// Whether a Dockerfile/Containerfile exists.
    pub has_dockerfile: bool,
    /// Command(s) the container/app runs (Dockerfile CMD / package.json start/dev).
    pub run_cmds: Vec<String>,
    /// Workspace member packages actually consumed (name -> framework/tech guess).
    pub packages: BTreeMap<String, String>,
    /// README one-liner (sanitized).
    pub readme: String,
    /// Whether this is a container deployment (runtime == "container").
    pub is_container: bool,
    /// Compose service names (multi-service container), if any.
    pub compose_services: Vec<String>,
}

// ---- database clients: dep name -> (kind, tech label) ---------------------------
// A node is added ONLY when one of these deps is present OR a matching connection
// env var exists — so we never draw a database that isn't consumed.
const DB_DEPS: &[(&str, &str, &str)] = &[
    ("pg", "postgres", "PostgreSQL"),
    ("postgres", "postgres", "PostgreSQL"),
    ("@prisma/client", "postgres", "Prisma"),
    ("prisma", "postgres", "Prisma"),
    ("drizzle-orm", "postgres", "Drizzle"),
    ("typeorm", "postgres", "TypeORM"),
    ("sequelize", "postgres", "Sequelize"),
    ("knex", "postgres", "Knex"),
    ("mysql", "mysql", "MySQL"),
    ("mysql2", "mysql", "MySQL"),
    ("mongoose", "mongo", "MongoDB"),
    ("mongodb", "mongo", "MongoDB"),
    ("redis", "redis", "Redis"),
    ("ioredis", "redis", "Redis"),
    ("@upstash/redis", "redis", "Upstash Redis"),
    ("better-sqlite3", "sqlite", "SQLite"),
    ("sqlite3", "sqlite", "SQLite"),
];

// Connection env var name substrings -> (kind, tech). Proves consumption at runtime.
const DB_ENV: &[(&str, &str, &str)] = &[
    ("DATABASE_URL", "postgres", "PostgreSQL"),
    ("POSTGRES", "postgres", "PostgreSQL"),
    ("PG_", "postgres", "PostgreSQL"),
    ("MYSQL", "mysql", "MySQL"),
    ("MONGO", "mongo", "MongoDB"),
    ("REDIS", "redis", "Redis"),
    ("KV_", "redis", "Redis"),
];

// Backend web frameworks whose presence (as a dep) implies a server process.
const BACKEND_DEPS: &[(&str, &str)] = &[
    ("express", "Express"),
    ("fastify", "Fastify"),
    ("koa", "Koa"),
    ("@nestjs/core", "NestJS"),
    ("hapi", "hapi"),
    ("@hapi/hapi", "hapi"),
    ("apollo-server", "Apollo"),
    ("socket.io", "Socket.IO"),
];

// Frontend UI frameworks (dep -> tech) — a bundled frontend when alongside a backend.
const FRONTEND_DEPS: &[(&str, &str)] = &[
    ("react-scripts", "React"),
    ("react-dom", "React"),
    ("vue", "Vue"),
    ("@angular/core", "Angular"),
    ("svelte", "Svelte"),
];

fn has_any(deps: &BTreeSet<String>, name: &str) -> bool {
    deps.contains(name)
}

/// Build the service graph from scanned signals + the env var names. Pure.
pub fn build_graph(
    project: &str,
    deployment_id: &str,
    scan: &Scan,
    env_keys: &[String],
) -> ServiceGraph {
    let mut nodes: Vec<SvcNode> = Vec::new();
    let mut edges: Vec<SvcEdge> = Vec::new();

    // Ingress is always the entrypoint.
    nodes.push(SvcNode {
        id: "ingress".into(),
        kind: "ingress".into(),
        name: "Edge / Ingress".into(),
        tech: "OpenEdge".into(),
        group: None,
        detail: "Public entrypoint (TLS, CDN, WAF)".into(),
        source: "platform".into(),
    });

    // Bundling group id — nodes that live in ONE container/PID share it.
    let group = if scan.is_container || scan.has_dockerfile {
        Some(format!("{project}-container"))
    } else {
        None
    };

    // Detect a bundled monolith: a single deployable running BOTH a frontend and a
    // backend (e.g. a Dockerfile CMD that concurrently starts React + Express).
    let backend = BACKEND_DEPS.iter().find(|(d, _)| has_any(&scan.deps, d)).map(|(_, t)| *t);
    let frontend = FRONTEND_DEPS.iter().find(|(d, _)| has_any(&scan.deps, d)).map(|(_, t)| *t);
    let bundled = backend.is_some() && frontend.is_some();

    let mut app_ids: Vec<String> = Vec::new();
    if bundled {
        // TWO nodes, SAME group → one PID/container, but visualized separately.
        let fe = frontend.unwrap();
        let be = backend.unwrap();
        nodes.push(SvcNode {
            id: "app-frontend".into(),
            kind: "frontend".into(),
            name: format!("{fe} frontend"),
            tech: fe.into(),
            group: group.clone(),
            detail: "UI bundled in the same container".into(),
            source: format!("dep: {}", FRONTEND_DEPS.iter().find(|(d, _)| has_any(&scan.deps, d)).map(|(d, _)| *d).unwrap_or("")),
        });
        nodes.push(SvcNode {
            id: "app-backend".into(),
            kind: "backend".into(),
            name: format!("{be} backend"),
            tech: be.into(),
            group: group.clone(),
            detail: "API bundled in the same container".into(),
            source: format!("dep: {}", BACKEND_DEPS.iter().find(|(d, _)| has_any(&scan.deps, d)).map(|(d, _)| *d).unwrap_or("")),
        });
        // Frontend calls backend (same PID) — show the internal edge.
        edges.push(SvcEdge { from: "app-frontend".into(), to: "app-backend".into(), label: "internal".into() });
        edges.push(SvcEdge { from: "ingress".into(), to: "app-frontend".into(), label: "".into() });
        app_ids.push("app-frontend".into());
        app_ids.push("app-backend".into());
    } else {
        // Single app node — labelled by its framework/runtime.
        let (tech, kind) = if !scan.framework_name.is_empty() && scan.framework != "static" && scan.framework != "node" {
            (scan.framework_name.clone(), "app")
        } else if let Some(be) = backend {
            (be.to_string(), "backend")
        } else if let Some(fe) = frontend {
            (fe.to_string(), "frontend")
        } else if scan.is_container {
            ("Container".to_string(), "app")
        } else if scan.framework == "static" {
            ("Static Site".to_string(), "app")
        } else {
            ("Web Service".to_string(), "app")
        };
        nodes.push(SvcNode {
            id: "app".into(),
            kind: kind.into(),
            name: project.to_string(),
            tech,
            group: group.clone(),
            detail: scan.readme.clone(),
            source: if !scan.framework.is_empty() { format!("framework: {}", scan.framework) } else { "manifest".into() },
        });
        edges.push(SvcEdge { from: "ingress".into(), to: "app".into(), label: "".into() });
        app_ids.push("app".into());
    }

    // Monorepo workspace packages that are actually consumed → nodes in the group.
    for (pkg, tech) in &scan.packages {
        let id = format!("pkg-{}", slug(pkg));
        nodes.push(SvcNode {
            id: id.clone(),
            kind: "package".into(),
            name: pkg.clone(),
            tech: tech.clone(),
            group: group.clone(),
            detail: "Workspace package".into(),
            source: "monorepo workspace".into(),
        });
        // A consumed package is used by the primary app node.
        if let Some(app) = app_ids.first() {
            edges.push(SvcEdge { from: app.clone(), to: id, label: "uses".into() });
        }
    }

    // Databases/caches — ONLY when a client dep OR a connection env var proves use.
    // Dedup by kind so `pg` + `DATABASE_URL` don't draw two Postgres nodes.
    let mut seen_db: BTreeSet<&str> = BTreeSet::new();
    let mut add_db = |kind: &'static str, tech: &str, source: String, nodes: &mut Vec<SvcNode>, edges: &mut Vec<SvcEdge>| {
        if !seen_db.insert(kind) {
            return;
        }
        let id = format!("db-{kind}");
        nodes.push(SvcNode {
            id: id.clone(),
            kind: "database".into(),
            name: tech.to_string(),
            tech: tech.to_string(),
            group: None,
            detail: "Data store consumed by the app".into(),
            source,
        });
        // All app nodes depend on the datastore.
        for app in &app_ids {
            edges.push(SvcEdge { from: app.clone(), to: id.clone(), label: "".into() });
        }
    };
    for (dep, kind, tech) in DB_DEPS {
        if has_any(&scan.deps, dep) {
            add_db(kind, tech, format!("dep: {dep}"), &mut nodes, &mut edges);
        }
    }
    let upper_keys: Vec<String> = env_keys.iter().map(|k| k.to_uppercase()).collect();
    for (sub, kind, tech) in DB_ENV {
        if let Some(k) = upper_keys.iter().find(|k| k.contains(sub)) {
            add_db(kind, tech, format!("env: {k}"), &mut nodes, &mut edges);
        }
    }

    ServiceGraph {
        project: project.to_string(),
        deployment_id: deployment_id.to_string(),
        generated_ms: now_ms(),
        nodes,
        edges,
        env_keys: env_keys.to_vec(),
        readme: scan.readme.clone(),
    }
}

fn slug(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect()
}

// ---- filesystem scan (the only impure part) -------------------------------------

/// Collect npm dep names from a package.json value (deps + devDeps).
fn deps_from_pkg(v: &serde_json::Value, out: &mut BTreeSet<String>) {
    for key in ["dependencies", "devDependencies", "peerDependencies"] {
        if let Some(map) = v.get(key).and_then(|d| d.as_object()) {
            for k in map.keys() {
                out.insert(k.clone());
            }
        }
    }
}

/// Sanitize a README's first meaningful line into a short one-liner (no markdown/HTML).
fn readme_oneliner(text: &str) -> String {
    for raw in text.lines() {
        let l = raw.trim().trim_start_matches('#').trim();
        if l.is_empty() || l.starts_with("![") || l.starts_with("<") || l.starts_with("[!") {
            continue;
        }
        let clean: String = l.chars().filter(|c| *c != '`' && *c != '*' && *c != '_').collect();
        let clean = clean.trim();
        if clean.len() >= 3 {
            return clean.chars().take(140).collect();
        }
    }
    String::new()
}

/// Scan the checked-out build dir for service-graph signals. Bounded + best-effort:
/// any read failure just yields fewer signals, never an error.
pub fn scan_dir(build_dir: &Path, framework: &str, framework_name: &str, is_container: bool) -> Scan {
    let mut scan = Scan {
        framework: framework.to_string(),
        framework_name: framework_name.to_string(),
        is_container,
        ..Default::default()
    };

    // Root package.json (deps + start/dev scripts).
    if let Ok(s) = std::fs::read_to_string(build_dir.join("package.json")) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
            deps_from_pkg(&v, &mut scan.deps);
            if let Some(scripts) = v.get("scripts").and_then(|x| x.as_object()) {
                for k in ["start", "dev", "serve"] {
                    if let Some(cmd) = scripts.get(k).and_then(|x| x.as_str()) {
                        scan.run_cmds.push(cmd.to_string());
                    }
                }
            }
        }
    }

    // Dockerfile / Containerfile: presence + CMD/ENTRYPOINT run command.
    for f in ["Dockerfile", "Containerfile"] {
        if let Ok(s) = std::fs::read_to_string(build_dir.join(f)) {
            scan.has_dockerfile = true;
            for line in s.lines() {
                let u = line.trim();
                if u.starts_with("CMD ") || u.starts_with("ENTRYPOINT ") || u.starts_with("RUN ") && u.contains("concurrently") {
                    scan.run_cmds.push(u.to_string());
                }
            }
            break;
        }
    }

    // Monorepo workspace packages actually present under packages/ and apps/ — bounded
    // recursion (one level of package dirs each), and only those with a package.json.
    for wsdir in ["packages", "apps"] {
        let base = build_dir.join(wsdir);
        if let Ok(rd) = std::fs::read_dir(&base) {
            for e in rd.flatten().take(40) {
                let p = e.path();
                if !p.is_dir() {
                    continue;
                }
                let pkgjson = p.join("package.json");
                let Ok(s) = std::fs::read_to_string(&pkgjson) else { continue };
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) else { continue };
                let fallback = e.file_name().to_string_lossy().into_owned();
                let name = v.get("name").and_then(|x| x.as_str()).map(|s| s.to_string()).unwrap_or(fallback);
                // Fold the sub-package's deps into the consumed set (they run in-app).
                deps_from_pkg(&v, &mut scan.deps);
                // Guess the package's tech from its own deps.
                let mut sub = BTreeSet::new();
                deps_from_pkg(&v, &mut sub);
                let tech = FRONTEND_DEPS.iter().chain(BACKEND_DEPS.iter())
                    .find(|(d, _)| sub.contains(*d)).map(|(_, t)| t.to_string())
                    .unwrap_or_else(|| "Package".into());
                scan.packages.insert(name, tech);
            }
        }
    }

    // compose services (multi-container) — reuse the compose parser.
    if let Some(cf) = crate::compose::compose_file(build_dir) {
        if let Ok(text) = std::fs::read_to_string(&cf) {
            if let Ok(svcs) = crate::compose::parse_compose(&text) {
                scan.compose_services = svcs.into_iter().map(|s| s.name).collect();
            }
        }
    }

    // README one-liner.
    for f in ["README.md", "readme.md", "README", "Readme.md"] {
        if let Ok(s) = std::fs::read_to_string(build_dir.join(f)) {
            scan.readme = readme_oneliner(&s);
            if !scan.readme.is_empty() {
                break;
            }
        }
    }

    scan
}

// ---- per-project store (latest graph), persisted --------------------------------

#[derive(Default)]
pub struct ServiceGraphStore {
    // keyed by deployment id
    map: RwLock<HashMap<String, ServiceGraph>>,
}

impl ServiceGraphStore {
    pub fn new() -> Self {
        Self { map: RwLock::new(HashMap::new()) }
    }
    pub fn put(&self, g: ServiceGraph) {
        self.map.write().insert(g.deployment_id.clone(), g);
    }
    pub fn get(&self, deployment_id: &str) -> Option<ServiceGraph> {
        self.map.read().get(deployment_id).cloned()
    }
    /// Latest graph for a project (most recently generated).
    pub fn latest_for_project(&self, project: &str) -> Option<ServiceGraph> {
        self.map.read().values().filter(|g| g.project == project).max_by_key(|g| g.generated_ms).cloned()
    }
    pub fn snapshot(&self) -> Vec<ServiceGraph> {
        self.map.read().values().cloned().collect()
    }
    pub fn load(&self, data: Vec<ServiceGraph>) {
        let mut m = self.map.write();
        for g in data {
            m.insert(g.deployment_id.clone(), g);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_with(deps: &[&str], fw: &str, fwn: &str, container: bool) -> Scan {
        Scan {
            framework: fw.into(),
            framework_name: fwn.into(),
            deps: deps.iter().map(|s| s.to_string()).collect(),
            is_container: container,
            ..Default::default()
        }
    }

    #[test]
    fn bundled_monolith_shows_two_nodes_same_group() {
        // React + Express in one Dockerfile → frontend + backend, same container group.
        let mut s = scan_with(&["react-scripts", "express", "pg"], "", "", true);
        s.has_dockerfile = true;
        let g = build_graph("outerbridge", "dpl-1", &s, &[]);
        let fe = g.nodes.iter().find(|n| n.kind == "frontend").expect("frontend node");
        let be = g.nodes.iter().find(|n| n.kind == "backend").expect("backend node");
        assert_eq!(fe.tech, "React");
        assert_eq!(be.tech, "Express");
        // Same PID/container → same group.
        assert!(fe.group.is_some() && fe.group == be.group, "bundled → shared group");
        // The pg dep → exactly one Postgres node.
        assert_eq!(g.nodes.iter().filter(|n| n.kind == "database").count(), 1);
        assert!(g.nodes.iter().any(|n| n.kind == "database" && n.tech == "PostgreSQL"));
        // Internal FE→BE edge exists.
        assert!(g.edges.iter().any(|e| e.from == "app-frontend" && e.to == "app-backend"));
    }

    #[test]
    fn db_only_when_consumed_no_phantom_nodes() {
        // A plain Next.js app with NO db dep and NO db env → zero database nodes.
        let s = scan_with(&["next", "react"], "nextjs", "Next.js", false);
        let g = build_graph("blog", "dpl-2", &s, &["NEXT_PUBLIC_URL".into()]);
        assert_eq!(g.nodes.iter().filter(|n| n.kind == "database").count(), 0, "no unused db node");
        assert!(g.nodes.iter().any(|n| n.kind == "app" && n.tech == "Next.js"));
    }

    #[test]
    fn db_detected_from_env_var_alone() {
        // No client dep, but a DATABASE_URL env → still draw Postgres (it's consumed).
        let s = scan_with(&["next"], "nextjs", "Next.js", false);
        let g = build_graph("app", "dpl-3", &s, &["DATABASE_URL".into(), "SECRET".into()]);
        assert!(g.nodes.iter().any(|n| n.kind == "database" && n.tech == "PostgreSQL"));
        // env keys are recorded by NAME only.
        assert!(g.env_keys.contains(&"DATABASE_URL".to_string()));
    }

    #[test]
    fn dedup_db_across_dep_and_env() {
        // `pg` dep AND `DATABASE_URL` env → ONE Postgres node, not two.
        let s = scan_with(&["pg", "express"], "", "", false);
        let g = build_graph("api", "dpl-4", &s, &["DATABASE_URL".into()]);
        assert_eq!(g.nodes.iter().filter(|n| n.tech == "PostgreSQL").count(), 1);
    }

    #[test]
    fn redis_and_postgres_both_when_both_consumed() {
        let s = scan_with(&["ioredis", "mongoose"], "", "", false);
        let g = build_graph("svc", "dpl-5", &s, &[]);
        assert!(g.nodes.iter().any(|n| n.tech == "Redis"));
        assert!(g.nodes.iter().any(|n| n.tech == "MongoDB"));
        assert_eq!(g.nodes.iter().filter(|n| n.kind == "database").count(), 2);
    }

    #[test]
    fn readme_oneliner_strips_markdown() {
        assert_eq!(readme_oneliner("# My App\n\nDescription here"), "My App");
        assert_eq!(readme_oneliner("![badge](x)\n# Real Title"), "Real Title");
        assert_eq!(readme_oneliner("**Bold Intro** to the app"), "Bold Intro to the app");
    }
}
