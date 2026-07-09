// CRUD with `GuardianDataSource` and Zod validation.
//
// `GuardianDataSource` starts an embedded GuardianDB PostgreSQL gateway and then
// behaves like a normal TypeORM DataSource. Every write is validated by a Zod
// schema (`./schema`) before it reaches the database; reads use the Repository
// and QueryBuilder APIs.
//
// Build the gateway binary first, then run with tsx:
//   cargo build --features pgwire --bin guardian-pgwire
//   node --import tsx examples/crud.ts

import "reflect-metadata";
import { ZodError } from "zod";
import { GuardianDataSource } from "../src";
import { User, Post } from "./entities";
import { CreateUserSchema, UpdateUserSchema, CreatePostSchema, formatIssues } from "./schema";

async function main(): Promise<void> {
  const ds = new GuardianDataSource({
    path: "./guardian_pg_data", // data dir for the GuardianDB-backed gateway
    database: "app",
    consistency: "local", // "local" (CRDT/local-first) | "strict"
    entities: [User, Post],
    synchronize: true, // auto-create the schema from the entities
  });

  await ds.initialize(); // spawns the gateway, then connects
  try {
    const users = ds.getRepository(User);
    const posts = ds.getRepository(Post);

    // CREATE — validate untrusted input, then persist. The schema trims the
    // name and applies defaults, yielding exactly the entity's shape.
    const input = CreateUserSchema.parse({
      email: "alice@example.com",
      name: "  Alice  ",
      settings: { theme: "dark" },
    });
    const alice = await users.save(users.create(input));
    console.log("CREATE user:", alice.id, alice.name, JSON.stringify(alice.settings));

    // Invalid input is rejected before any database call.
    const bad = CreateUserSchema.safeParse({ email: "nope", name: "" });
    console.log("CREATE rejected invalid input:", !bad.success, bad.success ? "" : `-> ${formatIssues(bad.error)}`);

    // CREATE a related row. `authorId` is validated, but the entity links the
    // author by relation, so omit it and set `author` instead.
    const { authorId, ...postFields } = CreatePostSchema.parse({
      title: "Hello GuardianDB",
      published: true,
      authorId: alice.id,
    });
    void authorId;
    await posts.save(posts.create({ ...postFields, author: alice }));
    console.log("CREATE post:", postFields.title);

    // READ — by unique column and via a QueryBuilder with a filter + count.
    const found = await users.findOneByOrFail({ email: "alice@example.com" });
    console.log("READ findOneBy:", found.name);
    const [items, total] = await posts
      .createQueryBuilder("p")
      .innerJoinAndSelect("p.author", "u")
      .where("p.published = :pub", { pub: true })
      .orderBy("p.title", "ASC")
      .getManyAndCount();
    console.log("READ published posts:", total, "->", items.map((p) => p.title).join(", "));

    // UPDATE — validate a partial patch, then apply it (load → assign → save,
    // which keeps the JSONB `settings` type intact under `strict`).
    const patch = UpdateUserSchema.parse({ name: "Alice Cooper", settings: { theme: "light" } });
    const toUpdate = await users.findOneByOrFail({ email: "alice@example.com" });
    Object.assign(toUpdate, patch);
    await users.save(toUpdate);
    console.log("UPDATE user:", toUpdate.name, JSON.stringify(toUpdate.settings));

    // DELETE — remove the user's posts, then the user.
    await posts.createQueryBuilder().delete().where("authorId = :id", { id: alice.id }).execute();
    const res = await users.delete({ email: "alice@example.com" });
    console.log("DELETE user:", res.affected ?? 0, "remaining:", await users.count());

    console.log("\nCRUD walkthrough complete ✅");
  } finally {
    await ds.destroy(); // disconnects and stops the embedded gateway
  }
}

main().catch((e) => {
  if (e instanceof ZodError) console.error("validation failed:", formatIssues(e));
  else console.error("example failed:", e);
  process.exitCode = 1;
});
