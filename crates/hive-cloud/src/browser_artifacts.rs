//! Browser-executable function artifacts: build-time bundling, the content-addressed on-disk store
//! and its guarded GC (browser-function-artifact-build-contract).
//!
//! A function is browser-eligible by fluid.json opt-in (`functions[].browser`,
//! [`fluid_core::BrowserPolicy`]) or automatically (`git::infer_browser_entry`), once it survives
//! [`bundle`]'s rejection pass. The artifact is ONE deterministic, self-contained source string, an
//! `async function (request, ops)` expression in the shape the substrate evaluates (`module.exports
//! = (<source>)`, run as CommonJS by the vendored node-worker guest, see
//! `crates/hive-browser/www/node-worker-host.js`), with no env and no secrets.
//!
//! The build does NOT reject Node/Bun runtime tokens (`require`, `process.*`, `Buffer`,
//! `__dirname`, `import(...)`, `Deno.*`/`Bun.*`): the substrate supplies a Node API, and a module
//! it cannot provide (`net`, `dns`, outbound `http.request`) throws a NAMED error at the point of
//! use. ES module syntax is REWRITTEN to CommonJS ([`crate::browser_esm`]) because the substrate
//! loads FILES while the artifact is an EXPRESSION, and rejecting it excluded every framework build
//! output; only forms with no CommonJS equivalent are refused (mixing `export default` with named
//! exports, an unreadable `export`, an unterminated statement). A CommonJS entry is returned
//! byte-identical.
//!
//! Refused loudly: what NO substrate can provide (machine code: `process.dlopen`, a `*.node` addon,
//! `node-gyp-build`; Node's internal `process.binding`) plus the standing checks (non-JS runtime,
//! TypeScript entry, oversized source, no handler export).
//!
//! Store layout, `persist::data_dir()/browser-artifacts/`: `<policy_digest>.js` is the emitted
//! source, content-addressed by the canonical-policy digest (the 64-hex value the wire protocol
//! routes on); `<policy_digest>.json` is a sidecar with the [`fluid_core::BrowserArtifact`]
//! descriptor and owning deployment ids (bookkeeping only: the GC keep-set comes from LIVE
//! deployment records). The bytes are node-local and do NOT ride `PlatformSnapshot` or store_sync;
//! only descriptor metadata replicates, inside the deployment manifest.
//!
//! GC ([`gc`], [`spawn_gc_loop`]) copies `gc_rootfs_images`' blast-radius guards: an empty
//! keep-set, or a reap set over `HIVE_BROWSER_ARTIFACT_GC_MAX_REAP_FRACTION` of the store, refuses
//! the whole pass, and only files older than a grace window are candidates.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    extract::{Path as AxumPath, Query, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use fluid_core::{BrowserArtifact, BrowserFunctionRef, FunctionConfig};

use crate::state::CloudState;

const STORE_DIR_NAME: &str = "browser-artifacts";
/// Files younger than this are never GC candidates: a concurrent build may
/// have written bytes whose deployment record isn't visible yet.
const DEFAULT_GC_GRACE_SECS: u64 = 600;
const DEFAULT_GC_INTERVAL_SECS: u64 = 3600;
/// Same shape as `HIVE_GC_MAX_REAP_FRACTION` for rootfs images: no legitimate
/// pass ever needs to reap most of the store at once.
const DEFAULT_GC_MAX_REAP_FRACTION: f64 = 0.5;

pub fn store_dir() -> PathBuf {
    crate::persist::data_dir().join(STORE_DIR_NAME)
}

/// The emitted bundle: the source string plus its descriptor (digests,
/// size, resolved limits) ready to stamp onto `FunctionConfig::browser_artifact`.
pub struct BundledArtifact {
    pub source: String,
    pub descriptor: BrowserArtifact,
    /// Policy-resolution notes (e.g. ceiling clamps) for the build log.
    pub notes: Vec<String>,
}

/// Whether a CommonJS entry assigns a handler the wrapper can resolve (`module.exports`,
/// `.handler`, `exports.handler` or the TypeScript-transpiled `exports.default`), mirroring the
/// wrapper's runtime resolution; a plain substring scan suffices. It must run at BUILD time, since
/// the wrapper throws when no handler resolves: a server/library/bare-script entry is rejected here
/// (skipped silently when auto-detected, a failed build when explicitly opted in) instead of
/// shipping bytes that throw in every donor's browser.
fn handler_export_present(src: &str) -> bool {
    src.contains("module.exports")
        || src.contains("exports.handler")
        || src.contains("exports.default")
}

/// Surfaces NO browser substrate can provide (machine code and Node's internal bindings), matched
/// line-wise so the rejection names the line. Everything else (a missing module, outbound
/// `http.request`) is left to the runtime to name at the point of use, never a build-time guess.
const UNIMPLEMENTABLE: &[(&str, &str)] = &[
    (
        "process.dlopen(",
        "loads a native addon (`process.dlopen`) — a browser Worker cannot execute machine code, \
         so this entry cannot run in a donor's browser at all",
    ),
    (
        "process.binding(",
        "uses Node's internal `process.binding` — there is no such API in a browser Worker",
    ),
    (
        "node-gyp-build",
        "loads a native addon through `node-gyp-build` — compiled machine code cannot run in a \
         browser Worker; use the package's pure-JS/WASM build",
    ),
    (
        "require(\"bindings\")",
        "loads a native addon through `bindings` — compiled machine code cannot run in a browser \
         Worker",
    ),
    (
        "require('bindings')",
        "loads a native addon through `bindings` — compiled machine code cannot run in a browser \
         Worker",
    ),
];

pub(crate) fn unimplementable_lines(source: &str) -> Vec<(usize, &'static str)> {
    let mut out = Vec::new();
    for (i, line) in source.lines().enumerate() {
        for (needle, why) in UNIMPLEMENTABLE {
            if line.contains(needle) {
                out.push((i + 1, *why));
            }
        }
        if (line.contains(".node\"") || line.contains(".node'"))
            && line.contains("require(")
        {
            out.push((
                i + 1,
                "requires a `*.node` native addon — compiled machine code cannot run in a browser \
                 Worker",
            ));
        }
    }
    out
}

/// The `import.meta` shim the envelope defines when the rewritten entry uses
/// it: the artifact is ONE in-memory file, so there is no real disk path — the
/// entry path is reported as it is known to the deployment.
pub(crate) fn import_meta_binding(entry: &str) -> String {
    let rel = entry.trim().trim_start_matches("./");
    let esc = rel.replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        "const {} = {{ url: \"file:///hive/{esc}\", filename: \"/hive/{esc}\", dirname: \"/hive\", \
         env: {{}}, resolve: (s) => s }};",
        crate::browser_esm::IMPORT_META_SHIM
    )
}

/// Resolves the entry path inside the deployment build dir. The entry is tenant-controlled input:
/// an absolute path, a `..` component or a symlink out of the build output (a zip can carry one)
/// must never let the build read or bundle a host file.
fn resolve_entry(build_dir: &Path, entry: &str) -> Result<PathBuf, String> {
    let rel = Path::new(entry);
    if rel.is_absolute() {
        return Err(format!(
            "browser entry {entry:?} is an absolute path — entries are relative to the deployment root"
        ));
    }
    if rel
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(format!(
            "browser entry {entry:?} escapes the deployment root (`..` components are rejected)"
        ));
    }
    let path = build_dir.join(rel);
    if !path.is_file() {
        return Err(format!(
            "browser entry {entry:?} does not exist in the build output"
        ));
    }
    let canon = path
        .canonicalize()
        .map_err(|e| format!("browser entry {entry:?} cannot be resolved: {e}"))?;
    let canon_root = build_dir
        .canonicalize()
        .map_err(|e| format!("build dir cannot be resolved: {e}"))?;
    if !canon.starts_with(&canon_root) {
        return Err(format!(
            "browser entry {entry:?} escapes the deployment root through a symlink"
        ));
    }
    Ok(path)
}

/// Bundles one opted-in (or auto-detected) function into its deterministic browser artifact. Every
/// ineligibility is a loud `Err` naming the function and the reason: the build pipeline fails an
/// EXPLICIT opt-in on it and skips an auto-detected one silently.
///
/// The emitted bytes (and both digests) depend only on the entry source and the policy: line
/// endings normalize to LF and the wrapper is a constant template. The envelope wraps the user's
/// file verbatim and (a) resolves the handler, (b) normalizes the request (a JSON string on the
/// guest path, an object on the native path), (c) normalizes the response into the `{status,
/// headers, body, bodyBase64}` string the gateway's `browser_response` parses. `__hive_node_bridge`
/// is the ONE hook the Node runtime installs: absent it (a substrate without Node, e.g. the native
/// DOM lane) `request`/`ops` pass through and the handler's RETURN value is the response; present,
/// the handler also gets Node's `(req, res)` convention, where `req` and `res` are supersets of the
/// request descriptor and `ops`, and `bridge.settle(out)` reconciles the two (an explicit non-`res`
/// return wins, else the response written on `res`). Every binding the wrapper introduces is
/// `__hive_`-prefixed because the entry shares its scope: a plain `const handler` collided with the
/// common `function handler(request, ops)` and failed to PARSE in every donor's browser.
pub fn bundle(build_dir: &Path, f: &FunctionConfig) -> Result<BundledArtifact, String> {
    let name = || format!("function {:?}", f.name);
    let Some(policy) = f.browser.as_ref() else {
        return Err(format!(
            "{} has no browser policy — bundle() is only for opted-in functions",
            name()
        ));
    };
    let resolved_runtime = hive_core::Runtime::resolve(&f.runtime, &f.start_cmd);
    match resolved_runtime {
        hive_core::Runtime::Node | hive_core::Runtime::Bun => {}
        other => {
            return Err(format!(
                "{} opted into browser execution but runs as {other:?} — only plain JS handlers \
                 (node/bun runtime) are browser-eligible; container, python, go, and command \
                 functions are unsupported",
                name()
            ));
        }
    }
    if policy.entry.trim().is_empty() {
        return Err(format!(
            "{} opted into browser execution without a `browser.entry` — the function's start_cmd \
             entry is its long-running SERVER, a different contract from the request→response \
             browser handler, so an explicit entry file is required",
            name()
        ));
    }
    let entry_path =
        resolve_entry(build_dir, policy.entry.trim()).map_err(|e| format!("{}: {e}", name()))?;
    let ext = entry_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match ext.as_str() {
        "js" | "mjs" | "cjs" => {}
        "ts" | "mts" | "cts" => {
            return Err(format!(
                "{}: browser entry {:?} is TypeScript — no deterministic toolchain-free transpile \
                 exists on the build node, so TS entries are unsupported; ship the compiled .js",
                name(),
                policy.entry
            ));
        }
        other => {
            return Err(format!(
                "{}: browser entry {:?} has unsupported extension {other:?} — only .js/.mjs/.cjs \
                 handler files are browser-eligible",
                name(),
                policy.entry
            ));
        }
    }
    let resolved = policy
        .resolve()
        .map_err(|e| format!("{}: invalid browser policy: {e}", name()))?;
    let raw = std::fs::read_to_string(&entry_path).map_err(|e| {
        format!(
            "{}: browser entry {:?} is not readable UTF-8: {e}",
            name(),
            policy.entry
        )
    })?;
    if raw.len() > fluid_core::BROWSER_ENTRY_MAX_SOURCE_BYTES {
        return Err(format!(
            "{}: browser entry {:?} is {} bytes, over the {}-byte single-file bound",
            name(),
            policy.entry,
            raw.len(),
            fluid_core::BROWSER_ENTRY_MAX_SOURCE_BYTES
        ));
    }
    let user = raw.replace("\r\n", "\n").replace('\r', "\n");

    let rewritten = match crate::browser_esm::rewrite_esm(&user) {
        Ok(esm) => esm,
        Err(reason) => {
            return Err(format!(
                "{} is browser-eligible in principle, but its entry {:?} cannot be evaluated as a \
                 browser artifact:\n  - {reason}",
                name(),
                policy.entry
            ));
        }
    };
    let user = rewritten.source;
    let mut rejections: Vec<String> = Vec::new();
    for (line, why) in unimplementable_lines(&user) {
        rejections.push(format!("line {line}: {why}"));
    }
    if !handler_export_present(&user) {
        rejections.push(
            "defines no handler export — assign the request→response handler to `module.exports` \
             (or `exports.handler`); an ES module entry exports it as `export default handler` or \
             `export const handler = ...`, which the build rewrites to CommonJS. An entry that \
             exports none is a server/library, not a browser handler"
                .to_string(),
        );
    }
    if !rejections.is_empty() {
        return Err(format!(
            "{} is browser-eligible in principle, but its entry {:?} cannot be evaluated as a \
             browser artifact:\n  - {}",
            name(),
            policy.entry,
            rejections.join("\n  - ")
        ));
    }

    let prelude = if rewritten.uses_import_meta {
        format!(
            "\x20 const exports = module.exports;\n\x20 {}\n",
            import_meta_binding(policy.entry.trim())
        )
    } else {
        "\x20 const exports = module.exports;\n".to_string()
    };
    let esm_note = if rewritten.rewritten {
        ", ES module syntax rewritten to CommonJS"
    } else {
        ""
    };
    let source = format!(
        "async function (request, ops) {{\n\
         \x20 \"use strict\";\n\
         \x20 const module = {{ exports: {{}} }};\n\
         {prelude}\
         \x20 /* ---- begin deployment entry (verbatim, LF-normalized{esm_note}) ---- */\n\
         {user}\n\
         \x20 /* ---- end deployment entry ---- */\n\
         \x20 ;const __hive_handler = typeof module.exports === \"function\" ? module.exports : (module.exports && (module.exports.handler || module.exports.default));\n\
         \x20 if (typeof __hive_handler !== \"function\") {{\n\
         \x20   throw new TypeError(\"browser entry must assign its handler to module.exports, exports.handler or exports.default\");\n\
         \x20 }}\n\
         \x20 const __hive_bridge = typeof globalThis.__hive_node_bridge === \"function\" ? globalThis.__hive_node_bridge(request, ops) : null;\n\
         \x20 const __hive_req = __hive_bridge ? __hive_bridge.request : (typeof request === \"string\" ? JSON.parse(request) : request);\n\
         \x20 const __hive_res = __hive_bridge ? __hive_bridge.response : ops;\n\
         \x20 let __hive_out = await __hive_handler(__hive_req, __hive_res);\n\
         \x20 if (__hive_bridge) __hive_out = await __hive_bridge.settle(__hive_out);\n\
         \x20 if (typeof __hive_out === \"string\") return __hive_out;\n\
         \x20 if (!__hive_out || typeof __hive_out !== \"object\") {{\n\
         \x20   throw new TypeError(\"browser handler must return a response object or an envelope string\");\n\
         \x20 }}\n\
         \x20 const __hive_status = __hive_out.status === undefined ? 200 : Number(__hive_out.status);\n\
         \x20 if (!Number.isInteger(__hive_status) || __hive_status < 100 || __hive_status > 599) {{\n\
         \x20   throw new RangeError(\"browser response status must be an integer in 100..599\");\n\
         \x20 }}\n\
         \x20 const __hive_headers = __hive_out.headers && typeof __hive_out.headers === \"object\" ? __hive_out.headers : {{}};\n\
         \x20 const __hive_envelope = {{ status: __hive_status, headers: __hive_headers }};\n\
         \x20 if (typeof __hive_out.bodyBase64 === \"string\") __hive_envelope.bodyBase64 = __hive_out.bodyBase64;\n\
         \x20 __hive_envelope.body = typeof __hive_out.body === \"string\" ? __hive_out.body : JSON.stringify(__hive_out.body === undefined ? \"\" : __hive_out.body);\n\
         \x20 return JSON.stringify(__hive_envelope);\n\
         }}",
        prelude = prelude,
        esm_note = esm_note
    );
    let source_digest = fluid_core::browser_source_digest(&source);
    let policy_digest = fluid_core::browser_policy_digest(
        &source_digest,
        resolved.mode,
        resolved.timeout_ms,
        resolved.memory_bytes,
        resolved.stack_bytes,
        &resolved.allowed_ops,
    )
    .map_err(|e| format!("{}: cannot compute canonical policy digest: {e}", name()))?;
    Ok(BundledArtifact {
        descriptor: BrowserArtifact {
            source_digest,
            policy_digest,
            source_bytes: source.len() as u64,
            mode: resolved.mode,
            timeout_ms: resolved.timeout_ms,
            memory_bytes: resolved.memory_bytes,
            stack_bytes: resolved.stack_bytes,
            allowed_ops: resolved.allowed_ops,
        },
        source,
        notes: resolved.notes,
    })
}

/// Sidecar persisted next to each artifact: descriptor + owning deployments.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct ArtifactSidecar {
    descriptor: BrowserArtifact,
    #[serde(default)]
    owners: BTreeSet<String>,
    #[serde(default)]
    created_ms: u64,
}

/// The `.js`/`.json` store paths for a policy digest, validated as 64 lowercase hex BEFORE it
/// becomes a path component (a store keyed on a tenant-influenced string is the `hive-vol-`
/// bind-mount footgun again).
fn digest_paths(digest: &str) -> Result<(PathBuf, PathBuf), String> {
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(format!(
            "artifact digest must be 64 lowercase hexadecimal characters, got {digest:?}"
        ));
    }
    let dir = store_dir();
    Ok((
        dir.join(format!("{digest}.js")),
        dir.join(format!("{digest}.json")),
    ))
}

/// Persist the emitted bytes + sidecar content-addressed (idempotent: same
/// source + policy ⇒ same digest ⇒ same file). Existing owners are preserved.
/// The `.js` is written through a temp file and renamed, so a crash never leaves a truncated file
/// under a valid digest.
pub async fn persist(source: &str, descriptor: &BrowserArtifact) -> std::io::Result<()> {
    let (js_path, json_path) = digest_paths(&descriptor.policy_digest)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    tokio::fs::create_dir_all(store_dir()).await?;
    let tmp = store_dir().join(format!(".{}.tmp", descriptor.policy_digest));
    tokio::fs::write(&tmp, source.as_bytes()).await?;
    tokio::fs::rename(&tmp, &js_path).await?;
    let mut sidecar = read_sidecar(&json_path).await.unwrap_or(ArtifactSidecar {
        descriptor: descriptor.clone(),
        owners: BTreeSet::new(),
        created_ms: hive_core::now_ms(),
    });
    sidecar.descriptor = descriptor.clone();
    write_sidecar(&json_path, &sidecar).await
}

/// Record that `deployment_id` owns a reference to the artifact. Called after
/// the deployment record exists (the id is minted inside `deploy_full`).
pub async fn add_owner(policy_digest: &str, deployment_id: &str) -> std::io::Result<()> {
    let (_, json_path) = digest_paths(policy_digest)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let Some(mut sidecar) = read_sidecar(&json_path).await else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("browser artifact {policy_digest} is not persisted on this node"),
        ));
    };
    if !sidecar.owners.insert(deployment_id.to_string()) {
        return Ok(());
    }
    write_sidecar(&json_path, &sidecar).await
}

async fn read_sidecar(path: &Path) -> Option<ArtifactSidecar> {
    let bytes = tokio::fs::read(path).await.ok()?;
    serde_json::from_slice(&bytes).ok()
}

async fn write_sidecar(path: &Path, sidecar: &ArtifactSidecar) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(sidecar)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension("json.tmp");
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, path).await
}

/// Every policy digest referenced by a LIVE deployment record on this node: the GC keep-set. All
/// records count (Error-state ones too): a record leaves it only when the deployment is removed,
/// the conservative direction for reaping.
pub fn live_digests(cloud: &Arc<CloudState>) -> BTreeSet<String> {
    cloud
        .gw
        .deployment_records()
        .into_iter()
        .flat_map(|record| record.manifest.functions)
        .filter_map(|f| f.browser_artifact.map(|a| a.policy_digest))
        .collect()
}

/// Guarded reaper. Returns (reaped artifacts, reclaimed bytes).
///
/// BLAST-RADIUS GUARDS (the `gc_rootfs_images` precedent): an empty keep-set refuses the pass
/// loudly (it makes every artifact look orphaned, indistinguishable from a caller/state bug); so
/// does a candidate set over `HIVE_BROWSER_ARTIFACT_GC_MAX_REAP_FRACTION` (default 0.5) of the
/// store; and only files older than the grace window are candidates (an unreadable mtime counts as
/// recent).
pub async fn gc(keep: &BTreeSet<String>) -> (usize, u64) {
    let grace_secs = std::env::var("HIVE_BROWSER_ARTIFACT_GC_GRACE_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_GC_GRACE_SECS);
    let max_fraction = std::env::var("HIVE_BROWSER_ARTIFACT_GC_MAX_REAP_FRACTION")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|f| *f > 0.0 && *f <= 1.0)
        .unwrap_or(DEFAULT_GC_MAX_REAP_FRACTION);
    let now = std::time::SystemTime::now();
    let mut total = 0usize;
    let mut candidates: Vec<(String, u64)> = Vec::new();
    let mut entries = match tokio::fs::read_dir(store_dir()).await {
        Ok(d) => d,
        Err(_) => return (0, 0),
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(stem) = name.strip_suffix(".js") else {
            continue;
        };
        total += 1;
        if keep.contains(stem) {
            continue;
        }
        let Ok(md) = entry.metadata().await else {
            continue;
        };
        let recent = md
            .modified()
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .map(|age| age.as_secs() < grace_secs)
            .unwrap_or(true);
        if recent {
            continue;
        }
        candidates.push((stem.to_string(), md.len()));
    }
    if keep.is_empty() && total > 0 {
        tracing::error!(
            total,
            "browser-artifact gc: REFUSING to reclaim — the keep-set is empty, which would \
             delete every artifact on this node. This is a caller/state bug, not an empty store."
        );
        return (0, 0);
    }
    if total > 0 && candidates.len() as f64 / total as f64 > max_fraction {
        tracing::error!(
            candidates = candidates.len(),
            total,
            fraction = candidates.len() as f64 / total as f64,
            max_fraction,
            "browser-artifact gc: REFUSING to reclaim — reap set exceeds the max-reap fraction; \
             treat this as a keep-set bug and investigate before relaxing \
             HIVE_BROWSER_ARTIFACT_GC_MAX_REAP_FRACTION"
        );
        return (0, 0);
    }
    let mut reaped = 0usize;
    let mut reclaimed = 0u64;
    for (stem, bytes) in candidates {
        let Ok((js_path, json_path)) = digest_paths(&stem) else {
            continue;
        };
        if tokio::fs::remove_file(&js_path).await.is_ok() {
            reaped += 1;
            reclaimed += bytes;
            let _ = tokio::fs::remove_file(&json_path).await;
        }
    }
    (reaped, reclaimed)
}

/// Periodic sweep — every node (the store is per-host, like the podman lock
/// pool), same registration shape as `spawn_container_lock_sweep`.
/// The first interval tick fires at once, so two are skipped: the first pass runs a full interval
/// after boot, when persisted deployments are restored into the keep-set.
/// `HIVE_BROWSER_ARTIFACT_GC_SECS` sets the cadence (default 3600).
pub fn spawn_gc_loop(cloud: Arc<CloudState>) {
    let interval_secs = std::env::var("HIVE_BROWSER_ARTIFACT_GC_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_GC_INTERVAL_SECS);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        tick.tick().await;
        tick.tick().await;
        loop {
            let keep = live_digests(&cloud);
            let (reaped, bytes) = gc(&keep).await;
            if reaped > 0 {
                tracing::info!(
                    reaped,
                    reclaimed_mib = bytes / (1024 * 1024),
                    "browser-artifact gc: reclaimed unreferenced artifacts"
                );
            }
            tick.tick().await;
        }
    });
}

/// Admission's view of one function's browser eligibility, resolved against
/// the replicated deployment state (local gw records first, then the gossiped
/// `peer_deployments` view — the same two sources `resolve_for_tenant`
/// consults):
///
/// * `None` — no Ready deployment/function by that name exists for the
///   tenant (today's `deployment_not_ready` outcome).
/// * `Some(None)` — the function exists but carries no build-stamped browser
///   artifact descriptor: it never opted in, or the build rejected it. NOT
///   browser-eligible, loudly.
/// * `Some(Some(descriptor))` — eligible; the admission's effective digest is
///   `descriptor.policy_digest` (server-derived; the donor's own digest field
///   is rollout-compat and never consulted).
pub fn descriptor_for(
    cloud: &Arc<CloudState>,
    tenant: &str,
    deployment: &str,
    function: &str,
) -> Option<Option<BrowserFunctionRef>> {
    for record in cloud.gw.deployment_records() {
        if record.id == deployment
            && record.state == fluid_core::DeployState::Ready
            && crate::admin::record_tenant(&record.tenant) == tenant
        {
            let Some(f) = record
                .manifest
                .functions
                .iter()
                .find(|f| f.name == function)
            else {
                return None;
            };
            return Some(
                f.browser_artifact
                    .clone()
                    .map(|artifact| BrowserFunctionRef {
                        name: function.to_string(),
                        artifact,
                    }),
            );
        }
    }
    for deployments in cloud.peer_deployments.read().values() {
        for info in deployments {
            if info.id.as_str() == deployment
                && info.state == fluid_core::DeployState::Ready
                && crate::admin::record_tenant(&info.tenant) == tenant
            {
                if !info.functions.iter().any(|name| name == function) {
                    return None;
                }
                return Some(
                    info.browser_functions
                        .iter()
                        .find(|bf| bf.name == function)
                        .cloned(),
                );
            }
        }
    }
    None
}

/// One browser-eligible function of one Ready deployment, resolved from
/// replicated state (browser-auto-serve-eligible-set). Everything here is
/// SERVER-DERIVED from the deployment record — no field originates with the
/// donor, which is what lets the whole set become an admission capability.
#[derive(Clone, Debug)]
pub struct EligibleFunction {
    pub deployment: String,
    pub project: String,
    pub function: String,
    pub artifact: BrowserArtifact,
    /// Ordering inputs only (never sent to the donor): a promoted production
    /// deployment outranks a preview, and newer outranks older, so the cap
    /// below truncates the LEAST relevant work rather than an arbitrary slice.
    production: bool,
    created_at_ms: u64,
}

/// Default ceiling on how many eligible functions one admission may carry.
/// Each one costs the donor a pinned artifact (bounded source bytes, a lazily
/// booted runner) and the gateway one routing entry, so the set is bounded by
/// policy rather than by however many deployments a tenant happens to own.
const DEFAULT_AUTO_SERVE_MAX: usize = 16;

pub fn auto_serve_max() -> usize {
    std::env::var("HIVE_BROWSER_AUTO_SERVE_MAX")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_AUTO_SERVE_MAX)
        .min(fluid_gateway::MAX_BROWSER_TARGETS_PER_ENDPOINT)
}

/// EVERY browser-eligible function a tenant owns across every Ready deployment, the automatic
/// counterpart of [`descriptor_for`]'s one hand-named target. Same replicated sources, order and
/// tenant gate as `descriptor_for`/`resolve_for_tenant`, deduplicated by (deployment, function); a
/// deployment that is not Ready, belongs to another tenant or has no build-stamped artifact is
/// simply absent: absence of capability, never a broader one. Deterministically ordered
/// (production, then newest, then id/name) and truncated to `auto_serve_max()`, so every node
/// computing it from the same state agrees and a renewal does not reshuffle a donor's pins.
pub fn eligible_for_tenant(cloud: &Arc<CloudState>, tenant: &str) -> Vec<EligibleFunction> {
    let mut out: Vec<EligibleFunction> = Vec::new();
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    for record in cloud.gw.deployment_records() {
        if record.state != fluid_core::DeployState::Ready
            || crate::admin::record_tenant(&record.tenant) != tenant
        {
            continue;
        }
        for f in &record.manifest.functions {
            let Some(artifact) = f.browser_artifact.clone() else {
                continue;
            };
            if !seen.insert((record.id.clone(), f.name.clone())) {
                continue;
            }
            out.push(EligibleFunction {
                deployment: record.id.clone(),
                project: record.project.clone(),
                function: f.name.clone(),
                artifact,
                production: record.production,
                created_at_ms: record.created_at_ms,
            });
        }
    }
    for deployments in cloud.peer_deployments.read().values() {
        for info in deployments {
            if info.state != fluid_core::DeployState::Ready
                || crate::admin::record_tenant(&info.tenant) != tenant
            {
                continue;
            }
            for bf in &info.browser_functions {
                if !seen.insert((info.id.0.clone(), bf.name.clone())) {
                    continue;
                }
                out.push(EligibleFunction {
                    deployment: info.id.0.clone(),
                    project: info.project.clone(),
                    function: bf.name.clone(),
                    artifact: bf.artifact.clone(),
                    production: info.production,
                    created_at_ms: info.created_at_ms,
                });
            }
        }
    }
    out.sort_by(|a, b| {
        b.production
            .cmp(&a.production)
            .then(b.created_at_ms.cmp(&a.created_at_ms))
            .then(a.deployment.cmp(&b.deployment))
            .then(a.function.cmp(&b.function))
    });
    out.truncate(auto_serve_max());
    out
}

/// Hard ceiling on an artifact body served or accepted from a peer: the build bounds the entry at
/// [`fluid_core::BROWSER_ENTRY_MAX_SOURCE_BYTES`] and this adds wrapper headroom, so a corrupt or
/// oversized file or reply is rejected before it is read in full.
const MAX_ARTIFACT_BYTES: u64 = fluid_core::BROWSER_ENTRY_MAX_SOURCE_BYTES as u64 + 64 * 1024;

/// The delivery route, `GET /v1/browser/artifacts/<policy_digest>`: serves the BYTES to an
/// authenticated tenant owning a Ready deployment that references the descriptor. The bytes exist
/// only on the node that ran the build while the public ingress is round-robin, so it follows the
/// owner-read pattern: serve locally, else proxy from the owning node (HTTP admin first, then the
/// iroh gossip transport with its `dispatch` arm in gossip.rs). Proxied bytes are RE-VERIFIED
/// against the descriptor before serving: a node must never emit bytes under immutable digest
/// headers that do not hash to the descriptor's source digest, whichever peer they came from.
pub fn routes() -> Router<Arc<CloudState>> {
    Router::new().route("/v1/browser/artifacts/:digest", get(get_artifact))
}

/// A tenant's resolved claim to one artifact: the descriptor (identical for
/// every deployment referencing the same content-addressed digest) plus every
/// node known to host a Ready deployment referencing it — the proxy candidate
/// set, in local-then-peer order.
pub struct ArtifactResolution {
    pub descriptor: BrowserArtifact,
    pub hosts: Vec<String>,
}

/// Resolves `policy_digest` to its descriptor under `tenant`'s ownership from the SAME replicated
/// state admission trusts. `None` covers BOTH "no such artifact" and "owned by another tenant" (the
/// handler maps both to 404), so the route never confirms a digest across tenants.
pub fn resolve_for_tenant(
    cloud: &Arc<CloudState>,
    tenant: &str,
    policy_digest: &str,
) -> Option<ArtifactResolution> {
    let mut descriptor: Option<BrowserArtifact> = None;
    let mut hosts: Vec<String> = Vec::new();
    for record in cloud.gw.deployment_records() {
        if record.state != fluid_core::DeployState::Ready
            || crate::admin::record_tenant(&record.tenant) != tenant
        {
            continue;
        }
        for f in &record.manifest.functions {
            let Some(a) = &f.browser_artifact else {
                continue;
            };
            if a.policy_digest == policy_digest {
                if descriptor.is_none() {
                    descriptor = Some(a.clone());
                }
                if !hosts.contains(&cloud.node_name) {
                    hosts.push(cloud.node_name.clone());
                }
            }
        }
    }
    let peers = cloud.peer_deployments.read();
    for (node, deployments) in peers.iter() {
        for info in deployments {
            if info.state != fluid_core::DeployState::Ready
                || crate::admin::record_tenant(&info.tenant) != tenant
            {
                continue;
            }
            for bf in &info.browser_functions {
                if bf.artifact.policy_digest == policy_digest {
                    if descriptor.is_none() {
                        descriptor = Some(bf.artifact.clone());
                    }
                    if !hosts.contains(node) {
                        hosts.push(node.clone());
                    }
                }
            }
        }
    }
    Some(ArtifactResolution {
        descriptor: descriptor?,
        hosts,
    })
}

/// Exact-size + BLAKE3 check every served body must pass, local or proxied.
fn verify_bytes(descriptor: &BrowserArtifact, bytes: &[u8]) -> bool {
    if bytes.len() as u64 != descriptor.source_bytes || bytes.len() as u64 > MAX_ARTIFACT_BYTES {
        return false;
    }
    let Ok(source) = std::str::from_utf8(bytes) else {
        return false;
    };
    fluid_core::browser_source_digest(source) == descriptor.source_digest
}

/// Read the local store copy for `descriptor`, verified. `None` on any
/// miss/mismatch — a corrupted store file is treated as absent so the caller
/// falls through to the owner proxy instead of serving bad bytes.
pub(crate) async fn read_verified(descriptor: &BrowserArtifact) -> Option<Vec<u8>> {
    let (js_path, _) = digest_paths(&descriptor.policy_digest).ok()?;
    let md = tokio::fs::metadata(&js_path).await.ok()?;
    if md.len() > MAX_ARTIFACT_BYTES || md.len() != descriptor.source_bytes {
        return None;
    }
    let bytes = tokio::fs::read(&js_path).await.ok()?;
    verify_bytes(descriptor, &bytes).then_some(bytes)
}

/// The immutable content-addressed response: the digest IS the URL, so the body never changes under
/// it (cache forever), and both digests are echoed so a worker can preflight before its own BLAKE3
/// recompute (verification stays client-side in `BrowserFunctionRuntime.pin`).
fn artifact_response(descriptor: &BrowserArtifact, bytes: Vec<u8>) -> Response {
    let mut resp = (StatusCode::OK, bytes).into_response();
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/javascript; charset=utf-8"),
    );
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    if let Ok(v) = HeaderValue::from_str(&format!("\"{}\"", descriptor.policy_digest)) {
        h.insert(header::ETAG, v);
    }
    if let Ok(v) = HeaderValue::from_str(&descriptor.policy_digest) {
        h.insert("x-hive-policy-digest", v);
    }
    if let Ok(v) = HeaderValue::from_str(&descriptor.source_digest) {
        h.insert("x-hive-source-digest", v);
    }
    resp
}

/// Nodes that may hold the bytes, best-first: every host of a Ready deployment referencing the
/// digest (the build ran there in the fanout flow), then the control-plane leader
/// (coordinator-built or re-homed). Self is excluded: the local store was tried first.
fn owner_candidates(cloud: &Arc<CloudState>, hosts: &[String]) -> Vec<String> {
    let mut out: Vec<String> = hosts
        .iter()
        .filter(|n| *n != &cloud.node_name)
        .cloned()
        .collect();
    if let Some(leader) = cloud.leader_forward_target() {
        if !out.contains(&leader) {
            out.push(leader);
        }
    }
    out
}

/// Fetches raw artifact bytes from a candidate owner, the byte-oriented sibling of
/// `admin::fetch_from_host`: HTTP admin when a URL is known, else the iroh gossip transport.
/// `local=true` tells the receiver to serve local-only, so a proxied read can never re-proxy.
async fn fetch_artifact_from_host(
    cloud: &Arc<CloudState>,
    node: &str,
    digest: &str,
    team: &str,
) -> Option<Vec<u8>> {
    let path = format!("/v1/browser/artifacts/{digest}");
    let admin = cloud.node_admins.read().get(node).cloned();
    if let Some(admin) = admin {
        let mut rb = cloud
            .http
            .get(format!("{admin}{path}?local=true"))
            .header("x-hive-team", team)
            .timeout(std::time::Duration::from_secs(15));
        if crate::auth::enforced() {
            if let Ok(tok) = crate::auth::issue(
                "mesh-internal",
                team,
                "service",
                false,
                crate::auth::MESH_DELEGATION_TOKEN_TTL_SECS,
            ) {
                rb = rb.bearer_auth(tok);
            }
        }
        if let Ok(resp) = rb.send().await {
            if resp.status().is_success() {
                if let Ok(bytes) = resp.bytes().await {
                    return Some(bytes.to_vec());
                }
            }
        }
    }
    let target = cloud
        .registry
        .nodes()
        .into_iter()
        .find(|n| n.name == node)
        .and_then(|n| Some((n.peer_id?, n.iroh_addr?)));
    if let Some((id, addr)) = target {
        let p = format!("{path}?{}", crate::admin::mesh_team_qs(team));
        return crate::gossip::request_to(cloud, &id, &addr, hive_p2p::GOSSIP_GET, &p, &[], 20)
            .await;
    }
    None
}

async fn get_artifact(
    State(cloud): State<Arc<CloudState>>,
    claims: Option<axum::Extension<crate::auth::Claims>>,
    AxumPath(digest): AxumPath<String>,
    Query(q): Query<crate::admin::LocalQ>,
) -> Result<Response, (StatusCode, String)> {
    let claims = claims.map(|c| c.0).ok_or((
        StatusCode::UNAUTHORIZED,
        "a verified platform session is required".to_string(),
    ))?;
    if !hive_browser_proto::valid_blake3_digest(&digest) {
        return Err((
            StatusCode::BAD_REQUEST,
            "artifact digest must be 64 lowercase hexadecimal characters".into(),
        ));
    }
    let tenant = crate::admin::norm(&claims.tenant).to_string();
    let Some(resolution) = resolve_for_tenant(&cloud, &tenant, &digest) else {
        return Err((StatusCode::NOT_FOUND, "browser artifact not found".into()));
    };
    if let Some(bytes) = read_verified(&resolution.descriptor).await {
        return Ok(artifact_response(&resolution.descriptor, bytes));
    }
    if q.local == Some(true) {
        return Err((
            StatusCode::NOT_FOUND,
            "browser artifact is not persisted on this node".into(),
        ));
    }
    for node in owner_candidates(&cloud, &resolution.hosts) {
        let Some(bytes) = fetch_artifact_from_host(&cloud, &node, &digest, &tenant).await else {
            continue;
        };
        if bytes.is_empty() {
            continue;
        }
        if verify_bytes(&resolution.descriptor, &bytes) {
            return Ok(artifact_response(&resolution.descriptor, bytes));
        }
        tracing::warn!(
            node = %node,
            digest = %digest,
            "browser artifact bytes from candidate owner failed descriptor verification; trying next"
        );
    }
    Err((
        StatusCode::NOT_FOUND,
        "browser artifact bytes are unavailable on every owning node".into(),
    ))
}

/// Serving half of the gossip `/v1/browser/artifacts/` dispatch arm: the owner re-runs the SAME
/// tenant-ownership gate against its own state. An empty body means not servable (the dispatch
/// layer has no status channel and artifact bodies are never empty), so the caller tries the next
/// candidate.
pub async fn mesh_get(cloud: &Arc<CloudState>, tenant: &str, digest: &str) -> Vec<u8> {
    if !hive_browser_proto::valid_blake3_digest(digest) {
        return Vec::new();
    }
    let tenant = crate::admin::norm(tenant).to_string();
    let Some(resolution) = resolve_for_tenant(cloud, &tenant, digest) else {
        return Vec::new();
    };
    read_verified(&resolution.descriptor)
        .await
        .unwrap_or_default()
}
