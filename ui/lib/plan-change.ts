import { ApiError, ROLE_REFUSED, apiSend } from "@/lib/api";

/** The tiers in rising order, as `billing::plan_rank` ranks them. */
export const PLAN_TIERS = ["hobby", "pro", "enterprise"] as const;

const GENERIC_REFUSAL = "The plan could not be changed. Try again, or use the Billing page.";
const PERMISSION_REFUSAL =
  "You can't change this team's plan from here. Only a team owner or admin can lower it, a higher plan is bought on the Billing page, and only a platform operator can change a plan without paying.";
const NO_TEAM_REFUSAL = "This team has no plan record to change here. The Billing page manages the plan of the team you are working in.";
const RAW_REQUEST_LINE = /^[A-Z]+ \/\S* -> \d+$/;

const tierRank = (plan: string) => Math.max(0, PLAN_TIERS.indexOf(plan.toLowerCase() as (typeof PLAN_TIERS)[number]));

export type PlanAction = "none" | "switch" | "checkout";

/** How a plan button acts. A plan is bought or granted, never self-assigned (`admin::apply_plan_everywhere`): `PUT /v1/teams/:slug/plan` keeps or LOWERS a tier for an owner or admin and raises it only for a platform
 *  operator, so an operator switches directly, a lower tier is a direct switch for everyone, and a RAISE by anyone else is bought through the Billing page's checkout (an unknown operator status, `null`, takes the
 *  checkout too: it is valid for everyone and the backend decides). */
export function planAction(current: string, target: string, operator: boolean | null): PlanAction {
  if (tierRank(target) === tierRank(current)) return "none";
  return operator === true || tierRank(target) < tierRank(current) ? "switch" : "checkout";
}

/** Opens the checkout the Billing page opens (`POST /v1/billing/checkout`, kind `plan`): `applied` means a free plan took effect at once, otherwise `url` is Stripe's hosted page or the mock checkout. */
export async function startPlanCheckout(plan: string): Promise<{ applied: boolean; url: string }> {
  const opened = await apiSend<{ url?: string; applied?: boolean }>("POST", "/v1/billing/checkout", { kind: "plan", plan });
  if (!opened.applied && !opened.url) throw new Error("the checkout returned no payment page");
  return { applied: !!opened.applied, url: opened.url ?? "" };
}

export function followCheckout(url: string): void {
  window.location.assign(url);
}

/** A failed plan change in words a person can act on: a permission refusal is explained, the node's own sentences (billing not set up, the payment provider unavailable) pass through, and a raw request line, a markup
 *  body or anything long is never echoed. */
export function planRefusal(error: unknown): string {
  if (!(error instanceof ApiError)) return GENERIC_REFUSAL;
  if (error.code === ROLE_REFUSED) return error.message;
  if (error.status === 403) return PERMISSION_REFUSAL;
  if (error.status === 404) return NO_TEAM_REFUSAL;
  const sentence = error.message.trim();
  const readable = sentence && sentence.length <= 240 && !/[<{]/.test(sentence) && !RAW_REQUEST_LINE.test(sentence);
  return readable ? sentence[0].toUpperCase() + sentence.slice(1) : GENERIC_REFUSAL;
}
