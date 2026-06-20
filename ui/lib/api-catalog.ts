// The shadw REST API catalog — the single source of truth for the API Reference
// docs: the left-sidebar tree, the Endpoints overview grid, and every generated
// per-method page. Mirrors the real node admin API (see crates/hive-cloud).

export type HttpMethod = "GET" | "POST" | "PUT" | "PATCH" | "DELETE";

export interface ApiParam {
  name: string;
  type: string;
  required?: boolean;
  desc: string;
}

export interface ApiEndpoint {
  /** URL slug within its category, e.g. "create-a-deployment". */
  slug: string;
  /** Human title, e.g. "Create a deployment". */
  name: string;
  method: HttpMethod;
  /** Path with `{param}` placeholders, e.g. "/v1/projects/{project}/redeploy". */
  path: string;
  summary: string;
  pathParams?: ApiParam[];
  queryParams?: ApiParam[];
  bodyParams?: ApiParam[];
  /** Pretty-printed sample JSON response. */
  response?: string;
}

export interface ApiCategory {
  slug: string;
  name: string;
  endpoints: ApiEndpoint[];
}

/** Public base URL documented for the API. */
export const API_BASE = "https://api.shadw.cloud";

export const API_CATEGORIES: ApiCategory[] = [
  {
    slug: "deployments",
    name: "Deployments",
    endpoints: [
      {
        slug: "create-a-deployment",
        name: "Create a deployment",
        method: "POST",
        path: "/v1/git/deploy",
        summary: "Clone, build and deploy a Git repository. Returns a build id you can stream logs from. The branch (vs the project's production branch) decides production vs preview.",
        bodyParams: [
          { name: "repo_url", type: "string", required: true, desc: "Git repository URL to clone and build." },
          { name: "branch", type: "string", desc: "Branch to deploy (defaults to the repo's default branch)." },
          { name: "project", type: "string", desc: "Project name (auto-generated when omitted)." },
          { name: "root_dir", type: "string", desc: "Subdirectory to build, for monorepos." },
          { name: "target", type: '"production" | "preview"', desc: "Force the environment; omit to classify by branch." },
          { name: "use_cache", type: "boolean", desc: "Reuse the dependency build cache (default true)." },
        ],
        response: `{
  "build_id": "dpl-9f2a1c7b04",
  "project": "acme-app"
}`,
      },
      {
        slug: "get-a-build",
        name: "Get a build",
        method: "GET",
        path: "/v1/builds/{id}",
        summary: "Poll a build's status and streamed logs. State is one of building, ready or error.",
        pathParams: [{ name: "id", type: "string", required: true, desc: "The build id returned by Create a deployment." }],
        response: `{
  "id": "dpl-9f2a1c7b04",
  "project": "acme-app",
  "state": "ready",
  "branch": "main",
  "commit": "3b273ff",
  "deployment_id": "dpl-46f4fb3397",
  "alias": "acme-app.localhost"
}`,
      },
      {
        slug: "promote-a-deployment",
        name: "Promote a deployment",
        method: "POST",
        path: "/v1/deployments/{id}/promote",
        summary: "Promote (or roll back to) a deployment — instantly re-points the production alias to it. No rebuild.",
        pathParams: [{ name: "id", type: "string", required: true, desc: "The deployment id to promote." }],
        response: `{
  "id": "dpl-46f4fb3397",
  "project": "acme-app",
  "production": true,
  "alias": "acme-app.localhost"
}`,
      },
      {
        slug: "redeploy-a-project",
        name: "Redeploy a project",
        method: "POST",
        path: "/v1/projects/{project}/redeploy",
        summary: "Rebuild a project's latest source with the current project settings.",
        pathParams: [{ name: "project", type: "string", required: true, desc: "The project name." }],
        bodyParams: [
          { name: "target", type: '"production" | "preview"', desc: "Environment for the new deployment." },
          { name: "use_cache", type: "boolean", desc: "Reuse the existing build cache (default true)." },
        ],
        response: `{ "build_id": "dpl-7c1e02ab93" }`,
      },
      {
        slug: "delete-a-deployment",
        name: "Delete a deployment",
        method: "DELETE",
        path: "/v1/deployments/{id}",
        summary: "Delete a single deployment and unregister its functions.",
        pathParams: [{ name: "id", type: "string", required: true, desc: "The deployment id to delete." }],
        response: `{ "removed": "dpl-46f4fb3397", "project": "acme-app" }`,
      },
    ],
  },
  {
    slug: "projects",
    name: "Projects",
    endpoints: [
      {
        slug: "get-project-settings",
        name: "Get project settings",
        method: "GET",
        path: "/v1/projects/{project}/settings",
        summary: "Read a project's settings (build config, env, domains, production branch, preview protection).",
        pathParams: [{ name: "project", type: "string", required: true, desc: "The project name." }],
        response: `{
  "production_branch": "main",
  "build": { "framework": "", "install_command": "", "build_command": "", "output_dir": "", "root_dir": "" },
  "env": [],
  "domains": [],
  "preview_protection": true,
  "team": "personal"
}`,
      },
      {
        slug: "set-an-environment-variable",
        name: "Set an environment variable",
        method: "POST",
        path: "/v1/projects/{project}/env",
        summary: "Add or update an environment variable. Sensitive values are encrypted at rest (ChaCha20-Poly1305).",
        pathParams: [{ name: "project", type: "string", required: true, desc: "The project name." }],
        bodyParams: [
          { name: "key", type: "string", required: true, desc: "Variable name." },
          { name: "value", type: "string", required: true, desc: "Value (sealed at rest when sensitive)." },
          { name: "target", type: '"production" | "preview" | "all"', desc: "Which environments it applies to." },
          { name: "sensitive", type: "boolean", desc: "Encrypt at rest and mask in the UI." },
        ],
        response: `{ "key": "DATABASE_URL", "target": "all", "sensitive": true }`,
      },
      {
        slug: "delete-an-environment-variable",
        name: "Delete an environment variable",
        method: "DELETE",
        path: "/v1/projects/{project}/env/{key}",
        summary: "Remove an environment variable from a project.",
        pathParams: [
          { name: "project", type: "string", required: true, desc: "The project name." },
          { name: "key", type: "string", required: true, desc: "The variable name to delete." },
        ],
        response: `{ "deleted": true }`,
      },
      {
        slug: "delete-a-project",
        name: "Delete a project",
        method: "DELETE",
        path: "/v1/projects/{project}",
        summary: "Delete a project and all of its deployments.",
        pathParams: [{ name: "project", type: "string", required: true, desc: "The project name." }],
        response: `{ "project": "acme-app", "removed_deployments": ["dpl-46f4fb3397"] }`,
      },
    ],
  },
  {
    slug: "domains",
    name: "Domains",
    endpoints: [
      {
        slug: "list-domains",
        name: "List domains",
        method: "GET",
        path: "/v1/domains",
        summary: "List the custom domains for the active team.",
        response: `[
  { "domain": "acme.com", "project": "acme-app" }
]`,
      },
      {
        slug: "attach-a-domain",
        name: "Attach a domain to a project",
        method: "POST",
        path: "/v1/projects/{project}/domains",
        summary: "Attach a custom domain to a project. The domain's root label is aliased to the project's current deployment.",
        pathParams: [{ name: "project", type: "string", required: true, desc: "The project name." }],
        bodyParams: [{ name: "domain", type: "string", required: true, desc: "The fully-qualified domain to attach." }],
        response: `{ "domain": "acme.com", "project": "acme-app", "attached": true }`,
      },
      {
        slug: "get-a-domain",
        name: "Get a domain",
        method: "GET",
        path: "/v1/domains/{domain}",
        summary: "Get a domain with its DNS records, nameservers, SSL certificate and connected projects.",
        pathParams: [{ name: "domain", type: "string", required: true, desc: "The domain name." }],
        response: `{
  "domain": "acme.com",
  "nameservers": ["ns1.shadw.cloud", "ns2.shadw.cloud"],
  "auto_renew": true,
  "records": [
    { "id": "rec_a1", "type": "A", "name": "@", "value": "76.76.21.21", "ttl": 3600 }
  ]
}`,
      },
      {
        slug: "set-nameservers",
        name: "Set nameservers",
        method: "PUT",
        path: "/v1/domains/{domain}/nameservers",
        summary: "Set the domain's nameservers (when delegating the whole zone to shadw).",
        pathParams: [{ name: "domain", type: "string", required: true, desc: "The domain name." }],
        bodyParams: [{ name: "nameservers", type: "string[]", required: true, desc: "Nameservers to delegate the zone to." }],
        response: `{ "domain": "acme.com", "nameservers": ["ns1.shadw.cloud", "ns2.shadw.cloud"] }`,
      },
      {
        slug: "renew-ssl",
        name: "Renew the SSL certificate",
        method: "POST",
        path: "/v1/domains/{domain}/ssl/renew",
        summary: "Reissue the free managed (Let's Encrypt-style) certificate for the domain.",
        pathParams: [{ name: "domain", type: "string", required: true, desc: "The domain name." }],
        response: `{ "id": "cert_…", "provider": "shadw (Let's Encrypt)", "renewal": "auto" }`,
      },
    ],
  },
  {
    slug: "dns",
    name: "DNS",
    endpoints: [
      {
        slug: "create-a-dns-record",
        name: "Create a DNS record",
        method: "POST",
        path: "/v1/domains/{domain}/records",
        summary: "Create a DNS record. shadw's authoritative DNS server answers queries for the domain from these records.",
        pathParams: [{ name: "domain", type: "string", required: true, desc: "The domain name." }],
        bodyParams: [
          { name: "type", type: '"A" | "AAAA" | "CNAME" | "TXT" | "MX" | "NS"', required: true, desc: "Record type." },
          { name: "name", type: "string", required: true, desc: 'Subdomain (e.g. "www") or "@" for the apex.' },
          { name: "value", type: "string", required: true, desc: "Record value (IP, hostname, text…)." },
          { name: "ttl", type: "number", desc: "Time-to-live in seconds (default 3600)." },
          { name: "priority", type: "number", desc: "Priority (MX records)." },
        ],
        response: `{ "id": "rec_a1", "type": "A", "name": "@", "value": "76.76.21.21", "ttl": 3600 }`,
      },
      {
        slug: "update-a-dns-record",
        name: "Update a DNS record",
        method: "PUT",
        path: "/v1/domains/{domain}/records/{id}",
        summary: "Edit an existing DNS record (system-managed records are immutable).",
        pathParams: [
          { name: "domain", type: "string", required: true, desc: "The domain name." },
          { name: "id", type: "string", required: true, desc: "The record id." },
        ],
        bodyParams: [
          { name: "type", type: "string", required: true, desc: "Record type." },
          { name: "name", type: "string", required: true, desc: "Subdomain or @ for the apex." },
          { name: "value", type: "string", required: true, desc: "Record value." },
          { name: "ttl", type: "number", desc: "Time-to-live in seconds." },
        ],
        response: `{ "id": "rec_a1", "type": "A", "name": "@", "value": "76.76.21.22", "ttl": 120 }`,
      },
      {
        slug: "delete-a-dns-record",
        name: "Delete a DNS record",
        method: "DELETE",
        path: "/v1/domains/{domain}/records/{id}",
        summary: "Delete a DNS record.",
        pathParams: [
          { name: "domain", type: "string", required: true, desc: "The domain name." },
          { name: "id", type: "string", required: true, desc: "The record id." },
        ],
        response: `{ "deleted": true }`,
      },
      {
        slug: "import-dns-records",
        name: "Import DNS records",
        method: "POST",
        path: "/v1/domains/{domain}/import",
        summary: "Bulk-import records (DNS migration) from a list and/or a pasted BIND zone file. Idempotent — exact duplicates are skipped.",
        pathParams: [{ name: "domain", type: "string", required: true, desc: "The domain name." }],
        bodyParams: [
          { name: "records", type: "DnsRecord[]", desc: "Structured records to import." },
          { name: "zone", type: "string", desc: "Raw BIND-style zone text to parse and import." },
        ],
        response: `{ "imported": 3, "records": [ /* … */ ] }`,
      },
      {
        slug: "scan-existing-dns",
        name: "Scan existing DNS",
        method: "GET",
        path: "/v1/domains/{domain}/scan",
        summary: "Detect a domain's CURRENT public DNS records (via DNS-over-HTTPS) so they can be migrated into the console with one click. Records are returned, not added.",
        pathParams: [{ name: "domain", type: "string", required: true, desc: "The domain name." }],
        response: `{
  "domain": "acme.com",
  "records": [
    { "name": "", "type": "A", "value": "76.76.21.21", "ttl": 300, "priority": null }
  ]
}`,
      },
    ],
  },
  {
    slug: "webhooks",
    name: "Webhooks",
    endpoints: [
      {
        slug: "list-webhooks",
        name: "List webhooks",
        method: "GET",
        path: "/v1/webhooks",
        summary: "List outbound webhooks for the active team.",
        response: `[
  { "id": "wh_1", "url": "https://example.com/hook", "events": ["deployment.ready"] }
]`,
      },
      {
        slug: "list-webhook-events",
        name: "List webhook events",
        method: "GET",
        path: "/v1/webhooks/events",
        summary: "List the event types a webhook can subscribe to.",
        response: `[
  "deployment.created",
  "deployment.ready",
  "deployment.promoted",
  "deployment.error",
  "domain.added"
]`,
      },
      {
        slug: "inbound-git-webhook",
        name: "Inbound Git webhook",
        method: "POST",
        path: "/v1/git/webhook",
        summary: "Inbound GitHub webhook. A push to the production branch deploys to production; any other branch or a pull request builds a preview.",
        response: `{
  "repo": "acme/app",
  "branch": "main",
  "event": "push",
  "triggered": 1,
  "builds": [{ "project": "acme-app", "build_id": "dpl-…", "target": "auto" }]
}`,
      },
      {
        slug: "delete-a-webhook",
        name: "Delete a webhook",
        method: "DELETE",
        path: "/v1/webhooks/{id}",
        summary: "Delete an outbound webhook.",
        pathParams: [{ name: "id", type: "string", required: true, desc: "The webhook id." }],
        response: `{ "deleted": true }`,
      },
    ],
  },
  {
    slug: "teams",
    name: "Teams",
    endpoints: [
      {
        slug: "list-teams",
        name: "List teams",
        method: "GET",
        path: "/v1/teams",
        summary: "List the teams you belong to.",
        response: `[ { "slug": "acme", "name": "Acme", "plan": "pro" } ]`,
      },
      {
        slug: "get-a-team",
        name: "Get a team",
        method: "GET",
        path: "/v1/teams/{slug}",
        summary: "Get a team's details, members and plan.",
        pathParams: [{ name: "slug", type: "string", required: true, desc: "The team slug." }],
        response: `{ "slug": "acme", "name": "Acme", "plan": "pro", "members": [] }`,
      },
      {
        slug: "add-a-team-member",
        name: "Add a team member",
        method: "POST",
        path: "/v1/teams/{slug}/members",
        summary: "Invite a member to a team.",
        pathParams: [{ name: "slug", type: "string", required: true, desc: "The team slug." }],
        bodyParams: [
          { name: "email", type: "string", required: true, desc: "The member's email." },
          { name: "role", type: '"owner" | "member"', desc: "Role to assign (default member)." },
        ],
        response: `{ "team": "acme", "email": "dev@acme.com", "role": "member" }`,
      },
      {
        slug: "remove-a-team-member",
        name: "Remove a team member",
        method: "DELETE",
        path: "/v1/teams/{slug}/members/{email}",
        summary: "Remove a member from a team.",
        pathParams: [
          { name: "slug", type: "string", required: true, desc: "The team slug." },
          { name: "email", type: "string", required: true, desc: "The member's email." },
        ],
        response: `{ "removed": "dev@acme.com" }`,
      },
    ],
  },
  {
    slug: "tokens",
    name: "Tokens & API Keys",
    endpoints: [
      {
        slug: "mint-a-token",
        name: "Mint a token",
        method: "POST",
        path: "/v1/token",
        summary: "Mint a short-lived bearer token scoped to a team + role.",
        bodyParams: [
          { name: "tenant", type: "string", desc: "Team slug the token is scoped to." },
          { name: "role", type: '"owner" | "member"', desc: "Role embedded in the token." },
        ],
        response: `{ "token": "…", "tenant": "personal", "role": "owner" }`,
      },
      {
        slug: "list-api-keys",
        name: "List API keys",
        method: "GET",
        path: "/v1/apikeys",
        summary: "List the long-lived API keys for the active team.",
        response: `[ { "id": "key_1", "team": "acme", "created_ms": 1781906630000 } ]`,
      },
      {
        slug: "delete-an-api-key",
        name: "Delete an API key",
        method: "DELETE",
        path: "/v1/apikeys/{id}",
        summary: "Revoke an API key.",
        pathParams: [{ name: "id", type: "string", required: true, desc: "The API key id." }],
        response: `{ "deleted": true }`,
      },
    ],
  },
  {
    slug: "observability",
    name: "Observability",
    endpoints: [
      {
        slug: "get-overview",
        name: "Get the overview",
        method: "GET",
        path: "/v1/overview",
        summary: "Platform overview: deployments, regions, request counters and WAF status.",
        response: `{ "deployments": 12, "regions": ["los-angeles"], "requests": 90210 }`,
      },
      {
        slug: "get-logs",
        name: "Get logs",
        method: "GET",
        path: "/v1/logs",
        summary: "Recent request + control-plane events for the active team.",
        queryParams: [{ name: "limit", type: "number", desc: "Max events to return (default 100)." }],
        response: `[ { "ts_ms": 1781906630000, "method": "GET", "host": "acme-app.localhost", "status": 200 } ]`,
      },
      {
        slug: "get-resources",
        name: "Get cluster resources",
        method: "GET",
        path: "/v1/resources",
        summary: "Live cluster resource totals (CPU, memory, disk) across the mesh and this node's usage.",
        response: `{ "cluster": { "nodes": 2, "cpu_cores": 16, "mem_total_mb": 32768 } }`,
      },
    ],
  },
];

// ---- helpers ----

export const METHOD_LABEL: Record<HttpMethod, string> = {
  GET: "GET",
  POST: "POST",
  PUT: "PUT",
  PATCH: "PATCH",
  DELETE: "DEL",
};

/** Tailwind badge classes per method (Vercel-style coloring). */
export const METHOD_TONE: Record<HttpMethod, string> = {
  GET: "bg-emerald-500/15 text-emerald-600 dark:text-emerald-400",
  POST: "bg-blue-500/15 text-blue-600 dark:text-blue-400",
  PUT: "bg-amber-500/15 text-amber-600 dark:text-amber-400",
  PATCH: "bg-amber-500/15 text-amber-600 dark:text-amber-400",
  DELETE: "bg-red-500/15 text-red-600 dark:text-red-400",
};

export function endpointHref(catSlug: string, epSlug: string): string {
  return `/docs/api-reference/${catSlug}/${epSlug}`;
}

export function findEndpoint(catSlug: string, epSlug: string): { category: ApiCategory; endpoint: ApiEndpoint } | null {
  const category = API_CATEGORIES.find((c) => c.slug === catSlug);
  if (!category) return null;
  const endpoint = category.endpoints.find((e) => e.slug === epSlug);
  if (!endpoint) return null;
  return { category, endpoint };
}

/** The full documented URL for an endpoint (base + path). */
export function endpointUrl(ep: ApiEndpoint): string {
  return `${API_BASE}${ep.path}`;
}

/** Generate the TypeScript / Next.js / cURL request snippets for the code panel. */
export function codeExamples(ep: ApiEndpoint): { typescript: string; nextjs: string; curl: string } {
  const url = endpointUrl(ep);
  const hasBody = !!ep.bodyParams?.length;
  const bodyObj = hasBody
    ? `{
${ep.bodyParams!.map((p) => `    ${JSON.stringify(p.name)}: ${exampleValue(p)}`).join(",\n")}
  }`
    : null;

  const tsHeaders = `{
    'Authorization': 'Bearer YOUR_ACCESS_TOKEN',${hasBody ? `\n    'Content-Type': 'application/json',` : ""}
  }`;
  const typescript = `const response = await fetch('${url}', {
  method: '${ep.method}',
  headers: ${tsHeaders},${bodyObj ? `\n  body: JSON.stringify(${bodyObj}),` : ""}
});

const data = await response.json();
console.log(data);`;

  const nextjs = `// app/api/example/route.ts
export async function GET() {
  const res = await fetch('${url}', {
    method: '${ep.method}',
    headers: {
      Authorization: \`Bearer \${process.env.SHADW_TOKEN}\`,${hasBody ? `\n      'Content-Type': 'application/json',` : ""}
    },${bodyObj ? `\n    body: JSON.stringify(${bodyObj}),` : ""}
  });
  return Response.json(await res.json());
}`;

  const curlBody = bodyObj
    ? ` \\\n  -H 'Content-Type: application/json' \\\n  -d '${curlJson(ep)}'`
    : "";
  const curl = `curl -X ${ep.method} ${url} \\
  -H 'Authorization: Bearer YOUR_ACCESS_TOKEN'${curlBody}`;

  return { typescript, nextjs, curl };
}

function exampleValue(p: ApiParam): string {
  const t = p.type.toLowerCase();
  if (t.includes("bool")) return "true";
  if (t.includes("number")) return "3600";
  if (t.includes("[]")) return "[]";
  if (p.type.includes('"')) {
    const first = p.type.split("|")[0].trim();
    return first || `"${p.name}"`;
  }
  return `"${p.name}"`;
}

function curlJson(ep: ApiEndpoint): string {
  const obj = (ep.bodyParams || [])
    .map((p) => `"${p.name}": ${exampleValue(p)}`)
    .join(", ");
  return `{ ${obj} }`;
}

export function totalEndpoints(): number {
  return API_CATEGORIES.reduce((n, c) => n + c.endpoints.length, 0);
}
