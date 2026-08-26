use std::sync::Arc;

use axum::{
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Path, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};

use crate::runtime_artifact_transfer_wire::{
    decode_request, encode_reply, Operation, ReplyCode, TransferReply, TransferRequest,
    MAX_FRAME_BYTES,
};
use crate::state::CloudState;

pub use crate::runtime_artifact_transfer_service::{TransferService, TransferStats};

pub fn routes() -> Router<Arc<CloudState>> {
    Router::new()
        .route(
            "/v1/runtime-artifact-transfer/v1/:operation",
            post(http_dispatch).layer(DefaultBodyLimit::max(MAX_FRAME_BYTES)),
        )
        .route("/v1/runtime-artifact-transfer/v1/stats", get(http_stats))
}

async fn http_dispatch(
    State(cloud): State<Arc<CloudState>>,
    Path(operation): Path<String>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    body: Bytes,
) -> Response {
    let request = match decode_request(&body) {
        Ok(request) => request,
        Err(error) => {
            let code = if error
                .to_string()
                .contains("unsupported runtime artifact transfer protocol")
            {
                ReplyCode::UnsupportedProtocol
            } else {
                ReplyCode::Malformed
            };
            return wire_response(
                StatusCode::BAD_REQUEST,
                TransferReply::error(code, None, format!("{error:#}")),
            );
        }
    };
    if operation_name(request.operation()) != operation {
        return wire_response(
            StatusCode::BAD_REQUEST,
            TransferReply::error(
                ReplyCode::Malformed,
                Some(request.key()),
                "runtime artifact transfer route and frame operations differ",
            ),
        );
    }
    let Some(claims) = claims.map(|claims| claims.0) else {
        return wire_response(
            StatusCode::FORBIDDEN,
            TransferReply::error(
                ReplyCode::Unauthorized,
                Some(request.key()),
                "authenticated mesh-node service authority is required",
            ),
        );
    };
    dispatch_authorized(&cloud, request, &claims, None).await
}

async fn http_stats(
    State(cloud): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
) -> Response {
    let Some(claims) = claims.map(|claims| claims.0) else {
        return (
            StatusCode::FORBIDDEN,
            "platform operator authority required",
        )
            .into_response();
    };
    if !claims.platform_admin && !(claims.role == "service" && claims.sub.starts_with("mesh-node:"))
    {
        return (
            StatusCode::FORBIDDEN,
            "platform operator authority required",
        )
            .into_response();
    }
    axum::Json(cloud.runtime_artifact_transfer.stats()).into_response()
}

pub async fn mesh_dispatch(
    cloud: &Arc<CloudState>,
    operation: &str,
    signer_id: Option<&str>,
    token: Option<&str>,
    body: &[u8],
) -> Vec<u8> {
    let request = match decode_request(body) {
        Ok(request) => request,
        Err(error) => {
            return encode_reply(&TransferReply::error(
                ReplyCode::Malformed,
                None,
                format!("{error:#}"),
            ))
            .unwrap_or_default();
        }
    };
    if operation_name(request.operation()) != operation {
        return encode_reply(&TransferReply::error(
            ReplyCode::Malformed,
            Some(request.key()),
            "runtime artifact transfer gossip path and frame operations differ",
        ))
        .unwrap_or_default();
    }
    let Some(signer_id) = signer_id else {
        return encode_reply(&TransferReply::error(
            ReplyCode::Unauthorized,
            Some(request.key()),
            "runtime artifact transfer requires a verified iroh signer",
        ))
        .unwrap_or_default();
    };
    let claims = match token.and_then(|token| crate::auth::verify(token).ok()) {
        Some(claims) => claims,
        None => {
            return encode_reply(&TransferReply::error(
                ReplyCode::Unauthorized,
                Some(request.key()),
                "runtime artifact transfer requires a valid signed service token",
            ))
            .unwrap_or_default();
        }
    };
    let response = dispatch_authorized(cloud, request, &claims, Some(signer_id)).await;
    response_body(response).await
}

async fn dispatch_authorized(
    cloud: &Arc<CloudState>,
    request: TransferRequest,
    claims: &crate::auth::Claims,
    signer_id: Option<&str>,
) -> Response {
    let key = request.key();
    let expected_subject = format!("mesh-node:{}", key.coordinator_node);
    let normalized_tenant = crate::admin::norm(&key.tenant).to_string();
    if normalized_tenant != key.tenant
        || crate::admin::norm(&claims.tenant) != key.tenant
        || claims.role != "service"
        || claims.sub != expected_subject
    {
        return wire_response(
            StatusCode::FORBIDDEN,
            TransferReply::error(
                ReplyCode::Unauthorized,
                Some(key),
                "runtime artifact transfer authority does not match its coordinator or tenant",
            ),
        );
    }
    if key.target_node != cloud.node_name {
        return wire_response(
            StatusCode::MISDIRECTED_REQUEST,
            TransferReply::error(
                ReplyCode::WrongTarget,
                Some(key),
                "runtime artifact transfer target does not match this node",
            ),
        );
    }
    if let Some(signer_id) = signer_id {
        let signer_matches = cloud.registry.nodes().into_iter().any(|node| {
            node.name == key.coordinator_node && node.peer_id.as_deref() == Some(signer_id)
        });
        let signer_trusted = cloud
            .trusted_peer_ids
            .read()
            .map(|trusted| trusted.contains(signer_id))
            .unwrap_or(false);
        if !signer_matches || !signer_trusted {
            return wire_response(
                StatusCode::FORBIDDEN,
                TransferReply::error(
                    ReplyCode::Unauthorized,
                    Some(key),
                    "runtime artifact transfer signer does not match a trusted coordinator",
                ),
            );
        }
    }
    let binding = match &request {
        TransferRequest::Begin(begin) => {
            crate::runtime_artifact_transfer_service::TransferBinding {
                project: begin.project.clone(),
                project_incarnation: begin.project_incarnation,
                tenant: begin.key.tenant.clone(),
            }
        }
        _ => match cloud.runtime_artifact_transfer.binding(key) {
            Some(binding) => binding,
            None => {
                return wire_response(
                    StatusCode::NOT_FOUND,
                    TransferReply::error(
                        ReplyCode::NotFound,
                        Some(key),
                        "runtime artifact transfer transaction is unavailable",
                    ),
                )
            }
        },
    };
    let lifecycle = crate::project_settings::lifecycle_write(&binding.project).await;
    let settings = match cloud
        .projects
        .get_exact(&binding.project, binding.project_incarnation)
    {
        Ok(settings) => settings,
        Err(_) => {
            return wire_response(
                StatusCode::NOT_FOUND,
                TransferReply::error(
                    ReplyCode::NotFound,
                    Some(key),
                    "runtime artifact transfer project authority is unavailable",
                ),
            )
        }
    };
    if crate::admin::norm(&settings.team) != binding.tenant || binding.tenant != key.tenant {
        return wire_response(
            StatusCode::NOT_FOUND,
            TransferReply::error(
                ReplyCode::NotFound,
                Some(key),
                "runtime artifact transfer project authority is unavailable",
            ),
        );
    }
    let reply = cloud
        .runtime_artifact_transfer
        .dispatch(request, lifecycle)
        .await;
    let status = status_for(reply.code);
    wire_response(status, reply)
}

fn operation_name(operation: Operation) -> &'static str {
    match operation {
        Operation::Begin => "begin",
        Operation::Chunk => "chunk",
        Operation::Query => "query",
        Operation::Finalize => "finalize",
        Operation::Abort => "abort",
        Operation::Prepare => "prepare",
        Operation::Commit => "commit",
        Operation::Reply => "reply",
    }
}

fn status_for(code: ReplyCode) -> StatusCode {
    match code {
        ReplyCode::Ok => StatusCode::OK,
        ReplyCode::Malformed | ReplyCode::UnsupportedProtocol => StatusCode::BAD_REQUEST,
        ReplyCode::Unauthorized => StatusCode::FORBIDDEN,
        ReplyCode::WrongTarget => StatusCode::MISDIRECTED_REQUEST,
        ReplyCode::NotFound => StatusCode::NOT_FOUND,
        ReplyCode::Conflict | ReplyCode::OutOfOrder | ReplyCode::ChunkConflict => {
            StatusCode::CONFLICT
        }
        ReplyCode::ResourceExhausted | ReplyCode::QueueFull => StatusCode::SERVICE_UNAVAILABLE,
        ReplyCode::Failed | ReplyCode::Internal => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn wire_response(status: StatusCode, reply: TransferReply) -> Response {
    match encode_reply(&reply) {
        Ok(bytes) => {
            let mut response = Response::new(Body::from(bytes));
            *response.status_mut() = status;
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/vnd.hive.runtime-artifact-transfer-v1"),
            );
            response
        }
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("runtime artifact transfer reply encoding failed: {error:#}"),
        )
            .into_response(),
    }
}

async fn response_body(response: Response) -> Vec<u8> {
    match axum::body::to_bytes(response.into_body(), MAX_FRAME_BYTES).await {
        Ok(bytes) if !bytes.is_empty() => bytes.to_vec(),
        _ => encode_reply(&TransferReply::error(
            ReplyCode::Internal,
            None,
            "runtime artifact transfer response body is unavailable",
        ))
        .unwrap_or_default(),
    }
}
