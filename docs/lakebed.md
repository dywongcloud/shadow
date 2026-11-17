# Lakebed on shadw

Status: IMPLEMENTED — framework detection, build preset, frontend template tile and a live-witnessed
end-to-end example. NOT implemented: Lakebed's hosted source runtime. See "The honest boundary".

External references: [database](https://docs.lakebed.dev/database/),
[storage](https://docs.lakebed.dev/storage/), [capsule API](https://docs.lakebed.dev/capsule-api/),
[reference](https://docs.lakebed.dev/reference/).

## What works

- **Framework detection.** `crates/fluid-build/src/framework.rs` carries a `lakebed`
  `FrameworkPreset`: `npm install`, `npx lakebed build`, `npx lakebed dev`, output
  `.lakebed/artifacts`, `Primitive::Serverless`. `detect_with_package` recognizes a
  Lakebed repo by `lakebed.json`, by a `server/index.*` + `client/index.*` pair whose
  server entry declares `capsule(` or imports `lakebed/server`, or by a `lakebed`
  dependency. The check sits before the Vite/Node markers so a capsule's client bundle
  cannot misroute it to the static lane; a generic server/client pair with no capsule
  declaration stays `static` (witnessed live). Bounded reads, no symlink follow.
- **Catalog and overrides.** `GET /v1/build/frameworks` serializes
  `fluid_build::PRESETS`; `preset_by_name("lakebed")`/`preset_by_name("Lakebed")`
  and the project-settings `framework_override` path work without further edits.
  Witnessed live: the real node's `/v1/build/frameworks` answers with `lakebed`.
- **Frontend example.** `ui/app/new/page.tsx` `TEMPLATES` has a Lakebed tile pointing
  at `https://github.com/dywongcloud/shadow` with `root_dir: examples/lakebed-todo`.
  It has no icon asset on purpose: `Monogram` falls back to the `Lb` tag, so no
  missing image can appear. The repo is public and the path is committed.
- **One cohesive binding.** `examples/lakebed-todo/server/shadow.mjs` is the ONLY
  module that talks to the platform. It exports `database()` and `storage()` sharing
  one transport, one server-held credential and one validator. Both bind Lakebed's
  API shape to shadw's tenant-scoped Blob store:
  - `ctx.db.<table>`: `get/insert/update/delete`, `withIndex/order/take/collect/first/count/paginate`.
  - `ctx.storage`: `upload/get/list/delete/url`, returning Lakebed's `UploadedObject` fields.
- **A runnable capsule.** `server/index.mjs` default-exports `capsule({schema,
  queries, mutations, endpoints})` written against the documented `lakebed/server`
  exports (provided locally by `server/lakebed.mjs`). `server/serve.mjs` executes it
  on shadw's plain Node lane via `fluid.json`'s `start_cmd`, and `client/index.mjs`
  provides the `lakebed/client`-shaped browser surface. Zero npm dependencies, so a
  `fluid.json` deployment (which ships the checkout as-is, with no install/build)
  runs it directly. `fluid.json` declares `framework: "lakebed"`, `static_dir: "public"`
  (only the HTML is static) and an explicit `/client` → `api` function route: without
  it the browser's `/client/index.mjs` would resolve to Static, miss
  `public/client/index.mjs`, and get the SPA fallback `text/html` for a module script.
  The witness compiles the real `fluid_core::Manifest::from_json` + `resolve` to assert
  `/` → Static and `/api/*` and `/client/*` → `Function("api")`.
- **Live witness.** A real `hive-cloud` binary on isolated ports, a real provisioned
  Blob database, and the real capsule server measured 17 passing assertions:
  route resolution against the real `fluid_core` manifest parser, insert → list →
  count → toggle → delete, upload → exact read-back, delete via the
  sanctioned console path, static client, typed refusals, and no credential in any
  response. No test file was added.

## The honest boundary

- **No Lakebed hosted runtime.** `npx lakebed deploy` publishes `app.json` +
  `clientBundle` executed by Lakebed's "source runtime". shadw does not implement
  that runtime. The `lakebed` preset is detection and build planning, not a claim
  that shadw executes a Lakebed artifact. A plain `npx lakebed build` output is not
  a shadw-servable static tree; the example uses `fluid.json` for that reason.
- **No server-side index pushdown or transactions.** The binding evaluates
  `withIndex` ranges over rows read from blob keys, not in a database engine. Each
  write is one PUT; there is no multi-row atomic commit and no serialized-write
  guarantee. Lakebed v1 promises those; this binding does not. Writes are bounded
  to 1000 rows and every statement is a measured read or write, never an assumed one.
- **No public/unauthenticated objects.** `upload(file, {public: true})` only tags
  the key prefix. shadw has no unauthenticated blob GET, so every `url` requires the
  same tenant bearer. There are no presigned URLs.
- **Delete is a console command.** shadw registers `GET|PUT
  /v1/storage/blob/:bucket/:key` but no `DELETE` REST route. `storage.delete` and
  `ctx.db.<table>.delete` use the existing `DELETE <key>` database-console command
  (`write: true`, a NATIVE_WRITE, not the destructive tier), which needs
  `BLOB_DATABASE_ID`. Without it the binding raises a typed `DELETE_UNAVAILABLE`
  refusal instead of reporting a fabricated success. Console writes are node-local.
- **PUT 200 is not durability.** `BlobStore::put` swallows I/O errors, so
  `upload` reads the object back and compares byte length before returning. A
  missing or short read-back is `WRITE_NOT_DURABLE`/`WRITE_SIZE_MISMATCH`.
- **Credential discipline.** The browser never holds a platform credential.
  `HIVE_API_TOKEN` (a `hive_` API key / JWT) is server-side only; a `blob_rw_`
  object token is NOT a platform principal and is deliberately not accepted.
  Bucket, key and table names are validated and `encodeURIComponent`-ed into a
  URL parsed from a trusted base — a caller-controlled string never becomes a host
  path or a URL fragment. No secret value is printed, logged or returned.

## Running the witness locally

```sh
# From the repo root, after `cargo build -p hive-cloud --bin hive-cloud`:
bash scripts/lakebed-witness.sh   # live node + real capsule, 17 assertions
```

The script mints a throwaway API key on a throwaway node with a throwaway
`HIVE_DATA`, reads the Blob bucket from `/v1/databases/:id/credentials`, starts
`server/serve.mjs` with `SHADOW_API_BASE`, `BLOB_BUCKET`, `HIVE_API_TOKEN` and
`BLOB_DATABASE_ID`, then exercises every route over HTTP. It is a live-execution
witness committed at `scripts/lakebed-witness.sh`, not a test file; the project
forbids test files.
