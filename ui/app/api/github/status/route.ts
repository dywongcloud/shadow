import { NextResponse } from "next/server";
import { composioConfigured, githubStatus, resolveEntity } from "@/lib/composio";

export const dynamic = "force-dynamic";

export async function GET() {
  const entity = await resolveEntity();
  if (!composioConfigured()) {
    return NextResponse.json({ configured: false, connected: false, entity });
  }
  const status = await githubStatus(entity);
  return NextResponse.json({ ...status, entity });
}
