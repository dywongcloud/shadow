//! Live witness for stream mode 0x06 (`STREAM_CORE`) and `hive-cores`, over
//! real `hive_p2p::bind()` endpoints, the production `serve_tunnels_full`
//! accept path and a real trust set. SHADW chain plan, Annex IV, L3.
//!
//!   0. writer-reopen             the writer closes and reopens mid-way; appends after the
//!                                reopen must not overwrite (patched-hypercore offset fix)
//!   1. trusted-pull-full         a trusted peer pulls and verifies all 600 blocks (3 requests)
//!   2. trusted-pull-incremental  5 new blocks arrive as exactly 5 imported blocks
//!   3. untrusted-stream-dropped  an untrusted (join-pending) peer's 0x06 stream is never served
//!   4. unknown-core-refused      a core the node does not hold is refused
//!   5. replayed-capability       a capability lifted from one connection fails on another
//!   6. replica-reopen-then-pull  the replica reopens from disk and keeps pulling
//!   7. no-handler-reset          a node without a core handler fails the stream fast,
//!                                never as a tunnel session
//!
//! Usage: `cargo run -p hive-cores --example core_witness`
//! Exit code 0 = every witness line passed; 1 = at least one failed.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hive_cores::{discovery_key, CoreStore};
use sha2::{Digest, Sha256};

const BUDGET: Duration = Duration::from_secs(60);

fn report(ok: bool, label: &str, detail: &str, failures: &mut usize) {
    if ok {
        println!("WITNESS_OK:{label}: {detail}");
    } else {
        println!("WITNESS_FAIL:{label}: {detail}");
        *failures += 1;
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hive-cores-witness-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// A join handler that admits nobody: it keeps untrusted connections open so
/// the PER-STREAM gate (not connection admission) is what refuses 0x06.
fn refuse_all_joins() -> hive_p2p::JoinHandler {
    Arc::new(|_, _, _| Box::pin(async { Vec::new() }))
}

async fn serve_node(store: Option<&Arc<CoreStore>>, trust: hive_p2p::TrustSet) -> anyhow::Result<(String, String)> {
    let ep = hive_p2p::bind().await?;
    let id = ep.id().to_string();
    let addr = hive_p2p::addr_json(&ep).ok_or_else(|| anyhow::anyhow!("addr_json unavailable"))?;
    tokio::spawn(hive_p2p::serve_tunnels_full(
        ep,
        "http://127.0.0.1:1".into(),
        64,
        Some(trust),
        None,
        Some(refuse_all_joins()),
        None,
        None,
        None,
        &[],
        store.map(|s| s.handler()),
    ));
    Ok((id, addr))
}

async fn pull(
    pool: &hive_p2p::PeerPool,
    store: &CoreStore,
    id: &str,
    addr: &str,
    key: &[u8; 32],
) -> anyhow::Result<hive_cores::PullReport> {
    let stream = pool.open_core(id, addr).await?;
    tokio::time::timeout(BUDGET, store.pull(stream, key))
        .await
        .map_err(|_| anyhow::anyhow!("pull timed out"))?
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut failures = 0usize;
    let (dir_a, dir_b, dir_c) = (scratch("writer"), scratch("reader"), scratch("stranger"));

    // Node A writes a core of 600 blocks, closing and REOPENING its store halfway:
    // unpatched hypercore 0.16 appends after a reopen over its own data from
    // offset 8192, which the full pull below would then fail to verify.
    let key = {
        let store_a = CoreStore::open(&dir_a).await?;
        let key = store_a.create_writer().await?;
        for i in 0..300u32 {
            store_a.append(&key, format!("entry {i}").as_bytes()).await?;
        }
        key
    };
    let store_a = CoreStore::open(&dir_a).await?;
    let writer_reopened_len = store_a.info(&key).await?.length;
    for i in 300..600u32 {
        store_a.append(&key, format!("entry {i}").as_bytes()).await?;
    }
    report(writer_reopened_len == 300, "writer-reopen", &format!("writer length after reopen {writer_reopened_len}"), &mut failures);
    let trust: hive_p2p::TrustSet = Arc::new(std::sync::RwLock::new(Default::default()));
    let (id_a, addr_a) = serve_node(Some(&store_a), trust.clone()).await?;

    // Node B is trusted and follows the core.
    let ep_b = hive_p2p::bind().await?;
    trust.write().unwrap().insert(ep_b.id().to_string());
    let pool_b = hive_p2p::PeerPool::new(ep_b);
    let store_b = CoreStore::open(&dir_b).await?;
    store_b.follow(key).await?;

    // 1. Full pull.
    let t = Instant::now();
    let r = pull(&pool_b, &store_b, &id_a, &addr_a, &key).await;
    let last = store_b.get(&key, 599).await.ok().flatten();
    report(
        matches!(&r, Ok(p) if p.imported_blocks == 600 && p.length == 600) && last.as_deref() == Some(b"entry 599".as_slice()),
        "trusted-pull-full",
        &format!("{r:?}; block 599 = {:?}; {} ms", last.map(|b| String::from_utf8_lossy(&b).into_owned()), t.elapsed().as_millis()),
        &mut failures,
    );

    // 2. Incremental pull.
    for i in 600..605u32 {
        store_a.append(&key, format!("entry {i}").as_bytes()).await?;
    }
    let r = pull(&pool_b, &store_b, &id_a, &addr_a, &key).await;
    report(matches!(&r, Ok(p) if p.imported_blocks == 5 && p.length == 605), "trusted-pull-incremental", &format!("{r:?}"), &mut failures);

    // 3. Untrusted peer: connection stays open (join handler present) but its
    //    0x06 stream is dropped before the core handler ever sees it.
    let ep_c = hive_p2p::bind().await?;
    let pool_c = hive_p2p::PeerPool::new(ep_c);
    let store_c = CoreStore::open(&dir_c).await?;
    store_c.follow(key).await?;
    let r = pull(&pool_c, &store_c, &id_a, &addr_a, &key).await;
    let c_len = store_c.info(&key).await?.length;
    report(r.is_err() && c_len == 0, "untrusted-stream-dropped", &format!("pull error: {}; stranger's length {c_len}", r.err().map_or("none".into(), |e| e.to_string())), &mut failures);

    // 4. A core node A does not hold.
    let other_store = CoreStore::open(&scratch("other")).await?;
    let other = other_store.create_writer().await?;
    store_b.follow(other).await?;
    let r = pull(&pool_b, &store_b, &id_a, &addr_a, &other).await;
    report(
        matches!(&r, Err(e) if e.to_string().contains("refused")),
        "unknown-core-refused",
        &format!("{}", r.err().map_or("accepted".into(), |e| e.to_string())),
        &mut failures,
    );

    // 5. Capability bound to the connection: compute it on one stream, present it on another.
    let first = pool_b.open_core(&id_a, &addr_a).await?;
    let mut ekm = [0u8; 32];
    first.conn.export_keying_material(&mut ekm, b"shadw/core/cap/0", &key).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let lifted: [u8; 32] = Sha256::new().chain_update(b"shadw/core/cap/0").chain_update(key).chain_update(ekm).finalize().into();
    drop(first);
    let ep_d = hive_p2p::bind().await?;
    trust.write().unwrap().insert(ep_d.id().to_string());
    let pool_d = hive_p2p::PeerPool::new(ep_d);
    let mut other_conn = pool_d.open_core(&id_a, &addr_a).await?;
    let mut hello = discovery_key(&key).to_vec();
    hello.extend(lifted);
    other_conn.send.write_all(&hello).await?;
    let mut reply = [0u8; 9];
    let got = tokio::time::timeout(BUDGET, other_conn.recv.read_exact(&mut reply)).await;
    let refused = matches!(got, Ok(Ok(()))) && &reply[..8] == hive_cores::MAGIC && reply[8] == 1;
    report(refused, "replayed-capability", &format!("reply {:?}", &reply), &mut failures);

    // 6. Reopen the REPLICA from disk, then pull a block appended meanwhile.
    drop(store_b);
    let store_b = CoreStore::open(&dir_b).await?;
    let reopened_len = store_b.info(&key).await?.length;
    let appended = store_a.append(&key, b"after reopen").await.is_ok();
    let r = pull(&pool_b, &store_b, &id_a, &addr_a, &key).await;
    let tail = store_b.get(&key, 605).await.ok().flatten();
    report(
        reopened_len == 605 && appended && matches!(&r, Ok(p) if p.imported_blocks == 1 && p.length == 606) && tail.as_deref() == Some(b"after reopen".as_slice()),
        "replica-reopen-then-pull",
        &format!("replica length after reopen {reopened_len}; {r:?}; block 605 = {:?}", tail.map(|b| String::from_utf8_lossy(&b).into_owned())),
        &mut failures,
    );

    // 7. A node with no core handler resets 0x06 instead of treating it as a tunnel.
    let trust_e: hive_p2p::TrustSet = Arc::new(std::sync::RwLock::new(Default::default()));
    let ep_f = hive_p2p::bind().await?;
    trust_e.write().unwrap().insert(ep_f.id().to_string());
    let (id_e, addr_e) = serve_node(None, trust_e).await?;
    let pool_f = hive_p2p::PeerPool::new(ep_f);
    let t = Instant::now();
    let r = pull(&pool_f, &store_b, &id_e, &addr_e, &key).await;
    let fast = t.elapsed() < Duration::from_secs(10);
    report(r.is_err() && fast, "no-handler-reset", &format!("{} in {} ms", r.err().map_or("accepted".into(), |e| e.to_string()), t.elapsed().as_millis()), &mut failures);

    for d in [dir_a, dir_b, dir_c] {
        let _ = std::fs::remove_dir_all(d);
    }
    if failures == 0 {
        println!("WITNESS_SUMMARY: all passed");
        Ok(())
    } else {
        println!("WITNESS_SUMMARY: {failures} failed");
        std::process::exit(1);
    }
}
