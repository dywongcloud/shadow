import { NextResponse } from "next/server";
import { marketplaceDeploymentConfigured } from "@/lib/marketplace-deployment-server";

export const dynamic = "force-dynamic";

/** Configuration presence only: never disclose the Marketplace URL or credentials. */
export async function GET() {
  return NextResponse.json({
    configured: marketplaceDeploymentConfigured(),
    clerk_template: "autheo-marketplace-v1",
    endpoint: "/v1/marketplace/orders/{marketplace_order_id}/placement-policy",
    contract_version: 1,
  });
}
