"use client";

// Browser-node target resolution: the ONE place that turns the replicated descriptor metadata (`Deployment.browser_functions`,
// `Deployment.browser_db`) into the picker's selectable targets and revalidates a persisted selection against it, so a node is only ever
// started with a digest re-derived from CURRENT metadata (a redeploy rotates `policy_digest`). A target is OPTIONAL: `null` means "serve
// nothing", never an error, and this module never gates Start.

import { apiGet, BROWSER_DB_MAX_BYTES_DEFAULT, BROWSER_DB_VALUE_MAX_BYTES_DEFAULT, type Deployment } from "./api";

/** One selectable serve target: a ready deployment's browser-eligible function plus the limits its build policy stamped; the digest is the
 *  descriptor's policy digest, which the donor never sees or supplies. */
export interface BrowserFunctionTarget {
  kind: "function";
  deployment: string;
  fn: string;
  project: string;
  policyDigest: string;
  mode: string;
  timeoutMs: number;
  memoryBytes: number;
  stackBytes: number;
  sourceBytes: number;
  allowedOps: number[];
}

/** A DATABASE-only target: a ready deployment with a `browser_db` block but no browser-eligible function, so the node replicates that project's
 *  CRR replica and serves nothing. `fn`/`policyDigest` are empty by construction (the backend refuses to route a serve target without a function). */
export interface BrowserDatabaseTarget {
  kind: "database";
  deployment: string;
  fn: "";
  project: string;
  policyDigest: "";
  maxBytes: number;
  maxValueBytes: number;
  publicRead: boolean;
  tables: string[];
}

export type RunNodeTarget = BrowserFunctionTarget | BrowserDatabaseTarget;

/** Kept as the historical name — the serve-target shape other modules import. */
export type BrowserTarget = BrowserFunctionTarget;

export type ResolveResult = { ok: true; target: RunNodeTarget } | { ok: false; reason: string };

function functionTarget(d: Deployment, f: NonNullable<Deployment["browser_functions"]>[number]): BrowserFunctionTarget {
  return {
    kind: "function",
    deployment: d.id,
    fn: f.name,
    project: d.project,
    policyDigest: f.policy_digest,
    mode: f.mode,
    timeoutMs: f.timeout_ms,
    memoryBytes: f.memory_bytes,
    stackBytes: f.stack_bytes,
    sourceBytes: f.source_bytes,
    allowedOps: f.allowed_ops,
  };
}

function databaseTarget(d: Deployment): BrowserDatabaseTarget | null {
  const db = d.browser_db;
  if (!db) return null;
  return {
    kind: "database",
    deployment: d.id,
    fn: "",
    project: d.project,
    policyDigest: "",
    maxBytes: db.max_bytes || BROWSER_DB_MAX_BYTES_DEFAULT,
    maxValueBytes: db.max_value_bytes || BROWSER_DB_VALUE_MAX_BYTES_DEFAULT,
    publicRead: !!db.public_read,
    tables: (db.schema ?? []).map((t) => t.name),
  };
}

/** Every target a donor may attach to: a browser-eligible function of a ready deployment (serve, plus its database if the project opted in)
 *  and a ready deployment with a `browser_db` block but NO eligible function (database replication only). A deployment with both contributes
 *  only its function targets; an empty list is not a dead end. */
export function targetsFromDeployments(deployments: Deployment[]): RunNodeTarget[] {
  const out: RunNodeTarget[] = [];
  for (const d of deployments) {
    if (d.state !== "ready") continue;
    const fns = d.browser_functions ?? [];
    for (const f of fns) out.push(functionTarget(d, f));
    if (fns.length === 0) {
      const db = databaseTarget(d);
      if (db) out.push(db);
    }
  }
  return out;
}

/** One deployment contributing NO selectable target, with the reason WHY. */
export interface ExcludedDeployment {
  deployment: string;
  project: string;
  /** One sentence naming the cause: build-stamped where the fleet knows it (`Deployment.browser_ineligible`), derived here only for not-ready
   *  and never-evaluated. */
  reason: string;
}

/** Collapse a build-stamped reason (which can be multi-line — `bundle()` joins
 *  its rejection list with "\n  - ") into one bounded footnote line. */
function oneLine(text: string, max = 220): string {
  const flat = text.replace(/\s+/g, " ").trim();
  return flat.length > max ? `${flat.slice(0, max - 1)}…` : flat;
}

/** Why a deployment offers nothing to attach to; the one case the fleet cannot stamp is a deployment with neither an artifact nor a reason,
 *  built before browser eligibility was evaluated at all: it needs a redeploy, not a code change. */
function excludedReason(d: Deployment): string {
  if (d.state !== "ready") {
    return `deployment is ${d.state} — it can only be attached once it's ready`;
  }
  const stamped = d.browser_ineligible ?? [];
  if (stamped.length > 0) {
    return oneLine(stamped.map((x) => `${x.function}: ${x.reason}`).join(" · "));
  }
  if ((d.functions ?? []).length === 0) {
    return "static deployment — it has no functions to run";
  }
  return "built before browser eligibility was evaluated — redeploy and its functions are checked automatically";
}

/** Deployments contributing NO selectable target (not ready, or ready with neither a browser-eligible function nor a `browser_db` block),
 *  each with its reason. */
export function excludedDeployments(deployments: Deployment[]): ExcludedDeployment[] {
  return deployments
    .filter((d) => d.state !== "ready" || ((d.browser_functions ?? []).length === 0 && !d.browser_db))
    .map((d) => ({ deployment: d.id, project: d.project, reason: excludedReason(d) }));
}

/** Count-only view of [`excludedDeployments`], for callers that only need the
 *  number. Derived from the same predicate so the two can never disagree. */
export function excludedDeploymentCount(deployments: Deployment[]): number {
  return excludedDeployments(deployments).length;
}

/** Revalidates a persisted selection against a FRESH deployment list with distinct failure reasons (deleted, not ready, function no longer
 *  eligible, database block removed); a policy-digest ROTATION under the same deployment+function resolves silently to the new digest, and an
 *  empty `fn` selects the deployment's DATABASE target. */
export function resolveTarget(deployments: Deployment[], deployment: string, fn: string): ResolveResult {
  const dep = deployments.find((d) => d.id === deployment);
  if (!dep) {
    return {
      ok: false,
      reason: `Deployment ${deployment} is gone — it was deleted, or it belongs to a different team than the one you're viewing.`,
    };
  }
  if (dep.state !== "ready") {
    return {
      ok: false,
      reason: `Deployment ${dep.project} (${deployment}) is ${dep.state} — it isn't serving until it returns to ready.`,
    };
  }
  if (fn === "") {
    const target = databaseTarget(dep);
    if (!target) {
      return {
        ok: false,
        reason: `${dep.project} (${deployment}) no longer has a browser database — its fluid.json \`browser_db\` block was removed, or the latest build dropped it.`,
      };
    }
    return { ok: true, target };
  }
  const f = (dep.browser_functions ?? []).find((b) => b.name === fn);
  if (!f) {
    return {
      ok: false,
      reason: `Function "${fn}" on ${dep.project} (${deployment}) is no longer browser-eligible — it was removed or its browser opt-in didn't survive the latest build.`,
    };
  }
  return { ok: true, target: functionTarget(dep, f) };
}

/** Fresh (cache-bypassing) fetch + resolve for the owner-handoff replay, so the digest comes from what the fleet believes NOW; throws on
 *  fetch failure so the caller leaves the persisted start alone and retries rather than clearing a selection that may still be valid. */
export async function resolveTargetFresh(deployment: string, fn: string): Promise<ResolveResult> {
  const deployments = await apiGet<Deployment[]>("/deployments", { fresh: true });
  return resolveTarget(deployments ?? [], deployment, fn);
}
