import { NextRequest, NextResponse } from "next/server";
import { githubConfigured, githubStatus, commitFile, setRepoVariable, createRepoWebhook, resolveEntity } from "@/lib/github";
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
 * Install the DevHub deploy workflow + the legacy OPENEDGE_WEBHOOK_URL Actions variable
 * into a PROJECT's source repo, so pushes to it auto-trigger build+deploy. Called
 * after a Git import. No-ops gracefully ONLY when GitHub genuinely isn't part of
 * this deployment (not configured / unparseable repo).
 *
 * Body: { repo: string }  // clone URL or "owner/repo"
 *
 * Failure honesty: "GitHub not connected" is a 502 with `reason` preserved (a
 * connected-looking deployment that can't write is a broken integration, never
 * a benign skip), and an install attempt where neither the webhook nor the
 * workflow landed returns 502 with the underlying GitHub errors in `error` —
 * callers persist/show that instead of a bare silent `ok:false`.
 */
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

  // Primary mechanism: a real repo webhook (push + pull_request) → the node's
  // /v1/git/webhook, which creates a PRODUCTION deploy for the prod branch and a
  // PREVIEW deploy for every other branch + PR. Covers all branches + PRs, unlike
  // the Actions workflow (push-to-main only).
  let webhookInstalled = false;
  let webhookError: string | undefined;
  if (webhookUrl) {
    const hook = await createRepoWebhook(entity, owner, repo, `${webhookUrl}/v1/git/webhook`, secret);
    webhookInstalled = hook.ok;
    if (!hook.ok) webhookError = hook.error;
  }

  // Fallback ONLY when the real webhook couldn't be installed (e.g. no public URL):
  // install the Actions workflow (push-to-main) so production deploys still trigger.
  // We never install both — that would double-fire on push-to-main.
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
    // NEITHER write landed — a silent 200 here is how pushes used to stop
    // deploying with zero visible error. Fail loudly with the real reasons.
    return NextResponse.json(
      {
        ok: false,
        repo: `${owner}/${repo}`,
        webhookInstalled,
        workflowInstalled,
        variableSet,
        error: writeErrors.length
          ? writeErrors.join("; ")
          : "no DevHub webhook URL configured and the workflow install did not run",
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
