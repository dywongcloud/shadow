//! PostgreSQL gateway backed by a **real, replicated GuardianDB / Iroh node**.
//!
//! The default `guardian-pgwire` binary serves an in-memory relational store —
//! great for development and the TypeORM tests, but not persistent or
//! replicated. This example instead opens an **Iroh-backed GuardianDB document
//! store** and serves SQL over it, so every table you create through TypeORM,
//! `psql`, or node-postgres becomes an ordinary GuardianDB document that
//! persists locally and replicates to peers over Iroh (Willow range
//! reconciliation, LWW CRDT) — the local-first / P2P model, behind a plain
//! PostgreSQL wire endpoint.
//!
//! ## Run it
//!
//! ```bash
//! cargo run --features pgwire --example postgres_iroh_gateway
//! #   options: -- --addr 127.0.0.1:15432 --database app --path ./guardian_pg_data
//! ```
//!
//! ## Point TypeORM at it (no GuardianDB-specific client code)
//!
//! ```ts
//! import { DataSource } from "typeorm";
//! const ds = new DataSource({
//!   type: "postgres", host: "127.0.0.1", port: 15432,
//!   username: "guardian", password: "guardian", database: "app",
//!   synchronize: true, entities: [User, Post, Org],
//! });
//! await ds.initialize();   // schema sync, migrations, repositories, transactions
//! ```
//!
//! Or with `psql`:
//!
//! ```bash
//! psql 'postgres://guardian:guardian@127.0.0.1:15432/app'
//! ```
//!
//! ## Replication (two nodes)
//!
//! Start this gateway on machine A; it prints its Iroh node id. Start a second
//! instance on machine B with a different `--addr`/`--path`. Peers on the same
//! LAN auto-discover via mDNS (and globally via the n0 discovery service), so
//! rows written through TypeORM on A converge to B's gateway and vice-versa. A
//! background refresh loop below re-syncs the local relational view so a `SELECT`
//! observes rows a peer wrote.

use std::time::Duration;

use guardian_db::guardian::GuardianDB;
use guardian_db::guardian::core::NewGuardianDBOptions;
use guardian_db::p2p::network::client::IrohClient;
use guardian_db::p2p::network::config::ClientConfig;
use guardian_db::pgwire::{DEFAULT_ADDR, serve};
use guardian_db::sql::open_sql;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    // --- tiny arg parser: --addr / --database / --path -----------------------
    let mut addr = DEFAULT_ADDR.to_string();
    let mut database = "app".to_string();
    let mut data_path = "./guardian_pg_data".to_string();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--addr" | "-a" => addr = args.next().unwrap_or(addr),
            "--database" | "-d" => database = args.next().unwrap_or(database),
            "--path" | "-p" => data_path = args.next().unwrap_or(data_path),
            "--help" | "-h" => {
                println!(
                    "postgres_iroh_gateway — Iroh-backed PostgreSQL gateway\n\n\
                     Usage: --addr 127.0.0.1:15432 --database app --path ./guardian_pg_data"
                );
                return Ok(());
            }
            other => eprintln!("ignoring unknown argument: {other}"),
        }
    }

    // 1. Start a local Iroh node, persisting its state under `--path`.
    let client = IrohClient::new(ClientConfig::development().with_data_path(&data_path)).await?;
    let node_id = client.id().await?.id;

    // 2. Open a GuardianDB whose documents live on that Iroh node.
    let db = GuardianDB::new(
        client,
        Some(NewGuardianDBOptions {
            directory: Some(format!("{data_path}/guardian").into()),
            ..Default::default()
        }),
    )
    .await?;

    // 3. Open a relational SQL database backed by a replicated GuardianDB
    //    document store. Tables and rows created over the wire are ordinary
    //    GuardianDB documents and replicate exactly like any other.
    let database_sql = open_sql(&db, &database).await?;

    // 4. Keep the relational view fresh: the engine reads the document store's
    //    synchronous local index, which updates on local writes and on re-sync
    //    but not automatically when documents arrive from peers. Refresh
    //    periodically so a SELECT observes rows written by peers.
    let storage = database_sql.storage().clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let _ = storage.refresh().await;
        }
    });

    // 5. Serve PostgreSQL on `addr` until cancelled.
    println!("GuardianDB (Iroh-backed) PostgreSQL gateway listening on {addr}");
    println!("  node id : {node_id}   (share with peers to replicate)");
    println!("  database: {database}   (persisted under {data_path})");
    println!("  connect : psql 'postgres://guardian:guardian@{addr}/{database}'");
    serve(&addr, database_sql, "guardian".to_string()).await?;
    Ok(())
}
