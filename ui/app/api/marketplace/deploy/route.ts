import { NextRequest, NextResponse } from "next/server";
import { auth } from "@clerk/nextjs/server";
import {
  clerkMarketplaceTenant,
  fetchMarketplacePlacementPolicy,
  MarketplacePolicyError,
  validateMarketplacePlacementPolicy,
} from "@/lib/marketplace-placement-policy";
import { authTokenFrom, backend } from "@/lib/gitops-server";

export const dynamic = "force-dynamic";

const MARKETPLACE_URL = process.env.MARKETPLACE_URL || "";

/**
 * Marketplace deployment boundary.
 *
 * Browser input is limited to build inputs and an order id. Tenant identity,
 * Marketplace authorization, provider/listing data, the placement policy, and
 * its Clerk template JWT are all resolved on this server. In particular, the
 * browser's `hive_jwt` is never forwarded to Marketplace.
 */
export async function POST(req: NextRequest) {
  const body = (await req.json().catch(() => ({}))) as Record<string, unknown>;
  const marketplaceOrderId = typeof body.marketplace_order_id === "string" ? body.marketplace_order_id.trim() : "";
  if (!marketplaceOrderId || marketplaceOrderId.length > 256) {
    return NextResponse.json({ error: "A valid marketplace_order_id is required." }, { status: 400 });
  }
  if (!MARKETPLACE_URL) {
    return NextResponse.json({ error: "Marketplace deployment is not configured." }, { status: 503 });
  }

  try {
    const session = await auth();
    const buyerTenantId = clerkMarketplaceTenant({ userId: session.userId ?? null, orgId: session.orgId ?? null });
    const marketplaceJwt = await session.getToken({ template: "autheo-marketplace-v1" });
    if (!marketplaceJwt) {
      throw new MarketplacePolicyError(401, "MARKETPLACE_JWT_UNAVAILABLE", "Could not obtain the Clerk Marketplace token.");
    }
    const response = await fetchMarketplacePlacementPolicy(MARKETPLACE_URL, marketplaceOrderId, marketplaceJwt);
    const marketplacePlacement = validateMarketplacePlacementPolicy(response, marketplaceOrderId, buyerTenantId);

    // Copy only ordinary deploy inputs. Never pass through client tenant,
    // buyer/provider/role/policy fields, authorization headers, or hive_jwt.
    const deploy: Record<string, unknown> = {
      repo_url: typeof body.repo_url === "string" ? body.repo_url : "",
      marketplace_placement: marketplacePlacement,
    };
    for (const field of ["branch", "project", "root_dir", "target"] as const) {
      if (typeof body[field] === "string") deploy[field] = body[field];
    }
    if (typeof body.use_cache === "boolean") deploy.use_cache = body.use_cache;
    if (typeof body.redeploy === "boolean") deploy.redeploy = body.redeploy;
    if (body.env && typeof body.env === "object" && !Array.isArray(body.env)) deploy.env = body.env;

    // Hive's existing server-to-backend authentication remains local to this
    // application. It is intentionally separate from, and never sent to,
    // Marketplace. The backend derives its own project tenant from that
    // session; Marketplace buyer authorization is stored in the immutable
    // snapshot above.
    const result = await backend(
      "/v1/git/deploy",
      "",
      { method: "POST", body: JSON.stringify(deploy) },
      authTokenFrom(req),
    );
    const text = await result.text();
    return new NextResponse(text, {
      status: result.status,
      headers: { "content-type": result.headers.get("content-type") || "application/json" },
    });
  } catch (error) {
    if (error instanceof MarketplacePolicyError) {
      return NextResponse.json({ error: error.code }, { status: error.status });
    }
    return NextResponse.json({ error: "MARKETPLACE_POLICY_FAILED" }, { status: 502 });
  }
}
