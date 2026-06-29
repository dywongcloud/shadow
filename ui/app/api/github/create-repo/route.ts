import { NextRequest, NextResponse } from "next/server";
import { composioConfigured, createRepo, resolveEntity } from "@/lib/composio";

export const dynamic = "force-dynamic";

// Create a GitHub repository using THIS user's Composio GitHub connection, so the
// in-browser Git page can make a repo without a pasted PAT. The repo is created
// EMPTY (autoInit:false) so the page's own first local commit is the initial push
// (no non-fast-forward against an auto-generated base commit).
export async function POST(req: NextRequest) {
  if (!composioConfigured()) {
    return NextResponse.json(
      { error: "Composio not configured. Set COMPOSIO_API_KEY or use a GitHub PAT." },
      { status: 400 }
    );
  }
  const body = await req.json().catch(() => ({} as any));
  const name = typeof body?.name === "string" ? body.name.trim() : "";
  if (!name) return NextResponse.json({ error: "Repo name is required." }, { status: 400 });
  const isPrivate = body?.isPrivate !== false; // default private
  const org = typeof body?.org === "string" && body.org.trim() ? body.org.trim() : undefined;

  // Same stable entity as status/repos/connect — so the user's connected account is used.
  const entity = await resolveEntity();
  const res = await createRepo(entity, { name, org, isPrivate, autoInit: false });
  if (!res.ok) {
    return NextResponse.json(
      { error: res.error || "Failed to create repository", conflict: !!res.conflict },
      { status: res.conflict ? 409 : 500 }
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
