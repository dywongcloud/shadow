//! Detection for Vercel-shaped Rust function repositories.
//!
//! Vercel's Rust runtime compiles each `[[bin]]` in the root `Cargo.toml` whose `path` points
//! into `api/` (for example `hello.rs` at `api/hello.rs` -> `[[bin]] name = "hello"`). This is
//! the ONE shape the platform's native Rust lane admits: a plain `$PORT` HTTP/1 server binary per
//! function. Anything else (a library crate, a workspace, a lone `src/main.rs`) is not guessed at —
//! use `fluid.json` with an explicit `start_cmd` instead.
//!
//! A tenant string never becomes a host path: names and source paths are validated against a
//! closed ASCII grammar, `..` and absolute paths are refused, and each source must be a regular
//! (non-symlink) file under the checkout. The binary name is derived from the validated `[[bin]]`
//! name, never from a caller-supplied path.

use crate::{BuildContractError, BuildContractErrorCode};
use std::path::{Component, Path};

pub const RUST_PRESET_SLUG: &str = "rust";
// Clean argv: the sealed repository-build contract splits this into fixed argv, so it carries no
// shell quoting. The `[patch.crates-io]` lives in a platform-owned .cargo/config.toml instead.
// `--offline` is the security gate: a tenant build.rs never reaches the network. `--locked` is
// deliberately NOT used for tenant repos: the platform patch changes resolution, so a committed
// Cargo.lock naming crates.io vercel_runtime cannot stay locked.
pub const RUST_BUILD_COMMAND: &str = "cargo build --release --offline --bins";
pub const RUST_OUTPUT_DIR: &str = "target/release";
pub const MAX_CARGO_TOML_BYTES: u64 = 1024 * 1024;
pub const MAX_RUST_FUNCTIONS: usize = 64;
pub const MAX_NAME_BYTES: usize = 64;
pub const MAX_PATH_BYTES: usize = 256;

/// Bytes of the first-party shim, embedded in `hive-cloud` so a node holding only the binary can
/// materialize a `[patch.crates-io]` target into a tenant checkout. The `[workspace]` table is
/// appended at materialization time: it is REQUIRED for a standalone patch target (otherwise
/// cargo thinks it belongs to the tenant workspace) and FORBIDDEN inside our own workspace member.
pub const SHIM_CARGO_TOML: &str = include_str!("../../vercel-runtime/Cargo.toml");
pub const SHIM_LIB_RS: &str = include_str!("../../vercel-runtime/src/lib.rs");

/// Directory name inside the checkout holding the materialized shim. Platform-templated, never
/// derived from a tenant string.
pub const SHIM_DIR: &str = ".hive-rust-runtime/vercel-runtime";

/// Platform-owned cargo config pointing `vercel_runtime` at the materialized shim. Relative path
/// resolves against the checkout root (cargo config paths are relative to the config parent's
/// parent), so no absolute host path is embedded and no tenant string reaches a host path.
pub const SHIM_CONFIG: &str = "[patch.crates-io]\nvercel_runtime = { path = \".hive-rust-runtime/vercel-runtime\" }\n";

/// Path of the reviewed, seeded offline crate set inside the isolated builder image. Platform-
/// templated and absolute: never derived from a tenant string, never a host path on the host lane.
pub const RUST_VENDOR_DIR: &str = "/opt/hive-rust/vendor";

/// Config for the ISOLATED (gVisor BuildExecutor) lane. The image is built --network=none, so the
/// reviewed base must already carry a vendored crate set; this config makes cargo resolve against
/// it with NO network and NO mutable registry. The shim stays a [patch], which cargo applies on top
/// of a replaced source. A tenant crate outside the seeded set fails the build loudly. cargo reads
/// this from the checkout's .cargo/config.toml even under the executor's `env -i` wrapper, so no
/// CARGO_HOME needs to survive env_clear(). Witness: offline build + HTTP serve under `env -i`.
pub const SHIM_CONFIG_ISOLATED: &str = "[patch.crates-io]\nvercel_runtime = { path = \".hive-rust-runtime/vercel-runtime\" }\n\n[source.crates-io]\nreplace-with = \"vendored-sources\"\n\n[source.vendored-sources]\ndirectory = \"/opt/hive-rust/vendor\"\n\n[net]\noffline = true\n";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RustFunction {
    /// Validated `[[bin]]` name; also the `target/release/<name>` binary and the route label.
    pub name: String,
    /// Checkout-relative source path, always under `api/` and ending `.rs`.
    pub source: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RustApp {
    pub functions: Vec<RustFunction>,
}

impl RustApp {
    /// Route for one Vercel Rust function: `api/hello.rs` answers `/api/hello`.
    pub fn route(name: &str) -> String {
        format!("/api/{name}")
    }

    /// Checkout-relative executable. The backend resolves this inside its validated workdir;
    /// a relative path with `/` is never looked up on the host PATH.
    pub fn binary(name: &str) -> String {
        format!("{RUST_OUTPUT_DIR}/{name}")
    }
}

fn plain_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_NAME_BYTES
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        && !value.starts_with('-')
}

fn plain_path(value: &str) -> bool {
    if value.is_empty()
        || value.len() > MAX_PATH_BYTES
        || value.starts_with('/')
        || value.contains('\\')
        || !value.ends_with(".rs")
    {
        return false;
    }
    let path = Path::new(value);
    let mut components = path.components();
    let Some(Component::Normal(first)) = components.next() else {
        return false;
    };
    if first != "api" {
        return false;
    }
    let mut count = 1usize;
    for component in components {
        match component {
            Component::Normal(part) => {
                let part = part.to_string_lossy();
                if part.is_empty()
                    || part == "."
                    || part == ".."
                    || !part
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
                {
                    return false;
                }
                count += 1;
            }
            _ => return false,
        }
    }
    count >= 2
}

/// Read and validate the root `Cargo.toml`. Returns `Ok(None)` when this is not a
/// Vercel-shaped Rust app; `Err` when the manifest IS Rust-shaped but unsafe or malformed,
/// so a build fails loudly instead of silently shipping source as a static site.
pub fn parse(repo: &Path) -> anyhow::Result<Option<RustApp>> {
    let path = repo.join("Cargo.toml");
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => anyhow::bail!("could not inspect Cargo.toml: {e}"),
    };
    anyhow::ensure!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "Cargo.toml must be a regular file, not a symlink or directory"
    );
    anyhow::ensure!(
        metadata.len() <= MAX_CARGO_TOML_BYTES,
        "Cargo.toml is {} bytes; limit is {MAX_CARGO_TOML_BYTES}",
        metadata.len()
    );
    let text = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("could not read Cargo.toml: {e}"))?;
    let value: toml::Value = toml::from_str(&text)
        .map_err(|e| anyhow::anyhow!("invalid repository Cargo.toml: {e}"))?;
    let Some(bins) = value.get("bin").and_then(toml::Value::as_array) else {
        return Ok(None);
    };
    let mut functions: Vec<RustFunction> = Vec::new();
    for bin in bins {
        let table = bin
            .as_table()
            .ok_or_else(|| anyhow::anyhow!("Cargo.toml [[bin]] must be a table"))?;
        let name = table
            .get("name")
            .and_then(toml::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("Cargo.toml [[bin]] needs a string name"))?;
        let source = table
            .get("path")
            .and_then(toml::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("Cargo.toml [[bin]] needs a string path"))?;
        anyhow::ensure!(
            plain_name(name),
            "Cargo.toml [[bin]] name {name:?} is not a plain ASCII identifier"
        );
        anyhow::ensure!(
            plain_path(source),
            "Cargo.toml [[bin]] path {source:?} must be a relative api/*.rs source"
        );
        anyhow::ensure!(
            functions.iter().all(|f| f.name != name && f.source != source),
            "Cargo.toml declares duplicate Rust function names or sources"
        );
        anyhow::ensure!(
            functions.len() < MAX_RUST_FUNCTIONS,
            "Cargo.toml declares more than {MAX_RUST_FUNCTIONS} Rust functions"
        );
        let source_path = repo.join(source);
        let source_metadata = std::fs::symlink_metadata(&source_path)
            .map_err(|e| anyhow::anyhow!("Rust source {source} is missing: {e}"))?;
        anyhow::ensure!(
            source_metadata.file_type().is_file() && !source_metadata.file_type().is_symlink(),
            "Rust source {source} must be a regular file, not a symlink"
        );
        functions.push(RustFunction {
            name: name.to_string(),
            source: source.to_string(),
        });
    }
    if functions.is_empty() {
        return Ok(None);
    }
    Ok(Some(RustApp { functions }))
}

/// Typed refusal for the production build contract: a malformed Rust manifest is an invalid
/// framework, not a static site.
pub fn detect_checked(repo: &Path) -> Result<bool, BuildContractError> {
    parse(repo).map(|app| app.is_some()).map_err(|error| {
        BuildContractError::new(
            BuildContractErrorCode::InvalidMetadata,
            "detect Rust framework",
            error.to_string(),
        )
    })
}

/// Re-check the built binaries after `cargo build`: a missing direct entry must fail the BUILD,
/// never the launch (the same discipline as `preflight_direct_entries` for Node/Bun).
pub fn verify_binaries(repo: &Path, app: &RustApp) -> anyhow::Result<()> {
    for function in &app.functions {
        let relative = RustApp::binary(&function.name);
        let path = repo.join(&relative);
        let metadata = std::fs::symlink_metadata(&path).map_err(|e| {
            anyhow::anyhow!(
                "Rust build produced no {relative} for [[bin]] {:?}: {e}",
                function.name
            )
        })?;
        anyhow::ensure!(
            metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
            "Rust build output {relative} must be a regular file, not a symlink"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            anyhow::ensure!(
                metadata.permissions().mode() & 0o111 != 0,
                "Rust build output {relative} is not executable"
            );
        }
    }
    Ok(())
}
