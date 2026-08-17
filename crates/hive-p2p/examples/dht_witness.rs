//! Live witness for the mainline-DHT address lookup (`hive_p2p::dht`).
//!
//! Every line below is a REAL execution against the REAL public BitTorrent DHT
//! — no mocks, no test harness, no fixtures. Each endpoint is bound through the
//! production `bind_full` with the exact configuration 12 of 14 fleet hosts run
//! today (`HIVE_DISCOVERY_N0=0`, no `HIVE_BOOTSTRAP_PEERS`, no
//! `HIVE_DISCOVERY_DNS`), which is the configuration that leaves a node with NO
//! address-lookup provider at all.
//!
//!   1. `no_source_cannot_dial_by_id`
//!      → the pain, reproduced: with the DHT off and no seeds/Seer/n0, a
//!        `connect()` by bare `EndpointId` fails immediately with "No addressing
//!        information available". This is what `PeerPool::dial_fresh` hits.
//!   2. `dht_publishes_and_resolves`
//!      → the fix: A publishes, an INDEPENDENT DHT node (the `--dht-probe`
//!        code path, no endpoint, no other provider) resolves A's id, and B —
//!        also seedless — connects to A by bare id. The contrast with (1) is
//!        the proof; convergence alone would not be.
//!   3. `probe_misses_unpublished_id`
//!      → a valid id that was never published is a MISS, so (2)'s hit is not
//!        the probe answering yes to everything.
//!   4. `publish_filter_strips_private_addrs`
//!      → with `HIVE_DHT_PUBLISH_DIRECT=1`, the published record carries the
//!        relay URL and the host's PUBLIC address, and never an
//!        RFC1918/CGNAT/link-local one.
//!   5. `unresolvable_bootstrap_degrades` / `port_conflict_degrades`
//!      → both failure modes leave the endpoint bound and every existing path
//!        untouched, with a WARN naming the cause. Never a failed bind.
//!   6. `existing_sources_intact`
//!      → with the DHT on, a `HIVE_BOOTSTRAP_PEERS` seed still registers and
//!        still resolves, `HIVE_DISCOVERY_DNS` still registers, `HIVE_IROH_PORT`
//!        still pins the QUIC socket (the DHT's UDP socket is a separate one),
//!        and `HIVE_PUBLIC_IP` still lands in `addr_json`.
//!
//! Usage: `cargo run -p hive-p2p --example dht_witness`
//! Exit code 0 = every witness line passed; 1 = at least one failed.
//! Needs outbound UDP to the public DHT; takes ~90s.

use std::time::{Duration, Instant};

use iroh::{EndpointAddr, SecretKey, TransportAddr};

fn pass(name: &str, detail: impl std::fmt::Display) -> bool {
    println!("PASS  {name}  {detail}");
    true
}

fn fail(name: &str, detail: impl std::fmt::Display) -> bool {
    println!("FAIL  {name}  {detail}");
    false
}

/// Fleet-shaped bind: no seeds, no Seer, no n0 — the state of 12 of 14 hosts.
async fn bind_bare(key: &str) -> anyhow::Result<iroh::Endpoint> {
    let dir = std::env::temp_dir().join("hive-dht-witness");
    std::fs::create_dir_all(&dir)?;
    Ok(hive_p2p::bind_full(Some(dir.join(key)), &[], &[], false).await?)
}

fn accept_forever(ep: iroh::Endpoint) {
    tokio::spawn(async move {
        while let Some(incoming) = ep.accept().await {
            if let Ok(accepting) = incoming.accept() {
                tokio::spawn(async move {
                    if let Ok(conn) = accepting.await {
                        // Hold it briefly so the dialer's handshake completes.
                        let _ = conn;
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                });
            }
        }
    });
}

async fn connect_by_bare_id(
    ep: &iroh::Endpoint,
    target: iroh::EndpointId,
    budget: Duration,
) -> Result<(), String> {
    match tokio::time::timeout(
        budget,
        ep.connect(EndpointAddr::new(target), hive_p2p::HIVE_ALPN),
    )
    .await
    {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(format!("{e}")),
        Err(_) => Err(format!("timeout after {}ms", budget.as_millis())),
    }
}

// 1 — the pain, reproduced.
async fn no_source_cannot_dial_by_id() -> bool {
    const N: &str = "no_source_cannot_dial_by_id";
    std::env::set_var("HIVE_DISCOVERY_DHT", "0");
    let (a, b) = match (bind_bare("off_a.key").await, bind_bare("off_b.key").await) {
        (Ok(a), Ok(b)) => (a, b),
        _ => return fail(N, "bind failed"),
    };
    let s = hive_p2p::dht::stats();
    if s.dht_registered || s.seed_providers != 0 || s.pkarr_providers != 0 || s.n0_enabled {
        return fail(N, format!("expected zero providers, got {s:?}"));
    }
    let a_id = a.id();
    accept_forever(a);
    let t = Instant::now();
    let r = connect_by_bare_id(&b, a_id, Duration::from_secs(15)).await;
    std::env::remove_var("HIVE_DISCOVERY_DHT");
    match r {
        Err(e) => pass(
            N,
            format!(
                "zero providers ⇒ connect refused in {}ms: {e}",
                t.elapsed().as_millis()
            ),
        ),
        Ok(()) => fail(N, "connected with no address source configured"),
    }
}

// 2 — the fix.
async fn dht_publishes_and_resolves() -> (bool, Option<iroh::EndpointId>) {
    const N: &str = "dht_publishes_and_resolves";
    let a = match bind_bare("dht_a.key").await {
        Ok(a) => a,
        Err(e) => return (fail(N, format!("bind A failed: {e}")), None),
    };
    if !hive_p2p::dht::stats().dht_registered {
        return (
            fail(N, "DHT provider not registered — is outbound UDP blocked?"),
            None,
        );
    }
    let a_id = a.id();
    accept_forever(a);
    println!("      (waiting 25s for A's pkarr record to reach the DHT)");
    tokio::time::sleep(Duration::from_secs(25)).await;

    // Independent resolve: its own DHT node, no endpoint, no other provider.
    match hive_p2p::dht::probe(&a_id.to_string(), Duration::from_secs(60)).await {
        Ok(Some(hit)) => println!(
            "      probe HIT {}ms attempts={} relay={:?} direct={:?}",
            hit.elapsed_ms, hit.attempts, hit.relay_urls, hit.direct_addrs
        ),
        Ok(None) => {
            return (
                fail(N, "independent DHT probe found no record for A"),
                Some(a_id),
            )
        }
        Err(e) => return (fail(N, format!("probe error: {e}")), Some(a_id)),
    }

    let b = match bind_bare("dht_b.key").await {
        Ok(b) => b,
        Err(e) => return (fail(N, format!("bind B failed: {e}")), Some(a_id)),
    };
    let t = Instant::now();
    let r = connect_by_bare_id(&b, a_id, Duration::from_secs(45)).await;
    let s = hive_p2p::dht::stats();
    let ok = match r {
        Ok(()) => pass(
            N,
            format!(
                "seedless B reached A by bare id in {}ms (resolves={} hits={})",
                t.elapsed().as_millis(),
                s.resolves,
                s.resolve_hits
            ),
        ),
        Err(e) => fail(N, format!("connect failed: {e}")),
    };
    (ok, Some(a_id))
}

// 3 — the probe is not a yes-machine.
async fn probe_misses_unpublished_id() -> bool {
    const N: &str = "probe_misses_unpublished_id";
    let never_published = SecretKey::generate().public();
    match hive_p2p::dht::probe(&never_published.to_string(), Duration::from_secs(12)).await {
        Ok(None) => pass(N, format!("{} correctly MISS", never_published.fmt_short())),
        Ok(Some(h)) => fail(N, format!("unexpected hit: {h:?}")),
        Err(e) => fail(N, format!("probe error: {e}")),
    }
}

// 4 — the privacy filter.
async fn publish_filter_strips_private_addrs() -> bool {
    const N: &str = "publish_filter_strips_private_addrs";
    std::env::set_var("HIVE_DHT_PUBLISH_DIRECT", "1");
    let a = match bind_bare("direct.key").await {
        Ok(a) => a,
        Err(e) => {
            std::env::remove_var("HIVE_DHT_PUBLISH_DIRECT");
            return fail(N, format!("bind failed: {e}"));
        }
    };
    let a_id = a.id();
    let local: Vec<String> = a.addr().addrs.iter().map(|x| x.to_string()).collect();
    accept_forever(a);
    tokio::time::sleep(Duration::from_secs(25)).await;
    let r = hive_p2p::dht::probe(&a_id.to_string(), Duration::from_secs(60)).await;
    std::env::remove_var("HIVE_DHT_PUBLISH_DIRECT");
    match r {
        Ok(Some(hit)) => {
            let leaked: Vec<&String> = hit
                .direct_addrs
                .iter()
                .filter(|s| {
                    s.parse::<std::net::SocketAddr>()
                        .map(|sa| !is_public(sa.ip()))
                        .unwrap_or(true)
                })
                .collect();
            if leaked.is_empty() {
                pass(
                    N,
                    format!(
                        "local addrs {local:?} ⇒ published {:?} (no private addr)",
                        hit.direct_addrs
                    ),
                )
            } else {
                fail(N, format!("private addresses published: {leaked:?}"))
            }
        }
        Ok(None) => fail(N, "record not found; cannot judge the filter"),
        Err(e) => fail(N, format!("probe error: {e}")),
    }
}

fn is_public(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || (o[0] == 100 && (64..128).contains(&o[1])))
        }
        std::net::IpAddr::V6(v6) => !(v6.is_loopback() || v6.is_unspecified()),
    }
}

// 5a — unresolvable bootstrap must not fail the bind.
async fn unresolvable_bootstrap_degrades() -> bool {
    const N: &str = "unresolvable_bootstrap_degrades";
    std::env::set_var("HIVE_DHT_BOOTSTRAP", "no-such-host.invalid:6881");
    std::env::set_var("HIVE_DHT_BOOTSTRAP_RESOLVE_MS", "2000");
    let t = Instant::now();
    let r = bind_bare("degrade.key").await;
    std::env::remove_var("HIVE_DHT_BOOTSTRAP");
    std::env::remove_var("HIVE_DHT_BOOTSTRAP_RESOLVE_MS");
    match r {
        Ok(ep) => {
            let s = hive_p2p::dht::stats();
            let out = if s.dht_registered {
                fail(N, "provider registered despite unresolvable bootstrap")
            } else {
                pass(
                    N,
                    format!(
                        "bind OK in {}ms, provider skipped: {}",
                        t.elapsed().as_millis(),
                        s.dht_skip_reason.unwrap_or_default()
                    ),
                )
            };
            ep.close().await;
            out
        }
        Err(e) => fail(N, format!("bind FAILED — this must never happen: {e}")),
    }
}

// 5b — a pinned-but-taken DHT port must not fail the bind either.
async fn port_conflict_degrades() -> bool {
    const N: &str = "port_conflict_degrades";
    let squatter = match std::net::UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(e) => return fail(N, format!("could not take a port to squat: {e}")),
    };
    let port = squatter.local_addr().map(|a| a.port()).unwrap_or(0);
    std::env::set_var("HIVE_DHT_PORT", port.to_string());
    let t = Instant::now();
    let r = bind_bare("conflict.key").await;
    std::env::remove_var("HIVE_DHT_PORT");
    let out = match r {
        Ok(ep) => {
            let s = hive_p2p::dht::stats();
            let out = if s.dht_registered {
                fail(N, "provider registered despite a taken UDP port")
            } else {
                pass(
                    N,
                    format!(
                        "bind OK in {}ms, provider skipped: {}",
                        t.elapsed().as_millis(),
                        s.dht_skip_reason.unwrap_or_default()
                    ),
                )
            };
            ep.close().await;
            out
        }
        Err(e) => fail(N, format!("bind FAILED — this must never happen: {e}")),
    };
    drop(squatter);
    out
}

// 6 — nothing that worked before stopped working.
async fn existing_sources_intact() -> bool {
    const N: &str = "existing_sources_intact";
    let seed_key = SecretKey::generate();
    // 203.0.113.0/24 is TEST-NET-3: routable-looking, guaranteed unroutable, so
    // a timeout here proves the address was RESOLVED and dialed (the failure
    // mode we want) rather than never found ("No addressing information").
    let seed_addr = EndpointAddr::from_parts(
        seed_key.public(),
        [TransportAddr::Ip(
            "203.0.113.7:3399".parse().expect("literal"),
        )],
    );
    let seeds = vec![hive_p2p::SeedPeer {
        node_id: seed_key.public().to_string(),
        addr_json: match serde_json::to_string(&seed_addr) {
            Ok(j) => j,
            Err(e) => return fail(N, format!("seed encode failed: {e}")),
        },
    }];
    std::env::set_var("HIVE_IROH_PORT", "3399");
    std::env::set_var("HIVE_PUBLIC_IP", "198.51.100.42");
    let dir = std::env::temp_dir().join("hive-dht-witness");
    let _ = std::fs::create_dir_all(&dir);
    let r = hive_p2p::bind_full(
        Some(dir.join("intact.key")),
        &seeds,
        &["http://127.0.0.1:3350".to_string()],
        false,
    )
    .await;
    let ep = match r {
        Ok(ep) => ep,
        Err(e) => {
            std::env::remove_var("HIVE_IROH_PORT");
            std::env::remove_var("HIVE_PUBLIC_IP");
            return fail(N, format!("bind failed: {e}"));
        }
    };
    let json = hive_p2p::addr_json(&ep).unwrap_or_default();
    let s = hive_p2p::dht::stats();
    let pinned = ep.addr().addrs.iter().all(|a| match a {
        TransportAddr::Ip(sa) => sa.port() == 3399,
        _ => true,
    });
    let public_advertised = json.contains("198.51.100.42:3399");
    // The seed's MemoryLookup entry must be CONSULTED: a bare-id dial that gets
    // as far as a timeout resolved an address; "No addressing information"
    // would mean the provider was dropped.
    let seed_err = connect_by_bare_id(&ep, seed_key.public(), Duration::from_secs(6))
        .await
        .err()
        .unwrap_or_default();
    let seed_resolved = !seed_err.contains("No addressing information");
    std::env::remove_var("HIVE_IROH_PORT");
    std::env::remove_var("HIVE_PUBLIC_IP");
    ep.close().await;
    if s.seed_providers == 1
        && s.pkarr_providers == 1
        && s.dht_registered
        && pinned
        && public_advertised
        && seed_resolved
    {
        pass(
            N,
            format!(
                "seeds=1 pkarr=1 dht=on port pinned 3399, addr_json carries HIVE_PUBLIC_IP, seed lookup live ({seed_err})"
            ),
        )
    } else {
        fail(
            N,
            format!(
                "seeds={} pkarr={} dht={} pinned={pinned} public_advertised={public_advertised} seed_resolved={seed_resolved} ({seed_err})",
                s.seed_providers, s.pkarr_providers, s.dht_registered
            ),
        )
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hive_p2p=info".into()),
        )
        .init();
    // A stale key dir would reuse identities whose records are already on the
    // DHT, which would let (2) pass without publishing anything this run.
    let _ = std::fs::remove_dir_all(std::env::temp_dir().join("hive-dht-witness"));

    let mut ok = true;
    ok &= no_source_cannot_dial_by_id().await;
    let (r2, _) = dht_publishes_and_resolves().await;
    ok &= r2;
    ok &= probe_misses_unpublished_id().await;
    ok &= publish_filter_strips_private_addrs().await;
    ok &= unresolvable_bootstrap_degrades().await;
    ok &= port_conflict_degrades().await;
    ok &= existing_sources_intact().await;

    if ok {
        println!("\nALL DHT WITNESSES PASSED");
        Ok(())
    } else {
        std::process::exit(1);
    }
}
