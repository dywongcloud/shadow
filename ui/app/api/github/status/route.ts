import { NextResponse } from "next/server";
import { githubConnectionDetail, resolveEntity } from "@/lib/github";

/** HONEST GitHub connection status: whether the connection is ACTIVE and live (Composio stays ACTIVE after GitHub revokes a token), plus the granted scopes, the account login and
 *  whether private-repo (`repo`) and org-enumeration (`read:org`) access are present, so the Integrations UI can show what a user can reach and prompt reconnect/approve. Never
 *  returns a token value. */
export async function GET() {
  const entity = await resolveEntity();
  const detail = await githubConnectionDetail(entity);
  return NextResponse.json(detail);
}
