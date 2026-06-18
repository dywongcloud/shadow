"use client";

import { useEffect, useMemo, useState } from "react";
import { Github, Search, Plus, Database, Box, Lock, CreditCard, Loader2, Check } from "lucide-react";
import { Card, Button, Input, Badge, PageHeader, Triangle } from "@/components/ui";
import { cachedJson } from "@/lib/cache";

function Logo({ kind }: { kind: string }) {
  const wrap = "flex h-10 w-10 items-center justify-center rounded-lg border border-border bg-subtle";
  if (kind === "github") return <span className={wrap}><Github className="h-5 w-5" /></span>;
  if (kind === "postgres") return <span className={wrap}><Database className="h-5 w-5" /></span>;
  if (kind === "kv") return <span className={wrap}><Box className="h-5 w-5" /></span>;
  if (kind === "blob") return <span className={wrap}><Box className="h-5 w-5" /></span>;
  if (kind === "stripe") return <span className={wrap}><CreditCard className="h-5 w-5" /></span>;
  if (kind === "auth0") return <span className={wrap}><Lock className="h-5 w-5" /></span>;
  return <Triangle className="h-10 w-10" />;
}

interface Toolkit {
  slug: string;
  name: string;
  logo?: string;
  categories: string[];
  description?: string;
  no_auth: boolean;
}

const PER_PAGE = 24;
// Already represented in the Connected section — never duplicate in the grid.
const HIDDEN_SLUGS = new Set(["github"]);

function ToolkitLogo({ tk }: { tk: Toolkit }) {
  const [broken, setBroken] = useState(false);
  const wrap = "flex h-10 w-10 shrink-0 items-center justify-center overflow-hidden rounded-lg border border-border bg-subtle";
  if (tk.logo && !broken) {
    return (
      <span className={wrap}>
        {/* eslint-disable-next-line @next/next/no-img-element */}
        <img src={tk.logo} alt={tk.name} className="h-10 w-10 object-contain" onError={() => setBroken(true)} />
      </span>
    );
  }
  return (
    <span className={wrap}>
      <span className="text-sm font-semibold text-secondary">{(tk.name || tk.slug).charAt(0).toUpperCase()}</span>
    </span>
  );
}

function ToolkitCard({ tk, configured, onConnect, connecting }: { tk: Toolkit; configured: boolean; onConnect: (s: string) => void; connecting: boolean }) {
  return (
    <Card className="flex h-full flex-col p-4">
      <div className="mb-3 flex items-start gap-3">
        <ToolkitLogo tk={tk} />
        <div className="min-w-0 flex-1">
          <div className="truncate font-semibold" title={tk.name}>{tk.name}</div>
          {tk.description ? (
            <p className="mt-0.5 line-clamp-2 text-xs text-secondary">{tk.description}</p>
          ) : (
            <p className="mt-0.5 text-xs text-muted">{tk.slug}</p>
          )}
        </div>
      </div>
      {tk.categories.length > 0 ? (
        <div className="mb-3 flex flex-wrap gap-1">
          {tk.categories.slice(0, 2).map((c) => <Badge key={c} tone="blue">{c}</Badge>)}
        </div>
      ) : null}
      <div className="mt-auto pt-1">
        <Button variant="outline" className="w-full" disabled={!configured || connecting} onClick={() => onConnect(tk.slug)}>
          {connecting ? <Loader2 className="h-4 w-4 animate-spin" /> : <Plus className="h-4 w-4" />}
          {connecting ? "Connecting…" : "Connect"}
        </Button>
      </div>
    </Card>
  );
}

export default function IntegrationsPage() {
  const [gh, setGh] = useState<{ configured: boolean; connected: boolean }>({ configured: false, connected: false });

  // Marketplace catalog (loaded inline).
  const [toolkits, setToolkits] = useState<Toolkit[]>([]);
  const [tkConfigured, setTkConfigured] = useState(true);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [q, setQ] = useState("");
  const [page, setPage] = useState(0);
  const [connecting, setConnecting] = useState<string | null>(null);

  useEffect(() => {
    // GitHub status: short cache (reflects connection state).
    cachedJson<{ configured: boolean; connected: boolean }>("/api/github/status", 30_000)
      .then((s) => setGh({ configured: !!s.configured, connected: !!s.connected }))
      .catch(() => {});
  }, []);

  useEffect(() => {
    let cancelled = false;
    // Catalog: cached for an hour client-side (and on the server route) so the
    // 1,000+ toolkit list isn't refetched every visit.
    cachedJson<{ configured: boolean; toolkits: Toolkit[] }>("/api/composio/toolkits", 60 * 60_000)
      .then((d) => {
        if (cancelled) return;
        setTkConfigured(!!d.configured);
        const list: Toolkit[] = Array.isArray(d.toolkits) ? d.toolkits : [];
        setToolkits(list.filter((t) => !HIDDEN_SLUGS.has(t.slug)));
      })
      .catch(() => { if (!cancelled) setError("Failed to load the integration catalog."); })
      .finally(() => { if (!cancelled) setLoading(false); });
    return () => { cancelled = true; };
  }, []);

  const filtered = useMemo(() => {
    const needle = q.trim().toLowerCase();
    if (!needle) return toolkits;
    return toolkits.filter(
      (t) =>
        t.name.toLowerCase().includes(needle) ||
        t.slug.toLowerCase().includes(needle) ||
        t.categories.some((c) => c.toLowerCase().includes(needle))
    );
  }, [toolkits, q]);

  const pageCount = Math.max(1, Math.ceil(filtered.length / PER_PAGE));
  const safePage = Math.min(page, pageCount - 1);
  const shown = filtered.slice(safePage * PER_PAGE, safePage * PER_PAGE + PER_PAGE);

  // Reset to page 0 whenever the search changes.
  useEffect(() => { setPage(0); }, [q]);

  async function connectGithub() {
    const r = await fetch("/api/github/connect", { method: "POST" });
    const d = await r.json();
    if (d.redirectUrl) window.location.href = d.redirectUrl;
  }

  async function connectToolkit(slug: string) {
    setConnecting(slug);
    setError(null);
    try {
      const r = await fetch("/api/composio/connect", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ slug }) });
      const d = await r.json();
      if (d.redirectUrl) { window.location.href = d.redirectUrl; return; }
      setError(d.error || `Could not connect ${slug}.`);
    } catch {
      setError(`Could not connect ${slug}.`);
    } finally {
      setConnecting(null);
    }
  }

  const connected = [
    { kind: "github", name: "GitHub", desc: "Deploy projects automatically with Git integration", active: gh.connected },
    { kind: "postgres", name: "OpenEdge Postgres", desc: "Serverless SQL database built for the edge", active: true },
    { kind: "kv", name: "OpenEdge KV", desc: "Durable Redis database for caching", active: true },
  ];

  return (
    <div className="pb-16">
      <PageHeader
        title="Integrations"
        desc="Connect your stack — deploy from Git and link 1,000+ tools via Composio."
      />

      <h2 className="mb-3 text-base font-semibold">Connected</h2>
      <div className="mb-10 grid grid-cols-1 gap-4 md:grid-cols-3">
        {connected.map((i) => (
          <Card key={i.name} className="p-5">
            <div className="mb-3 flex items-start justify-between">
              <div className="flex items-center gap-3">
                <Logo kind={i.kind} />
                <div>
                  <div className="font-semibold">{i.name}</div>
                  <div className="text-xs text-muted">{i.kind === "github" ? (gh.connected ? "connected" : "not connected") : "hive-cloud"}</div>
                </div>
              </div>
              <Badge tone={i.active ? "green" : "default"}>
                {i.active ? <><Check className="h-3 w-3" /> Active</> : "Inactive"}
              </Badge>
            </div>
            <p className="mb-4 text-sm text-secondary">{i.desc}</p>
            <div className="flex items-center gap-3">
              {i.kind === "github" && !gh.connected && gh.configured ? (
                <Button onClick={connectGithub}>Connect</Button>
              ) : (
                <Button variant="outline">Configure</Button>
              )}
              <button className="text-sm text-secondary hover:text-fg">Disconnect</button>
            </div>
          </Card>
        ))}
      </div>

      {/* Marketplace search — type to find any of the 1,000+ integrations. */}
      <div className="relative mb-5 max-w-xl">
        <Search className="absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted" />
        <Input
          placeholder={loading ? "Loading catalog…" : "Search integrations…"}
          className="pl-9"
          value={q}
          onChange={(e) => setQ(e.target.value)}
        />
      </div>

      {!loading && !tkConfigured && (
        <div className="mb-4 rounded-md border border-amber-500/30 bg-amber-500/10 px-3 py-2 text-xs text-amber-600 dark:text-amber-400">
          Set <code>COMPOSIO_API_KEY</code> to connect the catalog. Browsing still works.
        </div>
      )}
      {error && (
        <div className="mb-4 rounded-md border border-red-500/30 bg-red-500/10 px-3 py-2 text-xs text-red-600 dark:text-red-400">{error}</div>
      )}

      {loading ? (
        <div className="flex h-40 items-center justify-center text-sm text-muted">
          <Loader2 className="mr-2 h-4 w-4 animate-spin" /> Loading the full catalog…
        </div>
      ) : filtered.length === 0 ? (
        <div className="flex h-40 items-center justify-center text-sm text-muted">No integrations match your search.</div>
      ) : (
        <>
          <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4">
            {shown.map((tk) => (
              <ToolkitCard key={tk.slug} tk={tk} configured={tkConfigured} connecting={connecting === tk.slug} onConnect={connectToolkit} />
            ))}
          </div>
          <div className="mt-6 flex items-center justify-between text-sm">
            <span className="text-muted">
              {filtered.length.toLocaleString()} integration{filtered.length === 1 ? "" : "s"} · page {safePage + 1} of {pageCount}
            </span>
            <div className="flex gap-2">
              <Button variant="outline" disabled={safePage === 0} onClick={() => setPage(safePage - 1)}>Previous</Button>
              <Button variant="outline" disabled={safePage >= pageCount - 1} onClick={() => setPage(safePage + 1)}>Next</Button>
            </div>
          </div>
        </>
      )}
    </div>
  );
}

