"use client";

import { useCallback, useEffect, useState, use } from "react";
import { ChevronDown, MoreHorizontal, Plus, Trash2, X } from "lucide-react";
import { Badge, Button, Card, Input, SettingCard, Switch, Triangle } from "@/components/ui";
import { apiGet, apiSend } from "@/lib/api";
import { cn } from "@/lib/utils";

// ---- API shapes (mirror crate::microfrontends) ----
type MfeRoute = { group?: string; flag?: string; paths: string[] };
type MfeDevelopment = { local?: string; task?: string; fallback?: string };
type MfeMember = {
  project: string;
  role: string; // "default" | "child"
  routing: MfeRoute[];
  default_route?: string;
  package_name?: string;
  asset_prefix?: string;
  observability_routing: string; // "default_application" | "this_project"
  development?: MfeDevelopment;
};
type MfeGroup = {
  id: string;
  name: string;
  host_project: string;
  members: MfeMember[];
  enabled: boolean;
  fallback_environment: string; // "same_environment" | "production" | "custom"
  custom_fallback_environment_name?: string;
  disable_overrides: boolean;
  local_proxy_port?: number;
};
type ProjectMfe = {
  project: string;
  in_group: boolean;
  role?: string;
  group?: MfeGroup | null;
  groups: MfeGroup[];
};

const selectCls =
  "w-full rounded-md border border-border bg-card px-3 py-2 text-sm text-fg focus:border-border-strong focus:outline-none focus:ring-2 focus:ring-border";

function errText(e: unknown): string {
  const s = String(e);
  const m = s.match(/\{.*\}/);
  if (m) {
    try {
      const j = JSON.parse(m[0]);
      return j.error || s;
    } catch {
      /* fall through */
    }
  }
  return s;
}

export function MicrofrontendsSettings({ paramsPromise }: { paramsPromise: Promise<{ project: string }> }) {
  const params = use(paramsPromise);
  const project = decodeURIComponent(params.project);
  const [data, setData] = useState<ProjectMfe | null>(null);
  const [err, setErr] = useState("");
  const [notice, setNotice] = useState("");
  const [expanded, setExpanded] = useState<Record<string, boolean>>({});
  const [showCreate, setShowCreate] = useState(false);
  const [showAdd, setShowAdd] = useState(false);

  const load = useCallback(async () => {
    try {
      const d = await apiGet<ProjectMfe>(`/v1/projects/${encodeURIComponent(project)}/settings/microfrontends`, { fresh: true });
      setData(d);
      setErr("");
    } catch (e) {
      setErr(errText(e));
    }
  }, [project]);
  useEffect(() => {
    // Fetch-on-mount/project-change: load() sets state only after its
    // internal await resolves, syncing external (server) settings into React.
    // eslint-disable-next-line react-hooks/set-state-in-effect -- fetch effect; state is set only after an internal await, not synchronously
    load();
  }, [load]);

  async function call(method: string, path: string, body?: unknown) {
    setErr("");
    setNotice("");
    try {
      await apiSend(method, path, body);
      await load();
      return true;
    } catch (e) {
      setErr(errText(e));
      return false;
    }
  }

  if (err && !data)
    return <div className="rounded-lg border border-red-500/30 bg-red-500/5 p-4 text-sm text-red-500">Failed to load microfrontends: {err}</div>;
  if (!data) return <div className="text-sm text-secondary">Loading…</div>;

  const myGroup = data.group ?? null;

  return (
    <div className="flex flex-col gap-6">
      <div>
        <h2 className="text-xl font-semibold">Microfrontends</h2>
        <p className="mt-1.5 max-w-2xl text-sm text-secondary">
          Allow this project to use microfrontends to embed other applications in this one or to be embedded in another
          application. A new deployment is required for your changes to take effect.
        </p>
      </div>

      {err ? <div className="rounded-lg border border-red-500/30 bg-red-500/5 p-3 text-sm text-red-500">{err}</div> : null}
      {notice ? <div className="rounded-lg border border-green-500/30 bg-green-500/5 p-3 text-sm text-green-600">{notice}</div> : null}

      {/* ---- Groups ---- */}
      <SettingCard
        title="Groups"
        desc="Microfrontend groups visible to this team. A group composes one default application with child projects that own path routes."
        footer="A project can belong to one group."
        footerAction={
          <Button variant="outline" onClick={() => setShowCreate(true)}>
            <Plus className="h-4 w-4" /> Create a New Group
          </Button>
        }
      >
        {data.groups.length === 0 ? (
          <div className="rounded-lg border border-dashed border-border p-6 text-center text-sm text-secondary">
            No microfrontend groups yet. Create one to start composing projects.
          </div>
        ) : (
          <div className="flex flex-col gap-2">
            {data.groups.map((g) => (
              <GroupCard
                key={g.id}
                group={g}
                project={project}
                open={expanded[g.id] ?? false}
                onToggle={() => setExpanded((o) => ({ ...o, [g.id]: !(o[g.id] ?? false) }))}
                onAddSelf={() => call("POST", `/v1/microfrontends/groups/${g.id}/members`, { project })}
                onDeleteGroup={() => call("DELETE", `/v1/microfrontends/groups/${g.id}`)}
                onPromote={(p) => call("PATCH", `/v1/microfrontends/groups/${g.id}`, { defaultProjectId: p })}
                onRemoveMember={(p) => call("DELETE", `/v1/microfrontends/groups/${g.id}/members/${encodeURIComponent(p)}`)}
              />
            ))}
          </div>
        )}
      </SettingCard>

      {/* ---- Current project membership + config ---- */}
      {!data.in_group ? (
        <SettingCard
          title="This Project"
          desc="This project is not part of a microfrontend group."
          footerAction={
            data.groups.length > 0 ? (
              <Button variant="outline" onClick={() => setShowAdd(true)}>
                <Plus className="h-4 w-4" /> Add to Group
              </Button>
            ) : (
              <Button variant="outline" onClick={() => setShowCreate(true)}>
                <Plus className="h-4 w-4" /> Create a New Group
              </Button>
            )
          }
        >
          <div className="text-sm text-secondary">
            Add <span className="font-medium text-fg">{project}</span> to a group to route paths to it or make it the default
            application.
          </div>
        </SettingCard>
      ) : (
        myGroup && <MembershipConfig project={project} group={myGroup} call={call} />
      )}

      {showCreate && (
        <CreateGroupDialog
          project={project}
          onClose={() => setShowCreate(false)}
          onCreate={async (name) => {
            const ok = await call("POST", "/v1/microfrontends/groups", { name, defaultProjectId: project });
            if (ok) setShowCreate(false);
          }}
        />
      )}
      {showAdd && (
        <AddToGroupDialog
          groups={data.groups}
          onClose={() => setShowAdd(false)}
          onAdd={async (groupId) => {
            const ok = await call("POST", `/v1/microfrontends/groups/${groupId}/members`, { project });
            if (ok) setShowAdd(false);
          }}
        />
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Group card (accordion)
// ---------------------------------------------------------------------------
function GroupCard({
  group,
  project,
  open,
  onToggle,
  onAddSelf,
  onDeleteGroup,
  onPromote,
  onRemoveMember,
}: {
  group: MfeGroup;
  project: string;
  open: boolean;
  onToggle: () => void;
  onAddSelf: () => void;
  onDeleteGroup: () => void;
  onPromote: (p: string) => void;
  onRemoveMember: (p: string) => void;
}) {
  const [menu, setMenu] = useState(false);
  const isMember = group.members.some((m) => m.project === project);
  return (
    <Card className="p-0">
      <div className="flex items-center justify-between gap-2 px-4 py-3">
        <button className="flex min-w-0 flex-1 items-center gap-2 text-left" onClick={onToggle}>
          <ChevronDown className={cn("h-4 w-4 shrink-0 text-secondary transition-transform", open ? "" : "-rotate-90")} />
          <span className="truncate text-sm font-medium">{group.name}</span>
          <Badge tone="default">
            {group.members.length} project{group.members.length === 1 ? "" : "s"}
          </Badge>
          {!group.enabled ? <Badge tone="amber">disabled</Badge> : null}
        </button>
        <div className="flex items-center gap-1">
          {!isMember ? (
            <Button variant="outline" onClick={onAddSelf}>
              <Plus className="h-4 w-4" /> Add to Group
            </Button>
          ) : null}
          <div className="relative">
            <Button variant="ghost" aria-label="Group actions" onClick={() => setMenu((v) => !v)}>
              <MoreHorizontal className="h-4 w-4" />
            </Button>
            {menu && (
              <div
                className="absolute right-0 z-20 mt-1 w-44 rounded-lg border border-border bg-card p-1 shadow-pop"
                onMouseLeave={() => setMenu(false)}
              >
                <button
                  className="flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-sm text-red-500 hover:bg-red-500/10"
                  onClick={() => {
                    setMenu(false);
                    onDeleteGroup();
                  }}
                >
                  <Trash2 className="h-4 w-4" /> Delete Group
                </button>
              </div>
            )}
          </div>
        </div>
      </div>
      {open && (
        <div className="border-t border-border px-4 py-3">
          <ul className="flex flex-col gap-1.5">
            {group.members.map((m) => (
              <li key={m.project} className="flex items-center justify-between gap-2 rounded-md px-2 py-1.5 hover:bg-subtle">
                <span className="flex min-w-0 items-center gap-2">
                  <Triangle className="h-4 w-4 shrink-0" />
                  <span className={cn("truncate text-sm", m.project === project ? "font-semibold" : "")}>{m.project}</span>
                  {m.role === "default" ? <Badge tone="blue">Default</Badge> : null}
                  {m.project === project ? <Badge tone="green">This project</Badge> : null}
                </span>
                <span className="flex items-center gap-1">
                  {m.role !== "default" ? (
                    <>
                      <Button variant="ghost" onClick={() => onPromote(m.project)} title="Make default application">
                        Make default
                      </Button>
                      <Button variant="ghost" aria-label={`Remove ${m.project}`} onClick={() => onRemoveMember(m.project)}>
                        <X className="h-4 w-4" />
                      </Button>
                    </>
                  ) : null}
                </span>
              </li>
            ))}
          </ul>
        </div>
      )}
    </Card>
  );
}

// ---------------------------------------------------------------------------
// Per-project membership configuration cards
// ---------------------------------------------------------------------------
function MembershipConfig({
  project,
  group,
  call,
}: {
  project: string;
  group: MfeGroup;
  call: (method: string, path: string, body?: unknown) => Promise<boolean>;
}) {
  const me = group.members.find((m) => m.project === project);
  const isDefault = group.host_project === project;
  if (!me) return null;

  const putMember = (body: unknown) => call("PUT", `/v1/projects/${encodeURIComponent(project)}/settings/microfrontends`, body);
  const patchGroup = (body: unknown) => call("PATCH", `/v1/microfrontends/groups/${group.id}`, body);

  return (
    <div className="flex flex-col gap-6">
      <div className="rounded-lg border border-border bg-subtle px-4 py-3 text-sm">
        <span className="text-secondary">This project is the </span>
        <span className="font-medium">{isDefault ? "default application" : "a child application"}</span>
        <span className="text-secondary"> in group </span>
        <span className="font-medium">{group.name}</span>.
      </div>

      {!isDefault && <PathRoutingCard member={me} onSave={(routing) => putMember({ routing })} />}

      <DefaultRouteCard member={me} onSave={(default_route) => putMember({ defaultRoute: default_route })} />

      <FallbackEnvironmentCard group={group} onSave={(body) => patchGroup(body)} />

      <ObservabilityCard member={me} onSave={(v) => putMember({ observabilityRouting: v })} />

      {!isDefault && <AssetPrefixCard member={me} onSave={(assetPrefix) => putMember({ assetPrefix })} />}

      <LocalDevelopmentCard member={me} group={group} onSaveMember={(development) => putMember({ development })} onSaveGroup={patchGroup} />

      <SettingCard
        title="Remove from Group"
        desc={
          isDefault
            ? "This project is the default application. Remove all child projects (or promote another default) before removing it."
            : "Remove this project from the microfrontend group. Its routes stop composing under the default application's domain."
        }
        footerAction={
          <Button variant="danger" onClick={() => call("DELETE", `/v1/microfrontends/groups/${group.id}/members/${encodeURIComponent(project)}`)}>
            Remove from Group
          </Button>
        }
      >
        <span />
      </SettingCard>
    </div>
  );
}

function PathRoutingCard({ member, onSave }: { member: MfeMember; onSave: (routing: MfeRoute[]) => void }) {
  const [routes, setRoutes] = useState<MfeRoute[]>(() =>
    member.routing.length ? member.routing.map((r) => ({ ...r, paths: [...r.paths] })) : [{ paths: [""] }]
  );
  const dirty = JSON.stringify(routes) !== JSON.stringify(member.routing.length ? member.routing : [{ paths: [""] }]);

  function setPath(ri: number, pi: number, v: string) {
    setRoutes((rs) => rs.map((r, i) => (i === ri ? { ...r, paths: r.paths.map((p, j) => (j === pi ? v : p)) } : r)));
  }
  const invalid = routes.some((r) => r.paths.some((p) => p.trim() && !p.startsWith("/")));

  return (
    <SettingCard
      title="Path Routing"
      desc="Paths this child project owns under the default application's domain. Supports Vercel patterns like /docs/:path*."
      footer={invalid ? <span className="text-red-500">Every path must start with “/”.</span> : "One or more path patterns are required for a child project."}
      footerAction={
        <Button disabled={!dirty || invalid} onClick={() => onSave(routes.map((r) => ({ ...r, paths: r.paths.filter((p) => p.trim()) })).filter((r) => r.paths.length))}>
          Save
        </Button>
      }
    >
      <div className="flex flex-col gap-4">
        {routes.map((r, ri) => (
          <div key={ri} className="rounded-lg border border-border p-3">
            <div className="mb-2 grid grid-cols-1 gap-2 sm:grid-cols-2">
              <div>
                <label className="mb-1 block text-xs text-secondary">Group label (optional)</label>
                <Input value={r.group ?? ""} placeholder="marketing" onChange={(e) => setRoutes((rs) => rs.map((x, i) => (i === ri ? { ...x, group: e.target.value } : x)))} />
              </div>
              <div>
                <label className="mb-1 block text-xs text-secondary">Feature flag (optional)</label>
                <Input value={r.flag ?? ""} placeholder="new-docs" onChange={(e) => setRoutes((rs) => rs.map((x, i) => (i === ri ? { ...x, flag: e.target.value } : x)))} />
              </div>
            </div>
            <label className="mb-1 block text-xs text-secondary">Paths</label>
            <div className="flex flex-col gap-1.5">
              {r.paths.map((p, pi) => (
                <div key={pi} className="flex items-center gap-2">
                  <Input value={p} placeholder="/docs/:path*" className="font-mono" onChange={(e) => setPath(ri, pi, e.target.value)} />
                  <Button variant="ghost" aria-label="Remove path" onClick={() => setRoutes((rs) => rs.map((x, i) => (i === ri ? { ...x, paths: x.paths.filter((_, j) => j !== pi) } : x)))}>
                    <X className="h-4 w-4" />
                  </Button>
                </div>
              ))}
              <button className="mt-1 flex w-fit items-center gap-1 text-xs text-secondary hover:text-fg" onClick={() => setRoutes((rs) => rs.map((x, i) => (i === ri ? { ...x, paths: [...x.paths, ""] } : x)))}>
                <Plus className="h-3.5 w-3.5" /> Add path
              </button>
            </div>
            {routes.length > 1 ? (
              <button className="mt-2 flex items-center gap-1 text-xs text-red-500 hover:opacity-80" onClick={() => setRoutes((rs) => rs.filter((_, i) => i !== ri))}>
                <Trash2 className="h-3.5 w-3.5" /> Remove route group
              </button>
            ) : null}
          </div>
        ))}
        <button className="flex w-fit items-center gap-1 text-sm text-secondary hover:text-fg" onClick={() => setRoutes((rs) => [...rs, { paths: [""] }])}>
          <Plus className="h-4 w-4" /> Add route group
        </button>
      </div>
    </SettingCard>
  );
}

function DefaultRouteCard({ member, onSave }: { member: MfeMember; onSave: (v: string) => void }) {
  const [v, setV] = useState(member.default_route ?? "");
  const dirty = v !== (member.default_route ?? "");
  const invalid = v.trim() !== "" && !v.startsWith("/");
  return (
    <SettingCard
      title="Default Route"
      desc="Modify the path used for screenshots and the default link to the project."
      footer={invalid ? <span className="text-red-500">Must start with “/”.</span> : undefined}
      footerAction={
        <Button disabled={!dirty || invalid} onClick={() => onSave(v)}>
          Save
        </Button>
      }
    >
      <Input value={v} placeholder="/" className="font-mono" onChange={(e) => setV(e.target.value)} />
    </SettingCard>
  );
}

function FallbackEnvironmentCard({ group, onSave }: { group: MfeGroup; onSave: (body: unknown) => void }) {
  const [env, setEnv] = useState(group.fallback_environment);
  const [custom, setCustom] = useState(group.custom_fallback_environment_name ?? "");
  const dirty = env !== group.fallback_environment || (env === "custom" && custom !== (group.custom_fallback_environment_name ?? ""));
  const invalid = env === "custom" && custom.trim() === "";
  return (
    <SettingCard
      title="Fallback Environment"
      desc="When a preview request has no matching child deployment for the same commit, choose which environment serves it. Production requests always route to production deployments."
      footer={invalid ? <span className="text-red-500">A custom environment name is required.</span> : undefined}
      footerAction={
        <Button disabled={!dirty || invalid} onClick={() => onSave({ fallbackEnvironment: env, customFallbackEnvironmentName: env === "custom" ? custom : "" })}>
          Save
        </Button>
      }
    >
      <div className="flex flex-col gap-3">
        <select className={selectCls} value={env} onChange={(e) => setEnv(e.target.value)}>
          <option value="same_environment">Same Environment</option>
          <option value="production">Production</option>
          <option value="custom">Custom Environment</option>
        </select>
        {env === "custom" ? <Input value={custom} placeholder="staging" onChange={(e) => setCustom(e.target.value)} /> : null}
      </div>
    </SettingCard>
  );
}

function ObservabilityCard({ member, onSave }: { member: MfeMember; onSave: (v: string) => void }) {
  const on = member.observability_routing === "this_project";
  return (
    <SettingCard
      title="Observability Routing"
      desc="Route this project's observability data to itself instead of the default application."
    >
      <div className="flex items-center justify-between gap-4">
        <span className="text-sm text-secondary">Send this project&apos;s analytics/logs to this project</span>
        <Switch checked={on} onChange={(v) => onSave(v ? "this_project" : "default_application")} label="Observability routing" />
      </div>
    </SettingCard>
  );
}

function AssetPrefixCard({ member, onSave }: { member: MfeMember; onSave: (v: string) => void }) {
  const [v, setV] = useState(member.asset_prefix ?? "");
  const dirty = v !== (member.asset_prefix ?? "");
  const need = v.trim() !== "" && !member.routing.some((r) => r.paths.some((p) => p.replace(/^\//, "").startsWith(`${v.trim().replace(/^\/|\/$/g, "")}/`)));
  return (
    <SettingCard
      title="Asset Prefix"
      desc="Serve this project's static assets under a prefix so they don't collide with the default application's."
      footer={
        need ? (
          <span className="text-amber-600">
            Add a matching route <code className="font-mono">/{v.trim().replace(/^\/|\/$/g, "")}/:path*</code> so assets resolve.
          </span>
        ) : (
          "Changing the asset prefix after a deployment can break references to previously built assets."
        )
      }
      footerAction={
        <Button disabled={!dirty} onClick={() => onSave(v)}>
          Save
        </Button>
      }
    >
      <Input value={v} placeholder="docs-assets" className="font-mono" onChange={(e) => setV(e.target.value)} />
    </SettingCard>
  );
}

function LocalDevelopmentCard({
  member,
  group,
  onSaveMember,
  onSaveGroup,
}: {
  member: MfeMember;
  group: MfeGroup;
  onSaveMember: (dev: MfeDevelopment) => void;
  onSaveGroup: (body: unknown) => void;
}) {
  const dev = member.development ?? {};
  const [local, setLocal] = useState(dev.local ?? "");
  const [task, setTask] = useState(dev.task ?? "");
  const [fallback, setFallback] = useState(dev.fallback ?? "");
  const [proxy, setProxy] = useState(group.local_proxy_port ? String(group.local_proxy_port) : "");
  const memberDirty = local !== (dev.local ?? "") || task !== (dev.task ?? "") || fallback !== (dev.fallback ?? "");
  const proxyDirty = proxy !== (group.local_proxy_port ? String(group.local_proxy_port) : "");

  return (
    <SettingCard
      title="Local Development"
      desc="Settings used by the local microfrontends proxy so this project can run alongside the others."
      footerAction={
        <div className="flex gap-2">
          <Button
            variant="outline"
            disabled={!proxyDirty}
            onClick={() => onSaveGroup({ localProxyPort: proxy.trim() === "" ? 0 : Number(proxy) })}
          >
            Save Proxy Port
          </Button>
          <Button disabled={!memberDirty} onClick={() => onSaveMember({ local, task, fallback })}>
            Save
          </Button>
        </div>
      }
    >
      <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
        <div>
          <label className="mb-1 block text-xs text-secondary">Local port / host</label>
          <Input value={local} placeholder="3001" onChange={(e) => setLocal(e.target.value)} />
        </div>
        <div>
          <label className="mb-1 block text-xs text-secondary">Dev task</label>
          <Input value={task} placeholder="dev" onChange={(e) => setTask(e.target.value)} />
        </div>
        <div>
          <label className="mb-1 block text-xs text-secondary">Fallback origin</label>
          <Input value={fallback} placeholder="example.com" onChange={(e) => setFallback(e.target.value)} />
        </div>
        <div>
          <label className="mb-1 block text-xs text-secondary">Group proxy port</label>
          <Input value={proxy} placeholder="3024" onChange={(e) => setProxy(e.target.value)} />
        </div>
      </div>
    </SettingCard>
  );
}

// ---------------------------------------------------------------------------
// Dialogs
// ---------------------------------------------------------------------------
function Modal({ title, onClose, children }: { title: string; onClose: () => void; children: React.ReactNode }) {
  return (
    <div className="fixed inset-0 z-[200] flex items-center justify-center bg-black/50 p-4 backdrop-blur-sm" onClick={onClose}>
      <div
        role="dialog"
        aria-modal="true"
        className="w-full max-w-md rounded-2xl border border-border bg-card shadow-pop"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="flex items-center justify-between border-b border-border px-5 py-3">
          <h3 className="text-sm font-semibold">{title}</h3>
          <button aria-label="Close" onClick={onClose} className="text-secondary hover:text-fg">
            <X className="h-4 w-4" />
          </button>
        </div>
        <div className="p-5">{children}</div>
      </div>
    </div>
  );
}

function CreateGroupDialog({ project, onClose, onCreate }: { project: string; onClose: () => void; onCreate: (name: string) => void }) {
  const [name, setName] = useState("");
  return (
    <Modal title="Create a New Group" onClose={onClose}>
      <label className="mb-1 block text-xs text-secondary">Group name</label>
      <Input value={name} placeholder="storefront" onChange={(e) => setName(e.target.value)} autoFocus />
      <p className="mt-2 text-xs text-secondary">
        <span className="font-medium text-fg">{project}</span> becomes the default application of this group.
      </p>
      <div className="mt-4 flex justify-end gap-2">
        <Button variant="ghost" onClick={onClose}>
          Cancel
        </Button>
        <Button disabled={!name.trim()} onClick={() => onCreate(name.trim())}>
          Create Group
        </Button>
      </div>
    </Modal>
  );
}

function AddToGroupDialog({ groups, onClose, onAdd }: { groups: MfeGroup[]; onClose: () => void; onAdd: (groupId: string) => void }) {
  const [gid, setGid] = useState(groups[0]?.id ?? "");
  return (
    <Modal title="Add to Group" onClose={onClose}>
      <label className="mb-1 block text-xs text-secondary">Group</label>
      <select className={selectCls} value={gid} onChange={(e) => setGid(e.target.value)}>
        {groups.map((g) => (
          <option key={g.id} value={g.id}>
            {g.name}
          </option>
        ))}
      </select>
      <p className="mt-2 text-xs text-secondary">The project joins as a child application. Configure its routes after adding.</p>
      <div className="mt-4 flex justify-end gap-2">
        <Button variant="ghost" onClick={onClose}>
          Cancel
        </Button>
        <Button disabled={!gid} onClick={() => onAdd(gid)}>
          Add to Group
        </Button>
      </div>
    </Modal>
  );
}
