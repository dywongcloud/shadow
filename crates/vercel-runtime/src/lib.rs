//! First-party, source-compatible shim for the **public** `vercel_runtime` 2.4 handler contract.
//!
//! A Vercel-shaped `api/*.rs` handler compiles against this crate unchanged:
//!
//! ```rust,ignore
//! use serde_json::{Value, json};
//! use vercel_runtime::{Error, Request, run, service_fn};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Error> {
//!     run(service_fn(handler)).await
//! }
//! async fn handler(_req: Request) -> Result<Value, Error> { Ok(json!({"message":"Hello, world!"})) }
//! ```
//!
//! **Honest boundary.** This is NOT Vercel's crate. `run` binds a loopback HTTP/1 listener on
//! `$PORT` (the platform's [`FunctionLaunch`] convention), or `VERCEL_DEV_PORT`, default 3000.
//! It answers `/_vercel/ping`. Vercel's `VERCEL_IPC_PATH` start/end/log frames, invocation-id
//! headers, `waitUntil` IPC drain and `.func` provisioning are deliberately NOT implemented: those
//! are Vercel's proprietary bridge, and our functions are plain-HTTP `$PORT` processes with
//! `env_clear()`. See `docs/rust-functions.md`.
//!
//! Canonical bytes and content identities are the platform's own concern; this crate does no
//! hashing. All dependencies are pinned in the workspace `Cargo.lock` and resolve offline.
//!
//! [`FunctionLaunch`]: https://docs.rs/hive-core (see `crates/hive-core/src/proto.rs`).

use bytes::Bytes;
use http_body_util::{BodyExt, Full, StreamBody, combinators::BoxBody};
use hyper::{Request as HyperRequest, Response, StatusCode, body::Frame, server::conn::http1};
use hyper_util::rt::TokioIo;
use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::net::TcpListener;
use tower::{Service, ServiceExt};

pub use hyper::Response as HttpResponse;
pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Request = HyperRequest<hyper::body::Incoming>;

/// Body wrapper mirroring `vercel_runtime::ResponseBody`.
#[derive(Debug)]
pub struct ResponseBody(pub BoxBody<Bytes, Error>);

impl std::ops::Deref for ResponseBody {
    type Target = BoxBody<Bytes, Error>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for ResponseBody {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl http_body::Body for ResponseBody {
    type Data = Bytes;
    type Error = Error;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        Pin::new(&mut self.0).poll_frame(cx)
    }
    fn is_end_stream(&self) -> bool {
        self.0.is_end_stream()
    }
    fn size_hint(&self) -> http_body::SizeHint {
        self.0.size_hint()
    }
}

fn full(bytes: Bytes) -> ResponseBody {
    ResponseBody(Full::new(bytes).map_err(|e| Box::new(e) as Error).boxed())
}

impl From<&str> for ResponseBody {
    fn from(v: &str) -> Self {
        full(Bytes::from(v.to_string()))
    }
}
impl From<String> for ResponseBody {
    fn from(v: String) -> Self {
        full(Bytes::from(v))
    }
}
impl From<Bytes> for ResponseBody {
    fn from(v: Bytes) -> Self {
        full(v)
    }
}
impl From<Vec<u8>> for ResponseBody {
    fn from(v: Vec<u8>) -> Self {
        full(Bytes::from(v))
    }
}
impl From<()> for ResponseBody {
    fn from(_: ()) -> Self {
        full(Bytes::new())
    }
}
impl From<serde_json::Value> for ResponseBody {
    fn from(v: serde_json::Value) -> Self {
        full(Bytes::from(
            serde_json::to_vec(&v).unwrap_or_else(|_| b"{}".to_vec()),
        ))
    }
}
impl<T> From<StreamBody<T>> for ResponseBody
where
    T: tokio_stream::Stream<Item = Result<Frame<Bytes>, Error>> + Send + Sync + 'static,
{
    fn from(v: StreamBody<T>) -> Self {
        ResponseBody(v.boxed())
    }
}

/// The `IntoFunctionResponse` adapter: converts a handler's output into an HTTP response.
pub trait IntoFunctionResponse {
    fn into_response(self) -> Result<Response<ResponseBody>, Error>;
}

fn ok(
    status: StatusCode,
    ct: Option<&'static str>,
    body: ResponseBody,
) -> Result<Response<ResponseBody>, Error> {
    let mut b = Response::builder().status(status);
    if let Some(c) = ct {
        b = b.header("content-type", c);
    }
    Ok(b.body(body)?)
}

impl IntoFunctionResponse for Response<ResponseBody> {
    fn into_response(self) -> Result<Response<ResponseBody>, Error> {
        Ok(self)
    }
}
impl<T: IntoFunctionResponse> IntoFunctionResponse for Result<T, Error> {
    fn into_response(self) -> Result<Response<ResponseBody>, Error> {
        match self {
            Ok(v) => v.into_response(),
            Err(e) => ok(
                StatusCode::INTERNAL_SERVER_ERROR,
                Some("application/json"),
                ResponseBody::from(format!("{{\"error\": \"{}\"}}", e)),
            ),
        }
    }
}
impl IntoFunctionResponse for String {
    fn into_response(self) -> Result<Response<ResponseBody>, Error> {
        ok(StatusCode::OK, Some("text/plain"), ResponseBody::from(self))
    }
}
impl IntoFunctionResponse for &str {
    fn into_response(self) -> Result<Response<ResponseBody>, Error> {
        ok(StatusCode::OK, Some("text/plain"), ResponseBody::from(self))
    }
}
impl IntoFunctionResponse for Bytes {
    fn into_response(self) -> Result<Response<ResponseBody>, Error> {
        ok(StatusCode::OK, None, ResponseBody::from(self))
    }
}
impl IntoFunctionResponse for Vec<u8> {
    fn into_response(self) -> Result<Response<ResponseBody>, Error> {
        ok(
            StatusCode::OK,
            Some("application/octet-stream"),
            ResponseBody::from(self),
        )
    }
}
impl IntoFunctionResponse for serde_json::Value {
    fn into_response(self) -> Result<Response<ResponseBody>, Error> {
        ok(
            StatusCode::OK,
            Some("application/json"),
            ResponseBody::from(self),
        )
    }
}
impl IntoFunctionResponse for StatusCode {
    fn into_response(self) -> Result<Response<ResponseBody>, Error> {
        ok(self, None, ResponseBody::from(""))
    }
}
impl IntoFunctionResponse for () {
    fn into_response(self) -> Result<Response<ResponseBody>, Error> {
        ok(StatusCode::NO_CONTENT, None, ResponseBody::from(()))
    }
}
impl<T: IntoFunctionResponse> IntoFunctionResponse for Option<T> {
    fn into_response(self) -> Result<Response<ResponseBody>, Error> {
        match self {
            Some(v) => v.into_response(),
            None => ok(
                StatusCode::NOT_FOUND,
                Some("text/plain"),
                ResponseBody::from("Not Found"),
            ),
        }
    }
}

fn map_parts<T: Into<ResponseBody>>(r: Response<T>) -> Response<ResponseBody> {
    let (p, b) = r.into_parts();
    Response::from_parts(p, b.into())
}
macro_rules! resp_impl {
    ($t:ty) => {
        impl IntoFunctionResponse for Response<$t> {
            fn into_response(self) -> Result<Response<ResponseBody>, Error> {
                Ok(map_parts(self))
            }
        }
    };
}
resp_impl!(String);
resp_impl!(&str);
resp_impl!(Vec<u8>);
resp_impl!(Bytes);
resp_impl!(serde_json::Value);

/// `Html<T>` from the upstream API, as a typed HTML response.
pub struct Html<T>(pub T);
impl<T: AsRef<str>> IntoFunctionResponse for Html<T> {
    fn into_response(self) -> Result<Response<ResponseBody>, Error> {
        ok(
            StatusCode::OK,
            Some("text/html; charset=utf-8"),
            ResponseBody::from(self.0.as_ref().to_string()),
        )
    }
}
impl<T: AsRef<str>> IntoFunctionResponse for Response<Html<T>> {
    fn into_response(self) -> Result<Response<ResponseBody>, Error> {
        let (p, b) = self.into_parts();
        ok(
            p.status,
            Some("text/html; charset=utf-8"),
            ResponseBody::from(b.0.as_ref().to_string()),
        )
    }
}

/// Per-request logging context. Vercel's IPC log frames are not implemented; `info`/`error`/
/// `warn`/`debug` write to stdout/stderr so a platform-side `read_capped_line` reader still sees
/// them. Never take a tenant string into a host path.
#[derive(Clone, Default)]
pub struct LogContext;
impl LogContext {
    pub fn new() -> Self {
        Self
    }
    pub fn info(&self, m: &str) {
        println!("INFO: {m}");
    }
    pub fn error(&self, m: &str) {
        eprintln!("ERROR: {m}");
    }
    pub fn warn(&self, m: &str) {
        println!("WARN: {m}");
    }
    pub fn debug(&self, m: &str) {
        println!("DEBUG: {m}");
    }
}

#[derive(Clone)]
pub struct AppState {
    pub log_context: LogContext,
}
impl AppState {
    pub fn new(log_context: LogContext) -> Self {
        Self { log_context }
    }
    /// `waitUntil` equivalent. Background futures are spawned and run to completion; there is no
    /// IPC drain. The process is supervised by `Fluid`'s health loop, not by a per-request guard.
    pub fn wait_until<F>(&self, f: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        tokio::spawn(f);
    }
}

pub trait Handler {
    type Output: IntoFunctionResponse;
    type Future: Future<Output = Self::Output> + Send + 'static;
    fn call(&self, req: Request, state: AppState) -> Self::Future;
}
impl<F, Fut, R> Handler for F
where
    F: Fn(Request, AppState) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = R> + Send + 'static,
    R: IntoFunctionResponse,
{
    type Output = R;
    type Future = Fut;
    fn call(&self, req: Request, state: AppState) -> Self::Future {
        self(req, state)
    }
}

#[derive(Clone)]
pub struct StatelessHandler<F> {
    f: F,
}
impl<F, Fut, R> Handler for StatelessHandler<F>
where
    F: Fn(Request) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = R> + Send + 'static,
    R: IntoFunctionResponse,
{
    type Output = R;
    type Future = Fut;
    fn call(&self, req: Request, _s: AppState) -> Self::Future {
        (self.f)(req)
    }
}

pub trait IntoServiceFn<Args> {
    type Handler: Handler;
    fn into_service_fn(self) -> ServiceFn<Self::Handler>;
}
impl<F, Fut, R> IntoServiceFn<(Request, AppState)> for F
where
    F: Fn(Request, AppState) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = R> + Send + 'static,
    R: IntoFunctionResponse,
{
    type Handler = F;
    fn into_service_fn(self) -> ServiceFn<F> {
        ServiceFn { f: self }
    }
}
impl<F, Fut, R> IntoServiceFn<(Request,)> for F
where
    F: Fn(Request) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = R> + Send + 'static,
    R: IntoFunctionResponse,
{
    type Handler = StatelessHandler<F>;
    fn into_service_fn(self) -> ServiceFn<StatelessHandler<F>> {
        ServiceFn {
            f: StatelessHandler { f: self },
        }
    }
}

/// Creates a Tower service from a handler function, matching the upstream signature.
pub fn service_fn<F, Args>(handler: F) -> ServiceFn<F::Handler>
where
    F: IntoServiceFn<Args>,
{
    handler.into_service_fn()
}

#[derive(Clone)]
pub struct ServiceFn<H> {
    f: H,
}
impl<H> Service<(AppState, Request)> for ServiceFn<H>
where
    H: Handler + Clone + Send + 'static,
    H::Future: Send + 'static,
    H::Output: Send + 'static,
{
    type Response = Response<ResponseBody>;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;
    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
    fn call(&mut self, (state, req): (AppState, Request)) -> Self::Future {
        let f = self.f.clone();
        Box::pin(async move { f.call(req, state).await.into_response() })
    }
}

/// Bind a loopback HTTP/1 listener. `$PORT` is the platform's `FunctionLaunch` contract;
/// `VERCEL_DEV_PORT` is Vercel's `vc dev` contract. Honour `PORT` first so the SAME binary runs
/// on Vercel and here without recompilation. A tenant never names a loopback address — the port is
/// chosen by the backend.
pub async fn run<S>(service: S) -> Result<(), Error>
where
    S: Service<
            (AppState, Request),
            Response = Response<ResponseBody>,
            Error = Error,
        > + Send
        + Clone
        + 'static,
    S::Future: Send + 'static,
{
    let port: u16 = std::env::var("PORT")
        .or_else(|_| std::env::var("VERCEL_DEV_PORT"))
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).await?;
    eprintln!("vercel_runtime: listening on 127.0.0.1:{port}");
    loop {
        let (stream, _) = listener.accept().await?;
        let svc = service.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let hyper_svc = hyper::service::service_fn(move |req: Request| {
                let mut svc = svc.clone();
                async move {
                    if req.uri().path() == "/_vercel/ping" {
                        return Ok::<_, Infallible>(
                            Response::builder()
                                .status(200)
                                .body(ResponseBody::from("OK"))
                                .unwrap_or_else(|_| {
                                    Response::new(ResponseBody::from("OK"))
                                }),
                        )
                    } else {
                        let response = match ServiceExt::ready(&mut svc).await {
                            Ok(ready) => {
                                match Service::call(ready, (AppState::new(LogContext::new()), req))
                                    .await
                                {
                                    Ok(r) => r,
                                    Err(_) => Response::builder()
                                        .status(500)
                                        .header("connection", "close")
                                        .body(ResponseBody::from("Internal Server Error"))
                                        .unwrap_or_else(|_| {
                                            Response::new(ResponseBody::from(
                                                "Internal Server Error",
                                            ))
                                        }),
                                }
                            }
                            Err(_) => Response::builder()
                                .status(500)
                                .header("connection", "close")
                                .body(ResponseBody::from("Service Not Ready"))
                                .unwrap_or_else(|_| Response::new(ResponseBody::from("Service Not Ready"))),
                        };
                        Ok::<_, Infallible>(response)
                    }
                }
            });
            let _ = http1::Builder::new()
                .serve_connection(io, hyper_svc)
                .with_upgrades()
                .await;
        });
    }
}

/// `waitUntil` background-work collector, mirroring the upstream module name. The upstream crate
/// drains at SIGTERM; our process is supervised by `Fluid`'s health loop and `kill_on_drop`, so
/// futures are simply spawned and a caller that needs a hard guarantee should own its own JoinHandle.
pub mod awaiter {
    use std::future::Future;
    /// Register a background future. Errors/panics are isolated to the spawned task, never abort
    /// the drain.
    pub fn wait_until<F>(f: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        tokio::spawn(f);
    }
    /// Seconds a `vc dev` bounded drain would use; kept for API parity. Production drains are
    /// unbounded.
    pub const WAIT_UNTIL_TIMEOUT: u64 = 30;
}

/// Minimal `axum`-feature module: declared so a Vercel Cargo.toml with `features = ["axum"]`
/// resolves offline. This is a generic Tower layer + streaming helper over hyper/tower; it does NOT
/// pull `axum 0.8` / `axum-streams` (neither is in the locked graph), and it is NOT a drop-in for
/// Vercel's axum `Router` adapter. The core `service_fn`/`run` handler contract above is the
/// supported path. See docs/rust-functions.md.
#[cfg(feature = "axum")]
pub mod axum {
    use super::*;
    use tower::Layer;

    /// Identity layer over a service taking `(AppState, Request)`, matching the upstream name so
    /// `ServiceBuilder::new().layer(VercelLayer::new())` resolves. State is passed through, never
    /// discarded.
    #[derive(Clone, Default)]
    pub struct VercelLayer;
    impl VercelLayer {
        pub fn new() -> Self {
            Self
        }
    }
    impl<S> Layer<S> for VercelLayer {
        type Service = S;
        fn layer(&self, inner: S) -> Self::Service {
            inner
        }
    }

    /// Build a chunked response from a channel generator. The generator owns the sender; each
    /// `Frame<Bytes>` it emits is streamed. Bounded by the caller's channel capacity, never by an
    /// unbounded buffer.
    pub fn stream_response<F, Fut>(generator: F) -> Response<ResponseBody>
    where
        F: FnOnce(tokio::sync::mpsc::Sender<Result<Frame<Bytes>, Error>>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Frame<Bytes>, Error>>(16);
        tokio::spawn(generator(tx));
        let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        let body = StreamBody::new(stream);
        Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .body(ResponseBody::from(body))
            .unwrap_or_else(|_| {
                Response::new(ResponseBody::from("stream error"))
            })
    }
}

/// `actix` feature parity: declared so a manifest naming it resolves offline, but no Actix adapter
/// is provided (actix-web is not in the locked graph). Kept explicit so a handler importing
/// `vercel_runtime::actix` fails loudly at compile time rather than silently at runtime.
#[cfg(feature = "actix")]
compile_error!(
    "vercel_runtime::actix is not implemented by this platform shim; use the core run/service_fn handler contract"
);
