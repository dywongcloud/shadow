//! Generic public raw-TCP ingress proxy — the data plane for non-HTTP
//! deployments (Postgres wire, Minecraft, gRPC, …).
//!
//! Generalizes `db_gateway.rs`'s pattern (two HARDCODED listeners on
//! :5432/:6379, SNI-routed, `tokio::io::copy_bidirectional` to a local
//! container) into an ALLOCATED-PORT-driven one: every public port the
//! raw-port allocator has stamped onto a deployment ([`crate::raw_ports`],
//! `PortSpec::public_port`) gets its own listener on EVERY mesh node, and the
//! deployment is reachable through any of them:
//!
//! * **Which ports to listen on** — a reconcile loop diffs the union of this
//!   node's own Ready deployments' stamped bindings and every gossiped peer
//!   deployment's bindings (`DeploymentInfo::raw_ports` rides the existing
//!   `/v1/fleet-deployments` gossip into `peer_deployments`) against the live
//!   listener set, binding/closing as allocations come and go. No TLS/SNI
//!   leg: unlike the DB gateway there is one port per service, so the PORT is
//!   the routing key — the client speaks its native protocol from byte one.
//! * **Local vs remote** — same single-owner routing the HTTP edge uses
//!   (`edge.rs`): a live container LEASE (`cloud.leases.owner_of(project)`,
//!   gossiped fleet-wide) names the one node allowed to serve a stateful
//!   container; absent a lease, local resolution ([`crate::mesh_raw::resolve`])
//!   decides — it succeeds exactly when a local Ready deployment publishes the
//!   binding — else the gossiped `peer_deployments` map names the hosting
//!   node(s).
//! * **Local splice** — [`crate::mesh_raw::resolve`] (shared with the mesh
//!   accept side, so local and mesh-inbound connections take ONE resolution
//!   path) leases a Fluid instance whose cell endpoint fronts the container
//!   with `TunnelServer::serve_raw` — a pure byte splice — then
//!   `copy_bidirectional` client ↔ instance, holding the lease guard for the
//!   life of the connection so inflight accounting keeps the instance alive.
//! * **Remote splice** — open a raw mesh stream to the owner with the
//!   `STREAM_RAW_TARGET` handshake (`PeerPool::open_raw_to_port`: a framed
//!   `RawTarget{project, function, container_port, proto}` naming the service
//!   — nothing HTTP-shaped precedes the splice, unlike `ws_proxy`'s
//!   upgrade-replay over `STREAM_RAW`), then `copy_bidirectional` client ↔
//!   mesh stream; the owner's end resolves through the same
//!   `mesh_raw::resolver` and splices into its local instance.
//!
//! TCP transports only (`tcp` + `grpc` — gRPC is an opaque TCP byte stream at
//! this layer). UDP bindings are deliberately NOT bound here: datagram relay
//! (socket semantics, `[u32 len]`-framed mesh datagrams) is the UDP relay
//! layer's, which shares the same allocations and mesh primitive.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use fluid_core::{DeployState, ServiceProtocol};
use hive_p2p::{RawProto, RawTarget};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};

use crate::state::CloudState;

/// Protocols this proxy carries: anything whose public transport is a TCP byte
/// stream. `Udp` needs the datagram relay. `Http`-family is here because of
/// compose PUBLISH requests (`ports: ["9001:9001"]` → a stamped public port on
/// an Http spec): the published port is served as a plain-TCP passthrough —
/// exactly what `docker compose up` publishes, plain HTTP with no TLS; the
/// shared 443 gateway remains the platform's TLS surface. An Http spec WITHOUT
/// a publish request still never gets a stamped public port, so nothing
/// reaches this proxy for it.
fn tcp_transport(p: ServiceProtocol) -> bool {
    matches!(
        p,
        ServiceProtocol::Tcp | ServiceProtocol::Grpc | ServiceProtocol::Http
    )
}

/// What a public port maps to — the flattened allocation identity, from
/// whichever source resolves first (this node's durable claim registry, a
/// local deployment's stamped bindings, or a gossiped peer deployment's).
#[derive(Clone, Debug)]
struct Target {
    project: String,
    function: String,
    container_port: u16,
    protocol: ServiceProtocol,
}

impl Target {
    /// The mesh/local-resolution handshake for this allocation. `deployment`
    /// stays empty: the RESOLVING node picks the project's current serving
    /// deployment from its own records (always fresher across a redeploy than
    /// anything an entry node could pin).
    fn raw_target(&self) -> RawTarget {
        RawTarget {
            project: self.project.clone(),
            function: self.function.clone(),
            deployment: String::new(),
            port: self.container_port,
            proto: RawProto::Tcp,
        }
    }
}

/// Spawn the listener reconcile loop. Runs on every node unconditionally
/// (like the HTTP edge, any node is an entry point for any deployment);
/// listeners exist only while some fleet deployment holds the allocation, so
/// a node with no raw deployments in sight binds nothing.
pub fn spawn(cloud: Arc<CloudState>) {
    tokio::spawn(async move {
        let mut listeners: HashMap<u16, tokio::task::JoinHandle<()>> = HashMap::new();
        loop {
            let desired = desired_ports(&cloud);
            // Close listeners whose allocation vanished fleet-wide (project
            // deleted / last deployment removed → `release_raw_ports`).
            // Aborting the accept task drops the socket; connections already
            // spliced run in their own tasks and drain naturally.
            let stale: Vec<u16> = listeners
                .keys()
                .filter(|p| !desired.contains(p))
                .copied()
                .collect();
            for p in stale {
                if let Some(h) = listeners.remove(&p) {
                    h.abort();
                    tracing::info!(port = p, "raw-proxy: listener closed (allocation released)");
                }
            }
            // A finished handle means the accept loop died (listener error) —
            // forget it so the port is re-bound below.
            listeners.retain(|_, h| !h.is_finished());
            for &p in &desired {
                if listeners.contains_key(&p) {
                    continue;
                }
                // Bind HERE (not inside the spawned task) so a squatted port is
                // observed and retried on the next reconcile tick instead of
                // leaving a dead handle behind.
                match TcpListener::bind(("0.0.0.0", p)).await {
                    Ok(l) => {
                        tracing::info!(port = p, "raw-proxy: public raw TCP listener up");
                        listeners.insert(p, tokio::spawn(accept_loop(cloud.clone(), p, l)));
                    }
                    Err(e) => {
                        tracing::warn!(port = p, error = %e, "raw-proxy: cannot bind allocated port (will retry)");
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    });
}

/// Every public port some Ready deployment (local or gossiped) holds a
/// TCP-transport binding for.
fn desired_ports(cloud: &Arc<CloudState>) -> BTreeSet<u16> {
    let mut out = BTreeSet::new();
    let mut add = |d: &fluid_core::DeploymentInfo| {
        if d.state != DeployState::Ready {
            return;
        }
        for b in &d.raw_ports {
            if tcp_transport(b.protocol) {
                out.insert(b.public_port);
            }
        }
    };
    for d in cloud.gw.list() {
        add(&d);
    }
    for deps in cloud.peer_deployments.read().values() {
        for d in deps {
            add(d);
        }
    }
    out
}

async fn accept_loop(cloud: Arc<CloudState>, port: u16, listener: TcpListener) {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!(port, error = %e, "raw-proxy: accept error");
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            }
        };
        let cloud = cloud.clone();
        tokio::spawn(async move {
            // WARN, not debug: every reachable error here is a genuine
            // resolution/splice failure (no deployment holds the port, a
            // protocol mismatch, a dead local instance, or every mesh
            // candidate refusing/timing out) — never a normal client
            // disconnect, which `copy_bidirectional` completes as `Ok`. A
            // fleet-wide misroute (e.g. the F1/F3 classes) would be invisible
            // at the default log level otherwise.
            if let Err(e) = handle(cloud, port, stream).await {
                tracing::warn!(port, %peer, error = %e, "raw-proxy: connection failed");
            }
        });
    }
}

/// One inbound public connection: map the port to its deployment, decide
/// local-vs-remote, splice.
async fn handle(cloud: Arc<CloudState>, port: u16, mut client: TcpStream) -> anyhow::Result<()> {
    let target = resolve_binding(&cloud, port)
        .ok_or_else(|| anyhow::anyhow!("no deployment holds public raw port {port}"))?;
    anyhow::ensure!(
        tcp_transport(target.protocol),
        "port {port} is a {} binding — not served by the TCP raw proxy",
        target.protocol
    );

    // Single-owner routing, exactly as the HTTP edge does it (`edge.rs`): a
    // live container lease names the ONE node allowed to serve this project's
    // container — never serve locally past someone else's lease, never proxy
    // away from our own.
    let lease_owner = cloud
        .leases
        .owner_of(&target.project)
        .filter(|o| o != &cloud.node_name);

    if lease_owner.is_none() {
        // We are the lease owner, or no live lease exists: local resolution
        // (`mesh_raw::resolve` — the SAME path an inbound mesh stream takes)
        // succeeds exactly when a local Ready deployment publishes this
        // binding, leasing the instance for the splice's lifetime.
        if let Some(conn) = crate::mesh_raw::resolve(&cloud, target.raw_target()).await {
            tracing::debug!(port, project = %target.project, function = %target.function, addr = %conn.addr, "raw-proxy: local splice");
            let mut backend = TcpStream::connect(&conn.addr)
                .await
                .map_err(|e| anyhow::anyhow!("local instance {} unreachable: {e}", conn.addr))?;
            // The db_gateway.rs:139 move: client ↔ local instance, bytes only,
            // until either side closes. `conn.guard` (the Fluid lease) drops
            // AFTER the splice ends.
            tokio::io::copy_bidirectional(&mut client, &mut backend).await?;
            drop(conn.guard);
            return Ok(());
        }
    }
    splice_remote(&cloud, port, &target, lease_owner, &mut client).await
}

/// `public_port` → allocation. Sources, most-authoritative first: this node's
/// own durable claim registry (the allocator node knows even before the record
/// gossips), then local deployments' stamped bindings, then gossiped peers'.
fn resolve_binding(cloud: &Arc<CloudState>, port: u16) -> Option<Target> {
    if let Some(a) = crate::raw_ports::lookup(port) {
        return Some(Target {
            project: a.project,
            function: a.function,
            container_port: a.container_port,
            protocol: a.protocol,
        });
    }
    let from = |d: &fluid_core::DeploymentInfo| {
        d.raw_ports
            .iter()
            .find(|b| b.public_port == port)
            .map(|b| Target {
                project: d.project.clone(),
                function: b.function.clone(),
                container_port: b.container_port,
                protocol: b.protocol,
            })
    };
    if let Some(t) = cloud.gw.list().iter().find_map(&from) {
        return Some(t);
    }
    cloud
        .peer_deployments
        .read()
        .values()
        .flatten()
        .find_map(&from)
}

/// Cross-node hop: open a raw mesh stream to the owner (the `ws_proxy`
/// pattern, minus the HTTP-upgrade replay — the `RawTarget` handshake names
/// the service instead) and splice the public client onto it. Candidates are
/// tried in order; `open_raw_to_port` only returns once the owner has
/// ADMITTED the target (status byte), and no client bytes are consumed before
/// then, so failover to the next candidate is always safe.
async fn splice_remote<S>(
    cloud: &Arc<CloudState>,
    port: u16,
    target: &Target,
    lease_owner: Option<String>,
    client: &mut S,
) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mesh = cloud.mesh.read().clone().ok_or_else(|| {
        anyhow::anyhow!("P2P mesh not enabled — cannot reach remote raw deployment")
    })?;
    // A live lease is authoritative and EXCLUSIVE (stateful single-writer):
    // only the owner may serve, so it is the only candidate. Without one,
    // any healthy node that gossiped a Ready deployment carrying this port.
    let cands: Vec<String> = match lease_owner {
        Some(owner) => vec![owner],
        None => hosting_nodes(cloud, port),
    };
    let nodes = cloud.registry.nodes();
    let hs = target.raw_target();
    let mut last_err = anyhow::anyhow!("no healthy node serves raw port {port}");
    for cand in &cands {
        let Some(node) = nodes.iter().find(|n| &n.id == cand && n.healthy) else {
            continue;
        };
        let Some(addr_json) = node.iroh_addr.clone() else {
            continue;
        };
        match mesh.open_raw_to_port(cand, &addr_json, &hs).await {
            Ok(mut raw) => {
                tracing::debug!(port, owner = %cand, project = %target.project, "raw-proxy: mesh splice");
                tokio::io::copy_bidirectional(client, &mut raw).await?;
                return Ok(());
            }
            Err(e) => {
                // Connect/open timeout = strongest dead-peer signal (#H4),
                // same handling as ws_proxy: stop ranking the node.
                if e.downcast_ref::<hive_p2p::DeadPeerTimeout>().is_some() {
                    crate::health::demote(
                        &cloud.registry,
                        cand,
                        "raw-proxy: connect/open timeout",
                        None,
                    );
                }
                last_err = e;
            }
        }
    }
    Err(last_err)
}

/// Healthy nodes that gossiped a Ready deployment carrying `port`, nearest
/// first (same region, then latency) — the raw-TCP analogue of the edge's
/// peer-route candidate ordering. `pub(crate)`: the UDP relay
/// (`crate::udp_relay`) orders its no-lease mesh candidates with the same
/// rule, so TCP and UDP ingress pick peers identically.
pub(crate) fn hosting_nodes(cloud: &Arc<CloudState>, port: u16) -> Vec<String> {
    let holders: BTreeSet<String> = cloud
        .peer_deployments
        .read()
        .iter()
        .filter(|(_, deps)| {
            deps.iter().any(|d| {
                d.state == DeployState::Ready && d.raw_ports.iter().any(|b| b.public_port == port)
            })
        })
        .map(|(node, _)| node.clone())
        .collect();
    let mut nodes: Vec<_> = cloud
        .registry
        .nodes()
        .into_iter()
        .filter(|n| n.healthy && holders.contains(&n.id))
        .collect();
    nodes.sort_by_key(|n| (n.region != cloud.region, n.latency_ms));
    nodes.into_iter().map(|n| n.id).collect()
}
