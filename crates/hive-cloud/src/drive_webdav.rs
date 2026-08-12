//! shadw drive v1 — WebDAV mount at `/v1/drive/webdav/:project`
//! (bn-shadw-drive-v1, design §3). A fresh [`DavHandler`] wrapping a
//! project-scoped [`DriveFs`] is built PER REQUEST (cheap — `Arc` clones
//! only) so Basic-auth verification and the filesystem's tenant binding both
//! happen before `dav_server` ever sees the request.
//!
//! `DriveFs` calls the exact same storage primitives `drive_api.rs`'s REST
//! handlers use (`relational::drive_*`, `drive_blobs::*`,
//! `drive_api::resolve_and_fetch_bytes` for the owner-proxy-on-miss read
//! path) — one storage layer, two protocol fronts.
//!
//! **Known v1 path collision, accepted deliberately:** a project literally
//! named `webdav` cannot reach `/v1/drive/webdav/*` as its own REST actions
//! (`list`/`mkdir`/...) — axum's router (matchit) resolves the static
//! `webdav` segment before the dynamic `:project` segment at the same trie
//! depth, so `GET /v1/drive/webdav/list` always means "WebDAV mount,
//! project=list", never "REST action=list, project=webdav". Reserving
//! `webdav` as a disallowed project name is a platform-wide naming-validation
//! change outside this task's scope; this is the honest, minimal-footprint
//! alternative to inventing a non-standard mount path that would depart from
//! the design's explicit `/v1/drive/webdav/:project`.

use axum::{
    extract::{Path, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::any,
    Router,
};
use dav_server::davpath::DavPath;
use dav_server::fakels::FakeLs;
use dav_server::fs::{
    DavDirEntry, DavFile, DavFileSystem, DavMetaData, FsError, FsFuture, FsResult, FsStream,
    OpenOptions, ReadDirMeta,
};
use dav_server::DavHandler;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::relational::{self, DriveNode};
use crate::state::CloudState;

pub fn routes() -> Router<Arc<CloudState>> {
    Router::new()
        .route("/v1/drive/webdav/:project", any(handler))
        .route("/v1/drive/webdav/:project/", any(handler))
        .route("/v1/drive/webdav/:project/*rest", any(handler))
}

fn unauthorized() -> Response {
    let mut resp = Response::new(axum::body::Body::from("authentication required"));
    *resp.status_mut() = StatusCode::UNAUTHORIZED;
    resp.headers_mut().insert(
        axum::http::header::WWW_AUTHENTICATE,
        axum::http::HeaderValue::from_static("Basic realm=\"shadw drive\""),
    );
    resp
}

/// Decode `Authorization: Basic <b64>` into (username, password); `None` on
/// any absent/malformed header (never a panic on attacker-controlled input).
fn basic_credentials(req: &Request) -> Option<(String, String)> {
    let v = req.headers().get(axum::http::header::AUTHORIZATION)?.to_str().ok()?;
    let b64 = v.strip_prefix("Basic ")?;
    use base64::Engine;
    let raw = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    let s = String::from_utf8(raw).ok()?;
    let (user, pass) = s.split_once(':')?;
    Some((user.to_string(), pass.to_string()))
}

async fn handler(
    State(c): State<Arc<CloudState>>,
    Path(params): Path<HashMap<String, String>>,
    req: Request,
) -> Response {
    let Some(project) = params.get("project").cloned() else {
        return (StatusCode::BAD_REQUEST, "missing project").into_response();
    };
    // A :project that isn't the authenticated credential's own project is
    // 403 before any DB lookup (design §5) — `basic_credentials` + the
    // username==project check together ARE that gate, since a WebDAV
    // credential is minted per-project and its username IS the project.
    let Some((user, pass)) = basic_credentials(&req) else {
        return unauthorized();
    };
    if !crate::drive_api::verify_webdav_credentials(&project, &user, &pass).await {
        return unauthorized();
    }

    let tenant = crate::admin::norm(&c.projects.team_of(&project)).to_string();
    let fs = DriveFs {
        cloud: c.clone(),
        project: project.clone(),
        tenant,
    };
    let prefix = format!("/v1/drive/webdav/{project}");
    let dav = DavHandler::builder()
        .filesystem(Box::new(fs))
        .locksystem(FakeLs::new())
        .strip_prefix(prefix)
        .build_handler();
    let resp = dav.handle(req).await;
    resp.map(axum::body::Body::new)
}

#[derive(Clone)]
struct DriveFs {
    cloud: Arc<CloudState>,
    project: String,
    tenant: String,
}

fn to_fs_path(path: &DavPath) -> String {
    path.as_rel_ospath().to_string_lossy().to_string()
}

#[derive(Debug, Clone)]
struct NodeMeta {
    size: u64,
    mtime_ms: i64,
    is_dir: bool,
}

impl DavMetaData for NodeMeta {
    fn len(&self) -> u64 {
        self.size
    }
    fn modified(&self) -> FsResult<SystemTime> {
        Ok(UNIX_EPOCH + Duration::from_millis(self.mtime_ms.max(0) as u64))
    }
    fn is_dir(&self) -> bool {
        self.is_dir
    }
}

fn meta_of(n: &DriveNode) -> NodeMeta {
    NodeMeta {
        size: n.size_bytes.max(0) as u64,
        mtime_ms: n.mtime,
        is_dir: n.kind == "dir",
    }
}

struct DirEntry {
    name: String,
    meta: NodeMeta,
}

impl DavDirEntry for DirEntry {
    fn name(&self) -> Vec<u8> {
        self.name.clone().into_bytes()
    }
    fn metadata(&self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        let meta = self.meta.clone();
        Box::pin(async move { Ok(Box::new(meta) as Box<dyn DavMetaData>) })
    }
}

/// Read-mode file: bytes are resolved (local-or-proxied) once at `open()` —
/// v1 has no true streaming, matching every other body handler in this
/// crate (`axum::body::Bytes` extraction bounded by the admin body limit).
#[derive(Debug)]
struct ReadFile {
    data: Vec<u8>,
    pos: usize,
    meta: NodeMeta,
}

impl DavFile for ReadFile {
    fn metadata(&mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        let meta = self.meta.clone();
        Box::pin(async move { Ok(Box::new(meta) as Box<dyn DavMetaData>) })
    }
    fn write_buf(&mut self, _buf: Box<dyn bytes::Buf + Send>) -> FsFuture<'_, ()> {
        Box::pin(async { Err(FsError::Forbidden) })
    }
    fn write_bytes(&mut self, _buf: bytes::Bytes) -> FsFuture<'_, ()> {
        Box::pin(async { Err(FsError::Forbidden) })
    }
    fn read_bytes(&mut self, count: usize) -> FsFuture<'_, bytes::Bytes> {
        Box::pin(async move {
            let end = (self.pos + count).min(self.data.len());
            let out = bytes::Bytes::copy_from_slice(&self.data[self.pos..end]);
            self.pos = end;
            Ok(out)
        })
    }
    fn seek(&mut self, pos: std::io::SeekFrom) -> FsFuture<'_, u64> {
        Box::pin(async move {
            let base = match pos {
                std::io::SeekFrom::Start(n) => n as i64,
                std::io::SeekFrom::End(n) => self.data.len() as i64 + n,
                std::io::SeekFrom::Current(n) => self.pos as i64 + n,
            };
            if base < 0 {
                return Err(FsError::GeneralFailure);
            }
            self.pos = (base as usize).min(self.data.len());
            Ok(self.pos as u64)
        })
    }
    fn flush(&mut self) -> FsFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}

/// Write-mode file: buffers in memory (same bounded-body convention as
/// `drive_api::put_file`) and commits — quota check, blob store, metadata
/// upsert — once, in `flush()` (called exactly once after every `write_*`
/// call, per `dav_server::handle_put`'s own body loop).
struct WriteFile {
    cloud: Arc<CloudState>,
    project: String,
    tenant: String,
    parent_id: String,
    name: String,
    buf: Vec<u8>,
    flushed: bool,
}

// `CloudState` has no `Debug` impl (it holds live connection pools /
// non-Debug guardian-db handles), so this is written by hand rather than
// derived — `DavFile: Debug` only needs something legible for logging, never
// a real field dump.
impl std::fmt::Debug for WriteFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteFile")
            .field("project", &self.project)
            .field("name", &self.name)
            .field("buf_len", &self.buf.len())
            .field("flushed", &self.flushed)
            .finish()
    }
}

impl DavFile for WriteFile {
    fn metadata(&mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        let size = self.buf.len() as u64;
        Box::pin(async move {
            Ok(Box::new(NodeMeta {
                size,
                mtime_ms: hive_core::now_ms() as i64,
                is_dir: false,
            }) as Box<dyn DavMetaData>)
        })
    }
    fn write_buf(&mut self, mut buf: Box<dyn bytes::Buf + Send>) -> FsFuture<'_, ()> {
        Box::pin(async move {
            while buf.has_remaining() {
                let chunk = buf.chunk().to_vec();
                let n = chunk.len();
                self.buf.extend_from_slice(&chunk);
                buf.advance(n);
            }
            Ok(())
        })
    }
    fn write_bytes(&mut self, b: bytes::Bytes) -> FsFuture<'_, ()> {
        Box::pin(async move {
            self.buf.extend_from_slice(&b);
            Ok(())
        })
    }
    fn read_bytes(&mut self, _count: usize) -> FsFuture<'_, bytes::Bytes> {
        Box::pin(async { Err(FsError::Forbidden) })
    }
    fn seek(&mut self, _pos: std::io::SeekFrom) -> FsFuture<'_, u64> {
        Box::pin(async { Err(FsError::NotImplemented) })
    }
    fn flush(&mut self) -> FsFuture<'_, ()> {
        Box::pin(async move {
            if self.flushed {
                return Ok(());
            }
            self.flushed = true;
            let max = crate::billing::plan_max_drive_bytes(&crate::drive_api::plan_of(&self.cloud, &self.tenant));
            if max > 0 {
                let used = relational::drive_bytes_used(&self.project).await;
                if used.saturating_add(self.buf.len() as u64) > max {
                    return Err(FsError::InsufficientStorage);
                }
            }
            let hash = crate::drive_blobs::add_bytes(bytes::Bytes::from(self.buf.clone()))
                .await
                .map_err(|_| FsError::GeneralFailure)?;
            relational::drive_put_file(
                &self.project,
                &self.parent_id,
                &self.name,
                self.buf.len() as u64,
                "application/octet-stream",
                &hash,
                &self.tenant,
                &self.cloud.node_name,
            )
            .await
            .map_err(|_| FsError::Exists)?;
            Ok(())
        })
    }
}

impl DavFileSystem for DriveFs {
    fn open<'a>(&'a self, path: &'a DavPath, options: OpenOptions) -> FsFuture<'a, Box<dyn DavFile>> {
        Box::pin(async move {
            let p = to_fs_path(path);
            if options.write {
                let (parent_id, name) = crate::drive_api::resolve_parent(&self.project, &p)
                    .await
                    .map_err(|_| FsError::NotFound)?;
                if options.create_new
                    && relational::drive_resolve(&self.project, &parent_id, &name).await.is_some()
                {
                    return Err(FsError::Exists);
                }
                Ok(Box::new(WriteFile {
                    cloud: self.cloud.clone(),
                    project: self.project.clone(),
                    tenant: self.tenant.clone(),
                    parent_id,
                    name,
                    buf: Vec::new(),
                    flushed: false,
                }) as Box<dyn DavFile>)
            } else {
                let (node, data) = crate::drive_api::resolve_and_fetch_bytes(&self.cloud, &self.project, &p, &self.tenant)
                    .await
                    .ok_or(FsError::NotFound)?;
                Ok(Box::new(ReadFile {
                    data,
                    pos: 0,
                    meta: meta_of(&node),
                }) as Box<dyn DavFile>)
            }
        })
    }

    fn read_dir<'a>(
        &'a self,
        path: &'a DavPath,
        _meta: ReadDirMeta,
    ) -> FsFuture<'a, FsStream<Box<dyn DavDirEntry>>> {
        Box::pin(async move {
            let p = to_fs_path(path);
            let dir_id = crate::drive_api::resolve_dir_id(&self.project, &p)
                .await
                .map_err(|_| FsError::NotFound)?;
            let children = relational::drive_list_children(&self.project, &dir_id).await;
            let entries: Vec<FsResult<Box<dyn DavDirEntry>>> = children
                .into_iter()
                .map(|n| {
                    Ok(Box::new(DirEntry {
                        name: n.name.clone(),
                        meta: meta_of(&n),
                    }) as Box<dyn DavDirEntry>)
                })
                .collect();
            let stream: FsStream<Box<dyn DavDirEntry>> = Box::pin(futures::stream::iter(entries));
            Ok(stream)
        })
    }

    fn metadata<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, Box<dyn DavMetaData>> {
        Box::pin(async move {
            let p = to_fs_path(path);
            if p.is_empty() {
                return Ok(Box::new(NodeMeta { size: 0, mtime_ms: 0, is_dir: true }) as Box<dyn DavMetaData>);
            }
            let node = crate::drive_api::resolve_full(&self.project, &p)
                .await
                .ok_or(FsError::NotFound)?;
            Ok(Box::new(meta_of(&node)) as Box<dyn DavMetaData>)
        })
    }

    fn create_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let p = to_fs_path(path);
            let (parent_id, name) = crate::drive_api::resolve_parent(&self.project, &p)
                .await
                .map_err(|_| FsError::NotFound)?;
            relational::drive_mkdir(&self.project, &parent_id, &name, &self.tenant)
                .await
                .map(|_| ())
                .map_err(|_| FsError::Exists)
        })
    }

    fn remove_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let p = to_fs_path(path);
            let node = crate::drive_api::resolve_full(&self.project, &p)
                .await
                .ok_or(FsError::NotFound)?;
            if node.kind != "dir" {
                return Err(FsError::Forbidden);
            }
            if !relational::drive_list_children(&self.project, &node.id).await.is_empty() {
                return Err(FsError::Forbidden);
            }
            relational::drive_tombstone(&node.id).await;
            Ok(())
        })
    }

    fn remove_file<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let p = to_fs_path(path);
            let node = crate::drive_api::resolve_full(&self.project, &p)
                .await
                .ok_or(FsError::NotFound)?;
            if node.kind != "file" {
                return Err(FsError::Forbidden);
            }
            relational::drive_tombstone(&node.id).await;
            Ok(())
        })
    }

    fn rename<'a>(&'a self, from: &'a DavPath, to: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let from_p = to_fs_path(from);
            let to_p = to_fs_path(to);
            let node = crate::drive_api::resolve_full(&self.project, &from_p)
                .await
                .ok_or(FsError::NotFound)?;
            let (new_parent_id, new_name) = crate::drive_api::resolve_parent(&self.project, &to_p)
                .await
                .map_err(|_| FsError::NotFound)?;
            if relational::drive_resolve(&self.project, &new_parent_id, &new_name).await.is_some() {
                return Err(FsError::Exists);
            }
            relational::drive_move(&node.id, &new_parent_id, &new_name).await;
            Ok(())
        })
    }
}
