/**
 * A local, dependency-free implementation of the Lakebed `lakebed/server`
 * surface for shadw deployments.
 *
 * The capsule in `server/index.mjs` is written against these exact exports. On
 * Lakebed's hosted runtime the same capsule imports `lakebed/server` instead;
 * on shadw the capsule is executed by `server/serve.mjs`, which supplies `ctx.db`
 * and `ctx.storage` from `server/shadow.mjs` (one binding to shadw storage).
 *
 * Only the declarative schema/query/mutation/endpoint shape is implemented —
 * enough to define and execute a capsule. There is no source-runtime VM, no
 * clientBundle and no `npx lakebed deploy` here. See `docs/lakebed.md`.
 */

/** Lakebed scalars chain `.default(v)` and `.optional()` in either order. */
function scalar(kind) {
  const rule = {
    kind,
    isOptional: false,
    defaultValue: undefined,
    default(value) {
      rule.defaultValue = value;
      return rule;
    },
    optional() {
      rule.isOptional = true;
      return rule;
    },
  };
  return rule;
}

export const string = () => scalar("string");
export const boolean = () => scalar("boolean");
export const number = () => scalar("number");
export const id = (table) => ({ kind: "id", table, isOptional: false, defaultValue: undefined });
export const userId = () => scalar("userId");

export function table(fields) {
  const indexes = [];
  return {
    kind: "table",
    fields,
    indexes,
    index(name, columns) {
      indexes.push({ name, columns: [...columns] });
      return this;
    },
  };
}

export const query = (handler) => ({ kind: "query", handler });
export const mutation = (handler) => ({ kind: "mutation", handler });
export const action = (handler) => ({ kind: "action", handler });

export function endpoint(spec, handler) {
  return { kind: "endpoint", method: spec.method, path: spec.path, readOnly: !!spec.readOnly, handler };
}

export function json(value, init = {}) {
  return { kind: "response", body: JSON.stringify(value), status: init.status || 200, contentType: "application/json" };
}

export function text(value, init = {}) {
  return { kind: "response", body: String(value), status: init.status || 200, contentType: "text/plain; charset=utf-8" };
}

export function empty(init = {}) {
  return { kind: "response", body: "", status: init.status || 204, contentType: "text/plain" };
}

export function redirect(to, init = {}) {
  return { kind: "response", body: "", status: init.status || 302, contentType: "text/plain", location: to };
}

export function capsule(definition) {
  const { schema = {}, queries = {}, mutations = {}, actions = {}, endpoints = {}, auth = {} } = definition;
  return { kind: "capsule", schema, queries, mutations, actions, endpoints, auth };
}
