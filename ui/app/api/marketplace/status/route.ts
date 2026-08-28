import { NextResponse } from "next/server";

export const dynamic = "force-dynamic";

/** Configuration presence only: never disclose the Marketplace URL or credentials. */
export async function GET() {
  return NextResponse.json({
    configured: Boolean(process.env.MARKETPLACE_URL),
    clerk_template: "autheo-marketplace-v1",
    endpoint: "/v1/marketplace/orders/{marketplace_order_id}/placement-policy",
    contract_version: 1,
  });
}
