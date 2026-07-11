//! [`AdminClient`] — the `RpcSource` backend: implements [`AdminSource`] over a
//! socket so the panel can inspect a live instance without touching its storage.
//!
//! Because a streaming subscription interleaves its events with replies to other
//! requests on the same socket, the client runs a background **read task** that
//! demultiplexes incoming lines by request `id`: single-response ops resolve a
//! `oneshot`, and each subscription feeds an `mpsc` stream.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_stream::wrappers::UnboundedReceiverStream;

use super::{
    AclSummary, AdminError, AdminEvent, AdminReply, AdminRequest, AdminResult, AdminSource,
    BlobContent, BlobSummary, CrdtHead, DocEntry, KeyInfo, KvEntry, LatencyStats, LogEntry,
    NodeIdentity, NodeSummary, PeerSummary, RelayStatus, StoreCreateOpts, StoreSummary,
    StoreTickets, ThroughputStats, TopoLink,
};
use async_trait::async_trait;
use futures::stream::{BoxStream, Stream};

/// A registered awaiter for a request id: a single-response op, or a subscription.
enum Pending {
    Once(oneshot::Sender<AdminReply>),
    Stream(mpsc::UnboundedSender<AdminEvent>),
}

type PendingMap = Arc<StdMutex<HashMap<u64, Pending>>>;
type SharedWriter = Arc<Mutex<OwnedWriteHalf>>;

/// A connected admin RPC client. Implements [`AdminSource`], so it is a drop-in
/// replacement for [`super::EmbeddedSource`] in code written against the seam.
pub struct AdminClient {
    writer: SharedWriter,
    next_id: AtomicU64,
    pending: PendingMap,
}

/// A live event subscription. Dropping it deregisters the client-side demux entry
/// and tells the server to stop the subscription (`events.unsubscribe`), so
/// subscribe/drop cycles don't leak entries or server-side stream tasks.
struct Subscription {
    id: u64,
    inner: UnboundedReceiverStream<AdminEvent>,
    pending: PendingMap,
    writer: SharedWriter,
}

impl Stream for Subscription {
    type Item = AdminEvent;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        // Stop routing to a receiver that no longer exists.
        if let Ok(mut map) = self.pending.lock() {
            map.remove(&self.id);
        }
        // Best-effort `events.unsubscribe` (Drop can't await, so fire a task);
        // skip if no runtime is available. The server also cleans up on close.
        let id = self.id;
        let writer = self.writer.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let req = AdminRequest {
                    id,
                    op: "events.unsubscribe".to_string(),
                    args: serde_json::json!({}),
                };
                if let Ok(mut line) = serde_json::to_vec(&req) {
                    line.push(b'\n');
                    let mut w = writer.lock().await;
                    let _ = w.write_all(&line).await;
                    let _ = w.flush().await;
                }
            });
        }
    }
}

impl AdminClient {
    /// Connect to an admin RPC server (e.g. `127.0.0.1:15433`).
    pub async fn connect(addr: &str) -> std::io::Result<Self> {
        let stream = TcpStream::connect(addr).await?;
        let (rd, wr) = stream.into_split();
        let pending: PendingMap = Arc::new(StdMutex::new(HashMap::new()));

        // Background reader: demux every incoming line by id.
        {
            let pending = pending.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(rd).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    match serde_json::from_str::<AdminReply>(&line) {
                        Ok(reply) => route(&pending, reply),
                        // A line we can't parse would otherwise leave a request's
                        // awaiter unresolved forever; log it so it isn't silent.
                        Err(e) => tracing::warn!("admin client: undecodable reply line: {e}"),
                    }
                }
                // Connection closed: drop every awaiter so callers see an error /
                // streams end.
                pending.lock().unwrap().clear();
            });
        }

        Ok(Self {
            writer: Arc::new(Mutex::new(wr)),
            next_id: AtomicU64::new(1),
            pending,
        })
    }

    /// Authenticate this connection with a shared token. Required before any other
    /// op when the server was started with a token.
    pub async fn authenticate(&self, token: &str) -> AdminResult<()> {
        self.request("auth", serde_json::json!({ "token": token }))
            .await?;
        Ok(())
    }

    /// Send one op and await its single reply. Public so callers can invoke ops
    /// not yet wrapped by a typed method.
    pub async fn request(
        &self,
        op: &str,
        args: serde_json::Value,
    ) -> AdminResult<serde_json::Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, Pending::Once(tx));

        self.write(&AdminRequest {
            id,
            op: op.to_string(),
            args,
        })
        .await?;

        match rx.await {
            Ok(AdminReply::Ok { data, .. }) => Ok(data),
            Ok(AdminReply::Err { error, .. }) => Err(error),
            Ok(_) => Err(AdminError::new("protocol", "unexpected streaming reply")),
            Err(_) => Err(AdminError::new(
                "disconnected",
                "server closed the connection",
            )),
        }
    }

    async fn write(&self, req: &AdminRequest) -> AdminResult<()> {
        let mut line =
            serde_json::to_vec(req).map_err(|e| AdminError::new("encode", e.to_string()))?;
        line.push(b'\n');
        let mut w = self.writer.lock().await;
        w.write_all(&line).await.map_err(io_err)?;
        w.flush().await.map_err(io_err)
    }
}

/// Route one demuxed reply to its awaiter.
fn route(pending: &PendingMap, reply: AdminReply) {
    let id = reply.id();
    match reply {
        AdminReply::Ok { .. } | AdminReply::Err { .. } => {
            match pending.lock().unwrap().remove(&id) {
                Some(Pending::Once(tx)) => {
                    let _ = tx.send(reply);
                }
                // A terminal Ok/Err arriving for a subscription id: dropping the
                // stream sender ends the stream. Surface the error rather than
                // silently swallowing it.
                Some(Pending::Stream(_)) => {
                    if let AdminReply::Err { error, .. } = reply {
                        tracing::warn!("admin subscription {id} ended with error: {error}");
                    }
                }
                None => {}
            }
        }
        AdminReply::Event { event, .. } => {
            if let Some(Pending::Stream(tx)) = pending.lock().unwrap().get(&id) {
                let _ = tx.send(event);
            }
        }
        AdminReply::End { .. } => {
            // Dropping the sender ends the stream on the receiver side.
            pending.lock().unwrap().remove(&id);
        }
    }
}

#[async_trait]
impl AdminSource for AdminClient {
    async fn stores_list(&self) -> AdminResult<Vec<StoreSummary>> {
        let data = self.request("stores.list", serde_json::json!({})).await?;
        decode_field(data, "stores")
    }

    async fn node_info(&self) -> AdminResult<NodeSummary> {
        let data = self.request("node.info", serde_json::json!({})).await?;
        serde_json::from_value(data).map_err(|e| AdminError::new("decode", e.to_string()))
    }

    async fn stores_create(
        &self,
        kind: &str,
        name: &str,
        opts: StoreCreateOpts,
    ) -> AdminResult<String> {
        let data = self
            .request(
                "stores.create",
                serde_json::json!({ "kind": kind, "name": name, "opts": opts }),
            )
            .await?;
        data.get("address")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| AdminError::new("decode", "missing address in response"))
    }

    async fn stores_close(&self, name: &str) -> AdminResult<()> {
        self.request("stores.close", serde_json::json!({ "name": name }))
            .await?;
        Ok(())
    }

    async fn stores_drop(&self, name: &str) -> AdminResult<()> {
        self.request("stores.drop", serde_json::json!({ "name": name }))
            .await?;
        Ok(())
    }

    async fn eventlog_append(&self, store: &str, data: &str) -> AdminResult<String> {
        let resp = self
            .request(
                "eventlog.append",
                serde_json::json!({ "store": store, "data": data }),
            )
            .await?;
        resp.get("hash")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| AdminError::new("decode", "missing hash in response"))
    }

    async fn docs_put(&self, store: &str, id: &str, json: &str) -> AdminResult<String> {
        let resp = self
            .request(
                "docs.put",
                serde_json::json!({ "store": store, "id": id, "json": json }),
            )
            .await?;
        resp.get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| AdminError::new("decode", "missing id in response"))
    }

    async fn docs_delete(&self, store: &str, id: &str) -> AdminResult<()> {
        self.request(
            "docs.delete",
            serde_json::json!({ "store": store, "id": id }),
        )
        .await?;
        Ok(())
    }

    async fn node_identity(&self) -> AdminResult<NodeIdentity> {
        let data = self.request("node.identity", serde_json::json!({})).await?;
        serde_json::from_value(data).map_err(|e| AdminError::new("decode", e.to_string()))
    }

    async fn stores_share(&self, name: &str) -> AdminResult<StoreTickets> {
        let data = self
            .request("stores.share", serde_json::json!({ "name": name }))
            .await?;
        serde_json::from_value(data).map_err(|e| AdminError::new("decode", e.to_string()))
    }

    async fn stores_import(
        &self,
        kind: &str,
        name: &str,
        ticket: &str,
        read_only: bool,
    ) -> AdminResult<String> {
        let data = self
            .request(
                "stores.import",
                serde_json::json!({ "kind": kind, "name": name, "ticket": ticket, "read_only": read_only }),
            )
            .await?;
        data.get("address")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| AdminError::new("decode", "missing address in response"))
    }

    async fn kv_entries(&self, store: &str) -> AdminResult<Vec<KvEntry>> {
        let data = self
            .request("kv.entries", serde_json::json!({ "store": store }))
            .await?;
        decode_field(data, "entries")
    }

    async fn eventlog_entries(
        &self,
        store: &str,
        limit: Option<usize>,
        before: Option<&str>,
    ) -> AdminResult<Vec<LogEntry>> {
        let mut args = serde_json::json!({ "store": store });
        if let Some(n) = limit {
            args["limit"] = serde_json::json!(n);
        }
        if let Some(cursor) = before {
            args["before"] = serde_json::json!(cursor);
        }
        let data = self.request("eventlog.entries", args).await?;
        decode_field(data, "entries")
    }

    async fn eventlog_heads(&self, store: &str) -> AdminResult<Vec<CrdtHead>> {
        let data = self
            .request("eventlog.heads", serde_json::json!({ "store": store }))
            .await?;
        decode_field(data, "heads")
    }

    async fn docs_list(&self, store: &str) -> AdminResult<Vec<DocEntry>> {
        let data = self
            .request("docs.list", serde_json::json!({ "store": store }))
            .await?;
        decode_field(data, "docs")
    }

    async fn docs_get(&self, store: &str, id: &str) -> AdminResult<DocEntry> {
        let data = self
            .request("docs.get", serde_json::json!({ "store": store, "id": id }))
            .await?;
        serde_json::from_value(data).map_err(|e| AdminError::new("decode", e.to_string()))
    }

    async fn peers_list(&self) -> AdminResult<Vec<PeerSummary>> {
        let data = self.request("peers.list", serde_json::json!({})).await?;
        decode_field(data, "peers")
    }

    async fn net_topology(&self) -> AdminResult<Vec<TopoLink>> {
        let data = self.request("net.topology", serde_json::json!({})).await?;
        decode_field(data, "links")
    }

    async fn net_relay(&self) -> AdminResult<Vec<RelayStatus>> {
        let data = self.request("net.relay", serde_json::json!({})).await?;
        decode_field(data, "relays")
    }

    async fn node_latency(&self) -> AdminResult<LatencyStats> {
        let data = self.request("node.latency", serde_json::json!({})).await?;
        serde_json::from_value(data).map_err(|e| AdminError::new("decode", e.to_string()))
    }

    async fn node_throughput(&self) -> AdminResult<ThroughputStats> {
        let data = self
            .request("node.throughput", serde_json::json!({}))
            .await?;
        serde_json::from_value(data).map_err(|e| AdminError::new("decode", e.to_string()))
    }

    async fn net_discovered(&self) -> AdminResult<Vec<String>> {
        let data = self
            .request("net.discovered", serde_json::json!({}))
            .await?;
        decode_field(data, "peers")
    }

    async fn blobs_list(&self) -> AdminResult<Vec<BlobSummary>> {
        let data = self.request("blobs.list", serde_json::json!({})).await?;
        decode_field(data, "blobs")
    }

    async fn blob_get(&self, hash: &str) -> AdminResult<BlobContent> {
        let data = self
            .request("blob.get", serde_json::json!({ "hash": hash }))
            .await?;
        serde_json::from_value(data).map_err(|e| AdminError::new("decode", e.to_string()))
    }

    async fn blob_add(&self, path: &str) -> AdminResult<String> {
        let data = self
            .request("blob.add", serde_json::json!({ "path": path }))
            .await?;
        data.get("hash")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| AdminError::new("decode", "missing hash in reply"))
    }

    async fn blob_export(&self, hash: &str, path: &str) -> AdminResult<u64> {
        let data = self
            .request(
                "blob.export",
                serde_json::json!({ "hash": hash, "path": path }),
            )
            .await?;
        Ok(data.get("bytes").and_then(|v| v.as_u64()).unwrap_or(0))
    }

    async fn blob_delete(&self, hash: &str) -> AdminResult<()> {
        self.request("blob.delete", serde_json::json!({ "hash": hash }))
            .await?;
        Ok(())
    }

    async fn events_subscribe(&self) -> AdminResult<BoxStream<'static, AdminEvent>> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::unbounded_channel();
        self.pending.lock().unwrap().insert(id, Pending::Stream(tx));

        self.write(&AdminRequest {
            id,
            op: "events.subscribe".to_string(),
            args: serde_json::json!({}),
        })
        .await?;

        // Wrap in a Subscription so dropping the stream deregisters the demux
        // entry and sends `events.unsubscribe` to the server.
        Ok(Box::pin(Subscription {
            id,
            inner: UnboundedReceiverStream::new(rx),
            pending: self.pending.clone(),
            writer: self.writer.clone(),
        }))
    }

    async fn kv_put(&self, store: &str, key: &str, value: Vec<u8>) -> AdminResult<()> {
        use base64::Engine;
        let value_b64 = base64::engine::general_purpose::STANDARD.encode(&value);
        self.request(
            "kv.put",
            serde_json::json!({ "store": store, "key": key, "value_b64": value_b64 }),
        )
        .await?;
        Ok(())
    }

    async fn kv_delete(&self, store: &str, key: &str) -> AdminResult<()> {
        self.request(
            "kv.delete",
            serde_json::json!({ "store": store, "key": key }),
        )
        .await?;
        Ok(())
    }

    async fn peer_sync(&self, node_id: &str) -> AdminResult<()> {
        self.request(
            "peers.force_sync",
            serde_json::json!({ "node_id": node_id }),
        )
        .await?;
        Ok(())
    }

    async fn keystore_list(&self) -> AdminResult<Vec<String>> {
        let data = self.request("keystore.list", serde_json::json!({})).await?;
        decode_field(data, "keys")
    }

    async fn keystore_detail(&self, key_id: &str) -> AdminResult<KeyInfo> {
        let data = self
            .request("keystore.detail", serde_json::json!({ "key_id": key_id }))
            .await?;
        serde_json::from_value(data).map_err(|e| AdminError::new("decode", e.to_string()))
    }

    async fn keystore_generate(&self, key_id: &str) -> AdminResult<String> {
        let data = self
            .request("keystore.generate", serde_json::json!({ "key_id": key_id }))
            .await?;
        data.get("public_key")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| AdminError::new("decode", "missing public_key in reply"))
    }

    async fn acl_list(&self) -> AdminResult<Vec<AclSummary>> {
        let data = self.request("acl.list", serde_json::json!({})).await?;
        decode_field(data, "controllers")
    }

    async fn acl_grant(&self, store: &str, role: &str, key_id: &str) -> AdminResult<()> {
        self.request(
            "acl.grant",
            serde_json::json!({ "store": store, "role": role, "key_id": key_id }),
        )
        .await?;
        Ok(())
    }

    async fn acl_revoke(&self, store: &str, role: &str, key_id: &str) -> AdminResult<()> {
        self.request(
            "acl.revoke",
            serde_json::json!({ "store": store, "role": role, "key_id": key_id }),
        )
        .await?;
        Ok(())
    }

    async fn acl_create(
        &self,
        controller_type: &str,
        name: &str,
        admin_keys: Vec<String>,
        write_keys: Vec<String>,
    ) -> AdminResult<String> {
        let data = self
            .request(
                "acl.create",
                serde_json::json!({
                    "controller_type": controller_type,
                    "name": name,
                    "admin_keys": admin_keys,
                    "write_keys": write_keys,
                }),
            )
            .await?;
        data.get("manifest")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| AdminError::new("decode", "missing manifest in reply"))
    }
}

/// Pull a named array field out of a reply payload and deserialize it.
fn decode_field<T: serde::de::DeserializeOwned>(
    data: serde_json::Value,
    field: &str,
) -> AdminResult<Vec<T>> {
    let value = data.get(field).cloned().unwrap_or(serde_json::Value::Null);
    serde_json::from_value(value).map_err(|e| AdminError::new("decode", e.to_string()))
}

fn io_err(e: std::io::Error) -> AdminError {
    AdminError::new("io", e.to_string())
}
