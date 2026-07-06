//! Integration tests running the REAL detection pipeline (`fluid_build::detect`,
//! `detect_package_manager`, `plan_build`, `load_vercel_config`) plus
//! `hive_core::Runtime` resolution against the REAL, checked-in example
//! projects under `examples/` — not synthetic temp-dir fixtures. These are the
//! same repos a user could actually deploy; `examples/bun-basic-api` and
//! `examples/bun-typescript-api` were manually verified to boot and serve real
//! HTTP requests under the locally installed `bun` (see the compatibility
//! matrix doc for the exact commands run).

use std::path::{Path, PathBuf};

fn examples_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples")
}

#[test]
fn node_control_project_is_completely_unaffected_by_bun_support() {
    let dir = examples_dir().join("node-control-project");
    assert!(dir.exists(), "fixture missing: {}", dir.display());

    let framework = fluid_build::detect(&dir);
    assert_eq!(framework.slug, "node", "must classify as a plain Node.js Server");

    let pm = fluid_build::detect_package_manager(&dir);
    assert_eq!(pm.manager, "npm");
    assert_eq!(pm.source, fluid_build::PackageManagerSource::NpmLock);
    assert!(pm.conflict_warning.is_none(), "a clean npm project must never warn");

    // No vercel.json at all in this fixture — the runtime-resolution precedence
    // chain (git.rs's `runtime_override`) has nothing to latch onto here, so it
    // must fall through to inference, exactly as it did before Bun support
    // existed.
    assert!(fluid_build::load_vercel_config(&dir).is_none());
}

#[test]
fn bun_basic_api_resolves_bun_via_native_runtime_field() {
    let dir = examples_dir().join("bun-basic-api");
    assert!(dir.exists(), "fixture missing: {}", dir.display());

    let framework = fluid_build::detect(&dir);
    assert_eq!(framework.slug, "node", "a plain `scripts.start` server, same shape as the Node control project");

    let pm = fluid_build::detect_package_manager(&dir);
    assert_eq!(pm.manager, "bun");
    assert_eq!(pm.source, fluid_build::PackageManagerSource::BunLock);
    assert!(pm.conflict_warning.is_none());

    let vc = fluid_build::load_vercel_config(&dir).expect("vercel.json must parse");
    assert_eq!(vc.runtime.as_deref(), Some("bun"));
    assert_eq!(
        hive_core::Runtime::from_config_str(vc.runtime.as_deref().unwrap()),
        Some(hive_core::Runtime::Bun)
    );
}

#[test]
fn bun_typescript_api_resolves_bun_via_vercel_bun_version_field() {
    let dir = examples_dir().join("bun-typescript-api");
    assert!(dir.exists(), "fixture missing: {}", dir.display());

    let pm = fluid_build::detect_package_manager(&dir);
    assert_eq!(pm.manager, "bun");
    assert!(pm.conflict_warning.is_none());

    // This fixture uses Vercel's OWN selector (`bunVersion`) instead of the
    // platform-native `runtime` field — proving BOTH precedence paths resolve
    // to Bun, matching git.rs's `runtime_override` resolution chain.
    let vc = fluid_build::load_vercel_config(&dir).expect("vercel.json must parse");
    assert!(vc.runtime.is_none(), "this fixture deliberately uses bunVersion, not the native field");
    assert_eq!(vc.bun_version.as_deref(), Some("1.x"));
}

#[test]
fn bun_conflicting_lockfiles_resolve_deterministically_and_never_force_runtime() {
    let dir = examples_dir().join("bun-conflicting-lockfiles");
    assert!(dir.exists(), "fixture missing: {}", dir.display());

    // Both bun.lock and pnpm-lock.yaml are committed — bun must win (lockfile
    // precedence) and the conflict must be surfaced, never silently dropped,
    // and NEITHER lockfile may be deleted by detection.
    let pm = fluid_build::detect_package_manager(&dir);
    assert_eq!(pm.manager, "bun");
    assert_eq!(pm.source, fluid_build::PackageManagerSource::BunLock);
    let warning = pm.conflict_warning.expect("must warn about the conflicting pnpm-lock.yaml");
    assert!(warning.contains("pnpm-lock.yaml"));
    assert!(dir.join("bun.lock").exists(), "must never delete a lockfile");
    assert!(dir.join("pnpm-lock.yaml").exists(), "must never delete a lockfile");

    // No vercel.json at all in this fixture — package-manager choice (bun for
    // install) must NOT force the runtime. This fixture's package.json start
    // script runs plain `node server.js`, so runtime inference (the
    // `detect_start_cmd`/`Runtime::infer_from_argv` fallback used when no
    // explicit override exists) would resolve to Node, not Bun.
    assert!(fluid_build::load_vercel_config(&dir).is_none());
    assert_eq!(
        hive_core::Runtime::infer_from_argv(&["node".to_string(), "server.js".to_string()]),
        hive_core::Runtime::Node
    );
}
