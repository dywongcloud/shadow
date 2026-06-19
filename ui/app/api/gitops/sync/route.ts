import { NextRequest, NextResponse } from "next/server";
import { composioConfigured, githubStatus, commitFiles, resolveEntity } from "@/lib/composio";
import { backend, buildOrgArtifacts } from "@/lib/gitops-server";

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
  if (!composioConfigured()) return NextResponse.json({ skipped: true, reason: "composio-not-configured" });
  const team = (await req.json().catch(() => ({})))?.team || req.headers.get("x-hive-team") || "personal";
  const entity = await resolveEntity();
  const status = await githubStatus(entity);
  if (!status.connected) return NextResponse.json({ skipped: true, reason: "github-not-connected" });

  const linkRes = await backend("/v1/gitops", team);
  const link = linkRes.ok ? await linkRes.json() : null;
  if (!link?.repo || !link.repo.includes("/")) {
    return NextResponse.json({ skipped: true, reason: "no-config-repo" });
  }
  const [owner, repo] = String(link.repo).split("/");
  const branch = link.branch || "main";
  const path = link.path || "openedge.yaml";

  const { files, hash, projectCount } = await buildOrgArtifacts(team, path);
  if (hash === link.last_hash) {
    return NextResponse.json({ unchanged: true, repo: link.repo, files: files.length });
  }

  const result = await commitFiles(entity, {
    owner, repo, branch,
    message: `chore(openedge): sync ${projectCount} project(s) — ${files.length} artifact(s)`,
    files,
  });
  if (!result.ok) return NextResponse.json({ ok: false, error: result.error }, { status: 502 });

  await backend("/v1/gitops/synced", team, {
    method: "POST",
    body: JSON.stringify({ commit: result.commit || "", hash }),
  }).catch(() => {});

  return NextResponse.json({
    ok: true, repo: link.repo, branch, commit: result.commit, files: files.length, projects: projectCount,
  });
}
