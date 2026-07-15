import { NextResponse } from "next/server";
import { githubConnectionDetail, resolveEntity } from "@/lib/composio";

export const dynamic = "force-dynamic";

/**
 * HONEST GitHub connection status: whether the connection is ACTIVE *and live*
 * (Composio keeps ACTIVE after GitHub revokes a token), plus the granted scopes,
 * the account login, and whether private-repo (`repo`) + org-enumeration
 * (`read:org`) access are present — so the Integrations UI can show exactly what a
 * user can reach and prompt reconnect/approve when something is missing. Never
 * returns a token value.
 */
export async function GET() {
  const entity = await resolveEntity();
  const detail = await githubConnectionDetail(entity);
  return NextResponse.json(detail);
}
