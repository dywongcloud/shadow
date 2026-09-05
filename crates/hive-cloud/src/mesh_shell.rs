//! Cross-node interactive-shell forwarding for sandboxes over the existing
//! `hive_p2p::STREAM_RAW_TARGET` mesh surface — no new wire protocol.
//!
//! WHY THIS EXISTS: `sandboxes_api::open_shell` used to answer a client that
//! landed on a non-owner node with a `wrong_node` message naming the owner
//! and closing — the frontend has no way to reconnect anywhere useful (no
//! cert-covered per-node hostname exists on this fleet; `acme.rs` issues
//! certs only for `*.{apps_domain}` and a fixed short list under
//! `{platform_domain}`, never arbitrary node subdomains). A real sandbox
//! (`sbx_253aa161efc04c5b`, genuinely running via real Firecracker on
//! fc-bangkok) was reachable only by luck of round-robin/geo DNS, and every
//! other landing showed a permanently "disconnected" terminal.
//!
//! THE FIX: the non-owner node opens a `RawTarget` mesh stream to the owner
//! (the exact same `STREAM_RAW_TARGET` handshake/failover machinery
//! `raw_proxy.rs`/`udp_relay.rs` already use for generic cross-node
//! forwarding — `mesh_raw::resolve` learns to recognize a SANDBOX-shaped
//! target and bridges it to a real local pty instead of a deployment's
//! container port) and pumps FRAMED bytes both ways. The browser's
//! websocket to `api.<domain>` never closes or redirects; only the
//! server-side plumbing changes, so the frontend (`terminal-panel.tsx`)
//! needs zero changes.
//!
//! WIRE FORMAT over the mesh TCP splice (this module's own, not
//! `hive_p2p`'s): `[1B tag][4B BE len][payload]`, chosen to preserve the
//! EXACT distinction `pump_shell`'s websocket wire contract already makes
//! between raw pty bytes (`Message::Binary`) and JSON control messages
//! (`Message::Text`) — collapsing them onto one untyped byte stream would
//! make a literal `{` a client types indistinguishable from a resize
//! control message, the same hazard `pump_shell`'s own doc comment already
//! calls out for the websocket leg.
//!   TAG_DATA (0)   — raw pty bytes, either direction.
//!   TAG_RESIZE (1) — owner-bound only: `[u16 BE cols][u16 BE rows]`.
//!   TAG_EXITED (2) — client-bound only: `[i32 BE exit_code]` (`i32::MIN`
//!                    sentinel means "no exit code", mirroring `AgentEvent::
//!                    PtyExited`'s `Option<i32>`).

use std::sync::Arc;

use hive_p2p::{RawProto, RawTarget, RawTargetConn};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::sandboxes::SandboxProvider;
use crate::state::CloudState;

/// Sentinel `RawTarget.function` prefix marking a sandbox-shell target — kept
/// inside the existing `RawTarget{project,function,port,proto}` shape rather
/// than adding a new stream mode, since the wire/failover/admission machinery
/// this needs already exists verbatim for exactly this purpose.
const SHELL_MARKER: &str = "__sandbox_shell__";

const TAG_DATA: u8 = 0;
const TAG_RESIZE: u8 = 1;
const TAG_EXITED: u8 = 2;

pub(crate) fn shell_target(sandbox_id: &str, cols: u16, rows: u16) -> RawTarget {
    RawTarget {
        project: SHELL_MARKER.into(),
        function: sandbox_id.into(),
        deployment: String::new(),
        // `port` doubles as the initial terminal size — cols in the high 16
        // bits is not needed (RawTarget.port is u16); ship the initial size
        // as the first frame instead (see `bridge_owner_side` below) so this
        // stays within RawTarget's existing field shape. `port` is unused by
        // this target kind and left 0.
        port: 0,
        proto: RawProto::Tcp,
    }
    .with_initial_size(cols, rows)
}

/// Small extension so `shell_target` can stash the initial size without a
/// new `RawTarget` field (that struct lives in `hive-p2p`, which this crate
/// deliberately does not modify for a hive-cloud-local feature). Encoded
/// into `deployment` (unused by this target kind) as `"<cols>x<rows>"`.
trait WithInitialSize {
    fn with_initial_size(self, cols: u16, rows: u16) -> Self;
}
impl WithInitialSize for RawTarget {
    fn with_initial_size(mut self, cols: u16, rows: u16) -> Self {
        self.deployment = format!("{cols}x{rows}");
        self
    }
}

/// Owner-side resolution: does `t` name a sandbox this node actually owns?
/// Returns `None` (→ `RAW_TARGET_NOT_FOUND`, opener fails over) for anything
/// else — `mesh_raw::resolve` calls this FIRST, before its own
/// deployment-lease resolution, so a real sandbox target never falls through
/// to (and is never confused with) a deployment raw-port target.
pub(crate) async fn resolve_sandbox_shell(
    cloud: &Arc<CloudState>,
    t: &RawTarget,
) -> Option<RawTargetConn> {
    if t.project != SHELL_MARKER {
        return None;
    }
    let sandbox_id = t.function.clone();
    let (cols, rows) = t
        .deployment
        .split_once('x')
        .and_then(|(c, r)| Some((c.parse().ok()?, r.parse().ok()?)))
        .unwrap_or((80u16, 24u16));

    // Real ownership check — a sandbox record with a DIFFERENT owner_node must
    // not be served here even if the id happens to match something local
    // (e.g. a stale adopted-metadata copy on a node that isn't the real
    // owner). An EMPTY owner_node is a record persisted before the field
    // existed, which the leader-placement rule owned — the same reading
    // `sandboxes_api::open_shell` applies on the forwarding side, so the two
    // ends can never disagree about who serves a legacy record.
    let rec = cloud.sandboxes.get_sandbox_by_id(&sandbox_id)?;
    let owned_here = if rec.owner_node.is_empty() {
        cloud.is_control_plane_leader()
    } else {
        rec.owner_node == cloud.node_name
    };
    if !owned_here {
        return None;
    }
    let (rx, pty) = cloud
        .sandboxes
        .open_shell(&rec.project_id, &sandbox_id, cols, rows)
        .await
        .ok()?;

    // A real local TCP listener bridging this ONE pty session — `RawTargetConn`
    // only ever carries an ADDRESS (the contract every other target kind
    // already uses), so the bridge has to be a real socket, not a direct
    // handoff of `rx`/`pty`. Bound to loopback, ephemeral port, torn down
    // when the single accepted connection ends (one-shot: `mesh_raw`'s
    // opener connects to it immediately after this returns, so there is no
    // window for a second client to race the accept).
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()?;
    let addr = listener.local_addr().ok()?.to_string();
    tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            bridge_owner_side(stream, rx, pty).await;
        }
    });
    Some(RawTargetConn { addr, guard: None })
}

/// Owner side of the bridge: pump `AgentEvent::PtyOutput`/`PtyExited` and
/// `PtyIo` (the exact same real pty this node's OWN `pump_shell` already
/// drives for a local client) onto the framed TCP connection instead of a
/// websocket. Mirrors `sandboxes_api::pump_shell`'s behavior exactly, so a
/// client sees identical output whether it's local or reached over the
/// mesh.
///
/// `read_frame` is NOT cancel-safe mid-frame (a `select!` branch that loses
/// the race drops a partially-read length prefix or payload, corrupting
/// framing for the rest of the connection) — so, mirroring `udp_relay.rs`'s
/// `pump_mesh`, the inbound (client→pty) direction runs in its OWN spawned
/// task, never as a `select!` arm. The outbound (pty→client) loop owns the
/// write half exclusively; dropping it on exit closes the connection, which
/// is this bridge's end-of-session signal for the inbound task.
async fn bridge_owner_side(
    stream: tokio::net::TcpStream,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<hive_core::AgentEvent>,
    pty: hive_backend::PtyIo,
) {
    let (mut r, mut w) = stream.into_split();
    let inbound = tokio::spawn(async move {
        loop {
            match read_frame(&mut r).await {
                Ok(Some((TAG_DATA, payload))) => pty.input(payload),
                Ok(Some((TAG_RESIZE, payload))) if payload.len() == 4 => {
                    let cols = u16::from_be_bytes([payload[0], payload[1]]);
                    let rows = u16::from_be_bytes([payload[2], payload[3]]);
                    pty.resize(cols, rows);
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
    });
    loop {
        match rx.recv().await {
            Some(hive_core::AgentEvent::PtyOutput { bytes, .. }) => {
                if write_frame(&mut w, TAG_DATA, &bytes).await.is_err() {
                    break;
                }
            }
            Some(hive_core::AgentEvent::PtyExited { exit_code, .. }) => {
                let code = exit_code.unwrap_or(i32::MIN);
                let _ = write_frame(&mut w, TAG_EXITED, &code.to_be_bytes()).await;
                break;
            }
            Some(_) => {}
            None => break,
        }
    }
    inbound.abort();
}

/// Non-owner side: given an already-admitted raw mesh stream (opened via
/// `PeerPool::open_raw_to_port`), pump it against a real axum `WebSocket` —
/// translating this module's frame tags back into the exact
/// `Message::Binary`/`Message::Text` shapes `pump_shell` already produces
/// for a local client, so `terminal-panel.tsx` sees byte-identical behavior
/// regardless of which node it landed on.
///
/// Same cancel-safety split as `bridge_owner_side`: `read_frame` on the mesh
/// leg runs in its own task (writing decoded frames to the websocket
/// directly — `WebSocket::send` needs no external synchronization against
/// the socket's own recv side, axum's `WebSocket` splits cleanly), while
/// this function's own loop owns `socket.recv()` (cancel-safe: backed by an
/// internal channel poll, unlike a raw partial-frame read) and writes
/// outbound frames onto the mesh stream.
pub(crate) async fn bridge_client_side<S>(mut socket: axum::extract::ws::WebSocket, raw: S)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    use axum::extract::ws::Message;
    let (mut r, mut w) = tokio::io::split(raw);
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
    let inbound = tokio::spawn(async move {
        loop {
            match read_frame(&mut r).await {
                Ok(Some((TAG_DATA, payload))) => {
                    if out_tx.send(Message::Binary(payload)).is_err() {
                        break;
                    }
                }
                Ok(Some((TAG_EXITED, payload))) if payload.len() == 4 => {
                    let code =
                        i32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                    let exit_code = if code == i32::MIN { None } else { Some(code) };
                    let _ = out_tx.send(Message::Text(
                        serde_json::json!({ "type": "exited", "exit_code": exit_code })
                            .to_string(),
                    ));
                    break;
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
    });
    loop {
        tokio::select! {
            m = out_rx.recv() => match m {
                Some(msg) => {
                    if socket.send(msg).await.is_err() {
                        break;
                    }
                }
                None => break,
            },
            client = socket.recv() => match client {
                Some(Ok(Message::Binary(bytes))) => {
                    if write_frame(&mut w, TAG_DATA, &bytes).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Text(t))) => {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) {
                        if v.get("type").and_then(|x| x.as_str()) == Some("resize") {
                            let cols = v.get("cols").and_then(|x| x.as_u64()).unwrap_or(80) as u16;
                            let rows = v.get("rows").and_then(|x| x.as_u64()).unwrap_or(24) as u16;
                            let mut payload = Vec::with_capacity(4);
                            payload.extend_from_slice(&cols.to_be_bytes());
                            payload.extend_from_slice(&rows.to_be_bytes());
                            if write_frame(&mut w, TAG_RESIZE, &payload).await.is_err() {
                                break;
                            }
                        }
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Err(_)) => break,
                _ => {}
            },
        }
    }
    inbound.abort();
}

async fn write_frame<W: AsyncWrite + Unpin>(
    w: &mut W,
    tag: u8,
    payload: &[u8],
) -> std::io::Result<()> {
    w.write_all(&[tag]).await?;
    w.write_all(&(payload.len() as u32).to_be_bytes()).await?;
    w.write_all(payload).await?;
    w.flush().await
}

/// `Ok(None)` on a clean EOF at a frame boundary (owner closed / stream
/// ended normally); `Err` on any other read failure (mesh transport died
/// mid-frame — surfaced to the caller as a real error so the browser
/// websocket closes with an error rather than hanging silently, per this
/// module's own framing-cancellation discipline).
async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Option<(u8, Vec<u8>)>> {
    let mut tag = [0u8; 1];
    match r.read(&mut tag).await {
        Ok(0) => return Ok(None),
        Ok(_) => {}
        Err(e) => return Err(e),
    }
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    // Same cap as `hive_p2p::RAW_MAX_DATAGRAM` — this stream is TCP (not
    // datagram-boundary-preserving), but bounding it defends against a
    // corrupted/hostile length prefix the same way every other framed mesh
    // read in this codebase does.
    if len > hive_p2p::RAW_MAX_DATAGRAM {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "sandbox shell frame too large",
        ));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload).await?;
    Ok(Some((tag[0], payload)))
}
