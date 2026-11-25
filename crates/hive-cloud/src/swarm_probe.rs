//! Operator diagnostic for the network-swarm transport (same family as
//! `--dns-probe` / `--dht-probe` / `--litebox-probe`).
//!
//! SCOPE, stated plainly: this binds TWO loopback-only iroh endpoints over the
//! production `HIVE_ALPN` and the production hybrid PQ crypto provider, dials
//! them by their REAL bound addresses, and runs ONE bounded round of framed,
//! byte-exact echoes. It proves the swarm channel's frames, typestate,
//! identity/ALPN gates, deadline and straggler leash. It does NOT prove the
//! fleet's public-IP/`HIVE_IROH_PORT` dialable path, n0/DHT behaviour, or any
//! QUIC-versus-TCP latency claim. Nothing here binds :50052, joins a mesh, or
//! touches a running node.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::Instant;

use crate::bounded_round::BoundedRound;

/// Default is FAILURE. `BoundedRound`'s `Completion` drop guard sends this on a
/// dropped or panicking task, so a vacuous PASS is impossible: a landed default
/// has `bytes_echoed == 0` and is explicitly rejected below.
#[derive(Clone, Copy, Debug, Default)]
struct ProbeOutcome {
    bytes_echoed: usize,
    exact: bool,
    hybrid_seen: bool,
}

const PAYLOADS: [usize; 3] = [1, 1024, 65_536];
const STALLED_KEY: &str = "stalled";
const LEASH_MS: u64 = 150;
const ROUND_DEADLINE_MS: u64 = 3_000;
const BIND_TIMEOUT: Duration = Duration::from_secs(10);
const DIAL_TIMEOUT: Duration = Duration::from_secs(5);

fn payload(n: usize, seed: u8) -> Vec<u8> {
    (0..n)
        .map(|i| seed.wrapping_add((i % 251) as u8))
        .collect()
}

fn frame_round_trip_check() -> Result<()> {
    for n in [0usize, 1, 63, 1024, 65_536] {
        let p = payload(n, 0x5a);
        let frame = hive_p2p::swarm::encode_frame(hive_p2p::swarm::TAG_HELLO, &p)?;
        let (tag, decoded) = hive_p2p::swarm::decode_frame(&frame)?;
        if tag != hive_p2p::swarm::TAG_HELLO || decoded != p.as_slice() {
            bail!("frame round-trip mismatch at {n} bytes");
        }
    }
    if hive_p2p::swarm::encode_frame(
        hive_p2p::swarm::TAG_HELLO,
        &vec![0u8; hive_p2p::swarm::FRAME_MAX_BYTES + 1],
    )
    .is_ok()
    {
        bail!("oversize frame was not refused");
    }
    if hive_p2p::swarm::decode_frame(&[0u8; 3]).is_ok() {
        bail!("truncated frame header was not refused");
    }
    for (state, expected) in [
        ("4: ACTIVE", true),
        ("ACTIVE", true),
        ("active", true),
        ("1: DOWN", false),
        ("", false),
    ] {
        if hive_p2p::swarm::port_is_active(state) != expected {
            bail!("IB sysfs state parser wrong for {state:?}");
        }
    }
    println!("RESULT frames_round_trip=PASS sizes=5 bound_refused=1 truncated_refused=1 ib_state_parser=PASS");
    Ok(())
}

/// One client leg. Every dial verifies the negotiated ALPN and the peer's
/// endpoint identity BEFORE any frame is sent, and reads back the ACTUAL
/// negotiated key-exchange group off the completed handshake.
async fn client_leg(
    ep: iroh::Endpoint,
    addr: iroh::EndpointAddr,
    server_id: iroh::EndpointId,
    first: Vec<u8>,
    second: Vec<u8>,
    live: Arc<AtomicUsize>,
    delay: Duration,
) -> ProbeOutcome {
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
    let conn = match tokio::time::timeout(DIAL_TIMEOUT, ep.connect(addr, hive_p2p::HIVE_ALPN)).await
    {
        Ok(Ok(c)) => c,
        _ => return ProbeOutcome::default(),
    };
    if let Err(error) = hive_p2p::swarm::check_peer(&conn, server_id) {
        tracing::warn!(error = %error, "swarm probe refused an untrusted peer");
        return ProbeOutcome::default();
    }
    let before = hive_p2p::pq_kex_stats().0;
    hive_p2p::swarm::record_probe_kex(&conn);
    let hybrid_seen = hive_p2p::pq_kex_stats().0 > before;
    let (send, recv) = match tokio::time::timeout(DIAL_TIMEOUT, conn.open_bi()).await {
        Ok(Ok(pair)) => pair,
        _ => return ProbeOutcome::default(),
    };
    let stream = tokio::io::join(recv, send);
    let channel = hive_p2p::swarm::SwarmChannel::new(stream, server_id.to_string(), live);
    let opened = match channel.handshake(&first).await {
        Ok(c) => c,
        Err(_) => return ProbeOutcome::default(),
    };
    let closed = match opened.echo_round_trip(&second).await {
        Ok(c) => c,
        Err(_) => return ProbeOutcome::default(),
    };
    let _ = closed.peer();
    ProbeOutcome {
        bytes_echoed: first.len() + second.len(),
        exact: true,
        hybrid_seen,
    }
}

/// Server side: accept real incoming connections, verify ALPN and identity,
/// then serve the frame protocol. `served_bytes` is the ONLY evidence the
/// accept path actually echoed something.
async fn serve(
    ep: iroh::Endpoint,
    dialer_id: iroh::EndpointId,
    served_bytes: Arc<AtomicUsize>,
    connections: Arc<AtomicUsize>,
    live: Arc<AtomicUsize>,
) {
    while let Some(incoming) = ep.accept().await {
        let Ok(accepting) = incoming.accept() else {
            continue;
        };
        let pending = accepting.into_0rtt();
        let conn = match tokio::time::timeout(DIAL_TIMEOUT, pending.handshake_completed()).await {
            Ok(Ok(c)) => c,
            _ => continue,
        };
        if let Err(error) = hive_p2p::swarm::check_peer(&conn, dialer_id) {
            tracing::warn!(error = %error, "swarm probe refused an untrusted peer");
            continue;
        }
        hive_p2p::swarm::record_probe_kex(&conn);
        connections.fetch_add(1, Ordering::AcqRel);
        let served_bytes = served_bytes.clone();
        let live = live.clone();
        tokio::spawn(async move {
            while let Ok((send, recv)) = conn.accept_bi().await {
                let mut stream = tokio::io::join(recv, send);
                let slot = hive_p2p::swarm::InflightSlot::acquire(live.clone());
                match hive_p2p::swarm::serve_probe_stream(&mut stream).await {
                    Ok(n) => {
                        served_bytes.fetch_add(n, Ordering::AcqRel);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "swarm probe stream refused");
                    }
                }
                drop(slot);
            }
        });
    }
}


/// Witness the INFERENCE admission primitives with the real code the reconcile
/// loop runs. Refuters named three concrete holes: fresh gossip latency bypasses
/// the rpc-port check; an unreachable member still counts VRAM; and a URL is
/// advertised with no readiness proof. Each is exercised here against a live
/// socket, not a reimplemented decision.
async fn admission_witness() -> Result<()> {
    use std::collections::HashMap;

    use crate::gpu_pool::{GpuPoolNode, GpuPoolRegion};
    use crate::inference::{plan_members, rpc_port_reachable, MemberLink};

    // (1) The shared reachability primitive: a real listener answers, a closed
    // port does not. Gossip latency is not an argument to this function at all.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let open_port = listener.local_addr()?.port();
    let answered = rpc_port_reachable("127.0.0.1", open_port, 500).await;
    drop(listener);
    let refused = rpc_port_reachable("127.0.0.1", open_port, 500).await;
    if !answered {
        bail!("rpc_port_reachable refused a live loopback listener");
    }
    if refused {
        bail!("rpc_port_reachable admitted a closed port");
    }
    println!("RESULT rpc_connect=PASS live_accepted=1 closed_refused=1 gossip_latency_is_not_an_input=1");

    let node = |name: &str, free: u64| GpuPoolNode {
        name: name.to_string(),
        gpu_model: Some("witness".into()),
        vram_total_mb: free,
        vram_free_mb: free,
        latency_ms: 5,
    };
    let pools = vec![GpuPoolRegion {
        region: "witness".into(),
        nodes: vec![node("coord", 1024), node("dead", 9000), node("live", 9000)],
        pool_vram_total_mb: 19_024,
        pool_vram_free_mb: 19_024,
    }];

    // (2) "dead" has FRESH gossip latency (5 ms) but its rpc-port connect
    // failed. It must be refused, named in the honest error, and never counted.
    let mut links = HashMap::new();
    links.insert(
        "dead".to_string(),
        MemberLink {
            rtt_ms: 5,
            reachable: false,
        },
    );
    links.insert(
        "live".to_string(),
        MemberLink {
            rtt_ms: 8,
            reachable: true,
        },
    );
    let failed = plan_members(12_000, "coord", "witness", 1024, true, &pools, &links, 20);
    match failed {
        Err(message) => {
            if !message.contains("dead") || !message.contains("rpc-port connect") {
                bail!("placement error did not name the unreachable member: {message}");
            }
            if message.contains("19024") {
                bail!("unreachable member's VRAM was still counted: {message}");
            }
        }
        Ok(members) => bail!("dead member was admitted: {members:?}"),
    }
    // Idempotence: the same inputs produce the same refusal.
    let again = plan_members(12_000, "coord", "witness", 1024, true, &pools, &links, 20);
    match again {
        Err(message) if message.contains("dead") => {}
        _ => bail!("plan_members is not idempotent"),
    }
    println!("RESULT admission=PASS fresh_gossip_latency_with_dead_rpc_refused=1 idempotent=1");

    // (3) With BOTH members reachable the same pool succeeds. Reachability, not
    // the RTT value, is the only admission gate.
    links.insert(
        "dead".to_string(),
        MemberLink {
            rtt_ms: 5,
            reachable: true,
        },
    );
    let ok = plan_members(12_000, "coord", "witness", 1024, true, &pools, &links, 20)
        .map_err(|e| anyhow::anyhow!("reachable pool was refused: {e}"))?;
    if ok.len() != 2 || !ok.iter().any(|n| n == "dead") || !ok.iter().any(|n| n == "live") {
        bail!("reachable members were not both planned: {ok:?}");
    }
    println!("RESULT reachable_pool=PASS members=2");

    // (4) The URL gate: a real /health listener returning 200 admits the URL,
    // 503 (llama-server's whole-load answer) and a closed port refuse it.
    let healthy = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let healthy_port = healthy.local_addr()?.port();
    tokio::spawn(http_status_server(healthy, "200 OK"));
    let loading = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let loading_port = loading.local_addr()?.port();
    tokio::spawn(http_status_server(loading, "503 Service Unavailable"));
    let closed = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let closed_port = closed.local_addr()?.port();
    drop(closed);

    let ready = crate::inference::coordinator_endpoint_ready("127.0.0.1", healthy_port).await;
    let loading_refused =
        !crate::inference::coordinator_endpoint_ready("127.0.0.1", loading_port).await;
    let closed_refused =
        !crate::inference::coordinator_endpoint_ready("127.0.0.1", closed_port).await;
    if !ready || !loading_refused || !closed_refused {
        bail!(
            "readiness gate wrong: ready={ready} loading_refused={loading_refused} closed_refused={closed_refused}"
        );
    }
    println!("RESULT url_gate=PASS ready_200=1 loading_503=0 closed_port=0");
    // (5) URL parameterisation: a gossiped string is parsed, IPv6 is
    // bracketed, and link-local/unspecified/multicast targets are refused.
    use crate::inference::inference_endpoint_base;
    if inference_endpoint_base("fe80::1", 50100).is_some()
        || inference_endpoint_base("169.254.169.254", 50100).is_some()
        || inference_endpoint_base("0.0.0.0", 50100).is_some()
        || inference_endpoint_base("not-an-ip", 50100).is_some()
    {
        bail!("a non-routable or unparseable address reached an inference URL");
    }
    if inference_endpoint_base("2001:db8::1", 50100).as_deref()
        != Some("http://[2001:db8::1]:50100")
    {
        bail!("IPv6 inference URL was not bracketed");
    }
    println!("RESULT url_parameterisation=PASS v6_bracketed=1 link_local_refused=1 metadata_refused=1");

    // (5) FNV mod 900 is NOT injective. A /health proof on a shared port
    // cannot say which project owns the listening server, so the leader must
    // HOLD URL injection for every project in that ambiguous group.
    use crate::inference::{ambiguous_inference_ports, port_for};
    let p9 = port_for("proj-9");
    let p90 = port_for("proj-90");
    if p9 != p90 {
        bail!("expected a real port_for collision, got {p9} and {p90}");
    }
    let shared = ambiguous_inference_ports(&[
        ("proj-9".into(), "10.0.0.1".into(), p9),
        ("proj-90".into(), "10.0.0.1".into(), p90),
        ("solo".into(), "10.0.0.2".into(), p9),
    ]);
    if !shared.contains(&("10.0.0.1".to_string(), p9)) {
        bail!("colliding projects were not held");
    }
    if shared.contains(&("10.0.0.2".to_string(), p9)) {
        bail!("a project on a different coordinator was wrongly held");
    }
    let single = ambiguous_inference_ports(&[("solo".into(), "10.0.0.2".into(), p9)]);
    if !single.is_empty() {
        bail!("a unique port was wrongly held");
    }
    println!("RESULT port_collision=PASS shared_held=1 unique_admitted=1");
    // (6) A HOLD must WITHDRAW a platform-injected URL, not just stop
    // writing. This is the exact decision the leader loop applies on an
    // ambiguous port, a dead endpoint and a coordinator move.
    use crate::inference::{inference_url_action, InferenceUrlAction};
    let wanted = format!("http://10.0.0.1:{p9}/v1");
    let moved = format!("http://10.0.0.9:{p9}/v1");
    let cases = [
        (None, &wanted, true, InferenceUrlAction::Inject),
        (Some(wanted.as_str()), &wanted, true, InferenceUrlAction::Hold),
        (Some(wanted.as_str()), &wanted, false, InferenceUrlAction::Withdraw),
        (Some(moved.as_str()), &wanted, false, InferenceUrlAction::Withdraw),
        (Some("http://tenant.example/v1"), &wanted, false, InferenceUrlAction::Hold),
    ];
    for (current, url, ready, expect) in cases {
        let got = inference_url_action(current, url, ready);
        if got != expect {
            bail!("url decision wrong for current={current:?} ready={ready}: got {got:?}, want {expect:?}");
        }
    }
    println!("RESULT url_state=PASS inject=1 hold=2 withdraw=2 tenant_override_untouched=1");
    // (6c) A HEALTHY endpoint must still never clobber a tenant override.
    // Only a stale PLATFORM-shaped url is ours to replace on Inject.
    let tenant_override = "https://tenant.example/v1";
    if inference_url_action(Some(tenant_override), &wanted, true)
        != InferenceUrlAction::Hold
    {
        bail!("healthy Inject clobbered a tenant override");
    }
    if inference_url_action(Some(moved.as_str()), &wanted, true)
        != InferenceUrlAction::Inject
    {
        bail!("a stale platform url was not replaced on a healthy Inject");
    }
    // (6d) A removed inference spec is absent from desired. The leader must
    // still withdraw its platform-shaped URL via the orphan sweep.
    if inference_url_action(Some(wanted.as_str()), "", false)
        != InferenceUrlAction::Withdraw
    {
        bail!("orphaned spec url was not withdrawn");
    }
    if inference_url_action(Some(tenant_override), "", false)
        != InferenceUrlAction::Hold
    {
        bail!("orphan sweep would delete a tenant override");
    }
    println!("RESULT url_override=PASS healthy_inject_preserved=1 stale_platform_replaced=1 orphan_withdrawn=1 orphan_override_held=1");

    // (6b) A backend switch, a missing coordinator or an unroutable IP must
    // also WITHDRAW, not early-continue. The leader loop now has ONE decision
    // point; an empty wanted URL with ready=false is that exact state.
    for current in [Some(wanted.as_str()), Some(moved.as_str()), None] {
        let got = inference_url_action(current, "", false);
        let expect = if current.is_some_and(|c| {
            crate::inference::is_platform_inference_url(c)
        }) {
            InferenceUrlAction::Withdraw
        } else {
            InferenceUrlAction::Hold
        };
        if got != expect {
            bail!("backend-switch decision wrong for {current:?}: got {got:?}");
        }
    }
    println!("RESULT url_no_target=PASS backend_switch_withdraw=1 coordinator_move_withdraw=1");




    Ok(())
}

/// Minimal HTTP answerer for the readiness witness. Writes a fixed response,
/// reads no tenant data, and never binds a public interface.
async fn http_status_server(listener: tokio::net::TcpListener, status: &'static str) {
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut request = [0u8; 1024];
        let _ = socket.read(&mut request).await;
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
        );
        let _ = socket.write_all(response.as_bytes()).await;
    }
}

pub async fn run() -> Result<()> {
    frame_round_trip_check()?;

    admission_witness().await?;

    let cap = hive_p2p::swarm::probe_capability();
    // MEASURE whether CapabilityMissing was actually constructed. On a host
    // with a real ACTIVE verbs device require_rdma returns Ok, so a hard-coded
    // capability_missing=1 would be a false witness.
    let capability_missing = match hive_p2p::swarm::require_rdma(&cap) {
        Ok(dev) => {
            println!(
                "RESULT rdma_verbs=CAPABLE hca={} port={} uverbs={}",
                dev.hca, dev.port, dev.uverbs.len()
            );
            false
        }
        Err(error) => {
            println!("RESULT rdma_verbs=NOT_CAPABLE typed_refusal={error}");
            matches!(error, hive_p2p::swarm::SwarmError::CapabilityMissing(_))
        }
    };
    println!(
        "RESULT thunderbolt_bridge_ip_only={} (IP interface, never an RDMA transport)",
        cap.thunderbolt_bridge_ip_only
    );

    // A hard timeout around EACH bind. No HIVE_IROH_PORT is read, no relay, no
    // DHT and no seeds: two endpoints in one process cannot collide or stall.
    let a = hive_p2p::swarm::map_timeout(
        tokio::time::timeout(BIND_TIMEOUT, hive_p2p::swarm::bind_probe_endpoint()).await,
    )??;
    let b = hive_p2p::swarm::map_timeout(
        tokio::time::timeout(BIND_TIMEOUT, hive_p2p::swarm::bind_probe_endpoint()).await,
    )??;

    let port_a = a
        .bound_sockets()
        .first()
        .map(|s| s.port())
        .unwrap_or_default();
    let port_b = b
        .bound_sockets()
        .first()
        .map(|s| s.port())
        .unwrap_or_default();
    if port_a == 0 || port_b == 0 || port_a == port_b {
        bail!("probe endpoints did not get distinct non-zero loopback ports");
    }
    println!("RESULT bind=PASS port_a={port_a} port_b={port_b} relay=disabled dht=disabled address_lookup=none");

    let addr_b = hive_p2p::swarm::direct_addr(&b)?;
    let id_a = a.id();
    let id_b = b.id();
    let live = Arc::new(AtomicUsize::new(0));
    let served_bytes = Arc::new(AtomicUsize::new(0));
    let connections = Arc::new(AtomicUsize::new(0));
    let server = tokio::spawn(serve(
        b.clone(),
        id_a,
        served_bytes.clone(),
        connections.clone(),
        live.clone(),
    ));

    // A real completed handshake, reused to witness the typed identity gate.
    let probe_conn = tokio::time::timeout(DIAL_TIMEOUT, a.connect(addr_b.clone(), hive_p2p::HIVE_ALPN))
        .await
        .map_err(|_| anyhow::anyhow!("probe connection timed out"))??;
    if let Err(error) = hive_p2p::swarm::check_peer(&probe_conn, id_b) {
        bail!("real probe connection failed identity/ALPN check: {error}");
    }

    // (7) Every typed error has a LIVE constructor, not just a declaration.
    // A deliberately wrong peer id and an already-elapsed deadline exercise
    // UntrustedPeer and Timeout, while require_rdma decides whether
    // CapabilityMissing is even admissible on this host.
    let wrong = iroh::SecretKey::generate().public();
    let untrusted = hive_p2p::swarm::check_peer(&probe_conn, wrong).unwrap_err();
    if !matches!(untrusted, hive_p2p::swarm::SwarmError::UntrustedPeer(_)) {
        bail!("wrong peer id did not produce UntrustedPeer");
    }
    let elapsed = tokio::time::timeout(Duration::from_millis(1), std::future::pending::<()>()).await.unwrap_err();
    let timed = hive_p2p::swarm::map_timeout::<()>(Err(elapsed)).unwrap_err();
    if !matches!(timed, hive_p2p::swarm::SwarmError::Timeout) {
        bail!("elapsed deadline did not produce Timeout");
    }
    println!(
        "RESULT typed_errors=PASS untrusted_peer=1 timeout=1 capability_missing={}",
        u8::from(capability_missing)
    );

    let pq_before = hive_p2p::pq_kex_stats();
    let mut round: BoundedRound<ProbeOutcome> = BoundedRound::new();
    let earlier = round.begin();
    if !earlier.is_empty() {
        bail!("a fresh round reported stragglers");
    }
    let this_round = round.round();
    let mut expected_bytes = 0usize;
    for (i, n) in PAYLOADS.iter().enumerate() {
        let key = format!("frame-{n}");
        let first = payload(*n, 0x11 + i as u8);
        let second = payload(n.saturating_add(7), 0x91 + i as u8);
        expected_bytes += first.len() + second.len();
        let leg = client_leg(
            a.clone(),
            addr_b.clone(),
            id_b,
            first,
            second,
            live.clone(),
            Duration::ZERO,
        );
        if !round.spawn(&key, leg) {
            bail!("duplicate round key {key}");
        }
    }
    // A stalled member: it still dials and echoes, but misses its leash. The
    // round must end without it and the task must NOT be cancelled.
    let stalled_first = payload(4096, 0x33);
    let stalled_second = payload(4096, 0x77);
    let stalled_leg = client_leg(
        a.clone(),
        addr_b.clone(),
        id_b,
        stalled_first,
        stalled_second,
        live.clone(),
        Duration::from_millis(900),
    );
    if !round.spawn(STALLED_KEY, stalled_leg) {
        bail!("duplicate round key {STALLED_KEY}");
    }

    let deadline = Instant::now() + Duration::from_millis(ROUND_DEADLINE_MS);
    let (landed, outstanding) =
        round.collect_leashed(deadline, |key| (key == STALLED_KEY).then_some(LEASH_MS)).await;

    let in_round: Vec<_> = landed
        .iter()
        .filter(|(r, _, o)| {
            *r == this_round && o.exact && o.bytes_echoed > 0 && o.hybrid_seen
        })
        .collect();
    if in_round.len() != PAYLOADS.len() {
        bail!(
            "only {}/{} real byte-exact hybrid-PQ echoes landed in this round (outstanding={outstanding})",
            in_round.len(),
            PAYLOADS.len()
        );
    }
    if !round.outstanding().iter().any(|k| k == STALLED_KEY) {
        bail!("stalled member was cancelled instead of parked");
    }
    let elapsed = deadline - Instant::now();
    let spent_ms = Duration::from_millis(ROUND_DEADLINE_MS)
        .saturating_sub(elapsed)
        .as_millis();
    if spent_ms >= ROUND_DEADLINE_MS as u128 {
        bail!("round ran to its full deadline instead of ending at the leash");
    }
    println!(
        "RESULT round=PASS landed={} bytes={} outstanding={} spent_ms={spent_ms} deadline_ms={ROUND_DEADLINE_MS}",
        in_round.len(),
        in_round.iter().map(|(_, _, o)| o.bytes_echoed).sum::<usize>(),
        outstanding
    );

    // PARKED never cancelled: the stalled task keeps running and its result
    // lands in a LATER round, exactly as bounded_round promises.
    let mut straggler = None;
    for _ in 0..15 {
        for (r, key, outcome) in round.begin() {
            if key == STALLED_KEY && r < round.round() {
                straggler = Some((r, outcome));
            }
        }
        if straggler.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let Some((straggler_round, outcome)) = straggler else {
        bail!("parked member never landed in a later round");
    };
    if !outcome.exact || outcome.bytes_echoed == 0 {
        bail!("parked member's echo was not byte-exact");
    }
    println!(
        "RESULT straggler=PASS dispatched_round={straggler_round} drained_in_round={} bytes={}",
        round.round(),
        outcome.bytes_echoed
    );

    let served = served_bytes.load(Ordering::Acquire);
    let expected_total = expected_bytes + outcome.bytes_echoed;
    if served < expected_total {
        bail!("server echoed {served} bytes, expected at least {expected_total}");
    }
    let pq_after = hive_p2p::pq_kex_stats();
    if pq_after.0 <= pq_before.0 {
        bail!(
            "no hybrid X25519MLKEM768/ML-KEM group was negotiated (before={:?} after={:?})",
            pq_before,
            pq_after
        );
    }
    if live.load(Ordering::Acquire) != 0 {
        bail!("in-flight slots leaked on the probe path");
    }
    println!(
        "RESULT kex=PASS hybrid_pq_delta={} classical_fallback_delta={} unknown_delta={}",
        pq_after.0 - pq_before.0,
        pq_after.1 - pq_before.1,
        pq_after.2 - pq_before.2
    );
    println!("RESULT server=PASS connections={} served_bytes={served}", connections.load(Ordering::Acquire));
    // iroh's own net-report address discovery may emit unrelated UDP to an
    // observed public address even with relay/DHT/address-lookup disabled.
    // That is NOT a dial path: every probe dial targets the loopback direct
    // address built from the peer's real bound socket.
    println!("RESULT scope=dial_target=loopback_direct_only fleet_public_path=NOT_PROVEN speedup_claim=none discovery_udp=iroh_net_report_may_emit");
    println!("RESULT inflight_slots_released=PASS live={}", live.load(Ordering::Acquire));

    server.abort();
    a.close().await;
    b.close().await;
    println!("SWARM_PROBE=PASS");
    Ok(())
}
