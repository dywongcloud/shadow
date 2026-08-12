//! Additional public HTTPS listeners for the paid dedicated-IPv4 addon.
//!
//! A naive implementation would bind the dedicated IP as a bare TCP splice —
//! `raw_proxy.rs`'s pattern — but that bypasses `edge_pipeline` (WAF + bot
//! defense) entirely, since that middleware only runs on the shared public
//! router. `edge_pipeline`'s enforcement is keyed off the REQUEST (host,
//! path, headers), not the local socket address it arrived on, so reusing
//! the exact same `axum::Router` this node already built for the primary
//! HTTPS listener — same `SniResolver`-backed rustls config, same
//! middleware stack — is what makes a dedicated-IP request get the SAME
//! WAF/bot coverage a shared-IP request gets. Never build a second, stripped
//! router for this: that is precisely the gap this module exists to avoid.
//!
//! **Which addresses to bind** — a reconcile loop (the `raw_proxy.rs`
//! pattern applied to `DeploymentInfo::dedicated_ipv4` instead of
//! `raw_ports`) diffs this node's own Ready deployments' + every gossiped
//! peer deployment's dedicated-IPv4 allocations, filtered to
//! `owner_node == cloud.node_name` (only the node the address is actually
//! associated to at the network layer can bind it — every other node must
//! NOT, or the bind fails or silently steals traffic depending on host
//! routing), against the live listener set.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use fluid_core::DeployState;

use crate::state::CloudState;

/// Spawn the reconcile loop. Runs on every node unconditionally (mirrors
/// `raw_proxy::spawn`) — a node associated with no dedicated address binds
/// nothing extra.
pub fn spawn(cloud: Arc<CloudState>, router: axum::Router) {
    tokio::spawn(async move {
        let mut listeners: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
        loop {
            let desired = owned_addresses(&cloud);
            let stale: Vec<String> = listeners
                .keys()
                .filter(|a| !desired.contains(a.as_str()))
                .cloned()
                .collect();
            for addr in stale {
                if let Some(h) = listeners.remove(&addr) {
                    h.abort();
                    tracing::info!(address = %addr, "dedicated-ipv4: listener closed (allocation released or reassigned)");
                }
            }
            listeners.retain(|_, h| !h.is_finished());
            for addr in &desired {
                if listeners.contains_key(addr) {
                    continue;
                }
                let Ok(ip): Result<std::net::Ipv4Addr, _> = addr.parse() else {
                    tracing::warn!(address = %addr, "dedicated-ipv4: not a valid IPv4 literal, skipping");
                    continue;
                };
                let bind_addr = SocketAddr::from((ip, 443));
                let router = router.clone();
                let cfg = axum_server::tls_rustls::RustlsConfig::from_config(
                    crate::acme::server_config(),
                );
                let addr_owned = addr.clone();
                let handle = tokio::spawn(async move {
                    tracing::info!(address = %bind_addr, "dedicated-ipv4: public HTTPS listener up");
                    if let Err(e) = axum_server::bind_rustls(bind_addr, cfg)
                        .serve(router.into_make_service_with_connect_info::<SocketAddr>())
                        .await
                    {
                        tracing::warn!(address = %addr_owned, error = %e, "dedicated-ipv4: listener failed (will retry next reconcile tick)");
                    }
                });
                listeners.insert(addr.clone(), handle);
            }
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        }
    });
}

/// Every dedicated IPv4 address some Ready deployment (local or gossiped)
/// carries, filtered to the ones actually associated to THIS node.
fn owned_addresses(cloud: &Arc<CloudState>) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    let mut add = |d: &fluid_core::DeploymentInfo| {
        if d.state != DeployState::Ready {
            return;
        }
        if let Some(alloc) = &d.dedicated_ipv4 {
            if alloc.owner_node == cloud.node_name {
                out.insert(alloc.address.clone());
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
