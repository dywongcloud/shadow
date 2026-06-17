//! `hive-p2p` — distribute the infra over a peer-to-peer QUIC mesh with iroh.
//!
//! The Fluid tunnel protocol ([`fluid_tunnel`]) is transport-agnostic, so we can
//! carry it over an iroh P2P connection: an instance on node B is reachable from
//! node A's gateway by **node id** (a public key), with NAT traversal / relay
//! fallback handled by iroh. This turns the single-machine platform into a
//! distributed one — boxes and instances can live anywhere.
//!
//! * [`bind`] — start an iroh endpoint speaking the Hive ALPN.
//! * [`serve_tunnels`] — accept P2P connections and serve each as a tunnel to a
//!   local function (the instance side).
//! * [`dial`] — open a P2P connection to a remote instance and return a duplex
//!   byte stream a [`fluid_tunnel::TunnelClient`] can drive (the gateway side).

use anyhow::Result;
use iroh::{endpoint::presets::N0, Endpoint, EndpointAddr};
use tokio::io::{AsyncRead, AsyncWrite};

/// ALPN identifying the Hive function-tunnel protocol over iroh.
pub const HIVE_ALPN: &[u8] = b"hive/tunnel/0";

/// Bind an iroh endpoint that can accept Hive tunnels (N0 preset = relay + DNS
/// discovery so peers are reachable by endpoint id from anywhere).
pub async fn bind() -> Result<Endpoint> {
    let ep = Endpoint::builder(N0)
        .alpns(vec![HIVE_ALPN.to_vec()])
        .bind()
        .await?;
    Ok(ep)
}

/// The combined send+recv halves of a P2P stream, usable as one duplex stream.
pub type P2pStream = tokio::io::Join<iroh::endpoint::RecvStream, iroh::endpoint::SendStream>;

/// Dial a remote endpoint and open a bidirectional stream for one tunnel.
pub async fn dial(ep: &Endpoint, addr: impl Into<EndpointAddr>) -> Result<P2pStream> {
    let conn = ep.connect(addr, HIVE_ALPN).await?;
    let (send, recv) = conn.open_bi().await?;
    Ok(tokio::io::join(recv, send))
}

/// Accept P2P connections forever; serve every bidirectional stream as a tunnel
/// to the local function server at `local_http`. This is the instance side.
pub async fn serve_tunnels(ep: Endpoint, local_http: String, max_concurrency: u32) {
    while let Some(incoming) = ep.accept().await {
        let local = local_http.clone();
        tokio::spawn(async move {
            let conn = match incoming.await {
                Ok(c) => c,
                Err(_) => return,
            };
            loop {
                match conn.accept_bi().await {
                    Ok((send, recv)) => {
                        let stream = tokio::io::join(recv, send);
                        let local = local.clone();
                        tokio::spawn(async move {
                            fluid_tunnel::TunnelServer::serve(stream, local, max_concurrency).await;
                        });
                    }
                    Err(_) => break,
                }
            }
        });
    }
}

/// Assert at compile time that a `P2pStream` satisfies the tunnel transport bound.
fn _assert_duplex<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>() {}
#[allow(dead_code)]
fn _check() {
    _assert_duplex::<P2pStream>();
}
