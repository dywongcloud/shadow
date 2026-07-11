# guardiandb-odm — GuardianDB TypeScript ODM

> **Scope: this package is for regular GuardianDB (the document/collection store), _not_ the PostgreSQL compatibility layer.**
> It speaks GuardianDB's native document model — collections, Mongoose-style CRUD, schemas — directly over an Iroh-backed transport.
> If you want SQL, TypeORM, or a Postgres wire connection to GuardianDB, use
> [`guardiandb-postgres-typeorm`](../guardiandb-postgres-typeorm) and the `pgwire` server instead; the two layers are independent and their data is not interchangeable.

This package contains the optional TypeScript/JavaScript ODM proposed in issue #17. It is transport-driven: the high-level `GuardianDB` and `Collection<T>` APIs do not depend on one particular Node, browser, React Native, or WASM binding.

```ts
import GuardianDB from "guardiandb-odm";
import Iroh from "iroh";

const iroh = await Iroh.create();
const db = await GuardianDB.init("DatabaseName", iroh, { path: "./.guardiandb" });
const employees = await db.initCollection("employees");

await employees.insertOne({ name: "Elon", ssn: "562-48-5384", hourly_pay: "$15" });
const employee = await employees.findOne({ ssn: "562-48-5384" });
const updated = await employees.update(
  { ssn: "562-48-5384" },
  { $set: { hourly_pay: "$100" } },
);

console.log(await GuardianDB.listDatabases());
console.log(await db.listCollections());
```

Typed schemas add validation, primary keys, unique and secondary indexes, defaults, custom validators, strict mode, and timestamps:

```ts
import { defineSchema, type Document } from "guardiandb-odm";

interface Employee extends Document {
  id?: string;
  ssn: string;
  name: string;
}

const schema = defineSchema<Employee>({
  timestamps: true,
  fields: {
    id: { type: String, primaryKey: true },
    ssn: { type: String, required: true, unique: true },
    name: { type: String, required: true, index: true },
  },
});

const employees = await db.initCollection<Employee>("employees", { schema });
```

## Choosing between the ODM and the PostgreSQL layer

| | `guardiandb-odm` (this package) | `guardiandb-postgres-typeorm` |
|---|---|---|
| Data model | Documents / collections | Relational tables (SQL) |
| API style | Mongoose-like CRUD, typed schemas | TypeORM entities / raw SQL |
| Talks to | GuardianDB document store via `GuardianTransport` | GuardianDB `pgwire` server (Postgres wire protocol) |
| Use when | You want the native local-first document model | You need SQL semantics or existing Postgres/TypeORM tooling |

## Transport integration

A native GuardianDB/Iroh binding should expose a `GuardianTransport` as `iroh.guardianDBTransport`, or callers can pass it explicitly in `GuardianDB.init(..., { transport })`. The included `MemoryTransport` is a deterministic process-local reference implementation used by tests and development; it does not provide decentralized persistence. Until a native adapter is supplied, `GuardianDB.init` falls back to that reference transport so the SDK surface remains executable.

Writes with the default `local_atomic` transaction context serialize validation, index maintenance, and mutation per collection. The `replicated` consistency value is reserved for a future distributed coordinator and is rejected by the reference transport rather than implying cross-peer ACID guarantees.
