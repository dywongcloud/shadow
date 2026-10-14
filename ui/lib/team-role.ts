/** The team roles in rising order, as `crates/hive-cloud/src/roles.rs` ranks them (viewer < member < admin < owner). */
export const TEAM_ROLES = ["viewer", "member", "admin", "owner"] as const;
export type TeamRole = (typeof TEAM_ROLES)[number];

/** The ONE copy of the dashboard's view of the backend matrix (`roles.rs` `RULES`): the lowest role per action and the requests the action covers, so a drift from the backend is a diff of this table
 *  against `RULES`. The backend stays the authority and answers 403 "insufficient team role"; the UI only decides whether to offer a control. A request no row names needs viewer to read and ADMIN
 *  to change anything: a member's route is listed there deliberately, and `work` stands for those (day-to-day project work). */
export const TEAM_ACTIONS = {
  work: { floor: "member", routes: ["POST /v1/apikeys", "POST /v1/git/deploy", "POST /v1/deploy/zip", "POST /v1/projects/:project/env", "POST /v1/databases", "POST /v1/teams"] },
  "enterprise.project": { floor: "member", routes: ["POST /v1/enterprise/microfrontends", "DELETE /v1/enterprise/microfrontends/:id", "POST /v1/enterprise/conformance/run"] },
  "webhooks.write": { floor: "admin", routes: ["POST /v1/webhooks", "DELETE /v1/webhooks/:id", "POST /v1/projects/:project/webhooks"] },
  "integrations.write": { floor: "admin", routes: ["POST /v1/integrations", "DELETE /v1/integrations/:id"] },
  "project.team": { floor: "admin", routes: ["PUT /v1/projects/:project/team"] },
  "project.delete": { floor: "admin", routes: ["DELETE /v1/projects/:project"] },
  "apikey.revoke": { floor: "admin", routes: ["DELETE /v1/apikeys/:id"] },
  "billing.pay": { floor: "admin", routes: ["POST /v1/billing/checkout", "POST /v1/billing/confirm", "POST /v1/billing/charge"] },
  "dns.write": {
    floor: "admin",
    routes: [
      "POST /v1/domains/:domain/import",
      "PUT /v1/domains/:domain/nameservers",
      "POST /v1/domains/:domain/records",
      "PUT /v1/domains/:domain/records/:id",
      "DELETE /v1/domains/:domain/records/:id",
      "POST /v1/domains/:domain/ssl/renew",
    ],
  },
  "database.destroy": { floor: "admin", routes: ["DELETE /v1/databases/:id", "POST /v1/databases/:id/rotate"] },
  "edge.security": { floor: "admin", routes: ["POST /v1/waf/rules", "DELETE /v1/waf/rules/:id", "PUT /v1/waf/managed", "PUT /v1/ratelimit", "PUT /v1/bot"] },
  "sms.provider": { floor: "admin", routes: ["POST /v1/push/sms-key", "POST /v1/push/sms-direct-mx", "POST /v1/push/sms-relay"] },
  "repo.link": { floor: "admin", routes: ["PUT /v1/gitops", "DELETE /v1/gitops"] },
  "enterprise.config": {
    floor: "admin",
    routes: [
      "POST /v1/enterprise/ip-blocks",
      "DELETE /v1/enterprise/ip-blocks/:id",
      "PUT /v1/enterprise/saml",
      "POST /v1/enterprise/scim",
      "DELETE /v1/enterprise/scim",
      "PUT /v1/enterprise/siem",
      "POST /v1/enterprise/siem/test",
      "PUT /v1/enterprise/projects/:project/protection",
    ],
  },
  "team.members": { floor: "admin", routes: ["POST /v1/teams/:slug/members", "DELETE /v1/teams/:slug/members/:email"] },
  "team.plan": { floor: "admin", routes: ["PUT /v1/teams/:slug/plan"] },
  "team.sso": { floor: "admin", routes: ["PUT /v1/teams/:slug/sso"] },
  "team.delete": { floor: "owner", routes: ["DELETE /v1/teams/:slug"] },
} as const satisfies Record<string, { floor: TeamRole; routes: readonly string[] }>;

export type TeamAction = keyof typeof TEAM_ACTIONS;

const rank = (role: TeamRole) => TEAM_ROLES.indexOf(role);

/** A role claim as a `TeamRole`, or `null` for one the table does not name (the backend ranks that lowest, the UI treats it as unknown and lets the server answer). */
export function teamRoleOf(claim: string | null | undefined): TeamRole | null {
  return (TEAM_ROLES as readonly string[]).includes(claim ?? "") ? (claim as TeamRole) : null;
}

/** May `role` do `action`? An unknown role (`null`) may: the control stays and the server decides. */
export function can(role: TeamRole | null, action: TeamAction): boolean {
  return role === null || rank(role) >= rank(TEAM_ACTIONS[action].floor);
}

/** The roles a team member may hand to a new API key: none above their own (an unknown role is offered all). */
export function rolesUpTo(role: TeamRole | null): readonly TeamRole[] {
  return role === null ? TEAM_ROLES : TEAM_ROLES.filter((r) => rank(r) <= rank(role));
}

/** The short tooltip for a control the role cannot use. */
export function roleHint(action: TeamAction): string {
  const floor = TEAM_ACTIONS[action].floor;
  return floor === "owner" ? "Needs the team owner" : floor === "admin" ? "Needs a team admin" : "Read-only: needs team member access";
}

/** The line shown above a read-only group of controls. */
export function roleNote(action: TeamAction): string {
  const floor = TEAM_ACTIONS[action].floor;
  return floor === "owner"
    ? "Read-only for your role. Ask the team owner to change this."
    : floor === "admin"
      ? "Read-only for your role. Ask a team admin to change this."
      : "Read-only for your role: changing this needs team member access.";
}

const refusalFor = (floor: TeamRole) => (floor === "owner" ? "Your role can't do this. Ask the team owner." : "Your role can't do this. Ask a team admin.");

/** The sentence for a role-matrix refusal (`roles.rs` and `admin::require_team` answer 403 "insufficient team role ..."), or `null` when `body` is anything else. */
export function roleRefusalSentence(body: string): string | null {
  const text = body.trim();
  if (!/^insufficient team role/i.test(text)) return null;
  const needs = /this needs (\w+) or above/i.exec(text)?.[1]?.toLowerCase();
  return refusalFor(needs === "owner" ? "owner" : "admin");
}

/** The same sentence for an action whose role floor this side already knows the caller misses (a server route refusing BEFORE it creates anything the backend would then refuse). */
export function roleRefusalFor(action: TeamAction): string {
  return refusalFor(TEAM_ACTIONS[action].floor);
}
