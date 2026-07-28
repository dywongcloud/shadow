//! shadw — the official command-line interface for the shadw platform.
//!
//! Wraps the platform REST API using the API key issued in the dashboard
//! (Settings → API Keys). Run `shadw auth login --token shadw_…` once, then every
//! command is scoped to that key's team.

mod client;
mod output;

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use client::{Client, Config};
use output::Out;
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Parser)]
#[command(name = "shadw", version, about = "Command-line interface for the shadw peer-to-peer cloud")]
struct Cli {
    /// API base URL (env: SHADW_API_URL). Default http://127.0.0.1:8786.
    #[arg(long, global = true)]
    api: Option<String>,
    /// API key — `shadw_…` from the dashboard (env: SHADW_TOKEN).
    #[arg(long, global = true)]
    token: Option<String>,
    /// Team slug to scope to (env: SHADW_TEAM). Usually inferred from the key.
    #[arg(long, global = true)]
    team: Option<String>,
    /// Output raw JSON (scriptable / pipe to jq).
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Authenticate the CLI with your API key (saves ~/.shadw/config.json).
    #[command(subcommand)]
    Auth(AuthCmd),
    /// Deploy a Git repository (clone → build → deploy).
    Deploy(DeployArgs),
    /// Inspect a build (status + streamed logs).
    #[command(subcommand)]
    Build(BuildCmd),
    /// Deployments — list, promote (rollback), delete, inspect.
    #[command(subcommand, visible_alias = "deps")]
    Deployments(DepCmd),
    /// Projects — settings, env vars, redeploy, domains, delete.
    #[command(subcommand, visible_alias = "proj")]
    Projects(ProjCmd),
    /// Domains & DNS — records, migration (scan/import), nameservers, SSL.
    #[command(subcommand, visible_alias = "dom")]
    Domains(DomCmd),
    /// Outbound webhooks.
    #[command(subcommand, visible_alias = "hooks")]
    Webhooks(HookCmd),
    /// Teams & members.
    #[command(subcommand)]
    Teams(TeamCmd),
    /// API keys.
    #[command(subcommand, visible_alias = "apikeys")]
    Keys(KeyCmd),
    /// Databases.
    #[command(subcommand, visible_alias = "db")]
    Databases(DbCmd),
    /// Cron jobs.
    #[command(subcommand)]
    Cron(CronCmd),
    /// Workflows.
    #[command(subcommand, visible_alias = "wf")]
    Workflows(WfCmd),
    /// Observability — overview, logs, resources, metrics, regions, nodes.
    #[command(subcommand, visible_alias = "obs")]
    Observability(ObsCmd),
    /// Generic API passthrough for any endpoint (escape hatch).
    Api(ApiArgs),
}

// ---- auth ----
#[derive(Subcommand)]
enum AuthCmd {
    /// Save your API key + API URL and verify the connection.
    Login {
        #[arg(long)]
        token: Option<String>,
        #[arg(long)]
        api: Option<String>,
        #[arg(long)]
        team: Option<String>,
        /// Account email (mint-mode: recorded in minted tokens so the backend
        /// can derive platform-operator status from its own owner config).
        #[arg(long)]
        email: Option<String>,
        /// HIVE_INTERNAL_TOKEN for mint-mode auto-login: the CLI then mints and
        /// auto-refreshes a platform JWT, so mutations work on an enforced
        /// platform without pasting short-lived tokens.
        #[arg(long)]
        internal_token: Option<String>,
    },
    /// Show the active config + verify the token works.
    Whoami,
    /// Mint a short-lived bearer token (POST /v1/token).
    Token {
        #[arg(long, default_value = "cli")]
        sub: String,
        #[arg(long, default_value = "personal")]
        tenant: String,
        #[arg(long, default_value = "owner")]
        role: String,
    },
    /// Remove the saved config.
    Logout,
}

// ---- deploy / build ----
#[derive(Parser)]
struct DeployArgs {
    /// Git repository URL to deploy.
    repo_url: String,
    #[arg(long)]
    branch: Option<String>,
    #[arg(long)]
    project: Option<String>,
    /// Monorepo subdirectory to build.
    #[arg(long)]
    root_dir: Option<String>,
    /// Environment: production | preview (default: classify by branch).
    #[arg(long)]
    target: Option<String>,
    /// Skip the dependency build cache.
    #[arg(long)]
    no_cache: bool,
    /// Env var to inject (repeatable): -e KEY=VALUE
    #[arg(short = 'e', long = "env", value_name = "KEY=VALUE")]
    env: Vec<String>,
    /// Stream the build logs until it finishes.
    #[arg(long)]
    follow: bool,
}

#[derive(Subcommand)]
enum BuildCmd {
    /// Get a build's status + logs.
    Get { id: String },
    /// Stream a build's logs until it finishes.
    Logs {
        id: String,
        #[arg(long)]
        follow: bool,
    },
}

// ---- deployments ----
#[derive(Subcommand)]
enum DepCmd {
    /// List deployments for the active team.
    List,
    /// Promote a deployment to production (instant rollback).
    Promote { id: String },
    /// Delete a deployment.
    Delete { id: String },
    /// Show a deployment's functions + static assets.
    Resources { id: String },
}

// ---- projects ----
#[derive(Subcommand)]
enum ProjCmd {
    /// List projects (with git + production deployment).
    List,
    /// Show a project's settings.
    Settings { project: String },
    /// Rebuild a project's latest source.
    Redeploy {
        project: String,
        #[arg(long)]
        target: Option<String>,
        #[arg(long)]
        no_cache: bool,
    },
    /// Delete a project and all its deployments.
    Delete { project: String },
    /// Attach a custom domain to a project.
    Domain { project: String, domain: String },
    /// Environment variables.
    #[command(subcommand)]
    Env(EnvCmd),
}

#[derive(Subcommand)]
enum EnvCmd {
    /// List a project's env vars.
    List { project: String },
    /// Set an env var (KEY VALUE).
    Set {
        project: String,
        key: String,
        value: String,
        #[arg(long, default_value = "all")]
        target: String,
        #[arg(long)]
        sensitive: bool,
    },
    /// Remove an env var.
    Rm { project: String, key: String },
}

// ---- domains / dns ----
#[derive(Subcommand)]
enum DomCmd {
    /// List custom domains.
    List,
    /// Show a domain (records, nameservers, SSL).
    Get { domain: String },
    /// Detect the domain's live public DNS records (migration helper).
    Scan { domain: String },
    /// Import records from a BIND zone file.
    Import {
        domain: String,
        #[arg(long, value_name = "FILE")]
        zone: String,
    },
    /// Set the domain's nameservers.
    Nameservers { domain: String, nameservers: Vec<String> },
    /// Reissue the managed SSL certificate.
    SslRenew { domain: String },
    /// DNS records.
    #[command(subcommand)]
    Record(RecordCmd),
}

#[derive(Subcommand)]
enum RecordCmd {
    /// List a domain's DNS records.
    List { domain: String },
    /// Add a DNS record.
    Add {
        domain: String,
        #[arg(long = "type", value_name = "TYPE")]
        kind: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        value: String,
        #[arg(long, default_value_t = 3600)]
        ttl: u32,
        #[arg(long)]
        priority: Option<u32>,
    },
    /// Update a DNS record by id.
    Update {
        domain: String,
        id: String,
        #[arg(long = "type", value_name = "TYPE")]
        kind: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        value: String,
        #[arg(long, default_value_t = 3600)]
        ttl: u32,
    },
    /// Delete a DNS record by id.
    Rm { domain: String, id: String },
}

// ---- webhooks ----
#[derive(Subcommand)]
enum HookCmd {
    /// List webhooks.
    List,
    /// List subscribable event types.
    Events,
    /// Recent delivery attempts.
    Deliveries,
    /// Create a webhook (optionally project-scoped).
    Create {
        url: String,
        #[arg(long)]
        project: Option<String>,
        /// Comma-separated event names (default: all).
        #[arg(long)]
        events: Option<String>,
    },
    /// Delete a webhook.
    Delete { id: String },
}

// ---- teams ----
#[derive(Subcommand)]
enum TeamCmd {
    List,
    Get { slug: String },
    /// Member management.
    #[command(subcommand)]
    Member(MemberCmd),
    /// Set a team's plan.
    Plan { slug: String, plan: String },
}

#[derive(Subcommand)]
enum MemberCmd {
    Add {
        slug: String,
        email: String,
        #[arg(long, default_value = "member")]
        role: String,
    },
    Rm { slug: String, email: String },
}

// ---- keys ----
#[derive(Subcommand)]
enum KeyCmd {
    List,
    Create {
        name: String,
        #[arg(long, default_value = "owner")]
        role: String,
    },
    Revoke { id: String },
}

// ---- databases ----
#[derive(Subcommand)]
enum DbCmd {
    List,
    Get { id: String },
    Create {
        name: String,
        #[arg(long, default_value = "postgres")]
        engine: String,
    },
    Delete { id: String },
    Credentials { id: String },
}

// ---- cron ----
#[derive(Subcommand)]
enum CronCmd {
    List,
    Add {
        name: String,
        schedule: String,
        url: String,
    },
    Delete { id: String },
}

// ---- workflows ----
#[derive(Subcommand)]
enum WfCmd {
    List,
    Runs,
    Run { id: String },
}

// ---- observability ----
#[derive(Subcommand)]
enum ObsCmd {
    Overview,
    Logs {
        #[arg(long, default_value_t = 100)]
        limit: u32,
    },
    Resources,
    Metrics,
    Regions,
    Nodes,
}

// ---- generic passthrough ----
#[derive(Parser)]
struct ApiArgs {
    /// HTTP method (GET, POST, PUT, DELETE).
    method: String,
    /// Path, e.g. /v1/overview
    path: String,
    /// Inline JSON body.
    #[arg(long)]
    data: Option<String>,
    /// Read JSON body from a file.
    #[arg(long)]
    data_file: Option<String>,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let out = Out { json: cli.json };
    let explicit_token = cli.token.is_some() || std::env::var("SHADW_TOKEN").is_ok() || std::env::var("SHADW_API_KEY").is_ok();
    let mut c = Client::resolve(cli.api.clone(), cli.token.clone(), cli.team.clone());
    // Mint-mode auto-login (config `internal_token`): transparently swap in a
    // fresh platform JWT so mutations pass the enforced ingress. No-op for the
    // plain API-key flow or an explicit --token/env token.
    c.ensure_fresh_token(explicit_token).await;
    if let Err(e) = run(cli, &c, out).await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli, c: &Client, out: Out) -> Result<()> {
    match cli.cmd {
        Cmd::Auth(a) => auth(a, c, out).await,
        Cmd::Deploy(a) => deploy(a, c, out).await,
        Cmd::Build(b) => match b {
            BuildCmd::Get { id } => {
                out.value(&c.get(&format!("/v1/builds/{id}")).await?);
                Ok(())
            }
            BuildCmd::Logs { id, follow } => follow_build(&id, follow, c, out).await,
        },
        Cmd::Deployments(d) => {
            match d {
                DepCmd::List => {
                    let v = c.get("/deployments").await?;
                    out.table(&v, &[("id", "ID"), ("project", "PROJECT"), ("target", "ENV"), ("state", "STATE"), ("alias", "URL"), ("git.branch", "BRANCH")]);
                }
                DepCmd::Promote { id } => out.value(&c.post(&format!("/v1/deployments/{id}/promote"), json!({})).await?),
                DepCmd::Delete { id } => out.value(&c.delete(&format!("/v1/deployments/{id}")).await?),
                DepCmd::Resources { id } => out.value(&c.get(&format!("/v1/deployments/{id}/resources")).await?),
            }
            Ok(())
        }
        Cmd::Projects(p) => projects(p, c, out).await,
        Cmd::Domains(d) => domains(d, c, out).await,
        Cmd::Webhooks(h) => webhooks(h, c, out).await,
        Cmd::Teams(t) => teams(t, c, out).await,
        Cmd::Keys(k) => keys(k, c, out).await,
        Cmd::Databases(d) => databases(d, c, out).await,
        Cmd::Cron(cr) => cron(cr, c, out).await,
        Cmd::Workflows(w) => workflows(w, c, out).await,
        Cmd::Observability(o) => observability(o, c, out).await,
        Cmd::Api(a) => api_passthrough(a, c, out).await,
    }
}

async fn auth(a: AuthCmd, c: &Client, out: Out) -> Result<()> {
    match a {
        AuthCmd::Login { token, api, team, email, internal_token } => {
            let prev = client::load_config();
            let cfg = Config {
                api: api.or_else(|| Some(c.api.clone())),
                token: token.or_else(|| c.token.clone()),
                team: team.or_else(|| c.team.clone()),
                email: email.or(prev.email),
                internal_token: internal_token.or(prev.internal_token),
                // Force a fresh mint under the possibly-changed identity.
                jwt: None,
                jwt_exp: None,
            };
            let mint_mode = cfg.internal_token.as_deref().unwrap_or("") != "";
            if !mint_mode && cfg.token.as_deref().unwrap_or("").is_empty() {
                return Err(anyhow!("no credentials provided — pass --token hive_… (dashboard: Settings → API Keys) or --internal-token for mint-mode auto-login"));
            }
            client::save_config(&cfg)?;
            // Verify with a freshly-resolved client (mint-mode mints here).
            let mut verify = Client::resolve(cfg.api.clone(), cfg.token.clone(), cfg.team.clone());
            verify.ensure_fresh_token(false).await;
            let who = verify.get("/v1/whoami").await?;
            let authed = who["authenticated"].as_bool().unwrap_or(false);
            if !authed {
                return Err(anyhow!("saved config, but /v1/whoami reports unauthenticated — check the token/internal token"));
            }
            println!(
                "Logged in to {} as {} (tenant: {}). Config saved to {}.",
                cfg.api.as_deref().unwrap_or(""),
                who["sub"].as_str().unwrap_or(cfg.email.as_deref().unwrap_or("?")),
                who["tenant"].as_str().unwrap_or("?"),
                client::config_path().display()
            );
            Ok(())
        }
        AuthCmd::Whoami => {
            let cfg = client::load_config();
            println!("API:   {}", c.api);
            println!("Token: {}", c.token.as_deref().map(mask).unwrap_or_else(|| "(none)".into()));
            if let Some(t) = &c.team {
                println!("Team:  {t}");
            }
            if let Some(e) = &cfg.email {
                println!("Email: {e}");
            }
            if cfg.internal_token.is_some() {
                println!("Auth:  mint-mode (auto-refreshed platform JWT)");
            }
            let who = c.get("/v1/whoami").await?;
            println!("Authenticated: {} (sub: {}, tenant: {})",
                who["authenticated"].as_bool().unwrap_or(false),
                who["sub"].as_str().unwrap_or("-"),
                who["tenant"].as_str().unwrap_or("-"));
            let auth = c.get("/v1/auth").await?;
            println!("Auth enforced: {}", auth["enforced"].as_bool().unwrap_or(false));
            Ok(())
        }
        AuthCmd::Token { sub, tenant, role } => {
            // Include the account email so the backend derives platform-operator
            // status from its own owner config (it never trusts a client claim).
            let email = client::load_config().email.unwrap_or_default();
            out.value(&c.post("/v1/token", json!({ "sub": sub, "tenant": tenant, "role": role, "email": email })).await?);
            Ok(())
        }
        AuthCmd::Logout => {
            let p = client::config_path();
            if p.exists() {
                std::fs::remove_file(&p)?;
                println!("Logged out — removed {}.", p.display());
            } else {
                println!("Not logged in.");
            }
            Ok(())
        }
    }
}

/// Build the POST /v1/git/deploy body from deploy args. Pure + testable: only
/// includes fields that were set, and parses `-e KEY=VALUE` pairs.
fn build_deploy_body(
    repo_url: &str,
    branch: &Option<String>,
    project: &Option<String>,
    root_dir: &Option<String>,
    target: &Option<String>,
    no_cache: bool,
    env_pairs: &[String],
) -> Result<Value> {
    let mut env = serde_json::Map::new();
    for kv in env_pairs {
        let (k, v) = kv.split_once('=').ok_or_else(|| anyhow!("invalid --env '{kv}', expected KEY=VALUE"))?;
        env.insert(k.to_string(), Value::String(v.to_string()));
    }
    let mut body = json!({ "repo_url": repo_url, "creator": "cli" });
    if let Some(b) = branch {
        body["branch"] = json!(b);
    }
    if let Some(p) = project {
        body["project"] = json!(p);
    }
    if let Some(r) = root_dir {
        body["root_dir"] = json!(r);
    }
    if let Some(t) = target {
        body["target"] = json!(t);
    }
    if no_cache {
        body["use_cache"] = json!(false);
    }
    if !env.is_empty() {
        body["env"] = Value::Object(env);
    }
    Ok(body)
}

async fn deploy(a: DeployArgs, c: &Client, out: Out) -> Result<()> {
    let body = build_deploy_body(&a.repo_url, &a.branch, &a.project, &a.root_dir, &a.target, a.no_cache, &a.env)?;
    let res = c.post("/v1/git/deploy", body).await?;
    let build_id = res["build_id"].as_str().unwrap_or("").to_string();
    if out.json {
        out.value(&res);
    } else {
        println!("Deploy started: build {} (project {})", build_id, res["project"].as_str().unwrap_or("?"));
        println!("Stream logs:  shadw build logs {build_id} --follow");
    }
    if a.follow && !build_id.is_empty() {
        println!();
        follow_build(&build_id, true, c, out).await?;
    }
    Ok(())
}

/// Poll a build and print new log lines until it reaches a terminal state.
async fn follow_build(id: &str, follow: bool, c: &Client, out: Out) -> Result<()> {
    let mut shown = 0usize;
    loop {
        let b = c.get(&format!("/v1/builds/{id}")).await?;
        if out.json && !follow {
            out.value(&b);
            return Ok(());
        }
        if let Some(lines) = b["lines"].as_array() {
            for line in lines.iter().skip(shown) {
                let text = line["line"].as_str().or_else(|| line.as_str()).unwrap_or("");
                println!("{text}");
            }
            shown = lines.len();
        }
        let state = b["state"].as_str().unwrap_or("");
        if !follow || state == "ready" || state == "error" {
            if !follow {
                out.value(&b);
            } else {
                println!("\nBuild {id} → {state}");
            }
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(1200)).await;
    }
}

async fn projects(p: ProjCmd, c: &Client, out: Out) -> Result<()> {
    match p {
        ProjCmd::List => {
            let v = c.get("/v1/gitops/projects").await?;
            out.table(&v, &[("project", "PROJECT"), ("root_dir", "ROOT"), ("git.branch", "BRANCH"), ("production.alias", "PRODUCTION URL")]);
        }
        ProjCmd::Settings { project } => out.value(&c.get(&format!("/v1/projects/{project}/settings")).await?),
        ProjCmd::Redeploy { project, target, no_cache } => {
            let mut body = json!({ "use_cache": !no_cache });
            if let Some(t) = target {
                body["target"] = json!(t);
            }
            out.value(&c.post(&format!("/v1/projects/{project}/redeploy"), body).await?);
        }
        ProjCmd::Delete { project } => out.value(&c.delete(&format!("/v1/projects/{project}")).await?),
        ProjCmd::Domain { project, domain } => {
            out.value(&c.post(&format!("/v1/projects/{project}/domains"), json!({ "domain": domain })).await?)
        }
        ProjCmd::Env(e) => match e {
            EnvCmd::List { project } => {
                let s = c.get(&format!("/v1/projects/{project}/settings")).await?;
                out.table(&s["env"], &[("key", "KEY"), ("target", "TARGET"), ("sensitive", "SENSITIVE")]);
            }
            EnvCmd::Set { project, key, value, target, sensitive } => out.value(
                &c.post(
                    &format!("/v1/projects/{project}/env"),
                    json!({ "key": key, "value": value, "target": target, "sensitive": sensitive }),
                )
                .await?,
            ),
            EnvCmd::Rm { project, key } => out.value(&c.delete(&format!("/v1/projects/{project}/env/{key}")).await?),
        },
    }
    Ok(())
}

async fn domains(d: DomCmd, c: &Client, out: Out) -> Result<()> {
    match d {
        DomCmd::List => {
            let v = c.get("/v1/domains").await?;
            out.table(&v, &[("domain", "DOMAIN"), ("project", "PROJECT")]);
        }
        DomCmd::Get { domain } => out.value(&c.get(&format!("/v1/domains/{domain}")).await?),
        DomCmd::Scan { domain } => out.value(&c.get(&format!("/v1/domains/{domain}/scan")).await?),
        DomCmd::Import { domain, zone } => {
            let text = std::fs::read_to_string(&zone).map_err(|e| anyhow!("reading {zone}: {e}"))?;
            out.value(&c.post(&format!("/v1/domains/{domain}/import"), json!({ "zone": text })).await?);
        }
        DomCmd::Nameservers { domain, nameservers } => {
            out.value(&c.put(&format!("/v1/domains/{domain}/nameservers"), json!({ "nameservers": nameservers })).await?)
        }
        DomCmd::SslRenew { domain } => out.value(&c.post(&format!("/v1/domains/{domain}/ssl/renew"), json!({})).await?),
        DomCmd::Record(r) => match r {
            RecordCmd::List { domain } => {
                let dv = c.get(&format!("/v1/domains/{domain}")).await?;
                let recs = dv.get("domain").and_then(|d| d.get("records")).cloned().unwrap_or(dv["records"].clone());
                out.table(&recs, &[("type", "TYPE"), ("name", "NAME"), ("value", "VALUE"), ("ttl", "TTL"), ("id", "ID")]);
            }
            RecordCmd::Add { domain, kind, name, value, ttl, priority } => {
                let mut body = json!({ "type": kind, "name": name, "value": value, "ttl": ttl });
                if let Some(p) = priority {
                    body["priority"] = json!(p);
                }
                out.value(&c.post(&format!("/v1/domains/{domain}/records"), body).await?);
            }
            RecordCmd::Update { domain, id, kind, name, value, ttl } => out.value(
                &c.put(&format!("/v1/domains/{domain}/records/{id}"), json!({ "type": kind, "name": name, "value": value, "ttl": ttl })).await?,
            ),
            RecordCmd::Rm { domain, id } => out.value(&c.delete(&format!("/v1/domains/{domain}/records/{id}")).await?),
        },
    }
    Ok(())
}

async fn webhooks(h: HookCmd, c: &Client, out: Out) -> Result<()> {
    match h {
        HookCmd::List => {
            let v = c.get("/v1/webhooks").await?;
            out.table(&v, &[("id", "ID"), ("project", "PROJECT"), ("url", "URL")]);
        }
        HookCmd::Events => out.value(&c.get("/v1/webhooks/events").await?),
        HookCmd::Deliveries => out.value(&c.get("/v1/webhooks/deliveries").await?),
        HookCmd::Create { url, project, events } => {
            let ev: Vec<String> = events.map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()).unwrap_or_default();
            let body = json!({ "url": url, "events": ev });
            let path = match project {
                Some(p) => format!("/v1/projects/{p}/webhooks"),
                None => "/v1/webhooks".into(),
            };
            out.value(&c.post(&path, body).await?);
        }
        HookCmd::Delete { id } => out.value(&c.delete(&format!("/v1/webhooks/{id}")).await?),
    }
    Ok(())
}

async fn teams(t: TeamCmd, c: &Client, out: Out) -> Result<()> {
    match t {
        TeamCmd::List => {
            let v = c.get("/v1/teams").await?;
            out.table(&v, &[("slug", "SLUG"), ("name", "NAME"), ("plan", "PLAN")]);
        }
        TeamCmd::Get { slug } => out.value(&c.get(&format!("/v1/teams/{slug}")).await?),
        TeamCmd::Member(m) => match m {
            MemberCmd::Add { slug, email, role } => {
                out.value(&c.post(&format!("/v1/teams/{slug}/members"), json!({ "email": email, "role": role })).await?)
            }
            MemberCmd::Rm { slug, email } => out.value(&c.delete(&format!("/v1/teams/{slug}/members/{email}")).await?),
        },
        TeamCmd::Plan { slug, plan } => out.value(&c.put(&format!("/v1/teams/{slug}/plan"), json!({ "plan": plan })).await?),
    }
    Ok(())
}

async fn keys(k: KeyCmd, c: &Client, out: Out) -> Result<()> {
    match k {
        KeyCmd::List => {
            let v = c.get("/v1/apikeys").await?;
            out.table(&v, &[("id", "ID"), ("name", "NAME"), ("prefix", "PREFIX"), ("team", "TEAM"), ("role", "ROLE")]);
        }
        KeyCmd::Create { name, role } => {
            let v = c.post("/v1/apikeys", json!({ "name": name, "role": role })).await?;
            if !out.json {
                if let Some(tok) = v["token"].as_str() {
                    println!("Created key '{}'. Save this token now — it is shown only once:\n\n  {}\n", name, tok);
                    return Ok(());
                }
            }
            out.value(&v);
        }
        KeyCmd::Revoke { id } => out.value(&c.delete(&format!("/v1/apikeys/{id}")).await?),
    }
    Ok(())
}

async fn databases(d: DbCmd, c: &Client, out: Out) -> Result<()> {
    match d {
        DbCmd::List => {
            let v = c.get("/v1/databases").await?;
            out.table(&v, &[("id", "ID"), ("name", "NAME"), ("engine", "ENGINE"), ("status", "STATUS")]);
        }
        DbCmd::Get { id } => out.value(&c.get(&format!("/v1/databases/{id}")).await?),
        DbCmd::Create { name, engine } => out.value(&c.post("/v1/databases", json!({ "name": name, "engine": engine })).await?),
        DbCmd::Delete { id } => out.value(&c.delete(&format!("/v1/databases/{id}")).await?),
        DbCmd::Credentials { id } => out.value(&c.get(&format!("/v1/databases/{id}/credentials")).await?),
    }
    Ok(())
}

async fn cron(cr: CronCmd, c: &Client, out: Out) -> Result<()> {
    match cr {
        CronCmd::List => {
            let v = c.get("/v1/cron").await?;
            out.table(&v, &[("id", "ID"), ("name", "NAME"), ("schedule", "SCHEDULE"), ("url", "URL")]);
        }
        CronCmd::Add { name, schedule, url } => {
            out.value(&c.post("/v1/cron", json!({ "name": name, "schedule": schedule, "url": url })).await?)
        }
        CronCmd::Delete { id } => out.value(&c.delete(&format!("/v1/cron/{id}")).await?),
    }
    Ok(())
}

async fn workflows(w: WfCmd, c: &Client, out: Out) -> Result<()> {
    match w {
        WfCmd::List => out.value(&c.get("/v1/workflows").await?),
        WfCmd::Runs => out.value(&c.get("/v1/workflows/runs").await?),
        WfCmd::Run { id } => out.value(&c.post(&format!("/v1/workflows/{id}/run"), json!({})).await?),
    }
    Ok(())
}

async fn observability(o: ObsCmd, c: &Client, out: Out) -> Result<()> {
    match o {
        ObsCmd::Overview => out.value(&c.get("/v1/overview").await?),
        ObsCmd::Logs { limit } => out.value(&c.get(&format!("/v1/logs?limit={limit}")).await?),
        ObsCmd::Resources => out.value(&c.get("/v1/resources").await?),
        ObsCmd::Metrics => out.value(&c.get("/v1/metrics").await?),
        ObsCmd::Regions => out.value(&c.get("/v1/regions").await?),
        ObsCmd::Nodes => {
            let v = c.get("/v1/nodes").await?;
            out.table(&v, &[("name", "NAME"), ("region", "REGION"), ("healthy", "HEALTHY"), ("public_url", "URL")]);
        }
    }
    Ok(())
}

async fn api_passthrough(a: ApiArgs, c: &Client, out: Out) -> Result<()> {
    let body = if let Some(f) = a.data_file {
        let s = std::fs::read_to_string(&f).map_err(|e| anyhow!("reading {f}: {e}"))?;
        Some(serde_json::from_str(&s).map_err(|e| anyhow!("invalid JSON in {f}: {e}"))?)
    } else if let Some(d) = a.data {
        Some(serde_json::from_str(&d).map_err(|e| anyhow!("invalid --data JSON: {e}"))?)
    } else {
        None
    };
    out.value(&c.request(&a.method, &a.path, body).await?);
    Ok(())
}

fn mask(token: &str) -> String {
    if token.len() <= 12 {
        return "****".into();
    }
    format!("{}…{}", &token[..10], &token[token.len() - 4..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deploy_body_minimal_only_repo_and_creator() {
        let b = build_deploy_body("https://github.com/a/b", &None, &None, &None, &None, false, &[]).unwrap();
        assert_eq!(b["repo_url"], "https://github.com/a/b");
        assert_eq!(b["creator"], "cli");
        // Optional fields are omitted when unset (so the server applies defaults).
        assert!(b.get("branch").is_none());
        assert!(b.get("project").is_none());
        assert!(b.get("target").is_none());
        assert!(b.get("use_cache").is_none());
        assert!(b.get("env").is_none());
    }

    #[test]
    fn deploy_body_full_with_env_and_no_cache() {
        let b = build_deploy_body(
            "file:///tmp/r",
            &Some("main".into()),
            &Some("proj".into()),
            &Some("examples/nextjs".into()),
            &Some("production".into()),
            true,
            &["FOO=bar".to_string(), "BAZ=qux=ok".to_string()],
        )
        .unwrap();
        assert_eq!(b["branch"], "main");
        assert_eq!(b["project"], "proj");
        assert_eq!(b["root_dir"], "examples/nextjs");
        assert_eq!(b["target"], "production");
        assert_eq!(b["use_cache"], false); // --no-cache → use_cache:false
        assert_eq!(b["env"]["FOO"], "bar");
        // Only the FIRST '=' splits, so values may contain '='.
        assert_eq!(b["env"]["BAZ"], "qux=ok");
    }

    #[test]
    fn deploy_body_rejects_malformed_env() {
        let err = build_deploy_body("r", &None, &None, &None, &None, false, &["NOEQUALS".to_string()]).unwrap_err();
        assert!(err.to_string().contains("KEY=VALUE"));
    }

    #[test]
    fn mask_hides_secret_but_shows_recognizable_ends() {
        let m = mask("hive_1234567890abcdefXYZ");
        assert!(m.starts_with("hive_12345"));
        assert!(m.ends_with("fXYZ"));
        assert!(!m.contains("67890abcde")); // middle is hidden
        // Short tokens fully masked.
        assert_eq!(mask("short"), "****");
    }
}
