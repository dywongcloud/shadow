"use client";

import { useState, use, useEffect } from "react";
import Link from "next/link";
import {
  ChevronRight, RefreshCw, ShieldCheck, Lock, Trash2, Plus, Loader2, Copy, MoreHorizontal,
  Pencil, X, DownloadCloud, FileUp, Check, AlertTriangle, Link2,
} from "lucide-react";
import { Card, Button, Input, Triangle, Badge } from "@/components/ui";
import { Switch } from "@/components/ui";
import { apiGet, apiSend, usePoll, verifyDomainNow, detachDomain, attachDomain, fetchNsRoster, type DomainDetail, type DnsRecord, type Deployment, type DomainAttachResult, type NsRoster } from "@/lib/api";
import { toast } from "@/components/toast";
import { timeAgo, copyText } from "@/lib/utils";

interface ScanRecord { name: string; type: string; value: string; ttl: number; priority: number | null }

const RECORD_TYPES = ["A", "AAAA", "CNAME", "ALIAS", "MX", "TXT", "CAA", "NS", "SRV"];

function fmtDate(ms: number) {
  return new Date(ms).toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" });
}

export function DomainDetailPage({ paramsPromise }: { paramsPromise: Promise<{ domain: string }> }) {
  const params = use(paramsPromise);
  const domain = decodeURIComponent(params.domain);
  const { data, refresh } = usePoll<DomainDetail>(`/v1/domains/${encodeURIComponent(domain)}`, 6000);
  const [connectOpen, setConnectOpen] = useState(false);

  if (!data) {
    return <div className="py-20 text-center text-sm text-secondary"><Loader2 className="mx-auto h-5 w-5 animate-spin" /></div>;
  }
  const d = data.domain;
  // A domain with a real attach flow (`verify`) has no registrar/renewal/SSL
  // state on the platform — those record fields are legacy simulated
  // placeholders and must not render next to the real verification card.
  const attached = !!d.verify;

  return (
    <div className="pb-24">
      {/* Breadcrumb */}
      <div className="mb-6 flex items-center gap-1.5 text-sm text-secondary">
        <Link href="/domains" className="hover:text-fg">Domains</Link>
        <ChevronRight className="h-3.5 w-3.5 text-muted" />
        <span className="flex items-center gap-1.5 font-medium text-fg">{domain}
          <button onClick={() => navigator.clipboard?.writeText(domain)} className="text-muted hover:text-fg"><Copy className="h-3.5 w-3.5" /></button>
        </span>
      </div>

      {/* Header */}
      <div className="mb-6 flex items-start justify-between">
        <h1 className="text-3xl font-semibold tracking-tight">{domain}</h1>
        <div className="flex items-center gap-2">
          {!attached && <Button>Renew Domain</Button>}
          <button className="flex h-9 w-9 items-center justify-center rounded-md border border-border-strong text-muted hover:bg-subtle"><MoreHorizontal className="h-4 w-4" /></button>
        </div>
      </div>

      {/* Meta row */}
      <div className="mb-8 grid grid-cols-2 gap-y-4 border-b border-border pb-6 text-sm sm:grid-cols-3 lg:grid-cols-6">
        <Meta label="Expiration Date" value={<span className="flex items-center gap-1.5"><RefreshCw className="h-3.5 w-3.5 text-muted" /> {fmtDate(d.expires_ms)}</span>} />
        {!attached && <Meta label="Renewal Price" value={d.renewal_price} />}
        {!attached && <Meta label="Registrar" value={<span className="flex items-center gap-1.5"><Triangle className="h-4 w-4" /> {d.registrar}</span>} />}
        {!attached && <Meta label="Auto Renewal" value={<AutoRenew domain={domain} on={d.auto_renew} onChange={refresh} />} />}
        <Meta label="Age" value={timeAgo(d.created_ms)} />
        <Meta label="DevHub CDN" value={d.cdn_active ? <span className="flex items-center gap-1.5 text-emerald-500"><ShieldCheck className="h-4 w-4" /> Active</span> : "Inactive"} />
      </div>

      {/* Ownership verification (custom-domain attachment) */}
      {d.verify && (
        <VerifyCard domain={domain} v={d.verify} onChange={refresh} />
      )}

      {/* Connected Projects */}
      <Section title="Connected Projects" desc="Subdomains that are connected to projects on this team." action={<Button variant="outline" onClick={() => setConnectOpen(true)}>Connect</Button>}>
        <Card className="p-0">
          {data.connected.length === 0 ? (
            <div className="px-4 py-10 text-center text-sm text-secondary">No projects on this team are using this domain.</div>
          ) : (
            data.connected.map((cn) => (
              <Link key={cn.domain} href={`/projects/${encodeURIComponent(cn.project)}`} className="flex items-center justify-between border-b border-border px-4 py-3 text-sm last:border-0 hover:bg-subtle/50">
                <span className="font-mono">{cn.domain}</span>
                <span className="text-secondary">{cn.project} <ChevronRight className="inline h-3.5 w-3.5" /></span>
              </Link>
            ))
          )}
        </Card>
      </Section>

      {/* DNS Records */}
      <DnsRecords domain={domain} records={d.records} onChange={refresh} />

      {/* Migrate / import existing DNS */}
      <MigrateDns domain={domain} onChange={refresh} />

      {/* Nameservers */}
      <Nameservers domain={domain} nameservers={d.nameservers} attached={attached} onChange={refresh} />

      {/* SSL Certificates — simulated placeholder data, only meaningful for
          legacy DNS-only records (no attach flow). */}
      {!attached && (
      <Section title="SSL Certificates" desc="By default, DevHub issues and auto-renews a free SSL certificate for your domains.">
        <Card className="p-0">
          <div className="grid grid-cols-[2fr_1.4fr_0.8fr_1.2fr_auto] border-b border-border px-4 py-2.5 text-xs font-medium uppercase tracking-wide text-muted">
            <span>ID</span><span>CNs</span><span>Renewal</span><span>Expiration</span><span>Age</span>
          </div>
          <div className="grid grid-cols-[2fr_1.4fr_0.8fr_1.2fr_auto] items-center gap-2 px-4 py-3 text-sm">
            <span className="flex items-center gap-2 font-mono text-xs"><Lock className="h-3.5 w-3.5 text-muted" /> {d.ssl.id}</span>
            <span className="flex flex-wrap gap-1">{d.ssl.cns.map((c) => <span key={c} className="rounded bg-subtle px-1.5 py-0.5 text-[11px] text-secondary">{c}</span>)}</span>
            <span className="capitalize text-secondary">{d.ssl.renewal}</span>
            <span className="text-secondary">{fmtDate(d.ssl.expires_ms)}</span>
            <span className="text-muted">{timeAgo(d.ssl.issued_ms)}</span>
          </div>
          <div className="flex items-center justify-between border-t border-border px-4 py-2.5 text-xs text-muted">
            <span className="flex items-center gap-1.5"><ShieldCheck className="h-3.5 w-3.5 text-emerald-500" /> Provided free by {d.ssl.provider}</span>
            <button onClick={() => apiSend("POST", `/v1/domains/${encodeURIComponent(domain)}/ssl/renew`).then(refresh)} className="rounded-md border border-border-strong px-2.5 py-1 hover:bg-subtle">Renew now</button>
          </div>
        </Card>
      </Section>
      )}

      {/* Registrant — simulated WHOIS/registrar copy, legacy DNS-only only. */}
      {!attached && (
      <Section title="Registrant Information" desc="We collect this information to meet ICANN requirements and establish you as the legal domain holder." action={<Button variant="outline">Manage WHOIS Privacy</Button>}>
        <Card className="p-5 text-sm text-secondary">WHOIS privacy is enabled — your contact details are protected.</Card>
      </Section>
      )}

      <ConnectDomain domain={domain} open={connectOpen} onClose={() => setConnectOpen(false)} onChange={refresh} />
    </div>
  );
}

function Meta({ label, value }: { label: string; value: React.ReactNode }) {
  return (
    <div>
      <div className="text-xs text-muted">{label}</div>
      <div className="mt-1 text-sm">{value}</div>
    </div>
  );
}

/** The ownership-verification card for a custom-domain attachment: pending
 *  shows the TXT to add (with copy + manual re-check); verified shows the
 *  live state and offers detach. */
function VerifyCard({ domain, v, onChange }: { domain: string; v: import("@/lib/api").DomainVerify; onChange: () => void }) {
  const [busy, setBusy] = useState(false);
  const [copied, setCopied] = useState(false);
  const verified = v.status === "verified";

  async function reVerify() {
    setBusy(true);
    try {
      const r = await verifyDomainNow(domain);
      if (r.status !== "verified") {
        toast(`Not verified yet — ${r.probe ?? "DNS still propagating"}`, {});
      } else {
        toast(`${domain} verified and live`, { tone: "blue" });
      }
      onChange();
    } catch (e) {
      toast(`Verify failed: ${String(e).replace(/^Error:\s*/, "")}`, {});
    } finally {
      setBusy(false);
    }
  }

  async function detach() {
    if (!confirm(`Detach ${domain} from project "${v.project}"? The domain stops routing here immediately (its DNS records page stays).`)) return;
    setBusy(true);
    try {
      await detachDomain(v.project, domain);
      toast(`${domain} detached`, { tone: "blue" });
      onChange();
    } catch (e) {
      toast(`Couldn't detach: ${String(e).replace(/^Error:\s*/, "")}`, {});
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className={`mb-10 rounded-lg border p-4 ${verified ? "border-emerald-500/30 bg-emerald-500/5" : "border-amber-500/40 bg-amber-500/5"}`}>
      <div className="flex items-center justify-between gap-3">
        <div className="flex items-center gap-2">
          {verified
            ? <Badge tone="green"><ShieldCheck className="h-3.5 w-3.5" /> Verified — live on {v.project}</Badge>
            : <Badge tone="amber"><AlertTriangle className="h-3.5 w-3.5" /> Ownership not proven yet</Badge>}
        </div>
        <div className="flex items-center gap-2">
          {!verified && (
            <Button variant="outline" onClick={reVerify} disabled={busy}>
              {busy ? <Loader2 className="h-4 w-4 animate-spin" /> : null}
              Verify now
            </Button>
          )}
          <Button variant="danger" onClick={detach} disabled={busy}>
            <Link2 className="h-4 w-4" /> Detach
          </Button>
        </div>
      </div>
      {!verified && (
        <div className="mt-3">
          <p className="text-xs text-secondary">
            Routing stays off until this domain proves ownership. Add the TXT record below at your DNS provider —
            verification then completes automatically (checked continuously).
          </p>
          <div className="mt-2 flex items-start gap-3 rounded-md border border-border bg-card p-3">
            <div className="min-w-0 flex-1">
              <div className="font-mono text-xs text-secondary">{v.txt_name}</div>
              <div className="break-all font-mono text-xs text-fg">{v.txt_value}</div>
              <div className="mt-0.5 text-[11px] text-muted">TXT · exact name and value · last check: {v.last_probe || "not yet"}</div>
            </div>
            <button
              onClick={async () => {
                if (await (await import("@/lib/utils")).copyText(`${v.txt_name}  ${v.txt_value}`)) {
                  setCopied(true);
                  setTimeout(() => setCopied(false), 1200);
                }
              }}
              className="shrink-0 text-muted hover:text-fg"
              title="Copy TXT"
            >
              {copied ? <Check className="h-3.5 w-3.5 text-green" /> : <Copy className="h-3.5 w-3.5" />}
            </button>
          </div>
        </div>
      )}
      {verified && v.verified_ms > 0 && (
        <p className="mt-2 text-xs text-muted">Verified {timeAgo(v.verified_ms)} ago · attached to project {v.project}</p>
      )}
    </div>
  );
}

function Section({ title, desc, action, children }: { title: string; desc?: string; action?: React.ReactNode; children: React.ReactNode }) {
  return (
    <div className="mb-10">
      <div className="mb-4 flex items-start justify-between gap-4">
        <div>
          <h2 className="text-xl font-semibold tracking-tight">{title}</h2>
          {desc ? <p className="mt-1 max-w-2xl text-sm text-secondary">{desc}</p> : null}
        </div>
        {action}
      </div>
      {children}
    </div>
  );
}

function AutoRenew({ domain, on, onChange }: { domain: string; on: boolean; onChange: () => void }) {
  const [v, setV] = useState(on);
  return (
    <Switch checked={v} onChange={(next) => { setV(next); apiSend("PUT", `/v1/domains/${encodeURIComponent(domain)}/auto-renew`, { on: next }).then(onChange).catch(() => setV(on)); }} />
  );
}

function DnsRecords({ domain, records, onChange }: { domain: string; records: DnsRecord[]; onChange: () => void }) {
  const [name, setName] = useState("");
  const [type, setType] = useState("A");
  const [value, setValue] = useState("");
  const [ttl, setTtl] = useState("60");
  const [priority, setPriority] = useState("");
  const [comment, setComment] = useState("");
  const [busy, setBusy] = useState(false);
  // When set, the form edits an existing record (PUT) instead of adding (POST).
  const [editId, setEditId] = useState<string | null>(null);
  const needsPriority = type === "MX" || type === "SRV";

  function reset() {
    setName(""); setType("A"); setValue(""); setTtl("60"); setPriority(""); setComment(""); setEditId(null);
  }
  function startEdit(r: DnsRecord) {
    setEditId(r.id);
    setName(r.name); setType(r.type); setValue(r.value);
    setTtl(String(r.ttl)); setPriority(r.priority != null ? String(r.priority) : ""); setComment(r.comment || "");
    if (typeof window !== "undefined") window.scrollTo({ top: 0, behavior: "smooth" });
  }

  async function submit() {
    if (!value.trim()) return;
    setBusy(true);
    const body = {
      name, type, value, ttl: parseInt(ttl) || 60,
      priority: needsPriority && priority ? parseInt(priority) : null,
      comment,
    };
    try {
      if (editId) {
        await apiSend("PUT", `/v1/domains/${encodeURIComponent(domain)}/records/${encodeURIComponent(editId)}`, body);
      } else {
        await apiSend("POST", `/v1/domains/${encodeURIComponent(domain)}/records`, body);
      }
      reset();
      onChange();
    } finally { setBusy(false); }
  }
  async function del(id: string) {
    await apiSend("DELETE", `/v1/domains/${encodeURIComponent(domain)}/records/${encodeURIComponent(id)}`).then(onChange).catch(() => {});
  }

  return (
    <Section title="DNS Records" desc="DNS records point to services your domain uses — forwarding, email (MX), subdomains (A/CNAME), wildcards (*), and more.">
      {/* Add / edit form */}
      <Card className="mb-4 p-5">
        {editId && (
          <div className="mb-3 flex items-center gap-2 rounded-md bg-link/10 px-3 py-2 text-xs text-link">
            <Pencil className="h-3.5 w-3.5" /> Editing a record — change the fields and save.
          </div>
        )}
        <div className="grid grid-cols-1 gap-3 md:grid-cols-[1fr_120px_1.4fr_90px_90px]">
          <Field label="Name"><Input placeholder="subdomain  (or * for wildcard)" value={name} onChange={(e) => setName(e.target.value)} /></Field>
          <Field label="Type">
            <select value={type} onChange={(e) => setType(e.target.value)} className="w-full rounded-md border border-border bg-card px-3 py-2 text-sm focus:border-border-strong focus:outline-none">
              {RECORD_TYPES.map((t) => <option key={t} value={t}>{t}</option>)}
            </select>
          </Field>
          <Field label="Value"><Input placeholder={type === "A" ? "76.76.21.21" : type === "CNAME" ? "cname.openedge.app" : "value"} value={value} onChange={(e) => setValue(e.target.value)} /></Field>
          <Field label="TTL"><Input value={ttl} onChange={(e) => setTtl(e.target.value)} /></Field>
          <Field label="Priority"><Input value={priority} onChange={(e) => setPriority(e.target.value)} disabled={!needsPriority} placeholder={needsPriority ? "10" : "—"} /></Field>
        </div>
        <div className="mt-3"><Field label="Comment"><Input placeholder="A comment explaining what this DNS record is for" value={comment} onChange={(e) => setComment(e.target.value)} /></Field></div>
        <div className="mt-4 flex items-center justify-between border-t border-border pt-4">
          <span className="text-xs text-muted">Wildcards (<span className="font-mono">*</span>), apex (empty name), MX, CNAME, TXT, CAA &amp; more supported.</span>
          <div className="flex gap-2">
            {editId && <Button variant="outline" onClick={reset}>Cancel</Button>}
            <Button onClick={submit} disabled={busy || !value.trim()}>
              {busy ? <Loader2 className="h-4 w-4 animate-spin" /> : editId ? <Check className="h-4 w-4" /> : <Plus className="h-4 w-4" />} {editId ? "Save" : "Add"}
            </Button>
          </div>
        </div>
      </Card>

      {/* Records table */}
      <Card className="p-0">
        <div className="grid grid-cols-[1.2fr_0.7fr_1.6fr_0.6fr_0.7fr_0.9fr_auto] border-b border-border px-4 py-2.5 text-xs font-medium uppercase tracking-wide text-muted">
          <span>Name</span><span>Type</span><span>Value</span><span>TTL</span><span>Priority</span><span>Age</span><span></span>
        </div>
        {records.length === 0 ? (
          <div className="px-4 py-10 text-center text-sm text-secondary">No DNS records yet.</div>
        ) : (
          records.map((r) => (
            <div key={r.id} className={`grid grid-cols-[1.2fr_0.7fr_1.6fr_0.6fr_0.7fr_0.9fr_auto] items-center gap-2 border-b border-border px-4 py-3 text-sm last:border-0 ${editId === r.id ? "bg-link/5" : ""}`}>
              <span className="flex items-center gap-1.5 font-mono text-xs">{r.system && <Lock className="h-3 w-3 text-muted" />}{r.name || <span className="text-muted">@</span>}</span>
              <span className="font-medium">{r.type}</span>
              <span className="truncate font-mono text-xs" title={r.value}>{r.value}</span>
              <span className="text-secondary">{r.ttl}</span>
              <span className="text-secondary">{r.priority ?? "—"}</span>
              <span className="text-muted">{timeAgo(r.created_ms)}</span>
              <span className="flex items-center justify-end gap-2">
                {!r.system && (
                  <>
                    <button onClick={() => startEdit(r)} title="Edit" className="text-muted hover:text-fg"><Pencil className="h-3.5 w-3.5" /></button>
                    <button onClick={() => del(r.id)} title="Delete" className="text-muted hover:text-red-500"><Trash2 className="h-3.5 w-3.5" /></button>
                  </>
                )}
              </span>
            </div>
          ))
        )}
      </Card>
    </Section>
  );
}

/** Migrate existing DNS: auto-detect the domain's current public records (via the
 *  node's DNS-over-HTTPS scan) or paste a BIND zone file, then import in bulk. */
function MigrateDns({ domain, onChange }: { domain: string; onChange: () => void }) {
  const [scanning, setScanning] = useState(false);
  const [scanned, setScanned] = useState<ScanRecord[] | null>(null);
  const [picked, setPicked] = useState<Set<number>>(new Set());
  const [zone, setZone] = useState("");
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState("");

  async function scan() {
    setScanning(true); setMsg("");
    try {
      const r = await apiGet<{ records: ScanRecord[] }>(`/v1/domains/${encodeURIComponent(domain)}/scan`);
      setScanned(r.records || []);
      setPicked(new Set((r.records || []).map((_, i) => i))); // pre-select all
      if (!r.records?.length) setMsg("No public DNS records found for this domain yet.");
    } catch (e) { setMsg(String(e)); } finally { setScanning(false); }
  }
  function toggle(i: number) {
    setPicked((s) => { const n = new Set(s); n.has(i) ? n.delete(i) : n.add(i); return n; });
  }
  async function importScanned() {
    if (!scanned) return;
    const records = scanned.filter((_, i) => picked.has(i));
    if (!records.length) return;
    setBusy(true); setMsg("");
    try {
      const r = await apiSend<{ imported: number }>("POST", `/v1/domains/${encodeURIComponent(domain)}/import`, { records });
      setMsg(`Imported ${r.imported} record(s).`);
      setScanned(null); setPicked(new Set());
      onChange();
    } catch (e) { setMsg(String(e)); } finally { setBusy(false); }
  }
  async function importZone() {
    if (!zone.trim()) return;
    setBusy(true); setMsg("");
    try {
      const r = await apiSend<{ imported: number }>("POST", `/v1/domains/${encodeURIComponent(domain)}/import`, { zone });
      setMsg(`Imported ${r.imported} record(s) from the zone file.`);
      setZone("");
      onChange();
    } catch (e) { setMsg(String(e)); } finally { setBusy(false); }
  }

  return (
    <Section title="Migrate DNS" desc="Moving a domain from another provider? Detect its current records automatically, or paste your existing zone file — then import them here in one step.">
      <Card className="p-5">
        {/* Auto-detect */}
        <div className="flex flex-wrap items-center justify-between gap-3">
          <div>
            <div className="text-sm font-medium">Detect existing records</div>
            <div className="text-xs text-muted">Looks up the live A, AAAA, MX, TXT, NS, CAA and www records for {domain}.</div>
          </div>
          <Button variant="outline" onClick={scan} disabled={scanning}>
            {scanning ? <Loader2 className="h-4 w-4 animate-spin" /> : <DownloadCloud className="h-4 w-4" />} Scan existing DNS
          </Button>
        </div>

        {scanned && scanned.length > 0 && (
          <div className="mt-4 overflow-hidden rounded-lg border border-border">
            <div className="flex items-center justify-between border-b border-border bg-subtle/40 px-3 py-2 text-xs text-muted">
              <span>{picked.size} of {scanned.length} selected</span>
              <button onClick={() => setPicked(picked.size === scanned.length ? new Set() : new Set(scanned.map((_, i) => i)))} className="hover:text-fg">
                {picked.size === scanned.length ? "Deselect all" : "Select all"}
              </button>
            </div>
            {scanned.map((r, i) => (
              <button key={i} onClick={() => toggle(i)} className="flex w-full items-center gap-3 border-b border-border px-3 py-2 text-left text-sm last:border-0 hover:bg-subtle/40">
                <span className={`flex h-4 w-4 shrink-0 items-center justify-center rounded border ${picked.has(i) ? "border-fg bg-fg text-bg" : "border-border-strong"}`}>
                  {picked.has(i) && <Check className="h-3 w-3" strokeWidth={3} />}
                </span>
                <span className="w-14 font-medium">{r.type}</span>
                <span className="w-24 truncate font-mono text-xs">{r.name || "@"}</span>
                <span className="flex-1 truncate font-mono text-xs text-secondary">{r.value}</span>
                {r.priority != null && <span className="text-xs text-muted">pri {r.priority}</span>}
              </button>
            ))}
            <div className="flex justify-end border-t border-border px-3 py-2.5">
              <Button onClick={importScanned} disabled={busy || picked.size === 0}>
                {busy ? <Loader2 className="h-4 w-4 animate-spin" /> : <FileUp className="h-4 w-4" />} Import {picked.size} record(s)
              </Button>
            </div>
          </div>
        )}

        {/* Paste a zone file */}
        <div className="mt-6 border-t border-border pt-5">
          <div className="text-sm font-medium">Or paste a zone file</div>
          <div className="mt-0.5 text-xs text-muted">BIND-style lines, e.g. <span className="font-mono">www 3600 IN A 76.76.21.21</span></div>
          <textarea
            value={zone}
            onChange={(e) => setZone(e.target.value)}
            rows={5}
            placeholder={"@      IN  A      76.76.21.21\nwww    IN  CNAME  app.example.com.\n@      IN  MX     10 mail.example.com."}
            className="mt-3 w-full resize-y rounded-lg border border-border bg-card px-3 py-2.5 font-mono text-xs focus:border-border-strong focus:outline-none"
          />
          <div className="mt-3 flex justify-end">
            <Button onClick={importZone} disabled={busy || !zone.trim()}>
              {busy ? <Loader2 className="h-4 w-4 animate-spin" /> : <FileUp className="h-4 w-4" />} Import zone file
            </Button>
          </div>
        </div>

        {msg && <p className="mt-4 text-sm text-secondary">{msg}</p>}
      </Card>
    </Section>
  );
}

/** Attach this domain (or a subdomain of it) to a project. */
function ConnectDomain({ domain, open, onClose, onChange }: { domain: string; open: boolean; onClose: () => void; onChange: () => void }) {
  const { data: deps } = usePoll<Deployment[]>("/deployments", 8000);
  const projects = Array.from(new Set((deps ?? []).map((d) => d.project)));
  const [project, setProject] = useState("");
  const [sub, setSub] = useState("");
  const [err, setErr] = useState("");
  const [busy, setBusy] = useState(false);
  const [pending, setPending] = useState<DomainAttachResult | null>(null);
  const [copied, setCopied] = useState(false);
  if (!open) return null;
  const fqdn = sub.trim() ? `${sub.trim()}.${domain}` : domain;

  async function connect() {
    if (!project) { setErr("Pick a project."); return; }
    setBusy(true); setErr("");
    try {
      const r = await attachDomain(project, fqdn);
      onChange();
      if (r.status === "pending" && r.verify) {
        // Attached but not live: ownership TXT is outstanding — show it
        // instead of closing as if the domain were already routed.
        setPending(r);
      } else {
        onClose(); setProject(""); setSub(""); setPending(null);
      }
    } catch (e) { setErr(String(e)); } finally { setBusy(false); }
  }

  if (pending?.verify) {
    return (
      <div className="fixed inset-0 z-40 flex items-center justify-center bg-black/40 p-4" onClick={onClose}>
        <Card className="w-full max-w-md" onClick={(e) => e.stopPropagation()}>
          <div className="flex items-center justify-between">
            <h3 className="text-lg font-semibold">Verify ownership</h3>
            <button onClick={onClose} className="text-muted hover:text-fg"><X className="h-4 w-4" /></button>
          </div>
          <p className="mt-1 text-sm text-secondary">
            <span className="font-mono">{fqdn}</span> is attached to <span className="font-medium text-fg">{project}</span> but
            not live yet — add this TXT record at your DNS provider to prove ownership. Verification then completes
            automatically (checked continuously).
          </p>
          <div className="mt-3 flex items-start gap-3 rounded-md border border-border bg-subtle/30 p-3">
            <div className="min-w-0 flex-1">
              <div className="font-mono text-xs text-secondary">{pending.verify.txt_name}</div>
              <div className="break-all font-mono text-xs text-fg">{pending.verify.txt_value}</div>
            </div>
            <button
              onClick={async () => {
                if (await copyText(`${pending.verify!.txt_name}  ${pending.verify!.txt_value}`)) {
                  setCopied(true);
                  setTimeout(() => setCopied(false), 1200);
                }
              }}
              className="shrink-0 text-muted hover:text-fg"
              title="Copy TXT"
            >
              {copied ? <Check className="h-3.5 w-3.5 text-green" /> : <Copy className="h-3.5 w-3.5" />}
            </button>
          </div>
          <div className="mt-5 flex justify-end">
            <Button onClick={() => { onClose(); setProject(""); setSub(""); setPending(null); }}>Done</Button>
          </div>
        </Card>
      </div>
    );
  }

  return (
    <div className="fixed inset-0 z-40 flex items-center justify-center bg-black/40 p-4" onClick={onClose}>
      <Card className="w-full max-w-md" onClick={(e) => e.stopPropagation()}>
        <div className="flex items-center justify-between">
          <h3 className="text-lg font-semibold">Connect to a project</h3>
          <button onClick={onClose} className="text-muted hover:text-fg"><X className="h-4 w-4" /></button>
        </div>
        <p className="mt-1 text-sm text-secondary">Point <span className="font-mono">{fqdn}</span> at a project&apos;s production deployment.</p>
        <label className="mt-5 block text-sm font-medium">Subdomain (optional)</label>
        <div className="mt-1.5 flex items-center gap-2">
          <Input placeholder="www, app, … (blank = apex)" value={sub} onChange={(e) => setSub(e.target.value)} />
          <span className="shrink-0 text-sm text-muted">.{domain}</span>
        </div>
        <label className="mt-4 block text-sm font-medium">Project</label>
        <select value={project} onChange={(e) => setProject(e.target.value)} className="mt-1.5 w-full rounded-md border border-border bg-card px-3 py-2 text-sm focus:outline-none">
          <option value="">Select a project…</option>
          {projects.map((p) => <option key={p} value={p}>{p}</option>)}
        </select>
        {err ? <p className="mt-3 text-sm text-red-500">{err}</p> : null}
        <div className="mt-5 flex justify-between">
          <Button variant="outline" onClick={onClose}>Cancel</Button>
          <Button onClick={connect} disabled={busy}>{busy ? <Loader2 className="h-4 w-4 animate-spin" /> : <Plus className="h-4 w-4" />} Connect</Button>
        </div>
      </Card>
    </div>
  );
}

function Nameservers({ domain, nameservers, attached, onChange }: { domain: string; nameservers: string[]; attached: boolean; onChange: () => void }) {
  const [editing, setEditing] = useState(false);
  const [text, setText] = useState(nameservers.join("\n"));
  const [roster, setRoster] = useState<NsRoster | null>(null);
  // Attached domains have no stored nameserver truth — the record's
  // `nameservers` are simulated placeholders. The real, peer-attested
  // platform set comes from the roster endpoint.
  useEffect(() => {
    if (attached) fetchNsRoster().then(setRoster).catch(() => setRoster(null));
  }, [attached]);
  async function save() {
    const ns = text.split("\n").map((s) => s.trim()).filter(Boolean);
    await apiSend("PUT", `/v1/domains/${encodeURIComponent(domain)}/nameservers`, { nameservers: ns });
    setEditing(false); onChange();
  }
  if (attached) {
    const ns = roster?.nameservers ?? [];
    const ready = roster?.delegation_ready === true && ns.length > 0;
    return (
      <Section
        title="Nameservers"
        desc="Delegate these platform nameservers at your registrar if you want us to serve this domain's DNS zone."
      >
        <Card className="p-0">
          {ready ? (
            ns.map((n) => (
              <div key={n} className="border-b border-border px-4 py-3 font-mono text-sm last:border-0">{n}</div>
            ))
          ) : (
            <div className="px-4 py-3 text-sm text-secondary">Not yet ready — the platform nameserver set is still converging.</div>
          )}
        </Card>
      </Section>
    );
  }
  return (
    <Section
      title="Nameservers"
      desc="By default, DevHub propagates its nameservers for your domains. You can view them or add custom ones here."
      action={editing ? <div className="flex gap-2"><Button variant="outline" onClick={() => { setEditing(false); setText(nameservers.join("\n")); }}>Cancel</Button><Button onClick={save}>Save</Button></div> : <div className="flex gap-2"><Button variant="outline" onClick={() => apiSend("PUT", `/v1/domains/${encodeURIComponent(domain)}/nameservers`, { nameservers: ["ns1.openedge-dns.com", "ns2.openedge-dns.com"] }).then(onChange)}>Restore Original</Button><Button onClick={() => { setText(nameservers.join("\n")); setEditing(true); }}>Edit</Button></div>}
    >
      <Card className="p-0">
        {editing ? (
          <textarea value={text} onChange={(e) => setText(e.target.value)} rows={Math.max(2, nameservers.length)} className="w-full resize-none rounded-xl bg-card px-4 py-3 font-mono text-sm focus:outline-none" />
        ) : (
          nameservers.map((ns) => (
            <div key={ns} className="border-b border-border px-4 py-3 font-mono text-sm last:border-0">{ns}</div>
          ))
        )}
      </Card>
    </Section>
  );
}

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div>
      <label className="mb-1.5 block text-xs text-secondary">{label}</label>
      {children}
    </div>
  );
}
