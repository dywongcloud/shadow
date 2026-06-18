import { NextResponse } from "next/server";
import { composioConfigured, listToolkits } from "@/lib/composio";

// ISR-style caching: the 1,000+ toolkit catalog rarely changes, so cache it and
// revalidate hourly instead of refetching the whole list on every page load.
// https://nextjs.org/docs/pages/guides/incremental-static-regeneration
export const revalidate = 3600;

export async function GET() {
  const configured = composioConfigured();
  const toolkits = configured ? await listToolkits() : [];
  return NextResponse.json(
    { configured, toolkits },
    { headers: { "Cache-Control": "public, s-maxage=3600, stale-while-revalidate=86400" } }
  );
}
