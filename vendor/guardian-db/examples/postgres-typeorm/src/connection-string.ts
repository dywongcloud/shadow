// Connecting to the GuardianDB PostgreSQL gateway with an ordinary
// connection string — the same `postgres://` URI you would use against a
// real PostgreSQL server. Run with `npm run connection-string`.
//
// Canonical form (default gateway flags):
//
//   postgres://guardian:guardian@127.0.0.1:15432/app?sslmode=disable
//
// `sslmode=disable` matters for libpq-based clients (psql, DBeaver, psycopg):
// the gateway is a plaintext loopback socket and does not negotiate TLS.
// node-postgres and TypeORM default to no-SSL and work without it.
//
// This demo spawns its own gateway on an ephemeral port to stay
// self-contained; against a long-running gateway you would just hardcode
// port 15432 or read the string from DATABASE_URL.

import "reflect-metadata";
import pg from "pg";
import { DataSource } from "typeorm";
import { Org } from "./entities/Org";
import { User } from "./entities/User";
import { Post } from "./entities/Post";
import { startGateway } from "./gateway";

async function main() {
  const gw = await startGateway();
  const url = `postgres://guardian:guardian@127.0.0.1:${gw.port}/app`;
  console.log(`gateway ready — connection string: ${url}`);

  try {
    // 1. node-postgres: pass the string as `connectionString`.
    const client = new pg.Client({ connectionString: `${url}?sslmode=disable` });
    await client.connect();
    await client.query("CREATE TABLE greetings (id INT PRIMARY KEY, msg TEXT NOT NULL)");
    await client.query("INSERT INTO greetings VALUES ($1, $2)", [1, "hello over a connection string"]);
    const { rows } = await client.query("SELECT msg FROM greetings WHERE id = $1", [1]);
    console.log("node-postgres:", rows[0].msg);
    await client.end();

    // 2. TypeORM: pass the string as `url` (instead of host/port/username/...).
    const ds = new DataSource({
      type: "postgres",
      url,
      ssl: false,
      entities: [Org, User, Post],
      synchronize: true,
      logging: ["error", "warn"],
    });
    await ds.initialize();
    const orgs = ds.getRepository(Org);
    const org = await orgs.save(orgs.create({ name: "Acme" }));
    console.log("typeorm:", `saved org #${org.id} via url-configured DataSource`);
    await ds.destroy();

    console.log("\nConnection-string demo complete ✅");
  } finally {
    gw.stop();
  }
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
