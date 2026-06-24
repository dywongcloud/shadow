import { NextRequest, NextResponse } from "next/server";
import { composioConfigured, githubStatus, commitFile, setRepoVariable, createRepoWebhook, resolveEntity } from "@/lib/composio";
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
  const webhookUrl = (process.env.OPENEDGE_WEBHOOK_URL || "").replace(/\/$/, "");
  const secret = process.env.GITHUB_WEBHOOK_SECRET || undefined;

  // Primary mechanism: a real repo webhook (push + pull_request) → the node's
  // /v1/git/webhook, which creates a PRODUCTION deploy for the prod branch and a
  // PREVIEW deploy for every other branch + PR. Covers all branches + PRs, unlike
  // the Actions workflow (push-to-main only).
  let webhookInstalled = false;
  if (webhookUrl) {
    const hook = await createRepoWebhook(entity, owner, repo, `${webhookUrl}/v1/git/webhook`, secret);
    webhookInstalled = hook.ok;
  }

  // Fallback ONLY when the real webhook couldn't be installed (e.g. no public URL):
  // install the Actions workflow (push-to-main) so production deploys still trigger.
  // We never install both — that would double-fire on push-to-main.
  let workflowInstalled = false;
  let variableSet = false;
  if (!webhookInstalled) {
    const commit = await commitFile(entity, {
      owner, repo,
      path: ".github/workflows/openedge-deploy.yml",
      content: deployWorkflowContent(),
      message: "ci(openedge): add deploy workflow",
    });
    workflowInstalled = commit.ok;
    if (webhookUrl) {
      const v = await setRepoVariable(entity, owner, repo, "OPENEDGE_WEBHOOK_URL", webhookUrl);
      variableSet = v.ok;
    }
  }

  return NextResponse.json({
    ok: webhookInstalled || workflowInstalled,
    repo: `${owner}/${repo}`,
    webhookInstalled,
    workflowInstalled,
    variableSet,
  });
}
