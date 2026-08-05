"use client";

// Browser-node target resolution (browser-run-node-target-picker): the ONE
// place that turns the replicated deployment descriptor metadata
// (`Deployment.browser_functions`, `Deployment.browser_db`) into the picker's
// selectable-target list and revalidates a persisted selection against it.
// Shared by the /run-node page (picker rendering + click-time resolution) and
// use-run-node's owner-handoff replay — a selection is only ever started with
// a digest re-derived from CURRENT metadata, so a target rotation (redeploy →
// new policy_digest under the same deployment+function) is picked up instead
// of replaying a stale one.
//
// browser-node-optional-serve-target: a target is OPTIONAL. Running a node and
// having something browser-servable are independent facts — a donor whose
// deployments are all long-running servers (Next.js, Express) or containers
// still joins the mesh, holds a relay identity, publishes presence on the
// constellation, and (with a `browser_db` block) replicates that database.
// `null` is therefore a first-class selection meaning "serve nothing", not an
// error state, and this module never gates Start.

import { apiGet, BROWSER_DB_MAX_BYTES_DEFAULT, BROWSER_DB_VALUE_MAX_BYTES_DEFAULT, type Deployment } from "./api";

/** One selectable serve target: a ready deployment's browser-eligible
 *  function plus the limits its build policy stamped. The digest is the
 *  descriptor's policy digest — the donor never sees or supplies it. */
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

/** A DATABASE-only target: a ready deployment carrying a `browser_db` block
 *  but no browser-eligible function. The node replicates that project's CRR
 *  replica and serves nothing — which is exactly what a tenant whose apps are
 *  all servers can still contribute. `fn`/`policyDigest` are empty by
 *  construction, and the backend refuses to route a serve target for an
 *  admission with no function (`BrowserAdmission::serving`, plus
 *  `fluid_gateway::upsert_browser_target`'s own independent rejection of an
 *  empty deployment/function/digest). */
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
    // 0 means "platform default" on the wire (fluid_core::BrowserDbPolicy) —
    // resolved here only for DISPLAY; the fleet resolves its own copy and the
    // capability the node actually receives is server-derived either way.
    maxBytes: db.max_bytes || BROWSER_DB_MAX_BYTES_DEFAULT,
    maxValueBytes: db.max_value_bytes || BROWSER_DB_VALUE_MAX_BYTES_DEFAULT,
    publicRead: !!db.public_read,
    tables: (db.schema ?? []).map((t) => t.name),
  };
}

/** Every target a donor may attach this node to, in one list:
 *
 *  * a browser-eligible function of a ready deployment (serve +, if the
 *    project opted in, its database), and
 *  * a ready deployment that carries a `browser_db` block but NO
 *    browser-eligible function — database replication only.
 *
 *  A deployment with both contributes only its function targets: selecting one
 *  already carries the same database grant, so a separate database entry would
 *  be a duplicate of a capability the donor already has.
 *
 *  An empty list is NOT a dead end (browser-node-optional-serve-target) — the
 *  node starts with no target at all and contributes everything that does not
 *  require one. */
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
  /** One sentence naming the cause — build-stamped where the fleet knows it
   *  (`Deployment.browser_ineligible`), derived here only for the two states
   *  the fleet cannot stamp: not-ready, and never-evaluated. */
  reason: string;
}

/** Collapse a build-stamped reason (which can be multi-line — `bundle()` joins
 *  its rejection list with "\n  - ") into one bounded footnote line. */
function oneLine(text: string, max = 220): string {
  const flat = text.replace(/\s+/g, " ").trim();
  return flat.length > max ? `${flat.slice(0, max - 1)}…` : flat;
}

/** Why this deployment offers nothing to attach to.
 *
 *  The ONE case the fleet cannot stamp at build time is the one that explains
 *  most confused reports: a deployment carrying neither an artifact NOR a
 *  reason was built before the pipeline evaluated browser eligibility at all,
 *  so nothing is wrong with the code — it just needs a redeploy. Reporting
 *  that as "not browser-eligible" (the previous blanket copy) actively sends
 *  the reader to debug a handler that was never examined. */
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

/** Deployments contributing NO selectable target — not ready, or ready with
 *  neither a browser-eligible function nor a `browser_db` block — each with the
 *  reason. A bare count made a tenant's 14 deployments one undifferentiated
 *  blob whose single blanket explanation was wrong for most of them; the reason
 *  is what turns "my opted-in function isn't listed" into an actionable line. */
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

/** Revalidate a persisted selection against a FRESH deployment list. Distinct
 *  failure reasons so the UI can say exactly why a previously-working
 *  selection no longer resolves (deleted deployment, no-longer-ready
 *  deployment, function no longer browser-eligible, database block removed)
 *  instead of a generic "not found". A policy-digest ROTATION under the same
 *  deployment+function is not an error — it resolves to the new digest
 *  silently. An empty `fn` selects the deployment's DATABASE target. */
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

/** Fresh (cache-bypassing) fetch + resolve in one step, for the owner-handoff
 *  replay path: the whole point is that the digest comes from what the fleet
 *  believes NOW, not from anything persisted. Throws on fetch failure so the
 *  caller can leave the persisted start alone and retry on the next handoff
 *  rather than clearing a selection that may still be valid. */
export async function resolveTargetFresh(deployment: string, fn: string): Promise<ResolveResult> {
  const deployments = await apiGet<Deployment[]>("/deployments", { fresh: true });
  return resolveTarget(deployments ?? [], deployment, fn);
}
