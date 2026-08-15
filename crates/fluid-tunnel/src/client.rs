//! Gateway-side tunnel: multiplex many concurrent requests over one connection.

use crate::codec::{read_frame, Frame, FrameKind};
use crate::{Health, Metrics, ReqMeta, RespMeta};
use bytes::Bytes;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

struct Pending {
    head: Option<oneshot::Sender<anyhow::Result<RespMeta>>>,
    body: mpsc::UnboundedSender<Bytes>,
}

struct Shared {
    pending: Mutex<HashMap<u64, Pending>>,
    health: Mutex<Health>,
    closed: AtomicBool,
}

/// A live multiplexed tunnel to one instance.
pub struct TunnelClient {
    shared: Arc<Shared>,
    out: mpsc::UnboundedSender<Frame>,
    next_id: AtomicU64,
}

/// A streamed response from the instance.
pub struct TunnelResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub wait_until_ms: u64,
    pub body: mpsc::UnboundedReceiver<Bytes>,
}

impl TunnelClient {
    pub fn new<S>(stream: S) -> TunnelClient
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (mut rd, mut wr) = tokio::io::split(stream);
        let shared = Arc::new(Shared {
            pending: Mutex::new(HashMap::new()),
            health: Mutex::new(Health {
                alive: true,
                ..Default::default()
            }),
            closed: AtomicBool::new(false),
        });
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Frame>();

        // Writer task.
        tokio::spawn(async move {
            while let Some(frame) = out_rx.recv().await {
                if wr.write_all(&frame.encode()).await.is_err() {
                    break;
                }
            }
        });

        // Reader task: demux responses, update health.
        let s = shared.clone();
        tokio::spawn(async move {
            loop {
                let frame = match read_frame(&mut rd).await {
                    Ok(f) => f,
                    Err(_) => break,
                };
                match frame.kind {
                    FrameKind::RespHead => {
                        if let Ok(meta) = serde_json::from_slice::<RespMeta>(&frame.payload) {
                            if let Some(p) = s.pending.lock().get_mut(&frame.stream_id) {
                                if let Some(h) = p.head.take() {
                                    let _ = h.send(Ok(meta));
                                }
                            }
                        }
                    }
                    FrameKind::RespBody => {
                        if let Some(p) = s.pending.lock().get(&frame.stream_id) {
                            let _ = p.body.send(frame.payload);
                        }
                    }
                    FrameKind::RespEnd => {
                        s.pending.lock().remove(&frame.stream_id); // drops body sender
                    }
                    FrameKind::Nack => {
                        if let Some(mut p) = s.pending.lock().remove(&frame.stream_id) {
                            if let Some(h) = p.head.take() {
                                let _ = h.send(Err(anyhow::anyhow!("instance nack (overloaded)")));
                            }
                        }
                    }
                    FrameKind::Metrics => {
                        if let Ok(m) = serde_json::from_slice::<Metrics>(&frame.payload) {
                            let mut h = s.health.lock();
                            h.inflight = m.inflight;
                            h.max_concurrency = m.max_concurrency;
                            h.last_update_ms = now_ms();
                            h.alive = true;
                            // #14: tunnel byte/backpressure metering.
                            h.bytes_in = m.bytes_in;
                            h.bytes_out = m.bytes_out;
                            h.queue_depth = m.queue_depth;
                            h.backpressure_events = m.backpressure_events;
                        }
                    }
                    _ => {}
                }
            }
            // Connection closed: fail everything outstanding.
            s.closed.store(true, Ordering::SeqCst);
            s.health.lock().alive = false;
            let mut pending = s.pending.lock();
            for (_, mut p) in pending.drain() {
                if let Some(h) = p.head.take() {
                    let _ = h.send(Err(anyhow::anyhow!("tunnel closed")));
                }
            }
        });

        TunnelClient {
            shared,
            out: out_tx,
            next_id: AtomicU64::new(1),
        }
    }

    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::SeqCst)
    }

    pub fn health(&self) -> Health {
        *self.shared.health.lock()
    }

    /// Send a request and await its response head; the body streams via the
    /// returned receiver.
    pub async fn request(
        &self,
        method: &str,
        path: &str,
        headers: Vec<(String, String)>,
        body: &[u8],
        head_timeout: std::time::Duration,
    ) -> anyhow::Result<TunnelResponse> {
        anyhow::ensure!(!self.is_closed(), "tunnel closed");
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);

        let meta = ReqMeta {
            method: method.to_string(),
            path: path.to_string(),
            headers,
            body_len: body.len() as u32,
        };
        let meta_json = serde_json::to_vec(&meta)?;
        let mut payload = Vec::with_capacity(4 + meta_json.len() + body.len());
        payload.extend_from_slice(&(meta_json.len() as u32).to_be_bytes());
        payload.extend_from_slice(&meta_json);
        payload.extend_from_slice(body);

        let (head_tx, head_rx) = oneshot::channel();
        let (body_tx, body_rx) = mpsc::unbounded_channel();
        self.shared.pending.lock().insert(
            id,
            Pending {
                head: Some(head_tx),
                body: body_tx,
            },
        );

        self.out
            .send(Frame::new(id, FrameKind::Request, payload))
            .map_err(|_| anyhow::anyhow!("tunnel writer gone"))?;

        // MUST be the caller's own per-invocation budget (the deployment's
        // `max_duration_secs`, default 300s at the fluid-gateway call site),
        // never a shorter hardcoded value: a previous fixed 30s here fired
        // BEFORE the gateway's own `tokio::time::timeout(max_dur, ...)` around
        // this whole call could ever see a real over-budget timeout, so every
        // request past 30s (well within budget for an LLM/webhook call) hit
        // this branch's `Err`, was misread as "the function never answered",
        // and got retried — while the ORIGINAL invocation was still running
        // server-side with no cancellation signal, silently duplicating side
        // effects (this is the "shoomoo incident" `proxy_function` documents:
        // `last_err = "timed out waiting for response head"` was this branch,
        // not a real transport failure).
        let meta = match tokio::time::timeout(head_timeout, head_rx).await {
            Ok(Ok(r)) => r?,
            Ok(Err(_)) => {
                self.shared.pending.lock().remove(&id);
                anyhow::bail!("tunnel closed before response");
            }
            Err(_) => {
                self.shared.pending.lock().remove(&id);
                anyhow::bail!("timed out waiting for response head");
            }
        };
        Ok(TunnelResponse {
            status: meta.status,
            headers: meta.headers,
            wait_until_ms: meta.wait_until_ms,
            body: body_rx,
        })
    }
}
