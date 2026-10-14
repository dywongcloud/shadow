//! Generic public raw-TCP ingress proxy: the data plane for non-HTTP deployments (Postgres wire,
//! Minecraft, gRPC, ...).
//!
//! It generalizes `db_gateway.rs`'s pattern (two HARDCODED listeners on :5432/:6379, SNI-routed,
//! `tokio::io::copy_bidirectional` to a local container) into an ALLOCATED-PORT-driven one: every
//! public port the raw-port allocator has stamped onto a deployment ([`crate::raw_ports`],
//! `PortSpec::public_port`) gets its own listener on EVERY mesh node, and the deployment is
//! reachable through any of them:
//!
//! * **Which ports to listen on**: a reconcile loop diffs the union of this node's own Ready
//!   deployments' stamped bindings and every gossiped peer deployment's bindings
//!   (`DeploymentInfo::raw_ports` rides the existing `/v1/fleet-deployments` gossip into
//!   `peer_deployments`) against the live listener set, binding and closing as allocations come and
//!   go. There is no SNI routing: unlike the DB gateway there is one port per service, so the PORT
//!   is the routing key and the client speaks its native protocol from byte one.
//! * **Local vs remote**: the same single-owner routing the HTTP edge uses (`edge.rs`). A live
//!   container LEASE (`cloud.leases.owner_of(project)`, gossiped fleet-wide) names the one node
//!   allowed to serve a stateful container; unless a peer holds it, local resolution
//!   ([`crate::mesh_raw::resolve`]) decides first, succeeding exactly when a local Ready deployment
//!   publishes the binding; else the gossiped `peer_deployments` map names the hosting node(s).
//! * **Local splice**: [`crate::mesh_raw::resolve`] (shared with the mesh accept side, so local and
//!   mesh-inbound connections take ONE resolution path) leases a Fluid instance whose cell endpoint
//!   fronts the container with `TunnelServer::serve_raw`, a pure byte splice, then
//!   `copy_bidirectional` client <-> instance, holding the lease guard for the life of the
//!   connection so inflight accounting keeps the instance alive.
//! * **Remote splice**: opens a raw mesh stream to the owner with the `STREAM_RAW_TARGET` handshake
//!   (`PeerPool::open_raw_to_port`: a framed `RawTarget{project, function, port, proto}`, `port`
//!   being the CONTAINER port, naming the service; nothing HTTP-shaped precedes the splice, unlike
//!   `ws_proxy`'s upgrade replay over `STREAM_RAW`), then `copy_bidirectional` client <-> mesh
//!   stream; the owner's end resolves through the same `mesh_raw::resolver` and splices into its
//!   local instance.
//!
//! Transports: `tcp` and `grpc` (an opaque TCP byte stream at this layer) are pure passthrough. An
//! `http` port with a PUBLISH request (`PortSpec::preferred_public_port`, the shape of a compose
//! `ports: ["9001:9001"]`) also lands here and terminates TLS when the client opens with a TLS
//! handshake ([`serve_maybe_tls`]). UDP bindings are deliberately NOT bound here: datagram relay
//! (socket semantics, `[u32 len]`-framed mesh datagrams) is the UDP relay layer's, which shares the
//! same allocations and mesh primitive.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use fluid_core::{DeployState, ServiceProtocol};
use hive_p2p::{RawProto, RawTarget};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};

use crate::state::CloudState;

/// Protocols this proxy carries: anything whose public transport is a TCP byte stream. `Udp` needs
/// the datagram relay. The `Http` family is here because of PUBLISH requests
/// (`PortSpec::preferred_public_port`, the shape of a compose `ports: ["9001:9001"]`, gives an Http
/// spec a stamped public port): the published port serves plain HTTP as a TCP passthrough, exactly
/// what `docker compose up` publishes, and terminates TLS for a TLS client, while the shared 443
/// gateway remains the platform's TLS surface for everything else. An Http spec WITHOUT a publish
/// request never gets a stamped public port, so nothing reaches this proxy for it.
fn tcp_transport(p: ServiceProtocol) -> bool {
    matches!(
        p,
        ServiceProtocol::Tcp | ServiceProtocol::Grpc | ServiceProtocol::Http
    )
}

/// What a public port maps to: the flattened allocation identity, from whichever source resolves
/// first (this node's durable claim registry, a local deployment's stamped bindings, or a gossiped
/// peer deployment's).
#[derive(Clone, Debug)]
struct Target {
    project: String,
    function: String,
    container_port: u16,
    protocol: ServiceProtocol,
}

impl Target {
    /// The mesh/local-resolution handshake for this allocation. `deployment` stays empty: the
    /// RESOLVING node picks the project's current serving deployment from its own records, which
    /// are always fresher across a redeploy than anything an entry node could pin.
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

/// Spawns the listener reconcile loop. Runs on every node unconditionally (like the HTTP edge, any
/// node is an entry point for any deployment); listeners exist only while some fleet deployment
/// holds the allocation, so a node with no raw deployments in sight binds nothing. Each pass closes
/// the listeners whose allocation vanished fleet-wide (project deleted or last deployment removed,
/// `release_raw_ports`): aborting the accept task drops the socket, and connections already spliced
/// run in their own tasks and drain naturally. A finished handle means the accept task died, so it
/// is forgotten and the port re-bound. Binding happens HERE, not inside the spawned task, so a
/// squatted port is observed and retried on the next tick instead of leaving a dead handle behind.
pub fn spawn(cloud: Arc<CloudState>) {
    tokio::spawn(async move {
        let mut listeners: HashMap<u16, tokio::task::JoinHandle<()>> = HashMap::new();
        loop {
            let desired = desired_ports(&cloud);
            let stale: Vec<u16> = listeners
                .keys()
                .filter(|p| !desired.contains_key(p))
                .copied()
                .collect();
            for p in stale {
                if let Some(h) = listeners.remove(&p) {
                    h.abort();
                    tracing::info!(port = p, "raw-proxy: listener closed (allocation released)");
                }
            }
            listeners.retain(|_, h| !h.is_finished());
            for (&p, &proto) in &desired {
                if listeners.contains_key(&p) {
                    continue;
                }
                let tls = proto == ServiceProtocol::Http;
                match TcpListener::bind(("0.0.0.0", p)).await {
                    Ok(l) => {
                        tracing::info!(port = p, tls, "raw-proxy: public raw TCP listener up");
                        listeners.insert(p, tokio::spawn(accept_loop(cloud.clone(), p, l, tls)));
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

/// Every public port some Ready deployment (local or gossiped) holds a TCP-transport binding for,
/// with the binding's protocol: Http-protocol (published) ports get TLS termination at accept time,
/// raw Tcp/Grpc stay pure passthrough. First writer wins a (theoretical) protocol collision; public
/// ports are fleet-unique by allocation.
fn desired_ports(cloud: &Arc<CloudState>) -> BTreeMap<u16, ServiceProtocol> {
    let mut out = BTreeMap::new();
    let mut add = |d: &fluid_core::DeploymentInfo| {
        if d.state != DeployState::Ready {
            return;
        }
        for b in &d.raw_ports {
            if tcp_transport(b.protocol) {
                out.entry(b.public_port).or_insert(b.protocol);
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

/// Accept loop of one public port. A TLS acceptor is SCOPED to the hostnames of the project that
/// owns the port: the fleet-wide SNI resolver holds every platform certificate
/// (api./admin./dashboard) and every other tenant's custom domain, so handing it to a
/// tenant-controlled listener would let that port present any of them to a client that asked by
/// SNI, a credential-harvesting surface on a port the tenant chose. It is built once, when the
/// accept loop starts, from the project's hostnames at that moment (a hostname added later is not
/// served until the listener is re-created); ALPN stays http/1.1-only (see
/// `acme::raw_server_config_for`). Connection errors log at WARN, not debug: every reachable error
/// is a genuine resolution or splice failure (no deployment holds the port, a protocol mismatch, a
/// dead local instance, or every mesh candidate refusing or timing out), never a normal client
/// disconnect, which `copy_bidirectional` completes as `Ok`, so a fleet-wide misroute would
/// otherwise be invisible at the default log level.
async fn accept_loop(cloud: Arc<CloudState>, port: u16, listener: TcpListener, tls: bool) {
    let acceptor = tls.then(|| {
        let hosts = scoped_tls_hosts(&cloud, port);
        tracing::info!(
            port,
            hosts = hosts.len(),
            "raw-proxy: TLS acceptor scoped to this port's project hostnames"
        );
        tokio_rustls::TlsAcceptor::from(crate::acme::raw_server_config_for(hosts))
    });
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
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            let r = match acceptor {
                Some(acc) => serve_maybe_tls(cloud, port, stream, acc).await,
                None => handle(cloud, port, stream).await,
            };
            if let Err(e) = r {
                tracing::warn!(port, %peer, error = %e, "raw-proxy: connection failed");
            }
        });
    }
}

/// A PUBLISHED Http port serves both schemes on one number, the way a docker-published port would
/// if docker carried the platform's certs: the first byte decides. 0x16 is a TLS handshake record,
/// so TLS terminates with the platform cert (the browser's `https://<host>:9001` case, the reason
/// this exists) and the decrypted bytes are spliced; anything else is plain HTTP and a pure
/// passthrough (curl/S3 SDKs pointed at an `http://` endpoint, docker parity). The peek costs
/// nothing on the wire, as it does not consume the byte, and a connection closed before sending
/// anything simply ends.
async fn serve_maybe_tls(
    cloud: Arc<CloudState>,
    port: u16,
    client: TcpStream,
    acceptor: tokio_rustls::TlsAcceptor,
) -> anyhow::Result<()> {
    let mut first = [0u8; 1];
    let n = tokio::time::timeout(std::time::Duration::from_secs(10), client.peek(&mut first))
        .await
        .map_err(|_| anyhow::anyhow!("no first byte within 10s"))??;
    if n == 0 {
        return Ok(());
    }
    if first[0] == 0x16 {
        let tls_stream =
            tokio::time::timeout(std::time::Duration::from_secs(10), acceptor.accept(client))
                .await
                .map_err(|_| anyhow::anyhow!("TLS handshake timed out"))?
                .map_err(|e| anyhow::anyhow!("TLS handshake failed: {e}"))?;
        handle(cloud, port, tls_stream).await
    } else {
        handle(cloud, port, client).await
    }
}

/// One inbound public connection: map the port to its deployment, decide
/// local-vs-remote, splice.
/// Routing follows the HTTP edge's single-owner rule (`edge.rs`): a live container lease names the
/// ONE node allowed to serve this project's container, so a connection is never served locally past
/// someone else's lease. When we hold the lease or none exists, local resolution
/// (`mesh_raw::resolve`, the SAME path an inbound mesh stream takes) succeeds exactly when a local
/// Ready deployment publishes this binding and leases the instance for the splice's lifetime; the
/// splice is bytes only until either side closes, and the lease guard drops AFTER it ends. A failed
/// local resolution falls through to [`splice_remote`] even when this node holds the lease.
async fn handle<S>(cloud: Arc<CloudState>, port: u16, mut client: S) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let target = resolve_binding(&cloud, port)
        .ok_or_else(|| anyhow::anyhow!("no deployment holds public raw port {port}"))?;
    anyhow::ensure!(
        tcp_transport(target.protocol),
        "port {port} is a {} binding — not served by the TCP raw proxy",
        target.protocol
    );

    let lease_owner = cloud
        .leases
        .owner_of(&target.project)
        .filter(|o| o != &cloud.node_name);

    if lease_owner.is_none() {
        if let Some(conn) = crate::mesh_raw::resolve(&cloud, target.raw_target()).await {
            tracing::debug!(port, project = %target.project, function = %target.function, addr = %conn.addr, "raw-proxy: local splice");
            let mut backend = TcpStream::connect(&conn.addr)
                .await
                .map_err(|e| anyhow::anyhow!("local instance {} unreachable: {e}", conn.addr))?;
            tokio::io::copy_bidirectional(&mut client, &mut backend).await?;
            drop(conn.guard);
            return Ok(());
        }
    }
    splice_remote(&cloud, port, &target, lease_owner, &mut client).await
}

/// The hostnames a PUBLISHED port may legitimately present a certificate for: every alias label of
/// the owning project, rendered on the apps domain, plus any managed custom domain whose first
/// label is one of those (aliases are keyed by first label, the platform-wide routing convention),
/// included under its FULL name so a browser hitting `shop.example.com:9001` completes while
/// `api.<platform>` (never a label of this project) does not. Empty when the port resolves to
/// nothing, which fails every handshake: the correct outcome for a listener with no owner.
fn scoped_tls_hosts(cloud: &Arc<CloudState>, port: u16) -> Vec<String> {
    let Some(target) = resolve_binding(cloud, port) else {
        return Vec::new();
    };
    let labels = cloud.gw.alias_labels_for_project(&target.project);
    let apps = cloud.apps_domain.trim().trim_matches('.').to_string();
    let mut hosts: Vec<String> = Vec::new();
    for l in &labels {
        if !apps.is_empty() {
            hosts.push(format!("{l}.{apps}"));
        }
    }
    for d in cloud.domains.snapshot() {
        let first = d.domain.split('.').next().unwrap_or("");
        if !first.is_empty() && labels.iter().any(|l| l == first) {
            hosts.push(d.domain.to_ascii_lowercase());
        }
    }
    hosts.sort();
    hosts.dedup();
    hosts
}

/// Maps `public_port` to its allocation. Sources, most-authoritative first: this node's own durable
/// claim registry (the allocator node knows even before the record gossips), then local
/// deployments' stamped bindings, then gossiped peers'.
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

/// Cross-node hop: opens a raw mesh stream to the owner (the `ws_proxy` pattern, minus the
/// HTTP-upgrade replay: the `RawTarget` handshake names the service instead) and splices the public
/// client onto it. Candidates are tried in order; `open_raw_to_port` only returns once the owner
/// has ADMITTED the target (status byte), and no client bytes are consumed before then, so failover
/// to the next candidate is always safe. A lease held by a peer is authoritative and EXCLUSIVE
/// (stateful single-writer), so that peer is the only candidate; otherwise any healthy node that
/// gossiped a Ready deployment carrying this port is. A connect/open timeout is the strongest
/// dead-peer signal, so, as in `ws_proxy`, the node is demoted and stops being ranked.
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

/// Healthy nodes that gossiped a Ready deployment carrying `port`, nearest first (same region, then
/// latency): the raw-TCP analogue of the edge's peer-route candidate ordering. `pub(crate)` because
/// the UDP relay (`crate::udp_relay`) orders its no-lease mesh candidates with the same rule, so
/// TCP and UDP ingress pick peers identically.
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
