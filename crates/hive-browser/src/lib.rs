//! hive-browser — a SHADW mesh node that runs inside a browser tab.
//!
//! This is the wasm32 networking scaffold (PRD `bn-impl-crate-scaffold`). It
//! boots a real iroh `Endpoint` over the browser's relay-only WebSocket
//! transport, publishes/resolves identity through a pkarr relay, and accepts
//! inbound connections on its OWN ALPN (`hive/browser/0`) — the dedicated,
//! never-trusted surface the design (`docs/browser-node-proposal.md` §2.8)
//! keeps structurally disjoint from the fleet control plane. There are NO
//! gossip/join/raw arms here by construction; the handler only serves the
//! small op set in `hive-browser-proto` (echo plus edge-function invoke today),
//! which is enough to prove that a browser tab is a real bidirectional iroh peer
//! without granting it any fleet-node authority.
//!
//! Everything the browser cannot do (UDP, hole-punching) is absent by the same
//! compile-time cfg iroh itself uses; all traffic rides a relay WebSocket and
//! stays end-to-end encrypted (the relay cannot decrypt it).

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use iroh::{
    endpoint::{Connection, QuicTransportConfig, RecvStream, SendStream, VarInt},
    Endpoint, EndpointAddr, EndpointId, RelayMap, RelayMode, RelayUrl, SecretKey, TransportAddr,
    Watcher as _,
};
use n0_future::{time::Instant, StreamExt};
use serde::Serialize;
use wasm_bindgen::{prelude::*, JsCast};
use wasm_bindgen_futures::JsFuture;

/// The registered edge-function invoke handler: a JS callback
/// `(codeDigest: string, requestJson: string, signal: AbortSignal) => Promise<string>` that resolves
/// the digest to a LOCALLY pinned artifact. Executable source never crosses the
/// wire. `Rc<RefCell<>>`, not `Arc<Mutex<>>` — wasm32 in a browser tab is
/// single-threaded, and `js_sys::Function` isn't `Send` anyway.
type InvokeHandler = Rc<RefCell<Option<js_sys::Function>>>;

/// Live endpoint-address observer registered by the trusted worker. The callback
/// receives one JSON object whenever iroh's connected home-relay set changes.
type AddressHandler = Rc<RefCell<Option<js_sys::Function>>>;

/// Trusted local asset reader:
/// `(digest: string, offset: number, maxLen: number, signal: AbortSignal) => Promise<{total, bytes}>`.
type AssetHandler = Rc<RefCell<Option<js_sys::Function>>>;

/// Exact execution scopes granted to each TLS-authenticated iroh endpoint.
/// Empty at boot and shared (not snapshotted) across connection/stream tasks so
/// revocation applies to existing pooled QUIC connections.
type InvokerGrants = Rc<RefCell<BTreeMap<EndpointId, BTreeSet<String>>>>;
type AssetGrants = Rc<RefCell<BTreeMap<EndpointId, BTreeSet<String>>>>;
type PeerConnections = Rc<RefCell<BTreeMap<EndpointId, usize>>>;
type PeerStreams = Rc<RefCell<BTreeMap<EndpointId, usize>>>;

const MAX_PENDING_CONNECTIONS: usize = 16;
const MAX_CONNECTIONS_PER_ENDPOINT: usize = 4;
const MAX_ACTIVE_STREAMS: usize = 32;
const MAX_STREAMS_PER_ENDPOINT: usize = 8;
const MAX_STREAMS_PER_CONNECTION: usize = 8;
const MAX_ACTIVE_ECHO: usize = 4;
const MAX_ACTIVE_HANDLERS: usize = 32;
const MAX_OUTBOUND_CONNECTIONS: usize = 16;
const MAX_ACTIVE_OUTBOUND_STREAMS: usize = 32;
const MAX_OUTBOUND_DIAL_WAITERS: usize = 32;
const MAX_INFLIGHT_ASSET_BYTES: usize = 128 << 20;
const RELAY_ONLINE_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECTION_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const STREAM_READ_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const STREAM_WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const OUTBOUND_STREAM_OPEN_TIMEOUT: Duration = Duration::from_secs(60);
const OUTBOUND_IO_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const HANDLER_TIMEOUT: Duration = Duration::from_secs(10);
const BROWSER_STREAM_WINDOW: u32 = (BROWSER_MAX_FRAME + 4) as u32;
const BROWSER_CONNECTION_WINDOW: u32 = BROWSER_STREAM_WINDOW * MAX_STREAMS_PER_CONNECTION as u32;
static HANDLER_SLOTS: tokio::sync::Semaphore =
    tokio::sync::Semaphore::const_new(MAX_ACTIVE_HANDLERS);

struct PeerConnectionGuard {
    endpoint_id: EndpointId,
    counts: PeerConnections,
}

impl PeerConnectionGuard {
    fn acquire(counts: PeerConnections, endpoint_id: EndpointId) -> Option<Self> {
        let mut current = counts.borrow_mut();
        let count = current.entry(endpoint_id.clone()).or_default();
        if *count >= MAX_CONNECTIONS_PER_ENDPOINT {
            return None;
        }
        *count += 1;
        drop(current);
        Some(Self {
            endpoint_id,
            counts,
        })
    }
}

impl Drop for PeerConnectionGuard {
    fn drop(&mut self) {
        let mut counts = self.counts.borrow_mut();
        let remove = counts.get_mut(&self.endpoint_id).is_some_and(|count| {
            *count -= 1;
            *count == 0
        });
        if remove {
            counts.remove(&self.endpoint_id);
        }
    }
}

struct PeerStreamGuard {
    endpoint_id: EndpointId,
    counts: PeerStreams,
}

impl PeerStreamGuard {
    fn acquire(counts: PeerStreams, endpoint_id: EndpointId) -> Option<Self> {
        let mut current = counts.borrow_mut();
        let count = current.entry(endpoint_id.clone()).or_default();
        if *count >= MAX_STREAMS_PER_ENDPOINT {
            return None;
        }
        *count += 1;
        drop(current);
        Some(Self {
            endpoint_id,
            counts,
        })
    }
}

impl Drop for PeerStreamGuard {
    fn drop(&mut self) {
        let mut counts = self.counts.borrow_mut();
        let remove = counts.get_mut(&self.endpoint_id).is_some_and(|count| {
            *count -= 1;
            *count == 0
        });
        if remove {
            counts.remove(&self.endpoint_id);
        }
    }
}

struct ConnectionActivity {
    active: usize,
    idle_since: Option<Instant>,
}

struct ConnectionStreamGuard {
    activity: Rc<RefCell<ConnectionActivity>>,
}

impl ConnectionStreamGuard {
    fn acquire(activity: Rc<RefCell<ConnectionActivity>>) -> Self {
        let mut state = activity.borrow_mut();
        state.active += 1;
        state.idle_since = None;
        drop(state);
        Self { activity }
    }
}

impl Drop for ConnectionStreamGuard {
    fn drop(&mut self) {
        let mut state = self.activity.borrow_mut();
        state.active = state.active.saturating_sub(1);
        if state.active == 0 {
            state.idle_since = Some(Instant::now());
        }
    }
}

struct RelayMutationGuard {
    active: Rc<Cell<bool>>,
}

impl RelayMutationGuard {
    fn acquire(active: Rc<Cell<bool>>) -> Option<Self> {
        if active.replace(true) {
            None
        } else {
            Some(Self { active })
        }
    }
}

impl Drop for RelayMutationGuard {
    fn drop(&mut self) {
        self.active.set(false);
    }
}

#[derive(Clone)]
struct TaskGroup {
    handles: Rc<RefCell<BTreeMap<u64, n0_future::task::AbortHandle>>>,
    closing: Rc<Cell<bool>>,
    next_id: Rc<Cell<u64>>,
}

struct TaskGuard {
    id: u64,
    handles: Rc<RefCell<BTreeMap<u64, n0_future::task::AbortHandle>>>,
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        self.handles.borrow_mut().remove(&self.id);
    }
}

impl TaskGroup {
    fn new() -> Self {
        Self {
            handles: Rc::new(RefCell::new(BTreeMap::new())),
            closing: Rc::new(Cell::new(false)),
            next_id: Rc::new(Cell::new(1)),
        }
    }

    fn spawn(&self, future: impl Future<Output = ()> + 'static) {
        if self.closing.get() {
            return;
        }
        let id = self.next_id.get();
        self.next_id.set(id.wrapping_add(1).max(1));
        let guard = TaskGuard {
            id,
            handles: self.handles.clone(),
        };
        let handle = n0_future::task::spawn(async move {
            let _guard = guard;
            future.await;
        });
        self.handles.borrow_mut().insert(id, handle.abort_handle());
    }

    fn abort_all(&self) {
        self.closing.set(true);
        for handle in self.handles.borrow().values() {
            handle.abort();
        }
    }

    async fn shutdown(&self) {
        self.abort_all();
        let drained = n0_future::time::timeout(Duration::from_secs(1), async {
            while !self.handles.borrow().is_empty() {
                n0_future::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await;
        if drained.is_err() {
            tracing::warn!(
                remaining = self.handles.borrow().len(),
                "browser task drain timed out"
            );
            self.handles.borrow_mut().clear();
        }
    }
}

#[derive(Clone)]
struct OutboundPool {
    inner: Rc<OutboundPoolInner>,
}

struct OutboundPoolInner {
    ep: Endpoint,
    tasks: TaskGroup,
    state: RefCell<OutboundPoolState>,
}

struct OutboundPoolState {
    closed: bool,
    next_generation: u64,
    active_streams: usize,
    entries: BTreeMap<EndpointId, OutboundPoolEntry>,
}

struct OutboundPoolEntry {
    generation: u64,
    connection: Option<Connection>,
    dialing: bool,
    waiters: Vec<tokio::sync::oneshot::Sender<Result<Connection, String>>>,
    stream_slots: Arc<tokio::sync::Semaphore>,
    active_streams: usize,
    last_used: Instant,
}

struct OutboundConnectionLease {
    pool: OutboundPool,
    endpoint_id: EndpointId,
    connection: Connection,
    _stream_slot: tokio::sync::OwnedSemaphorePermit,
}

enum OutboundAcquirePlan {
    Ready {
        endpoint_id: EndpointId,
        connection: Connection,
    },
    Wait {
        endpoint_id: EndpointId,
        receiver: tokio::sync::oneshot::Receiver<Result<Connection, String>>,
    },
    Dial {
        endpoint_id: EndpointId,
        generation: u64,
        addr: EndpointAddr,
        receiver: tokio::sync::oneshot::Receiver<Result<Connection, String>>,
    },
}

impl OutboundPool {
    fn new(ep: Endpoint, tasks: TaskGroup) -> Self {
        Self {
            inner: Rc::new(OutboundPoolInner {
                ep,
                tasks,
                state: RefCell::new(OutboundPoolState {
                    closed: false,
                    next_generation: 1,
                    active_streams: 0,
                    entries: BTreeMap::new(),
                }),
            }),
        }
    }

    async fn acquire(&self, peer_addr_json: &str) -> Result<OutboundConnectionLease, JsError> {
        let addr: EndpointAddr = serde_json::from_str(peer_addr_json)
            .map_err(|error| JsError::new(&format!("bad peer addr_json: {error}")))?;
        let endpoint_id = addr.id.clone();
        let plan = {
            let mut state = self.inner.state.borrow_mut();
            if state.closed {
                return Err(JsError::new("browser node is closed"));
            }
            if state.active_streams >= MAX_ACTIVE_OUTBOUND_STREAMS {
                return Err(JsError::new("browser outbound stream limit reached"));
            }

            let dead = state
                .entries
                .iter()
                .filter(|(_, entry)| {
                    !entry.dialing
                        && entry.active_streams == 0
                        && entry
                            .connection
                            .as_ref()
                            .is_some_and(|connection| connection.close_reason().is_some())
                })
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            for id in dead {
                if let Some(entry) = state.entries.remove(&id) {
                    entry.stream_slots.close();
                }
            }

            let ready = state.entries.get(&endpoint_id).and_then(|entry| {
                entry
                    .connection
                    .as_ref()
                    .filter(|connection| connection.close_reason().is_none())
                    .cloned()
            });
            if let Some(connection) = ready {
                OutboundAcquirePlan::Ready {
                    endpoint_id,
                    connection,
                }
            } else if state
                .entries
                .get(&endpoint_id)
                .is_some_and(|entry| entry.dialing)
            {
                let entry = state
                    .entries
                    .get_mut(&endpoint_id)
                    .expect("dialing outbound entry is present");
                if entry.waiters.len() >= MAX_OUTBOUND_DIAL_WAITERS {
                    return Err(JsError::new("browser outbound dial waiter limit reached"));
                }
                let (sender, receiver) = tokio::sync::oneshot::channel();
                entry.waiters.push(sender);
                OutboundAcquirePlan::Wait {
                    endpoint_id,
                    receiver,
                }
            } else {
                if state.entries.len() >= MAX_OUTBOUND_CONNECTIONS {
                    let oldest_idle = state
                        .entries
                        .iter()
                        .filter(|(_, entry)| {
                            !entry.dialing
                                && entry.active_streams == 0
                                && entry.connection.is_some()
                        })
                        .min_by(|(_, a), (_, b)| {
                            a.last_used
                                .partial_cmp(&b.last_used)
                                .unwrap_or(std::cmp::Ordering::Equal)
                        })
                        .map(|(id, _)| id.clone());
                    let Some(oldest_idle) = oldest_idle else {
                        return Err(JsError::new("browser outbound connection limit reached"));
                    };
                    if let Some(entry) = state.entries.remove(&oldest_idle) {
                        entry.stream_slots.close();
                        if let Some(connection) = entry.connection {
                            connection.close(
                                proto_reset::OVERLOADED.into(),
                                b"browser outbound pool evicted",
                            );
                        }
                    }
                }
                let generation = state.next_generation;
                state.next_generation = state.next_generation.wrapping_add(1).max(1);
                let (sender, receiver) = tokio::sync::oneshot::channel();
                state.entries.insert(
                    endpoint_id.clone(),
                    OutboundPoolEntry {
                        generation,
                        connection: None,
                        dialing: true,
                        waiters: vec![sender],
                        stream_slots: Arc::new(tokio::sync::Semaphore::new(
                            MAX_STREAMS_PER_CONNECTION,
                        )),
                        active_streams: 0,
                        last_used: Instant::now(),
                    },
                );
                OutboundAcquirePlan::Dial {
                    endpoint_id,
                    generation,
                    addr,
                    receiver,
                }
            }
        };

        match plan {
            OutboundAcquirePlan::Ready {
                endpoint_id,
                connection,
            } => self.claim(endpoint_id, connection).await,
            OutboundAcquirePlan::Wait {
                endpoint_id,
                receiver,
            } => {
                let connection = receiver
                    .await
                    .map_err(|_| JsError::new("browser outbound dial cancelled"))?
                    .map_err(|error| JsError::new(&error))?;
                self.claim(endpoint_id, connection).await
            }
            OutboundAcquirePlan::Dial {
                endpoint_id,
                generation,
                addr,
                receiver,
            } => {
                self.start_dial(endpoint_id.clone(), generation, addr);
                let connection = receiver
                    .await
                    .map_err(|_| JsError::new("browser outbound dial cancelled"))?
                    .map_err(|error| JsError::new(&error))?;
                self.claim(endpoint_id, connection).await
            }
        }
    }

    fn start_dial(&self, endpoint_id: EndpointId, generation: u64, addr: EndpointAddr) {
        let ep = self.inner.ep.clone();
        let pool = self.clone();
        self.inner.tasks.spawn(async move {
            let result =
                match n0_future::time::timeout(HANDSHAKE_TIMEOUT, ep.connect(addr, BROWSER_ALPN))
                    .await
                {
                    Ok(Ok(connection)) => Ok(connection),
                    Ok(Err(error)) => Err(format!("connect failed: {error}")),
                    Err(_) => Err("browser outbound connect deadline exceeded".into()),
                };
            pool.finish_dial(endpoint_id, generation, result);
        });
    }

    fn finish_dial(
        &self,
        endpoint_id: EndpointId,
        generation: u64,
        mut result: Result<Connection, String>,
    ) {
        let waiters = {
            let mut state = self.inner.state.borrow_mut();
            let closed = state.closed;
            let Some(entry) = state.entries.get_mut(&endpoint_id) else {
                if let Ok(connection) = result {
                    connection.close(
                        proto_reset::DEADLINE_EXCEEDED.into(),
                        b"stale browser outbound dial",
                    );
                }
                return;
            };
            if closed || entry.generation != generation {
                if let Ok(connection) = result {
                    connection.close(
                        proto_reset::DEADLINE_EXCEEDED.into(),
                        b"stale browser outbound dial",
                    );
                }
                return;
            }
            entry.dialing = false;
            if result
                .as_ref()
                .is_ok_and(|connection| connection.close_reason().is_some())
            {
                result = Err("browser outbound connection closed during dial".into());
            }
            if let Ok(connection) = &result {
                entry.connection = Some(connection.clone());
                entry.last_used = Instant::now();
            }
            let waiters = std::mem::take(&mut entry.waiters);
            if result.is_err() {
                entry.stream_slots.close();
                state.entries.remove(&endpoint_id);
            }
            waiters
        };
        for waiter in waiters {
            let _ = waiter.send(result.clone());
        }
    }

    async fn claim(
        &self,
        endpoint_id: EndpointId,
        connection: Connection,
    ) -> Result<OutboundConnectionLease, JsError> {
        let stable_id = connection.stable_id();
        let stream_slots = {
            let state = self.inner.state.borrow();
            if state.closed {
                return Err(JsError::new("browser node is closed"));
            }
            if state.active_streams >= MAX_ACTIVE_OUTBOUND_STREAMS {
                return Err(JsError::new("browser outbound stream limit reached"));
            }
            let Some(entry) = state.entries.get(&endpoint_id) else {
                return Err(JsError::new("browser outbound connection became stale"));
            };
            let matches = entry
                .connection
                .as_ref()
                .is_some_and(|current| current.stable_id() == stable_id);
            if !matches || connection.close_reason().is_some() {
                return Err(JsError::new("browser outbound connection became stale"));
            }
            entry.stream_slots.clone()
        };
        let stream_slot =
            n0_future::time::timeout(OUTBOUND_STREAM_OPEN_TIMEOUT, stream_slots.acquire_owned())
                .await
                .map_err(|_| JsError::new("browser outbound stream-slot deadline exceeded"))?
                .map_err(|_| JsError::new("browser outbound stream pool is closed"))?;

        let mut state = self.inner.state.borrow_mut();
        if state.closed {
            return Err(JsError::new("browser node is closed"));
        }
        if state.active_streams >= MAX_ACTIVE_OUTBOUND_STREAMS {
            return Err(JsError::new("browser outbound stream limit reached"));
        }
        let matches = state.entries.get(&endpoint_id).is_some_and(|entry| {
            entry
                .connection
                .as_ref()
                .is_some_and(|current| current.stable_id() == stable_id)
        });
        if !matches || connection.close_reason().is_some() {
            return Err(JsError::new("browser outbound connection became stale"));
        }
        state.active_streams += 1;
        let entry = state
            .entries
            .get_mut(&endpoint_id)
            .expect("claimed outbound entry is present");
        entry.active_streams += 1;
        entry.last_used = Instant::now();
        drop(state);
        Ok(OutboundConnectionLease {
            pool: self.clone(),
            endpoint_id,
            connection,
            _stream_slot: stream_slot,
        })
    }

    fn release(&self, endpoint_id: &EndpointId, stable_id: usize) {
        let mut state = self.inner.state.borrow_mut();
        let (released, remove) = if let Some(entry) = state.entries.get_mut(endpoint_id) {
            let matches = entry
                .connection
                .as_ref()
                .is_some_and(|connection| connection.stable_id() == stable_id);
            if matches {
                entry.active_streams = entry.active_streams.saturating_sub(1);
                entry.last_used = Instant::now();
                let remove = entry
                    .connection
                    .as_ref()
                    .is_some_and(|connection| connection.close_reason().is_some());
                (true, remove)
            } else {
                (false, false)
            }
        } else {
            (false, false)
        };
        if released {
            state.active_streams = state.active_streams.saturating_sub(1);
        }
        if remove {
            if let Some(entry) = state.entries.remove(endpoint_id) {
                entry.stream_slots.close();
            }
        }
    }

    fn evict(&self, endpoint_id: &EndpointId, stable_id: usize, reason: &'static [u8]) {
        let removed = {
            let mut state = self.inner.state.borrow_mut();
            let matches = state.entries.get(endpoint_id).is_some_and(|entry| {
                entry
                    .connection
                    .as_ref()
                    .is_some_and(|connection| connection.stable_id() == stable_id)
            });
            if matches {
                let active_streams = state
                    .entries
                    .get(endpoint_id)
                    .map(|entry| entry.active_streams)
                    .unwrap_or_default();
                state.active_streams = state.active_streams.saturating_sub(active_streams);
                state.entries.remove(endpoint_id)
            } else {
                None
            }
        };
        if let Some(entry) = removed {
            entry.stream_slots.close();
            if let Some(connection) = entry.connection {
                connection.close(proto_reset::DEADLINE_EXCEEDED.into(), reason);
            }
            for waiter in entry.waiters {
                let _ = waiter.send(Err("browser outbound connection evicted".into()));
            }
        }
    }

    fn close_all(&self) {
        let entries = {
            let mut state = self.inner.state.borrow_mut();
            if state.closed {
                return;
            }
            state.closed = true;
            state.next_generation = state.next_generation.wrapping_add(1).max(1);
            state.active_streams = 0;
            std::mem::take(&mut state.entries)
        };
        for (_, entry) in entries {
            entry.stream_slots.close();
            if let Some(connection) = entry.connection {
                connection.close(
                    proto_reset::DEADLINE_EXCEEDED.into(),
                    b"browser node closing",
                );
            }
            for waiter in entry.waiters {
                let _ = waiter.send(Err("browser node is closed".into()));
            }
        }
    }

    fn connection_count(&self) -> usize {
        self.inner
            .state
            .borrow()
            .entries
            .values()
            .filter(|entry| {
                entry
                    .connection
                    .as_ref()
                    .is_some_and(|connection| connection.close_reason().is_none())
            })
            .count()
    }
}

impl Drop for OutboundConnectionLease {
    fn drop(&mut self) {
        self.pool
            .release(&self.endpoint_id, self.connection.stable_id());
    }
}

struct AssetReservation {
    bytes: Rc<Cell<usize>>,
    reserved: usize,
}

impl AssetReservation {
    fn acquire(bytes: Rc<Cell<usize>>, reserved: usize) -> Option<Self> {
        let next = bytes.get().checked_add(reserved)?;
        if next > MAX_INFLIGHT_ASSET_BYTES {
            return None;
        }
        bytes.set(next);
        Some(Self { bytes, reserved })
    }
}

impl Drop for AssetReservation {
    fn drop(&mut self) {
        let current = self.bytes.get();
        debug_assert!(current >= self.reserved);
        self.bytes.set(current.saturating_sub(self.reserved));
    }
}

struct OutboundRequestGuard {
    _lease: OutboundConnectionLease,
    send: Option<SendStream>,
    recv: Option<RecvStream>,
    armed: bool,
}

impl OutboundRequestGuard {
    fn new(lease: OutboundConnectionLease, send: SendStream, recv: RecvStream) -> Self {
        Self {
            _lease: lease,
            send: Some(send),
            recv: Some(recv),
            armed: true,
        }
    }

    fn finish_success(&mut self) {
        self.armed = false;
    }

    fn cancel(&mut self, code: u32) {
        if !self.armed {
            return;
        }
        if let Some(send) = self.send.as_mut() {
            let _ = send.reset(code.into());
        }
        if let Some(recv) = self.recv.as_mut() {
            let _ = recv.stop(code.into());
        }
        self.armed = false;
    }
}

impl Drop for OutboundRequestGuard {
    fn drop(&mut self) {
        if self.armed {
            self.cancel(proto_reset::DEADLINE_EXCEEDED);
        }
    }
}

struct HandlerAbortGuard {
    controller: Option<web_sys::AbortController>,
}

impl HandlerAbortGuard {
    fn new() -> Result<Self, u32> {
        let controller =
            web_sys::AbortController::new().map_err(|_| proto_reset::HANDLER_FAILED)?;
        Ok(Self {
            controller: Some(controller),
        })
    }

    fn signal(&self) -> web_sys::AbortSignal {
        self.controller
            .as_ref()
            .expect("handler abort guard is armed")
            .signal()
    }

    fn abort(&mut self) {
        if let Some(controller) = self.controller.take() {
            controller.abort();
        }
    }

    fn disarm(&mut self) {
        self.controller.take();
    }
}

impl Drop for HandlerAbortGuard {
    fn drop(&mut self) {
        self.abort();
    }
}

fn js_result_u32(value: &JsValue, key: &str) -> Result<u32, u32> {
    let number = js_sys::Reflect::get(value, &JsValue::from_str(key))
        .map_err(|_| proto_reset::HANDLER_FAILED)?
        .as_f64()
        .ok_or(proto_reset::HANDLER_FAILED)?;
    if !number.is_finite() || number < 0.0 || number.fract() != 0.0 || number > u32::MAX as f64 {
        return Err(proto_reset::HANDLER_FAILED);
    }
    Ok(number as u32)
}

fn bounded_utf8(value: JsValue) -> Result<Vec<u8>, u32> {
    if !value.is_string() {
        return Err(proto_reset::HANDLER_FAILED);
    }
    let source: js_sys::JsString = value.unchecked_into();
    let source_units = source.length();
    if source_units as usize > BROWSER_MAX_FRAME {
        return Err(proto_reset::FRAME_TOO_LARGE);
    }
    let encoder = web_sys::TextEncoder::new().map_err(|_| proto_reset::HANDLER_FAILED)?;
    let encode_into = js_sys::Reflect::get(encoder.as_ref(), &JsValue::from_str("encodeInto"))
        .and_then(|value| value.dyn_into::<js_sys::Function>())
        .map_err(|_| proto_reset::HANDLER_FAILED)?;
    let destination = js_sys::Uint8Array::new_with_length(BROWSER_MAX_FRAME as u32);
    let progress = encode_into
        .call2(encoder.as_ref(), source.as_ref(), destination.as_ref())
        .map_err(|_| proto_reset::HANDLER_FAILED)?;
    let read = js_result_u32(&progress, "read")?;
    let written = js_result_u32(&progress, "written")?;
    if written as usize > BROWSER_MAX_FRAME {
        return Err(proto_reset::HANDLER_FAILED);
    }
    if read != source_units {
        return Err(proto_reset::FRAME_TOO_LARGE);
    }
    Ok(destination.subarray(0, written).to_vec())
}

async fn write_outbound_with_progress(
    send: &mut SendStream,
    mut payload: &[u8],
) -> Result<(), String> {
    while !payload.is_empty() {
        match n0_future::time::timeout(OUTBOUND_IO_IDLE_TIMEOUT, send.write(payload)).await {
            Ok(Ok(written)) if written > 0 => payload = &payload[written..],
            Ok(Ok(_)) => return Err("outbound write made no progress".into()),
            Ok(Err(error)) => return Err(format!("write request: {error}")),
            Err(_) => return Err("browser outbound write idle deadline exceeded".into()),
        }
    }
    Ok(())
}

async fn read_outbound_with_progress(
    recv: &mut RecvStream,
    mut payload: &mut [u8],
    stage: &'static str,
) -> Result<(), String> {
    while !payload.is_empty() {
        match n0_future::time::timeout(OUTBOUND_IO_IDLE_TIMEOUT, recv.read(payload)).await {
            Ok(Ok(Some(read))) if read > 0 => payload = &mut payload[read..],
            Ok(Ok(_)) => return Err(format!("{stage}: stream ended before frame completed")),
            Ok(Err(error)) => return Err(format!("{stage}: {error}")),
            Err(_) => return Err(format!("{stage}: idle deadline exceeded")),
        }
    }
    Ok(())
}

async fn read_outbound_eof(recv: &mut RecvStream) -> Result<(), (u32, String)> {
    let mut trailing = [0u8; 1];
    match n0_future::time::timeout(OUTBOUND_IO_IDLE_TIMEOUT, recv.read(&mut trailing)).await {
        Ok(Ok(None)) => Ok(()),
        Ok(Ok(Some(_))) => Err((
            proto_reset::MALFORMED_PAYLOAD,
            "reply frame contains trailing bytes".into(),
        )),
        Ok(Err(error)) => Err((
            proto_reset::DEADLINE_EXCEEDED,
            format!("read reply eof: {error}"),
        )),
        Err(_) => Err((
            proto_reset::DEADLINE_EXCEEDED,
            "read reply eof: idle deadline exceeded".into(),
        )),
    }
}

async fn write_reply(send: &mut SendStream, payload: &[u8]) -> Result<(), ()> {
    send.write_all(&(payload.len() as u32).to_le_bytes())
        .await
        .map_err(|_| ())?;
    send.write_all(payload).await.map_err(|_| ())
}

fn reject_stream(send: &mut SendStream, recv: &mut RecvStream, code: u32) {
    let _ = send.reset(code.into());
    let _ = recv.stop(code.into());
}

enum HandlerWait {
    Completed(Result<Vec<u8>, u32>),
    Cancelled,
}

fn watch_handler(
    future: impl Future<Output = Result<Vec<u8>, u32>> + 'static,
    permit: tokio::sync::SemaphorePermit<'static>,
) -> tokio::sync::oneshot::Receiver<Result<Vec<u8>, u32>> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    n0_future::task::spawn(async move {
        let _permit = permit;
        let _ = tx.send(future.await);
    });
    rx
}

/// The browser node's dedicated ALPN, plus the rest of the wire contract.
/// Intentionally distinct from `hive/tunnel/0` (the fleet data/control ALPN) so
/// a browser peer can never be one byte from a control-plane stream mode — the
/// fleet-side accept path dispatches per-ALPN and this one has no privileged
/// arms.
///
/// Everything here comes from `hive-browser-proto`, which `crates/hive-p2p`
/// also depends on: this crate being wasm32-only and workspace-excluded does
/// not prevent a path dependency, so there is no second copy of the ALPN, the
/// frame cap, the op bytes, or the framing helpers to keep in sync.
pub use hive_browser_proto::BROWSER_ALPN;

use hive_browser_proto::{
    check_len, encode_asset_get, encode_asset_reply, encode_invoke, encode_request,
    reset as proto_reset, split_asset_get, split_asset_reply, split_invoke, valid_blake3_digest,
    valid_function_digest, Op, ASSET_CHUNK_MAX, BROWSER_MAX_ASSET, BROWSER_MAX_ECHO,
    BROWSER_MAX_FRAME,
};

#[wasm_bindgen(start)]
pub fn on_load() {
    console_error_panic_hook::set_once();
    // Best-effort structured logs to the devtools console; harmless if it fails.
    let _ = std::panic::catch_unwind(|| tracing_wasm::set_as_global_default());
}

/// Canonical content address shared by function artifacts and asset bytes.
#[wasm_bindgen(js_name = blake3Hex)]
pub fn blake3_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// bn-p2p-version-negotiation (remaining scope, item 3/3: the PWA wasm bundle
/// itself). A service-worker-cached `hive_browser_bg.wasm` can go stale
/// independently of the JS glue that loads it (the two are versioned/cached
/// together as a pair by `ui/scripts/sync-browser-node.mjs`, but a partial or
/// interrupted cache update could still leave a stale wasm binary paired with
/// fresh `hive_browser.js`). `run-node-worker.js` calls this immediately after
/// `await init()` succeeds — before constructing `BrowserNode` — and compares
/// it against its own `WASM_BUNDLE_VERSION` copy (same cross-language-constant
/// pattern as `PROTOCOL_VERSION`/`HOST_ABI_VERSION` there). Bump this whenever
/// a change to this crate's wasm-exposed surface (BrowserNode's methods, the
/// wire contract it implements) is not safely usable by an older worker.
#[wasm_bindgen(js_name = wasmBundleVersion)]
pub fn wasm_bundle_version() -> u32 {
    13
}

/// A live browser mesh node. Holds the bound endpoint and the spawned accept
/// loop for its lifetime; dropping it tears the endpoint down.
#[wasm_bindgen]
pub struct BrowserNode {
    ep: Endpoint,
    /// Trusted worker callback for live iroh address/home-relay updates.
    address_handler: AddressHandler,
    /// Canonical configured relay URLs. Runtime removal refuses the final entry.
    configured_relays: Rc<RefCell<BTreeSet<String>>>,
    /// Single-writer guard for async relay-set mutations. Acquired before the
    /// first await so overlapping wasm exports cannot both pass a stale count.
    relay_mutation: Rc<Cell<bool>>,
    /// Count of echo requests served so far — surfaced to the page as a liveness
    /// signal that the inbound accept path actually fired.
    served: Arc<std::sync::atomic::AtomicU64>,
    /// See [`InvokeHandler`]; `None` until `setInvokeHandler` is called.
    invoke: InvokeHandler,
    /// Demand-side asset reader; serving remains disabled until explicitly set.
    asset: AssetHandler,
    /// Boot-empty, typed endpoint → pinned-code-digest execution scopes.
    grants: InvokerGrants,
    /// Separate endpoint → asset-digest scopes. A function grant can never read
    /// an asset accidentally, even when both identifiers are 64 hex bytes.
    asset_grants: AssetGrants,
    /// Synchronous idempotency flag for the async close path.
    closed: Rc<Cell<bool>>,
    /// Abortable ownership for every platform-spawned Rust task.
    tasks: TaskGroup,
    /// Canonical EndpointId-keyed reusable outbound QUIC trunks.
    outbound_pool: OutboundPool,
    /// Bytes reserved by complete outbound asset destinations.
    asset_bytes: Rc<Cell<usize>>,
}

impl Drop for BrowserNode {
    fn drop(&mut self) {
        // wasm-bindgen's generated `free()` cannot await, but it must not leave
        // the accept loop/relay connection alive forever. Clear capabilities
        // synchronously and begin best-effort shutdown. Callers needing a
        // witnessed drain use the explicit async `close()` before `free()`.
        self.invoke.borrow_mut().take();
        self.asset.borrow_mut().take();
        self.address_handler.borrow_mut().take();
        self.grants.borrow_mut().clear();
        self.asset_grants.borrow_mut().clear();
        self.outbound_pool.close_all();
        self.tasks.abort_all();
        if !self.closed.replace(true) {
            let ep = self.ep.clone();
            n0_future::task::spawn(async move { ep.close().await });
        }
    }
}

#[derive(Serialize)]
struct Status {
    node_id: String,
    relay: String,
    served: u64,
    outbound_connections: usize,
    addr_json: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AddressStatus {
    online: bool,
    relays: Vec<String>,
    addr_json: String,
}

fn address_status(addr: EndpointAddr) -> AddressStatus {
    let relays = addr
        .addrs
        .iter()
        .filter_map(|transport| match transport {
            TransportAddr::Relay(url) => Some(url.to_string()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let online = !relays.is_empty();
    let addr_json = if online {
        serde_json::to_string(&addr).unwrap_or_default()
    } else {
        String::new()
    };
    AddressStatus {
        online,
        relays,
        addr_json,
    }
}

fn emit_address(handler: &AddressHandler, addr: EndpointAddr) {
    let Some(callback) = handler.borrow().as_ref().cloned() else {
        return;
    };
    let Ok(json) = serde_json::to_string(&address_status(addr)) else {
        tracing::warn!("browser address status could not be serialized");
        return;
    };
    if let Err(error) = callback.call1(&JsValue::NULL, &JsValue::from_str(&json)) {
        tracing::warn!(?error, "browser address handler threw");
    }
}

fn spawn_address_loop(ep: Endpoint, handler: AddressHandler, tasks: TaskGroup) {
    let mut addresses = ep.watch_addr().stream();
    let closed = ep.closed();
    tasks.spawn(async move {
        closed
            .run_until(async move {
                while let Some(addr) = addresses.next().await {
                    emit_address(&handler, addr);
                }
            })
            .await;
    });
}

#[wasm_bindgen]
impl BrowserNode {
    /// Boot a node: bind an endpoint against `relay_urls` (comma-separated,
    /// same convention as the fleet's own `HIVE_RELAY_URLS` — see
    /// `hive_p2p::relay_map_from_env` — so a caller can hand multiple relays
    /// for failover instead of pinning to one; each must be `http://` or
    /// `https://` — iroh derives the relay's `ws://`/`wss://` transport),
    /// optionally wire a
    /// pkarr `discovery_url` for publish+resolve, and restore identity from
    /// `secret_hex` (32-byte ed25519 seed, hex) if given — else generate a
    /// fresh one. Spawns the `hive/browser/0` accept loop before returning.
    #[wasm_bindgen]
    pub async fn boot(
        relay_urls: String,
        discovery_url: Option<String>,
        secret_hex: Option<String>,
    ) -> Result<BrowserNode, JsError> {
        let relays: Vec<RelayUrl> = relay_urls
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| {
                let parsed = url::Url::parse(s)
                    .map_err(|e| JsError::new(&format!("bad relay url {s:?}: {e}")))?;
                if !matches!(parsed.scheme(), "http" | "https") {
                    return Err(JsError::new(&format!(
                        "bad relay url {s:?}: scheme must be http or https (iroh derives ws/wss)"
                    )));
                }
                s.parse::<RelayUrl>()
                    .map_err(|e| JsError::new(&format!("bad relay url {s:?}: {e}")))
            })
            .collect::<Result<_, _>>()?;
        if relays.is_empty() {
            return Err(JsError::new("relay_urls must name at least one relay"));
        }
        let configured_relays = Rc::new(RefCell::new(
            relays
                .iter()
                .map(ToString::to_string)
                .collect::<BTreeSet<_>>(),
        ));
        let map = RelayMap::from_iter(relays);
        let relay_mutation = Rc::new(Cell::new(false));

        // Minimal preset (no n0 pkarr/DNS baked in) + our own relay map, so the
        // browser only ever talks to the relay we hand it.
        let transport = QuicTransportConfig::builder()
            .max_concurrent_bidi_streams(VarInt::from_u32(MAX_ACTIVE_STREAMS as u32))
            .stream_receive_window(VarInt::from_u32(BROWSER_STREAM_WINDOW))
            .receive_window(VarInt::from_u32(BROWSER_CONNECTION_WINDOW))
            .build();
        let mut builder = Endpoint::builder(iroh::endpoint::presets::Minimal)
            .relay_mode(RelayMode::Custom(map))
            .alpns(vec![BROWSER_ALPN.to_vec()])
            .transport_config(transport);

        // Identity: restore a persisted seed if the page gave us one, else the
        // caller is expected to read `secret back out` via a later export. A
        // stable seed ⇒ a stable EndpointId across reloads.
        if let Some(hex) = secret_hex.as_deref().filter(|h| !h.is_empty()) {
            let raw = hex_to_32(hex)
                .ok_or_else(|| JsError::new("secret_hex must be 64 hex chars (32 bytes)"))?;
            builder = builder.secret_key(SecretKey::from_bytes(&raw));
        }

        if let Some(url) = discovery_url.as_deref().filter(|u| !u.is_empty()) {
            let u = url::Url::parse(url)
                .map_err(|e| JsError::new(&format!("bad discovery url {url:?}: {e}")))?;
            builder = builder
                .address_lookup(iroh::address_lookup::PkarrPublisher::builder(u.clone()))
                .address_lookup(iroh::address_lookup::PkarrResolver::builder(u));
        }

        let ep = builder
            .bind()
            .await
            .map_err(|e| JsError::new(&format!("endpoint bind failed: {e}")))?;

        // `bind()` resolving does NOT mean this node is dialable. The relay
        // entry that a browser peer needs — the ONLY transport it has, since a
        // browser cannot dial raw IP — is registered asynchronously once the
        // home-relay connection comes up, so `ep.addr()` before that returns an
        // EndpointAddr with an EMPTY addrs list. Handing that out looks like a
        // valid address and fails much later at the DIALER with "No addressing
        // information available", blaming the caller for this node's
        // un-readiness. `browser_echo_native.rs` hit exactly this and carries
        // the same `online()` call for the same reason.
        if n0_future::time::timeout(RELAY_ONLINE_TIMEOUT, ep.online())
            .await
            .is_err()
        {
            ep.close().await;
            return Err(JsError::new("browser relay online deadline exceeded"));
        }

        let served = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let invoke: InvokeHandler = Rc::new(RefCell::new(None));
        let address_handler: AddressHandler = Rc::new(RefCell::new(None));
        let asset: AssetHandler = Rc::new(RefCell::new(None));
        let grants: InvokerGrants = Rc::new(RefCell::new(BTreeMap::new()));
        let asset_grants: AssetGrants = Rc::new(RefCell::new(BTreeMap::new()));
        let closed = Rc::new(Cell::new(false));
        let tasks = TaskGroup::new();
        let outbound_pool = OutboundPool::new(ep.clone(), tasks.clone());
        let asset_bytes = Rc::new(Cell::new(0));
        spawn_accept_loop(
            ep.clone(),
            served.clone(),
            invoke.clone(),
            asset.clone(),
            grants.clone(),
            asset_grants.clone(),
            tasks.clone(),
        );
        spawn_address_loop(ep.clone(), address_handler.clone(), tasks.clone());

        Ok(BrowserNode {
            ep,
            address_handler,
            configured_relays,
            relay_mutation,
            served,
            invoke,
            asset,
            grants,
            asset_grants,
            closed,
            tasks,
            outbound_pool,
            asset_bytes,
        })
    }

    /// The node's cryptographic identity (64-hex EndpointId = its ed25519
    /// public key). Stable across reloads iff booted from the same seed.
    #[wasm_bindgen(js_name = nodeId)]
    pub fn node_id(&self) -> String {
        self.ep.id().to_string()
    }

    /// Proof-of-possession signature for admission (bn-p2p-heartbeat-lease):
    /// signs `"{this node's endpoint_id}:{challenge_ms}"` with this node's
    /// OWN ed25519 secret key, returning 128 hex chars. `challenge_ms` is the
    /// caller's own current-time claim (no separate server round trip to
    /// fetch a nonce) — the backend rejects a signature whose challenge_ms is
    /// outside a tight freshness window, bounding replay of a captured
    /// signature to that window rather than forever. Borrowed from
    /// Folding@home's admission-binding pattern (research:
    /// volunteer-compute-trust-admission-models) — without this, an admission
    /// request naming ANY endpoint_id is accepted on the CALLER's platform
    /// auth alone, with nothing proving the caller actually controls that
    /// endpoint's private key.
    #[wasm_bindgen(js_name = signAdmission)]
    pub fn sign_admission(&self, challenge_ms: String) -> String {
        let endpoint_id = self.ep.id().to_string();
        let message = format!("{endpoint_id}:{challenge_ms}");
        let sig = self.ep.secret_key().sign(message.as_bytes());
        bytes_to_hex(&sig.to_bytes())
    }

    /// The raw 32-byte ed25519 seed, as 64 hex chars — the ONLY way key
    /// material leaves this module. Per `docs/browser-node-proposal.md` §2.2,
    /// this exists solely so the caller can wrap it with a non-extractable
    /// WebCrypto key before it ever touches durable storage; the wasm module
    /// itself never persists anything. JS strings cannot be zeroed after use
    /// (no mutable-buffer access), so the caller must encrypt this value
    /// immediately on receipt and never log or store it bare.
    #[wasm_bindgen(js_name = secretHex)]
    pub fn secret_hex(&self) -> String {
        bytes_to_hex(&self.ep.secret_key().to_bytes())
    }

    /// Serialized `EndpointAddr` (id + relay/transport hints) a peer needs to
    /// dial this browser node.
    ///
    /// THROWS rather than returning an address with no transport hints. `boot`
    /// awaits `online()` so this should not happen, but an address carrying
    /// only an id is undialable, and silently returning one moves the failure
    /// to a different machine's dial attempt where the real cause is invisible.
    /// Fail here, where the node that is not ready can actually be named.
    #[wasm_bindgen(js_name = addrJson)]
    pub fn addr_json(&self) -> Result<String, JsError> {
        self.ensure_open()?;
        let addr = self.ep.addr();
        if addr.addrs.is_empty() {
            return Err(JsError::new(
                "node has no transport hints yet (not online) — refusing to hand out an undialable addr",
            ));
        }
        serde_json::to_string(&addr).map_err(|e| JsError::new(&format!("addr serialize: {e}")))
    }

    /// Subscribe to the endpoint's live dialable address. The callback receives
    /// `{online, relays, addrJson}` immediately and whenever iroh changes the
    /// connected home-relay set. An offline update has `online:false`, an empty
    /// relay list, and an empty addrJson; callers must never keep advertising a
    /// previous address through that state.
    #[wasm_bindgen(js_name = setAddressHandler)]
    pub fn set_address_handler(&self, handler: js_sys::Function) -> Result<(), JsError> {
        self.ensure_open()?;
        if !handler.is_function() {
            return Err(JsError::new("address handler must be a function"));
        }
        *self.address_handler.borrow_mut() = Some(handler);
        emit_address(&self.address_handler, self.ep.addr());
        Ok(())
    }

    /// Remove one configured relay at runtime. iroh's home-relay actor migrates
    /// to another configured relay and `setAddressHandler` reports the new
    /// dialable address. The final relay is structurally non-removable: leaving
    /// a live node with no possible transport is never a valid state.
    #[wasm_bindgen(js_name = removeRelay)]
    pub async fn remove_relay(&self, relay_url: String) -> Result<bool, JsError> {
        self.ensure_open()?;
        let Some(_mutation) = RelayMutationGuard::acquire(self.relay_mutation.clone()) else {
            return Err(JsError::new(
                "another relay mutation is already in progress",
            ));
        };
        let relay = relay_url
            .parse::<RelayUrl>()
            .map_err(|e| JsError::new(&format!("bad relay url {relay_url:?}: {e}")))?;
        let canonical = relay.to_string();
        {
            let configured = self.configured_relays.borrow();
            if !configured.contains(&canonical) {
                return Ok(false);
            }
            if configured.len() == 1 {
                return Err(JsError::new(
                    "refusing to remove the final configured relay",
                ));
            }
        }
        let removed = self.ep.remove_relay(&relay).await.is_some();
        if removed {
            self.configured_relays.borrow_mut().remove(&canonical);
        }
        Ok(removed)
    }

    /// How many inbound echo requests this node has served — proof the accept
    /// path fired, readable from the page after a peer connects.
    #[wasm_bindgen(js_name = servedCount)]
    pub fn served_count(&self) -> u64 {
        self.served.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// One JSON blob of everything the status UI needs.
    #[wasm_bindgen(js_name = statusJson)]
    pub fn status_json(&self) -> String {
        let address = address_status(self.ep.addr());
        serde_json::to_string(&Status {
            node_id: self.ep.id().to_string(),
            relay: address.relays.join(","),
            served: self.served_count(),
            outbound_connections: self.outbound_pool.connection_count(),
            addr_json: address.addr_json,
        })
        .unwrap_or_default()
    }

    /// Outbound test: dial `peer_addr_json` on `hive/browser/0`, send `msg` as
    /// an [`Op::Echo`] request, and return the echoed reply. Proves the browser
    /// node's OUTBOUND path (browser → relay → peer) in addition to the
    /// accept loop's inbound path.
    #[wasm_bindgen(js_name = echoTo)]
    pub async fn echo_to(&self, peer_addr_json: String, msg: String) -> Result<String, JsError> {
        if msg.len() > BROWSER_MAX_ECHO {
            return Err(JsError::new(&format!(
                "echo payload exceeds the {BROWSER_MAX_ECHO}-byte diagnostic cap"
            )));
        }
        let reply = self
            .request(peer_addr_json, Op::Echo, msg.as_bytes())
            .await?;
        String::from_utf8(reply).map_err(|e| JsError::new(&format!("reply not utf8: {e}")))
    }

    /// Register the trusted resolver called for every authorized
    /// [`Op::Invoke`] request. `handler` is
    /// `(codeDigest: string, requestJson: string) => Promise<string>` and must
    /// resolve `codeDigest` to a LOCALLY pinned artifact; executable source is
    /// never accepted from a peer. Installing a handler grants nobody.
    #[wasm_bindgen(js_name = setInvokeHandler)]
    pub fn set_invoke_handler(&self, handler: js_sys::Function) -> Result<(), JsError> {
        self.ensure_open()?;
        if !handler.is_function() {
            return Err(JsError::new("invoke handler must be a function"));
        }
        *self.invoke.borrow_mut() = Some(handler);
        Ok(())
    }

    /// Register the trusted local reader used by [`Op::AssetGet`]. Registration
    /// grants nobody; each caller still needs an exact endpoint/digest scope.
    #[wasm_bindgen(js_name = setAssetHandler)]
    pub fn set_asset_handler(&self, handler: js_sys::Function) -> Result<(), JsError> {
        self.ensure_open()?;
        if !handler.is_function() {
            return Err(JsError::new("asset handler must be a function"));
        }
        *self.asset.borrow_mut() = Some(handler);
        Ok(())
    }

    /// Grant one authenticated endpoint permission to pull one exact asset.
    #[wasm_bindgen(js_name = grantAsset)]
    pub fn grant_asset(&self, endpoint_id: String, digest: String) -> Result<bool, JsError> {
        self.ensure_open()?;
        let endpoint_id = parse_endpoint_id(&endpoint_id)?;
        validate_asset_digest(&digest)?;
        Ok(self
            .asset_grants
            .borrow_mut()
            .entry(endpoint_id)
            .or_default()
            .insert(digest))
    }

    /// Revoke one endpoint/asset capability; pooled connections re-read it on
    /// every chunk, so revocation also stops a transfer already in progress.
    #[wasm_bindgen(js_name = revokeAsset)]
    pub fn revoke_asset(&self, endpoint_id: String, digest: String) -> Result<bool, JsError> {
        self.ensure_open()?;
        let endpoint_id = parse_endpoint_id(&endpoint_id)?;
        validate_asset_digest(&digest)?;
        let mut grants = self.asset_grants.borrow_mut();
        let Some(scopes) = grants.get_mut(&endpoint_id) else {
            return Ok(false);
        };
        let removed = scopes.remove(&digest);
        if scopes.is_empty() {
            grants.remove(&endpoint_id);
        }
        Ok(removed)
    }

    /// Grant one TLS-authenticated iroh endpoint permission to invoke exactly
    /// one pinned code digest. The boot-empty map is the execution boundary;
    /// future platform admission is its sole production writer.
    #[wasm_bindgen(js_name = grantInvoker)]
    pub fn grant_invoker(&self, endpoint_id: String, code_digest: String) -> Result<bool, JsError> {
        self.ensure_open()?;
        let endpoint_id = parse_endpoint_id(&endpoint_id)?;
        validate_code_digest(&code_digest)?;
        Ok(self
            .grants
            .borrow_mut()
            .entry(endpoint_id)
            .or_default()
            .insert(code_digest))
    }

    /// Revoke one exact endpoint/digest scope. Idempotent: a valid but absent
    /// scope returns `false`; malformed IDs/digests throw without mutation.
    /// Existing connections re-read this map for every invoke stream.
    #[wasm_bindgen(js_name = revokeInvoker)]
    pub fn revoke_invoker(
        &self,
        endpoint_id: String,
        code_digest: String,
    ) -> Result<bool, JsError> {
        self.ensure_open()?;
        let endpoint_id = parse_endpoint_id(&endpoint_id)?;
        validate_code_digest(&code_digest)?;
        let mut grants = self.grants.borrow_mut();
        let Some(scopes) = grants.get_mut(&endpoint_id) else {
            return Ok(false);
        };
        let removed = scopes.remove(&code_digest);
        if scopes.is_empty() {
            grants.remove(&endpoint_id);
        }
        Ok(removed)
    }

    /// Clear every capability, abort and drain the platform-owned Rust task tree,
    /// then gracefully close the iroh endpoint. JavaScript handlers receive an
    /// AbortSignal; noncooperative Promises remain counted against the fixed
    /// handler pool until they actually settle.
    #[wasm_bindgen(js_name = close)]
    pub async fn close(&self) {
        self.invoke.borrow_mut().take();
        self.asset.borrow_mut().take();
        self.address_handler.borrow_mut().take();
        self.grants.borrow_mut().clear();
        self.asset_grants.borrow_mut().clear();
        self.outbound_pool.close_all();
        if !self.closed.replace(true) {
            self.tasks.shutdown().await;
            self.ep.close().await;
        } else {
            self.ep.closed().await;
        }
    }

    fn ensure_open(&self) -> Result<(), JsError> {
        if self.closed.get() || self.ep.is_closed() {
            return Err(JsError::new("browser node is closed"));
        }
        Ok(())
    }

    /// Outbound: dial `peer_addr_json` and ask it to invoke the locally pinned
    /// artifact named by `code_digest` against `request_json` (a Lagon-shaped
    /// `{method,path,headers,body}` envelope), returning raw response bytes as a
    /// UTF-8 string. No executable source crosses the wire.
    #[wasm_bindgen(js_name = invokeOn)]
    pub async fn invoke_on(
        &self,
        peer_addr_json: String,
        code_digest: String,
        request_json: String,
    ) -> Result<String, JsError> {
        self.ensure_open()?;
        let payload = encode_invoke(&code_digest, &request_json)
            .map_err(|e| JsError::new(&format!("bad invoke: {e}")))?;
        let reply = self.request(peer_addr_json, Op::Invoke, &payload).await?;
        String::from_utf8(reply).map_err(|e| JsError::new(&format!("reply not utf8: {e}")))
    }

    /// Pull a complete BLAKE3-addressed asset in bounded chunks. Every reply
    /// repeats the immutable total length. Bytes land directly in one capped JS
    /// buffer while BLAKE3 is updated incrementally, so wasm never retains a
    /// second whole-asset copy.
    #[wasm_bindgen(js_name = assetOn)]
    pub async fn asset_on(
        &self,
        peer_addr_json: String,
        digest: String,
    ) -> Result<js_sys::Uint8Array, JsError> {
        self.ensure_open()?;
        validate_asset_digest(&digest)?;
        let mut out = None;
        let mut _reservation = None;
        let mut total = None;
        let mut received = 0usize;
        let mut hasher = blake3::Hasher::new();
        loop {
            let payload = encode_asset_get(&digest, received as u64, ASSET_CHUNK_MAX)
                .map_err(|e| JsError::new(&format!("bad asset request: {e}")))?;
            let reply = self
                .request(peer_addr_json.clone(), Op::AssetGet, &payload)
                .await?;
            let (reply_total, chunk) = split_asset_reply(&reply)
                .map_err(|e| JsError::new(&format!("bad asset reply: {e}")))?;
            let reply_total = usize::try_from(reply_total)
                .map_err(|_| JsError::new("asset length exceeds this browser's address space"))?;
            if reply_total > BROWSER_MAX_ASSET {
                return Err(JsError::new(&format!(
                    "asset length {reply_total} exceeds the {BROWSER_MAX_ASSET}-byte browser cap"
                )));
            }
            match total {
                Some(known) if known != reply_total => {
                    return Err(JsError::new("asset length changed during transfer"));
                }
                None => {
                    let reservation = AssetReservation::acquire(
                        self.asset_bytes.clone(),
                        reply_total,
                    )
                    .ok_or_else(|| {
                        JsError::new(&format!(
                            "asset memory budget exhausted ({MAX_INFLIGHT_ASSET_BYTES} bytes in flight)"
                        ))
                    })?;
                    total = Some(reply_total);
                    out = Some(js_sys::Uint8Array::new_with_length(reply_total as u32));
                    _reservation = Some(reservation);
                }
                _ => {}
            }
            let next = received
                .checked_add(chunk.len())
                .ok_or_else(|| JsError::new("asset length overflow"))?;
            if next > reply_total || (chunk.is_empty() && next != reply_total) {
                return Err(JsError::new("asset peer returned an inconsistent range"));
            }
            out.as_ref()
                .expect("asset destination is initialized with its total")
                .subarray(received as u32, next as u32)
                .copy_from(chunk);
            hasher.update(chunk);
            received = next;
            if received == reply_total {
                break;
            }
        }
        if hasher.finalize().to_hex().as_str() != digest {
            return Err(JsError::new("asset BLAKE3 mismatch"));
        }
        Ok(out.expect("asset peer supplied a first reply"))
    }

    /// Shared request/response plumbing for every outbound op. Dial, stream-open,
    /// write, and reply phases each have their own bounded idle deadline, so a
    /// large frame making progress is not cancelled by a transfer-size-blind
    /// whole-request timer. Once a stream exists, its Drop guard sends explicit
    /// DEADLINE_EXCEEDED cancellation on timeout or caller-future abandonment.
    async fn request(
        &self,
        peer_addr_json: String,
        op: Op,
        op_payload: &[u8],
    ) -> Result<Vec<u8>, JsError> {
        self.ensure_open()?;
        if op_payload.len() > BROWSER_MAX_FRAME - 1 {
            return Err(JsError::new(&format!(
                "request payload exceeds the {}-byte op payload cap",
                BROWSER_MAX_FRAME - 1
            )));
        }
        let frame = encode_request(op, op_payload);
        let mut retried_open = false;
        let (lease, send, recv) = loop {
            let lease = self.outbound_pool.acquire(&peer_addr_json).await?;
            match n0_future::time::timeout(OUTBOUND_STREAM_OPEN_TIMEOUT, lease.connection.open_bi())
                .await
            {
                Ok(Ok((send, recv))) => break (lease, send, recv),
                Ok(Err(_)) if !retried_open => {
                    let endpoint_id = lease.endpoint_id.clone();
                    let stable_id = lease.connection.stable_id();
                    self.outbound_pool.evict(
                        &endpoint_id,
                        stable_id,
                        b"browser outbound stream open failed",
                    );
                    drop(lease);
                    retried_open = true;
                }
                Ok(Err(error)) => {
                    return Err(JsError::new(&format!("open_bi failed: {error}")));
                }
                Err(_) => {
                    return Err(JsError::new(
                        "browser outbound stream-open deadline exceeded",
                    ));
                }
            }
        };
        let mut io = OutboundRequestGuard::new(lease, send, recv);
        if let Err(error) = write_outbound_with_progress(
            io.send.as_mut().expect("outbound send stream is present"),
            &frame,
        )
        .await
        {
            io.cancel(proto_reset::DEADLINE_EXCEEDED);
            return Err(JsError::new(&error));
        }
        if let Err(error) = io
            .send
            .as_mut()
            .expect("outbound send stream is present")
            .finish()
        {
            io.cancel(proto_reset::DEADLINE_EXCEEDED);
            return Err(JsError::new(&format!("finish: {error}")));
        }
        let mut lenb = [0u8; 4];
        if let Err(error) = read_outbound_with_progress(
            io.recv
                .as_mut()
                .expect("outbound receive stream is present"),
            &mut lenb,
            "read reply len",
        )
        .await
        {
            io.cancel(proto_reset::DEADLINE_EXCEEDED);
            return Err(JsError::new(&error));
        }
        let len = match check_len(lenb) {
            Ok(len) => len,
            Err(error) => {
                io.cancel(error.reset_code());
                return Err(JsError::new(&format!("reply frame: {error}")));
            }
        };
        let mut reply = vec![0u8; len];
        if let Err(error) = read_outbound_with_progress(
            io.recv
                .as_mut()
                .expect("outbound receive stream is present"),
            &mut reply,
            "read reply body",
        )
        .await
        {
            io.cancel(proto_reset::DEADLINE_EXCEEDED);
            return Err(JsError::new(&error));
        }
        if let Err((code, error)) = read_outbound_eof(
            io.recv
                .as_mut()
                .expect("outbound receive stream is present"),
        )
        .await
        {
            io.cancel(code);
            return Err(JsError::new(&error));
        }
        io.finish_success();
        Ok(reply)
    }
}

/// Spawn the `hive/browser/0` accept loop. One connection → many bi streams;
/// each stream is a `[u32 len][op][payload]` request dispatched on its op byte.
/// No gossip, no join, no trust arms — this ALPN has no privileged surface, and
/// growing one belongs behind admission (`bn-impl-mesh-admission`), not here.
///
/// Every refusal is loud and carries a distinct `hive-browser-proto` reset code.
/// There is deliberately no "unknown op falls back to echo" arm: that would
/// answer a future protocol version's request with its own raw bytes and look
/// like success to the caller.
async fn read_with_progress_deadline(recv: &mut RecvStream, mut out: &mut [u8]) -> Result<(), u32> {
    while !out.is_empty() {
        match n0_future::time::timeout(STREAM_READ_IDLE_TIMEOUT, recv.read(out)).await {
            Ok(Ok(Some(read))) if read > 0 => out = &mut out[read..],
            Ok(Ok(_)) | Ok(Err(_)) => return Err(proto_reset::MALFORMED_PAYLOAD),
            Err(_) => return Err(proto_reset::DEADLINE_EXCEEDED),
        }
    }
    Ok(())
}

async fn read_eof_with_progress_deadline(recv: &mut RecvStream) -> Result<(), u32> {
    let mut trailing = [0u8; 1];
    match n0_future::time::timeout(STREAM_READ_IDLE_TIMEOUT, recv.read(&mut trailing)).await {
        Ok(Ok(None)) => Ok(()),
        Ok(Ok(Some(_))) | Ok(Err(_)) => Err(proto_reset::MALFORMED_PAYLOAD),
        Err(_) => Err(proto_reset::DEADLINE_EXCEEDED),
    }
}

fn spawn_accept_loop(
    ep: Endpoint,
    served: Arc<std::sync::atomic::AtomicU64>,
    invoke: InvokeHandler,
    asset: AssetHandler,
    grants: InvokerGrants,
    asset_grants: AssetGrants,
    tasks: TaskGroup,
) {
    let connection_slots = Arc::new(tokio::sync::Semaphore::new(MAX_PENDING_CONNECTIONS));
    let stream_slots = Arc::new(tokio::sync::Semaphore::new(MAX_ACTIVE_STREAMS));
    let echo_slots = Arc::new(tokio::sync::Semaphore::new(MAX_ACTIVE_ECHO));
    let peer_connections: PeerConnections = Rc::new(RefCell::new(BTreeMap::new()));
    let peer_streams: PeerStreams = Rc::new(RefCell::new(BTreeMap::new()));
    let connection_tasks = tasks.clone();
    tasks.spawn(async move {
        while let Some(incoming) = ep.accept().await {
            let Ok(connection_slot) = connection_slots.clone().try_acquire_owned() else {
                tracing::warn!("browser connection refused: node connection limit reached");
                incoming.refuse();
                continue;
            };
            let served = served.clone();
            let invoke = invoke.clone();
            let asset = asset.clone();
            let grants = grants.clone();
            let asset_grants = asset_grants.clone();
            let stream_slots = stream_slots.clone();
            let echo_slots = echo_slots.clone();
            let peer_connections = peer_connections.clone();
            let peer_streams = peer_streams.clone();
            let stream_tasks = connection_tasks.clone();
            connection_tasks.spawn(async move {
                let _connection_slot = connection_slot;
                let conn = match n0_future::time::timeout(HANDSHAKE_TIMEOUT, incoming).await {
                    Ok(Ok(conn)) => conn,
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "browser accept: handshake failed");
                        return;
                    }
                    Err(_) => {
                        tracing::warn!("browser accept: handshake deadline exceeded");
                        return;
                    }
                };
                if conn.alpn() != BROWSER_ALPN {
                    conn.close(proto_reset::UNEXPECTED_ALPN.into(), b"unexpected alpn");
                    return;
                }
                let remote_id = conn.remote_id();
                let Some(_peer_connection) =
                    PeerConnectionGuard::acquire(peer_connections, remote_id.clone())
                else {
                    conn.close(
                        proto_reset::OVERLOADED.into(),
                        b"browser peer connection limit reached",
                    );
                    return;
                };
                let connection_stream_slots =
                    Arc::new(tokio::sync::Semaphore::new(MAX_STREAMS_PER_CONNECTION));
                let connection_activity = Rc::new(RefCell::new(ConnectionActivity {
                    active: 0,
                    idle_since: Some(Instant::now()),
                }));
                loop {
                    let wait = {
                        let activity = connection_activity.borrow();
                        if activity.active > 0 {
                            CONNECTION_IDLE_TIMEOUT
                        } else {
                            CONNECTION_IDLE_TIMEOUT.saturating_sub(
                                activity
                                    .idle_since
                                    .map(|since| since.elapsed())
                                    .unwrap_or_default(),
                            )
                        }
                    };
                    let (mut send, mut recv) =
                        match n0_future::time::timeout(wait, conn.accept_bi()).await {
                            Ok(Ok(streams)) => streams,
                            Ok(Err(_)) => break,
                            Err(_) => {
                                let idle = {
                                    let activity = connection_activity.borrow();
                                    activity.active == 0
                                        && activity.idle_since.is_some_and(|since| {
                                            since.elapsed() >= CONNECTION_IDLE_TIMEOUT
                                        })
                                };
                                if idle {
                                    conn.close(
                                        proto_reset::DEADLINE_EXCEEDED.into(),
                                        b"browser connection idle",
                                    );
                                    break;
                                }
                                continue;
                            }
                        };
                    let peer_stream =
                        PeerStreamGuard::acquire(peer_streams.clone(), remote_id.clone());
                    let permits = stream_slots
                        .clone()
                        .try_acquire_owned()
                        .ok()
                        .zip(connection_stream_slots.clone().try_acquire_owned().ok())
                        .zip(peer_stream);
                    let Some(((node_stream_slot, connection_stream_slot), peer_stream)) = permits
                    else {
                        reject_stream(&mut send, &mut recv, proto_reset::OVERLOADED);
                        continue;
                    };
                    let served = served.clone();
                    let invoke = invoke.clone();
                    let asset = asset.clone();
                    let grants = grants.clone();
                    let asset_grants = asset_grants.clone();
                    let remote_id = remote_id.clone();
                    let echo_slots = echo_slots.clone();
                    let stream_conn = conn.clone();
                    let connection_activity =
                        ConnectionStreamGuard::acquire(connection_activity.clone());
                    stream_tasks.spawn(async move {
                        let _node_stream_slot = node_stream_slot;
                        let _connection_stream_slot = connection_stream_slot;
                        let _peer_stream = peer_stream;
                        let _connection_activity = connection_activity;
                        let request = async {
                            let mut lenb = [0u8; 4];
                            read_with_progress_deadline(&mut recv, &mut lenb).await?;
                            let len = check_len(lenb).map_err(|error| error.reset_code())?;
                            if len == 0 {
                                return Err(proto_reset::MALFORMED_PAYLOAD);
                            }
                            let mut opb = [0u8; 1];
                            read_with_progress_deadline(&mut recv, &mut opb).await?;
                            let op = Op::from_byte(opb[0]).map_err(|error| error.reset_code())?;
                            if op == Op::Echo && len - 1 > BROWSER_MAX_ECHO {
                                return Err(proto_reset::FRAME_TOO_LARGE);
                            }
                            let endpoint_granted = match op {
                                Op::Echo => true,
                                Op::Invoke => grants.borrow().contains_key(&remote_id),
                                Op::AssetGet => asset_grants.borrow().contains_key(&remote_id),
                            };
                            if !endpoint_granted {
                                return Err(proto_reset::FORBIDDEN);
                            }
                            let mut payload = vec![0u8; len - 1];
                            read_with_progress_deadline(&mut recv, &mut payload).await?;
                            read_eof_with_progress_deadline(&mut recv).await?;
                            Ok::<(Op, Vec<u8>), u32>((op, payload))
                        }
                        .await;

                        let (op, payload) = match request {
                            Ok(request) => request,
                            Err(code) => {
                                reject_stream(&mut send, &mut recv, code);
                                return;
                            }
                        };

                        let echo_permit = if op == Op::Echo {
                            match echo_slots.clone().try_acquire_owned() {
                                Ok(permit) => Some(permit),
                                Err(_) => {
                                    reject_stream(&mut send, &mut recv, proto_reset::OVERLOADED);
                                    return;
                                }
                            }
                        } else {
                            None
                        };
                        let _echo_permit = echo_permit;
                        let out = if op == Op::Echo {
                            Ok(payload)
                        } else {
                            let permit = match HANDLER_SLOTS.try_acquire() {
                                Ok(permit) => permit,
                                Err(_) => {
                                    reject_stream(&mut send, &mut recv, proto_reset::OVERLOADED);
                                    return;
                                }
                            };
                            let mut abort = match HandlerAbortGuard::new() {
                                Ok(abort) => abort,
                                Err(code) => {
                                    reject_stream(&mut send, &mut recv, code);
                                    return;
                                }
                            };
                            let signal = abort.signal();
                            let handler = match op {
                                Op::Invoke => {
                                    let invoke = invoke.clone();
                                    let grants = grants.clone();
                                    let remote_id = remote_id.clone();
                                    let (code_digest, request_json) =
                                        match parse_invoke_owned(&payload, &remote_id) {
                                            Ok(request) => request,
                                            Err(code) => {
                                                reject_stream(&mut send, &mut recv, code);
                                                return;
                                            }
                                        };
                                    drop(payload);
                                    watch_handler(
                                        async move {
                                            run_invoke(
                                                &invoke,
                                                &grants,
                                                remote_id,
                                                code_digest,
                                                request_json,
                                                signal,
                                            )
                                            .await
                                        },
                                        permit,
                                    )
                                }
                                Op::AssetGet => {
                                    let asset = asset.clone();
                                    let asset_grants = asset_grants.clone();
                                    let remote_id = remote_id.clone();
                                    let payload = payload;
                                    watch_handler(
                                        async move {
                                            run_asset(
                                                &asset,
                                                &asset_grants,
                                                remote_id,
                                                &payload,
                                                signal,
                                            )
                                            .await
                                        },
                                        permit,
                                    )
                                }
                                Op::Echo => unreachable!(),
                            };
                            let handler_wait = async {
                                match n0_future::time::timeout(HANDLER_TIMEOUT, handler).await {
                                    Ok(Ok(result)) => HandlerWait::Completed(result),
                                    Ok(Err(_)) => {
                                        HandlerWait::Completed(Err(proto_reset::HANDLER_FAILED))
                                    }
                                    Err(_) => HandlerWait::Cancelled,
                                }
                            };
                            let peer_wait = async {
                                n0_future::future::race(
                                    async {
                                        let _ = send.stopped().await;
                                    },
                                    async {
                                        stream_conn.closed().await;
                                    },
                                )
                                .await;
                                HandlerWait::Cancelled
                            };
                            match n0_future::future::race(handler_wait, peer_wait).await {
                                HandlerWait::Completed(result) => {
                                    abort.disarm();
                                    result
                                }
                                HandlerWait::Cancelled => {
                                    abort.abort();
                                    Err(proto_reset::DEADLINE_EXCEEDED)
                                }
                            }
                        };
                        let out = match out {
                            Ok(out) => out,
                            Err(code) => {
                                reject_stream(&mut send, &mut recv, code);
                                return;
                            }
                        };
                        if out.len() > BROWSER_MAX_FRAME {
                            reject_stream(&mut send, &mut recv, proto_reset::FRAME_TOO_LARGE);
                            return;
                        }
                        match n0_future::time::timeout(
                            STREAM_WRITE_TIMEOUT,
                            write_reply(&mut send, &out),
                        )
                        .await
                        {
                            Ok(Ok(())) => {}
                            Ok(Err(())) => return,
                            Err(_) => {
                                reject_stream(&mut send, &mut recv, proto_reset::DEADLINE_EXCEEDED);
                                return;
                            }
                        }
                        if send.finish().is_err() {
                            return;
                        }
                        served.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    });
                }
            });
        }
    });
}

/// Run one [`Op::Invoke`] payload against the trusted local artifact resolver.
///
/// `Err` is the reset code to refuse the stream with, so each distinguishable
/// failure reaches the caller as a distinct code instead of collapsing into an
/// empty reply. Authorization is re-read at the final synchronous point before
/// `Function::call3`; pooled connections never cache a grant.
///
/// The handler and grant `Rc`s are NOT borrowed across the await. `RefCell` has
/// no async awareness, so keeping either borrow alive over `JsFuture` would
/// panic on concurrent invoke/grant/revoke activity.
fn parse_invoke_owned(payload: &[u8], remote_id: &EndpointId) -> Result<(String, String), u32> {
    let (code_digest, request_json) = split_invoke(payload).map_err(|error| {
        tracing::warn!(error = %error, remote = %remote_id, "browser invoke: bad payload");
        error.reset_code()
    })?;
    Ok((code_digest.to_owned(), request_json.to_owned()))
}

async fn run_invoke(
    invoke: &InvokeHandler,
    grants: &InvokerGrants,
    remote_id: EndpointId,
    code_digest: String,
    request_json: String,
    signal: web_sys::AbortSignal,
) -> Result<Vec<u8>, u32> {
    let allowed = grants
        .borrow()
        .get(&remote_id)
        .is_some_and(|scopes| scopes.contains(&code_digest));
    if !allowed {
        tracing::warn!(remote = %remote_id, digest = code_digest, "browser invoke forbidden");
        return Err(proto_reset::FORBIDDEN);
    }

    let handler = invoke.borrow().clone();
    let Some(handler) = handler else {
        tracing::warn!("browser invoke: no handler registered (call setInvokeHandler first)");
        return Err(proto_reset::NO_HANDLER);
    };

    let ret = handler
        .call3(
            &JsValue::NULL,
            &JsValue::from_str(&code_digest),
            &JsValue::from_str(&request_json),
            signal.as_ref(),
        )
        .map_err(|e| {
            tracing::warn!(error = ?e, "browser invoke: handler threw");
            proto_reset::HANDLER_FAILED
        })?;
    drop(request_json);

    // The handler may be sync or async; `Promise::resolve` normalises both, so
    // a handler returning a plain string is not a silent failure.
    let resolved = JsFuture::from(js_sys::Promise::resolve(&ret))
        .await
        .map_err(|e| {
            tracing::warn!(error = ?e, "browser invoke: handler rejected");
            proto_reset::HANDLER_FAILED
        })?;

    let allowed = grants
        .borrow()
        .get(&remote_id)
        .is_some_and(|scopes| scopes.contains(&code_digest));
    if !allowed {
        tracing::warn!(remote = %remote_id, digest = code_digest, "browser invoke revoked while handler was running");
        return Err(proto_reset::FORBIDDEN);
    }
    bounded_utf8(resolved)
}

async fn run_asset(
    handler: &AssetHandler,
    grants: &AssetGrants,
    remote_id: EndpointId,
    payload: &[u8],
    signal: web_sys::AbortSignal,
) -> Result<Vec<u8>, u32> {
    let (digest, offset, max_len) = split_asset_get(payload).map_err(|e| {
        tracing::warn!(error = %e, remote = %remote_id, "browser asset: bad payload");
        e.reset_code()
    })?;
    let allowed = grants
        .borrow()
        .get(&remote_id)
        .is_some_and(|scopes| scopes.contains(digest));
    if !allowed {
        tracing::warn!(remote = %remote_id, digest, "browser asset forbidden");
        return Err(proto_reset::FORBIDDEN);
    }
    if offset > js_sys::Number::MAX_SAFE_INTEGER as u64 {
        return Err(proto_reset::MALFORMED_PAYLOAD);
    }
    let Some(handler) = handler.borrow().clone() else {
        return Err(proto_reset::NO_HANDLER);
    };
    let args = js_sys::Array::new();
    args.push(&JsValue::from_str(digest));
    args.push(&JsValue::from_f64(offset as f64));
    args.push(&JsValue::from_f64(max_len as f64));
    args.push(signal.as_ref());
    let value = handler
        .apply(&JsValue::NULL, &args)
        .map_err(|_| proto_reset::HANDLER_FAILED)?;
    let resolved = JsFuture::from(js_sys::Promise::resolve(&value))
        .await
        .map_err(|_| proto_reset::HANDLER_FAILED)?;
    let allowed = grants
        .borrow()
        .get(&remote_id)
        .is_some_and(|scopes| scopes.contains(digest));
    if !allowed {
        tracing::warn!(remote = %remote_id, digest, "browser asset revoked while handler was running");
        return Err(proto_reset::FORBIDDEN);
    }
    let total = js_sys::Reflect::get(&resolved, &JsValue::from_str("total"))
        .ok()
        .and_then(|v| v.as_f64())
        .filter(|n| n.is_finite() && *n >= 0.0 && n.fract() == 0.0)
        .ok_or(proto_reset::HANDLER_FAILED)?;
    if total > js_sys::Number::MAX_SAFE_INTEGER {
        return Err(proto_reset::HANDLER_FAILED);
    }
    let bytes = js_sys::Reflect::get(&resolved, &JsValue::from_str("bytes"))
        .map_err(|_| proto_reset::HANDLER_FAILED)?;
    if bytes.is_null() || bytes.is_undefined() {
        return Err(proto_reset::HANDLER_FAILED);
    }
    let chunk = js_sys::Uint8Array::new(&bytes).to_vec();
    if chunk.len() > max_len || offset.saturating_add(chunk.len() as u64) > total as u64 {
        return Err(proto_reset::HANDLER_FAILED);
    }
    encode_asset_reply(total as u64, &chunk).map_err(|e| e.reset_code())
}

/// Parse the cryptographic endpoint identity used by iroh's completed TLS
/// handshake. Keeping the typed key in the grants map avoids string aliases
/// (hex/base32/case) that could make revocation miss the original grant.
fn parse_endpoint_id(raw: &str) -> Result<EndpointId, JsError> {
    raw.trim()
        .parse::<EndpointId>()
        .map_err(|e| JsError::new(&format!("invalid endpoint id: {e}")))
}

fn validate_code_digest(digest: &str) -> Result<(), JsError> {
    if valid_function_digest(digest) {
        Ok(())
    } else {
        Err(JsError::new(
            "code digest must be exactly 64 lowercase hexadecimal bytes",
        ))
    }
}

fn validate_asset_digest(digest: &str) -> Result<(), JsError> {
    if valid_blake3_digest(digest) {
        Ok(())
    } else {
        Err(JsError::new(
            "asset digest must be exactly 64 lowercase hexadecimal bytes",
        ))
    }
}

/// Render bytes as lowercase hex.
fn bytes_to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Parse 64 hex chars into 32 bytes; `None` on any malformed input.
fn hex_to_32(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16)?;
        let lo = (chunk[1] as char).to_digit(16)?;
        out[i] = (hi * 16 + lo) as u8;
    }
    Some(out)
}
