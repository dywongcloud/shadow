//! Network-swarm transport primitives over the mesh's authenticated QUIC trunk.
//!
//! This module is the PROBED, bounded part of the swarm: one strict wire frame, one
//! affine channel whose typestate cannot be replayed, and one host capability probe.
//! It does NOT claim a tensor plane, an RDMA verbs path, or a speedup. A fleet node
//! with no verbs device reports NOT CAPABLE; macOS Thunderbolt Bridge is an IP
//! interface and is deliberately never reported as an RDMA transport.
//!
//! Every public item here has a live caller in the operator diagnostic
//! (hive-cloud --swarm-probe). No symbol is a placeholder for unbuilt work.

use std::marker::PhantomData;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::Result;
use iroh::{Endpoint, EndpointAddr, SecretKey, TransportAddr};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::{pq_hybrid_crypto_provider, BROWSER_ALPN, HIVE_ALPN};

/// One frame is LENGTH-PREFIXED: a bare join of fields is not injective.
/// Layout: 4-byte big-endian payload length, one tag byte, payload bytes.
pub const FRAME_HEADER_BYTES: usize = 5;
/// A hard bound, so tenant-influenced sizes can never grow a read to exhaustion.
pub const FRAME_MAX_BYTES: usize = 1 << 20;

pub const TAG_HELLO: u8 = 0x01;
pub const TAG_ECHO: u8 = 0x02;
pub const TAG_ACK: u8 = 0x03;

/// What this host PROVED it has, never what a flag claims it has.
/// rdma_verbs == None means NOT CAPABLE, in the same sense as
/// NodeInfo::wasm_runtime == None.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct SwarmCapability {
    /// A verbs character device with an ACTIVE port. None on this Mac and on
    /// every host in the current live roster.
    pub rdma_verbs: Option<RdmaDevice>,
    /// macOS Thunderbolt Bridge (bridge0 / llw0) is plain IP over Ethernet.
    /// It is NOT libibverbs and is never admitted as an RDMA transport.
    pub thunderbolt_bridge_ip_only: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct RdmaDevice {
    /// The HCA/port whose state was ACTUALLY read ACTIVE. uverbs lists every
    /// device node present so the verbs stack is known to exist; it is NOT a
    /// claim that a given node maps to this HCA, which sysfs alone cannot prove.
    pub hca: String,
    pub port: String,
    pub uverbs: Vec<String>,
}

/// Probe the real device nodes. Absent or unreadable paths are NOT CAPABLE,
/// not an error: a probe that fails closed is still an honest measurement.
/// Linux IB sysfs writes a numeric prefix with the state, e.g. `4: ACTIVE`,
/// not bare `ACTIVE`. Accept both shapes; every other state is NOT CAPABLE.
pub fn port_is_active(state: &str) -> bool {
    state
        .trim()
        .split_whitespace()
        .last()
        .is_some_and(|word| word.eq_ignore_ascii_case("ACTIVE"))
}

fn probe_rdma_verbs() -> Option<RdmaDevice> {
    // Do NOT pair the first uverbsN with an arbitrary ACTIVE HCA: a host
    // with uverbs0 on a DOWN HCA and an ACTIVE mlx5_1 must not be reported
    // as CAPABLE with the wrong device. Require at least one uverbs node for
    // the verbs stack, then return the HCA/port we actually measured.
    let uverbs: Vec<String> = std::fs::read_dir("/dev/infiniband")
        .ok()?
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("uverbs"))
        .collect();
    if uverbs.is_empty() {
        return None;
    }
    for hca in std::fs::read_dir("/sys/class/infiniband").ok()?.flatten() {
        let Ok(ports) = std::fs::read_dir(hca.path().join("ports")) else {
            continue;
        };
        for port in ports.flatten() {
            let state = std::fs::read_to_string(port.path().join("state")).unwrap_or_default();
            if port_is_active(&state) {
                return Some(RdmaDevice {
                    hca: hca.file_name().to_string_lossy().into_owned(),
                    port: port.file_name().to_string_lossy().into_owned(),
                    uverbs,
                });
            }
        }
    }
    None
}

/// A real interface probe. macOS exposes Thunderbolt Bridge as bridge0/llw0;
/// neither is a verbs device, so this only ever sets the IP-only flag.
fn probe_thunderbolt_bridge() -> bool {
    if !cfg!(target_os = "macos") {
        return false;
    }
    match std::process::Command::new("/sbin/ifconfig").arg("-l").output() {
        Ok(out) => String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .any(|name| name == "bridge0" || name == "llw0"),
        Err(_) => false,
    }
}

/// Require a PROVEN verbs device. NOT CAPABLE is a typed refusal, never an
/// assumed capability and never a silent fallback to QUIC.
pub fn require_rdma(cap: &SwarmCapability) -> Result<&RdmaDevice, SwarmError> {
    cap.rdma_verbs
        .as_ref()
        .ok_or(SwarmError::CapabilityMissing("rdma_verbs"))
}

/// Verify the peer's endpoint identity and negotiated ALPN on a completed
/// handshake. A mismatch is a typed refusal, never a default outcome.
pub fn check_peer(
    conn: &iroh::endpoint::Connection,
    expected: iroh::EndpointId,
) -> Result<(), SwarmError> {
    if conn.remote_id() != expected || conn.alpn() != HIVE_ALPN {
        return Err(SwarmError::UntrustedPeer(conn.remote_id().to_string()));
    }
    Ok(())
}

/// Map a bind/probe deadline to the typed timeout refusal.
pub fn map_timeout<T>(r: Result<T, tokio::time::error::Elapsed>) -> Result<T, SwarmError> {
    r.map_err(|_| SwarmError::Timeout)
}

pub fn probe_capability() -> SwarmCapability {
    SwarmCapability {
        rdma_verbs: probe_rdma_verbs(),
        thunderbolt_bridge_ip_only: probe_thunderbolt_bridge(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SwarmError {
    FrameTooLarge { len: usize, max: usize },
    TruncatedHeader { got: usize },
    TruncatedPayload { want: usize, got: usize },
    UnexpectedTag { expected: u8, got: u8 },
    PayloadMismatch,
    Io(String),
    Timeout,
    UntrustedPeer(String),
    CapabilityMissing(&'static str),
}

impl std::fmt::Display for SwarmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FrameTooLarge { len, max } => write!(f, "frame {len} exceeds bound {max}"),
            Self::TruncatedHeader { got } => write!(f, "frame header truncated at {got} bytes"),
            Self::TruncatedPayload { want, got } => {
                write!(f, "frame payload truncated: want {want}, got {got}")
            }
            Self::UnexpectedTag { expected, got } => {
                write!(f, "unexpected frame tag: expected {expected}, got {got}")
            }
            Self::PayloadMismatch => write!(f, "echoed payload differs byte-for-byte"),
            Self::Io(e) => write!(f, "swarm stream I/O failed: {e}"),
            Self::Timeout => write!(f, "swarm round exceeded its deadline"),
            Self::UntrustedPeer(id) => write!(f, "swarm peer {id} is not the trusted endpoint"),
            Self::CapabilityMissing(what) => write!(f, "swarm capability not proven: {what}"),
        }
    }
}

impl std::error::Error for SwarmError {}

impl From<std::io::Error> for SwarmError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

/// Pure morphism: decode(encode(tag, bytes)) == (tag, bytes).
/// The length prefix is the payload length, so truncation is detectable.
pub fn encode_frame(tag: u8, payload: &[u8]) -> Result<Vec<u8>, SwarmError> {
    if payload.len() > FRAME_MAX_BYTES {
        return Err(SwarmError::FrameTooLarge {
            len: payload.len(),
            max: FRAME_MAX_BYTES,
        });
    }
    let mut out = Vec::with_capacity(FRAME_HEADER_BYTES + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.push(tag);
    out.extend_from_slice(payload);
    Ok(out)
}

pub fn decode_frame(bytes: &[u8]) -> Result<(u8, &[u8]), SwarmError> {
    if bytes.len() < FRAME_HEADER_BYTES {
        return Err(SwarmError::TruncatedHeader { got: bytes.len() });
    }
    let len = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    if len > FRAME_MAX_BYTES {
        return Err(SwarmError::FrameTooLarge {
            len,
            max: FRAME_MAX_BYTES,
        });
    }
    let tag = bytes[4];
    let payload = &bytes[FRAME_HEADER_BYTES..];
    if payload.len() != len {
        return Err(SwarmError::TruncatedPayload {
            want: len,
            got: payload.len(),
        });
    }
    Ok((tag, payload))
}

/// The in-flight slot is released on EVERY exit, including a dropped future.
/// A channel that loses its slot guard cannot be constructed.
pub struct InflightSlot {
    live: Arc<AtomicUsize>,
}

impl InflightSlot {
    /// Take one slot. The increment and the guard are the same operation, so a
    /// caller cannot reserve a slot without also owning its release.
    pub fn acquire(live: Arc<AtomicUsize>) -> Self {
        live.fetch_add(1, Ordering::AcqRel);
        Self { live }
    }
}

impl Drop for InflightSlot {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::AcqRel);
    }
}

pub struct Handshaking;
pub struct Open;
pub struct Closed;

/// Affine typestate: every transition consumes self, so no state can be
/// replayed or used twice. The stream is owned by exactly one state at a time.
pub struct SwarmChannel<S> {
    stream: crate::P2pStream,
    peer: String,
    slot: InflightSlot,
    _state: PhantomData<S>,
}

async fn read_frame<S>(stream: &mut S) -> Result<(u8, Vec<u8>), SwarmError>
where
    S: AsyncReadExt + Unpin,
{
    let mut header = [0u8; FRAME_HEADER_BYTES];
    stream.read_exact(&mut header).await?;
    let len = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
    if len > FRAME_MAX_BYTES {
        return Err(SwarmError::FrameTooLarge {
            len,
            max: FRAME_MAX_BYTES,
        });
    }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await?;
    let mut full = Vec::with_capacity(FRAME_HEADER_BYTES + len);
    full.extend_from_slice(&header);
    full.extend_from_slice(&payload);
    let (tag, bytes) = decode_frame(&full)?;
    Ok((tag, bytes.to_vec()))
}

impl SwarmChannel<Handshaking> {
    pub fn new(stream: crate::P2pStream, peer: String, live: Arc<AtomicUsize>) -> Self {
        live.fetch_add(1, Ordering::AcqRel);
        Self {
            stream,
            peer,
            slot: InflightSlot { live },
            _state: PhantomData,
        }
    }

    /// One hello, one byte-exact echo, one ack. All three are verified;
    /// the channel cannot reach Open without them.
    pub async fn handshake(mut self, payload: &[u8]) -> Result<SwarmChannel<Open>, SwarmError> {
        let hello = encode_frame(TAG_HELLO, payload)?;
        self.stream.write_all(&hello).await?;
        self.stream.flush().await?;
        let (tag, echoed) = read_frame(&mut self.stream).await?;
        if tag != TAG_ECHO || echoed != payload {
            return Err(SwarmError::PayloadMismatch);
        }
        let ack = encode_frame(TAG_ACK, payload)?;
        self.stream.write_all(&ack).await?;
        self.stream.flush().await?;
        Ok(SwarmChannel {
            stream: self.stream,
            peer: self.peer,
            slot: self.slot,
            _state: PhantomData,
        })
    }
}

impl SwarmChannel<Open> {
    pub fn peer(&self) -> &str {
        &self.peer
    }

    pub async fn echo_round_trip(
        mut self,
        payload: &[u8],
    ) -> Result<SwarmChannel<Closed>, SwarmError> {
        let frame = encode_frame(TAG_ECHO, payload)?;
        self.stream.write_all(&frame).await?;
        self.stream.flush().await?;
        let (tag, echoed) = read_frame(&mut self.stream).await?;
        if tag != TAG_ACK || echoed != payload {
            return Err(SwarmError::PayloadMismatch);
        }
        Ok(SwarmChannel {
            stream: self.stream,
            peer: self.peer,
            slot: self.slot,
            _state: PhantomData,
        })
    }
}

impl SwarmChannel<Closed> {
    pub fn peer(&self) -> &str {
        &self.peer
    }
}

/// Server side of ONE probe stream, speaking exactly the frames
/// `SwarmChannel` speaks. HELLO is echoed byte-for-byte as ECHO, the ACK is
/// consumed, and a second ECHO is answered with an ACK of the same payload.
/// Returns the number of payload bytes echoed, so a caller can assert a
/// NON-DEFAULT, byte-exact exchange rather than an empty stream.
pub async fn serve_probe_stream(stream: &mut crate::P2pStream) -> Result<usize, SwarmError> {
    let (tag, hello) = read_frame(stream).await?;
    if tag != TAG_HELLO {
        return Err(SwarmError::UnexpectedTag {
            expected: TAG_HELLO,
            got: tag,
        });
    }
    let echo = encode_frame(TAG_ECHO, &hello)?;
    stream.write_all(&echo).await?;
    stream.flush().await?;
    let (tag, ack) = read_frame(stream).await?;
    if tag != TAG_ACK || ack != hello {
        return Err(SwarmError::UnexpectedTag {
            expected: TAG_ACK,
            got: tag,
        });
    }
    let (tag, second) = read_frame(stream).await?;
    if tag != TAG_ECHO {
        return Err(SwarmError::UnexpectedTag {
            expected: TAG_ECHO,
            got: tag,
        });
    }
    let reply = encode_frame(TAG_ACK, &second)?;
    stream.write_all(&reply).await?;
    stream.flush().await?;
    Ok(hello.len() + second.len())
}

/// Count the ACTUAL negotiated key-exchange group on a completed handshake,
/// so a probe can assert hybrid PQ rather than merely trusting the ALPN.
pub fn record_probe_kex(conn: &iroh::endpoint::Connection) {
    crate::record_kex_telemetry(conn, "swarm-probe");
}

/// Bind two loopback-only endpoints that share the production ALPN and the
/// production hybrid crypto provider, with relay and DHT explicitly disabled.
/// No HIVE_IROH_PORT is read, so two binds in one process cannot collide.
/// The caller wraps EACH bind in its own hard timeout.
pub async fn bind_probe_endpoint() -> Result<Endpoint> {
    let builder = Endpoint::builder(iroh::endpoint::presets::Minimal)
        .relay_mode(iroh::RelayMode::Disabled)
        .alpns(vec![HIVE_ALPN.to_vec(), BROWSER_ALPN.to_vec()])
        .secret_key(SecretKey::generate());
    let builder = builder
        .clear_ip_transports()
        .bind_addr(std::net::SocketAddr::from((
            std::net::Ipv4Addr::LOCALHOST,
            0,
        )))?
        .crypto_provider(pq_hybrid_crypto_provider());
    Ok(builder.bind().await?)
}

/// Direct-address dial target from an endpoint's REAL bound sockets.
/// No relay, no DHT, no bare endpoint id: the address is proven to exist.
pub fn direct_addr(ep: &Endpoint) -> Result<EndpointAddr> {
    let sockets = ep.bound_sockets();
    let Some(socket) = sockets
        .iter()
        .find(|s| s.ip() == std::net::Ipv4Addr::LOCALHOST)
    else {
        return Err(anyhow::anyhow!("no loopback socket on probe endpoint"));
    };
    Ok(EndpointAddr::from_parts(
        ep.id(),
        [TransportAddr::Ip(*socket)],
    ))
}
