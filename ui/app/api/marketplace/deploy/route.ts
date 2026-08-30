import { NextRequest, NextResponse } from "next/server";
import { auth } from "@clerk/nextjs/server";
import {
  clerkMarketplaceTenant,
  fetchMarketplacePlacementPolicy,
  MarketplacePolicyError,
  validateMarketplacePlacementPolicy,
} from "@/lib/marketplace-placement-policy";
import { authTokenFrom, backend } from "@/lib/gitops-server";
import { marketplaceDeploymentUrl } from "@/lib/marketplace-deployment-server";

export const dynamic = "force-dynamic";

const PROJECT = /^[a-zA-Z0-9][a-zA-Z0-9._-]{0,127}$/;
const ENV_KEY = /^[A-Za-z_][A-Za-z0-9_]{0,127}$/;

function optionalText(body: Record<string, unknown>, field: string, limit: number): string | undefined {
  const value = body[field];
  if (value === undefined) return undefined;
  if (typeof value !== "string" || value.length > limit) {
    throw new MarketplacePolicyError(400, "INVALID_DEPLOY_INPUT", `${field} must be a string no longer than ${limit} characters.`);
  }
  return value.trim() || undefined;
}

function safeRepositoryUrl(value: string): string {
  let url: URL;
  try {
    url = new URL(value);
  } catch {
    throw new MarketplacePolicyError(400, "INVALID_DEPLOY_INPUT", "repo_url must be an absolute HTTP(S) repository URL.");
  }
  if (
    (url.protocol !== "https:" && url.protocol !== "http:") ||
    url.username ||
    url.password ||
    url.search ||
    url.hash
  ) {
    throw new MarketplacePolicyError(
      400,
      "INVALID_DEPLOY_INPUT",
      "repo_url must be an HTTP(S) repository URL without embedded credentials, a query string, or a fragment.",
    );
  }
  return url.toString();
}

function safeEnv(value: unknown): Record<string, string> | undefined {
  if (value === undefined) return undefined;
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new MarketplacePolicyError(400, "INVALID_DEPLOY_INPUT", "env must be a key/value object.");
  }
  const entries = Object.entries(value as Record<string, unknown>);
  if (entries.length > 100) throw new MarketplacePolicyError(400, "INVALID_DEPLOY_INPUT", "env may contain at most 100 variables.");
  const env: Record<string, string> = {};
  for (const [key, item] of entries) {
    if (!ENV_KEY.test(key) || typeof item !== "string" || item.length > 16_384) {
      throw new MarketplacePolicyError(400, "INVALID_DEPLOY_INPUT", "Environment variable names or values are invalid.");
    }
    env[key] = item;
  }
  return env;
}

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
  try {
    const repoUrl = optionalText(body, "repo_url", 2048);
    if (!repoUrl) throw new MarketplacePolicyError(400, "INVALID_DEPLOY_INPUT", "repo_url is required.");
    const safeRepoUrl = safeRepositoryUrl(repoUrl);
    const project = optionalText(body, "project", 128);
    if (project && !PROJECT.test(project)) {
      throw new MarketplacePolicyError(400, "INVALID_DEPLOY_INPUT", "project contains unsupported characters.");
    }
    const rootDir = optionalText(body, "root_dir", 512);
    if (rootDir && (rootDir.startsWith("/") || rootDir.includes("\\") || rootDir.split("/").some((part) => part === ".."))) {
      throw new MarketplacePolicyError(400, "INVALID_DEPLOY_INPUT", "root_dir must stay within the repository.");
    }
    const target = optionalText(body, "target", 32);
    if (target && target !== "production" && target !== "preview") {
      throw new MarketplacePolicyError(400, "INVALID_DEPLOY_INPUT", "target must be production or preview.");
    }
    const session = await auth();
    const buyerTenantId = clerkMarketplaceTenant({ userId: session.userId ?? null, orgId: session.orgId ?? null });
    const marketplaceJwt = await session.getToken({ template: "autheo-marketplace-v1" });
    if (!marketplaceJwt) {
      throw new MarketplacePolicyError(401, "MARKETPLACE_JWT_UNAVAILABLE", "Could not obtain the Clerk Marketplace token.");
    }
    const response = await fetchMarketplacePlacementPolicy(marketplaceDeploymentUrl(), marketplaceOrderId, marketplaceJwt);
    const marketplacePlacement = validateMarketplacePlacementPolicy(response, marketplaceOrderId, buyerTenantId);

    // Copy only ordinary deploy inputs. Never pass through client tenant,
    // buyer/provider/role/policy fields, authorization headers, or hive_jwt.
    const deploy: Record<string, unknown> = {
      repo_url: safeRepoUrl,
      marketplace_placement: marketplacePlacement,
    };
    const branch = optionalText(body, "branch", 256);
    if (branch) deploy.branch = branch;
    if (project) deploy.project = project;
    if (rootDir) deploy.root_dir = rootDir;
    if (target) deploy.target = target;
    if (typeof body.use_cache === "boolean") deploy.use_cache = body.use_cache;
    if (typeof body.redeploy === "boolean") deploy.redeploy = body.redeploy;
    const env = safeEnv(body.env);
    if (env && Object.keys(env).length) deploy.env = env;

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
