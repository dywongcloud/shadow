import { NextResponse } from "next/server";
import { disconnectGithub, githubConfigured, resolveEntity } from "@/lib/github";

/**
 * Disconnect the signed-in user's GitHub connection (deletes EVERY Composio
 * connected_account the entity has for GitHub, so a later reconnect binds a fresh
 * connection with the current scopes). Backs the Integrations "Disconnect" button
 * and is the first half of "reconnect / adjust access" (disconnect → re-run OAuth).
 */
export async function POST() {
  if (!githubConfigured()) {
    return NextResponse.json({ ok: false, error: "GitHub isn't configured on this deployment." }, { status: 400 });
  }
  const entity = await resolveEntity();
  const res = await disconnectGithub(entity);
  return NextResponse.json(res, { status: res.ok ? 200 : 502 });
}
