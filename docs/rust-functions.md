# Native Rust functions (Vercel Rust runtime compatibility)

## What works

A repository with a root `Cargo.toml` declaring Vercel-shaped `[[bin]]` entries is detected,
built into native binaries with `cargo build --release --offline --bins`, and served as one
plain-HTTP `$PORT` process per function. An **unmodified** Vercel Rust handler compiles against
the first-party `crates/vercel-runtime` shim:

```toml
[package]
name = "hello-world"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "hello"
path = "api/hello.rs"

[dependencies]
tokio = { version = "1", features = ["full"] }
serde_json = "1"
vercel_runtime = "2"
```

```rust
use serde_json::{Value, json};
use vercel_runtime::{Error, Request, run, service_fn};

#[tokio::main]
async fn main() -> Result<(), Error> { run(service_fn(handler)).await }

async fn handler(_req: Request) -> Result<Value, Error> {
    Ok(json!({ "message": "Hello, world!" }))
}
```

Routes: `/api/hello` -> the `hello` binary, and `/` -> the first declared binary. Every
function gets `env_clear()` plus a backend-chosen `$PORT`; the shim reads `PORT` first, then
`VERCEL_DEV_PORT` (default 3000), so the same source runs on Vercel and here unchanged.

`GET /_vercel/ping` answers `200 OK` on the function process's own `$PORT` listener (Vercel's
internal readiness probe, and the CI witness). It is NOT a public-gateway route: the gateway
reserves the `/_vercel/*` namespace for its analytics/insights sinks and returns 204 for unknown
paths in it. Platform readiness is a TCP connect, not an HTTP ping.

## The honest boundary

This is **source compatibility with the public `vercel_runtime` 2.4 handler contract**, not a
reimplementation of Vercel's runtime. Deliberately NOT implemented:

- **Vercel IPC** (`VERCEL_IPC_PATH` start/end/log frames), invocation-id headers and the
  `waitUntil` IPC drain. Here `AppState::wait_until` spawns a tokio task; the process is
  supervised by the platform health loop, not by Vercel's per-request drain.
- **`.func` / Build Output v3 provisioning.** Build Output v3 still accepts only the Node
  runtimes (`nodejs20.x|22.x|24.x`); Rust takes the dedicated lane, not a widened allowlist.
- **axum 0.8 / axum-streams / actix-web adapters.** The `axum` feature exists so a Vercel
  `Cargo.toml` declaring it resolves offline; its `VercelLayer`/stream helper is a generic Tower
  layer, not a Router drop-in. The `actix` feature fails loudly at compile time. Templates that
  import `vercel_runtime::axum` need the core `service_fn`/`run` contract instead.
- **Arbitrary crates.io dependencies.** `--offline` is a security gate: a tenant `build.rs`
  never reaches the network. Only crates already in the node's cargo registry cache resolve.
  Provision the builder image's offline registry for the dependencies you need.
- **Windows / non-Unix targets.** Native binaries are validated as regular executable files.

## Security

- The build runs ONE repository command through `require_build_session`: `env_clear()` on the
  host lane, an isolated BuildExecutor v1 session on Firecracker. It never runs `begin_host`
  for a Rust build on a backend that has an executor.
- `cargo` AND `rustc` are **probed** on the filesystem the build actually execs against
  (`resources::BUILD_TOOLCHAINS` bit `1 << 7`, the last free exit-code bit). A missing toolchain
  fails the build with `BUILD_TOOLCHAIN_MISSING`; placement gates `cargo` exactly, with no npm
  substitution. `None` (an unreported node) is NOT capable.
- `[[bin]]` names and `api/*.rs` paths are validated against a closed ASCII grammar; `..`,
  absolute paths, symlinks and control characters are refused. A name never becomes a host path.
- The platform writes `.cargo/config.toml` and refuses a checkout that already ships one, so a
  tenant cannot inject `[patch]`, `rustflags` or a `rustc-wrapper`. `RUSTFLAGS`,
  `CARGO_ENCODED_RUSTFLAGS`, `RUSTC_WRAPPER`, `RUSTC` and `CARGO_TARGET_DIR` are unset before
  the build. `--locked` is intentionally absent: the shim patch changes resolution, so a
  committed `Cargo.lock` naming crates.io `vercel_runtime` cannot stay locked.
- A missing `target/release/<name>` after the build fails the BUILD, never the launch.
- The shim source is embedded in `hive-cloud` via `include_str!`, so a node holding only the
  binary can materialize it; no source tree is needed at deploy time.

## Deployment prerequisite (operator one-way door)

The isolated builder image `ansible/roles/build_executor/templates/Containerfile.j2` currently
asserts `curl node npm corepack bun python3 go buildah` but NOT `cargo`/`rustc`. Until an
authorized roll adds a pinned Rust toolchain and a seeded offline crate registry, Rust source
builds succeed on host-exec lanes (mock/litebox) and fail closed with
`BUILD_TOOLCHAIN_MISSING` on isolated-executor nodes. **This change is NOT deployed or rolled.**
A `git push` auto-deploys through the webhook and `git::spawn_git_poll_reconcile`, which is the
operator's one-way door.

## Zero-build alternative

For a prebuilt native binary, `fluid.json` already accepts an arbitrary `start_cmd` with
`"runtime": "rust"` (which resolves to `Runtime::Command`). That lane needs no toolchain on the
node and no repository build command at all.
