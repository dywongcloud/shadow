// Self-contained runnable demo: spawns the GuardianDB PostgreSQL gateway, runs
// the TypeORM migration, seeds data, and exercises the query/transaction
// examples — then tears the gateway down. Run with `npm run demo`.
//
// In a real deployment you would start the gateway separately
// (`cargo run --features pgwire --bin guardian-pgwire`) and just point TypeORM
// at it. See `crud.ts` for a CRUD-focused walkthrough with Zod validation.

import "reflect-metadata";
import { DataSource } from "typeorm";
import { options } from "./data-source";
import { seed } from "./seed";
import { runQueries } from "./queries";
import { startGateway } from "./gateway";

async function main() {
  const gw = await startGateway();
  console.log(`gateway ready on 127.0.0.1:${gw.port}`);
  try {
    const ds = new DataSource(options({ port: gw.port }));
    await ds.initialize();
    console.log("DataSource initialized");

    const applied = await ds.runMigrations();
    console.log("migrations applied:", applied.map((m) => m.name).join(", ") || "(none)");

    await seed(ds);
    await runQueries(ds);

    await ds.destroy();
    console.log("\nDemo complete ✅");
  } finally {
    gw.stop();
  }
}

main().catch((e) => {
  console.error("Demo failed:", e);
  process.exitCode = 1;
});
