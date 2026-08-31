import { NextResponse } from "next/server";
import { githubOrgs, resolveEntity } from "@/lib/github";

/**
 * The organizations the connected GitHub user belongs to — powers the org picker
 * (replacing the free-text org login) and lets the UI show which orgs a connection
 * can reach. Needs the `read:org` scope; degrades to an empty list (not an error)
 * when the scope is absent or GitHub isn't connected, so the UI can fall back.
 */
export async function GET() {
  const entity = await resolveEntity();
  const orgs = await githubOrgs(entity);
  return NextResponse.json({ orgs });
}
