import { NextRequest, NextResponse } from "next/server";
import { composioConfigured, githubStatus, commitFile, setRepoVariable, resolveEntity } from "@/lib/composio";
import { deployWorkflowContent } from "@/lib/gitops-yaml";

export const dynamic = "force-dynamic";

/** Parse "owner/repo" from a clone URL or full_name. */
function parseRepo(input: string): { owner: string; repo: string } | null {
  let s = (input || "").trim().toLowerCase();
  s = s.replace(/^git@github\.com:/, "").replace(/^https?:\/\//, "").replace(/^ssh:\/\//, "");
  s = s.replace(/^github\.com\//, "").replace(/\.git$/, "");
  s = s.split("@").pop() || s;
  const parts = s.split("/").filter(Boolean);
  if (parts.length >= 2) return { owner: parts[parts.length - 2], repo: parts[parts.length - 1] };
  return null;
}

/**
 * Install the OpenEdge deploy workflow + the OPENEDGE_WEBHOOK_URL Actions variable
 * into a PROJECT's source repo, so pushes to it auto-trigger build+deploy. Called
 * after a Git import. No-ops gracefully when GitHub isn't connected.
 *
 * Body: { repo: string }  // clone URL or "owner/repo"
 */
export async function POST(req: NextRequest) {
  if (!composioConfigured()) return NextResponse.json({ skipped: true, reason: "composio-not-configured" });
  const body = await req.json().catch(() => ({} as any));
  const parsed = parseRepo(String(body?.repo || ""));
  if (!parsed) return NextResponse.json({ skipped: true, reason: "bad-repo" });

  const entity = await resolveEntity();
  const status = await githubStatus(entity);
  if (!status.connected) return NextResponse.json({ skipped: true, reason: "github-not-connected" });

  const { owner, repo } = parsed;
  const commit = await commitFile(entity, {
    owner, repo,
    path: ".github/workflows/openedge-deploy.yml",
    content: deployWorkflowContent(),
    message: "ci(openedge): add deploy workflow",
  });

  const webhookUrl = (process.env.OPENEDGE_WEBHOOK_URL || "").replace(/\/$/, "");
  let variableSet = false;
  if (webhookUrl) {
    const v = await setRepoVariable(entity, owner, repo, "OPENEDGE_WEBHOOK_URL", webhookUrl);
    variableSet = v.ok;
  }

  return NextResponse.json({
    ok: commit.ok,
    repo: `${owner}/${repo}`,
    workflowInstalled: commit.ok,
    variableSet,
    error: commit.ok ? undefined : commit.error,
  });
}
