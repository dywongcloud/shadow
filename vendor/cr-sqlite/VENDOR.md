# Vendored: superfly/cr-sqlite

Flat vendor (no live git submodule — see below), matching this repo's
`vendor/guardian-db` precedent: full source, own toolchain, excluded from the
main Cargo workspace (`Cargo.toml`'s `exclude`).

## Provenance

- Upstream: https://github.com/superfly/cr-sqlite
- Vendored commit: `ec0d669daa9a051d4c6f4a4d9c653eac40e7a437` (2026-07-14,
  "Merge pull request #25 from superfly/gorbak/new-readme")
- Version: `crsql_version()` reports `170000` (v0.17.0)
- `core/rs/sqlite-rs-embedded` was itself a git submodule pointed at
  `git@github.com:vlcn-io/sqlite-rs-embedded.git` (SSH-only URL — fails to
  clone without a configured SSH key, which is exactly what broke a fresh
  `git submodule update` here). Flattened into plain vendored files instead of
  kept as a live submodule; `.gitmodules` removed. Vendored commit for that
  piece: `aba562843b26642e2242f8e3388b83a1a2625031`.

## Why this fork and not vlcn-io/cr-sqlite

See PRD row `bn-impl-sqlite-automerge`'s resolved-decision text (`.gm/prd.yml`)
for the full live-verified rationale: vlcn-io/cr-sqlite is dormant (last push
2024-10-25); superfly/cr-sqlite is the actively-maintained fork used in
production as Fly.io's Corrosion replication engine (pushed 2026-07-26 as of
vendoring). It introduces breaking clock-table/`ts`-column changes vs upstream
v0.15.0 — anything speaking cr-sqlite's wire format (a browser wasm build
included) must vendor this SAME commit/version, not a mix.

## Toolchain

Building `core/rs/{bundle,bundle_static,core,fractindex-core}` still requires
the pinned `nightly-2023-10-05` toolchain (`core/rs/*/rust-toolchain.toml`,
inherited from upstream — `#![feature(lang_items)]` in `bundle`'s `cdylib`
entry point is not on stable). `rustup` auto-installs it on first build
(verified: ~10s once cached). This does NOT affect the rest of this
workspace's stable toolchain — see `exclude` in the root `Cargo.toml`.

## Building the loadable extension (native, local dev)

```
cd vendor/cr-sqlite && ./build.sh
```

Produces `vendor/cr-sqlite/core/dist/crsqlite.{dylib,so,dll}` (platform-
dependent extension suffix, `core/Makefile`'s `LOADABLE_EXTENSION`). Loaded at
RUNTIME by `rusqlite`'s `Connection::load_extension` — this is a build-time
artifact only, never linked into any workspace crate's Cargo dependency graph,
so there is no toolchain conflict with the rest of the workspace.

Verified working 2026-08-01 against a real in-memory SQLite connection via
`rusqlite` (`load_extension` + `crsql_as_crr` + inserts/updates + a real
`crsql_changes` read showing the tracked column history) — see
`crates/hive-crsql/src/lib.rs`'s doc comment for the exact witness.

## NOT yet done

- Fleet build across both glibc groups (2.38 / 2.39) on real fleet hosts —
  blocked on real fleet SSH/build access (`bn-impl-fleet-crr-peer`).
- The wasm32-unknown-emscripten browser build (`bn-impl-sqlite-automerge`) —
  this vendor is the shared core for BOTH sides, but the wasm packaging
  (wa-sqlite-style Emscripten harness) is separate, unstarted work.
