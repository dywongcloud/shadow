import { NextRequest, NextResponse } from "next/server";
import { githubConfigured, githubStatus, commitFile, setRepoVariable, createRepoWebhook, resolveEntity } from "@/lib/github";
import { deployWorkflowContent } from "@/lib/gitops-yaml";

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

/** Installs the OpenEdge deploy workflow and the OPENEDGE_WEBHOOK_URL Actions variable into a PROJECT's source repo so pushes auto-trigger build + deploy; body
 *  `{ repo }` (a clone URL or "owner/repo"). A real repo webhook is primary (the node's `/v1/git/webhook` makes a production deploy for the prod branch and a
 *  preview for every other branch and PR); the Actions workflow (push-to-main only) is installed ONLY when the webhook could not be, never both (it would
 *  double-fire). A no-op only when GitHub is genuinely not part of this deployment: "not connected" and an install where neither landed are 502s that keep
 *  the underlying `reason`/`error`, never a silent skip. */
export async function POST(req: NextRequest) {
  if (!githubConfigured()) return NextResponse.json({ skipped: true, reason: "github-not-configured" });
  const body = await req.json().catch(() => ({} as any));
  const parsed = parseRepo(String(body?.repo || ""));
  if (!parsed) return NextResponse.json({ skipped: true, reason: "bad-repo" });

  const entity = await resolveEntity();
  const status = await githubStatus(entity);
  if (!status.connected) {
    return NextResponse.json(
      { ok: false, reason: "github-not-connected", error: "GitHub is not connected — the CI webhook/workflow cannot be installed. Reconnect GitHub (Integrations), then retry." },
      { status: 502 }
    );
  }

  const { owner, repo } = parsed;
  const webhookUrl = (process.env.OPENEDGE_WEBHOOK_URL || "").replace(/\/$/, "");
  const secret = process.env.GITHUB_WEBHOOK_SECRET || undefined;

  let webhookInstalled = false;
  let webhookError: string | undefined;
  if (webhookUrl) {
    const hook = await createRepoWebhook(entity, owner, repo, `${webhookUrl}/v1/git/webhook`, secret);
    webhookInstalled = hook.ok;
    if (!hook.ok) webhookError = hook.error;
  }

  let workflowInstalled = false;
  let variableSet = false;
  const writeErrors: string[] = webhookError ? [`webhook: ${webhookError}`] : [];
  if (!webhookInstalled) {
    const commit = await commitFile(entity, {
      owner, repo,
      path: ".github/workflows/openedge-deploy.yml",
      content: deployWorkflowContent(),
      message: "ci(openedge): add deploy workflow",
    });
    workflowInstalled = commit.ok;
    if (!commit.ok) writeErrors.push(`workflow: ${commit.error || "commit failed"}`);
    if (webhookUrl) {
      const v = await setRepoVariable(entity, owner, repo, "OPENEDGE_WEBHOOK_URL", webhookUrl);
      variableSet = v.ok;
      if (!v.ok) writeErrors.push(`variable: ${v.error || "set failed"}`);
    }
  }

  if (!webhookInstalled && !workflowInstalled) {
    return NextResponse.json(
      {
        ok: false,
        repo: `${owner}/${repo}`,
        webhookInstalled,
        workflowInstalled,
        variableSet,
        error: writeErrors.length
          ? writeErrors.join("; ")
          : "no OpenEdge webhook URL configured and the workflow install did not run",
      },
      { status: 502 }
    );
  }

  return NextResponse.json({
    ok: true,
    repo: `${owner}/${repo}`,
    webhookInstalled,
    workflowInstalled,
    variableSet,
  });
}
