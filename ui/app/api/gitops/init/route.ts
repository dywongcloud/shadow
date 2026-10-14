import { NextRequest, NextResponse } from "next/server";
import { githubConfigured, githubStatus, createRepo, commitFiles, randomConfigRepoName, setRepoVariable, resolveEntity } from "@/lib/github";
import { authTokenFrom, backend, buildOrgArtifacts, claimedRole } from "@/lib/gitops-server";
import { can, roleRefusalFor, roleRefusalSentence } from "@/lib/team-role";

interface GitopsInitBody {
  team?: string;
  scope?: "personal" | "org";
  create?: boolean;
  org?: string;
  isPrivate?: boolean;
  repo?: string;
  branch?: string;
}

const MAX_REPO_NAME_ATTEMPTS = 4;

/** Sets up GitOps for a tenant in one call: optionally CREATE the config repo (always a fresh random name, never an existing repo), link it, then
 *  generate the full artifact tree and commit it as one commit. */
export async function POST(req: NextRequest) {
  if (!githubConfigured()) {
    return NextResponse.json({ ok: false, error: "GitHub is not configured." }, { status: 400 });
  }
  const body: GitopsInitBody = await req.json().catch(() => ({}));
  const team = body?.team || req.headers.get("x-hive-team") || "personal";
  const scope = body?.scope === "org" ? "org" : "personal";
  const entity = await resolveEntity();
  const status = await githubStatus(entity);
  if (!status.connected) return NextResponse.json({ ok: false, error: "GitHub not connected." }, { status: 400 });

  // The repository link is an admin write (`PUT /v1/gitops`): refuse a role that misses it BEFORE creating a repository on the user's GitHub account that no link would point at. The role is read from the caller's own
  // JWT only to decide that; the backend verifies it and answers the link below.
  const authToken = authTokenFrom(req);
  const claimed = claimedRole(authToken);
  if (!claimed.operator && !can(claimed.role, "repo.link")) {
    return NextResponse.json({ ok: false, error: roleRefusalFor("repo.link") }, { status: 403 });
  }

  let fullName: string;
  let branch = (body?.branch && String(body.branch)) || "main";
  let created = false;
  if (body?.create) {
    const org = scope === "org" ? String(body?.org || "").trim() || undefined : undefined;
    const isPrivate = body?.isPrivate !== false;
    let made: Awaited<ReturnType<typeof createRepo>> | null = null;
    for (let attempt = 0; attempt < MAX_REPO_NAME_ATTEMPTS; attempt++) {
      const r = await createRepo(entity, { name: randomConfigRepoName(), org, isPrivate });
      if (r.ok && r.full_name) { made = r; break; }
      if (!r.conflict) {
        return NextResponse.json(
          { ok: false, error: r.error || "Failed to create repository.", approve_url: r.approve_url, restricted: r.restricted },
          { status: r.restricted ? 403 : 502 }
        );
      }
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

  const linked = await backend("/v1/gitops", team, authToken, {
    method: "PUT",
    body: JSON.stringify({ repo: fullName, branch, path, scope }),
  });
  if (!linked.ok) {
    const refusal = linked.status === 403 ? roleRefusalSentence(await linked.text().catch(() => "")) : null;
    return NextResponse.json(
      { ok: false, repo: fullName, created, error: refusal ?? `the platform refused the repository link (HTTP ${linked.status})` },
      { status: refusal ? 403 : 502 }
    );
  }

  const { files, hash, projectCount, failures } = await buildOrgArtifacts(team, authToken, path);
  if (failures.length) {
    console.error(
      `[gitops/init] refusing to commit a partial tree repo=${fullName} team=${team}: platform state unreadable (${failures.join(", ")})`
    );
    return NextResponse.json(
      { ok: false, repo: fullName, created, error: `platform state unreadable (${failures.join(", ")}) — refusing to commit a partial config tree` },
      { status: 502 }
    );
  }
  const result = await commitFiles(entity, {
    owner, repo, branch,
    message: created
      ? `feat(openedge): initialize GitOps config (${files.length} artifacts)`
      : `chore(openedge): sync GitOps config (${files.length} artifacts)`,
    files,
    managedPrefixes: ["projects/"],
  });
  if (!result.ok) {
    console.error(
      `[gitops/init] commit failed repo=${fullName} branch=${branch} team=${team} created=${created}: ${result.error}`
    );
    return NextResponse.json({ ok: false, repo: fullName, created, error: result.error }, { status: 502 });
  }

  if (!result.deletionsSkipped) {
    await backend("/v1/gitops/synced", team, authToken, {
      method: "POST",
      body: JSON.stringify({ commit: result.commit || "", hash }),
    }).catch(() => {});
  }

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
    ...(result.deletionsSkipped ? { deletionsSkipped: true } : {}),
  });
}
