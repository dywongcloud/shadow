//! **`--dht-probe <64hex>` — can THIS host find THAT node id, from cold?**
//!
//! Same operator-diagnostic family as [`crate::dns_probe`]'s `--dns-probe`: it
//! answers one question with real network I/O, prints the evidence, and exits
//! without starting a node or touching any port a running node owns.
//!
//! The question it answers is deliberately narrow. `hive_p2p::dht::probe` binds
//! no iroh endpoint and registers no other address-lookup provider, so a hit
//! here CANNOT be explained by a bootstrap seed, a Seer pkarr relay, a cached
//! `peer_iroh.json` or an inbound gossip join — the three explanations that
//! make "the mesh converged" worthless as proof that the DHT did anything. A
//! hit means the target node's pkarr record is live on the public DHT and this
//! host's egress can read it.
//!
//! It retries until the budget expires (`HIVE_DHT_PROBE_TIMEOUT_MS`, default
//! 30s) because a cold DHT routing table legitimately misses the first attempts
//! and succeeds seconds later; a single-shot probe manufactures false negatives.
//!
//! Non-zero exit on a miss, so it composes into a shell check.

use std::time::Duration;

/// Run the probe for each id and print one line per target.
pub async fn run_cli(targets: &[String]) -> anyhow::Result<()> {
    let budget = hive_p2p::dht::probe_budget();
    println!(
        "probing {} endpoint id(s) on the public mainline DHT (budget {}s, this host's egress only)",
        targets.len(),
        budget.as_secs()
    );
    let mut misses = 0usize;
    for t in targets {
        let t = t.trim();
        match hive_p2p::dht::probe(t, budget).await {
            Ok(Some(hit)) => {
                println!(
                    "{:<64} FOUND    {}ms attempts={} relay=[{}] direct=[{}]",
                    hit.endpoint_id,
                    hit.elapsed_ms,
                    hit.attempts,
                    hit.relay_urls.join(","),
                    hit.direct_addrs.join(",")
                );
            }
            Ok(None) => {
                misses += 1;
                println!(
                    "{t:<64} MISS     no pkarr record on the DHT within {}s",
                    budget.as_secs()
                );
            }
            Err(e) => {
                misses += 1;
                println!("{t:<64} ERROR    {e}");
            }
        }
        // A tiny gap between targets so a multi-target probe does not hammer the
        // same bootstrap nodes back-to-back.
        if targets.len() > 1 {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
    if misses > 0 {
        anyhow::bail!(
            "{misses} of {} endpoint id(s) not resolvable via the public DHT from this host",
            targets.len()
        );
    }
    Ok(())
}
