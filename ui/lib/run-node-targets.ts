"use client";

// Browser-node target resolution (browser-run-node-target-picker): the ONE
// place that turns the replicated deployment descriptor metadata
// (`Deployment.browser_functions`) into the picker's eligible-target list and
// revalidates a persisted deployment+function selection against it. Shared by
// the /run-node page (picker rendering + click-time resolution) and
// use-run-node's owner-handoff replay — a selection is only ever started with
// a digest re-derived from CURRENT metadata, so a target rotation (redeploy →
// new policy_digest under the same deployment+function) is picked up instead
// of replaying a stale one.

import { apiGet, type Deployment } from "./api";

/** One selectable serve target: a ready deployment's browser-eligible
 *  function plus the limits its build policy stamped. The digest is the
 *  descriptor's policy digest — the donor never sees or supplies it. */
export interface BrowserTarget {
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

export type ResolveResult = { ok: true; target: BrowserTarget } | { ok: false; reason: string };

/** Eligible = deployment is ready AND carries at least one browser-eligible
 *  function descriptor. Anything else (queued/building/error, or no browser
 *  opt-in surviving the build) is never listed. */
export function targetsFromDeployments(deployments: Deployment[]): BrowserTarget[] {
  const out: BrowserTarget[] = [];
  for (const d of deployments) {
    if (d.state !== "ready") continue;
    for (const f of d.browser_functions ?? []) {
      out.push({
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
      });
    }
  }
  return out;
}

/** Revalidate a persisted selection against a FRESH deployment list. Distinct
 *  failure reasons so the UI can say exactly why a previously-working
 *  selection no longer resolves (deleted deployment, no-longer-ready
 *  deployment, function no longer browser-eligible) instead of a generic
 *  "not found". A policy-digest ROTATION under the same deployment+function
 *  is not an error — it resolves to the new digest silently. */
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
  const f = (dep.browser_functions ?? []).find((b) => b.name === fn);
  if (!f) {
    return {
      ok: false,
      reason: `Function "${fn}" on ${dep.project} (${deployment}) is no longer browser-eligible — it was removed or its browser opt-in didn't survive the latest build.`,
    };
  }
  return {
    ok: true,
    target: {
      deployment: dep.id,
      fn: f.name,
      project: dep.project,
      policyDigest: f.policy_digest,
      mode: f.mode,
      timeoutMs: f.timeout_ms,
      memoryBytes: f.memory_bytes,
      stackBytes: f.stack_bytes,
      sourceBytes: f.source_bytes,
      allowedOps: f.allowed_ops,
    },
  };
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
