/**
 * Todo capsule — Lakebed's documented `capsule` shape, executed on shadw.
 *
 * The ONLY difference from a Lakebed-hosted capsule is the import: a hosted
 * project writes `from "lakebed/server"`. Here `./lakebed.mjs` is the same
 * declarative surface (schema builders + query/mutation/endpoint wrappers),
 * and `serve.mjs` supplies `ctx.db` / `ctx.storage` from one cohesive binding
 * to shadw storage (`./shadow.mjs`). The capsule body below is portable.
 */
import {
  boolean, capsule, endpoint, json, mutation, query, string, table, text,
} from "./lakebed.mjs";

export default capsule({
  auth: { requireSignIn: false },
  schema: {
    todos: table({
      text: string(),
      done: boolean().default(false),
      ownerId: string(),
    })
      .index("by_owner", ["ownerId"])
      .index("by_done", ["done"]),
  },
  queries: {
    /** Bounded, measured read: no invented rows, no fabricated ceiling. */
    list: query(async (ctx, args) => {
      const ownerId = typeof args?.ownerId === "string" && args.ownerId.length <= 128
        ? args.ownerId
        : "anonymous";
      return ctx.db.todos
        .withIndex("by_owner", (q) => q.eq("ownerId", ownerId))
        .order("desc")
        .take(50)
        .collect();
    }),
    count: query(async (ctx, args) => {
      const ownerId = typeof args?.ownerId === "string" && args.ownerId.length <= 128
        ? args.ownerId
        : "anonymous";
      return ctx.db.todos.withIndex("by_owner", (q) => q.eq("ownerId", ownerId)).count();
    }),
  },
  mutations: {
    add: mutation(async (ctx, args) => {
      const text = typeof args?.text === "string" ? args.text.trim() : "";
      if (!text || text.length > 280 || text.includes("\0")) {
        throw new Error("text must be 1..280 characters");
      }
      const ownerId = typeof args?.ownerId === "string" && args.ownerId.length <= 128
        ? args.ownerId
        : "anonymous";
      return { id: await ctx.db.todos.insert({ text, done: false, ownerId }) };
    }),
    toggle: mutation(async (ctx, args) => {
      const id = args?.id;
      if (typeof id !== "string" || !/^[a-f0-9]{64}$/.test(id)) throw new Error("invalid id");
      const row = await ctx.db.todos.get(id);
      if (!row) return null;
      return ctx.db.todos.update(id, { done: !row.done });
    }),
    remove: mutation(async (ctx, args) => {
      const id = args?.id;
      if (typeof id !== "string" || !/^[a-f0-9]{64}$/.test(id)) throw new Error("invalid id");
      return { deleted: await ctx.db.todos.delete(id) };
    }),
  },
  endpoints: {
    /** Storage round trip through the same binding, never a direct client URL. */
    upload: endpoint({ method: "POST", path: "/api/files", readOnly: false }, async (ctx, req) => {
      const body = req.body;
      if (typeof body !== "string" || body.length === 0 || body.length > 5 * 1024 * 1024) {
        return text("empty or oversized body", { status: 400 });
      }
      const object = await ctx.storage.upload(body, { public: false, contentType: "text/plain" });
      return json({ key: object.key, size: object.size, url: object.url });
    }),
    read: endpoint({ method: "GET", path: "/api/files/:key", readOnly: true }, async (ctx, req) => {
      const key = req.params?.key;
      const bytes = await ctx.storage.get(key);
      if (bytes === null) return text("not found", { status: 404 });
      return { kind: "response", body: new TextDecoder().decode(bytes), status: 200, contentType: "text/plain" };
    }),
  },
});
