import { NextRequest, NextResponse } from "next/server";
import { githubConfigured, githubStatus, createRepo, commitFiles, randomConfigRepoName, setRepoVariable, resolveEntity } from "@/lib/github";
import { backend, buildOrgArtifacts } from "@/lib/gitops-server";

export const dynamic = "force-dynamic";

/**
 * Set up GitOps for a tenant: optionally CREATE the GitHub config repo itself,
 * link it, then auto-generate the full spec/config/meta artifact tree and commit
 * + push it (`git add .`-style). One call does repo creation → scaffold → push.
 *
 * Body: {
 *   team, scope: "personal"|"org",
 *   create: boolean,           // create a new repo vs. use an existing one
 *   name?: string,             // new repo name (when create)
 *   org?: string,              // GitHub org login (when scope==="org")
 *   isPrivate?: boolean,
 *   repo?: string,             // existing "owner/name" (when !create)
 *   branch?: string,
 * }
 */
export async function POST(req: NextRequest) {
  if (!githubConfigured()) {
    return NextResponse.json({ ok: false, error: "GitHub is not configured." }, { status: 400 });
  }
  const body = await req.json().catch(() => ({} as any));
  const team = body?.team || req.headers.get("x-hive-team") || "personal";
  const scope = body?.scope === "org" ? "org" : "personal";
  const entity = await resolveEntity();
  const status = await githubStatus(entity);
  if (!status.connected) return NextResponse.json({ ok: false, error: "GitHub not connected." }, { status: 400 });

  // 1) Resolve / create the repo.
  let fullName: string;
  let branch = (body?.branch && String(body.branch)) || "main";
  let created = false;
  if (body?.create) {
    // ALWAYS a fresh, randomly-named config repo — never an existing one, so we
    // can't clobber a real source repo. Retry with a new random name on the rare
    // chance of a collision.
    const org = scope === "org" ? String(body?.org || "").trim() || undefined : undefined;
    const isPrivate = body?.isPrivate !== false;
    let made: Awaited<ReturnType<typeof createRepo>> | null = null;
    for (let attempt = 0; attempt < 4; attempt++) {
      const r = await createRepo(entity, { name: randomConfigRepoName(), org, isPrivate });
      if (r.ok && r.full_name) { made = r; break; }
      if (!r.conflict) {
        // Forward the org OAuth-restriction approval URL so the UI can render an
        // actionable "approve this app" link instead of raw GitHub JSON.
        return NextResponse.json(
          { ok: false, error: r.error || "Failed to create repository.", approve_url: r.approve_url, restricted: r.restricted },
          { status: r.restricted ? 403 : 502 }
        );
      }
      // conflict → loop and try another random name
    }
    if (!made?.full_name) {
      return NextResponse.json({ ok: false, error: "Could not allocate a config repository name." }, { status: 502 });
    }
    fullName = made.full_name;
    branch = made.default_branch || branch;
    created = true;
  } else {
    fullName = String(body?.repo || "");
    if (!fullName.includes("/")) {
      return NextResponse.json({ ok: false, error: "Select or name a repository." }, { status: 400 });
    }
  }
  const [owner, repo] = fullName.split("/");
  const path = "openedge.yaml";

  // 2) Link the repo to this tenant.
  await backend("/v1/gitops", team, {
    method: "PUT",
    body: JSON.stringify({ repo: fullName, branch, path, scope }),
  });

  // 3) Scaffold + push the full artifact tree as one commit.
  const { files, hash, projectCount } = await buildOrgArtifacts(team, path);
  const result = await commitFiles(entity, {
    owner, repo, branch,
    message: created
      ? `feat(openedge): initialize GitOps config (${files.length} artifacts)`
      : `chore(openedge): sync GitOps config (${files.length} artifacts)`,
    files,
  });
  if (!result.ok) {
    // The repo is still linked; report so the UI can show the error + retry sync.
    return NextResponse.json({ ok: false, repo: fullName, created, error: result.error }, { status: 502 });
  }

  await backend("/v1/gitops/synced", team, {
    method: "POST",
    body: JSON.stringify({ commit: result.commit || "", hash }),
  }).catch(() => {});

  // Auto-set the Actions variable so the committed workflow can reach the node.
  const webhookUrl = (process.env.OPENEDGE_WEBHOOK_URL || "").replace(/\/$/, "");
  let variableSet = false;
  if (webhookUrl) {
    const v = await setRepoVariable(entity, owner, repo, "OPENEDGE_WEBHOOK_URL", webhookUrl);
    variableSet = v.ok;
  }

  return NextResponse.json({
    ok: true,
    repo: fullName,
    url: `https://github.com/${fullName}`,
    branch,
    created,
    commit: result.commit,
    files: files.map((f) => f.path),
    projects: projectCount,
    variableSet,
  });
}
