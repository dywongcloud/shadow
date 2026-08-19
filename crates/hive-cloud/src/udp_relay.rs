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
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use hive_p2p::{read_raw_datagram, write_raw_datagram, RawProto, RawTarget, RAW_MAX_DATAGRAM};
use parking_lot::Mutex;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch, Notify};

use crate::state::CloudState;

/// How often the manager re-derives the desired listener set from the
/// deployment records (new deploys bind within this; deletions unbind).
const RECONCILE_SECS: u64 = 5;
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// Idle-check granularity inside a session (the timeout itself is
/// [`idle_timeout`]; this only bounds how late past it a session can linger).
const SWEEP_TICK: Duration = Duration::from_secs(5);
/// Client→upstream datagrams buffered per session while the upstream leg is
/// still resolving (lease cold-start, mesh dial). A full queue drops datagrams
/// — correct UDP semantics (loss, never backpressure on the shared listener).
const SESSION_QUEUE: usize = 256;

const MAX_IDLE_SECS: u64 = 24 * 60 * 60;
static IDLE_TIMEOUT: OnceLock<Duration> = OnceLock::new();

/// Session idle timeout. Parse once because process environment is immutable,
/// and clamp hostile/mistyped values so `Instant` deadline arithmetic cannot
/// overflow into an immediate hot loop.
fn idle_timeout() -> Duration {
    *IDLE_TIMEOUT.get_or_init(|| {
        let configured = std::env::var("HIVE_UDP_IDLE_SECS")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|&v| v > 0)
            .unwrap_or(60);
        let secs = configured.min(MAX_IDLE_SECS);
        if secs != configured {
            tracing::warn!(
                configured_secs = configured,
                max_secs = MAX_IDLE_SECS,
                "HIVE_UDP_IDLE_SECS exceeds the safe maximum; clamping"
            );
        }
        Duration::from_secs(secs)
    })
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

enum RelayState {
    Running(Arc<Notify>),
    Stopping,
}

/// A running or draining per-port relay: its route (to detect re-allocation),
/// lifecycle state, and listener task (whose exit means "retry next reconcile").
struct RelayHandle {
    route: UdpRoute,
    state: RelayState,
    task: tokio::task::JoinHandle<()>,
}

/// A panic/cancellation of the detached manager must not detach every relay and
/// lose their stop handles. Aborting them drops the bound sockets and each
/// relay's `JoinSet`, which in turn aborts its sessions.
struct RelayHandles(HashMap<u16, RelayHandle>);

impl std::ops::Deref for RelayHandles {
    type Target = HashMap<u16, RelayHandle>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for RelayHandles {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for RelayHandles {
    fn drop(&mut self) {
        for (_, handle) in self.0.drain() {
            if let RelayState::Running(stop) = handle.state {
                stop.notify_one();
            }
            handle.task.abort();
        }
    }
}

/// One live client session in a relay's NAT-style table: the channel into its
/// session task, and a generation stamp so only the CURRENT session for a
/// client address may remove/replace the entry (an evicted predecessor racing
/// its own cleanup can never delete its successor).
struct SessionHandle {
    tx: mpsc::Sender<Vec<u8>>,
    stop: watch::Sender<bool>,
    activity: watch::Sender<tokio::time::Instant>,
    gen: u64,
}

/// Tokio detaches a `JoinHandle` on drop. Session cancellation or panic must
/// instead cancel the non-cancel-safe mesh reader so it cannot retain the old
/// public socket and stream behind a completed session task.
struct AbortOnDropTask<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDropTask<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

type SessionTable = Arc<Mutex<HashMap<SocketAddr, SessionHandle>>>;

struct SessionRegistration {
    sessions: SessionTable,
    client: SocketAddr,
    generation: u64,
}

impl Drop for SessionRegistration {
    fn drop(&mut self) {
        let mut sessions = self.sessions.lock();
        if sessions.get(&self.client).map(|session| session.gen) == Some(self.generation) {
            sessions.remove(&self.client);
        }
    }
}

/// Start the UDP relay manager: reconciles the set of bound public UDP ports
/// against the deployment records every [`RECONCILE_SECS`]. Supervised like
/// every other periodic loop on the node: a panic in the reconcile body
/// otherwise killed all UDP relaying silently until process restart — and
/// `RelayHandles::drop` aborts every relay on unwind, which is exactly the
/// fail-closed posture that makes a supervised respawn clean (adversarial
/// finding).
pub fn spawn(cloud: Arc<CloudState>) {
    crate::supervise::spawn_supervised("udp-relay", move || {
        let cloud = cloud.clone();
        async move {
            crate::supervise::beat("udp-relay");
            relay_manager(cloud).await;
        }
    });
}

async fn relay_manager(cloud: Arc<CloudState>) {
        let mut running = RelayHandles(HashMap::new());
        loop {
            let desired = desired_routes(&cloud);
            let mut blocked_ports = Vec::new();
            let finished: Vec<u16> = running
                .iter()
                .filter(|(_, h)| h.task.is_finished())
                .map(|(port, _)| *port)
                .collect();
            for port in finished {
                if let Some(h) = running.remove(&port) {
                    if let Err(e) = h.task.await {
                        blocked_ports.push(port);
                        tracing::warn!(port, error = %e, "udp relay: task failed; delaying rebind");
                    }
                }
            }

            let stale: Vec<u16> = running
                .iter()
                .filter(|(port, h)| {
                    matches!(&h.state, RelayState::Running(_))
                        && desired.get(port).map(|r| r != &h.route).unwrap_or(true)
                })
                .map(|(port, _)| *port)
                .collect();
            for port in &stale {
                if let Some(h) = running.get_mut(port) {
                    tracing::info!(port, project = %h.route.project, "udp relay: stopping (allocation released/re-pointed)");
                    if let RelayState::Running(stop) =
                        std::mem::replace(&mut h.state, RelayState::Stopping)
                    {
                        stop.notify_one();
                    }
                }
            }
            let teardown_deadline = tokio::time::Instant::now() + TEARDOWN_TIMEOUT;
            for port in stale {
                let Some(mut h) = running.remove(&port) else {
                    continue;
                };
                match tokio::time::timeout_at(teardown_deadline, &mut h.task).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        blocked_ports.push(port);
                        tracing::warn!(port, error = %e, "udp relay: teardown task failed; delaying rebind");
                    }
                    Err(_) => {
                        tracing::warn!(
                            port,
                            "udp relay: teardown timed out; replacement remains unbound"
                        );
                        running.insert(port, h);
                    }
                }
            }

            // Teardown can consume the whole deadline. Never bind from the
            // pre-teardown snapshot: the claim may have moved again while the
            // old socket and sessions were draining.
            let desired = desired_routes(&cloud);
            for (port, route) in desired {
                if running.contains_key(&port) || blocked_ports.contains(&port) {
                    continue;
                }
                let sock = match UdpSocket::bind(("0.0.0.0", port)).await {
                    Ok(sock) => sock,
                    Err(e) => {
                        tracing::warn!(port, error = %e, "udp relay: cannot bind public port");
                        continue;
                    }
                };
                if desired_routes(&cloud).get(&port) != Some(&route) {
                    tracing::info!(
                        port,
                        "udp relay: allocation changed during bind; dropping obsolete socket"
                    );
                    continue;
                }
                let sock = Arc::new(sock);
                let stop = Arc::new(Notify::new());
                let task =
                    tokio::spawn(run_relay(cloud.clone(), route.clone(), sock, stop.clone()));
                running.insert(
                    port,
                    RelayHandle {
                        route,
                        state: RelayState::Running(stop),
                        task,
                    },
                );
            }
            tokio::time::sleep(Duration::from_secs(RECONCILE_SECS)).await;
        }
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

/// One per-port relay: demux datagrams from an already-bound public socket into
/// per-client sessions, and hold the session table the reply path routes by.
async fn run_relay(
    cloud: Arc<CloudState>,
    route: UdpRoute,
    sock: Arc<UdpSocket>,
    stop: Arc<Notify>,
) {
    tracing::info!(
        port = route.public_port,
        project = %route.project,
        function = %route.function,
        container_port = route.container_port,
        "udp relay listening"
    );
    let sessions: SessionTable = Arc::new(Mutex::new(HashMap::new()));
    let mut session_tasks = tokio::task::JoinSet::new();
    let cap = max_sessions();
    let mut gen: u64 = 0;
    let mut buf = vec![0u8; RAW_MAX_DATAGRAM];
    loop {
        tokio::select! {
            _ = stop.notified() => break,
            Some(result) = session_tasks.join_next(), if !session_tasks.is_empty() => {
                if let Err(e) = result {
                    tracing::warn!(port = route.public_port, error = %e, "udp relay: session task failed");
                }
            }
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
                let existing = sessions.lock().get(&client).map(|session| {
                    (
                        session.tx.clone(),
                        session.activity.clone(),
                        session.gen,
                    )
                });
                let retry = match existing {
                    Some((tx, activity, generation)) => {
                        activity.send_replace(tokio::time::Instant::now());
                        match tx.try_send(payload) {
                            Ok(()) => None,
                            Err(mpsc::error::TrySendError::Full(_)) => None,
                            Err(mpsc::error::TrySendError::Closed(payload)) => {
                                let mut sessions = sessions.lock();
                                if sessions.get(&client).map(|session| session.gen)
                                    == Some(generation)
                                {
                                    sessions.remove(&client);
                                }
                                Some(payload)
                            }
                        }
                    }
                    None => Some(payload),
                };
                if let Some(payload) = retry {
                    if sessions.lock().len() >= cap {
                        tracing::warn!(port = route.public_port, cap, "udp relay: session table full; dropping datagram from new client");
                        continue;
                    }
                    gen += 1;
                    let (tx, rx) = mpsc::channel::<Vec<u8>>(SESSION_QUEUE);
                    let (session_stop, stop_rx) = watch::channel(false);
                    let (activity, activity_rx) = watch::channel(tokio::time::Instant::now());
                    // First datagram can never fail: the queue is fresh.
                    let _ = tx.try_send(payload);
                    sessions.lock().insert(
                        client,
                        SessionHandle {
                            tx,
                            stop: session_stop,
                            activity,
                            gen,
                        },
                    );
                    session_tasks.spawn(run_session(
                        cloud.clone(),
                        route.clone(),
                        sock.clone(),
                        sessions.clone(),
                        client,
                        gen,
                        stop_rx,
                        activity_rx,
                        rx,
                    ));
                }
            }
        }
    }
    let stops: Vec<_> = sessions
        .lock()
        .drain()
        .map(|(_, session)| session.stop)
        .collect();
    for stop in stops {
        let _ = stop.send(true);
    }
    while let Some(result) = session_tasks.join_next().await {
        if let Err(e) = result {
            tracing::warn!(port = route.public_port, error = %e, "udp relay: session teardown failed");
        }
    }
    // A panicking session can only abort its nested mesh reader from Drop; wait
    // until that cancellation has actually released every socket clone before
    // telling the manager it is safe to bind a replacement.
    while Arc::strong_count(&sock) > 1 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
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

async fn session_stopped(stop: &mut watch::Receiver<bool>) {
    if !*stop.borrow_and_update() {
        let _ = stop.changed().await;
    }
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
    mut stop: watch::Receiver<bool>,
    mut activity: watch::Receiver<tokio::time::Instant>,
    rx: mpsc::Receiver<Vec<u8>>,
) {
    tracing::debug!(port = route.public_port, %client, "udp session open");
    let _registration = SessionRegistration {
        sessions,
        client,
        generation: my_gen,
    };
    let open = open_upstream(&cloud, &route);
    tokio::pin!(open);
    let idle = idle_timeout();
    let upstream = loop {
        let last = *activity.borrow_and_update();
        let deadline = last.checked_add(idle).unwrap_or(last);
        tokio::select! {
            _ = session_stopped(&mut stop) => break None,
            changed = activity.changed() => {
                if changed.is_err() {
                    break None;
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                if activity.borrow().elapsed() >= idle {
                    tracing::debug!(port = route.public_port, %client, "udp session idle while resolving upstream; evicting");
                    break None;
                }
            }
            upstream = &mut open => break upstream,
        }
    };
    match upstream {
        Some(Upstream::Local(conn)) => {
            pump_local(conn, &route, &public, client, &mut stop, rx).await
        }
        Some(Upstream::Mesh(stream)) => {
            pump_mesh(stream, &route, public.clone(), client, &mut stop, rx).await
        }
        None => {}
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
    stop: &mut watch::Receiver<bool>,
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
    'session: loop {
        tokio::select! {
            _ = session_stopped(stop) => break,
            _ = tick.tick() => {
                if last.elapsed() >= idle {
                    tracing::debug!(port = route.public_port, %client, "udp session idle; evicting");
                    break;
                }
            }
            d = rx.recv() => match d {
                None => break, // relay stopped / session replaced
                Some(d) => {
                    // Receiving the datagram is activity even if the best-effort
                    // UDP send is flow-control delayed or reports ICMP failure.
                    last = tokio::time::Instant::now();
                    let deadline = last.checked_add(idle).unwrap_or(last);
                    tokio::select! {
                        _ = session_stopped(stop) => break 'session,
                        _ = tokio::time::sleep_until(deadline) => {
                            tracing::debug!(port = route.public_port, %client, "udp local send stalled through idle deadline; evicting");
                            break 'session;
                        }
                        _ = up.send(&d) => {}
                    }
                }
            },
            r = up.recv(&mut buf) => match r {
                Ok(n) => {
                    last = tokio::time::Instant::now();
                    let deadline = last.checked_add(idle).unwrap_or(last);
                    tokio::select! {
                        _ = session_stopped(stop) => break 'session,
                        _ = tokio::time::sleep_until(deadline) => {
                            tracing::debug!(port = route.public_port, %client, "udp public send stalled through idle deadline; evicting");
                            break 'session;
                        }
                        _ = public.send_to(&buf[..n], client) => {}
                    }
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
    stop: &mut watch::Receiver<bool>,
    mut rx: mpsc::Receiver<Vec<u8>>,
) {
    let (mut r, mut w) = tokio::io::split(stream);
    let (activity, mut observed_activity) = watch::channel(tokio::time::Instant::now());
    let inbound_activity = activity.clone();
    let inbound_public = public.clone();
    let mut inbound = AbortOnDropTask(tokio::spawn(async move {
        while let Ok(Some(d)) = read_raw_datagram(&mut r).await {
            inbound_activity.send_replace(tokio::time::Instant::now());
            let _ = inbound_public.send_to(&d, client).await;
        }
    }));
    let idle = idle_timeout();
    let mut inbound_done = false;
    'session: loop {
        let last = *observed_activity.borrow_and_update();
        let deadline = last.checked_add(idle).unwrap_or(last);
        tokio::select! {
            _ = session_stopped(stop) => break,
            _ = tokio::time::sleep_until(deadline) => {
                if observed_activity.borrow().elapsed() >= idle {
                    tracing::debug!(port = route.public_port, %client, "udp mesh session idle; evicting");
                    break;
                }
            }
            joined = &mut inbound.0 => {
                inbound_done = true;
                if let Err(error) = joined {
                    tracing::warn!(port = route.public_port, %client, %error, "udp relay: mesh reader failed");
                }
                break;
            }
            d = rx.recv() => match d {
                None => break,
                Some(d) => {
                    // Keep one ordered frame in flight, but retain ownership of
                    // the future across activity notifications. Stop, reader
                    // exit, and the idle deadline can terminate a flow-control-
                    // stalled write without waiting forever.
                    activity.send_replace(tokio::time::Instant::now());
                    let write = write_raw_datagram(&mut w, &d);
                    tokio::pin!(write);
                    loop {
                        let last = *observed_activity.borrow_and_update();
                        let deadline = last.checked_add(idle).unwrap_or(last);
                        tokio::select! {
                            _ = session_stopped(stop) => break 'session,
                            joined = &mut inbound.0 => {
                                inbound_done = true;
                                if let Err(error) = joined {
                                    tracing::warn!(port = route.public_port, %client, %error, "udp relay: mesh reader failed");
                                }
                                break 'session;
                            }
                            changed = observed_activity.changed() => {
                                if changed.is_err() {
                                    break 'session;
                                }
                            }
                            _ = tokio::time::sleep_until(deadline) => {
                                if observed_activity.borrow().elapsed() >= idle {
                                    tracing::debug!(port = route.public_port, %client, "udp mesh session idle during write; evicting");
                                    break 'session;
                                }
                            }
                            result = &mut write => {
                                if result.is_err() {
                                    break 'session;
                                }
                                activity.send_replace(tokio::time::Instant::now());
                                break;
                            }
                        }
                    }
                }
            },
        }
    }
    if !inbound_done {
        inbound.0.abort();
        let _ = (&mut inbound.0).await;
    }
}
