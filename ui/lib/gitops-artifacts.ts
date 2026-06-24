// Minimal, dependency-free YAML emitter + the OpenEdge org-config builder.
//
// PURE + isomorphic: no `server-only` guard, so it runs in BOTH the server-side
// GitOps sync routes AND the client-side local provider (`gitops-local.ts`), which
// materializes the same artifact tree into an in-browser git repo when no remote
// config repo is linked. `gitops-yaml.ts` re-exports this under a server-only guard
// for the existing server consumers — this file is the single source of truth.
//
// Supports exactly what the config needs: nested maps, scalars, **simple YAML
// lists** (arrays of scalars rendered as `- item`) and lists of maps. This keeps
// the committed `openedge.yaml` human-readable and round-trippable without adding
// a YAML dependency to the dashboard.

type Y = string | number | boolean | null | undefined | Y[] | { [k: string]: Y };

const NEEDS_QUOTE = /[:#\[\]{}&*!|>'"%@`,]|^\s|\s$|^[-?]|^$|^(true|false|null|yes|no|~)$/i;

function scalar(v: string | number | boolean | null | undefined): string {
  if (v === null || v === undefined) return "";
  if (typeof v === "number" || typeof v === "boolean") return String(v);
  const s = String(v);
  // Quote strings that could be misread as YAML syntax or other types.
  if (s === "" || NEEDS_QUOTE.test(s) || /^[+-]?\d/.test(s)) {
    return JSON.stringify(s); // double-quoted, properly escaped
  }
  return s;
}

function isMap(v: Y): v is { [k: string]: Y } {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

function emit(value: Y, indent: number, out: string[]) {
  const pad = "  ".repeat(indent);
  if (Array.isArray(value)) {
    if (value.length === 0) return; // caller renders `key: []`
    for (const item of value) {
      if (isMap(item)) {
        // List of maps: first key on the `- ` line, rest indented under it.
        const entries = Object.entries(item).filter(([, v]) => v !== undefined);
        if (entries.length === 0) {
          out.push(`${pad}- {}`);
          continue;
        }
        entries.forEach(([k, v], i) => {
          const prefix = i === 0 ? `${pad}- ` : `${pad}  `;
          // Item keys sit at column (indent*2 + 2); nested values go one level
          // deeper than that, i.e. indent + 2.
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

// ---- OpenEdge config model ----

export interface OrgConfigInput {
  org: { name: string; slug: string; plan?: string };
  generatedAt: string; // ISO timestamp (passed in — server can't use Date.now in some contexts)
  projects: Array<{
    project: string;
    settings?: any;
    git?: { repo_url?: string; branch?: string; commit?: string } | null;
    production?: any;
    root_dir?: string;
  }>;
}

type ProjectInput = OrgConfigInput["projects"][number];

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
  return node;
}

/**
 * Build the declarative OpenEdge config. Mirrors the live platform objects:
 * the org/team meta plus a `projects:` array (a simple YAML list). Env values are
 * intentionally NOT serialized (only their keys + which targets) so secrets never
 * land in git — the config is reproducible infra, not a secret store.
 */
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
    // Always present — an empty/minimal meta state is valid when no projects exist.
    projects: (input.projects || []).map(projectNode),
  };
}

function slugFile(s: string): string {
  return (s || "project").toLowerCase().replace(/[^a-z0-9-_]+/g, "-").replace(/^-+|-+$/g, "") || "project";
}

export interface ArtifactInput extends OrgConfigInput {
  region?: string;
  rootPath?: string; // canonical config file name (default openedge.yaml)
}

/**
 * The full GitOps artifact tree committed to the config repo — not just one file,
 * but the platform's declarative spec/config/meta set, suitable for `git add .`:
 *
 *   openedge.yaml                         org spec (projects[] as a YAML list)
 *   README.md                             human overview (auto-managed)
 *   .openedge/meta.yaml                   platform meta state
 *   projects/<name>.yaml                  per-project spec (one file each)
 *   .github/workflows/openedge-deploy.yml CI that pings the OpenEdge webhook
 *   .gitignore
 */
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
  // Raw string (GitHub Actions \${{ }} expressions must not be YAML-quoted).
  //
  // The webhook URL is read from a repo *variable* (set automatically by OpenEdge,
  // not sensitive) with a secret fallback. Safe-by-default: if it's not set the job
  // *skips* (exit 0) instead of failing, and a webhook error is non-fatal — so a
  // fresh repo never shows a red ✗. OPENEDGE_WEBHOOK_SIG (optional) is a secret.
  return `# Auto-generated by OpenEdge GitOps — repos become deployable workflows.
# OPENEDGE_WEBHOOK_URL is auto-set as a repo variable by OpenEdge (the public URL
# of your node). Optionally add an OPENEDGE_WEBHOOK_SIG secret if the node sets
# GITHUB_WEBHOOK_SECRET.
name: OpenEdge Deploy

on:
  push:
    branches: [main]

jobs:
  deploy:
    runs-on: ubuntu-latest
    steps:
      - name: Trigger OpenEdge build & deploy
        env:
          OPENEDGE_WEBHOOK_URL: \${{ vars.OPENEDGE_WEBHOOK_URL || secrets.OPENEDGE_WEBHOOK_URL }}
          OPENEDGE_WEBHOOK_SIG: \${{ secrets.OPENEDGE_WEBHOOK_SIG }}
        run: |
          if [ -z "$OPENEDGE_WEBHOOK_URL" ]; then
            echo "OPENEDGE_WEBHOOK_URL not set — skipping OpenEdge deploy trigger."
            exit 0
          fi
          payload=$(printf '{"ref":"%s","after":"%s","repository":{"full_name":"%s"}}' "$GITHUB_REF" "$GITHUB_SHA" "$GITHUB_REPOSITORY")
          curl -sS -X POST "$OPENEDGE_WEBHOOK_URL/v1/git/webhook" \\
            -H "x-github-event: push" \\
            -H "content-type: application/json" \\
            \${OPENEDGE_WEBHOOK_SIG:+-H "x-hub-signature-256: $OPENEDGE_WEBHOOK_SIG"} \\
            -d "$payload" || echo "OpenEdge webhook call failed (non-fatal)."
`;
}
