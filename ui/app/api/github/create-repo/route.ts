import { NextRequest, NextResponse } from "next/server";
import { createRepo, githubConfigured, resolveEntity } from "@/lib/github";

/** Creates a GitHub repository with THIS user's connection (first-party GitHub App preferred, Composio fallback) so the in-browser Git page can make one without a pasted PAT.
 *  The repo is created EMPTY (`autoInit: false`) so the page's own first local commit is the initial push, with no non-fast-forward against an auto-generated base; an org-approval
 *  restriction is threaded back (`restricted` + `approve_url`) so the UI can link the approval instead of showing raw GitHub JSON. */
export async function POST(req: NextRequest) {
  if (!githubConfigured()) {
    return NextResponse.json(
      { error: "GitHub is not configured on this deployment. Connect GitHub or use a PAT." },
      { status: 400 }
    );
  }
  const body = await req.json().catch(() => ({} as any));
  const name = typeof body?.name === "string" ? body.name.trim() : "";
  if (!name) return NextResponse.json({ error: "Repo name is required." }, { status: 400 });
  const isPrivate = body?.isPrivate !== false;
  const org = typeof body?.org === "string" && body.org.trim() ? body.org.trim() : undefined;

  const entity = await resolveEntity();
  const res = await createRepo(entity, { name, org, isPrivate, autoInit: false });
  if (!res.ok) {
    return NextResponse.json(
      {
        error: res.error || "Failed to create repository",
        conflict: !!res.conflict,
        restricted: !!res.restricted,
        approve_url: res.approve_url,
      },
      { status: res.conflict ? 409 : res.restricted ? 403 : 500 }
    );
  }
  const fullName = res.full_name || "";
  return NextResponse.json({
    ok: true,
    full_name: fullName,
    default_branch: res.default_branch || "main",
    clone_url: fullName ? `https://github.com/${fullName}.git` : "",
    html_url: fullName ? `https://github.com/${fullName}` : "",
  });
}
