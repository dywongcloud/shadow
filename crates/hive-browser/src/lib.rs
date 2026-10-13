//! A mesh node that runs inside a browser tab (wasm32): a real iroh `Endpoint` over the relay-only
//! WebSocket transport, identity via a pkarr relay, accepting only the `hive/browser/0` op set from
//! `hive-browser-proto` — no gossip/join/raw arms by construction, so a tab is a real bidirectional
//! peer with no fleet authority. All traffic rides a relay WebSocket, end-to-end encrypted.

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

/// The registered invoke handler `(codeDigest, requestJson, signal) => Promise<string>`, resolving
/// the digest to a LOCALLY pinned artifact; `Rc<RefCell>` because a tab is single-threaded and
/// `js_sys::Function` is not `Send`.
type InvokeHandler = Rc<RefCell<Option<js_sys::Function>>>;

/// Live endpoint-address observer registered by the trusted worker. The callback
/// receives one JSON object whenever iroh's connected home-relay set changes.
type AddressHandler = Rc<RefCell<Option<js_sys::Function>>>;

/// Trusted local asset reader:
/// `(digest: string, offset: number, maxLen: number, signal: AbortSignal) => Promise<{total, bytes}>`.
type AssetHandler = Rc<RefCell<Option<js_sys::Function>>>;

/// Trusted local CRR sync responder `(requestBytes, signal) => Promise<Uint8Array>`; the JS glue
/// owns the CRR semantics, this half knows only frames.
type CrrSyncHandler = Rc<RefCell<Option<js_sys::Function>>>;

/// Exact execution scopes granted per TLS-authenticated endpoint; shared, not snapshotted, so
/// revocation reaches pooled connections.
type InvokerGrants = Rc<RefCell<BTreeMap<EndpointId, BTreeSet<String>>>>;
type AssetGrants = Rc<RefCell<BTreeMap<EndpointId, BTreeSet<String>>>>;
/// Endpoints permitted to drive `Op::CrrSync` against this node's replica; scopeless because the
/// fleet-resolved grant already pins the one database this node replicates.
type CrrSyncGrants = Rc<RefCell<BTreeSet<EndpointId>>>;
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
/// Budget for the best-effort "is the home relay up yet?" wait in `boot()`: larger than one
/// worst-case net_report round (~13 s+) and non-fatal on expiry.
const RELAY_ONLINE_TIMEOUT: Duration = Duration::from_secs(20);
const CONNECTION_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const STREAM_READ_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const STREAM_WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const OUTBOUND_STREAM_OPEN_TIMEOUT: Duration = Duration::from_secs(60);
const OUTBOUND_IO_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const HANDLER_TIMEOUT: Duration = Duration::from_secs(10);
/// One full maximum-size frame per stream (CrrSync's 4 MiB), so a round never degenerates into
/// window-update round trips.
const BROWSER_STREAM_WINDOW: u32 = (BROWSER_MAX_CRR_FRAME + 4) as u32;
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
) -> Result<(), (u32, String)> {
    while !payload.is_empty() {
        match n0_future::time::timeout(OUTBOUND_IO_IDLE_TIMEOUT, recv.read(payload)).await {
            Ok(Ok(Some(read))) if read > 0 => payload = &mut payload[read..],
            Ok(Ok(_)) => {
                return Err((
                    proto_reset::MALFORMED_PAYLOAD,
                    format!("{stage}: stream ended before frame completed"),
                ))
            }
            Ok(Err(error)) => {
                return Err((
                    proto_reset::MALFORMED_PAYLOAD,
                    format!("{stage}: {error}"),
                ))
            }
            Err(_) => {
                return Err((
                    proto_reset::DEADLINE_EXCEEDED,
                    format!("{stage}: idle deadline exceeded"),
                ))
            }
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
            proto_reset::MALFORMED_PAYLOAD,
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

/// The browser node's dedicated ALPN and the rest of the wire contract, from `hive-browser-proto`
/// (shared with `hive-p2p`, so there is no second copy to keep in sync).
pub use hive_browser_proto::BROWSER_ALPN;

use hive_browser_proto::{
    check_len_for, encode_asset_get, encode_asset_reply, encode_invoke, encode_request,
    reset as proto_reset, split_asset_get, split_asset_reply, split_invoke, valid_blake3_digest,
    valid_function_digest, Op, ASSET_CHUNK_MAX, BROWSER_MAX_ASSET, BROWSER_MAX_CRR_FRAME,
    BROWSER_MAX_ECHO, BROWSER_MAX_FRAME,
};

#[wasm_bindgen(start)]
pub fn on_load() {
    console_error_panic_hook::set_once();
    let _ = std::panic::catch_unwind(|| tracing_wasm::set_as_global_default());
}

/// Canonical content address shared by function artifacts and asset bytes.
#[wasm_bindgen(js_name = blake3Hex)]
pub fn blake3_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Version of this wasm bundle, compared by `run-node-worker.js` against its own
/// `WASM_BUNDLE_VERSION` right after `init()` so a stale service-worker-cached `.wasm` paired with
/// fresh JS glue is detected; bump whenever the wasm-exposed surface changes incompatibly.
#[wasm_bindgen(js_name = wasmBundleVersion)]
pub fn wasm_bundle_version() -> u32 {
    15
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
    /// Inbound CRR sync responder; `Op::CrrSync` refuses NO_HANDLER until set.
    crr_sync: CrrSyncHandler,
    /// Boot-empty, typed endpoint → pinned-code-digest execution scopes.
    grants: InvokerGrants,
    /// Separate endpoint → asset-digest scopes. A function grant can never read
    /// an asset accidentally, even when both identifiers are 64 hex bytes.
    asset_grants: AssetGrants,
    /// Endpoints allowed to run CRR sync rounds against this node's replica.
    crr_sync_grants: CrrSyncGrants,
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
        self.invoke.borrow_mut().take();
        self.asset.borrow_mut().take();
        self.crr_sync.borrow_mut().take();
        self.address_handler.borrow_mut().take();
        self.grants.borrow_mut().clear();
        self.asset_grants.borrow_mut().clear();
        self.crr_sync_grants.borrow_mut().clear();
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
    /// Boot a node: bind against `relay_urls` (comma-separated `http(s)://` URLs, same convention as
    /// `HIVE_RELAY_URLS`), optionally wire pkarr `discovery_url`, restore identity from `secret_hex`
    /// (32-byte ed25519 seed) or generate one, and spawn the accept loop.
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

        let transport = QuicTransportConfig::builder()
            .max_concurrent_bidi_streams(VarInt::from_u32(MAX_ACTIVE_STREAMS as u32))
            .stream_receive_window(VarInt::from_u32(BROWSER_STREAM_WINDOW))
            .receive_window(VarInt::from_u32(BROWSER_CONNECTION_WINDOW))
            .build();
        let mut builder = Endpoint::builder(iroh::endpoint::presets::Minimal)
            .relay_mode(RelayMode::Custom(map))
            .alpns(vec![BROWSER_ALPN.to_vec()])
            .transport_config(transport);

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

        if n0_future::time::timeout(RELAY_ONLINE_TIMEOUT, ep.online())
            .await
            .is_err()
        {
            tracing::warn!(
                timeout_secs = RELAY_ONLINE_TIMEOUT.as_secs(),
                "browser relay not online yet — continuing; the address handler reports readiness when the home relay lands"
            );
        }

        let served = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let invoke: InvokeHandler = Rc::new(RefCell::new(None));
        let address_handler: AddressHandler = Rc::new(RefCell::new(None));
        let asset: AssetHandler = Rc::new(RefCell::new(None));
        let crr_sync: CrrSyncHandler = Rc::new(RefCell::new(None));
        let grants: InvokerGrants = Rc::new(RefCell::new(BTreeMap::new()));
        let asset_grants: AssetGrants = Rc::new(RefCell::new(BTreeMap::new()));
        let crr_sync_grants: CrrSyncGrants = Rc::new(RefCell::new(BTreeSet::new()));
        let closed = Rc::new(Cell::new(false));
        let tasks = TaskGroup::new();
        let outbound_pool = OutboundPool::new(ep.clone(), tasks.clone());
        let asset_bytes = Rc::new(Cell::new(0));
        spawn_accept_loop(
            ep.clone(),
            served.clone(),
            invoke.clone(),
            asset.clone(),
            crr_sync.clone(),
            grants.clone(),
            asset_grants.clone(),
            crr_sync_grants.clone(),
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
            crr_sync,
            grants,
            asset_grants,
            crr_sync_grants,
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

    /// Proof-of-possession for admission: sign `"{endpoint_id}:{challenge_ms}"` with this node's own
    /// ed25519 key (128 hex chars); the backend bounds replay by requiring `challenge_ms` inside a
    /// tight freshness window.
    #[wasm_bindgen(js_name = signAdmission)]
    pub fn sign_admission(&self, challenge_ms: String) -> String {
        let endpoint_id = self.ep.id().to_string();
        let message = format!("{endpoint_id}:{challenge_ms}");
        let sig = self.ep.secret_key().sign(message.as_bytes());
        bytes_to_hex(&sig.to_bytes())
    }

    /// Sign one peer-mesh signalling envelope: `context` (built by `www/peer-mesh.js::envelopeContext`)
    /// binds both endpoint ids, the session id, a timestamp and the DTLS certificate fingerprint, so
    /// the signalling relay cannot substitute its own fingerprint. Domain-separated
    /// (`hive-browser-webrtc-v1\0`) so a mesh signature can never replay as an admission proof, nor
    /// the reverse.
    #[wasm_bindgen(js_name = signMeshEnvelope)]
    pub fn sign_mesh_envelope(&self, context: String) -> Result<String, JsError> {
        self.ensure_open()?;
        let message = mesh_signing_message(&context)?;
        let sig = self.ep.secret_key().sign(&message);
        Ok(bytes_to_hex(&sig.to_bytes()))
    }

    /// The raw 32-byte ed25519 seed as 64 hex chars — the only way key material leaves this module;
    /// the caller must wrap it in a non-extractable WebCrypto key before it touches durable storage
    /// and never log it.
    #[wasm_bindgen(js_name = secretHex)]
    pub fn secret_hex(&self) -> String {
        bytes_to_hex(&self.ep.secret_key().to_bytes())
    }

    /// Serialized `EndpointAddr` a peer needs to dial this node; THROWS rather than returning an
    /// address with no transport hints, so un-readiness is named here instead of at a remote dial.
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

    /// Subscribe to the live dialable address: `{online, relays, addrJson}` immediately and on every
    /// home-relay change; an offline update must stop any previous advertisement.
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

    /// Remove one configured relay at runtime; iroh migrates to another and `setAddressHandler`
    /// reports the new address. The final relay is non-removable.
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

    /// Outbound test: dial `peer_addr_json` on `hive/browser/0` and echo `msg`, proving the
    /// browser → relay → peer path.
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

    /// Register the trusted resolver for authorized [`Op::Invoke`] requests; it must resolve
    /// `codeDigest` to a LOCALLY pinned artifact (source never comes from a peer). Installing a
    /// handler grants nobody.
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

    /// Grant one TLS-authenticated endpoint permission to invoke exactly one pinned digest; the
    /// boot-empty map is the execution boundary.
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

    /// Revoke one exact endpoint/digest scope; idempotent (absent scope → `false`, malformed input
    /// throws without mutation). Existing connections re-read the map per stream.
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

    /// Clear every capability, abort and drain the Rust task tree, then close the endpoint; JS
    /// handlers get an AbortSignal and uncooperative Promises stay counted until they settle.
    #[wasm_bindgen(js_name = close)]
    pub async fn close(&self) {
        self.invoke.borrow_mut().take();
        self.asset.borrow_mut().take();
        self.crr_sync.borrow_mut().take();
        self.address_handler.borrow_mut().take();
        self.grants.borrow_mut().clear();
        self.asset_grants.borrow_mut().clear();
        self.crr_sync_grants.borrow_mut().clear();
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

    /// Outbound: ask `peer_addr_json` to invoke its locally pinned `code_digest` against
    /// `request_json` (a `{method,path,headers,body}` envelope); no source crosses the wire.
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

    /// Pull a complete BLAKE3-addressed asset in bounded chunks into one capped JS buffer while
    /// BLAKE3 is updated incrementally; wasm never holds a second whole-asset copy.
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

    /// Register the trusted local responder for inbound [`Op::CrrSync`] rounds `(requestBytes,
    /// signal) => Promise<Uint8Array>`; installing it grants nobody — each caller needs `grantCrrSync`.
    #[wasm_bindgen(js_name = setCrrSyncHandler)]
    pub fn set_crr_sync_handler(&self, handler: js_sys::Function) -> Result<(), JsError> {
        self.ensure_open()?;
        if !handler.is_function() {
            return Err(JsError::new("crr sync handler must be a function"));
        }
        *self.crr_sync.borrow_mut() = Some(handler);
        Ok(())
    }

    /// Grant one TLS-authenticated fleet endpoint permission to run CRR sync rounds; boot-empty until
    /// the admission's server-derived peer set is granted.
    #[wasm_bindgen(js_name = grantCrrSync)]
    pub fn grant_crr_sync(&self, endpoint_id: String) -> Result<bool, JsError> {
        self.ensure_open()?;
        let endpoint_id = parse_endpoint_id(&endpoint_id)?;
        Ok(self.crr_sync_grants.borrow_mut().insert(endpoint_id))
    }

    /// Revoke one endpoint's sync grant. Existing pooled connections re-read
    /// the set per request, so revocation also stops a session in progress.
    #[wasm_bindgen(js_name = revokeCrrSync)]
    pub fn revoke_crr_sync(&self, endpoint_id: String) -> Result<bool, JsError> {
        self.ensure_open()?;
        let endpoint_id = parse_endpoint_id(&endpoint_id)?;
        Ok(self.crr_sync_grants.borrow_mut().remove(&endpoint_id))
    }

    /// Outbound CRR sync round: send one encoded `Op::CrrSync` request to `peer_addr_json` and return
    /// the reply frame verbatim; pure transport.
    #[wasm_bindgen(js_name = crrSyncOn)]
    pub async fn crr_sync_on(
        &self,
        peer_addr_json: String,
        request: js_sys::Uint8Array,
    ) -> Result<js_sys::Uint8Array, JsError> {
        self.ensure_open()?;
        let payload = request.to_vec();
        if payload.len() > BROWSER_MAX_CRR_FRAME - 1 {
            return Err(JsError::new(&format!(
                "crr sync payload exceeds the {}-byte op payload cap",
                BROWSER_MAX_CRR_FRAME - 1
            )));
        }
        let reply = self.request(peer_addr_json, Op::CrrSync, &payload).await?;
        Ok(js_sys::Uint8Array::from(reply.as_slice()))
    }

    /// Shared plumbing for every outbound op: dial, stream-open, write and reply each have their own
    /// bounded idle deadline; a stream's Drop guard sends `DEADLINE_EXCEEDED` on timeout or abandonment.
    async fn request(
        &self,
        peer_addr_json: String,
        op: Op,
        op_payload: &[u8],
    ) -> Result<Vec<u8>, JsError> {
        self.ensure_open()?;
        if op_payload.len() > op.frame_cap() - 1 {
            return Err(JsError::new(&format!(
                "request payload exceeds the {}-byte op payload cap",
                op.frame_cap() - 1
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
        if let Err((code, error)) = read_outbound_with_progress(
            io.recv
                .as_mut()
                .expect("outbound receive stream is present"),
            &mut lenb,
            "read reply len",
        )
        .await
        {
            io.cancel(code);
            return Err(JsError::new(&error));
        }
        let len = match check_len_for(op, lenb) {
            Ok(len) => len,
            Err(error) => {
                io.cancel(error.reset_code());
                return Err(JsError::new(&format!("reply frame: {error}")));
            }
        };
        let mut reply = vec![0u8; len];
        if let Err((code, error)) = read_outbound_with_progress(
            io.recv
                .as_mut()
                .expect("outbound receive stream is present"),
            &mut reply,
            "read reply body",
        )
        .await
        {
            io.cancel(code);
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
    crr_sync: CrrSyncHandler,
    grants: InvokerGrants,
    asset_grants: AssetGrants,
    crr_sync_grants: CrrSyncGrants,
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
            let crr_sync = crr_sync.clone();
            let grants = grants.clone();
            let asset_grants = asset_grants.clone();
            let crr_sync_grants = crr_sync_grants.clone();
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
                    let crr_sync = crr_sync.clone();
                    let grants = grants.clone();
                    let asset_grants = asset_grants.clone();
                    let crr_sync_grants = crr_sync_grants.clone();
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
                            let declared = u32::from_le_bytes(lenb) as usize;
                            if declared == 0 {
                                return Err(proto_reset::MALFORMED_PAYLOAD);
                            }
                            let mut opb = [0u8; 1];
                            read_with_progress_deadline(&mut recv, &mut opb).await?;
                            let op = Op::from_byte(opb[0]).map_err(|error| error.reset_code())?;
                            let len = check_len_for(op, lenb).map_err(|error| error.reset_code())?;
                            if len == 0 {
                                return Err(proto_reset::MALFORMED_PAYLOAD);
                            }
                            if op == Op::Echo && len - 1 > BROWSER_MAX_ECHO {
                                return Err(proto_reset::FRAME_TOO_LARGE);
                            }
                            let endpoint_granted = match op {
                                Op::Echo => true,
                                Op::Invoke => grants.borrow().contains_key(&remote_id),
                                Op::AssetGet => asset_grants.borrow().contains_key(&remote_id),
                                Op::CrrSync => crr_sync_grants.borrow().contains(&remote_id),
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
                                Op::CrrSync => {
                                    let crr_sync = crr_sync.clone();
                                    let crr_sync_grants = crr_sync_grants.clone();
                                    let remote_id = remote_id.clone();
                                    let payload = payload;
                                    watch_handler(
                                        async move {
                                            run_crr_sync(
                                                &crr_sync,
                                                &crr_sync_grants,
                                                remote_id,
                                                payload,
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
                        if out.len() > op.frame_cap() {
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
                            Ok(Err(())) => {
                                reject_stream(&mut send, &mut recv, proto_reset::HANDLER_FAILED);
                                return;
                            }
                            Err(_) => {
                                reject_stream(&mut send, &mut recv, proto_reset::DEADLINE_EXCEEDED);
                                return;
                            }
                        }
                        if send.finish().is_err() {
                            reject_stream(&mut send, &mut recv, proto_reset::HANDLER_FAILED);
                            return;
                        }
                        served.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    });
                }
            });
        }
    });
}

/// `Err` is the reset code to refuse the stream with.
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

/// Run one inbound [`Op::CrrSync`] payload against the trusted local glue, re-reading the grant
/// set at the synchronous points around the await (never borrowed across it); request and reply
/// frames pass verbatim.
async fn run_crr_sync(
    handler: &CrrSyncHandler,
    grants: &CrrSyncGrants,
    remote_id: EndpointId,
    payload: Vec<u8>,
    signal: web_sys::AbortSignal,
) -> Result<Vec<u8>, u32> {
    if !grants.borrow().contains(&remote_id) {
        tracing::warn!(remote = %remote_id, "browser crr sync forbidden");
        return Err(proto_reset::FORBIDDEN);
    }
    let Some(handler) = handler.borrow().clone() else {
        tracing::warn!("browser crr sync: no handler registered (call setCrrSyncHandler first)");
        return Err(proto_reset::NO_HANDLER);
    };
    let args = js_sys::Array::new();
    args.push(&js_sys::Uint8Array::from(payload.as_slice()));
    args.push(signal.as_ref());
    let value = handler
        .apply(&JsValue::NULL, &args)
        .map_err(|e| {
            tracing::warn!(error = ?e, "browser crr sync: handler threw");
            proto_reset::HANDLER_FAILED
        })?;
    drop(payload);
    let resolved = JsFuture::from(js_sys::Promise::resolve(&value))
        .await
        .map_err(|e| {
            tracing::warn!(error = ?e, "browser crr sync: handler rejected");
            proto_reset::HANDLER_FAILED
        })?;
    if !grants.borrow().contains(&remote_id) {
        tracing::warn!(remote = %remote_id, "browser crr sync revoked while handler was running");
        return Err(proto_reset::FORBIDDEN);
    }
    if !resolved.is_instance_of::<js_sys::Uint8Array>() {
        tracing::warn!("browser crr sync: handler did not resolve to a Uint8Array");
        return Err(proto_reset::HANDLER_FAILED);
    }
    let out = js_sys::Uint8Array::new(&resolved).to_vec();
    if out.len() > BROWSER_MAX_CRR_FRAME {
        return Err(proto_reset::FRAME_TOO_LARGE);
    }
    Ok(out)
}

/// Domain separator for every peer-mesh signalling signature; MUST stay byte-identical to
/// `MESH_SIG_DOMAIN` in `www/peer-mesh.js`.
const MESH_SIG_DOMAIN: &[u8] = b"hive-browser-webrtc-v1\0";

/// Upper bound on a signable context: only the fixed-shape binding string is signed (an SDP's
/// fingerprint is extracted and bound explicitly), so a whole session description is refused.
const MESH_CONTEXT_MAX: usize = 1024;

fn mesh_signing_message(context: &str) -> Result<Vec<u8>, JsError> {
    if context.is_empty() {
        return Err(JsError::new("mesh envelope context must not be empty"));
    }
    if context.len() > MESH_CONTEXT_MAX {
        return Err(JsError::new(&format!(
            "mesh envelope context exceeds the {MESH_CONTEXT_MAX}-byte bound"
        )));
    }
    let mut message = Vec::with_capacity(MESH_SIG_DOMAIN.len() + context.len());
    message.extend_from_slice(MESH_SIG_DOMAIN);
    message.extend_from_slice(context.as_bytes());
    Ok(message)
}

/// Verify a peer's signalling envelope against the EndpointId the SERVER admitted; a free function
/// needing no local key or endpoint. `false` for a wrong signature, THROWS for malformed input.
#[wasm_bindgen(js_name = verifyMeshEnvelope)]
pub fn verify_mesh_envelope(
    endpoint_id: &str,
    context: &str,
    signature_hex: &str,
) -> Result<bool, JsError> {
    let key = parse_endpoint_id(endpoint_id)?;
    let message = mesh_signing_message(context)?;
    let raw = hex_to_64(signature_hex)
        .ok_or_else(|| JsError::new("mesh signature must be 128 hex chars (64 bytes)"))?;
    let signature = iroh::Signature::from_bytes(&raw);
    Ok(key.verify(&message, &signature).is_ok())
}

/// Parse the endpoint identity from iroh's completed TLS handshake; the typed key in the grants
/// map avoids hex/base32/case aliases that could make revocation miss.
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
    let mut out = [0u8; 32];
    hex_into(s, &mut out)?;
    Some(out)
}

/// Parse 128 hex chars into an ed25519 signature's 64 bytes.
fn hex_to_64(s: &str) -> Option<[u8; 64]> {
    let mut out = [0u8; 64];
    hex_into(s, &mut out)?;
    Some(out)
}

fn hex_into(s: &str, out: &mut [u8]) -> Option<()> {
    let s = s.trim();
    if s.len() != out.len() * 2 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16)?;
        let lo = (chunk[1] as char).to_digit(16)?;
        out[i] = (hi * 16 + lo) as u8;
    }
    Some(())
}
