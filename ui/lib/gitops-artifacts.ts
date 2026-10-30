// Minimal dependency-free YAML emitter + the OpenEdge org-config builder: PURE, building the artifact tree for the server-side GitOps sync
// routes (`/api/gitops/sync`, `/api/gitops/init` via `gitops-server.ts`; `gitops-yaml.ts` re-exports it under a `server-only` guard). Supports
// exactly what the config needs: nested maps, scalars, simple lists and lists of maps.

type Y = string | number | boolean | null | undefined | Y[] | { [k: string]: Y };

const NEEDS_QUOTE = /[:#\[\]{}&*!|>'"%@`,]|^\s|\s$|^[-?]|^$|^(true|false|null|yes|no|~)$/i;

function scalar(v: string | number | boolean | null | undefined): string {
  if (v === null || v === undefined) return "";
  if (typeof v === "number" || typeof v === "boolean") return String(v);
  const s = String(v);
  if (s === "" || NEEDS_QUOTE.test(s) || /^[+-]?\d/.test(s)) {
    return JSON.stringify(s);
  }
  return s;
}

function isMap(v: Y): v is { [k: string]: Y } {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

function emit(value: Y, indent: number, out: string[]) {
  const pad = "  ".repeat(indent);
  if (Array.isArray(value)) {
    if (value.length === 0) return;
    for (const item of value) {
      if (isMap(item)) {
        const entries = Object.entries(item).filter(([, v]) => v !== undefined);
        if (entries.length === 0) {
          out.push(`${pad}- {}`);
          continue;
        }
        entries.forEach(([k, v], i) => {
          const prefix = i === 0 ? `${pad}- ` : `${pad}  `;
          renderKey(prefix, k, v, indent + 2, out);
        });
      } else if (Array.isArray(item)) {
        out.push(`${pad}-`);
        emit(item, indent + 1, out);
      } else {
        out.push(`${pad}- ${scalar(item as any)}`);
      }
    }
    return;
  }
  if (isMap(value)) {
    for (const [k, v] of Object.entries(value)) {
      if (v === undefined) continue;
      renderKey(pad, k, v, indent + 1, out);
    }
  }
}

function renderKey(prefix: string, key: string, v: Y, childIndent: number, out: string[]) {
  if (Array.isArray(v)) {
    if (v.length === 0) {
      out.push(`${prefix}${key}: []`);
    } else {
      out.push(`${prefix}${key}:`);
      emit(v, childIndent, out);
    }
  } else if (isMap(v)) {
    if (Object.keys(v).length === 0) {
      out.push(`${prefix}${key}: {}`);
    } else {
      out.push(`${prefix}${key}:`);
      emit(v, childIndent, out);
    }
  } else {
    out.push(`${prefix}${key}: ${scalar(v as any)}`);
  }
}

/** Serialize a plain object/array to YAML text. */
export function toYaml(value: Y): string {
  const out: string[] = [];
  emit(value, 0, out);
  return out.join("\n") + "\n";
}

export interface OrgConfigInput {
  org: { name: string; slug: string; plan?: string };
  generatedAt: string;
  projects: Array<{
    project: string;
    settings?: any;
    git?: { repo_url?: string; branch?: string; commit?: string } | null;
    production?: any;
    root_dir?: string;
    /** Enterprise deployment protection (mode/scope only — never the password). */
    protection?: { mode?: string; scope?: string } | null;
  }>;
  /** Raw database records (e.g. straight off `GET /v1/databases`) that may carry a `connection` credentials map; only databaseNode()'s allowlist
   *  (name/kind/region/mode) reaches the YAML. */
  databases?: Array<{
    name: string;
    kind?: string;
    region?: string;
    mode?: string;
    [k: string]: any;
  }>;
}

type ProjectInput = OrgConfigInput["projects"][number];
type DatabaseInput = OrgConfigInput["databases"] extends Array<infer D> | undefined ? D : never;

/** The declarative spec for a single project (shared by org + per-project files). */
export function projectNode(p: ProjectInput): { [k: string]: Y } {
  const s = p.settings || {};
  const fns = s.functions || {};
  const build = s.build || {};
  const envKeys = Array.isArray(s.env)
    ? s.env.map((e: any) => ({ key: e.key, target: e.target, sensitive: !!e.sensitive }))
    : [];
  const node: { [k: string]: Y } = {
    name: p.project,
    framework: build.framework || "auto",
  };
  if (p.git?.repo_url) {
    node.git = { repo: p.git.repo_url, branch: p.git.branch || "main" };
    if (p.git.commit) node.git = { ...(node.git as object), commit: p.git.commit };
  }
  if (p.root_dir) node.rootDir = p.root_dir;
  node.build = {
    installCommand: build.install_command || "",
    buildCommand: build.build_command || "",
    outputDirectory: build.output_dir || "",
  };
  node.functions = {
    runtime: fns.fluid_enabled === false ? "edge" : "fluid",
    maxDuration: fns.default_max_duration_secs ?? 10,
    memory: fns.memory_mib ?? 512,
    regions: Array.isArray(fns.regions) ? fns.regions : [],
    failover: !!fns.failover,
  };
  if (Array.isArray(s.domains) && s.domains.length) node.domains = s.domains;
  if (envKeys.length) node.env = envKeys;
  if (p.protection?.mode && p.protection.mode !== "off") {
    node.protection = { mode: p.protection.mode, scope: p.protection.scope || "preview" };
  }
  return node;
}

/** The declarative spec for one database: an explicit field allowlist, NEVER a spread of the raw record, so `connection` (credentials) and any
 *  other credential-shaped field cannot reach the committed YAML (the no-secrets-in-git rule `projectNode()` follows for env values). */
export function databaseNode(d: DatabaseInput): { [k: string]: Y } {
  return {
    name: d.name,
    kind: d.kind || "unknown",
    region: d.region || "",
    mode: d.mode || "live",
  };
}

/** Builds the declarative OpenEdge config: org/team meta plus a `projects:` list. Env values are NOT serialized (only keys and targets) so
 *  secrets never land in git. */
export function buildOrgConfig(input: OrgConfigInput): Y {
  return {
    apiVersion: "openedge.dev/v1",
    kind: "Org",
    metadata: {
      name: input.org.name,
      slug: input.org.slug,
      plan: input.org.plan || "pro",
      generatedAt: input.generatedAt,
      managedBy: "OpenEdge GitOps",
    },
    projects: (input.projects || []).map(projectNode),
    databases: (input.databases || []).map(databaseNode),
  };
}

function slugFile(s: string): string {
  return (s || "project").toLowerCase().replace(/[^a-z0-9-_]+/g, "-").replace(/^-+|-+$/g, "") || "project";
}

export interface ArtifactInput extends OrgConfigInput {
  region?: string;
  rootPath?: string;
  /** Platform-level declarative config (CDN routing, enterprise governance).
   *  Secrets are NEVER included — counts/modes only. */
  platform?: {
    routing?: { redirects?: any[]; rewrites?: any[] };
    enterprise?: {
      ipBlocks?: { prefix: string; note?: string }[];
      siem?: { enabled: boolean; format?: string };
      saml?: { enabled: boolean; enforced?: boolean };
      scim?: { enabled: boolean };
      microfrontends?: { name: string; host: string; children: { project: string; path: string }[] }[];
    };
  };
}

/** The full GitOps artifact tree for the config repo: `openedge.yaml` (org spec, projects[] list), `README.md`, `.openedge/meta.yaml`,
 *  `projects/<name>.yaml` (one per project), `.github/workflows/openedge-deploy.yml` (CI that pings the OpenEdge webhook) and `.gitignore`. */
export function buildArtifacts(input: ArtifactInput): { path: string; content: string }[] {
  const rootPath = input.rootPath || "openedge.yaml";
  const projects = input.projects || [];
  const files: { path: string; content: string }[] = [];

  files.push({ path: rootPath, content: toYaml(buildOrgConfig(input)) });

  files.push({
    path: ".openedge/meta.yaml",
    content: toYaml({
      apiVersion: "openedge.dev/v1",
      kind: "Meta",
      org: input.org.slug,
      name: input.org.name,
      plan: input.org.plan || "pro",
      region: input.region || "",
      projectCount: projects.length,
      generatedAt: input.generatedAt,
      managedBy: "OpenEdge GitOps",
    }),
  });

  for (const p of projects) {
    files.push({
      path: `projects/${slugFile(p.project)}.yaml`,
      content: toYaml({
        apiVersion: "openedge.dev/v1",
        kind: "Project",
        org: input.org.slug,
        spec: projectNode(p),
      }),
    });
  }

  if (input.platform && (input.platform.routing || input.platform.enterprise)) {
    files.push({
      path: ".openedge/platform.yaml",
      content: toYaml({
        apiVersion: "openedge.dev/v1",
        kind: "Platform",
        org: input.org.slug,
        routing: input.platform.routing
          ? {
              redirects: (input.platform.routing.redirects || []).map((r: any) => ({
                source: r.source, destination: r.destination, status: r.status,
              })),
              rewrites: (input.platform.routing.rewrites || []).map((r: any) => ({
                source: r.source, destination: r.destination,
              })),
            }
          : undefined,
        enterprise: input.platform.enterprise,
      }),
    });
  }

  files.push({ path: "README.md", content: readme(input) });
  files.push({ path: ".github/workflows/openedge-deploy.yml", content: deployWorkflowContent() });
  files.push({
    path: ".gitignore",
    content: "# OpenEdge GitOps — managed config repo\nnode_modules/\n.env\n.env.*\n.DS_Store\n",
  });

  return files;
}

function readme(input: ArtifactInput): string {
  const list = (input.projects || [])
    .map((p) => `- \`${p.project}\`${p.git?.repo_url ? ` — ${p.git.repo_url}` : ""}`)
    .join("\n");
  return `# ${input.org.name} — OpenEdge GitOps

> This repository is **auto-generated and managed by OpenEdge GitOps**. It is the
> declarative source of truth for the **${input.org.slug}** org/team.

Changes to runtime, regions, tier, env keys, domains or project settings in the
OpenEdge dashboard are committed back here automatically. Pushing changes to a
project's source repo triggers a new build + deployment.

## Layout

| Path | Description |
|------|-------------|
| \`openedge.yaml\` | Org spec — all projects as a YAML list |
| \`.openedge/meta.yaml\` | Platform meta state |
| \`projects/*.yaml\` | One spec file per project |
| \`.github/workflows/openedge-deploy.yml\` | CI that triggers OpenEdge on push |

## Projects (${(input.projects || []).length})

${list || "_No projects yet — minimal meta state._"}

---
_Plan: ${input.org.plan || "pro"} · Generated ${input.generatedAt}_
`;
}

/** The OpenEdge deploy GitHub Actions workflow (also installed into project repos). */
export function deployWorkflowContent(): string {
  return `# Auto-generated by OpenEdge GitOps — repos become deployable workflows.
# OPENEDGE_WEBHOOK_URL is auto-set as a repo variable by OpenEdge (the public URL
# of your node). Optionally add an OPENEDGE_WEBHOOK_SIG secret if the node sets
# GITHUB_WEBHOOK_SECRET (the same value) — the signature is computed here, not sent raw.
name: OpenEdge Deploy

on:
  push:
    branches: [main]
  pull_request:
    types: [opened, synchronize, reopened]

jobs:
  deploy:
    runs-on: ubuntu-latest
    steps:
      - name: Trigger OpenEdge build & deploy
        env:
          OPENEDGE_WEBHOOK_URL: \${{ vars.OPENEDGE_WEBHOOK_URL || secrets.OPENEDGE_WEBHOOK_URL }}
          OPENEDGE_WEBHOOK_SIG: \${{ secrets.OPENEDGE_WEBHOOK_SIG }}
          PR_NUMBER: \${{ github.event.pull_request.number }}
          PR_HEAD_REF: \${{ github.event.pull_request.head.ref }}
          PR_HEAD_SHA: \${{ github.event.pull_request.head.sha }}
        run: |
          if [ -z "$OPENEDGE_WEBHOOK_URL" ]; then
            echo "OPENEDGE_WEBHOOK_URL not set — skipping OpenEdge deploy trigger."
            exit 0
          fi
          if [ "$GITHUB_EVENT_NAME" = "pull_request" ]; then
            event="pull_request"
            payload=$(printf '{"action":"synchronize","number":%s,"pull_request":{"number":%s,"head":{"ref":"%s","sha":"%s"}},"repository":{"full_name":"%s"}}' \\
              "$PR_NUMBER" "$PR_NUMBER" "$PR_HEAD_REF" "$PR_HEAD_SHA" "$GITHUB_REPOSITORY")
          else
            event="push"
            payload=$(printf '{"ref":"%s","after":"%s","repository":{"full_name":"%s"}}' "$GITHUB_REF" "$GITHUB_SHA" "$GITHUB_REPOSITORY")
          fi
          sig_header=""
          if [ -n "$OPENEDGE_WEBHOOK_SIG" ]; then
            sig=$(printf '%s' "$payload" | openssl dgst -sha256 -hmac "$OPENEDGE_WEBHOOK_SIG" | awk '{print $NF}')
            sig_header="sha256=$sig"
          fi
          curl -sS -X POST "$OPENEDGE_WEBHOOK_URL/v1/git/webhook" \\
            -H "x-github-event: $event" \\
            -H "content-type: application/json" \\
            \${sig_header:+-H "x-hub-signature-256: $sig_header"} \\
            -d "$payload" || echo "OpenEdge webhook call failed (non-fatal)."
`;
}
