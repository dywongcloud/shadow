# GuardianDB + TypeORM example

A TypeORM application that talks to GuardianDB **as if it were PostgreSQL** —
using the standard `type: "postgres"` driver, with no GuardianDB-specific code.

It demonstrates: `DataSource` initialization, a migration authored with
`QueryRunner`, entities with relations (`Org` → `User` → `Post`), repository
saves/finds/updates/deletes, `findOneBy`, eager relation loading, QueryBuilder
joins and aggregates, a transaction, generated integer **and** UUID ids, JSONB
columns, timestamp columns, unique constraints and indexes — plus a separate
**CRUD walkthrough** with **typed domain models** and **Zod-validated input**
(see [CRUD, types & validation](#crud-types--validation-zod)).

### Source layout

| File                  | What it shows                                                       |
| --------------------- | ------------------------------------------------------------------ |
| `src/entities/*.ts`   | TypeORM entities with relations, JSONB, generated ids              |
| `src/migrations/*.ts` | A `QueryRunner` migration (the schema real apps ship)              |
| `src/types.ts`        | Plain domain types & interfaces (`UserRecord`, `Page<T>`, …)       |
| `src/schema.ts`       | **Schema creation with Zod** — validators + types via `z.infer`    |
| `src/crud.ts`         | A focused **CRUD** walkthrough (`npm run crud`)                    |
| `src/queries.ts`      | Joins, aggregates and a transaction (`npm run demo`)              |

## Run it

```bash
# 1. Build the gateway (from the repo root)
cargo build --features pgwire --bin guardian-pgwire

# 2. Run the self-contained demo (spawns the gateway, migrates, seeds, queries)
cd examples/postgres-typeorm
npm install
npm run demo
```

Expected output (abridged):

```
gateway ready on 127.0.0.1:NNNNN
DataSource initialized
migrations applied: Init1700000000000
seeded: 2 orgs, 2 users, 3 posts
findOneBy: Alice settings= {"theme":"dark"}
relations: org = Acme posts = 2
post counts by author: [{"author":"Alice","posts":2},{"author":"Bob","posts":1}]
published titles: Globex update, Hello GuardianDB
transaction: reassigned + published a post
updated bob -> Robert: Robert
deleted unpublished posts: 3 -> 2
Demo complete ✅
```

## CRUD, types & validation (Zod)

`npm run crud` runs a focused create/read/update/delete walkthrough where every
write is validated **before** it reaches the database. The pieces:

- **`src/types.ts`** — plain, framework-agnostic domain types (`UserRecord`,
  `UserSettings`, `PostMeta`, `Page<T>`). The vocabulary the app speaks in.
- **`src/schema.ts`** — Zod is the single source of truth for the shape of
  untrusted input. Input DTO types are *derived from* the schemas, so types and
  runtime validation can never drift:

  ```ts
  export const CreateUserSchema = z.object({
    email: z.email().max(160),
    name: z.string().trim().min(1).max(200),
    settings: UserSettingsSchema.default({}),
    orgId: z.number().int().positive().nullable().default(null),
  });
  export type CreateUser = z.output<typeof CreateUserSchema>; // types follow the schema
  ```

- **`src/crud.ts`** — validate, then persist with an ordinary TypeORM repository:

  ```ts
  const input = CreateUserSchema.parse(rawInput);     // throws ZodError on bad input
  const alice = await users.save(users.create(input));
  ```

```bash
cargo build --features pgwire --bin guardian-pgwire   # once, from the repo root
npm install
npm run crud
```

Expected output (abridged):

```
CREATE user: 1 Alice {"theme":"dark"}
CREATE rejected invalid input: true -> email: Invalid email address; name: Too small: expected string to have >=1 characters
CREATE posts: 2 (1 published, 1 draft)
READ findOneBy: Alice
READ relations: org = Acme posts = 2
READ page 1: 1/1 published -> Postgres on Iroh
UPDATE user: Alice Cooper {"theme":"light"}
DELETE unpublished posts: removed 1 (2 -> 1)
DELETE user; remaining users: 0
CRUD walkthrough complete ✅
```

The same typed-model + Zod pattern is also shown against the embedded
`GuardianDataSource` in the package's
[`examples/`](../../packages/guardiandb-postgres-typeorm/examples).

## Connecting to a long-running gateway

In a real deployment, start the gateway separately and point any PostgreSQL
client at it with an ordinary connection string:

```bash
cargo run --features pgwire --bin guardian-pgwire            # listens on 127.0.0.1:15432

# canonical connection string (sslmode=disable: the gateway has no TLS;
# needed by libpq clients like psql — node-postgres/TypeORM work without it)
psql 'postgres://guardian:guardian@127.0.0.1:15432/app?sslmode=disable'
```

With TypeORM, either the single-string form:

```ts
const ds = new DataSource({
  type: "postgres",
  url: "postgres://guardian:guardian@127.0.0.1:15432/app",
  ssl: false,
  synchronize: true,
  entities: [User, Post, Org],
});
```

or discrete options:

```ts
const ds = new DataSource({
  type: "postgres",
  host: "127.0.0.1",
  port: 15432,
  username: "guardian",
  password: "guardian",
  database: "app",
  synchronize: true,
  entities: [User, Post, Org],
});
```

This example's [`data-source.ts`](./src/data-source.ts) honors `DATABASE_URL`,
so the migration CLI and demos accept a connection string directly:

```bash
DATABASE_URL=postgres://guardian:guardian@127.0.0.1:15432/app npm run migration:run
# or with discrete PG* variables:
PGPORT=15432 npm run migration:run
```

For a runnable end-to-end demo of both connection-string forms, see
[`src/connection-string.ts`](./src/connection-string.ts):

```bash
npm run connection-string
```

## Native GuardianDB driver (optional)

The `@guardiandb/postgres-typeorm` package (`packages/guardiandb-postgres-typeorm`) offers a
`GuardianDataSource` convenience that manages an embedded gateway for you. See
its README. The PostgreSQL wire path shown here is the primary, required path.

## Replicated, local-first gateway (Postgres on Iroh)

The `guardian-pgwire` binary used above is an **in-memory** dev gateway. To run
TypeORM against a **persistent, replicated GuardianDB / Iroh node** instead,
start the Iroh-backed gateway example and point TypeORM at it unchanged:

```bash
cargo run --features pgwire --example postgres_iroh_gateway
#   options: -- --addr 127.0.0.1:15432 --database app --path ./guardian_pg_data
```

Every table TypeORM creates becomes a GuardianDB document that persists locally
and replicates to peers over Iroh. See
[`docs/postgres-compat.md`](../../docs/postgres-compat.md) §1 and
[`examples/postgres_iroh_gateway.rs`](../postgres_iroh_gateway.rs).
