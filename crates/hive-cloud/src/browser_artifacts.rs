//! Browser-executable function artifacts: build-time bundling, the
//! content-addressed on-disk store, and its guarded GC
//! (browser-function-artifact-build-contract).
//!
//! A function becomes browser-eligible ONLY by opting in via fluid.json
//! `functions[].browser` ([`fluid_core::BrowserPolicy`]) and surviving
//! [`bundle`]'s rejection pass. The emitted artifact is ONE deterministic,
//! self-contained source string — an `async function (request, ops)`
//! expression in exactly the shape
//! `crates/hive-browser/www/pkg/function-worker.js` evaluates — carrying no
//! env, no imports, and no Node/Bun/Deno runtime surface (the scan below is
//! what makes "unsupported native/container/runtime dependency" a LOUD build
//! failure instead of a silent browser-side misexecution).
//!
//! Store layout under `persist::data_dir()/browser-artifacts/`:
//!
//! * `<policy_digest>.js`   — the emitted source bytes, content-addressed by
//!   the canonical-policy digest (the same 64-hex value the whole wire
//!   protocol routes on: `encode_invoke`'s code digest,
//!   `BrowserFunctionRuntime.pin`'s return, the admission's `digest`).
//! * `<policy_digest>.json` — sidecar: the [`fluid_core::BrowserArtifact`]
//!   descriptor plus the owning deployment ids. Ownership is bookkeeping for
//!   operators; the GC keep-set is computed from LIVE deployment records
//!   (the authoritative state), never from these sidecars.
//!
//! The bytes are node-local derived data and deliberately do NOT ride
//! `PlatformSnapshot` or store_sync (the `dns_geo.json` precedent): only
//! descriptor metadata replicates, inside the deployment manifest.
//!
//! GC ([`gc`] / [`spawn_gc_loop`]) copies `gc_rootfs_images`' blast-radius
//! guards: an empty keep-set or a reap set larger than
//! `HIVE_BROWSER_ARTIFACT_GC_MAX_REAP_FRACTION` of the store refuses the
//! whole pass, and only files older than a grace window are candidates, so a
//! just-built artifact whose record hasn't settled yet is never reaped.

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
use fluid_core::{BrowserArtifact, BrowserExecMode, BrowserFunctionRef, FunctionConfig};

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

/// One native/container/runtime dependency the source scan found, with the
/// human-facing reason it cannot run inside the browser substrate.
struct ForbiddenToken {
    /// Word-boundary needle, e.g. "process." — matched only when the
    /// preceding character is not identifier-ish (see `contains_token`).
    needle: &'static str,
    reason: &'static str,
}

/// Runtime-surface tokens denied in EVERY mode. An artifact carries no
/// module system and no host runtime: any of these means the handler depends
/// on a Node/Bun/Deno/native runtime the browser substrate does not have.
const FORBIDDEN_ALWAYS: &[ForbiddenToken] = &[
    ForbiddenToken {
        needle: "require(",
        reason: "CommonJS `require(...)` — artifacts are single-file; dependencies must be inlined by the author",
    },
    ForbiddenToken {
        needle: "import(",
        reason: "dynamic `import(...)` — the artifact has no module loader",
    },
    ForbiddenToken {
        needle: "process.",
        reason: "Node `process.*` (env/argv/exit) — artifacts get NO environment: project env and secrets must never ship to a donor's browser",
    },
    ForbiddenToken {
        needle: "Buffer",
        reason: "Node `Buffer` — use Uint8Array/TextEncoder (both exist in QuickJS)",
    },
    ForbiddenToken {
        needle: "__dirname",
        reason: "Node `__dirname` — no host filesystem exists in the browser substrate",
    },
    ForbiddenToken {
        needle: "__filename",
        reason: "Node `__filename` — no host filesystem exists in the browser substrate",
    },
    ForbiddenToken {
        needle: "Deno.",
        reason: "Deno runtime API — the browser substrate is not Deno",
    },
    ForbiddenToken {
        needle: "Bun.",
        reason: "Bun runtime API (`Bun.serve` etc.) — a browser artifact is a request→response handler, not a server",
    },
];

/// Additional tokens denied in QuickJS mode only. The native-mode Worker has
/// real web globals; the QuickJS guest's ONLY host boundary is numbered ops —
/// a direct fetch would bypass the metered op path entirely.
const FORBIDDEN_QUICKJS: &[ForbiddenToken] = &[ForbiddenToken {
    needle: "fetch(",
    reason: "direct `fetch(...)` — QuickJS has no web globals; network access must go through an allowed host op",
}];

/// `needle` appears in `hay` at a token boundary: the character immediately
/// before the match (if any) is not an identifier character and not `.`/`$`,
/// so `myprocess.env`, `x.Buffer`, or `this.fetch(` never false-positive,
/// while `(require("fs"))`, `new Buffer(...)`, and `process.env` do.
fn contains_token(hay: &str, needle: &str) -> bool {
    let mut start = 0;
    while let Some(at) = hay[start..].find(needle) {
        let at = start + at;
        let preceded_ok = hay[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '$' | '.')));
        if preceded_ok {
            return true;
        }
        start = at + needle.len();
    }
    false
}

/// Whether a CommonJS entry assigns a handler the wrapper can resolve —
/// `module.exports = ...`, `module.exports.handler = ...`, or `exports.handler
/// = ...`. Mirrors the wrapper's runtime resolution (`typeof module.exports ===
/// "function" ? module.exports : module.exports.handler`), scanned statically
/// so a server/library/bare-script entry that never touches `module.exports`
/// is filtered before it can ship. A plain substring is enough: any reference
/// to `module.exports` (whole-object or `.handler`) means the file participates
/// in the CommonJS export the wrapper reads; a bare `exports.handler` covers
/// the aliased form the wrapper also accepts.
fn handler_export_present(src: &str) -> bool {
    src.contains("module.exports") || src.contains("exports.handler")
}

/// Every static `import ...` / `export ...` module form. Matched line-wise so
/// the denial message can name the exact line.
fn module_syntax_lines(source: &str) -> Vec<usize> {
    source
        .lines()
        .enumerate()
        .filter_map(|(i, line)| {
            let t = line.trim_start();
            if t.starts_with("import ")
                || t.starts_with("import{")
                || t.starts_with("export ")
                || t.starts_with("export{")
                || t == "import"
            {
                Some(i + 1)
            } else {
                None
            }
        })
        .collect()
}

/// Resolve the entry path safely inside the deployment build dir. The entry
/// is tenant-controlled input, so this is the same class of check as
/// `container_volume_cfg`'s sanitization: an absolute path or a `..`
/// component must never let a fluid.json read (or, worse, have the build
/// bundle) a host file outside the build output.
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
    // `..` components are rejected above, but a SYMLINK inside the build
    // output could still point outside it (a zip can carry one). Resolve and
    // re-verify containment so an entry can never bundle a host file.
    let canon = path.canonicalize().map_err(|e| {
        format!("browser entry {entry:?} cannot be resolved: {e}")
    })?;
    let canon_root = build_dir.canonicalize().map_err(|e| {
        format!("build dir cannot be resolved: {e}")
    })?;
    if !canon.starts_with(&canon_root) {
        return Err(format!(
            "browser entry {entry:?} escapes the deployment root through a symlink"
        ));
    }
    Ok(path)
}

/// Bundle one opted-in function into its deterministic QuickJS-compatible
/// artifact. Every ineligibility is a loud `Err` naming the function and the
/// reason — the caller (the build pipeline) fails the deployment on it.
pub fn bundle(build_dir: &Path, f: &FunctionConfig) -> Result<BundledArtifact, String> {
    let name = || format!("function {:?}", f.name);
    let Some(policy) = f.browser.as_ref() else {
        return Err(format!(
            "{} has no browser policy — bundle() is only for opted-in functions",
            name()
        ));
    };
    // Runtime eligibility: only JS handlers (node/bun-class runtimes) can
    // produce a browser artifact. Containers, prebuilt images, and every
    // non-JS runtime are unsupported by construction.
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
    let entry_path = resolve_entry(build_dir, policy.entry.trim()).map_err(|e| format!("{}: {e}", name()))?;
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
    // Deterministic bytes: identical input source must hash identically
    // regardless of the author's platform, so line endings normalize to LF
    // before wrapping (and before hashing).
    let user = raw.replace("\r\n", "\n").replace('\r', "\n");

    let mut rejections: Vec<String> = Vec::new();
    for line in module_syntax_lines(&user) {
        rejections.push(format!(
            "line {line}: ES module syntax — the artifact is one self-contained source; assign \
             the handler to `module.exports` (or `exports.handler`) instead"
        ));
    }
    for token in FORBIDDEN_ALWAYS {
        if contains_token(&user, token.needle) {
            rejections.push(format!("{:?}: {}", token.needle, token.reason));
        }
    }
    if resolved.mode == BrowserExecMode::Quickjs {
        for token in FORBIDDEN_QUICKJS {
            if contains_token(&user, token.needle) {
                rejections.push(format!("{:?}: {}", token.needle, token.reason));
            }
        }
    }
    // A browser artifact MUST resolve to a request→response handler at runtime:
    // the wrapper below assigns `module.exports`/`exports.handler` and THROWS if
    // it is not a function. Verify that export exists STATICALLY here so a
    // non-handler entry (a long-running server, a library, a bare script) is
    // caught at build — an AUTO-detected candidate is then skipped silently and
    // an EXPLICIT opt-in fails the build loudly, instead of shipping bytes that
    // throw in every donor's browser. This is what makes probing a function's
    // own entry (not just a `.browser.js` convention file) safe: a server's
    // entry never assigns module.exports, so it is filtered here.
    if !handler_export_present(&user) {
        rejections.push(
            "defines no handler export — assign the request→response handler to `module.exports` \
             (or `exports.handler`); an entry that exports none is a server/library, not a browser \
             handler"
                .to_string(),
        );
    }
    if !rejections.is_empty() {
        return Err(format!(
            "{} opted into browser execution but its entry {:?} uses unsupported \
             native/container/runtime dependencies:\n  - {}",
            name(),
            policy.entry,
            rejections.join("\n  - ")
        ));
    }

    // The deterministic single-function source: the user's CommonJS-style
    // file verbatim inside a fixed envelope that (a) resolves the handler,
    // (b) normalizes the request (a JSON string on the QuickJS path, an
    // object on the native path), and (c) normalizes the response into the
    // `{status, headers, body, bodyBase64}` envelope string the gateway's
    // `browser_response` parses. The wrapper is a constant template, so the
    // emitted bytes — and therefore both digests — depend only on the entry
    // source and the policy.
    let source = format!(
        "async function (request, ops) {{\n\
         \x20 \"use strict\";\n\
         \x20 const module = {{ exports: {{}} }};\n\
         \x20 const exports = module.exports;\n\
         \x20 /* ---- begin deployment entry (verbatim, LF-normalized) ---- */\n\
         {user}\n\
         \x20 /* ---- end deployment entry ---- */\n\
         \x20 ;const handler = typeof module.exports === \"function\" ? module.exports : module.exports.handler;\n\
         \x20 if (typeof handler !== \"function\") {{\n\
         \x20   throw new TypeError(\"browser entry must assign its handler to module.exports or exports.handler\");\n\
         \x20 }}\n\
         \x20 const req = typeof request === \"string\" ? JSON.parse(request) : request;\n\
         \x20 const out = await handler(req, ops);\n\
         \x20 if (typeof out === \"string\") return out;\n\
         \x20 if (!out || typeof out !== \"object\") {{\n\
         \x20   throw new TypeError(\"browser handler must return a response object or an envelope string\");\n\
         \x20 }}\n\
         \x20 const status = out.status === undefined ? 200 : Number(out.status);\n\
         \x20 if (!Number.isInteger(status) || status < 100 || status > 599) {{\n\
         \x20   throw new RangeError(\"browser response status must be an integer in 100..599\");\n\
         \x20 }}\n\
         \x20 const headers = out.headers && typeof out.headers === \"object\" ? out.headers : {{}};\n\
         \x20 const envelope = {{ status: status, headers: headers }};\n\
         \x20 if (typeof out.bodyBase64 === \"string\") envelope.bodyBase64 = out.bodyBase64;\n\
         \x20 envelope.body = typeof out.body === \"string\" ? out.body : JSON.stringify(out.body === undefined ? \"\" : out.body);\n\
         \x20 return JSON.stringify(envelope);\n\
         }}"
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

fn digest_paths(digest: &str) -> Result<(PathBuf, PathBuf), String> {
    // The digest is our own BLAKE3 hex, but validate before it ever becomes a
    // path component — a store keyed on a tenant-influenced string is the
    // `hive-vol-` bind-mount footgun all over again.
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
pub async fn persist(source: &str, descriptor: &BrowserArtifact) -> std::io::Result<()> {
    let (js_path, json_path) = digest_paths(&descriptor.policy_digest)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    tokio::fs::create_dir_all(store_dir()).await?;
    // Write-then-rename so a crash mid-write never leaves a truncated file
    // under a valid digest (the bytes must always hash to their own name).
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
        return Ok(()); // already recorded
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

/// Every policy digest referenced by a LIVE deployment record on this node —
/// the GC keep-set. All records count (including Error-state ones): a record
/// leaves the set only when the deployment is actually removed, which is the
/// conservative direction for reaping.
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
/// BLAST-RADIUS GUARDS (the `gc_rootfs_images` precedent): an empty keep-set
/// makes every artifact on disk look orphaned — indistinguishable from a
/// caller/state bug — so the whole pass refuses loudly instead. Likewise a
/// candidate set larger than `HIVE_BROWSER_ARTIFACT_GC_MAX_REAP_FRACTION`
/// (default 0.5) of the store: no legitimate pass reaps most artifacts at
/// once. And only files older than the grace window are candidates at all.
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
    let mut candidates: Vec<(String, u64)> = Vec::new(); // (digest stem, bytes)
    let mut entries = match tokio::fs::read_dir(store_dir()).await {
        Ok(d) => d,
        Err(_) => return (0, 0), // no store, nothing to reap
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
            .unwrap_or(true); // unknown mtime => treat as recent, never reap
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
pub fn spawn_gc_loop(cloud: Arc<CloudState>) {
    let interval_secs = std::env::var("HIVE_BROWSER_ARTIFACT_GC_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_GC_INTERVAL_SECS);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        // tokio intervals fire immediately on the FIRST tick — skip two so the
        // first real pass runs a full interval after boot, when persisted
        // deployments have long been restored into the keep-set (an empty
        // keep-set refuses the pass anyway, but never rely on one guard).
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
            let Some(f) = record.manifest.functions.iter().find(|f| f.name == function) else {
                // Deployment exists but has no such function — today's
                // `deployment_not_ready` outcome, not "ineligible".
                return None;
            };
            return Some(f.browser_artifact.clone().map(|artifact| BrowserFunctionRef {
                name: function.to_string(),
                artifact,
            }));
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

/// EVERY browser-eligible function a tenant owns, across every Ready
/// deployment — the automatic counterpart of [`descriptor_for`]'s one
/// hand-named target (browser-auto-serve-eligible-set).
///
/// Same two replicated sources, in the same order, under the same tenant gate
/// as `descriptor_for`/`resolve_for_tenant`: local gw records first, then the
/// gossiped peer view, with (deployment, function) dedup so a deployment
/// visible in both is counted once. A deployment that is not Ready, or belongs
/// to another tenant, or carries no build-stamped artifact simply is not in the
/// set — absence of capability, never a broader one.
///
/// Deterministically ordered (production, then newest, then id/name) and
/// truncated to `auto_serve_max()`, so every node computing this from the same
/// replicated state produces the same answer and a renewal does not reshuffle
/// a donor's pins for no reason.
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

// ---------------------------------------------------------------------------
// Content-addressed delivery (browser-function-artifact-delivery)
// ---------------------------------------------------------------------------
//
// `GET /v1/browser/artifacts/<policy_digest>` serves the artifact BYTES to an
// authenticated tenant that owns a Ready deployment referencing that exact
// descriptor. The bytes exist only on the node that ran the build (they never
// ride `PlatformSnapshot`, store_sync, or the deliver_build ext4), but the
// public ingress is round-robin across every node — the AGENTS.md "node-local
// state behind round-robin GETs" trap. This module follows the documented
// owner-read pattern exactly: serve locally when possible, otherwise resolve
// the owning (host/build) node and proxy the raw bytes from it — HTTP admin
// first (`fetch_bytes_from_host`'s shape), then the iroh gossip transport
// (`fetch_from_host`'s shape, which needs the `dispatch` arm registered in
// gossip.rs). Proxied bytes are RE-VERIFIED against the descriptor before
// being served: a node must never emit bytes under immutable digest headers
// that do not hash to the descriptor's source digest, no matter which peer
// they came from.
//
// The digest is validated (64 lowercase hex) before it ever becomes a path
// component (`digest_paths`), so no tenant-controlled path fragment can reach
// the filesystem — the `hive-vol-` bind-mount rule applied to a read path.

/// Hard ceiling on an artifact body we will serve or accept from a peer. The
/// build already bounds the entry at [`fluid_core::BROWSER_ENTRY_MAX_SOURCE_BYTES`];
/// this adds headroom for the fixed wrapper envelope so a corrupted/oversized
/// store file or peer reply is rejected before it is ever read in full.
const MAX_ARTIFACT_BYTES: u64 = fluid_core::BROWSER_ENTRY_MAX_SOURCE_BYTES as u64 + 64 * 1024;

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

/// Resolve `policy_digest` to its descriptor under `tenant`'s ownership,
/// consulting the SAME replicated deployment state admission trusts (local gw
/// records first, then the gossiped peer view). `None` covers BOTH "no such
/// artifact" and "owned by another tenant" — the handler maps both to 404 so
/// the route never confirms a digest's existence across tenant boundaries.
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
            let Some(a) = &f.browser_artifact else { continue };
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

/// The immutable content-addressed response. The policy digest IS the URL, so
/// the body can never change under it: cache forever, and echo both digests so
/// a worker can log/preflight before its own BLAKE3 recompute (the actual
/// verification stays client-side in `BrowserFunctionRuntime.pin`).
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

/// Nodes that may hold the bytes, best-first: every host of a Ready deployment
/// referencing the digest (the build ran on the host in the fanout flow), then
/// the control-plane leader (the coordinator-built / re-homed case, and the
/// `build_get` precedent). Self is excluded — the local store was already
/// tried before proxying is considered.
fn owner_candidates(cloud: &Arc<CloudState>, hosts: &[String]) -> Vec<String> {
    let mut out: Vec<String> = hosts
        .iter()
        .filter(|n| *n != &cloud.node_name)
        .cloned()
        .collect();
    if !cloud.is_control_plane_leader() {
        let leader = cloud.control_plane_leader();
        if leader != cloud.node_name && !out.contains(&leader) {
            out.push(leader);
        }
    }
    out
}

/// Fetch raw artifact bytes from a candidate owner node — the byte-oriented
/// sibling of `admin::fetch_from_host`: HTTP admin when we have a URL for the
/// node, else over the iroh gossip transport (NAT'd peers). The `local=true`
/// marker tells the receiving handler to serve local-only, so a proxied read
/// can never re-proxy (the `/v1/functions?local=true` no-loop precedent).
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
            if let Ok(tok) = crate::auth::issue("mesh-internal", team, "service", false, 60) {
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
    // `local=true` marks a node-to-node proxied read: serve local-only so the
    // request can never bounce between peers (every candidate would otherwise
    // re-resolve the same hosts and proxy right back).
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
            continue; // mesh "not servable" sentinel (dispatch has no status channel)
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

/// Serving half of the gossip `/v1/browser/artifacts/` dispatch arm. The
/// owner node re-runs the SAME tenant-ownership gate against its own (locally
/// authoritative, since it hosts the deployment) state before serving — the
/// sandbox `proxy_to_owner` re-check precedent. Empty body = not servable:
/// the dispatch layer has no status channel, and artifact bodies are never
/// empty (the wrapper envelope alone is ~1 KiB), so the caller safely treats
/// empty as "try the next candidate".
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
