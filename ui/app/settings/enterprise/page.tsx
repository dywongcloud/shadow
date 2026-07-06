"use client";

import { useEffect, useState, useCallback } from "react";
import {
  ShieldBan,
  ScrollText,
  KeyRound,
  Users,
  Boxes,
  ClipboardCheck,
  Trash2,
  Plus,
  Copy,
  Check,
} from "lucide-react";
import { Card, PageHeader, Badge, Button, Input, Switch } from "@/components/ui";
import { apiGet, apiSend, currentTeam } from "@/lib/api";

/* --------------------------------------------------------------------------
 * Enterprise settings — one consolidated surface for the enterprise feature
 * suite. Every section is team-scoped and plan-gated server-side (the API
 * returns 403 when the team isn't on the required plan); the UI surfaces those
 * errors inline rather than hiding the controls.
 * ------------------------------------------------------------------------ */

function Section({
  icon,
  title,
  desc,
  badge,
  children,
}: {
  icon: React.ReactNode;
  title: string;
  desc: string;
  badge?: string;
  children: React.ReactNode;
}) {
  return (
    <Card className="p-5 sm:p-6">
      <div className="mb-4 flex items-start gap-3">
        <span className="flex h-9 w-9 shrink-0 items-center justify-center rounded-lg bg-subtle text-secondary">{icon}</span>
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <h3 className="text-base font-semibold">{title}</h3>
            {badge ? <Badge tone="blue">{badge}</Badge> : null}
          </div>
          <p className="mt-0.5 text-sm text-secondary">{desc}</p>
        </div>
      </div>
      {children}
    </Card>
  );
}

function ErrMsg({ msg }: { msg: string | null }) {
  if (!msg) return null;
  return <p className="mt-3 text-sm text-red-500">{msg.replace(/^Error:\s*/, "")}</p>;
}

export default function EnterprisePage() {
  const [team, setTeam] = useState("personal");
  useEffect(() => {
    setTeam(currentTeam());
    const on = () => setTeam(currentTeam());
    window.addEventListener("hive-team-changed", on);
    return () => window.removeEventListener("hive-team-changed", on);
  }, []);

  return (
    <div>
      <PageHeader
        title="Enterprise"
        desc={`Security, identity, and governance for the "${team}" team. Most features require the Enterprise plan.`}
      />
      <div className="flex flex-col gap-5">
        <IpBlocking />
        <SamlSso />
        <ScimSync />
        <SiemStreaming />
        <Microfrontends />
        <Conformance />
      </div>
    </div>
  );
}

/* ============================ IP blocking ============================ */

interface IpBlock {
  id: string;
  prefix: string;
  note: string;
  enabled: boolean;
  created_ms: number;
}

function IpBlocking() {
  const [blocks, setBlocks] = useState<IpBlock[]>([]);
  const [prefix, setPrefix] = useState("");
  const [note, setNote] = useState("");
  const [err, setErr] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const d = await apiGet<{ blocks: IpBlock[] }>("/v1/enterprise/ip-blocks");
      setBlocks(d.blocks || []);
    } catch (e) {
      setErr(String(e));
    }
  }, []);
  useEffect(() => {
    load();
  }, [load]);

  const add = async () => {
    setErr(null);
    try {
      await apiSend("POST", "/v1/enterprise/ip-blocks", { prefix, note });
      setPrefix("");
      setNote("");
      load();
    } catch (e) {
      setErr(String(e));
    }
  };
  const remove = async (id: string) => {
    await apiSend("DELETE", `/v1/enterprise/ip-blocks/${id}`);
    load();
  };

  return (
    <Section
      icon={<ShieldBan className="h-4 w-4" />}
      title="Account-level IP Blocking"
      desc="Deny requests from specific IPs or prefixes across every deployment in this team. Enforced at the edge, before compute."
      badge="Enterprise"
    >
      <div className="flex flex-col gap-2 sm:flex-row">
        <Input placeholder="IP or prefix (e.g. 203.0.113. or 203.0.113.7)" value={prefix} onChange={(e) => setPrefix(e.target.value)} />
        <Input placeholder="Note (optional)" value={note} onChange={(e) => setNote(e.target.value)} />
        <Button onClick={add} disabled={!prefix.trim()} className="shrink-0">
          <Plus className="h-4 w-4" /> Block
        </Button>
      </div>
      <ErrMsg msg={err} />
      <div className="mt-4 flex flex-col gap-2">
        {blocks.length === 0 ? (
          <p className="text-sm text-muted">No IP blocks configured.</p>
        ) : (
          blocks.map((b) => (
            <div key={b.id} className="flex items-center justify-between rounded-lg border border-border px-3 py-2 text-sm">
              <div>
                <span className="font-mono font-medium">{b.prefix}</span>
                {b.note ? <span className="ml-2 text-muted">{b.note}</span> : null}
              </div>
              <button onClick={() => remove(b.id)} className="text-muted hover:text-red-500" aria-label="remove">
                <Trash2 className="h-4 w-4" />
              </button>
            </div>
          ))
        )}
      </div>
    </Section>
  );
}

/* ============================ SAML SSO ============================ */

interface SamlView {
  idp_entity_id: string;
  sso_url: string;
  has_cert: boolean;
  enabled: boolean;
  enforced: boolean;
  auto_provision: boolean;
}

function SamlSso() {
  const [cfg, setCfg] = useState<SamlView | null>(null);
  const [entityId, setEntityId] = useState("");
  const [ssoUrl, setSsoUrl] = useState("");
  const [cert, setCert] = useState("");
  const [enabled, setEnabled] = useState(false);
  const [enforced, setEnforced] = useState(false);
  const [autoProvision, setAutoProvision] = useState(true);
  const [err, setErr] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);

  const load = useCallback(async () => {
    try {
      const d = await apiGet<SamlView>("/v1/enterprise/saml");
      setCfg(d);
      setEntityId(d.idp_entity_id);
      setSsoUrl(d.sso_url);
      setEnabled(d.enabled);
      setEnforced(d.enforced);
      setAutoProvision(d.auto_provision);
    } catch (e) {
      setErr(String(e));
    }
  }, []);
  useEffect(() => {
    load();
  }, [load]);

  const save = async () => {
    setErr(null);
    setSaved(false);
    try {
      await apiSend("PUT", "/v1/enterprise/saml", {
        idp_entity_id: entityId,
        sso_url: ssoUrl,
        x509_cert: cert,
        enabled,
        enforced,
        auto_provision: autoProvision,
      });
      setCert("");
      setSaved(true);
      load();
    } catch (e) {
      setErr(String(e));
    }
  };

  return (
    <Section
      icon={<KeyRound className="h-4 w-4" />}
      title="SAML Single Sign-On"
      desc="Federate sign-in with your identity provider (Okta, Azure AD, Google). Configure the SP metadata in your IdP, then paste the IdP details here."
      badge="Enterprise"
    >
      <div className="grid gap-3 sm:grid-cols-2">
        <label className="text-sm">
          <span className="mb-1 block text-secondary">IdP Entity ID</span>
          <Input value={entityId} onChange={(e) => setEntityId(e.target.value)} placeholder="https://idp.example.com/..." />
        </label>
        <label className="text-sm">
          <span className="mb-1 block text-secondary">IdP SSO URL</span>
          <Input value={ssoUrl} onChange={(e) => setSsoUrl(e.target.value)} placeholder="https://idp.example.com/sso" />
        </label>
      </div>
      <label className="mt-3 block text-sm">
        <span className="mb-1 block text-secondary">IdP X.509 Signing Certificate (PEM){cfg?.has_cert ? " — configured" : ""}</span>
        <textarea
          value={cert}
          onChange={(e) => setCert(e.target.value)}
          placeholder={cfg?.has_cert ? "•••••• (leave blank to keep existing)" : "-----BEGIN CERTIFICATE-----"}
          className="h-24 w-full rounded-md border border-border bg-card px-3 py-2 font-mono text-xs text-fg placeholder:text-muted focus:border-border-strong focus:outline-none"
        />
      </label>
      <div className="mt-4 flex flex-col gap-3">
        <label className="flex items-center justify-between text-sm">
          <span>Enable SSO</span>
          <Switch checked={enabled} onChange={setEnabled} />
        </label>
        <label className="flex items-center justify-between text-sm">
          <span>Enforce SSO <span className="text-muted">(refuse non-SSO sign-in)</span></span>
          <Switch checked={enforced} onChange={setEnforced} />
        </label>
        <label className="flex items-center justify-between text-sm">
          <span>Auto-provision members on first login</span>
          <Switch checked={autoProvision} onChange={setAutoProvision} />
        </label>
      </div>
      <div className="mt-4 flex items-center gap-3">
        <Button onClick={save}>Save SAML config</Button>
        <a href="/cloud/v1/enterprise/saml/metadata" target="_blank" rel="noreferrer" className="text-sm text-secondary underline hover:text-fg">
          Download SP metadata
        </a>
        {saved ? <span className="text-sm text-green-500">Saved</span> : null}
      </div>
      <ErrMsg msg={err} />
    </Section>
  );
}

/* ============================ SCIM ============================ */

interface ScimUser {
  id: string;
  user_name: string;
  display_name: string;
  active: boolean;
}
interface ScimView {
  enabled: boolean;
  endpoint: string;
  has_token: boolean;
  users: ScimUser[];
}

function ScimSync() {
  const [view, setView] = useState<ScimView | null>(null);
  const [token, setToken] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setView(await apiGet<ScimView>("/v1/enterprise/scim"));
    } catch (e) {
      setErr(String(e));
    }
  }, []);
  useEffect(() => {
    load();
  }, [load]);

  const enable = async () => {
    setErr(null);
    try {
      const d = await apiSend<{ token: string }>("POST", "/v1/enterprise/scim", {});
      setToken(d.token);
      load();
    } catch (e) {
      setErr(String(e));
    }
  };
  const disable = async () => {
    await apiSend("DELETE", "/v1/enterprise/scim");
    setToken(null);
    load();
  };
  const copy = (text: string) => {
    navigator.clipboard.writeText(text);
    setCopied(true);
    setTimeout(() => setCopied(false), 1500);
  };

  return (
    <Section
      icon={<Users className="h-4 w-4" />}
      title="Directory Sync (SCIM 2.0)"
      desc="Automatically provision and de-provision team members from your identity provider. Point your IdP's SCIM connector at the endpoint below."
      badge="Enterprise"
    >
      {view?.enabled ? (
        <>
          <div className="rounded-lg border border-border bg-subtle/40 p-3 text-sm">
            <div className="mb-2 flex items-center justify-between">
              <span className="text-secondary">SCIM Base URL</span>
              <button onClick={() => copy(view.endpoint)} className="flex items-center gap-1 text-muted hover:text-fg">
                {copied ? <Check className="h-3.5 w-3.5" /> : <Copy className="h-3.5 w-3.5" />} copy
              </button>
            </div>
            <code className="block break-all font-mono text-xs">{view.endpoint}</code>
          </div>
          {token ? (
            <div className="mt-3 rounded-lg border border-amber-500/40 bg-amber-500/10 p-3 text-sm">
              <div className="mb-1 font-medium text-amber-600 dark:text-amber-400">Bearer token — copy it now, it won&apos;t be shown again</div>
              <div className="flex items-center gap-2">
                <code className="block break-all font-mono text-xs">{token}</code>
                <button onClick={() => copy(token)} className="shrink-0 text-muted hover:text-fg">
                  <Copy className="h-3.5 w-3.5" />
                </button>
              </div>
            </div>
          ) : null}
          <div className="mt-4">
            <div className="mb-2 text-sm font-medium">Synced users ({view.users.length})</div>
            <div className="flex flex-col gap-1.5">
              {view.users.length === 0 ? (
                <p className="text-sm text-muted">No users synced yet.</p>
              ) : (
                view.users.map((u) => (
                  <div key={u.id} className="flex items-center justify-between rounded-lg border border-border px-3 py-2 text-sm">
                    <span>{u.user_name}</span>
                    <Badge tone={u.active ? "green" : "default"}>{u.active ? "active" : "deactivated"}</Badge>
                  </div>
                ))
              )}
            </div>
          </div>
          <div className="mt-4 flex items-center gap-3">
            <Button variant="outline" onClick={enable}>Rotate token</Button>
            <Button variant="danger" onClick={disable}>Disable SCIM</Button>
          </div>
        </>
      ) : (
        <Button onClick={enable}>Enable SCIM directory sync</Button>
      )}
      <ErrMsg msg={err} />
    </Section>
  );
}

/* ============================ SIEM ============================ */

interface SiemView {
  format: string;
  endpoint: string;
  enabled: boolean;
  has_token: boolean;
  delivered?: number;
  failed?: number;
}

function SiemStreaming() {
  const [view, setView] = useState<SiemView | null>(null);
  const [format, setFormat] = useState("http");
  const [endpoint, setEndpoint] = useState("");
  const [tokenv, setTokenv] = useState("");
  const [enabled, setEnabled] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [msg, setMsg] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const d = await apiGet<SiemView>("/v1/enterprise/siem");
      setView(d);
      setFormat(d.format || "http");
      setEndpoint(d.endpoint || "");
      setEnabled(d.enabled);
    } catch (e) {
      setErr(String(e));
    }
  }, []);
  useEffect(() => {
    load();
  }, [load]);

  const save = async () => {
    setErr(null);
    setMsg(null);
    try {
      await apiSend("PUT", "/v1/enterprise/siem", { format, endpoint, token: tokenv || undefined, enabled });
      setTokenv("");
      setMsg("Saved");
      load();
    } catch (e) {
      setErr(String(e));
    }
  };
  const test = async () => {
    setErr(null);
    setMsg(null);
    try {
      await apiSend("POST", "/v1/enterprise/siem/test", {});
      setMsg("Test event emitted — check your SIEM.");
      setTimeout(load, 1000);
    } catch (e) {
      setErr(String(e));
    }
  };

  return (
    <Section
      icon={<ScrollText className="h-4 w-4" />}
      title="Audit Log Streaming (SIEM)"
      desc="Stream every audit event to your SIEM in real time. Supports generic HTTP JSON, Datadog, and Splunk HEC."
      badge="Enterprise"
    >
      <div className="grid gap-3 sm:grid-cols-[160px_1fr]">
        <label className="text-sm">
          <span className="mb-1 block text-secondary">Format</span>
          <select
            value={format}
            onChange={(e) => setFormat(e.target.value)}
            className="w-full rounded-md border border-border bg-card px-3 py-2 text-sm text-fg focus:border-border-strong focus:outline-none"
          >
            <option value="http">Generic HTTP (JSON)</option>
            <option value="datadog">Datadog</option>
            <option value="splunk-hec">Splunk HEC</option>
          </select>
        </label>
        <label className="text-sm">
          <span className="mb-1 block text-secondary">Endpoint URL</span>
          <Input value={endpoint} onChange={(e) => setEndpoint(e.target.value)} placeholder="https://http-intake.logs.datadoghq.com/..." />
        </label>
      </div>
      <label className="mt-3 block text-sm">
        <span className="mb-1 block text-secondary">API token / key{view?.has_token ? " — configured" : ""}</span>
        <Input type="password" value={tokenv} onChange={(e) => setTokenv(e.target.value)} placeholder={view?.has_token ? "•••••• (leave blank to keep)" : "token"} />
      </label>
      <label className="mt-4 flex items-center justify-between text-sm">
        <span>Enable streaming</span>
        <Switch checked={enabled} onChange={setEnabled} />
      </label>
      {view && (view.delivered || view.failed) ? (
        <p className="mt-3 text-xs text-muted">Delivered {view.delivered ?? 0} · Failed {view.failed ?? 0}</p>
      ) : null}
      <div className="mt-4 flex items-center gap-3">
        <Button onClick={save}>Save</Button>
        <Button variant="outline" onClick={test}>Send test event</Button>
        {msg ? <span className="text-sm text-green-500">{msg}</span> : null}
      </div>
      <ErrMsg msg={err} />
    </Section>
  );
}

/* ============================ Microfrontends ============================ */

interface MfeChild {
  project: string;
  path_prefix: string;
}
interface MfeGroup {
  id: string;
  name: string;
  host_project: string;
  children: MfeChild[];
  enabled: boolean;
}

function Microfrontends() {
  const [groups, setGroups] = useState<MfeGroup[]>([]);
  const [name, setName] = useState("");
  const [host, setHost] = useState("");
  const [childProj, setChildProj] = useState("");
  const [childPath, setChildPath] = useState("");
  const [children, setChildren] = useState<MfeChild[]>([]);
  const [err, setErr] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const d = await apiGet<{ groups: MfeGroup[] }>("/v1/enterprise/microfrontends");
      setGroups(d.groups || []);
    } catch (e) {
      setErr(String(e));
    }
  }, []);
  useEffect(() => {
    load();
  }, [load]);

  const addChild = () => {
    if (!childProj.trim() || !childPath.trim()) return;
    setChildren([...children, { project: childProj.trim(), path_prefix: childPath.trim() }]);
    setChildProj("");
    setChildPath("");
  };
  const create = async () => {
    setErr(null);
    try {
      await apiSend("POST", "/v1/enterprise/microfrontends", { name, host_project: host, children });
      setName("");
      setHost("");
      setChildren([]);
      load();
    } catch (e) {
      setErr(String(e));
    }
  };
  const remove = async (id: string) => {
    await apiSend("DELETE", `/v1/enterprise/microfrontends/${id}`);
    load();
  };

  return (
    <Section
      icon={<Boxes className="h-4 w-4" />}
      title="Microfrontends"
      desc="Compose multiple projects into one app under a single domain. The host project serves the root; children serve their path prefixes."
      badge="Pro"
    >
      <div className="grid gap-3 sm:grid-cols-2">
        <Input placeholder="Group name" value={name} onChange={(e) => setName(e.target.value)} />
        <Input placeholder="Host project (serves /)" value={host} onChange={(e) => setHost(e.target.value)} />
      </div>
      <div className="mt-3 flex flex-col gap-2 sm:flex-row">
        <Input placeholder="Child project" value={childProj} onChange={(e) => setChildProj(e.target.value)} />
        <Input placeholder="Path prefix (e.g. /blog)" value={childPath} onChange={(e) => setChildPath(e.target.value)} />
        <Button variant="outline" onClick={addChild} className="shrink-0">
          <Plus className="h-4 w-4" /> Add child
        </Button>
      </div>
      {children.length > 0 ? (
        <div className="mt-2 flex flex-wrap gap-2">
          {children.map((ch, i) => (
            <Badge key={i} tone="default">
              {ch.path_prefix} → {ch.project}
            </Badge>
          ))}
        </div>
      ) : null}
      <div className="mt-3">
        <Button onClick={create} disabled={!name.trim() || !host.trim()}>Create group</Button>
      </div>
      <ErrMsg msg={err} />
      <div className="mt-4 flex flex-col gap-2">
        {groups.map((g) => (
          <div key={g.id} className="rounded-lg border border-border px-3 py-2 text-sm">
            <div className="flex items-center justify-between">
              <span className="font-medium">{g.name} <span className="text-muted">· host {g.host_project}</span></span>
              <button onClick={() => remove(g.id)} className="text-muted hover:text-red-500" aria-label="remove">
                <Trash2 className="h-4 w-4" />
              </button>
            </div>
            {g.children.length > 0 ? (
              <div className="mt-1.5 flex flex-wrap gap-1.5">
                {g.children.map((ch, i) => (
                  <span key={i} className="rounded bg-subtle px-1.5 py-0.5 font-mono text-xs text-secondary">
                    {ch.path_prefix} → {ch.project}
                  </span>
                ))}
              </div>
            ) : null}
          </div>
        ))}
      </div>
    </Section>
  );
}

/* ============================ Conformance ============================ */

interface ConfResult {
  rule: string;
  title: string;
  passed: boolean;
  required: boolean;
  detail: string;
}
interface ConfReport {
  project: string;
  results: ConfResult[];
  passed: boolean;
  score: number;
}

function Conformance() {
  const [project, setProject] = useState("");
  const [report, setReport] = useState<ConfReport | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [running, setRunning] = useState(false);

  const run = async () => {
    setErr(null);
    setRunning(true);
    try {
      setReport(await apiSend<ConfReport>("POST", "/v1/enterprise/conformance/run", { project }));
    } catch (e) {
      setErr(String(e));
    } finally {
      setRunning(false);
    }
  };

  return (
    <Section
      icon={<ClipboardCheck className="h-4 w-4" />}
      title="Conformance"
      desc="Run governance checks against a project's latest deployment — HTTPS, secret hygiene, firewall, access protection, production readiness, pinned runtime."
      badge="Enterprise"
    >
      <div className="flex flex-col gap-2 sm:flex-row">
        <Input placeholder="Project name" value={project} onChange={(e) => setProject(e.target.value)} />
        <Button onClick={run} disabled={!project.trim() || running} className="shrink-0">
          {running ? "Running…" : "Run checks"}
        </Button>
      </div>
      <ErrMsg msg={err} />
      {report ? (
        <div className="mt-4">
          <div className="mb-3 flex items-center gap-3">
            <Badge tone={report.passed ? "green" : "red"}>{report.passed ? "Passing" : "Failing"}</Badge>
            <span className="text-sm text-secondary">Score {report.score}%</span>
          </div>
          <div className="flex flex-col gap-2">
            {report.results.map((r) => (
              <div key={r.rule} className="flex items-start justify-between rounded-lg border border-border px-3 py-2 text-sm">
                <div>
                  <div className="flex items-center gap-2 font-medium">
                    {r.title}
                    {r.required ? <span className="text-xs text-muted">required</span> : null}
                  </div>
                  <div className="text-xs text-muted">{r.detail}</div>
                </div>
                <Badge tone={r.passed ? "green" : r.required ? "red" : "default"}>{r.passed ? "pass" : "fail"}</Badge>
              </div>
            ))}
          </div>
        </div>
      ) : null}
    </Section>
  );
}
