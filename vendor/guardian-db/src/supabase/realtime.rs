//! Supabase Realtime-compatible WebSocket service.
//!
//! Speaks the Phoenix-channel protocol at `/realtime/v1/websocket` in both
//! encodings clients use: the JSON **object** form realtime-js v2 sends
//! (`{"topic","event","payload","ref"}`) and the Phoenix V2 **array** form
//! (`[join_ref, ref, topic, event, payload]`). Replies use the same form the
//! client's message used.
//!
//! Supported events: `phx_join` (with `config.postgres_changes` bindings and
//! `config.broadcast`), `phx_leave`, `heartbeat` (topic `phoenix`),
//! `broadcast` passthrough between subscribers of a topic, and `access_token`
//! rotation. Presence and binary frames get **typed error replies**
//! (`SUPA_COMPAT_REALTIME_*`), never silence.
//!
//! ## Change source
//!
//! Postgres-changes events come from the engine's local commit hook
//! ([`Database::subscribe_changes`]): every websocket connection registers a
//! listener and filters the stream against its bindings
//! (schema / table / event / `col=eq.value` filter).
//!
//! ## Authorization — no unauthorized delivery
//!
//! The connection authenticates with the project `apikey` query parameter
//! (optionally upgraded by `access_token` messages / join payloads). Each
//! candidate event is authorized against the subscriber's **own role** before
//! delivery:
//!
//! * RLS-bypass roles receive everything (`service_role` always; the engine
//!   owners unless the table declares `FORCE ROW LEVEL SECURITY`);
//! * tables without RLS enabled are visible to every role (engine semantics);
//! * for `INSERT`/`UPDATE` on RLS-enabled tables the row's primary key is
//!   re-selected through a session bound to the subscriber's role and claims —
//!   the event is delivered only if the row is visible under the caller's
//!   policies;
//! * `DELETE` events on RLS-enabled tables, and rows in tables **without a
//!   primary key**, cannot be re-checked, so they are **not delivered** to
//!   non-bypass roles (documented constraint: when in doubt, don't deliver).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{RawQuery, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use chrono::Utc;
use serde_json::{Map, Value as Json, json};

use crate::relational::Catalog;
use crate::relational::catalog::Table;
use crate::sql::engine::{ChangeEvent, ChangeOp};
use crate::sql::role_bypasses_rls_on;
use crate::sql::{RelationalStorage, SqlValue};
use crate::supabase::error::SupaError;
use crate::supabase::gateway::{AppState, AuthContext, load_catalog, run_sql_as};
use crate::supabase::jwt::Claims;
use crate::supabase::rest::{parse_query_pairs, value_to_json};

/// Shared realtime state: the broadcast bus between websocket connections and
/// the id generator for connections / subscription bindings.
pub struct RealtimeShared {
    bus: tokio::sync::broadcast::Sender<BroadcastMsg>,
    next_id: AtomicU64,
}

impl RealtimeShared {
    pub fn new() -> Self {
        let (bus, _) = tokio::sync::broadcast::channel(1024);
        Self {
            bus,
            next_id: AtomicU64::new(1),
        }
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }
}

impl Default for RealtimeShared {
    fn default() -> Self {
        Self::new()
    }
}

/// A broadcast message relayed between subscribers of a topic.
#[derive(Clone, Debug)]
struct BroadcastMsg {
    topic: String,
    sender_conn: u64,
    payload: Json,
}

/// The realtime subrouter, mounted at `/realtime/v1`. Sits **outside** the
/// apikey header middleware: browsers cannot set headers on websocket
/// connects, so the key arrives as the `apikey` (or `token`) query parameter
/// and is verified before the upgrade.
pub fn router<S: RelationalStorage + 'static>() -> Router<AppState<S>> {
    Router::new()
        .route("/websocket", get(ws_connect::<S>))
        .fallback(unsupported_route)
}

/// Typed catch-all for `/realtime/v1` paths this slice does not implement.
async fn unsupported_route() -> Response {
    let body = json!({
        "code": "SUPA_COMPAT_REALTIME_UNSUPPORTED_ROUTE",
        "message": "this realtime route is not implemented in the GuardianDB compatibility \
                    slice; connect a websocket to /realtime/v1/websocket",
    });
    (axum::http::StatusCode::NOT_FOUND, axum::Json(body)).into_response()
}

async fn ws_connect<S: RelationalStorage + 'static>(
    State(state): State<AppState<S>>,
    RawQuery(query): RawQuery,
    ws: WebSocketUpgrade,
) -> Response {
    // Verify the apikey before upgrading — a bad key is a typed 401, not a
    // silently dropped socket.
    let params = parse_query_pairs(query.as_deref().unwrap_or(""));
    let get = |k: &str| {
        params
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.clone())
    };
    let Some(apikey) = get("apikey")
        .or_else(|| get("token"))
        .filter(|s| !s.is_empty())
    else {
        return SupaError::MissingApiKey.into_response();
    };
    let now = Utc::now().timestamp();
    let api_claims = match state.project.keys.verify_api_key(&apikey, now) {
        Ok(c) => c,
        Err(_) => return SupaError::InvalidApiKey.into_response(),
    };

    ws.on_upgrade(move |socket| conn_task(state, socket, api_claims))
}

// ---------------------------------------------------------------------------
// Frames (Phoenix protocol, object and array forms)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum FrameStyle {
    /// `{"topic","event","payload","ref","join_ref"}` (realtime-js v2).
    Object,
    /// `[join_ref, ref, topic, event, payload]` (Phoenix V2 serializer).
    Array,
}

struct Frame {
    join_ref: Option<String>,
    reference: Option<String>,
    topic: String,
    event: String,
    payload: Json,
}

fn json_ref(v: Option<&Json>) -> Option<String> {
    match v? {
        Json::String(s) => Some(s.clone()),
        Json::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn parse_frame(text: &str) -> Option<(FrameStyle, Frame)> {
    let v: Json = serde_json::from_str(text).ok()?;
    match v {
        Json::Array(items) if items.len() == 5 => {
            let mut it = items.into_iter();
            let join_ref = it.next();
            let reference = it.next();
            let topic = it.next()?;
            let event = it.next()?;
            let payload = it.next().unwrap_or(Json::Null);
            Some((
                FrameStyle::Array,
                Frame {
                    join_ref: json_ref(join_ref.as_ref()),
                    reference: json_ref(reference.as_ref()),
                    topic: topic.as_str()?.to_string(),
                    event: event.as_str()?.to_string(),
                    payload,
                },
            ))
        }
        Json::Object(obj) => Some((
            FrameStyle::Object,
            Frame {
                join_ref: json_ref(obj.get("join_ref")),
                reference: json_ref(obj.get("ref")),
                topic: obj.get("topic")?.as_str()?.to_string(),
                event: obj.get("event")?.as_str()?.to_string(),
                payload: obj.get("payload").cloned().unwrap_or(Json::Null),
            },
        )),
        _ => None,
    }
}

fn encode_frame(
    style: FrameStyle,
    join_ref: Option<&str>,
    reference: Option<&str>,
    topic: &str,
    event: &str,
    payload: Json,
) -> String {
    match style {
        FrameStyle::Array => json!([join_ref, reference, topic, event, payload]).to_string(),
        FrameStyle::Object => {
            let mut obj = Map::new();
            obj.insert("topic".into(), json!(topic));
            obj.insert("event".into(), json!(event));
            obj.insert("payload".into(), payload);
            obj.insert("ref".into(), json!(reference));
            if let Some(jr) = join_ref {
                obj.insert("join_ref".into(), json!(jr));
            }
            Json::Object(obj).to_string()
        }
    }
}

fn phx_reply(status: &str, response: Json) -> Json {
    json!({ "status": status, "response": response })
}

fn error_reason(code: &str, message: &str) -> Json {
    json!({ "reason": message, "code": code })
}

// ---------------------------------------------------------------------------
// Subscriptions
// ---------------------------------------------------------------------------

/// One `postgres_changes` binding of a joined topic.
struct PgBinding {
    id: u64,
    event: String,
    schema: String,
    table: Option<String>,
    /// Parsed `col=eq.value` filter (the only operator this slice supports).
    filter: Option<(String, String)>,
}

impl PgBinding {
    /// The server-side descriptor echoed in the join reply (realtime-js
    /// matches its client bindings against these by event/schema/table/filter).
    fn descriptor(&self, original_filter: Option<&str>) -> Json {
        let mut obj = Map::new();
        obj.insert("id".into(), json!(self.id));
        obj.insert("event".into(), json!(self.event));
        obj.insert("schema".into(), json!(self.schema));
        if let Some(t) = &self.table {
            obj.insert("table".into(), json!(t));
        }
        if let Some(f) = original_filter {
            obj.insert("filter".into(), json!(f));
        }
        Json::Object(obj)
    }
}

struct TopicSub {
    join_ref: Option<String>,
    broadcast_self: bool,
    bindings: Vec<(PgBinding, Option<String>)>,
}

/// Per-connection authentication state.
struct ConnAuth {
    api_key_role: String,
    claims: Option<Claims>,
}

impl ConnAuth {
    fn role(&self) -> String {
        self.claims
            .as_ref()
            .map(|c| c.pg_role().to_string())
            .unwrap_or_else(|| self.api_key_role.clone())
    }

    /// An [`AuthContext`] equivalent for role-bound visibility queries.
    fn context(&self, request_id: &str) -> AuthContext {
        AuthContext {
            role: self.role(),
            api_key_role: self.api_key_role.clone(),
            claims: self.claims.clone(),
            request_id: request_id.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Connection task
// ---------------------------------------------------------------------------

async fn conn_task<S: RelationalStorage + 'static>(
    state: AppState<S>,
    mut socket: WebSocket,
    api_claims: Claims,
) {
    let conn_id = state.realtime.next_id();
    let request_id = format!("realtime-{conn_id}");
    let mut auth = ConnAuth {
        api_key_role: api_claims.pg_role().to_string(),
        // A user access token passed as the connect param acts as claims too.
        claims: if api_claims.is_user() {
            Some(api_claims)
        } else {
            None
        },
    };
    let mut style = FrameStyle::Object;
    let mut topics: HashMap<String, TopicSub> = HashMap::new();
    let mut bus_rx = state.realtime.bus.subscribe();
    let mut changes_rx = state.db.subscribe_changes();

    loop {
        tokio::select! {
            incoming = socket.recv() => {
                let Some(Ok(msg)) = incoming else { break };
                match msg {
                    Message::Text(text) => {
                        let Some((s, frame)) = parse_frame(text.as_str()) else {
                            let err = encode_frame(
                                style, None, None, "phoenix", "phx_error",
                                error_reason(
                                    "SUPA_COMPAT_REALTIME_MALFORMED_FRAME",
                                    "frames must be Phoenix messages: \
                                     {topic,event,payload,ref} or [join_ref,ref,topic,event,payload]",
                                ),
                            );
                            if socket.send(Message::Text(err.into())).await.is_err() { break; }
                            continue;
                        };
                        style = s;
                        if handle_frame(&state, &mut socket, style, frame, &mut auth, &mut topics, conn_id)
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Message::Binary(_) => {
                        let err = encode_frame(
                            style, None, None, "phoenix", "phx_error",
                            error_reason(
                                "SUPA_COMPAT_REALTIME_BINARY_UNSUPPORTED",
                                "binary Phoenix frames are not supported; send JSON text frames",
                            ),
                        );
                        if socket.send(Message::Text(err.into())).await.is_err() { break; }
                    }
                    Message::Close(_) => break,
                    // Ping/Pong are handled by the transport.
                    _ => {}
                }
            }
            broadcast = bus_rx.recv() => {
                match broadcast {
                    Ok(msg) => {
                        let deliver = topics.get(&msg.topic).map(|sub| {
                            msg.sender_conn != conn_id || sub.broadcast_self
                        }).unwrap_or(false);
                        if deliver {
                            let join_ref = topics.get(&msg.topic).and_then(|s| s.join_ref.clone());
                            let frame = encode_frame(
                                style, join_ref.as_deref(), None, &msg.topic, "broadcast", msg.payload,
                            );
                            if socket.send(Message::Text(frame.into())).await.is_err() { break; }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            change = changes_rx.recv() => {
                let Some(event) = change else { break };
                if deliver_change(&state, &mut socket, style, &auth, &topics, &request_id, &event)
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    }
}

/// Handle one client frame. `Err(())` means the socket is gone.
async fn handle_frame<S: RelationalStorage + 'static>(
    state: &AppState<S>,
    socket: &mut WebSocket,
    style: FrameStyle,
    frame: Frame,
    auth: &mut ConnAuth,
    topics: &mut HashMap<String, TopicSub>,
    conn_id: u64,
) -> Result<(), ()> {
    let send = |payload: Json, event: &'static str| {
        encode_frame(
            style,
            frame.join_ref.as_deref(),
            frame.reference.as_deref(),
            &frame.topic,
            event,
            payload,
        )
    };
    let reply = |payload: Json| send(payload, "phx_reply");

    match frame.event.as_str() {
        "heartbeat" => {
            let msg = reply(phx_reply("ok", json!({})));
            socket.send(Message::Text(msg.into())).await.map_err(|_| ())
        }
        "phx_join" => {
            // An access_token in the join payload upgrades the connection auth.
            if let Some(tok) = frame.payload.get("access_token").and_then(Json::as_str)
                && let Err(e) = update_access_token(state, auth, tok)
            {
                let msg = reply(phx_reply(
                    "error",
                    error_reason(
                        "SUPA_COMPAT_INVALID_JWT",
                        &format!("invalid access_token: {e}"),
                    ),
                ));
                return socket.send(Message::Text(msg.into())).await.map_err(|_| ());
            }
            let config = frame.payload.get("config").cloned().unwrap_or(json!({}));
            let broadcast_self = config
                .get("broadcast")
                .and_then(|b| b.get("self"))
                .and_then(Json::as_bool)
                .unwrap_or(false);
            let specs = config
                .get("postgres_changes")
                .and_then(Json::as_array)
                .cloned()
                .unwrap_or_default();
            let mut bindings = Vec::new();
            let mut descriptors = Vec::new();
            for spec in &specs {
                match parse_binding(state, spec) {
                    Ok(binding) => {
                        let original_filter = spec
                            .get("filter")
                            .and_then(Json::as_str)
                            .map(str::to_string);
                        descriptors.push(binding.descriptor(original_filter.as_deref()));
                        bindings.push((binding, original_filter));
                    }
                    Err((code, message)) => {
                        let msg = reply(phx_reply("error", error_reason(code, &message)));
                        return socket.send(Message::Text(msg.into())).await.map_err(|_| ());
                    }
                }
            }
            topics.insert(
                frame.topic.clone(),
                TopicSub {
                    join_ref: frame.join_ref.clone().or(frame.reference.clone()),
                    broadcast_self,
                    bindings,
                },
            );
            let msg = reply(phx_reply("ok", json!({ "postgres_changes": descriptors })));
            socket.send(Message::Text(msg.into())).await.map_err(|_| ())
        }
        "phx_leave" => {
            topics.remove(&frame.topic);
            let msg = reply(phx_reply("ok", json!({})));
            socket.send(Message::Text(msg.into())).await.map_err(|_| ())
        }
        "access_token" => {
            let token = frame
                .payload
                .get("access_token")
                .and_then(Json::as_str)
                .unwrap_or("");
            match update_access_token(state, auth, token) {
                Ok(()) => {
                    if frame.reference.is_some() {
                        let msg = reply(phx_reply("ok", json!({})));
                        socket.send(Message::Text(msg.into())).await.map_err(|_| ())
                    } else {
                        Ok(())
                    }
                }
                Err(e) => {
                    let msg = reply(phx_reply(
                        "error",
                        error_reason(
                            "SUPA_COMPAT_INVALID_JWT",
                            &format!("invalid access_token: {e}"),
                        ),
                    ));
                    socket.send(Message::Text(msg.into())).await.map_err(|_| ())
                }
            }
        }
        "broadcast" => {
            if !topics.contains_key(&frame.topic) {
                let msg = reply(phx_reply(
                    "error",
                    error_reason(
                        "SUPA_COMPAT_REALTIME_NOT_JOINED",
                        "join the topic before broadcasting to it",
                    ),
                ));
                return socket.send(Message::Text(msg.into())).await.map_err(|_| ());
            }
            let _ = state.realtime.bus.send(BroadcastMsg {
                topic: frame.topic.clone(),
                sender_conn: conn_id,
                payload: frame.payload.clone(),
            });
            if frame.reference.is_some() {
                let msg = reply(phx_reply("ok", json!({})));
                socket.send(Message::Text(msg.into())).await.map_err(|_| ())
            } else {
                Ok(())
            }
        }
        "presence" | "presence_state" | "presence_diff" => {
            let msg = reply(phx_reply(
                "error",
                error_reason(
                    "SUPA_COMPAT_REALTIME_PRESENCE_UNSUPPORTED",
                    "presence tracking is not implemented in this GuardianDB slice",
                ),
            ));
            socket.send(Message::Text(msg.into())).await.map_err(|_| ())
        }
        other => {
            let msg = reply(phx_reply(
                "error",
                error_reason(
                    "SUPA_COMPAT_REALTIME_UNSUPPORTED_EVENT",
                    &format!("unsupported realtime event: {other}"),
                ),
            ));
            socket.send(Message::Text(msg.into())).await.map_err(|_| ())
        }
    }
}

fn update_access_token<S: RelationalStorage>(
    state: &AppState<S>,
    auth: &mut ConnAuth,
    token: &str,
) -> Result<(), crate::supabase::jwt::JwtError> {
    let claims = state
        .project
        .keys
        .verify_api_key(token, Utc::now().timestamp())?;
    auth.claims = Some(claims);
    Ok(())
}

/// Parse one `postgres_changes` spec from a join config. Unsupported shapes
/// are typed errors (never silently accepted and never silently dropped).
fn parse_binding<S: RelationalStorage>(
    state: &AppState<S>,
    spec: &Json,
) -> Result<PgBinding, (&'static str, String)> {
    let event = spec
        .get("event")
        .and_then(Json::as_str)
        .unwrap_or("*")
        .to_ascii_uppercase();
    if !matches!(event.as_str(), "*" | "INSERT" | "UPDATE" | "DELETE") {
        return Err((
            "SUPA_COMPAT_REALTIME_UNSUPPORTED_EVENT",
            format!("unsupported postgres_changes event: {event}"),
        ));
    }
    let schema = spec
        .get("schema")
        .and_then(Json::as_str)
        .unwrap_or("public")
        .to_string();
    let table = spec
        .get("table")
        .and_then(Json::as_str)
        .filter(|t| !t.is_empty() && *t != "*")
        .map(str::to_string);
    let filter = match spec.get("filter").and_then(Json::as_str) {
        None | Some("") => None,
        Some(raw) => {
            let parsed = raw.split_once('=').and_then(|(col, rest)| {
                rest.strip_prefix("eq.")
                    .map(|v| (col.trim().to_string(), v.to_string()))
            });
            match parsed {
                Some(p) => Some(p),
                None => {
                    return Err((
                        "SUPA_COMPAT_REALTIME_UNSUPPORTED_FILTER",
                        format!(
                            "unsupported postgres_changes filter {raw:?}: this slice supports \
                             only col=eq.value"
                        ),
                    ));
                }
            }
        }
    };
    Ok(PgBinding {
        id: state.realtime.next_id(),
        event,
        schema,
        table,
        filter,
    })
}

// ---------------------------------------------------------------------------
// postgres_changes delivery
// ---------------------------------------------------------------------------

/// Deliver one engine change event to whichever of this connection's bindings
/// match — after authorizing it for the subscriber's role.
async fn deliver_change<S: RelationalStorage + 'static>(
    state: &AppState<S>,
    socket: &mut WebSocket,
    style: FrameStyle,
    auth: &ConnAuth,
    topics: &HashMap<String, TopicSub>,
    request_id: &str,
    event: &ChangeEvent,
) -> Result<(), ()> {
    // Collect matching (topic, ids) first; decode/authorize once only if
    // something matches.
    let mut matches: Vec<(&str, Option<&str>, Vec<u64>)> = Vec::new();
    for (topic, sub) in topics {
        let ids: Vec<u64> = sub
            .bindings
            .iter()
            .filter(|(b, _)| binding_matches(b, event))
            .map(|(b, _)| b.id)
            .collect();
        if !ids.is_empty() {
            matches.push((topic.as_str(), sub.join_ref.as_deref(), ids));
        }
    }
    if matches.is_empty() {
        return Ok(());
    }

    // Decode the rows with the catalog's column types.
    let catalog = match load_catalog(&state.db).await {
        Ok(Some(c)) => c,
        Ok(None) => return Ok(()),
        Err(_) => return Ok(()), // catalog unreadable: deliver nothing
    };
    let Some(table) = lookup_table(&catalog, &event.schema, &event.table) else {
        return Ok(());
    };

    if !authorized(state, auth, request_id, table, event).await {
        return Ok(());
    }

    let new_row = event.new.as_ref().map(|doc| decode_doc(table, doc));
    let old_row = event.old.as_ref().map(|doc| decode_doc(table, doc));
    let columns: Vec<Json> = table
        .columns
        .iter()
        .map(|c| json!({ "name": c.name, "type": c.ty.udt_name() }))
        .collect();

    for (topic, join_ref, ids) in matches {
        // Both the realtime-server wire keys (`type`/`record`/`old_record`)
        // and the realtime-js client keys (`eventType`/`new`/`old`) are
        // included, so raw consumers and supabase-js both work.
        let data = json!({
            "schema": event.schema,
            "table": event.table,
            "commit_timestamp": event.commit_time.to_rfc3339(),
            "eventType": event.op.as_str(),
            "type": event.op.as_str(),
            "new": new_row.clone().unwrap_or(json!({})),
            "record": new_row.clone().unwrap_or(json!({})),
            "old": old_row.clone().unwrap_or(json!({})),
            "old_record": old_row.clone().unwrap_or(json!({})),
            "columns": columns,
            "errors": Json::Null,
        });
        let payload = json!({ "ids": ids, "data": data });
        let frame = encode_frame(style, join_ref, None, topic, "postgres_changes", payload);
        socket
            .send(Message::Text(frame.into()))
            .await
            .map_err(|_| ())?;
    }
    Ok(())
}

fn binding_matches(b: &PgBinding, event: &ChangeEvent) -> bool {
    if b.event != "*" && b.event != event.op.as_str() {
        return false;
    }
    if b.schema != "*" && b.schema != event.schema {
        return false;
    }
    if let Some(t) = &b.table
        && *t != event.table
    {
        return false;
    }
    if let Some((col, value)) = &b.filter {
        // eq filters match the new row for INSERT/UPDATE and the old row for
        // DELETE (Supabase semantics).
        let doc = match event.op {
            ChangeOp::Insert | ChangeOp::Update => event.new.as_ref(),
            ChangeOp::Delete => event.old.as_ref(),
        };
        let Some(field) = doc.and_then(|d| d.get(col)) else {
            return false;
        };
        return json_text_eq(field, value);
    }
    true
}

/// Loose textual equality for `col=eq.value` filters ("2" matches 2, "true"
/// matches true, strings compare directly).
fn json_text_eq(field: &Json, value: &str) -> bool {
    match field {
        Json::String(s) => s == value,
        Json::Number(n) => n.to_string() == value,
        Json::Bool(b) => b.to_string() == value,
        Json::Null => value.eq_ignore_ascii_case("null"),
        other => {
            // Arrays/objects: compare the canonical JSON serialization.
            let text = other.to_string();
            text == value
        }
    }
}

fn lookup_table<'a>(catalog: &'a Catalog, schema: &str, name: &str) -> Option<&'a Table> {
    catalog
        .resolve_table_name(Some(schema), name)
        .and_then(|q| catalog.get_table(&q))
}

/// Is `event` visible to this subscriber? See the module docs for the exact
/// rules (bypass roles, RLS-disabled tables, PK re-select, and the DELETE /
/// no-PK "don't deliver" constraints).
async fn authorized<S: RelationalStorage + 'static>(
    state: &AppState<S>,
    auth: &ConnAuth,
    request_id: &str,
    table: &Table,
    event: &ChangeEvent,
) -> bool {
    let role = auth.role();
    // Per-table bypass: FORCE ROW LEVEL SECURITY revokes the owner roles'
    // exemption (BYPASSRLS-equivalent roles still pass).
    if role_bypasses_rls_on(&role, table) {
        return true;
    }
    if !table.rls_enabled {
        return true;
    }
    // DELETE cannot be re-checked (the row is gone): withhold.
    if event.op == ChangeOp::Delete {
        return false;
    }
    let pk = table.pk_columns();
    if pk.is_empty() {
        return false; // no PK: cannot re-select the row — withhold.
    }
    let Some(new_doc) = event.new.as_ref() else {
        return false;
    };
    let mut params = Vec::with_capacity(pk.len());
    let mut clauses = Vec::with_capacity(pk.len());
    for col in &pk {
        let Some(column) = table.column(col) else {
            return false;
        };
        let raw = new_doc.get(col).cloned().unwrap_or(Json::Null);
        let Ok(value) = SqlValue::decode_json(&raw, &column.ty) else {
            return false;
        };
        params.push(value);
        clauses.push(format!("\"{col}\" = ${}", params.len()));
    }
    let sql = format!(
        "SELECT count(*) AS c FROM \"{}\".\"{}\" WHERE {}",
        table.schema,
        table.name,
        clauses.join(" AND ")
    );
    let ctx = auth.context(request_id);
    match run_sql_as(&state.db, &ctx, &sql, params).await {
        Ok(crate::sql::ExecResult::Rows { rows, .. }) => rows
            .first()
            .and_then(|r| r.first())
            .and_then(|v| v.as_i64())
            .map(|n| n > 0)
            .unwrap_or(false),
        _ => false,
    }
}

/// Decode a stored row document into clean JSON using the table's column
/// types (bytea/base64, timestamps, uuids render like `/rest/v1` rows).
fn decode_doc(table: &Table, doc: &Json) -> Json {
    let mut out = Map::new();
    for col in &table.columns {
        let raw = doc.get(&col.name).cloned().unwrap_or(Json::Null);
        let rendered = match SqlValue::decode_json(&raw, &col.ty) {
            Ok(v) => value_to_json(&v),
            Err(_) => raw,
        };
        out.insert(col.name.clone(), rendered);
    }
    Json::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(op: ChangeOp, new: Option<Json>, old: Option<Json>) -> ChangeEvent {
        ChangeEvent {
            schema: "public".into(),
            table: "todos".into(),
            op,
            old,
            new,
            commit_time: Utc::now(),
        }
    }

    fn binding(event: &str, table: Option<&str>, filter: Option<(&str, &str)>) -> PgBinding {
        PgBinding {
            id: 1,
            event: event.to_string(),
            schema: "public".into(),
            table: table.map(str::to_string),
            filter: filter.map(|(c, v)| (c.to_string(), v.to_string())),
        }
    }

    #[test]
    fn frame_parsing_both_forms() {
        let (style, f) =
            parse_frame(r#"{"topic":"realtime:t","event":"phx_join","payload":{},"ref":"1"}"#)
                .unwrap();
        assert!(style == FrameStyle::Object);
        assert_eq!(f.topic, "realtime:t");
        assert_eq!(f.event, "phx_join");
        assert_eq!(f.reference.as_deref(), Some("1"));

        let (style, f) = parse_frame(r#"["3","4","realtime:t","heartbeat",{}]"#).unwrap();
        assert!(style == FrameStyle::Array);
        assert_eq!(f.join_ref.as_deref(), Some("3"));
        assert_eq!(f.reference.as_deref(), Some("4"));
        assert_eq!(f.event, "heartbeat");

        assert!(parse_frame("not json").is_none());
        assert!(parse_frame("[1,2,3]").is_none());
    }

    #[test]
    fn binding_matching() {
        let ev = event(ChangeOp::Insert, Some(json!({"id": 2, "title": "x"})), None);
        assert!(binding_matches(&binding("*", None, None), &ev));
        assert!(binding_matches(
            &binding("INSERT", Some("todos"), None),
            &ev
        ));
        assert!(!binding_matches(&binding("UPDATE", None, None), &ev));
        assert!(!binding_matches(&binding("*", Some("other"), None), &ev));
        assert!(binding_matches(&binding("*", None, Some(("id", "2"))), &ev));
        assert!(!binding_matches(
            &binding("*", None, Some(("id", "3"))),
            &ev
        ));
        // DELETE filters match against the old row.
        let del = event(ChangeOp::Delete, None, Some(json!({"id": 7})));
        assert!(binding_matches(
            &binding("DELETE", None, Some(("id", "7"))),
            &del
        ));
    }

    #[test]
    fn frame_encoding_round_trips() {
        let text = encode_frame(
            FrameStyle::Array,
            Some("1"),
            Some("2"),
            "realtime:t",
            "phx_reply",
            json!({"status":"ok"}),
        );
        let v: Json = serde_json::from_str(&text).unwrap();
        assert_eq!(v[2], "realtime:t");
        let text = encode_frame(
            FrameStyle::Object,
            None,
            Some("9"),
            "phoenix",
            "phx_reply",
            json!({"status":"ok"}),
        );
        let v: Json = serde_json::from_str(&text).unwrap();
        assert_eq!(v["ref"], "9");
        assert_eq!(v["event"], "phx_reply");
    }
}
