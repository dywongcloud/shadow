"use client";

import { useState } from "react";
import { Card, Badge, Button, Input, Switch, PageHeader, StateNotice, Table, Th, Td } from "@/components/ui";
import { RoleGate } from "@/components/role-gate";
import { ApiError, apiSend, setFirewallAttackMode, useAttackModeProjects, usePoll, type FwState, type WafRule } from "@/lib/api";
import { useProjectNames } from "@/lib/projects";

type BotAction = "allow" | "log" | "block";
interface WafConfig { managed: boolean; rules: WafRule[] }
interface BotPolicy {
  allow_good: boolean;
  block_bad: boolean;
  ai_crawler: BotAction;
  ai_assistant: BotAction;
  ai_agent: BotAction;
}

const AI_CATEGORIES: { key: keyof BotPolicy; name: string; desc: string; examples: string }[] = [
  { key: "ai_crawler", name: "AI Crawlers", desc: "Bots that scrape your content in bulk to train or index models. Not tied to any user request.", examples: "GPTBot · ClaudeBot · CCBot · Google-Extended · Bytespider" },
  { key: "ai_assistant", name: "AI Assistants", desc: "Fetch a page in real time to answer a user's prompt — often sends referral traffic back.", examples: "ChatGPT-User · OAI-SearchBot · Perplexity-User" },
  { key: "ai_agent", name: "AI Agents", desc: "Act autonomously on a user's behalf (browse, click, transact). Can mutate state.", examples: "ChatGPT-Agent · Operator" },
];

const ABSENT = "-";

/** Each firewall value is measured or absent: an older node, a refused poll or a missing
 *  optional field renders "-", never an invented zero or an invented status. */
function shown(v: unknown): string {
  if (v === null || v === undefined || v === "") return ABSENT;
  if (typeof v === "number" && !Number.isFinite(v)) return ABSENT;
  return String(v);
}

function fmtMeasured(v: number | null | undefined): string {
  return typeof v === "number" && Number.isFinite(v) ? v.toLocaleString("en-US") : ABSENT;
}

/** A measured epoch-ms deadline is rendered as a stable UTC instant, never a
 *  client-side countdown. An older node answers indefinite: true and expires:
 *  false; an absent one renders absent, never a fabricated deadline. */
function expiresText(indefinite: boolean | null | undefined, expires: boolean | string | number | null | undefined): string {
  if (indefinite === true || expires === false) return "Never while on";
  if (typeof expires === "number" && Number.isFinite(expires) && expires > 0 && expires <= 8.64e15) {
    return new Date(expires).toISOString().replace("T", " ").slice(0, 19) + " UTC";
  }
  if (typeof expires === "string") return shown(expires);
  return ABSENT;
}

/** Every new element stays inside its column: a long path or residual never scrolls the page. */
function cell(extra?: string): string {
  return extra ? "min-w-0 w-full max-w-full break-words " + extra : "min-w-0 w-full max-w-full break-words";
}

function FwField({ label, value }: { label: string; value: string }) {
  return (
    <div className={cell()}>
      <span className={cell("block text-xs text-muted")}>{label}</span>
      <span className={cell()}>{value}</span>
    </div>
  );
}

export default function FirewallPage() {
  const { data: waf, refresh } = usePoll<WafConfig>("/v1/waf", 4000);
  const { data: bot, refresh: refreshBot } = usePoll<BotPolicy>("/v1/bot", 4000);

  const [id, setId] = useState("");
  const [desc, setDesc] = useState("");
  const [contains, setContains] = useState("");
  const [ip, setIp] = useState("");
  const [err, setErr] = useState("");

  const [fwProject, setFwProject] = useState("");
  const [fwSearch, setFwSearch] = useState("");
  const [fwBusy, setFwBusy] = useState(false);
  const [fwWriteError, setFwWriteError] = useState<string | null>(null);
  const [fwTurningOff, setFwTurningOff] = useState<string | null>(null);
  const [fwDurationSecs, setFwDurationSecs] = useState(21600);
  const projectNames = useProjectNames();
  const filteredProjectNames = projectNames.filter((name) => name.toLowerCase().includes(fwSearch.trim().toLowerCase()));
  /** A selection hidden by the search text is NOT actionable: no write goes to a project the
   *  picker is not showing. */
  const fwSelectedProject = filteredProjectNames.includes(fwProject) ? fwProject : "";
  const fwStatePath = fwSelectedProject
    ? "/v1/firewall/state?project=" + encodeURIComponent(fwSelectedProject)
    : "/v1/firewall/state";
  const { data: fw, error: fwError, refresh: refreshFw } = usePoll<FwState>(fwStatePath, 5000);
  const { data: attackProjects, error: attackProjectsError, refresh: refreshAttackProjects } = useAttackModeProjects(5000);

  const fwMode = fw?.attack_mode ?? null;
  const fwEvents = fw?.events ?? null;
  const fwBots = fw?.verified_bots ?? null;
  const fwIntegrity = fw?.integrity ?? null;
  const fwResidualRows = fw?.residuals ?? null;
  const fwResiduals = Array.isArray(fwResidualRows) ? fwResidualRows : null;
  const fwByActionMap = fwEvents?.by_action ?? null;
  const fwByAction = fwByActionMap && typeof fwByActionMap === "object"
    ? Object.entries(fwByActionMap).slice(0, 8)
    : [];
  const fwRecentRows = fwEvents?.recent ?? null;
  const fwRecent = Array.isArray(fwRecentRows) ? fwRecentRows.slice(0, 8) : [];
  const fwBotRowsAll = fwBots?.rows ?? null;
  const fwBotRows = Array.isArray(fwBotRowsAll) ? fwBotRowsAll : [];
  const fwModeText = fwMode?.enabled === true ? "On" : fwMode?.enabled === false ? "Off" : ABSENT;
  const fwExpiresText = expiresText(fwMode?.indefinite, fwMode?.expires);

  /** One write of the node-local toggle, for the picker or one listed row. A typed refusal or a
   *  poll failure is shown inline by StateNotice; the server remains the authority on who may
   *  change it. Only a name from the tenant picker or the tenant-scoped list is sent. */
  async function setAttackMode(enable: boolean, project?: string) {
    if (fwBusy) return;
    const name = (project ?? fwProject).trim();
    if (!name) {
      setFwWriteError("Choose a project first.");
      return;
    }
    setFwWriteError(null);
    setFwBusy(true);
    setFwTurningOff(enable ? null : name);
    try {
      await setFirewallAttackMode(name, enable, fwDurationSecs);
      await Promise.all([refreshFw(), refreshAttackProjects()]);
    } catch (e) {
      setFwWriteError(
        e instanceof ApiError && e.message ? e.message : "Attack challenge mode could not be changed on this node.",
      );
    } finally {
      setFwBusy(false);
      setFwTurningOff(null);
    }
  }


  /** Runs one write of the edge rules: a refusal (a role refusal already reads "Your role can't do this. Ask a team admin.") shows above the cards, never as an unhandled rejection. */
  async function write(change: () => Promise<unknown>) {
    setErr("");
    try {
      await change();
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
    }
  }

  async function addRule() {
    if (!id) return;
    const rule: WafRule = {
      id,
      description: desc || id,
      action: "deny",
      enabled: true,
      when: {
        contains: contains || undefined,
        ip_prefix: ip || undefined,
      },
    };
    await write(async () => {
      await apiSend("POST", "/v1/waf/rules", rule);
      setId(""); setDesc(""); setContains(""); setIp("");
      refresh();
    });
  }

  return (
    <div>
      <PageHeader title="Firewall" desc="Web Application Firewall + bot management at the edge" />

      <RoleGate action="edge.security" note>
      {err ? <p className="mb-4 text-sm text-red-500">{err}</p> : null}
      <Card className="mb-4 flex items-center justify-between">
        <div>
          <div className="text-sm font-medium text-fg">Managed ruleset</div>
          <div className="text-xs text-muted">OWASP-style SQLi / XSS / path-traversal signatures</div>
        </div>
        <Switch
          checked={!!waf?.managed}
          onChange={(v) => write(async () => { await apiSend("PUT", "/v1/waf/managed", { enabled: v }); refresh(); })}
        />
      </Card>

      <Card className="mb-4">
        <div className="mb-4 text-sm font-medium text-fg">Bot management</div>
        <div className="flex flex-col gap-4 sm:flex-row sm:gap-10">
          <label className="flex cursor-pointer items-center gap-3 text-sm text-secondary">
            <Switch
              label="Allow verified bots"
              checked={!!bot?.allow_good}
              onChange={(v) => write(async () => { await apiSend("PUT", "/v1/bot", { ...bot, allow_good: v }); refreshBot(); })}
            />
            Allow verified bots (Googlebot, Slackbot…)
          </label>
          <label className="flex cursor-pointer items-center gap-3 text-sm text-secondary">
            <Switch
              label="Block bad bots"
              checked={!!bot?.block_bad}
              onChange={(v) => write(async () => { await apiSend("PUT", "/v1/bot", { ...bot, block_bad: v }); refreshBot(); })}
            />
            Block bad bots (scrapers, scanners)
          </label>
        </div>
      </Card>

      <Card className="mb-4">
        <div className="mb-1 text-sm font-medium text-fg">AI bot management</div>
        <p className="mb-4 text-xs text-secondary">
          Control the three types of AI traffic independently. Assistants usually drive referral
          traffic, while crawlers train models on your content — choose what to allow, log, or block.
        </p>
        <div className="flex flex-col divide-y divide-border">
          {AI_CATEGORIES.map((cat) => (
            <div key={cat.key} className="flex flex-col gap-3 py-4 first:pt-0 last:pb-0 sm:flex-row sm:items-center sm:justify-between">
              <div className="min-w-0">
                <div className="text-sm font-medium">{cat.name}</div>
                <div className="text-xs text-secondary">{cat.desc}</div>
                <div className="mt-1 font-mono text-[11px] text-muted">{cat.examples}</div>
              </div>
              <Segmented
                value={(bot?.[cat.key] as BotAction) ?? "log"}
                onChange={(v) => write(async () => { await apiSend("PUT", "/v1/bot", { ...bot, [cat.key]: v }); refreshBot(); })}
              />
            </div>
          ))}
        </div>
      </Card>

      <Card className="mb-4">
        <div className="mb-3 text-sm font-medium text-fg">Add a custom deny rule</div>
        <div className="grid grid-cols-2 gap-3 md:grid-cols-4">
          <Input placeholder="rule id" value={id} onChange={(e) => setId(e.target.value)} />
          <Input placeholder="description" value={desc} onChange={(e) => setDesc(e.target.value)} />
          <Input placeholder="path/query contains…" value={contains} onChange={(e) => setContains(e.target.value)} />
          <Input placeholder="ip prefix e.g. 10.0." value={ip} onChange={(e) => setIp(e.target.value)} />
        </div>
        <div className="mt-3"><Button onClick={addRule}>Add rule</Button></div>
      </Card>

      <div className="mb-2 text-sm font-medium text-fg">Custom rules</div>
      <Table>
        <thead>
          <tr><Th>ID</Th><Th>Action</Th><Th>Match</Th><Th>Enabled</Th><Th></Th></tr>
        </thead>
        <tbody>
          {(waf?.rules ?? []).map((r) => (
            <tr key={r.id}>
              <Td className="font-mono text-xs">{r.id}<div className="text-muted">{r.description}</div></Td>
              <Td><Badge tone={r.action === "deny" ? "red" : r.action === "allow" ? "green" : "amber"}>{r.action}</Badge></Td>
              <Td className="font-mono text-xs text-muted">
                {r.when.contains ? `contains "${r.when.contains}"` : ""}
                {r.when.ip_prefix ? ` ip ${r.when.ip_prefix}*` : ""}
                {r.when.path_regex ? ` path ~ ${r.when.path_regex}` : ""}
              </Td>
              <Td>{r.enabled ? <Badge tone="green">on</Badge> : <Badge>off</Badge>}</Td>
              <Td><Button variant="danger" onClick={() => write(async () => { await apiSend("DELETE", `/v1/waf/rules/${r.id}`); refresh(); })}>Delete</Button></Td>
            </tr>
          ))}
          {!waf?.rules?.length && <tr><Td className="text-muted">No custom rules — the managed ruleset is active.</Td></tr>}
        </tbody>
      </Table>

      <Card className={cell("mb-4")}>
        <h2 className={cell("text-sm font-medium text-fg")}>Attack Challenge Mode</h2>
        <p className={cell("mt-1 text-xs text-secondary")}>
          Attack challenge turns itself off at the selected deadline (1, 6 or 24 hours). The
          setting is a replicated ProjectSettings row, so every node converges on the same
          expiry; reads can lag briefly.
        </p>
        <div className={cell("mt-3 grid grid-cols-1 gap-2 text-sm sm:grid-cols-2 lg:grid-cols-3")}>
          <FwField label="Measured mode" value={fwModeText} />
          <FwField label="Expires" value={fwExpiresText} />
          <FwField label="Hours left" value={fmtMeasured(fwMode?.hours_left)} />
          <FwField label="Duration (secs)" value={fmtMeasured(fwMode?.duration_secs)} />
          <FwField label="Updated (ms)" value={fmtMeasured(fwMode?.updated_ms)} />
          <FwField label="Reported project" value={shown(fw?.project)} />
        </div>
        <div className={cell("mt-4 flex flex-col gap-3 sm:flex-row sm:flex-wrap sm:items-center")}>
          <Input
            aria-label="Search projects"
            placeholder="Search projects"
            maxLength={128}
            value={fwSearch}
            onChange={(e) => setFwSearch(e.target.value)}
            className={cell("min-h-[44px]")}
          />
          <select
            aria-label="Project"
            value={fwSelectedProject}
            onChange={(e) => setFwProject(e.target.value)}
            className={cell("min-h-[44px] rounded-md border border-border bg-card px-3 py-2 text-sm text-fg")}
          >
            <option value="">Select a project</option>
            {filteredProjectNames.map((name) => (
              <option key={name} value={name}>{name}</option>
            ))}
          </select>
          <select
            aria-label="Duration"
            value={String(fwDurationSecs)}
            onChange={(e) => setFwDurationSecs(Number(e.target.value))}
            disabled={fwBusy || !fwSelectedProject}
            className={cell("min-h-[44px] rounded-md border border-border bg-card px-3 py-2 text-sm text-fg")}
          >
            <option value="3600">1 hour</option>
            <option value="21600">6 hours</option>
            <option value="86400">24 hours</option>
          </select>
          <Button
            className={cell("min-h-[44px] sm:w-auto")}
            disabled={fwBusy || !fwSelectedProject}
            onClick={() => setAttackMode(fwMode?.enabled !== true, fwSelectedProject)}
          >
            {fwBusy ? "Applying" : fwMode?.enabled === true ? "Turn off" : "Turn on"}
          </Button>
        </div>
        {projectNames.length === 0 ? (
          <p className={cell("mt-3 text-xs text-secondary")}>No projects are available for this tenant.</p>
        ) : filteredProjectNames.length === 0 ? (
          <p className={cell("mt-3 text-xs text-secondary")}>No project names match that search.</p>
        ) : null}
        {fwWriteError || fwError ? (
          <div className={cell("mt-3")}>
            <StateNotice className={cell()}>
              {fwWriteError ? fwWriteError : "Could not load firewall state: " + shown(fwError)}
            </StateNotice>
          </div>
        ) : null}
        <div className={cell("mt-3 text-xs text-secondary")}>
          <span className={cell("block")}>Reported residuals</span>
          {fwResiduals && fwResiduals.length > 0 ? (
            <ul className={cell("mt-1 list-none space-y-1")}>
              {fwResiduals.map((r, i) => (
                <li key={"fw-residual-" + i} className={cell()}>{shown(r)}</li>
              ))}
            </ul>
          ) : (
            <span className={cell("block")}>{ABSENT}</span>
          )}
        </div>
      </Card>

      <Card className={cell("mb-4")}>
        <h2 className={cell("text-sm font-medium text-fg")}>Projects with attack challenge mode on</h2>
        <p className={cell("mt-1 text-xs text-secondary")}>
          Each row comes only from the tenant-scoped list for the signed-in session; no other
          project source is merged here. Tenancy is read from the verified session claim, never a
          team header, and a row is never inferred from a total.
        </p>
        {attackProjectsError ? (
          <div className={cell("mt-3")}>
            <StateNotice className={cell()}>Could not load the list: {shown(attackProjectsError)}</StateNotice>
          </div>
        ) : null}
        {fwWriteError ? <p className={cell("mt-3 text-sm text-red-500")}>{fwWriteError}</p> : null}
        <div className={cell("mt-3 flex flex-col divide-y divide-border")}>
          {attackProjects === null ? (
            <div className={cell("py-3 text-sm")}>{ABSENT}</div>
          ) : attackProjects.length === 0 ? (
            <div className={cell("py-3 text-sm text-muted")}>No projects have attack challenge mode on.</div>
          ) : attackProjects.map((row, i) => {
            const name = typeof row?.project === "string" ? row.project.trim() : "";
            return (
              <div key={"fw-attack-project-" + i} className={cell("flex min-h-[44px] flex-col gap-2 py-3 sm:flex-row sm:items-center sm:justify-between")}>
                <div className={cell("grid grid-cols-1 gap-1 sm:grid-cols-2")}>
                  <FwField label="Project" value={shown(row?.project)} />
                  <FwField label="Status" value={row?.enabled === true ? "On" : row?.enabled === false ? "Off" : ABSENT} />
                  <FwField label="Expires" value={expiresText(row?.indefinite, row?.expires)} />
                  <FwField label="Hours left" value={fmtMeasured(row?.hours_left)} />
                  <FwField label="Updated (ms)" value={fmtMeasured(row?.updated_ms)} />
                </div>
                <Button
                  variant="danger"
                  className={cell("min-h-[44px] sm:w-auto")}
                  disabled={fwBusy || !name}
                  onClick={() => setAttackMode(false, name)}
                >
                  {fwTurningOff === name ? "Turning off" : "Turn off"}
                </Button>
              </div>
            );
          })}
        </div>
      </Card>

      <Card className={cell("mb-4")}>
        <h2 className={cell("text-sm font-medium text-fg")}>Firewall events</h2>
        <p className={cell("mt-1 text-xs text-secondary")}>
          Node-local event ring: events node_local {shown(fwEvents?.node_local)}. A different node
          may answer with a different set of events; nothing here is a fleet-wide log.
        </p>
        <div className={cell("mt-3 grid grid-cols-1 gap-2 text-sm sm:grid-cols-2")}>
          <FwField label="Measured event count" value={fmtMeasured(fwEvents?.count)} />
          <FwField label="State node_local" value={shown(fw?.node_local)} />
        </div>
        <div className={cell("mt-3 text-xs text-secondary")}>By action</div>
        <div className={cell("mt-1 flex flex-col divide-y divide-border")}>
          {fwByAction.length > 0 ? fwByAction.map(([k, v]) => (
            <div key={k} className={cell("flex min-h-[44px] flex-col gap-1 py-2 text-sm sm:flex-row sm:justify-between")}>
              <span className={cell()}>{shown(k)}</span>
              <span className={cell("tabular-nums")}>{fmtMeasured(v)}</span>
            </div>
          )) : <div className={cell("py-2 text-sm")}>{ABSENT}</div>}
        </div>
        <div className={cell("mt-3 text-xs text-secondary")}>Recent events</div>
        <div className={cell("mt-1 flex flex-col divide-y divide-border")}>
          {fwRecent.length > 0 ? fwRecent.map((ev, i) => (
            <div key={"fw-event-" + i} className={cell("py-2 text-sm")}>
              <div className={cell("grid grid-cols-1 gap-1 sm:grid-cols-2")}>
                <FwField label="Time (ms)" value={fmtMeasured(ev?.ts_ms)} />
                <FwField label="Status" value={fmtMeasured(ev?.status)} />
                <FwField label="Action" value={shown(ev?.action)} />
                <FwField label="Reason" value={shown(ev?.reason)} />
              </div>
              <div className={cell("mt-1")}><FwField label="Path (text only)" value={shown(ev?.path)} /></div>
            </div>
          )) : <div className={cell("py-2 text-sm")}>{ABSENT}</div>}
        </div>
      </Card>

      <Card className={cell("mb-4")}>
        <h2 className={cell("text-sm font-medium text-fg")}>Verified bots</h2>
        <p className={cell("mt-1 text-xs text-secondary")}>
          User-agent matching alone is spoofable. These rows are not reverse-DNS verified and carry
          no bot score and no TLS fingerprint; a client can claim any of these strings.
        </p>
        <div className={cell("mt-3 grid grid-cols-1 gap-2 text-sm sm:grid-cols-2")}>
          <FwField label="Measured bot count" value={fmtMeasured(fwBots?.count)} />
          <FwField label="Reported spoofable" value={shown(fwBots?.spoofable)} />
        </div>
        <div className={cell("mt-3 flex flex-col divide-y divide-border")}>
          {fwBotRows.length > 0 ? fwBotRows.map((b, i) => (
            <div key={"fw-bot-" + i} className={cell("grid grid-cols-1 gap-1 py-2 text-sm sm:grid-cols-3")}>
              <FwField label="Name" value={shown(b?.name)} />
              <FwField label="User agent (text only)" value={shown(b?.ua)} />
              <FwField label="Verification method" value={shown(b?.verification_method)} />
            </div>
          )) : <div className={cell("py-2 text-sm")}>{ABSENT}</div>}
        </div>
      </Card>

      <Card className={cell("mb-4")}>
        <h2 className={cell("text-sm font-medium text-fg")}>Page integrity</h2>
        <p className={cell("mt-1 text-xs text-secondary")}>
          Detection only: this node reports script mismatches but does not enforce a script policy
          and does not inject nonces into served pages.
        </p>
        <div className={cell("mt-3 grid grid-cols-1 gap-2 text-sm sm:grid-cols-3")}>
          <FwField label="Detection only" value={shown(fwIntegrity?.detection_only)} />
          <FwField label="Scripts reported" value={fmtMeasured(fwIntegrity?.scripts_reported)} />
          <FwField label="Mismatches" value={fmtMeasured(fwIntegrity?.mismatches)} />
        </div>
      </Card>
      </RoleGate>
    </div>
  );
}

function Segmented({ value, onChange }: { value: BotAction; onChange: (v: BotAction) => void }) {
  const opts: { v: BotAction; label: string; tone: string }[] = [
    { v: "allow", label: "Allow", tone: "data-[on=true]:bg-green/15 data-[on=true]:text-green" },
    { v: "log", label: "Log", tone: "data-[on=true]:bg-amber-500/15 data-[on=true]:text-amber-600 dark:data-[on=true]:text-amber-400" },
    { v: "block", label: "Block", tone: "data-[on=true]:bg-red-500/15 data-[on=true]:text-red-600 dark:data-[on=true]:text-red-400" },
  ];
  return (
    <div className="flex shrink-0 rounded-md border border-border p-0.5">
      {opts.map((o) => (
        <button
          key={o.v}
          data-on={value === o.v}
          onClick={() => onChange(o.v)}
          className={`rounded px-3 py-1 text-xs font-medium text-secondary transition-colors hover:text-fg ${o.tone}`}
        >
          {o.label}
        </button>
      ))}
    </div>
  );
}
