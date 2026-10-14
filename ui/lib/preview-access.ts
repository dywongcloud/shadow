import "server-only";
import { clerkClient } from "@clerk/nextjs/server";
import { ownerEmails } from "@/lib/owner-emails";

/** `unavailable` (a failed Clerk lookup) is never a verdict: callers answer a retryable 503, not a refusal. */
export type Access = "granted" | "denied" | "unavailable";

const LEGACY_PERSONAL_TEAMS = new Set(["personal", "__personal__", ""]);

/** The legacy shared `personal` namespace and the per-user `u_<clerkUid>` namespaces, as opposed to a Clerk org. */
export function isPersonalTeam(team: string): boolean {
  return LEGACY_PERSONAL_TEAMS.has(team) || team.startsWith("u_");
}

async function clerkUsers() {
  return (typeof clerkClient === "function" ? await (clerkClient as any)() : clerkClient).users as any;
}

/** A personal namespace belongs to exactly ONE user, so membership is checked directly rather than through Clerk orgs: `u_<clerkUid>` must match exactly, and the legacy shared
 *  `personal` belongs to the platform owner(s) by HIVE_OWNER_EMAIL (with none configured only a non-production deployment lets anyone in). */
async function personalAccess(team: string, userId: string): Promise<Access> {
  if (team.startsWith("u_")) return team === `u_${userId}` ? "granted" : "denied";
  const owners = ownerEmails();
  if (owners.length === 0) return process.env.NODE_ENV !== "production" ? "granted" : "denied";
  try {
    const u = await (await clerkUsers()).getUser(userId);
    const emails: string[] = (u?.emailAddresses ?? []).map((e: any) => String(e?.emailAddress || "").toLowerCase());
    return emails.some((e) => owners.includes(e)) ? "granted" : "denied";
  } catch {
    return "unavailable";
  }
}

/** Is `userId` a member of the Clerk org whose slug (or id) == `team`? */
async function orgAccess(userId: string, team: string): Promise<Access> {
  try {
    const res = await (await clerkUsers()).getOrganizationMembershipList({ userId, limit: 200 });
    const list: any[] = res?.data ?? res ?? [];
    return list.some((m) => {
      const o = m.organization || {};
      return o.slug === team || o.id === team;
    })
      ? "granted"
      : "denied";
  } catch {
    return "unavailable";
  }
}

/** May the signed-in Clerk user act for `team`: the ONE authorization behind preview unlock and ZK roster enrollment, failing closed (a lookup that cannot be answered is `unavailable`). */
export async function teamAccess(team: string, userId: string): Promise<Access> {
  return isPersonalTeam(team) ? personalAccess(team, userId) : orgAccess(userId, team);
}
