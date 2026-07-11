//! The admin RPC server: accept loop + per-connection reader/writer + dispatch.
//!
//! Mirrors `pgwire::serve`/`serve_on`: bind, accept, spawn a task per connection.
//! Ordinary request/response ops are handled inline (one line in, one line out).
//! A streaming op (`events.subscribe`) runs as a background task that pushes many
//! lines over time, so the write half is shared behind a `Mutex` and concurrent
//! writers (the read loop's replies and each stream's events) serialize on it.

use std::collections::HashMap;
use std::sync::Arc;

use futures::StreamExt;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use super::{AdminError, AdminReply, AdminRequest, AdminSource};

/// Per-connection registry of active subscriptions (subscribe-request id → its
/// cancellation token), used to stop a stream on `events.unsubscribe` / close.
type Subscriptions = HashMap<u64, CancellationToken>;

/// Write half shared by the read loop and any active stream tasks.
type SharedWriter = Arc<Mutex<OwnedWriteHalf>>;

/// Serve the admin RPC on `addr` over `source` until cancelled. If `token` is
/// `Some`, a connection must authenticate (op `auth`) before any other op.
pub async fn serve(
    addr: &str,
    source: Arc<dyn AdminSource>,
    token: Option<String>,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    serve_on(listener, source, token).await
}

/// Serve on an already-bound listener (lets the caller learn the chosen port,
/// e.g. when binding `127.0.0.1:0` in tests).
pub async fn serve_on(
    listener: TcpListener,
    source: Arc<dyn AdminSource>,
    token: Option<String>,
) -> std::io::Result<()> {
    let token: Option<Arc<str>> = token.map(Arc::from);
    loop {
        // A transient accept error (e.g. EMFILE under FD pressure) must not tear
        // down the whole server; log and keep serving.
        let (socket, _peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!("admin accept error: {e}");
                continue;
            }
        };
        let source = source.clone();
        let token = token.clone();
        tokio::spawn(async move {
            let (rd, wr) = socket.into_split();
            let writer: SharedWriter = Arc::new(Mutex::new(wr));
            let mut lines = BufReader::new(rd).lines();
            // Without a configured token, connections start authenticated.
            let mut authenticated = token.is_none();
            let mut subs: Subscriptions = HashMap::new();

            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        if let Err(e) = handle_line(
                            &line,
                            &source,
                            &writer,
                            &token,
                            &mut authenticated,
                            &mut subs,
                        )
                        .await
                        {
                            tracing::warn!("admin conn write error: {e}");
                            break;
                        }
                    }
                    Ok(None) => break, // client closed
                    Err(e) => {
                        tracing::warn!("admin conn read error: {e}");
                        break;
                    }
                }
            }

            // Connection closed: stop every still-active subscription stream.
            for (_, cancel) in subs.drain() {
                cancel.cancel();
            }
        });
    }
}

/// Parse and act on one request line. Handles the `auth` handshake and gates all
/// other ops behind it; streaming ops spawn a background task; everything else
/// dispatches inline and writes a single reply.
async fn handle_line(
    line: &str,
    source: &Arc<dyn AdminSource>,
    writer: &SharedWriter,
    token: &Option<Arc<str>>,
    authenticated: &mut bool,
    subs: &mut Subscriptions,
) -> std::io::Result<()> {
    let req = match serde_json::from_str::<AdminRequest>(line) {
        Ok(req) => req,
        Err(e) => {
            return write_reply(
                writer,
                AdminReply::err(0, AdminError::new("bad_request", e.to_string())),
            )
            .await;
        }
    };

    // Auth handshake: validate the presented token against the configured one.
    if req.op == "auth" {
        let given = req.args.get("token").and_then(|v| v.as_str());
        let ok = match token.as_deref() {
            None => true, // no token required
            Some(expected) => given == Some(expected),
        };
        let reply = if ok {
            *authenticated = true;
            AdminReply::ok(req.id, serde_json::json!({ "authenticated": true }))
        } else {
            AdminReply::err(req.id, AdminError::new("unauthorized", "invalid token"))
        };
        return write_reply(writer, reply).await;
    }

    if !*authenticated {
        return write_reply(
            writer,
            AdminReply::err(
                req.id,
                AdminError::new("unauthorized", "authenticate first (op \"auth\")"),
            ),
        )
        .await;
    }

    match req.op.as_str() {
        "events.subscribe" => {
            // Re-subscribing on the same id: cancel the previous one first.
            if let Some(old) = subs.insert(req.id, CancellationToken::new()) {
                old.cancel();
            }
            let cancel = subs.get(&req.id).cloned().unwrap_or_default();
            let source = source.clone();
            let writer = writer.clone();
            tokio::spawn(stream_events(source, req.id, writer, cancel));
            Ok(())
        }
        // Fire-and-forget: stop the subscription with the given id, no reply.
        "events.unsubscribe" => {
            if let Some(cancel) = subs.remove(&req.id) {
                cancel.cancel();
            }
            Ok(())
        }
        _ => {
            let reply = dispatch(req, source.as_ref()).await;
            write_reply(writer, reply).await
        }
    }
}

/// Drive a subscription: forward each event as an `Event` reply until the stream
/// ends or the token is cancelled (unsubscribe / connection close), then `End`.
async fn stream_events(
    source: Arc<dyn AdminSource>,
    id: u64,
    writer: SharedWriter,
    cancel: CancellationToken,
) {
    match source.events_subscribe().await {
        Ok(mut stream) => {
            loop {
                tokio::select! {
                    // Cancellation only fires here (between writes), never mid-line.
                    _ = cancel.cancelled() => return,
                    next = stream.next() => match next {
                        Some(event) => {
                            if write_reply(&writer, AdminReply::event(id, event))
                                .await
                                .is_err()
                            {
                                return; // client gone
                            }
                        }
                        None => break, // source stream ended
                    },
                }
            }
            let _ = write_reply(&writer, AdminReply::end(id)).await;
        }
        Err(e) => {
            let _ = write_reply(&writer, AdminReply::err(id, e)).await;
        }
    }
}

/// Serialize one reply and write it as a single `\n`-terminated line.
async fn write_reply(writer: &SharedWriter, reply: AdminReply) -> std::io::Result<()> {
    let mut buf = serde_json::to_vec(&reply).unwrap_or_else(|e| {
        // Serialize a fresh error reply via serde (which escapes the message
        // properly) rather than hand-building a JSON string. The static fallback
        // is already-valid JSON with no interpolation.
        serde_json::to_vec(&AdminReply::err(
            0,
            AdminError::new("internal", e.to_string()),
        ))
        .unwrap_or_else(|_| {
            br#"{"id":0,"ok":false,"error":{"code":"internal","message":"serialize failed"}}"#
                .to_vec()
        })
    });
    buf.push(b'\n');
    let mut w = writer.lock().await;
    w.write_all(&buf).await?;
    w.flush().await
}

/// Route one request to the [`AdminSource`] and wrap the outcome in a reply.
async fn dispatch(req: AdminRequest, source: &dyn AdminSource) -> AdminReply {
    let id = req.id;
    let result = match req.op.as_str() {
        "stores.list" => source
            .stores_list()
            .await
            .map(|stores| serde_json::json!({ "stores": stores })),
        "stores.create" => match (str_arg(&req, "kind"), str_arg(&req, "name")) {
            (Ok(kind), Ok(name)) => {
                let opts = req
                    .args
                    .get("opts")
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
                    .unwrap_or_default();
                source
                    .stores_create(kind, name, opts)
                    .await
                    .map(|address| serde_json::json!({ "address": address }))
            }
            (Err(e), _) | (_, Err(e)) => Err(e),
        },
        "stores.close" => match str_arg(&req, "name") {
            Ok(name) => source.stores_close(name).await.map(|()| empty_ok()),
            Err(e) => Err(e),
        },
        "stores.drop" => match str_arg(&req, "name") {
            Ok(name) => source.stores_drop(name).await.map(|()| empty_ok()),
            Err(e) => Err(e),
        },
        "eventlog.append" => match (str_arg(&req, "store"), str_arg(&req, "data")) {
            (Ok(store), Ok(data)) => source
                .eventlog_append(store, data)
                .await
                .map(|hash| serde_json::json!({ "hash": hash })),
            (Err(e), _) | (_, Err(e)) => Err(e),
        },
        "docs.put" => match (str_arg(&req, "store"), str_arg(&req, "id")) {
            (Ok(store), Ok(id)) => match str_arg(&req, "json") {
                Ok(json) => source
                    .docs_put(store, id, json)
                    .await
                    .map(|id| serde_json::json!({ "id": id })),
                Err(e) => Err(e),
            },
            (Err(e), _) | (_, Err(e)) => Err(e),
        },
        "docs.delete" => match (str_arg(&req, "store"), str_arg(&req, "id")) {
            (Ok(store), Ok(id)) => source.docs_delete(store, id).await.map(|()| empty_ok()),
            (Err(e), _) | (_, Err(e)) => Err(e),
        },
        "node.identity" => source
            .node_identity()
            .await
            .and_then(|id| serde_json::to_value(id).map_err(encode_err)),
        "stores.share" => match str_arg(&req, "name") {
            Ok(name) => source
                .stores_share(name)
                .await
                .and_then(|t| serde_json::to_value(t).map_err(encode_err)),
            Err(e) => Err(e),
        },
        "stores.import" => match (str_arg(&req, "kind"), str_arg(&req, "name")) {
            (Ok(kind), Ok(name)) => match str_arg(&req, "ticket") {
                Ok(ticket) => {
                    let read_only = req
                        .args
                        .get("read_only")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    source
                        .stores_import(kind, name, ticket, read_only)
                        .await
                        .map(|address| serde_json::json!({ "address": address }))
                }
                Err(e) => Err(e),
            },
            (Err(e), _) | (_, Err(e)) => Err(e),
        },
        "node.info" => source
            .node_info()
            .await
            .and_then(|n| serde_json::to_value(n).map_err(encode_err)),
        "kv.entries" => match str_arg(&req, "store") {
            Ok(store) => source
                .kv_entries(store)
                .await
                .map(|entries| serde_json::json!({ "entries": entries })),
            Err(e) => Err(e),
        },
        "eventlog.entries" => match str_arg(&req, "store") {
            Ok(store) => {
                let limit = req
                    .args
                    .get("limit")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as usize);
                let before = req.args.get("before").and_then(|v| v.as_str());
                source
                    .eventlog_entries(store, limit, before)
                    .await
                    .map(|entries| serde_json::json!({ "entries": entries }))
            }
            Err(e) => Err(e),
        },
        "eventlog.heads" => match str_arg(&req, "store") {
            Ok(store) => source
                .eventlog_heads(store)
                .await
                .map(|heads| serde_json::json!({ "heads": heads })),
            Err(e) => Err(e),
        },
        "docs.list" => match str_arg(&req, "store") {
            Ok(store) => source
                .docs_list(store)
                .await
                .map(|docs| serde_json::json!({ "docs": docs })),
            Err(e) => Err(e),
        },
        "docs.get" => match (str_arg(&req, "store"), str_arg(&req, "id")) {
            (Ok(store), Ok(id)) => source
                .docs_get(store, id)
                .await
                .and_then(|d| serde_json::to_value(d).map_err(encode_err)),
            (Err(e), _) | (_, Err(e)) => Err(e),
        },
        "peers.list" => source
            .peers_list()
            .await
            .map(|peers| serde_json::json!({ "peers": peers })),
        "net.topology" => source
            .net_topology()
            .await
            .map(|links| serde_json::json!({ "links": links })),
        "net.relay" => source
            .net_relay()
            .await
            .map(|relays| serde_json::json!({ "relays": relays })),
        "node.latency" => source
            .node_latency()
            .await
            .and_then(|s| serde_json::to_value(s).map_err(encode_err)),
        "node.throughput" => source
            .node_throughput()
            .await
            .and_then(|s| serde_json::to_value(s).map_err(encode_err)),
        "net.discovered" => source
            .net_discovered()
            .await
            .map(|peers| serde_json::json!({ "peers": peers })),
        "blobs.list" => source
            .blobs_list()
            .await
            .map(|blobs| serde_json::json!({ "blobs": blobs })),
        "blob.get" => match str_arg(&req, "hash") {
            Ok(hash) => source
                .blob_get(hash)
                .await
                .and_then(|c| serde_json::to_value(c).map_err(encode_err)),
            Err(e) => Err(e),
        },
        "blob.add" => match str_arg(&req, "path") {
            Ok(path) => source
                .blob_add(path)
                .await
                .map(|hash| serde_json::json!({ "hash": hash })),
            Err(e) => Err(e),
        },
        "blob.export" => match (str_arg(&req, "hash"), str_arg(&req, "path")) {
            (Ok(hash), Ok(path)) => source
                .blob_export(hash, path)
                .await
                .map(|n| serde_json::json!({ "bytes": n })),
            (Err(e), _) | (_, Err(e)) => Err(e),
        },
        "blob.delete" => match str_arg(&req, "hash") {
            Ok(hash) => source.blob_delete(hash).await.map(|_| empty_ok()),
            Err(e) => Err(e),
        },
        "kv.put" => match (str_arg(&req, "store"), str_arg(&req, "key")) {
            (Ok(store), Ok(key)) => match decode_value(&req) {
                Ok(value) => source.kv_put(store, key, value).await.map(|_| empty_ok()),
                Err(e) => Err(e),
            },
            (Err(e), _) | (_, Err(e)) => Err(e),
        },
        "kv.delete" => match (str_arg(&req, "store"), str_arg(&req, "key")) {
            (Ok(store), Ok(key)) => source.kv_delete(store, key).await.map(|_| empty_ok()),
            (Err(e), _) | (_, Err(e)) => Err(e),
        },
        "peers.force_sync" => match str_arg(&req, "node_id") {
            Ok(node_id) => source.peer_sync(node_id).await.map(|_| empty_ok()),
            Err(e) => Err(e),
        },
        "keystore.list" => source
            .keystore_list()
            .await
            .map(|keys| serde_json::json!({ "keys": keys })),
        "keystore.detail" => match str_arg(&req, "key_id") {
            Ok(key_id) => source
                .keystore_detail(key_id)
                .await
                .and_then(|k| serde_json::to_value(k).map_err(encode_err)),
            Err(e) => Err(e),
        },
        "keystore.generate" => match str_arg(&req, "key_id") {
            Ok(key_id) => source
                .keystore_generate(key_id)
                .await
                .map(|public| serde_json::json!({ "public_key": public })),
            Err(e) => Err(e),
        },
        "acl.list" => source
            .acl_list()
            .await
            .map(|controllers| serde_json::json!({ "controllers": controllers })),
        "acl.grant" => {
            match (
                str_arg(&req, "store"),
                str_arg(&req, "role"),
                str_arg(&req, "key_id"),
            ) {
                (Ok(store), Ok(role), Ok(key_id)) => source
                    .acl_grant(store, role, key_id)
                    .await
                    .map(|_| empty_ok()),
                (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => Err(e),
            }
        }
        "acl.revoke" => {
            match (
                str_arg(&req, "store"),
                str_arg(&req, "role"),
                str_arg(&req, "key_id"),
            ) {
                (Ok(store), Ok(role), Ok(key_id)) => source
                    .acl_revoke(store, role, key_id)
                    .await
                    .map(|_| empty_ok()),
                (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => Err(e),
            }
        }
        "acl.create" => {
            let controller_type = req
                .args
                .get("controller_type")
                .and_then(|v| v.as_str())
                .unwrap_or("simple");
            let name = req.args.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let str_array = |field: &str| -> Vec<String> {
                req.args
                    .get(field)
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            source
                .acl_create(
                    controller_type,
                    name,
                    str_array("admin_keys"),
                    str_array("write_keys"),
                )
                .await
                .map(|hash| serde_json::json!({ "manifest": hash }))
        }
        other => Err(AdminError::new(
            "unknown_op",
            format!("unknown op: {other}"),
        )),
    };

    match result {
        Ok(data) => AdminReply::ok(id, data),
        Err(error) => AdminReply::err(id, error),
    }
}

fn encode_err(e: serde_json::Error) -> AdminError {
    AdminError::new("encode", e.to_string())
}

/// Empty success payload for action ops.
fn empty_ok() -> serde_json::Value {
    serde_json::json!({ "ok": true })
}

/// Decode a `kv.put` value: base64 in `value_b64`, else raw UTF-8 in `value`.
fn decode_value(req: &AdminRequest) -> Result<Vec<u8>, AdminError> {
    use base64::Engine;
    if let Some(b64) = req.args.get("value_b64").and_then(|v| v.as_str()) {
        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| AdminError::new("bad_args", format!("value_b64: {e}")))
    } else if let Some(s) = req.args.get("value").and_then(|v| v.as_str()) {
        Ok(s.as_bytes().to_vec())
    } else {
        Err(AdminError::new(
            "bad_args",
            "kv.put requires args.value_b64 or args.value",
        ))
    }
}

/// Extract a required string argument, or a `bad_args` error naming it.
fn str_arg<'a>(req: &'a AdminRequest, name: &str) -> Result<&'a str, AdminError> {
    req.args
        .get(name)
        .and_then(|v| v.as_str())
        .ok_or_else(|| AdminError::new("bad_args", format!("{} requires args.{name}", req.op)))
}
