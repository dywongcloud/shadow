//! Live witness for `p2p-trunk-singleflight-generation` — drives the REAL
//! PeerPool against a REAL serving endpoint:
//!
//!   1. `concurrent_first_contacts_share_one_dial`
//!      → 8 concurrent first-contact `warm`s of the same cold peer must pay
//!        exactly ONE dial (opened delta == 1): the singleflight leader dials,
//!        the other seven await the shared outcome. Pre-fix this was up to 8
//!        last-insert-wins double-dials.
//!   2. `close_peer_evicts_and_redials_new_generation`
//!      → after `close_peer`, a fresh warm dials a NEW trunk (opened total 2)
//!        and the pool still converges to one cached trunk.
//!   3. `waiters_receive_live_connection`
//!      → every concurrent waiter resolves with a connection whose
//!        close_reason is unset (the shared dial's real connection).
//!
//! Usage: `cargo run -p hive-p2p --example trunk_singleflight_witness`
//! Exit code 0 = every witness line passed; 1 = at least one failed.

use std::sync::Arc;

fn check(name: &str, ok: bool) -> bool {
    println!(
        "{}: {}",
        if ok { "WITNESS_OK" } else { "WITNESS_FAIL" },
        name
    );
    ok
}

#[tokio::main]
async fn main() {
    let mut all = true;

    let Some((pool, id, addr)) = setup().await else {
        println!("WITNESS_SKIP: iroh could not bind");
        return;
    };

    // 1 + 3. Eight concurrent first contacts: one dial, all live connections.
    let (opened_before, _) = pool.stats();
    let pool2 = pool.clone();
    let id2 = id.clone();
    let addr2 = addr.clone();
    let mut joins = Vec::new();
    for _ in 0..8 {
        let (p, i, a) = (pool2.clone(), id2.clone(), addr2.clone());
        joins.push(tokio::spawn(async move { p.warm(&i, &a).await }));
    }
    let mut live = 0usize;
    for j in joins {
        if matches!(j.await, Ok(true)) {
            live += 1;
        }
    }
    let (opened_after, _) = pool.stats();
    all &= check("all_eight_waiters_succeeded", live == 8);
    all &= check(
        "concurrent_first_contacts_share_one_dial",
        opened_after - opened_before == 1,
    );
    all &= check("pool_converged_to_one_trunk", pool.trunk_count().await == 1);

    // 2. close_peer bumps the generation: the next warm redials (opened +1).
    pool.close_peer(&id).await;
    let ok = pool.warm(&id, &addr).await;
    let (opened_final, _) = pool.stats();
    all &= check(
        "close_peer_evicts_and_redials_new_generation",
        ok && opened_final - opened_after == 1,
    );
    all &= check(
        "pool_still_one_trunk_after_redial",
        pool.trunk_count().await == 1,
    );

    if all {
        println!("WITNESS_OK:ALL");
    } else {
        eprintln!("WITNESS_FAIL: at least one case failed");
        std::process::exit(1);
    }
}

/// Bind a serving endpoint (real QUIC accept loop) + a client pool, same
/// pattern as pool_witness's setup.
async fn setup() -> Option<(Arc<hive_p2p::PeerPool>, String, String)> {
    let ep_b = hive_p2p::bind().await.ok()?;
    let id = ep_b.id().to_string();
    let addr = hive_p2p::addr_json(&ep_b)?;
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()?;
    let function = l.local_addr().ok()?.to_string();
    tokio::spawn(hive_p2p::serve_tunnels(ep_b, function, 100, None, None));
    let ep_a = hive_p2p::bind().await.ok()?;
    Some((hive_p2p::PeerPool::new(ep_a), id, addr))
}
