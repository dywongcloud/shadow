import { NextRequest, NextResponse } from "next/server";
import { githubConfigured, githubStatus, commitFiles, resolveEntity } from "@/lib/github";
import { authTokenFrom, backend, buildOrgArtifacts } from "@/lib/gitops-server";

/** Regenerates the org's full GitOps artifact tree and commits it to the linked repo as one atomic commit, skipping when the tree hash equals the
 *  last push. Outcome contract (a linked tenant never sees a silent no-op): `{skipped}` 200 = GitOps genuinely not set up; `{unchanged}` 200 = the hash
 *  matches; `{ok:true}` 200 = committed; `{ok:false,error}` 502 = a repo IS linked but the write could not run (no working credential), failed, or the
 *  platform state the tree is built from could not be read. */
export async function POST(req: NextRequest) {
  if (!githubConfigured()) return NextResponse.json({ skipped: true, reason: "github-not-configured" });
  const team = (await req.json().catch(() => ({})))?.team || req.headers.get("x-hive-team") || "personal";
  const authToken = authTokenFrom(req);

  const linkRes = await backend("/v1/gitops", team, authToken);
  const link = linkRes.ok ? await linkRes.json() : null;
  if (!link?.repo || !link.repo.includes("/")) {
    return NextResponse.json({ skipped: true, reason: "no-config-repo" });
  }
  const [owner, repo] = String(link.repo).split("/");
  const branch = link.branch || "main";
  const path = link.path || "openedge.yaml";

  const entity = await resolveEntity();
  const status = await githubStatus(entity);
  if (!status.connected) {
    return NextResponse.json(
      { ok: false, error: "GitHub is not connected — the linked config repo cannot be synced. Reconnect GitHub (Integrations) to resume GitOps mirroring." },
      { status: 502 }
    );
  }

  const { files, hash, projectCount, failures } = await buildOrgArtifacts(team, authToken, path);
  if (failures.length) {
    return NextResponse.json(
      { ok: false, error: `platform state unreadable (${failures.join(", ")}) — refusing to commit a partial config tree` },
      { status: 502 }
    );
  }
  if (hash === link.last_hash) {
    return NextResponse.json({ unchanged: true, repo: link.repo, files: files.length });
  }

  const result = await commitFiles(entity, {
    owner, repo, branch,
    message: `chore(openedge): sync ${projectCount} project(s) — ${files.length} artifact(s)`,
    files,
    managedPrefixes: ["projects/"],
  });
  if (!result.ok) return NextResponse.json({ ok: false, error: result.error }, { status: 502 });

  if (!result.deletionsSkipped) {
    await backend("/v1/gitops/synced", team, authToken, {
      method: "POST",
      body: JSON.stringify({ commit: result.commit || "", hash }),
    }).catch(() => {});
  }

  return NextResponse.json({
    ok: true, repo: link.repo, branch, commit: result.commit, files: files.length, projects: projectCount,
    ...(result.deletionsSkipped ? { deletionsSkipped: true } : {}),
  });
}
