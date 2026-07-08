// A focused, runnable CRUD walkthrough with end-to-end input validation.
//
// Every write crosses a Zod schema first (`./schema`), so only well-formed data
// reaches the database; reads use the Repository and QueryBuilder APIs. The
// schema is auto-created from the entities (`synchronize: true`) to keep this
// example about CRUD rather than migrations — see `demo.ts` for the migration
// workflow. Run with `npm run crud`.

import "reflect-metadata";
import { DataSource } from "typeorm";
import { options } from "./data-source";
import { startGateway } from "./gateway";
import { Org } from "./entities/Org";
import { User } from "./entities/User";
import { Post } from "./entities/Post";
import { CreateUserSchema, UpdateUserSchema, CreatePostSchema, formatIssues } from "./schema";
import type { Page } from "./types";

/** CREATE — validate untrusted input, then persist. */
async function create(ds: DataSource): Promise<void> {
  const orgs = ds.getRepository(Org);
  const users = ds.getRepository(User);
  const posts = ds.getRepository(Post);

  const acme = await orgs.save(orgs.create({ name: "Acme" }));

  // `.parse` throws on bad input; the result has defaults applied and is exactly
  // the shape the entity expects (note the name is trimmed by the schema).
  const input = CreateUserSchema.parse({
    email: "alice@example.com",
    name: "  Alice  ",
    settings: { theme: "dark" },
    orgId: acme.id,
  });
  const alice = await users.save(
    users.create({ email: input.email, name: input.name, settings: input.settings, org: acme }),
  );
  console.log("CREATE user:", alice.id, alice.name, JSON.stringify(alice.settings));

  // Invalid input is rejected *before* it touches the database.
  const bad = CreateUserSchema.safeParse({ email: "not-an-email", name: "" });
  console.log("CREATE rejected invalid input:", !bad.success, bad.success ? "" : `-> ${formatIssues(bad.error)}`);

  // CREATE a related row. `authorId` is validated, but the entity links the
  // author by relation, so drop it and set `author` instead.
  const { authorId, ...postFields } = CreatePostSchema.parse({
    title: "Postgres on Iroh",
    body: "Wire-compatible, replicated, local-first.",
    meta: { tags: ["sql", "p2p"] },
    published: true,
    authorId: alice.id,
  });
  void authorId;
  await posts.save(posts.create({ ...postFields, author: alice }));
  // A second, unpublished post so the read/delete steps have something to filter.
  await posts.save(
    posts.create({ title: "Draft notes", body: null, meta: { tags: [] }, published: false, author: alice }),
  );
  console.log("CREATE posts: 2 (1 published, 1 draft)");
}

/** READ — by unique column, with relations, and a paginated QueryBuilder. */
async function read(ds: DataSource): Promise<void> {
  const users = ds.getRepository(User);

  const alice = await users.findOneByOrFail({ email: "alice@example.com" });
  console.log("READ findOneBy:", alice.name);

  const withRels = await users.findOne({
    where: { id: alice.id },
    relations: { org: true, posts: true },
  });
  console.log("READ relations: org =", withRels?.org?.name, "posts =", withRels?.posts.length);

  const page = await readPostsPage(ds, { page: 1, pageSize: 10, publishedOnly: true });
  console.log(
    `READ page ${page.page}: ${page.items.length}/${page.total} published ->`,
    page.items.map((p) => p.title).join(", "),
  );
}

/** A reusable paginated read built with the QueryBuilder. */
async function readPostsPage(
  ds: DataSource,
  opts: { page: number; pageSize: number; publishedOnly?: boolean },
): Promise<Page<Post>> {
  const qb = ds.getRepository(Post).createQueryBuilder("p").innerJoinAndSelect("p.author", "u");
  if (opts.publishedOnly) qb.where("p.published = :pub", { pub: true });
  const [items, total] = await qb
    .orderBy("p.title", "ASC")
    .skip((opts.page - 1) * opts.pageSize)
    .take(opts.pageSize)
    .getManyAndCount();
  return { items, total, page: opts.page, pageSize: opts.pageSize };
}

/** UPDATE — validate a partial patch, then apply it. */
async function update(ds: DataSource): Promise<void> {
  const users = ds.getRepository(User);

  const patch = UpdateUserSchema.parse({ name: "Alice Cooper", settings: { theme: "light" } });
  await users.update({ email: "alice@example.com" }, patch);
  const updated = await users.findOneByOrFail({ email: "alice@example.com" });
  console.log("UPDATE user:", updated.name, JSON.stringify(updated.settings));
}

/** DELETE — by condition (reporting affected rows), then a row + its children. */
async function remove(ds: DataSource): Promise<void> {
  const users = ds.getRepository(User);
  const posts = ds.getRepository(Post);

  const before = await posts.count();
  const res = await posts.delete({ published: false });
  console.log(`DELETE unpublished posts: removed ${res.affected ?? 0} (${before} -> ${await posts.count()})`);

  // Remove the posts that reference Alice, then Alice herself.
  const alice = await users.findOneByOrFail({ email: "alice@example.com" });
  await posts.createQueryBuilder().delete().where("authorId = :id", { id: alice.id }).execute();
  await users.delete({ id: alice.id });
  console.log("DELETE user; remaining users:", await users.count());
}

async function main() {
  const gw = await startGateway();
  console.log(`gateway ready on 127.0.0.1:${gw.port}\n`);
  const ds = new DataSource(options({ port: gw.port, synchronize: true, migrations: [] }));
  try {
    await ds.initialize();
    await create(ds);
    await read(ds);
    await update(ds);
    await remove(ds);
    console.log("\nCRUD walkthrough complete ✅");
  } finally {
    await ds.destroy().catch(() => {});
    gw.stop();
  }
}

main().catch((e) => {
  console.error("CRUD example failed:", e);
  process.exitCode = 1;
});
