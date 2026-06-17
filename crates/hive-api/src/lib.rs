//! `hive-api` — the minimal per-Hive HTTP API.
//!
//! In the write-up this is the small API each Hive exposes for cell execution
//! requests. Here it is the ingress for the build pipeline / `hivectl`:
//! submit jobs, query state, and stream logs. It is a thin shell over the
//! control plane.

use axum::{
    body::{Body, Bytes},
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures::stream::{self, StreamExt};
use hive_controlplane::Hive;
use hive_core::{BuildJob, ClusterStatus, JobId, JobView, LogLine, SubmitResponse};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;

pub fn router(hive: Arc<Hive>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/status", get(status))
        .route("/v1/jobs", post(submit))
        .route("/v1/jobs/:id", get(get_job))
        .route("/v1/jobs/:id/logs", get(stream_logs))
        .with_state(hive)
}

/// Bind and serve until the process exits.
pub async fn serve(hive: Arc<Hive>, addr: std::net::SocketAddr) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "hive API listening");
    axum::serve(listener, router(hive)).await?;
    Ok(())
}

async fn status(State(hive): State<Arc<Hive>>) -> Json<ClusterStatus> {
    Json(hive.cluster_status())
}

async fn submit(
    State(hive): State<Arc<Hive>>,
    Json(job): Json<BuildJob>,
) -> Json<SubmitResponse> {
    let job_id = hive.submit(job);
    Json(SubmitResponse { job_id })
}

async fn get_job(
    State(hive): State<Arc<Hive>>,
    Path(id): Path<String>,
) -> Result<Json<JobView>, StatusCode> {
    hive.job_view(&JobId::from(id))
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

/// Stream a job's logs as newline-delimited JSON (NDJSON): the buffered backlog
/// first, then live lines, ending when the build completes.
async fn stream_logs(
    State(hive): State<Arc<Hive>>,
    Path(id): Path<String>,
) -> Response {
    let job_id = JobId::from(id);
    let Some((backlog, rx)) = hive.subscribe_logs(&job_id) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let backlog_stream = stream::iter(backlog.into_iter().map(ndjson_ok));

    let live = LogTail {
        rx,
        hive,
        id: job_id,
    };
    let live_stream = stream::unfold(live, |mut st| async move {
        loop {
            match tokio::time::timeout(Duration::from_millis(250), st.rx.recv()).await {
                Ok(Ok(line)) => return Some((ndjson_ok(line), st)),
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(broadcast::error::RecvError::Closed)) => return None,
                Err(_) => {
                    // Timed out waiting; if the build is finished, drain & stop.
                    if st.hive.logs_done(&st.id) {
                        match st.rx.try_recv() {
                            Ok(line) => return Some((ndjson_ok(line), st)),
                            _ => return None,
                        }
                    }
                    continue;
                }
            }
        }
    });

    let body = Body::from_stream(backlog_stream.chain(live_stream));
    Response::builder()
        .header("content-type", "application/x-ndjson")
        .body(body)
        .unwrap()
}

struct LogTail {
    rx: broadcast::Receiver<LogLine>,
    hive: Arc<Hive>,
    id: JobId,
}

fn ndjson_ok(line: LogLine) -> Result<Bytes, std::io::Error> {
    let mut s = serde_json::to_string(&line).unwrap_or_else(|_| "{}".to_string());
    s.push('\n');
    Ok(Bytes::from(s))
}
