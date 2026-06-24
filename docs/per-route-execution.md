# Per-route function splitting (Next.js) — status & usage

Vercel splits a Next.js app into one **traced function bundle per route** (`.func`),
choosing the cheapest correct primitive per route (CDN / ISR / Node function /
Edge function). OpenEdge historically serves a Next.js app as a **single
`next start` server** (one `api` function, catch-all route). This document tracks
the incremental move toward per-route execution.

The work is **behind a feature flag and does not change the default serve path.**

## How to enable

Set on the build node (the node that runs the deploy):

```
SHADOW_NEXT_PER_ROUTE=1
```

When set and the project is a Next.js app, the build, after `next build`, reads
the `.next` manifests + file traces, classifies every route, and writes a
per-route build manifest to `shadow-per-route.json` in the build output. The
build log prints a summary, e.g.:

```
per-route: classified 26 route(s) — 22 per-route-eligible (Node), 4 on next-start fallback (static/edge/middleware).
per-route: wrote shadow-per-route.json (build manifest). Serving still uses `next start` (fallback).
```

When the flag is **unset (default)**, none of this runs and behavior is byte-for-byte
the existing `next start` path.

## What is read (version-tolerant)

`crates/fluid-build/src/per_route.rs::discover()` reads (each optional — missing
manifests are skipped, so it degrades gracefully across Next versions):

| Manifest | Used for |
|---|---|
| `app-paths-manifest.json` | App Router route → server file |
| `pages-manifest.json` | Pages Router route → server file |
| `routes-manifest.json` | (reserved: redirects/headers/dynamic) |
| `prerender-manifest.json` | SSG/ISR routes + `initialRevalidateSeconds` |
| `middleware-manifest.json` | middleware + edge functions (→ fallback) |
| `required-server-files.json` | base files every Node bundle needs |
| `<server-file>.nft.json` | per-route file trace (the files a bundle includes) |

## Route classification

Each route → one of: `Static`, `Isr`, `ApiNode`, `RouteHandler`, `SsrPage`,
`Middleware`, `EdgeRoute`. The Node kinds (`ApiNode`, `RouteHandler`, `SsrPage`)
are **per-route-eligible**; the rest stay on the fallback. Each `RouteBundle`
records the route, kind, runtime (`nodejs`/`static`/`edge`), entrypoint, traced
files, ISR revalidate, and a `fallback` flag.

## Status table

| Capability | Status | Notes |
|---|---|---|
| Per-route discovery + classification | **Implemented** | `per_route.rs`, unit-tested (App + Pages Router, ISR, edge, middleware) |
| Per-route build manifest (`shadow-per-route.json`) | **Implemented** | written behind the flag; route→kind→runtime→entrypoint→trace→fallback |
| File-trace–based bundle plan (no whole-app copy) | **Implemented** | uses `*.nft.json` + `required-server-files.json`; lists only traced files |
| Physical per-route bundle materialization (copy traced files into `.func`-like dirs) | **Partial** | manifest + file list produced; on-disk bundle dirs not yet emitted |
| Runtime dispatch to per-route Node bundles | **Not implemented (designed)** | flag scaffolded; serve path unchanged; dispatcher is the next step |
| Static / ISR / image-optimization | **Unchanged (works)** | served by existing CDN + `next start` paths; not regressed |
| Edge runtime (`runtime="edge"`) + Middleware | **Fallback only** | classified + deferred to `next start`; **true V8 isolate runtime is NOT implemented** |
| Default `next start` serve path | **Unchanged** | flag-off = identical behavior |

### Fallback behavior
- Edge routes, middleware, and any route without a usable Node entrypoint/trace are
  marked `fallback` and served by `next start`.
- Discovery is best-effort: a missing/!version-matching manifest is skipped, never fatal.
- The serve path never depends on `shadow-per-route.json`; a broken/absent manifest
  cannot take down a deployment.

### Known remaining gaps
1. **Runtime dispatcher** — execute eligible routes from their own bundles (per-route
   cells/processes) with `next start` fallback on any failure.
2. **Physical bundle emission** — copy each route's traced files into an isolated
   bundle dir.
3. **True Edge V8 isolate runtime** for `runtime="edge"` — out of scope; edge stays on fallback.

## Validation commands

```
cargo test -p fluid-build per_route          # discovery/classification unit tests
cargo test -p fluid-build -p hive-cloud --features hive-cloud/zkauth   # full suites (no regression)
# Live (flag on): deploy a Next.js app with SHADOW_NEXT_PER_ROUTE=1 and check the
# build log for the "per-route: classified …" line + shadow-per-route.json.
```
