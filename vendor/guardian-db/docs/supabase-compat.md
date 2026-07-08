# Supabase-compatible layer for GuardianDB

A Kong-shaped HTTP gateway that lets Supabase client libraries (`supabase-js`,
PostgREST clients, GoTrue clients) talk to a GuardianDB node with no
GuardianDB-specific code. It lives entirely behind the `supabase` Cargo feature
(`supabase = ["sql", "dep:axum", "dep:tower"]`); default builds are unaffected.

Implemented end-to-end: **REST** (PostgREST-compatible), **Auth**
(GoTrue-compatible), **Storage** (storage-api-compatible), **postgres-meta**
(the API Supabase Studio talks to), **Realtime** (Phoenix-protocol
websocket: postgres_changes + broadcast), and **GraphQL**
(pg_graphql-compatible reflection of the `public` schema). The remaining Kong
service (**functions**) returns a typed `501` — never a bare 404 and never
fake success.

---

## 1. Scouted seams (what the layer is built on)

Everything is built on the SQL engine's *public* surface. No file under
`src/sql/**` or `src/relational/**` was modified (this keeps the layer
conflict-free with concurrent work on `src/sql/ext/`).

| Seam | Where | How the gateway uses it |
| --- | --- | --- |
| `Database<S: RelationalStorage>` | `src/sql/engine.rs` | Built once with `Database::new(Arc<S>, name)`; shared via `Arc` in `AppState`. |
| `Session<S>` | `src/sql/engine.rs` | **One session per HTTP request**, `Session::new(db, role)`. The `role` (Postgres role name) drives row-security enforcement; `Session::set_var("request.jwt.claims", ...)` injects the caller's claims for policies. |
| `Session::prepare` + `execute_one(&stmt, &[SqlValue])` | `src/sql/engine.rs` | The injection-safe path: a single parameterised statement with `$1..$n` binds. Used for all REST/Auth data ops. |
| `Session::execute(sql)` | `src/sql/engine.rs` | Multi-statement string (no params). Used only for the `auth` schema bootstrap DDL. |
| `ExecResult::{Rows{fields,rows}, Command{tag}}` | `src/sql/result.rs` | `OutField.name` + `SqlType` drive JSON rendering. |
| `SqlValue` / `SqlType` + `from_text` / `decode_json` | `src/relational/value.rs` | REST coerces request strings/JSON to the **declared column type** read from the catalog. |
| `Catalog` / `Table` / `Column` | `src/relational/catalog.rs` | Loaded via `db.storage().load_catalog()` to look up column types and primary keys (for upsert). |
| `RelError::sqlstate()` | `src/relational/error.rs` | Engine errors are surfaced in PostgREST shape with the real SQLSTATE. |
| `bcrypt` | already in-tree (pgcrypto) | Password hash/verify. |
| `hmac` + `sha2` + `base64` | already in-tree (`sql` feature) | HS256 JWTs implemented from scratch — **no `jsonwebtoken` dependency added**. |
| `MemoryStorage` / `open_sql` | `src/relational/storage.rs`, `src/sql/guardian_storage.rs` | In-memory (dev/tests) and persistent Iroh-replicated backends for the binary. |

Verified engine capabilities the layer relies on: `RETURNING`, `ON CONFLICT DO
UPDATE/NOTHING`, `DEFAULT` in `VALUES`, `ILIKE`, `ORDER BY ... NULLS FIRST/LAST`,
`count(*)`, schema-qualified DDL (`CREATE SCHEMA auth; CREATE TABLE auth.users
(...)`), `uuid`/`timestamptz`/`jsonb`/`boolean` columns.

---

## 2. Architecture

```
HTTP request
  │
  ├─ request_id middleware      (x-request-id: read or generate; echoed on response)
  │
  ├─ apikey middleware          (rest + auth only)
  │     • verify `apikey` (a JWT signed by the project secret) → api_key_role
  │     • verify optional `Authorization: Bearer` → claims (fail closed on bad JWT)
  │     • effective role = bearer.role ?? api_key_role  (PostgREST semantics)
  │     • attach AuthContext{ role, api_key_role, claims, request_id }
  │
  ├─ /rest/v1/*        → rest.rs     → Session(role)          → SQL → PostgREST JSON
  ├─ /graphql/v1       → graphql.rs  → Session(role)          → SQL → GraphQL JSON
  ├─ /auth/v1/*        → auth.rs     → Session(service_role)  → auth.* tables → GoTrue JSON
  ├─ /storage/v1/*     → storage.rs  → Session(role)          → storage.* tables (RLS-governed)
  │     (public/signed downloads sit outside the apikey layer)
  ├─ /pg-meta/*        → pg_meta.rs  → catalog + pg_catalog views (service_role-gated)
  ├─ /platform/pg-meta → alias of /pg-meta
  ├─ /realtime/v1/websocket → realtime.rs → Phoenix ws (apikey via query param)
  └─ /functions → 501 typed
```

Files (all under `src/supabase/`, behind `#[cfg(feature = "supabase")]`):

- `project.rs` — `SupabaseCompatProject`, `ServiceConfig`, `ProjectKeys`, `Secret`.
- `jwt.rs` — HS256 sign/verify + `Claims`.
- `error.rs` — `SupaError` taxonomy + PostgREST/GoTrue error rendering.
- `gateway.rs` — axum `Router`, middleware, `AuthContext`, shared exec helpers.
- `rest.rs` — PostgREST translation and handlers.
- `graphql.rs` — pg_graphql-compatible schema reflection + executor.
- `auth.rs` — GoTrue schema bootstrap and handlers.
- `storage.rs` — storage-api bucket/object handlers over the `storage` schema.
- `pg_meta.rs` — postgres-meta endpoints (what Studio needs).
- `realtime.rs` — Phoenix-protocol websocket (postgres_changes + broadcast).
- `src/bin/guardian-supabase.rs` — the binary.

A single project is served per gateway instance (the "single-project shell"),
configured from CLI flags / a supplied JWT secret.

---

## 3. Implemented routes

### REST (`/rest/v1`)

| Method | Path | Behaviour |
| --- | --- | --- |
| `GET`/`HEAD` | `/rest/v1/{table}` | SELECT with `select=`, filters, `order=`, `limit`/`offset`, `Range`. |
| `POST` | `/rest/v1/{table}` | INSERT (object or array), upsert via `Prefer: resolution=…` + `on_conflict=`. |
| `PATCH` | `/rest/v1/{table}` | UPDATE with filters. |
| `DELETE` | `/rest/v1/{table}` | DELETE with filters. |
| `GET`/`POST` | `/rest/v1/rpc/{fn}` | `SELECT fn(args)` — see RPC note below. |

### Auth (`/auth/v1`)

| Method | Path | Behaviour |
| --- | --- | --- |
| `POST` | `/auth/v1/signup` | bcrypt the password, insert user (auto-confirmed), issue access+refresh tokens. |
| `POST` | `/auth/v1/token?grant_type=password` | verify bcrypt, issue tokens. |
| `POST` | `/auth/v1/token?grant_type=refresh_token` | rotate refresh token, issue new access token. |
| `POST` | `/auth/v1/logout` | revoke the user's refresh tokens. |
| `GET` | `/auth/v1/user` | the user for the `Authorization: Bearer` access token. |
| `PUT` | `/auth/v1/user` | update email / password / metadata. |
| `GET`/`POST` | `/auth/v1/admin/users` | list / create (service_role only). |
| `GET`/`PUT`/`DELETE` | `/auth/v1/admin/users/{id}` | get / update / delete (service_role only). |

### Storage (`/storage/v1`)

| Method | Path | Behaviour |
| --- | --- | --- |
| `POST` | `/bucket` | create bucket (`{id?, name, public?, file_size_limit?, allowed_mime_types?}`). |
| `GET` | `/bucket` / `/bucket/{id}` | list / get buckets. |
| `PUT` | `/bucket/{id}` | update `public` / `file_size_limit` / `allowed_mime_types`. |
| `DELETE` | `/bucket/{id}` | delete (409 unless empty). |
| `POST` | `/bucket/{id}/empty` | delete every object in the bucket. |
| `POST`/`PUT` | `/object/{bucket}/{key}` | upload the **raw request body** (`content-type` header; `x-upsert: true` to replace). |
| `GET` | `/object/{bucket}/{key}` | authed download (RLS governs row visibility). |
| `GET` | `/object/public/{bucket}/{key}` | credential-less download, only if `bucket.public`. |
| `POST` | `/object/sign/{bucket}/{key}` | `{expiresIn}` → `{signedURL}` (HS256 token, project secret). |
| `GET` | `/object/sign/{bucket}/{key}?token=` | verify signature/expiry/object binding, serve bytes. |
| `DELETE` | `/object/{bucket}/{key}` and `/object/{bucket}` (`{prefixes:[…]}`) | delete one / many. |
| `POST` | `/object/move`, `/object/copy` | `{bucketId, sourceKey, destinationKey}`. |
| `POST` | `/object/list/{bucket}` | `{prefix, limit, offset, sortBy:{column,order}, search}`. |

Errors use the storage-api shape `{"statusCode","error","message"}`
(`statusCode` is a string, like Supabase's storage service). Unknown
`/storage/v1` paths return a typed `SUPA_COMPAT_STORAGE_UNSUPPORTED_ROUTE` —
never a bare 404. `multipart/form-data` uploads return a typed
`SUPA_COMPAT_STORAGE_MULTIPART_UNSUPPORTED` `501` (browsers' supabase-js sends
the raw body, which is supported).

**Where the bytes live.** Bucket and object *metadata* live in
`storage.buckets` / `storage.objects`; object *bytes* live in a dedicated
`storage._blobs(object_id uuid PK, content bytea)` table written through
parameterised SQL — so uploads persist and **replicate through the same
document store as every other row**. Honest trade-off: bytes travel through
the SQL layer (base64 inside the stored JSON document), which is fine for this
slice; an iroh-blobs content-addressed path is a later optimisation. Uploads
are capped at 50 MiB (`storage::MAX_UPLOAD_BYTES`), and per-bucket
`file_size_limit` / `allowed_mime_types` (`jsonb` array; stock Supabase uses
`text[]`) are enforced with typed 413/415 errors.

**Authorization rides RLS.** The bootstrap enables row security on
`storage.buckets` and `storage.objects` with **no default policies**:
`service_role` (an RLS-bypass role) has full access; every other role is
default-denied until you add policies. Object reads/writes run as the request
role with its JWT claims injected, so the standard Supabase pattern works
verbatim:

```sql
CREATE POLICY objects_owner_select ON storage.objects FOR SELECT
    TO authenticated USING (owner = auth.uid());
CREATE POLICY objects_owner_insert ON storage.objects FOR INSERT
    TO authenticated WITH CHECK (owner = auth.uid());
```

The `owner` column is set from `auth.uid()` (the bearer token's `sub`) on
upload/copy. Blob bytes are fetched only *after* the caller's role-bound query
proved the object row visible; signed-URL redemption is pre-authorized at
signing time (the signer must be able to see the object under their own role).
Known divergences: `/object/list` returns flat object keys (no synthesized
folder entries), and object/blob writes are two autocommits (metadata first,
bytes second) rather than one transaction.

### postgres-meta (`/pg-meta`, alias `/platform/pg-meta`)

Service-role-gated (Studio uses the service key; any other role gets a typed
403). Response keys mirror `github.com/supabase/postgres-meta`:

| Endpoint | Source |
| --- | --- |
| `GET /schemas`, `/tables`, `/columns`, `/indexes`, `/constraints`, `/policies`, `/views` | the persisted engine catalog (tables embed `columns`, `primary_keys`, and FK `relationships`; `?included_schemas=`/`?excluded_schemas=` filters apply). |
| `GET /types`, `/roles`, `/extensions` | the engine's `pg_type` / `pg_roles` / `pg_available_extensions` catalog views, queried through a `service_role` session (extensions include GuardianDB's `runtime` column: `native` vs `sidecar`). |
| `GET /functions`, `/triggers` | `[]` — the engine has no user-defined functions or triggers; an empty array is the honest answer. |
| `POST /query` | `{"query":"…"}` → runs through a `service_role` session (multi-statement OK, Studio's SQL editor path); rows return as an array of objects, errors as `{"error":{"message","code"}}` with the SQLSTATE. |

Honesty notes: `bytes`/`size`/`live_rows_estimate` on `/tables` are reported
as `0` (the engine keeps no relation statistics); `/roles` returns the engine
owner from `pg_roles` **plus** the three gateway roles (`anon`,
`authenticated`, `service_role`) that the JWT layer actually resolves.

**Pointing Supabase Studio at GuardianDB.** Studio needs two backends: a
postgres-meta URL and the project API. Run the official Studio image with:

```bash
docker run --rm -p 3000:3000 \
  -e STUDIO_PG_META_URL="http://host.docker.internal:54321/pg-meta" \
  -e SUPABASE_URL="http://host.docker.internal:54321" \
  -e SUPABASE_PUBLIC_URL="http://127.0.0.1:54321" \
  -e SUPABASE_ANON_KEY="$ANON_KEY" \
  -e SUPABASE_SERVICE_KEY="$SERVICE_ROLE_KEY" \
  -e AUTH_JWT_SECRET="$JWT_SECRET" \
  supabase/studio
```

Studio's table editor, SQL editor, policies and extensions pages ride the
endpoints above. Pages backed by services this slice does not implement
(functions, logs/analytics) will show their typed errors.

### Realtime (`/realtime/v1/websocket`)

A Phoenix-channel websocket compatible with `@supabase/realtime-js` v2. Both
message encodings are accepted and mirrored: the realtime-js **object** form
`{"topic","event","payload","ref"}` and the Phoenix V2 **array** form
`[join_ref, ref, topic, event, payload]`.

* **Connect:** `ws(s)://…/realtime/v1/websocket?apikey=<key>&vsn=1.0.0`. The
  apikey (or `token`) query parameter is verified **before** the upgrade —
  a bad key is a typed HTTP 401. `access_token` join-payload fields and
  `access_token` events rotate the connection's claims (verified against the
  project secret).
* **`phx_join`** on `realtime:<channel>` topics with
  `config.postgres_changes: [{event, schema, table, filter?}]` — the reply
  echoes each binding with a server-assigned `id` (what realtime-js matches
  on). Filters support exactly `col=eq.value`; anything else is a typed join
  error (`SUPA_COMPAT_REALTIME_UNSUPPORTED_FILTER`).
* **`heartbeat`** (topic `phoenix`), **`phx_leave`**, and **`broadcast`**
  passthrough between subscribers of a topic (`config.broadcast.self` honored;
  broadcasting to an unjoined topic is a typed error). Presence and binary
  frames get typed errors — nothing is silently dropped.
* **postgres_changes delivery:** events come from the engine's local commit
  hook (`Database::subscribe_changes` — emitted after every successful
  autocommit/COMMIT). Payloads carry both the realtime wire keys
  (`type`/`record`/`old_record`/`columns`) and the realtime-js client keys
  (`eventType`/`new`/`old`), plus `schema`, `table`, `commit_timestamp`,
  `errors: null`.

**Authorization — no unauthorized delivery.** Each candidate event is
authorized against the subscriber's own role before delivery: bypass roles
receive everything (`service_role` always; the engine-owner roles unless the
table declares `FORCE ROW LEVEL SECURITY`); tables without RLS are
visible to every role (engine semantics); for INSERT/UPDATE on RLS-enabled
tables the row's **primary key is re-selected through a session bound to the
subscriber's role and claims**, and the event is delivered only if the row is
visible under the caller's policies. Documented constraints (when in doubt,
don't deliver): DELETE events on RLS-enabled tables and rows of PK-less tables
cannot be re-checked, so they are **withheld** from non-bypass roles.
`TRUNCATE` produces no events, and only *local* commits are observed — writes
arriving via Iroh replication from another node do not flow into this node's
realtime stream (a replication-changefeed slice can lift this later).

### GraphQL (`/graphql/v1`)

A pg_graphql-compatible endpoint (`graphql.rs`): `POST /graphql/v1` with
`{"query","variables","operationName"}` (plus `GET ?query=` for GraphiQL;
mutations over GET are rejected). It sits under the apikey layer like
`/rest/v1`; every top-level field compiles to parameterised SQL run through a
session bound to the caller's role with `request.jwt.claims` injected, so
**RLS governs GraphQL exactly like REST** (anon default-deny, `service_role`
bypass — all covered by tests).

**Error contract.** GraphQL-level problems (unknown field, unsupported
feature, SQL errors, `atMost` violations) return
`{"errors":[{"message": …}]}` with HTTP **200** (GraphQL-over-HTTP
convention); execution errors carry `"data": null`. Any field error aborts
the whole operation — no partial silent results. Malformed *HTTP* requests
(bad JSON body, missing `query`) return the usual `SupaError` 4xx shapes, and
`/graphql/v1/<subpath>` returns a typed 404 with a GraphQL-shaped body —
never a bare 404.

**Reflection** (rebuilt per request from the catalog snapshot, so DDL is
picked up immediately). Only schema `public` user tables **with a primary
key** are reflected (pg_graphql's own rule). **Inflection is off** —
pg_graphql's default — so names are used exactly as-is: table `blog_posts`
becomes:

| Piece | Name |
| --- | --- |
| Object type (implements `Node`) | `blog_posts` — one field per column plus `nodeId: ID!` |
| Query field | `blog_postsCollection(first, last, before, after, offset, filter, orderBy)` → `blog_postsConnection` |
| Connection shape | `{ edges { cursor node } pageInfo { hasNextPage hasPreviousPage startCursor endCursor } totalCount }` |
| Filter / order inputs | `blog_postsFilter` (per-column comparators + `and`/`or`/`not`), `blog_postsOrderBy` with enum `OrderByDirection {AscNullsFirst, AscNullsLast, DescNullsFirst, DescNullsLast}` |
| Mutations | `insertIntoblog_postsCollection(objects: [blog_postsInsertInput!]!)`, `updateblog_postsCollection(set, filter, atMost: Int! = 1)`, `deleteFromblog_postsCollection(filter, atMost: Int! = 1)` — responses `{ affectedCount, records }` |
| Node lookup | `node(nodeId: ID!): Node`; `nodeId` is base64 of `[schema, table, pk values…]` JSON |

Filter comparators: every filterable scalar gets `eq, neq, in, is`
(`is` takes `FilterIs { NULL, NOT_NULL }`); ordered scalars add
`gt, gte, lt, lte`; `String` adds `like, ilike, startsWith` (SQL wildcard
semantics for like/ilike; `startsWith` escapes its argument).

**Scalar mapping** (pg_graphql shapes):

| PostgreSQL | GraphQL | JSON form |
| --- | --- | --- |
| `int2`, `int4` | `Int` | number |
| `int8` | `BigInt` | **string** |
| `float4`, `float8` | `Float` | number |
| `numeric` | `BigFloat` | **string** |
| `bool` | `Boolean` | bool |
| `text`, `varchar`, `char`, `citext` | `String` | string |
| `uuid` | `UUID` | string |
| `json`, `jsonb` | `JSON` | **string of serialized JSON** (opaque, in and out) |
| `timestamp`, `timestamptz` | `Datetime` | ISO-8601 string |
| `date` / `time` | `Date` / `Time` | string |
| `bytea` | `Opaque` | base64 string |
| arrays | list of the element scalar | array |
| anything exotic (`vector`, `hstore`, `ltree`, `cube`, …) | `String` (PostgreSQL text form) | string — reflection never fails on a type |

`JSON`/`Opaque` columns and array columns are **not filterable/orderable**
(absent from the Filter/OrderBy inputs — visible via introspection, so
nothing is silently ignored).

**Relationships** (single level in each direction, from the catalog's foreign
keys; constraint-name independent). For an FK `blog_posts.author_id →
authors.id`:

- child → parent object field on `blog_posts`, named **after the referenced
  table**: `authors` (nullable; a NULL FK or an RLS-hidden parent resolves to
  `null`);
- parent → child collection field on `authors`, named
  `blog_postsCollection`, with the full collection argument set (`filter`,
  `first`, `after`, …) applied on top of the implicit FK restriction
  (`totalCount` respects both).

When the plain name collides (several FKs to the same table, or a column of
the same name) the field is named `<table>_by_<fkcol1>[_<fkcolN>]`
(child side: `…Collection`); a still-colliding field is omitted from the
schema. Relationship traversal deeper than **8** levels is a GraphQL error.

**Pagination.** Cursors are opaque base64 of the row's primary-key values
(JSON array); keyset pagination runs on the primary key (lexicographic for
composite keys). `first`/`after` page forward, `last`/`before` page backward
(rows always return in ascending-PK base order); `offset` is supported for
forward pagination. A user `orderBy` (each list element sets **exactly one**
column — GraphQL input-object field order is not otherwise preservable) sorts
with an automatic PK tiebreak, but **combining `before`/`after` cursors with
`orderBy` is rejected with a GraphQL error**: cursors are PK-based and a
truthful error beats a wrong page. When neither `first` nor `last` is given
the page size defaults to 30.

**Mutations & transactions.** `atMost` semantics match pg_graphql: the
mutation runs in a transaction, the affected rows are counted, and if the
count exceeds `atMost` the transaction **rolls back** and the field errors
(`update impacts too many records` / `delete impacts too many records`).
Each mutation field runs in **its own transaction**, sequentially in document
order; the first error aborts the remaining mutation fields (divergence:
pg_graphql wraps the whole request in one transaction).

**Introspection.** `__schema`, `__type(name:)` and `__typename` implement
enough of the introspection spec for GraphiQL / graphql-js clients: types,
fields, args, enums, input objects, `NON_NULL`/`LIST` wrappers, interfaces
(`Node`), `queryType`/`mutationType`, and `subscriptionType: null`
(truthfully — no subscriptions). Directives exposed and executed: `@skip`
and `@include` only. Variables, aliases, named + inline fragments (including
fragments on introspection types), and default argument values all work.

**Deliberate divergences from pg_graphql** (everything else out of subset is
an in-band GraphQL error):

- `totalCount` is always present (pg_graphql requires the
  `@graphql({"totalCount": {"enabled": true}})` comment directive);
- cursors + `orderBy` rejected (pg_graphql encodes order values in cursors);
- one column per `orderBy` list element;
- each mutation field is its own transaction (see above);
- `first`/`last` are not clamped to a `max_rows` (pg_graphql clamps to 30);
- no comment-directive support (`@graphql` renames/inflection), no `nodeId`
  pseudo-column in filters;
- **views, functions-as-queries (pg_graphql's function support), computed
  columns and subscriptions are not reflected/supported** — reaching for
  them yields "Unknown field" / "subscriptions are not supported" errors;
- tables without a primary key, with GraphQL-invalid names, or with a column
  named `nodeId` are not reflected;
- engine note: insert coercion routes integers through `f64`, so `bigint`
  values beyond 2⁵³ lose precision engine-wide (REST and GraphQL alike).

### Not implemented in this slice → typed `501`

`/functions/v1/*` returns
`{"code":"SUPA_COMPAT_FUNCTIONS_NOT_IMPLEMENTED","message":"…","hint":"tracked for a later slice"}`
with HTTP `501` — functions would need a Deno/edge runtime. `/health` returns
`200 {"status":"ok"}`.

---

## 4. Auth, JWT, and Row-Level Security

**JWT.** HS256, implemented from scratch (`jwt.rs`) on `hmac`+`sha2`+`base64`.
Signatures are compared in constant time; `exp` is enforced. `Claims` carries
`{iss, role, sub, email, aud, iat, exp, session_id, extra}`; unknown claims
round-trip through `extra`.

**API keys.** `ProjectKeys::from_secret(secret, iat)` deterministically derives
the `anon` and `service_role` keys as real HS256 JWTs with the Supabase claim
shape (`{role, iss:"supabase", iat, exp}`, 10-year expiry). The generator is
**pure** — the secret and `iat` are injected, never sourced from `now()`/`rand`
inside the constructor — so tests are deterministic. The random secret generator
(`generate_jwt_secret`, ≥ 48 chars) and project-ref generator are the only impure
helpers, used by the binary.

**Secrets.** `Secret` and `ProjectKeys` redact themselves in `Debug`; no secret is
ever written to an error body or log.

**Passwords.** bcrypt (cost 10) via the in-tree `bcrypt` crate.

**Sessions & refresh tokens.** A sign-in creates an `auth.sessions` row and an
opaque `auth.refresh_tokens` row. Refresh **rotates**: the presented token is
marked `revoked`, a new token is minted on the same session (with `parent`
set), and a fresh access token is issued.

**Role resolution.** The effective Postgres role is the role claim of the
`Authorization: Bearer` token if present, else the `apikey`'s role (PostgREST
semantics). Each request opens `Session::new(db, role)`.

**Row-Level Security — enforced.** The Supabase security model is real:

- Every REST data path runs through `run_sql_as` (`gateway.rs`), which binds
  the per-request session to the resolved role **and injects the verified JWT
  claims** as the `request.jwt.claims` session variable
  (`Session::set_var`, no SQL round-trip). When no bearer token was supplied,
  a minimal `{"role": "<role>"}` document is synthesized so `auth.role()`
  still reflects the effective role.
- The engine evaluates policies per row (see `docs/postgres-compat.md`,
  "Row-Level Security"): tables with `ALTER TABLE ... ENABLE ROW LEVEL
  SECURITY` are filtered for `anon` / `authenticated` / custom roles by their
  `CREATE POLICY` rules — permissive policies OR together, restrictive
  policies AND, no applicable policy means **default deny**.
- `service_role` **bypasses** row security exactly like Supabase's service
  key (`BYPASSRLS`). The engine-owner roles `postgres`/`guardian` bypass it
  like PostgreSQL table owners — unless the table declares
  `ALTER TABLE ... FORCE ROW LEVEL SECURITY`, which subjects them to its
  policies (`BYPASSRLS` still wins; the flag surfaces as `rls_forced` in
  `/pg-meta/tables` and `pg_class.relforcerowsecurity`).
- Policies use the standard Supabase helpers: `auth.uid()` (the `sub` claim
  as `uuid`), `auth.role()`, `auth.jwt()`, and
  `current_setting('request.jwt.claims')`.
- A write denied by `WITH CHECK` surfaces as a PostgREST-shaped error with
  SQLSTATE `42501` and HTTP **403** (`new row violates row-level security
  policy for table "t"`); rows filtered by `USING` simply do not appear
  (reads return fewer rows; `UPDATE`/`DELETE` affect fewer rows) — never an
  error, matching PostgreSQL/PostgREST.
- Internal work (GoTrue's `auth.*` tables, schema bootstrap) runs as
  `service_role` via `run_sql` and is unaffected.

**Auth schema bootstrap.** On first auth request, `BOOTSTRAP_SQL` runs through a
`Session` (idempotent `CREATE SCHEMA/TABLE IF NOT EXISTS`), creating
`auth.users`, `auth.refresh_tokens`, `auth.sessions`, `auth.identities`,
`auth.audit_log_entries`, `auth.instances`, `auth.schema_migrations`. The full
integration suite exercises this, proving every statement executes on the engine.
Divergences from stock Supabase, all driven by engine support:
- `auth.refresh_tokens` is keyed by its opaque `token` (stock uses a `bigserial
  id`); the engine supports a `text` primary key cleanly.
- GoTrue's generated/identity columns, partial indexes, and `CHECK`-heavy columns
  are omitted; timestamps and metadata are generated in Rust and inserted
  explicitly (no reliance on DB-side `DEFAULT` functions).

---

## 5. REST behaviour

**Query translation** (`rest.rs`). PostgREST query params become parameterised
SQL. All literal values are bound as `$n`; identifiers (table, column, function,
schema) are validated against `^[A-Za-z_][A-Za-z0-9_]*$` — the only defense for
identifiers, which cannot be bound. Filter/body values are coerced to the
**declared column type** (read from the catalog) so numeric/temporal comparisons
are typed, not lexical; unknown columns fall back to value inference.

| PostgREST | SQL |
| --- | --- |
| `select=id,full_name:name` | `SELECT "id", "name" AS "full_name"` |
| `id=eq.1` | `"id" = $1` (bound `1`) |
| `name=ilike.*ali*` | `"name" ILIKE $1` (bound `%ali%`) |
| `id=in.(1,2,3)` | `"id" IN ($1,$2,$3)` |
| `deleted_at=is.null` | `"deleted_at" IS NULL` |
| `active=not.is.false` | `NOT ("active" IS FALSE)` |
| `order=created_at.desc.nullslast` | `ORDER BY "created_at" DESC NULLS LAST` |
| `limit=10&offset=20` / `Range: 20-29` | `LIMIT 10 OFFSET 20` |

Supported operators: `eq, neq, gt, gte, lt, lte, like, ilike, is, in`, each
optionally negated with the `not.` prefix. **Any other operator** (e.g. `cs`,
`cd`, `fts`) or a logical tree (`and=`, `or=`) is rejected with
`SUPA_COMPAT_REST_UNSUPPORTED_FILTER` (`400`) — never silently ignored.

**Writes.** `POST` accepts a single object or an array; columns are the union of
keys (missing cells become `DEFAULT`). Upsert: `Prefer: resolution=merge-duplicates`
+ `on_conflict=col,…` → `ON CONFLICT (…) DO UPDATE SET col = EXCLUDED.col`
(falls back to the primary key when `on_conflict` is absent);
`resolution=ignore-duplicates` → `DO NOTHING`. `PATCH`/`DELETE` apply the query
filters as the `WHERE`.

**Responses.** Rows render as a JSON array of objects (timestamps as ISO-8601
with a `T` separator). `Accept: application/vnd.pgrst.object+json` returns a
single object (`400` if not exactly one row). `Prefer: return=representation`
echoes affected rows (default `minimal` → `201`/`204` with no body).
`Prefer: count=exact` runs a `count(*)` and returns `206` with a
`Content-Range: start-end/total` header; otherwise `Content-Range: start-end/*`.

**RPC.** `POST /rest/v1/rpc/{fn}` builds `SELECT fn($1, …)` from the JSON body's
values and returns the rows as objects. **Known divergence:** because JSON object
keys are unordered, named arguments are passed **positionally in sorted key
order** rather than by true named binding. Functions must exist in the engine
(built-ins / extension functions); calling an unknown function returns the
engine's `42883` in PostgREST shape.

**Errors.** Engine errors render in PostgREST shape
`{"code": <SQLSTATE>, "message", "details", "hint"}` with the HTTP status derived
from the SQLSTATE class (e.g. `42P01` → `404`, `23505` → `409`, `22P02`/`42601` →
`400`).

---

## 6. Typed-error taxonomy

`SupaError` (`error.rs`) — every gateway-level failure is typed; no secret ever
appears in a body or log.

| Variant | HTTP | `code` |
| --- | --- | --- |
| `MissingApiKey` | 401 | `SUPA_COMPAT_MISSING_API_KEY` |
| `InvalidApiKey` | 401 | `SUPA_COMPAT_INVALID_API_KEY` |
| `InvalidJwt` | 401 | `SUPA_COMPAT_INVALID_JWT` |
| `Forbidden` | 403 | `SUPA_COMPAT_FORBIDDEN` |
| `NotImplemented(SERVICE)` | 501 | `SUPA_COMPAT_<SERVICE>_NOT_IMPLEMENTED` |
| `UnsupportedFilter` | 400 | `SUPA_COMPAT_REST_UNSUPPORTED_FILTER` |
| `BadRequest` | 400 | `SUPA_COMPAT_REST_BAD_REQUEST` |
| `AuthProviderUnsupported` | 400 | `SUPA_COMPAT_AUTH_PROVIDER_UNSUPPORTED` |
| `Sql(err)` | per SQLSTATE | the SQLSTATE (PostgREST shape) |
| `Internal` | 500 | `SUPA_COMPAT_INTERNAL` |

GoTrue endpoints additionally use GoTrue's own shapes:
`{"code","error_code","msg"}` for most endpoints and
`{"error","error_description"}` for the token endpoint (e.g. `invalid_grant` on a
bad password/refresh token). OAuth/SSO grants (`id_token`, `authorization_code`,
`pkce`, `web3`, `implicit`) return `AuthProviderUnsupported` — never fake success.

---

## 7. Deferred to later slices

- **Edge Functions.** Routed and returning typed `501`; not implemented.
- **GraphQL**: views, pg_graphql function support, computed columns,
  subscriptions, comment-directive configuration (inflection/renames),
  order-aware cursors (see §3 GraphQL divergences).
- **REST**: embedded resources / joins (`select=author(name)`), the full operator
  set (`cs`, `cd`, `ov`, `fts`, `wfts`, …), logical `and`/`or` trees, computed
  columns, true named-argument RPC binding, vertical filtering on embeds.
- **Auth**: email/SMS delivery and confirmation flows, OAuth/SSO providers, MFA,
  anonymous sign-in, password recovery, per-session logout scopes.
- **Storage**: multipart/resumable (TUS) uploads, image transformations
  (`/render/image`), folder-emulating list responses, iroh-blobs-backed object
  bytes (bytes currently ride the SQL layer, see §3).
- **Realtime**: presence, DELETE / PK-less-row delivery under RLS (withheld by
  design, see §3), replicated-write changefeed (only local commits are
  observed), Phoenix binary serializer.
- **pg-meta**: mutation endpoints Studio uses for point-and-click DDL
  (`POST /tables`, `PATCH /columns`, …) — Studio's SQL editor (`POST /query`)
  covers the same ground; relation statistics (sizes / row estimates).
- **Multi-project / multi-tenant** routing (the shell serves one project).

---

## 8. Running it

### Start the gateway

```bash
# In-memory (development); prints SUPABASE_URL, ANON_KEY, SERVICE_ROLE_KEY, JWT_SECRET
cargo run --features supabase --bin guardian-supabase

# Options
cargo run --features supabase --bin guardian-supabase -- \
  --addr 127.0.0.1:54321 \
  --database app \
  --jwt-secret "your-fixed-secret-if-you-want-stable-keys" \
  --path ./guardian_supabase_data   # persistent, Iroh-replicated node
```

Startup prints, e.g.:

```
  SUPABASE_URL      : http://127.0.0.1:54321
  ANON_KEY          : eyJhbGciOiJIUzI1NiIs...
  SERVICE_ROLE_KEY  : eyJhbGciOiJIUzI1NiIs...
  JWT_SECRET        : <generated>   (save this to reuse the keys)
```

### Point supabase-js at it

```ts
import { createClient } from "@supabase/supabase-js";

const supabase = createClient("http://127.0.0.1:54321", ANON_KEY);

// REST
const { data } = await supabase.from("todos").select("*").eq("done", false);
await supabase.from("todos").insert({ id: 1, title: "buy milk", done: false });

// Auth
await supabase.auth.signUp({ email: "a@b.com", password: "hunter2pass" });
const { data: session } = await supabase.auth.signInWithPassword({
  email: "a@b.com",
  password: "hunter2pass",
});

// Storage (create buckets with the service key or add bucket policies first)
await supabase.storage.from("avatars").upload("me.png", file);
const { data: url } = await supabase.storage
  .from("avatars")
  .createSignedUrl("me.png", 3600);

// Realtime
supabase
  .channel("room1")
  .on("postgres_changes", { event: "INSERT", schema: "public", table: "todos" },
      (payload) => console.log(payload.new))
  .on("broadcast", { event: "cursor" }, (msg) => console.log(msg))
  .subscribe();
```

Tables must be created first (over `psql`/pgwire, or a direct `Session`); REST
does not create tables. The `auth` schema is bootstrapped automatically on the
first auth request.

### curl smoke test

```bash
ANON=... # from startup
curl -s "http://127.0.0.1:54321/rest/v1/todos?select=*" -H "apikey: $ANON"
curl -s -X POST "http://127.0.0.1:54321/auth/v1/signup" \
  -H "apikey: $ANON" -H 'content-type: application/json' \
  -d '{"email":"a@b.com","password":"hunter2pass"}'
```

### Tests

```bash
cargo test --features supabase                       # everything
cargo test --features supabase --test supabase_gateway   # in-process gateway tests
cargo test --features supabase --test supabase_storage_realtime # storage + pg-meta + realtime
cargo test --features supabase --test supabase_graphql   # pg_graphql-compatible endpoint
cargo test --features supabase --lib supabase::          # unit tests
```

The REST/Auth/Storage/pg-meta integration tests drive the axum `Router`
in-process with `tower::ServiceExt::oneshot` over a `MemoryStorage`-backed
`Database` — no real ports are bound. The realtime tests bind an ephemeral
`127.0.0.1` port and connect a real websocket client (`tokio-tungstenite`).
