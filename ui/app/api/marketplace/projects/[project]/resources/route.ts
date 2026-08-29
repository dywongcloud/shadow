import { NextRequest, NextResponse } from "next/server";
import { auth } from "@clerk/nextjs/server";
import {
  attachMarketplaceOrder,
  MarketplaceIntegrationError,
  listMarketplaceProjectResources,
} from "@/lib/marketplace-project-resources";

export const dynamic = "force-dynamic";

const PROJECT = /^[a-zA-Z0-9][a-zA-Z0-9._-]{0,127}$/;

function projectId(value: string): string {
  const project = decodeURIComponent(value);
  if (!PROJECT.test(project)) {
    throw new MarketplaceIntegrationError(400, "INVALID_PROJECT", "Invalid project ID.");
  }
  return project;
}

async function marketplaceToken(): Promise<string> {
  const session = await auth();
  if (!session.userId) {
    throw new MarketplaceIntegrationError(401, "UNAUTHENTICATED", "Sign in before accessing Marketplace resources.");
  }
  const token = await session.getToken({ template: "autheo-marketplace-v1" });
  if (!token) {
    throw new MarketplaceIntegrationError(401, "UNAUTHENTICATED", "Marketplace Clerk token is unavailable.");
  }
  return token;
}

function failure(error: unknown) {
  if (error instanceof MarketplaceIntegrationError) {
    return NextResponse.json({ error: error.code }, { status: error.status });
  }
  return NextResponse.json({ error: "REQUEST_FAILED" }, { status: 502 });
}

export async function GET(_: NextRequest, { params }: { params: Promise<{ project: string }> }) {
  try {
    const project = projectId((await params).project);
    const resources = await listMarketplaceProjectResources(project, await marketplaceToken());
    return NextResponse.json({ resources });
  } catch (error) {
    return failure(error);
  }
}

/**
 * Marketplace redirects users back with `?marketplace_order=<id>` after
 * checkout. Attachment remains server-side and authenticated; the browser can
 * never claim a resource for a different Clerk tenant.
 */
export async function POST(req: NextRequest, { params }: { params: Promise<{ project: string }> }) {
  try {
    const project = projectId((await params).project);
    const body = await req.json().catch(() => ({})) as { marketplace_order_id?: unknown };
    const orderId = typeof body.marketplace_order_id === "string" ? body.marketplace_order_id.trim() : "";
    if (!orderId || orderId.length > 256) {
      throw new MarketplaceIntegrationError(400, "INVALID_ORDER", "Invalid Marketplace order ID.");
    }
    const resource = await attachMarketplaceOrder(project, orderId, await marketplaceToken());
    return NextResponse.json({ resource });
  } catch (error) {
    return failure(error);
  }
}
