import { NextRequest, NextResponse } from "next/server";
import { githubConfigured, githubStatus, commitFiles, resolveEntity } from "@/lib/github";
import { authTokenFrom, backend, buildOrgArtifacts } from "@/lib/gitops-server";

export const dynamic = "force-dynamic";

/**
 * Regenerate the org's full GitOps artifact tree and commit it to the linked repo
 * as a single atomic commit (`git add .`-style). Idempotent: if the generated
 * tree is byte-identical to the last push (content hash) it skips the commit.
 *
 * Called on link, and periodically by the dashboard so any settings / runtime /
 * region / tier change reflects in the committed config automatically.
 */
export async function POST(req: NextRequest) {
  if (!githubConfigured()) return NextResponse.json({ skipped: true, reason: "github-not-configured" });
  const team = (await req.json().catch(() => ({})))?.team || req.headers.get("x-hive-team") || "personal";
  const entity = await resolveEntity();
  const status = await githubStatus(entity);
  if (!status.connected) return NextResponse.json({ skipped: true, reason: "github-not-connected" });

  const authToken = authTokenFrom(req);
  const linkRes = await backend("/v1/gitops", team, undefined, authToken);
  const link = linkRes.ok ? await linkRes.json() : null;
  if (!link?.repo || !link.repo.includes("/")) {
    return NextResponse.json({ skipped: true, reason: "no-config-repo" });
  }
  const [owner, repo] = String(link.repo).split("/");
  const branch = link.branch || "main";
  const path = link.path || "openedge.yaml";

  const { files, hash, projectCount } = await buildOrgArtifacts(team, path, authToken);
  if (hash === link.last_hash) {
    return NextResponse.json({ unchanged: true, repo: link.repo, files: files.length });
  }

  const result = await commitFiles(entity, {
    owner, repo, branch,
    message: `chore(openedge): sync ${projectCount} project(s) — ${files.length} artifact(s)`,
    files,
    // A project deleted since the last sync must stop appearing in the repo,
    // not just stop appearing in `files` — see commitFiles' doc comment.
    managedPrefixes: ["projects/"],
  });
  if (!result.ok) return NextResponse.json({ ok: false, error: result.error }, { status: 502 });

  // commitFiles()'s catch-all per-file fallback has no batch-tree-delete equivalent,
  // so when it ran AND this sync had (or may have had) pending tombstone deletions,
  // the repo may still hold stale managed files even though the content commit
  // succeeded. In that case do NOT persist last_hash — `hash` is derived purely from
  // DESIRED file content, so persisting it here would make the next sync recompute
  // the identical hash and short-circuit as `{unchanged:true}` above, masking the
  // skipped deletion forever (until an unrelated content change happens to bump the
  // hash). Skipping the persist instead guarantees the next sync attempt retries the
  // atomic path — and its deletion reconciliation — rather than silently short-
  // circuiting.
  if (!result.deletionsSkipped) {
    await backend("/v1/gitops/synced", team, {
      method: "POST",
      body: JSON.stringify({ commit: result.commit || "", hash }),
    }, authToken).catch(() => {});
  }

  return NextResponse.json({
    ok: true, repo: link.repo, branch, commit: result.commit, files: files.length, projects: projectCount,
    ...(result.deletionsSkipped ? { deletionsSkipped: true } : {}),
  });
}
