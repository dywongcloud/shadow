//! Owner-side resolution for generic raw TCP/UDP mesh forwarding.
//!
//! The mesh transport (`hive_p2p::STREAM_RAW_TARGET` / `PeerPool::
//! open_raw_to_port`) delivers a `RawTarget` handshake naming which
//! project/function/container-port a remote edge node wants spliced; THIS
//! module answers "where does that live on this node?" — the one question the
//! transport crate deliberately cannot answer itself. It is the cross-node hop
//! the generic raw-port proxy (TCP) and the UDP relay use when the public port
//! for a raw-protocol deployment (`raw_ports.rs` allocations) is dialed on a
//! node that doesn't host the deployment: edge node accepts the client, calls
//! `PeerPool::open_raw_to_port(owner, …)`, and the owner resolves here.
//!
//! Resolution is deliberately owner-local: the edge pins a deployment id only
//! if it has one; otherwise the OWNER picks the project's current serving
//! deployment from its own records (always fresher across a redeploy than
//! anything the edge could have cached).
//!
//! The TCP local leg goes through `Fluid::lease` — the same admission every
//! HTTP request pays — so a raw connection cold-starts a scaled-to-zero
//! service, balances across instances, and holds the lease (inflight
//! accounting) for the connection's whole lifetime. The leased endpoint is the
//! per-instance tunnel listener, which for a raw-protocol function serves each
//! accepted connection as a pure byte splice into the container
//! (`fluid_tunnel::TunnelServer::serve_raw`) — so the mesh stream really is
//! wire-transparent end to end.

use std::sync::Arc;

use hive_p2p::{RawProto, RawTarget, RawTargetConn, RawTargetResolver};

use crate::state::CloudState;

/// Build the [`RawTargetResolver`] handed to `hive_p2p::serve_tunnels_full` at
/// boot: the owner-node accept side of every `open_raw_to_port` mesh stream.
pub fn resolver(cloud: Arc<CloudState>) -> RawTargetResolver {
    Arc::new(move |target: RawTarget| {
        let cloud = cloud.clone();
        Box::pin(async move { resolve(&cloud, target).await })
    })
}

/// Whether a declared [`fluid_core::ServiceProtocol`] port spec is reachable
/// over the given mesh transport class: `tcp` mesh streams carry any
/// stream-oriented raw protocol (raw TCP and gRPC — both are byte splices),
/// `udp` only UDP. HTTP-family specs are never raw-spliced (they ride the
/// L7 gateway), enforced separately via `needs_raw_proxy`.
fn proto_matches(spec: fluid_core::ServiceProtocol, proto: RawProto) -> bool {
    match proto {
        // Http is here for compose-PUBLISHED ports: a published Http port is a
        // plain TCP byte stream on the wire, and the spec filter below only
        // admits an Http spec when it carries a publish request — an unpublished
        // Http port still never resolves.
        RawProto::Tcp => matches!(
            spec,
            fluid_core::ServiceProtocol::Tcp
                | fluid_core::ServiceProtocol::Grpc
                | fluid_core::ServiceProtocol::Http
        ),
        RawProto::Udp => spec == fluid_core::ServiceProtocol::Udp,
    }
}

/// Resolve a raw target to its LOCAL leg on this node. `pub(crate)` because the
/// UDP relay (`udp_relay.rs`) reuses the exact same resolution for sessions
/// whose deployment lives on THIS node (no mesh hop) — one resolution path for
/// both the owner side of a mesh stream and a locally-served session.
pub(crate) async fn resolve(cloud: &Arc<CloudState>, t: RawTarget) -> Option<RawTargetConn> {
    // 1. The deployment record serving this target on THIS node: an explicit
    //    pin wins; otherwise the project's current serving deployment (prod
    //    alias holder first, then newest ready).
    let recs = cloud.gw.deployment_records();
    let rec = if !t.deployment.is_empty() {
        recs.into_iter().find(|r| {
            r.id == t.deployment
                && r.project == t.project
                && r.state == fluid_core::DeployState::Ready
        })?
    } else {
        recs.into_iter()
            .filter(|r| {
                r.project == t.project
                    && r.state == fluid_core::DeployState::Ready
                    && r.manifest.functions.iter().any(|f| f.name == t.function)
            })
            .max_by_key(|r| (r.production, r.created_at_ms))?
    };
    let f = rec
        .manifest
        .functions
        .iter()
        .find(|f| f.name == t.function)?;
    // 2. Only splice into a DECLARED raw-protocol port of that function — the
    //    handshake must name a (container_port, protocol) the deployment
    //    actually published (the same specs `raw_ports::allocate_raw_ports`
    //    allocated public ports for), never an arbitrary local port.
    let spec_idx = f.ports.iter().position(|s| {
        s.container_port == t.port
            && (s.protocol.needs_raw_proxy() || s.preferred_public_port.is_some())
            && proto_matches(s.protocol, t.proto)
    })?;
    match t.proto {
        RawProto::Tcp => {
            // The leased endpoint below is the per-instance tunnel listener,
            // which splices ONLY into the container's PRIMARY (first) declared
            // port and serves raw ONLY when the FUNCTION's protocol is raw
            // (`FunctionLaunch::raw_proxy` — see `hive_backend::
            // podman_run_container`'s accept loop). A non-primary port of a
            // multi-port service needs the container's per-port published
            // loopback `host_port` surfaced out of the backend (today it never
            // leaves `podman_run_container`) — same gap as UDP below; until
            // then, refuse loudly instead of splicing the wrong port.
            let key = fluid_compute::func_key(&rec.id, &t.function);
            let lease = match cloud.fluid.lease(&key).await {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!(func = %key, error = %e, "raw mesh target: lease failed");
                    return None;
                }
            };
            // Two local legs, chosen by what fronts the port:
            //  - The PRIMARY port of a raw-protocol function keeps the
            //    per-instance tunnel listener, which serves a raw byte splice
            //    when `FunctionLaunch::raw_proxy` is set (unchanged path).
            //  - Every OTHER eligible spec — extra Tcp/Grpc ports of a
            //    multi-port service, and compose-PUBLISHED Http ports (primary
            //    included: an Http function's tunnel is HTTP-framed and would
            //    corrupt a raw splice) — dials the container's own per-port
            //    loopback publish (`Lease::tcp_host_port`, the TCP twin of the
            //    UDP relay's `udp_host_port` leg).
            if spec_idx == 0 && f.needs_raw_proxy() {
                let addr = match &lease.endpoint {
                    hive_backend::CellEndpoint::Tcp(a) => a.clone(),
                    // A vsock (microVM) endpoint has no TCP address to splice; raw
                    // services run as host containers, so this is not a served shape.
                    hive_backend::CellEndpoint::Vsock { .. } => {
                        tracing::warn!(func = %key, "raw mesh target: vsock endpoint has no raw TCP leg");
                        return None;
                    }
                };
                // The lease rides as the guard so instance inflight accounting
                // covers the connection's whole lifetime (released on splice end).
                return Some(RawTargetConn {
                    addr,
                    guard: Some(Box::new(lease)),
                });
            }
            let Some(host_port) = lease.tcp_host_port(t.port) else {
                tracing::warn!(
                    project = %t.project, function = %t.function, port = t.port,
                    "raw tcp target refused: instance publishes no such TCP port (pre-upgrade \
                     instance still warm, non-container function, or publish skipped) — \
                     a redeploy re-publishes it"
                );
                return None;
            };
            Some(RawTargetConn {
                addr: format!("127.0.0.1:{host_port}"),
                guard: Some(Box::new(lease)),
            })
        }
        RawProto::Udp => {
            // The local UDP leg: the container publishes every declared UDP spec
            // on its own loopback host port (`-p 127.0.0.1:<host_port>:
            // <container_port>/udp` — chosen by fluid-compute's `cold_start`,
            // surfaced per-instance via `Lease::udp_host_port`). The lease is
            // the SAME admission every HTTP request pays, so a scaled-to-zero
            // UDP service cold-starts on its first datagram; it rides as the
            // guard so inflight accounting keeps the instance alive for the
            // whole session (the UDP relay / mesh pump drops it on idle-evict).
            let key = fluid_compute::func_key(&rec.id, &t.function);
            let lease = match cloud.fluid.lease(&key).await {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!(func = %key, error = %e, "udp raw target: lease failed");
                    return None;
                }
            };
            let Some(host_port) = lease.udp_host_port(t.port) else {
                tracing::warn!(
                    project = %t.project, function = %t.function, port = t.port,
                    "udp raw target refused: instance publishes no such UDP port (non-container function, or publish skipped)"
                );
                return None;
            };
            Some(RawTargetConn {
                addr: format!("127.0.0.1:{host_port}"),
                guard: Some(Box::new(lease)),
            })
        }
    }
}
