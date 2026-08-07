//! Public UDP relay — datagram ingress for UDP container deployments
//! (Minecraft Bedrock `19132/udp`, game servers, DNS, any raw-UDP service).
//!
//! The edge entry layer for the UDP half of the raw-port space: every node
//! binds a public UDP socket on each allocated UDP `public_port`
//! (`raw_ports.rs` allocations, learned node-locally from the gateway's
//! deployment records and fleet-wide from the gossiped
//! `DeploymentInfo::raw_ports` bindings) and relays datagrams to the
//! deployment's container.
//!
//! UDP is CONNECTIONLESS — there is no accept/close to hang a splice on, so
//! `tokio::io::copy_bidirectional` (the TCP-splice mechanism `db_gateway.rs`
//! and `edge::ws_proxy` use) does not apply. This module is UDP's own
//! mechanism, deliberately separate from the TCP raw-port splice path:
//!
//! * **NAT-style session table** — the first datagram from a `(src_ip,
//!   src_port)` client creates a session owning one upstream leg; the table
//!   maps the client to that leg so REPLY datagrams route back to the right
//!   client via the shared public socket (`send_to(client)`), exactly like a
//!   home router's UDP NAT.
//! * **Upstream leg** — resolved once per session:
//!   - **Local** (this node serves the deployment): a connected loopback UDP
//!     socket at the container's published `/udp` host port, resolved through
//!     [`crate::mesh_raw::resolve`] — the SAME path the owner side of a mesh
//!     stream uses, so a locally-hit port and a mesh-hit port behave
//!     identically (including cold-starting a scaled-to-zero service via
//!     `Fluid::lease`, whose lease rides as the session guard).
//!   - **Mesh** (a peer owns it): `PeerPool::open_raw_to_port` with `proto:
//!     udp` — the mesh transport is stream-oriented (an iroh QUIC bi stream),
//!     so datagrams are framed one `[u32 len][payload]` frame per datagram
//!     (`hive_p2p::write_raw_datagram`/`read_raw_datagram`, byte-identical to
//!     the owner-side pump in `hive_p2p::serve_raw_target`), preserving
//!     datagram boundaries end-to-end. Closing the mesh stream is the
//!     end-of-session signal to the owner.
//! * **Idle eviction** — UDP has no FIN: a session table without eviction is
//!   an unbounded memory (and upstream-socket/mesh-stream) leak. Every session
//!   tracks last activity in EITHER direction and self-evicts after
//!   `HIVE_UDP_IDLE_SECS` (default 60s) of silence, removing its own table
//!   entry (generation-guarded so an old session can never remove its
//!   replacement). A later datagram from the same client simply creates a
//!   fresh session. `HIVE_UDP_MAX_SESSIONS` (default 4096 per port) bounds the
//!   table against spoofed-source floods — at the cap, datagrams from NEW
//!   clients are dropped (existing sessions keep serving).
//!
//! Owner-side counterpart: `mesh_raw::resolve`'s `RawProto::Udp` arm (the
//! local leg) + `hive_p2p::serve_raw_target`'s UDP pump (the mesh accept
//! side). The container publishes each declared UDP spec on its own loopback
//! host port (`-p 127.0.0.1:<host_port>:<container_port>/udp`), chosen by
//! `fluid_compute`'s cold start and surfaced per-instance via
//! `Lease::udp_host_port`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use hive_p2p::{read_raw_datagram, write_raw_datagram, RawProto, RawTarget, RAW_MAX_DATAGRAM};
use parking_lot::Mutex;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Notify};

use crate::state::CloudState;

/// How often the manager re-derives the desired listener set from the
/// deployment records (new deploys bind within this; deletions unbind).
const RECONCILE_SECS: u64 = 5;
/// Idle-check granularity inside a session (the timeout itself is
/// [`idle_timeout`]; this only bounds how late past it a session can linger).
const SWEEP_TICK: Duration = Duration::from_secs(5);
/// Client→upstream datagrams buffered per session while the upstream leg is
/// still resolving (lease cold-start, mesh dial). A full queue drops datagrams
/// — correct UDP semantics (loss, never backpressure on the shared listener).
const SESSION_QUEUE: usize = 256;

/// Session idle timeout: a session with no datagrams in either direction for
/// this long is evicted (`HIVE_UDP_IDLE_SECS`, default 60s — generous enough
/// for game-server keepalives, short enough that dead clients don't pin
/// upstream legs).
fn idle_timeout() -> Duration {
    let secs = std::env::var("HIVE_UDP_IDLE_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(60);
    Duration::from_secs(secs)
}

/// Per-port session-table ceiling (`HIVE_UDP_MAX_SESSIONS`, default 4096) — a
/// spoofed-source datagram flood must not grow the table (and its upstream
/// sockets/mesh streams) without bound.
fn max_sessions() -> usize {
    std::env::var("HIVE_UDP_MAX_SESSIONS")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(4096)
}

/// One public UDP port this node should be relaying, and where its datagrams
/// go (the stable cross-node identity: project/function/container-port — host
/// and public ports are derived from it on whichever node serves).
#[derive(Clone, Debug, PartialEq, Eq)]
struct UdpRoute {
    public_port: u16,
    project: String,
    function: String,
    container_port: u16,
}

/// A running per-port relay: its route (to detect re-allocation), a stop
/// signal, and the listener task (whose exit means "retry next reconcile").
struct RelayHandle {
    route: UdpRoute,
    stop: Arc<Notify>,
    task: tokio::task::JoinHandle<()>,
}

/// One live client session in a relay's NAT-style table: the channel into its
/// session task, and a generation stamp so only the CURRENT session for a
/// client address may remove/replace the entry (an evicted predecessor racing
/// its own cleanup can never delete its successor).
struct SessionHandle {
    tx: mpsc::Sender<Vec<u8>>,
    gen: u64,
}

type SessionTable = Arc<Mutex<HashMap<SocketAddr, SessionHandle>>>;

/// Start the UDP relay manager: reconciles the set of bound public UDP ports
/// against the deployment records every [`RECONCILE_SECS`].
pub fn spawn(cloud: Arc<CloudState>) {
    tokio::spawn(async move {
        let mut running: HashMap<u16, RelayHandle> = HashMap::new();
        loop {
            let desired = desired_routes(&cloud);
            // Stop relays whose allocation vanished or was re-pointed, and
            // forget listeners that exited (bind failure → retried next tick).
            running.retain(|port, h| {
                if h.task.is_finished() {
                    return false;
                }
                match desired.get(port) {
                    Some(r) if *r == h.route => true,
                    _ => {
                        tracing::info!(port, project = %h.route.project, "udp relay: stopping (allocation released/re-pointed)");
                        h.stop.notify_one();
                        false
                    }
                }
            });
            // Start relays for newly-learned allocations.
            for (port, route) in desired {
                if running.contains_key(&port) {
                    continue;
                }
                let stop = Arc::new(Notify::new());
                let task = tokio::spawn(run_relay(cloud.clone(), route.clone(), stop.clone()));
                running.insert(port, RelayHandle { route, stop, task });
            }
            tokio::time::sleep(Duration::from_secs(RECONCILE_SECS)).await;
        }
    });
}

/// The desired `public_port → route` set. Sources, most-authoritative first
/// (mirroring `raw_proxy::resolve_binding`): this node's own durable claim
/// registry (the allocator node knows a claim before the record is Ready or
/// has gossiped), then LOCAL Ready deployments' stamped bindings, then the
/// gossiped fleet records (`DeploymentInfo::raw_ports`) — so every edge node
/// binds every allocated UDP port, wherever the deployment actually runs.
/// Bindings for one port are identical across sources by construction (the
/// claim key is stable across redeploys); first source wins.
fn desired_routes(cloud: &Arc<CloudState>) -> HashMap<u16, UdpRoute> {
    let mut out: HashMap<u16, UdpRoute> = HashMap::new();
    for a in crate::raw_ports::udp_allocations() {
        out.entry(a.public_port).or_insert(UdpRoute {
            public_port: a.public_port,
            project: a.project,
            function: a.function,
            container_port: a.container_port,
        });
    }
    let mut add = |info: &fluid_core::DeploymentInfo| {
        if info.state != fluid_core::DeployState::Ready {
            return;
        }
        for b in &info.raw_ports {
            if b.protocol != fluid_core::ServiceProtocol::Udp {
                continue;
            }
            out.entry(b.public_port).or_insert_with(|| UdpRoute {
                public_port: b.public_port,
                project: info.project.clone(),
                function: b.function.clone(),
                container_port: b.container_port,
            });
        }
    };
    for info in cloud.gw.list() {
        add(&info);
    }
    for infos in cloud.peer_deployments.read().values() {
        for info in infos {
            add(info);
        }
    }
    out
}

/// One per-port relay: bind the public socket, demux inbound datagrams into
/// per-client sessions, and hold the session table the reply path routes by.
async fn run_relay(cloud: Arc<CloudState>, route: UdpRoute, stop: Arc<Notify>) {
    let sock = match UdpSocket::bind(("0.0.0.0", route.public_port)).await {
        Ok(s) => Arc::new(s),
        Err(e) => {
            // Something else holds the port (allocator probes at claim time,
            // but a squatter can appear later). Loud + retried every reconcile.
            tracing::warn!(port = route.public_port, error = %e, "udp relay: cannot bind public port");
            return;
        }
    };
    tracing::info!(
        port = route.public_port,
        project = %route.project,
        function = %route.function,
        container_port = route.container_port,
        "udp relay listening"
    );
    let sessions: SessionTable = Arc::new(Mutex::new(HashMap::new()));
    let cap = max_sessions();
    let mut gen: u64 = 0;
    let mut buf = vec![0u8; RAW_MAX_DATAGRAM];
    loop {
        tokio::select! {
            _ = stop.notified() => break,
            r = sock.recv_from(&mut buf) => {
                let (n, client) = match r {
                    Ok(x) => x,
                    Err(e) => {
                        tracing::debug!(port = route.public_port, error = %e, "udp relay: recv_from error");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let payload = buf[..n].to_vec();
                // Existing session? Hand the datagram to its task. `Closed`
                // means the task already exited (idle-evicted / upstream
                // failed) — replace it; `Full` is plain UDP loss.
                let existing = sessions.lock().get(&client).map(|s| (s.tx.clone(), s.gen));
                let retry = match existing {
                    Some((tx, g)) => match tx.try_send(payload) {
                        Ok(()) => None,
                        Err(mpsc::error::TrySendError::Full(_)) => None,
                        Err(mpsc::error::TrySendError::Closed(p)) => {
                            let mut m = sessions.lock();
                            if m.get(&client).map(|s| s.gen) == Some(g) {
                                m.remove(&client);
                            }
                            Some(p)
                        }
                    },
                    None => Some(payload),
                };
                if let Some(payload) = retry {
                    if sessions.lock().len() >= cap {
                        tracing::warn!(port = route.public_port, cap, "udp relay: session table full; dropping datagram from new client");
                        continue;
                    }
                    gen += 1;
                    let (tx, rx) = mpsc::channel::<Vec<u8>>(SESSION_QUEUE);
                    // First datagram can never fail: the queue is fresh.
                    let _ = tx.try_send(payload);
                    sessions.lock().insert(client, SessionHandle { tx, gen });
                    tokio::spawn(run_session(
                        cloud.clone(),
                        route.clone(),
                        sock.clone(),
                        sessions.clone(),
                        client,
                        gen,
                        rx,
                    ));
                }
            }
        }
    }
    // Shutdown: dropping every sender ends every session task (their `rx`
    // yields `None`), which drops upstream sockets/mesh streams and leases.
    sessions.lock().clear();
    tracing::info!(port = route.public_port, "udp relay stopped");
}

/// A session's upstream leg.
enum Upstream {
    /// This node serves the deployment: a resolved local target (the
    /// container's published loopback UDP address + the lease guard).
    Local(hive_p2p::RawTargetConn),
    /// A peer serves it: an admitted raw-target mesh stream (datagram-framed).
    Mesh(hive_p2p::P2pStream),
}

/// One client session: resolve the upstream leg once, pump datagrams both ways
/// until idle/closed, then remove our own table entry (generation-guarded).
async fn run_session(
    cloud: Arc<CloudState>,
    route: UdpRoute,
    public: Arc<UdpSocket>,
    sessions: SessionTable,
    client: SocketAddr,
    my_gen: u64,
    rx: mpsc::Receiver<Vec<u8>>,
) {
    tracing::debug!(port = route.public_port, %client, "udp session open");
    match open_upstream(&cloud, &route).await {
        Some(Upstream::Local(conn)) => pump_local(conn, &route, &public, client, rx).await,
        Some(Upstream::Mesh(stream)) => pump_mesh(stream, &route, public.clone(), client, rx).await,
        None => {} // logged inside; datagrams dropped, next one retries fresh
    }
    let mut m = sessions.lock();
    if m.get(&client).map(|s| s.gen) == Some(my_gen) {
        m.remove(&client);
    }
    tracing::debug!(port = route.public_port, %client, "udp session closed");
}

/// Resolve where this session's datagrams go: the container lease owner when
/// one exists (single-owner containers — never fall back elsewhere on failure,
/// that would split-brain a stateful server), else locally, else any healthy
/// peer whose gossiped records carry this binding.
async fn open_upstream(cloud: &Arc<CloudState>, route: &UdpRoute) -> Option<Upstream> {
    let target = RawTarget {
        project: route.project.clone(),
        function: route.function.clone(),
        deployment: String::new(), // owner resolves its CURRENT serving deployment
        port: route.container_port,
        proto: RawProto::Udp,
    };
    // CONTAINER single-owner routing: only the lease owner may serve (mirrors
    // `edge_pipeline`'s container_owner rule for HTTP).
    if let Some(owner) = cloud.leases.owner_of(&route.project) {
        if owner == cloud.node_name {
            return local_upstream(cloud, target, route).await;
        }
        return mesh_upstream(cloud, &owner, &target, route).await;
    }
    // No lease (function-runtime UDP service, or lease not yet established):
    // serve locally when we can, else follow the gossiped records to a peer —
    // ordered by the same nearest-first rule the TCP raw proxy uses.
    if let Some(up) = local_upstream(cloud, target.clone(), route).await {
        return Some(up);
    }
    for node in crate::raw_proxy::hosting_nodes(cloud, route.public_port) {
        if let Some(up) = mesh_upstream(cloud, &node, &target, route).await {
            return Some(up);
        }
    }
    tracing::warn!(
        port = route.public_port,
        project = %route.project,
        "udp relay: no upstream (not served locally, no reachable peer serves it)"
    );
    None
}

async fn local_upstream(
    cloud: &Arc<CloudState>,
    target: RawTarget,
    route: &UdpRoute,
) -> Option<Upstream> {
    let conn = crate::mesh_raw::resolve(cloud, target).await?;
    tracing::debug!(port = route.public_port, backend = %conn.addr, "udp relay: local leg resolved");
    Some(Upstream::Local(conn))
}

async fn mesh_upstream(
    cloud: &Arc<CloudState>,
    node: &str,
    target: &RawTarget,
    route: &UdpRoute,
) -> Option<Upstream> {
    let Some(pool) = cloud.mesh.read().clone() else {
        tracing::warn!(
            port = route.public_port,
            "udp relay: mesh transport not bound; cannot forward to owner"
        );
        return None;
    };
    let addr = cloud
        .registry
        .nodes()
        .into_iter()
        .find(|n| n.id == node)
        .and_then(|n| n.iroh_addr);
    let Some(addr) = addr else {
        tracing::warn!(
            port = route.public_port,
            node,
            "udp relay: owner node has no iroh address"
        );
        return None;
    };
    match pool.open_raw_to_port(node, &addr, target).await {
        Ok(s) => {
            tracing::debug!(port = route.public_port, node, "udp relay: mesh leg open");
            Some(Upstream::Mesh(s))
        }
        Err(e) => {
            // Connect/open timeout = strongest dead-peer signal (#H4), same
            // handling as ws_proxy / the TCP raw proxy: stop ranking the node.
            if e.downcast_ref::<hive_p2p::DeadPeerTimeout>().is_some() {
                // Through the guarded chokepoint, NOT `set_health` directly: a
                // connect/open timeout on ONE leg is a local transport fact,
                // and this observer's `healthy` flag drives DNS and placement.
                // If the peer is still gossiping it stays healthy and is only
                // marked locally cold. See health.rs.
                crate::health::demote(
                    &cloud.registry,
                    node,
                    "udp relay: connect/open timeout",
                    None,
                );
            }
            tracing::warn!(port = route.public_port, node, error = %e, "udp relay: mesh leg failed");
            None
        }
    }
}

/// Local pump: client datagrams → connected loopback socket at the container's
/// published `/udp` host port; container replies → `send_to(client)` on the
/// shared public socket. The lease guard is held for the whole session.
async fn pump_local(
    conn: hive_p2p::RawTargetConn,
    route: &UdpRoute,
    public: &UdpSocket,
    client: SocketAddr,
    mut rx: mpsc::Receiver<Vec<u8>>,
) {
    let up = match UdpSocket::bind("127.0.0.1:0").await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(port = route.public_port, error = %e, "udp relay: local socket bind failed");
            return;
        }
    };
    if let Err(e) = up.connect(&conn.addr).await {
        tracing::warn!(port = route.public_port, backend = %conn.addr, error = %e, "udp relay: local connect failed");
        return;
    }
    // Keeps the leased instance's inflight accounting alive for the session.
    let _guard = conn.guard;
    let idle = idle_timeout();
    let mut last = tokio::time::Instant::now();
    let mut tick = tokio::time::interval(SWEEP_TICK);
    let mut buf = vec![0u8; RAW_MAX_DATAGRAM];
    loop {
        tokio::select! {
            _ = tick.tick() => {
                if last.elapsed() >= idle {
                    tracing::debug!(port = route.public_port, %client, "udp session idle; evicting");
                    break;
                }
            }
            d = rx.recv() => match d {
                None => break, // relay stopped / session replaced
                Some(d) => {
                    // Send errors (ICMP port-unreachable while the container is
                    // still booting) are UDP loss, not session death.
                    let _ = up.send(&d).await;
                    last = tokio::time::Instant::now();
                }
            },
            r = up.recv(&mut buf) => match r {
                Ok(n) => {
                    let _ = public.send_to(&buf[..n], client).await;
                    last = tokio::time::Instant::now();
                }
                Err(e) => {
                    // A connected UDP socket surfaces async ICMP errors here;
                    // pause briefly so a dead backend can't spin this loop hot.
                    tracing::debug!(port = route.public_port, error = %e, "udp relay: local recv error");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            },
        }
    }
}

/// Mesh pump: client datagrams → `[u32 len][payload]` frames on the raw-target
/// stream; owner frames → `send_to(client)`. `read_raw_datagram` is not
/// cancellation-safe mid-frame, so the inbound direction runs in its own task
/// (activity reported through a shared timestamp) — mirroring the owner-side
/// pump in `hive_p2p::serve_raw_target`. Dropping the write half on exit is
/// the end-of-session signal that tears down the owner's leg + lease.
async fn pump_mesh(
    stream: hive_p2p::P2pStream,
    route: &UdpRoute,
    public: Arc<UdpSocket>,
    client: SocketAddr,
    mut rx: mpsc::Receiver<Vec<u8>>,
) {
    let (mut r, mut w) = tokio::io::split(stream);
    let last = Arc::new(AtomicU64::new(hive_core::now_ms()));
    let inbound_last = last.clone();
    let inbound_public = public.clone();
    let mut inbound = tokio::spawn(async move {
        while let Ok(Some(d)) = read_raw_datagram(&mut r).await {
            let _ = inbound_public.send_to(&d, client).await;
            inbound_last.store(hive_core::now_ms(), Ordering::Relaxed);
        }
    });
    let idle_ms = idle_timeout().as_millis() as u64;
    let mut tick = tokio::time::interval(SWEEP_TICK);
    loop {
        tokio::select! {
            _ = tick.tick() => {
                if hive_core::now_ms().saturating_sub(last.load(Ordering::Relaxed)) >= idle_ms {
                    tracing::debug!(port = route.public_port, %client, "udp mesh session idle; evicting");
                    break;
                }
            }
            _ = &mut inbound => break, // owner closed its side
            d = rx.recv() => match d {
                None => break,
                Some(d) => {
                    if write_raw_datagram(&mut w, &d).await.is_err() {
                        break; // mesh stream dead — session ends, next datagram redials
                    }
                    last.store(hive_core::now_ms(), Ordering::Relaxed);
                }
            },
        }
    }
    inbound.abort();
}
