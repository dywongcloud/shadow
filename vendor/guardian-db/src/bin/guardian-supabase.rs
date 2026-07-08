//! The `guardian-supabase` gateway binary.
//!
//! Serves a Supabase-compatible HTTP surface (Kong-shaped `/rest/v1`,
//! `/auth/v1`, ...) over the GuardianDB SQL engine. By default it binds
//! `127.0.0.1:54321` (Supabase's local port) using an in-memory relational
//! store; pass `--path` to back it with a persistent, Iroh-replicated GuardianDB
//! node.
//!
//! On startup it prints `SUPABASE_URL`, `ANON_KEY` and `SERVICE_ROLE_KEY`
//! (and, when generated, the `JWT_SECRET`) so `supabase-js` can be pointed at
//! it directly:
//!
//! ```ts
//! import { createClient } from "@supabase/supabase-js";
//! const supabase = createClient("http://127.0.0.1:54321", ANON_KEY);
//! await supabase.from("todos").select("*");
//! ```

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::net::TcpListener;

use guardian_db::guardian::GuardianDB;
use guardian_db::guardian::core::NewGuardianDBOptions;
use guardian_db::p2p::network::client::IrohClient;
use guardian_db::p2p::network::config::ClientConfig;
use guardian_db::sql::MemoryStorage;
use guardian_db::sql::engine::Database;
use guardian_db::sql::{RelationalStorage, open_sql};
use guardian_db::supabase::project::{ProjectKeys, generate_jwt_secret};
use guardian_db::supabase::{AppState, ServiceConfig, SupabaseCompatProject, build_router};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let mut addr = "127.0.0.1:54321".to_string();
    let mut database = "app".to_string();
    let mut jwt_secret: Option<String> = None;
    let mut data_path: Option<String> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--addr" | "-a" => addr = args.next().unwrap_or(addr),
            "--database" | "-d" => database = args.next().unwrap_or(database),
            "--jwt-secret" => jwt_secret = args.next(),
            "--path" | "-p" => data_path = args.next(),
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            other => eprintln!("ignoring unknown argument: {other}"),
        }
    }

    // Derive the project keys. A generated secret is printed once on startup.
    let (secret, generated) = match jwt_secret {
        Some(s) => (s, false),
        None => (generate_jwt_secret(), true),
    };
    let keys = ProjectKeys::from_secret(&secret, Utc::now().timestamp())?;
    let api_url = format!("http://{addr}");
    let anon_key = keys.anon_key.clone();
    let service_role_key = keys.service_role_key.clone();
    let project = SupabaseCompatProject::shell(&database, &api_url, keys, Utc::now());
    let config = ServiceConfig::default();

    print_banner(
        &api_url,
        &anon_key,
        &service_role_key,
        generated.then_some(secret.as_str()),
    );

    match data_path {
        Some(path) => {
            // Persistent, Iroh-backed GuardianDB node.
            let client = IrohClient::new(ClientConfig::development().with_data_path(&path)).await?;
            let node_id = client.id().await?.id;
            let db = GuardianDB::new(
                client,
                Some(NewGuardianDBOptions {
                    directory: Some(format!("{path}/guardian").into()),
                    ..Default::default()
                }),
            )
            .await?;
            let database_sql = open_sql(&db, &database).await?;

            // Keep the local relational view fresh as peers replicate in.
            let storage = database_sql.storage().clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    let _ = storage.refresh().await;
                }
            });

            println!("  storage : GuardianDB (Iroh, replicated) under {path}");
            println!("  node id : {node_id}   (share with peers to replicate)");
            serve(database_sql, project, config, &addr).await?;
        }
        None => {
            println!("  storage : in-memory (non-persistent) — pass --path for a replicated node");
            let database_sql = Arc::new(Database::new(Arc::new(MemoryStorage::new()), database));
            serve(database_sql, project, config, &addr).await?;
        }
    }
    Ok(())
}

async fn serve<S: RelationalStorage + 'static>(
    db: Arc<Database<S>>,
    project: SupabaseCompatProject,
    config: ServiceConfig,
    addr: &str,
) -> std::io::Result<()> {
    let state = AppState::new(db, project, config);
    let app = build_router(state);
    let listener = TcpListener::bind(addr).await?;
    println!("\nguardian-supabase listening on http://{addr}\n");
    axum::serve(listener, app.into_make_service()).await
}

fn print_banner(api_url: &str, anon_key: &str, service_role_key: &str, secret: Option<&str>) {
    println!("guardian-supabase — Supabase-compatible gateway for GuardianDB\n");
    println!("  SUPABASE_URL      : {api_url}");
    println!("  ANON_KEY          : {anon_key}");
    println!("  SERVICE_ROLE_KEY  : {service_role_key}");
    if let Some(secret) = secret {
        println!("  JWT_SECRET        : {secret}   (generated — save this to reuse the keys)");
    }
}

fn print_help() {
    println!(
        "guardian-supabase — Supabase-compatible gateway for GuardianDB\n\n\
         Usage: guardian-supabase [--addr 127.0.0.1:54321] [--database app] \
         [--jwt-secret <secret>] [--path <dir>]\n\n\
         Without --path, an in-memory store is used (great for development).\n\
         With --path, a persistent Iroh-replicated GuardianDB node backs the gateway.\n\n\
         Point supabase-js at http://<addr> with the printed ANON_KEY."
    );
}
