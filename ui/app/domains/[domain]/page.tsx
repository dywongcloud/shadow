"use client";

import { useState } from "react";
import Link from "next/link";
import {
  ChevronRight, RefreshCw, ShieldCheck, Lock, Trash2, Plus, Loader2, Copy, MoreHorizontal,
} from "lucide-react";
import { Card, Button, Input, Triangle } from "@/components/ui";
import { Switch } from "@/components/ui";
import { apiSend, usePoll, type DomainDetail, type DnsRecord } from "@/lib/api";
import { timeAgo } from "@/lib/utils";

const RECORD_TYPES = ["A", "AAAA", "CNAME", "ALIAS", "MX", "TXT", "CAA", "NS", "SRV"];

function fmtDate(ms: number) {
  return new Date(ms).toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" });
}

export default function DomainDetailPage({ params }: { params: { domain: string } }) {
  const domain = decodeURIComponent(params.domain);
  const { data, refresh } = usePoll<DomainDetail>(`/v1/domains/${encodeURIComponent(domain)}`, 6000);

  if (!data) {
    return <div className="py-20 text-center text-sm text-secondary"><Loader2 className="mx-auto h-5 w-5 animate-spin" /></div>;
  }
  const d = data.domain;

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
          <Button>Renew Domain</Button>
          <button className="flex h-9 w-9 items-center justify-center rounded-md border border-border-strong text-muted hover:bg-subtle"><MoreHorizontal className="h-4 w-4" /></button>
        </div>
      </div>

      {/* Meta row */}
      <div className="mb-8 grid grid-cols-2 gap-y-4 border-b border-border pb-6 text-sm sm:grid-cols-3 lg:grid-cols-6">
        <Meta label="Expiration Date" value={<span className="flex items-center gap-1.5"><RefreshCw className="h-3.5 w-3.5 text-muted" /> {fmtDate(d.expires_ms)}</span>} />
        <Meta label="Renewal Price" value={d.renewal_price} />
        <Meta label="Registrar" value={<span className="flex items-center gap-1.5"><Triangle className="h-4 w-4" /> {d.registrar}</span>} />
        <Meta label="Auto Renewal" value={<AutoRenew domain={domain} on={d.auto_renew} onChange={refresh} />} />
        <Meta label="Age" value={timeAgo(d.created_ms)} />
        <Meta label="OpenEdge CDN" value={d.cdn_active ? <span className="flex items-center gap-1.5 text-emerald-500"><ShieldCheck className="h-4 w-4" /> Active</span> : "Inactive"} />
      </div>

      {/* Connected Projects */}
      <Section title="Connected Projects" desc="Subdomains that are connected to projects on this team." action={<Button variant="outline">Connect</Button>}>
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

      {/* Nameservers */}
      <Nameservers domain={domain} nameservers={d.nameservers} onChange={refresh} />

      {/* SSL Certificates */}
      <Section title="SSL Certificates" desc="By default, OpenEdge issues and auto-renews a free SSL certificate for your domains.">
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

      {/* Registrant */}
      <Section title="Registrant Information" desc="We collect this information to meet ICANN requirements and establish you as the legal domain holder." action={<Button variant="outline">Manage WHOIS Privacy</Button>}>
        <Card className="p-5 text-sm text-secondary">WHOIS privacy is enabled — your contact details are protected.</Card>
      </Section>
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
  const needsPriority = type === "MX" || type === "SRV";

  async function add() {
    if (!value.trim()) return;
    setBusy(true);
    try {
      await apiSend("POST", `/v1/domains/${encodeURIComponent(domain)}/records`, {
        name, type, value, ttl: parseInt(ttl) || 60,
        priority: needsPriority && priority ? parseInt(priority) : null,
        comment,
      });
      setName(""); setValue(""); setComment(""); setPriority("");
      onChange();
    } finally { setBusy(false); }
  }
  async function del(id: string) {
    await apiSend("DELETE", `/v1/domains/${encodeURIComponent(domain)}/records/${encodeURIComponent(id)}`).then(onChange).catch(() => {});
  }

  return (
    <Section title="DNS Records" desc="DNS records point to services your domain uses — forwarding, email (MX), subdomains (A/CNAME), wildcards (*), and more.">
      {/* Add form */}
      <Card className="mb-4 p-5">
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
          <Button onClick={add} disabled={busy || !value.trim()}>{busy ? <Loader2 className="h-4 w-4 animate-spin" /> : <Plus className="h-4 w-4" />} Add</Button>
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
            <div key={r.id} className="grid grid-cols-[1.2fr_0.7fr_1.6fr_0.6fr_0.7fr_0.9fr_auto] items-center gap-2 border-b border-border px-4 py-3 text-sm last:border-0">
              <span className="flex items-center gap-1.5 font-mono text-xs">{r.system && <Lock className="h-3 w-3 text-muted" />}{r.name || <span className="text-muted">@</span>}</span>
              <span className="font-medium">{r.type}</span>
              <span className="truncate font-mono text-xs" title={r.value}>{r.value}</span>
              <span className="text-secondary">{r.ttl}</span>
              <span className="text-secondary">{r.priority ?? "—"}</span>
              <span className="text-muted">{timeAgo(r.created_ms)}</span>
              <span className="text-right">
                {!r.system && <button onClick={() => del(r.id)} className="text-muted hover:text-red-500"><Trash2 className="h-3.5 w-3.5" /></button>}
              </span>
            </div>
          ))
        )}
      </Card>
    </Section>
  );
}

function Nameservers({ domain, nameservers, onChange }: { domain: string; nameservers: string[]; onChange: () => void }) {
  const [editing, setEditing] = useState(false);
  const [text, setText] = useState(nameservers.join("\n"));
  async function save() {
    const ns = text.split("\n").map((s) => s.trim()).filter(Boolean);
    await apiSend("PUT", `/v1/domains/${encodeURIComponent(domain)}/nameservers`, { nameservers: ns });
    setEditing(false); onChange();
  }
  return (
    <Section
      title="Nameservers"
      desc="By default, OpenEdge propagates its nameservers for your domains. You can view them or add custom ones here."
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
