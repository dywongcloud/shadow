# Examples — `@guardiandb/postgres-typeorm`

Runnable examples for the GuardianDB-aware TypeORM `DataSource`. They show the
full path most apps need: **typed domain models**, **Zod-validated input**, and
**CRUD** through `GuardianDataSource` (which manages an embedded GuardianDB
PostgreSQL gateway for you).

| File           | What it shows                                                              |
| -------------- | ------------------------------------------------------------------------- |
| `entities.ts`  | TypeORM entities (`User`, `Post`) — ordinary `postgres` entities          |
| `types.ts`     | Plain domain types & interfaces (`UserRecord`, `UserSettings`, `Page<T>`) |
| `schema.ts`    | **Schema creation with Zod** — validators + types derived via `z.infer`   |
| `crud.ts`      | **CRUD** (create/read/update/delete) with end-to-end validation           |

## The shape of it

```ts
// schema.ts — Zod is the source of truth for untrusted input.
export const CreateUserSchema = z.object({
  email: z.email().max(160),
  name: z.string().trim().min(1).max(200),
  settings: UserSettingsSchema.default({}),
});
export type CreateUser = z.output<typeof CreateUserSchema>; // types follow the schema
```

```ts
// crud.ts — validate, then persist with a normal TypeORM repository.
const ds = new GuardianDataSource({ path: "./data", database: "app", entities: [User, Post], synchronize: true });
await ds.initialize();

const input = CreateUserSchema.parse(rawInput);          // throws on bad input
const user = await ds.getRepository(User).save(input);   // typed, validated row
```

## Run

```bash
# 1. Build the gateway binary the DataSource spawns (from the repo root)
cargo build --features pgwire --bin guardian-pgwire

# 2. Install dev deps (typeorm, zod, tsx) and run the CRUD example
cd packages/guardiandb-postgres-typeorm
npm install
npm run example:crud
```

Expected output (abridged):

```
CREATE user: 1 Alice {"theme":"dark"}
CREATE rejected invalid input: true -> email: Invalid email address; name: Too small: expected string to have >=1 characters
CREATE post: Hello GuardianDB
READ findOneBy: Alice
READ published posts: 1 -> Hello GuardianDB
UPDATE user: Alice Cooper {"theme":"light"}
DELETE user: 1 remaining: 0
CRUD walkthrough complete ✅
```

> The default `guardian-pgwire` binary is an **in-memory** dev gateway, so these
> examples do not persist or replicate between runs. To make the very same code
> persistent and replicated over Iroh, point the DataSource at the Iroh-backed
> gateway (`examples/postgres_iroh_gateway.rs` at the repo root) via
> `GUARDIAN_PGWIRE_BIN`, or see [`docs/postgres-compat.md`](../../../docs/postgres-compat.md) §1.

For a standalone TypeORM app (no `GuardianDataSource`, plain `type: "postgres"`)
that also covers migrations, relations and transactions, see
[`examples/postgres-typeorm`](../../../examples/postgres-typeorm).
