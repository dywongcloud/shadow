# @guardiandb/postgres-typeorm

A GuardianDB-aware TypeORM `DataSource`. It is an **optional** convenience that
manages an embedded GuardianDB PostgreSQL gateway for you; once initialized it
behaves exactly like a normal TypeORM `DataSource` because the driver underneath
is the standard `postgres` one.

> The required, primary way to use GuardianDB from TypeORM is the PostgreSQL
> wire path (`type: "postgres"`). This package just removes the "start the
> gateway yourself" step for embedded use.

## Usage

```ts
import "reflect-metadata";
import { GuardianDataSource } from "@guardiandb/postgres-typeorm";
import { User, Post, Org } from "./entities";

const ds = new GuardianDataSource({
  path: "./data",            // data directory (GuardianDB-backed gateway)
  database: "app",
  peers: [],                 // iroh peers to replicate with
  consistency: "strict",     // "local" (default) | "strict"
  entities: [User, Post, Org],
  synchronize: true,
});

await ds.initialize();       // spawns the gateway, then connects
const users = ds.getRepository(User);
await users.save(users.create({ email: "a@x.com", name: "Alice" }));
await ds.destroy();          // disconnects and stops the gateway
```

Everything TypeORM offers works: entities, repositories, `EntityManager`,
migrations, schema sync, QueryBuilder, transactions, and relation metadata.

## Examples

The [`examples/`](./examples) directory has runnable code for the full path a
typical app needs — **typed domain models**, **Zod-validated input**, and
**CRUD** through `GuardianDataSource`:

| File                                    | What it shows                                            |
| --------------------------------------- | -------------------------------------------------------- |
| [`entities.ts`](./examples/entities.ts) | TypeORM entities (`User`, `Post`)                        |
| [`types.ts`](./examples/types.ts)       | Domain types & interfaces (`UserRecord`, `UserSettings`) |
| [`schema.ts`](./examples/schema.ts)     | Schema creation with **Zod** (types derived via `z.infer`) |
| [`crud.ts`](./examples/crud.ts)         | **CRUD** with end-to-end validation                      |

```bash
cargo build --features pgwire --bin guardian-pgwire   # gateway binary the DataSource spawns
npm install
npm run example:crud
```

See [`examples/README.md`](./examples/README.md) for the walkthrough and the
expected output.

## Configuration

| Option        | Default            | Meaning                                            |
| ------------- | ------------------ | -------------------------------------------------- |
| `path`        | —                  | Local data directory (GuardianDB-backed gateway)   |
| `database`    | `app`              | Logical database name                              |
| `peers`       | `[]`               | Iroh peer addresses to replicate with              |
| `consistency` | `local`            | `local` (CRDT/local-first) or `strict` (SQL)       |
| `port`        | `15432`            | Embedded gateway TCP port                          |
| `host`        | `127.0.0.1`        | Bind host                                          |
| `binary`      | resolved from env  | Path to the `guardian-pgwire` binary               |

Set `GUARDIAN_PGWIRE_BIN` to point at the gateway binary, or pass `{ binary }`.

> `path`, `peers` and `consistency` take effect with the GuardianDB-backed
> gateway (the `sql` feature of the `guardian-db` crate). The default
> `guardian-pgwire` binary is in-memory and accepts these flags as no-ops.

## Connecting with a plain connection string

`GuardianDataSource` manages the gateway for you, but you don't need this
package at all to talk to an already-running gateway — any PostgreSQL client
connects with an ordinary connection string:

```
postgres://guardian:guardian@127.0.0.1:15432/app?sslmode=disable
```

```ts
// node-postgres
const client = new pg.Client({ connectionString: "postgres://guardian:guardian@127.0.0.1:15432/app?sslmode=disable" });

// plain TypeORM
const ds = new DataSource({ type: "postgres", url: "postgres://guardian:guardian@127.0.0.1:15432/app", ssl: false, entities: [...] });
```

Pass `sslmode=disable` for libpq-based clients (`psql`, DBeaver, psycopg) —
the gateway is a plaintext loopback socket and does not negotiate TLS.
node-postgres and TypeORM default to no-SSL and work without it.

## Build / test

```bash
cargo build --features pgwire --bin guardian-pgwire   # gateway binary the package spawns
npm install
npm test
```
