"use client";

import { useCallback, useEffect, useMemo, useState } from "react";
import Link from "next/link";
import { Github, Search, Plus, Database, Box, Lock, CreditCard, Loader2, Check, AlertTriangle, RefreshCw, Building2, GitBranch } from "lucide-react";
import { Card, Button, Input, Badge, PageHeader, Triangle } from "@/components/ui";
import { cachedJson, invalidate } from "@/lib/cache";
import { currentTeam } from "@/lib/api";

/** Enriched, HONEST GitHub connection detail (matches githubConnectionDetail). */
interface GhDetail {
  configured: boolean;
  connected: boolean; // ACTIVE in Composio AND the token actually works
  entity?: string | null;
  login?: string | null;
  scopes?: string[];
  hasPrivateAccess?: boolean; // `repo`
  hasOrgScope?: boolean; // `read:org`
  live?: boolean;
  /** Which auth path serves this connection: the first-party GitHub App (org-level
   *  permissions) or the legacy Composio-managed OAuth app. */
  provider?: "github-app" | "composio";
  /** What the state was derived FROM — "installation" means GitHub's own
   *  server-to-server installation record answered (browser-cookie-independent). */
  via?: "user-token" | "installation" | "composio";
  /** Where to install/configure the App on orgs (github-app provider only). */
  installUrl?: string;
}
interface GhOrg { login: string; name?: string }

function Logo({ kind }: { kind: string }) {
  // White tile behind every icon (glyphs forced dark so they read on white in dark mode).
  const wrap = "flex h-10 w-10 items-center justify-center rounded-lg border border-border bg-white text-neutral-800 shadow-sm";
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
  // White circular tile behind the logo image (Vercel-style), so transparent
  // PNGs/SVGs always sit on white in both light and dark mode.
  const wrap = "flex h-12 w-12 shrink-0 items-center justify-center overflow-hidden rounded-full border border-border bg-white shadow-sm";
  if (tk.logo && !broken) {
    return (
      <span className={wrap}>
        {/* eslint-disable-next-line @next/next/no-img-element */}
        <img src={tk.logo} alt={tk.name} loading="lazy" decoding="async" width={32} height={32} className="h-8 w-8 object-contain" onError={() => setBroken(true)} />
      </span>
    );
  }
  return (
    <span className={wrap}>
      <span className="text-base font-semibold text-neutral-700">{(tk.name || tk.slug).charAt(0).toUpperCase()}</span>
    </span>
  );
}

function ToolkitCard({ tk, configured, onConnect, connecting }: { tk: Toolkit; configured: boolean; onConnect: (s: string) => void; connecting: boolean }) {
  return (
    <button
      onClick={() => configured && !connecting && onConnect(tk.slug)}
      disabled={!configured || connecting}
      className="group relative flex h-44 w-full flex-col rounded-xl border border-border bg-card p-5 text-left transition-colors hover:border-border-strong hover:bg-subtle/40 disabled:cursor-default"
    >
      {/* Category badge, top-left */}
      <span className="self-start rounded-md bg-subtle px-2 py-0.5 text-[11px] font-medium capitalize text-secondary">
        {tk.categories[0] || "Integration"}
      </span>
      {/* Centered logo + name */}
      <div className="flex flex-1 flex-col items-center justify-center gap-2.5">
        <ToolkitLogo tk={tk} />
        <div className="max-w-full truncate text-center text-[15px] font-semibold" title={tk.name}>{tk.name}</div>
      </div>
      {/* Description */}
      <p className="line-clamp-2 min-h-[2.25rem] text-center text-xs leading-snug text-secondary">
        {connecting ? "Connecting…" : tk.description || tk.slug}
      </p>
    </button>
  );
}

/** Decorative wavy line-art tile (matches Vercel's marketplace promo card). */
function MarketplaceArtCard({ count }: { count: number }) {
  return (
    <div className="relative flex h-44 flex-col justify-end overflow-hidden rounded-xl border border-border bg-card p-5">
      <div className="absolute inset-0 z-0 overflow-hidden rounded">
        <svg width="100%" height="100%" viewBox="0 0 306 220" fill="none" xmlns="http://www.w3.org/2000/svg" className="absolute inset-0" preserveAspectRatio="none">
          <path d="M-72.6319 0.914097C-72.7551 0.842564 -72.8476 0.788746 -72.9092 0.752964C-72.9398 0.735172 -72.9632 0.721806 -72.9785 0.712925C-72.9861 0.70853 -72.9923 0.705357 -72.9961 0.703159C-72.9979 0.702122 -72.9991 0.700779 -73 0.70023C-73.001 0.699665 -73.0008 0.697788 -72.8135 0.375034L-72.8047 3.43034e-05C-72.8038 4.95621e-05 -72.8024 3.78558e-06 -72.8008 3.4303e-05C-72.7978 9.53379e-05 -72.7901 0.000888794 -72.7832 0.00101086C-72.768 0.00128552 -72.7444 0.00142285 -72.7139 0.00198742C-72.6523 0.00313182 -72.5597 0.00459666 -72.4365 0.00687021L-49.1192 0.437532C-33.3275 0.729462 -9.63991 1.16812 21.9433 1.75198L306.193 7.00684L306.187 7.38184L306.18 7.75586C179.846 5.42042 85.0963 3.6697 21.9297 2.50198C-8.54546 1.93861 -31.6692 1.51138 -47.4414 1.21976C-31.6687 1.97789 -8.53658 3.08919 21.9541 4.55471C85.1207 7.59079 179.871 12.1446 306.204 18.2168L306.187 18.5918L306.169 18.9658C179.836 12.8937 85.0856 8.33982 21.9189 5.30374C-8.52391 3.84051 -31.6313 2.72977 -47.4024 1.97171L306.216 29.4277L306.157 30.1758L-47.3516 2.72855C-31.5803 4.41954 -8.4709 6.89654 21.9765 10.1612L306.227 40.6387L306.146 41.3848L21.8965 10.9073C-8.44054 7.65448 -31.4927 5.18375 -47.2598 3.4932C-31.4905 5.65039 -8.40798 8.80681 21.9873 12.9649C85.1539 21.6061 179.904 34.5682 306.237 51.8506L306.187 52.2217L306.136 52.5938C179.802 35.3114 85.0524 22.3492 21.8857 13.708C-8.37935 9.56778 -31.3941 6.42018 -47.1582 4.2637L306.248 63.0625L306.125 63.8027C179.792 42.7836 85.0416 27.0184 21.875 16.5088L-47.0469 5.04202L22.0088 18.5742C85.1754 30.9522 179.926 49.5186 306.259 74.2744L306.114 75.0107C179.781 50.2549 85.0309 31.6875 21.8642 19.3096C-8.21697 13.415 -31.1357 8.92439 -46.8916 5.83695L22.0195 21.3789C85.1861 35.6252 179.936 56.9949 306.269 85.4873L306.187 85.8525L306.104 86.2188L21.8545 22.1104L-46.7422 6.63968L306.279 96.6992L306.187 97.0625L306.094 97.4268C179.76 65.1975 85.0104 41.0248 21.8437 24.9102L-46.5469 7.46292C-30.7963 11.947 -7.93425 18.4561 22.039 26.9893C85.2057 44.9723 179.956 71.9461 306.289 107.912L306.187 108.273L306.084 108.634L21.834 27.71L-46.3506 8.29886C-30.6062 13.2468 -7.80629 20.4114 22.0488 29.794C85.2155 49.6453 179.966 79.4232 306.299 119.126L306.074 119.841L21.8242 30.5098C-7.74208 21.218 -30.389 14.1009 -46.1162 9.15824C-30.3799 14.5691 -7.65489 22.3827 22.0586 32.5996C85.2252 54.3194 179.975 86.8994 306.309 130.339L306.064 131.048L-45.8574 10.041C-30.1308 15.9138 -7.48909 24.3691 22.0674 35.4063L306.317 141.553L306.187 141.903L306.056 142.255L21.8056 36.1084C-7.4218 25.1941 -29.888 16.8056 -45.5918 10.9414C-29.8759 17.2751 -7.31911 26.3651 22.0771 38.2119C85.2438 63.6684 179.994 101.853 306.327 152.766L306.187 153.114L306.046 153.462L-45.3369 11.8526C-29.632 18.6464 -7.15785 28.3686 22.0849 41.0186C85.2515 68.3434 180.002 109.33 306.335 163.979L306.187 164.324L306.038 164.668L21.7881 41.7061C-7.09944 29.2098 -29.3819 19.5715 -45.0586 12.7901C-29.3665 20.0423 -6.98213 30.3865 22.0937 43.8242C85.2604 73.0174 180.01 116.808 306.344 175.194L306.187 175.534L306.029 175.875C179.696 117.489 84.9459 73.6981 21.7793 44.5049C-6.89497 31.2528 -29.0611 21.0088 -44.7188 13.7725L306.352 186.408L306.187 186.744L306.021 187.081L-44.4072 14.7618C-28.7484 22.9249 -6.57542 34.4831 22.1103 49.4375L306.36 197.622L306.187 197.955L306.013 198.287L21.7637 50.1026C-6.48659 35.3752 -28.4203 23.9416 -44.0362 15.8008L22.1172 52.2442C85.2838 87.0424 180.034 139.239 306.367 208.836L306.187 209.165L306.006 209.493C179.673 139.897 84.9225 87.6997 21.7558 52.9014L-43.6787 16.8535L306.375 220.051L305.998 220.699L-72.6319 0.914097Z" className="fill-[#EBEBEB] dark:fill-[#2a2a2a]" />
        </svg>
      </div>
      <div className="relative z-10">
        <div className="text-base font-semibold">Marketplace</div>
        <p className="mt-0.5 text-xs text-secondary">{count.toLocaleString()} integrations to connect your stack.</p>
      </div>
    </div>
  );
}

export default function IntegrationsPage() {
  const [gh, setGh] = useState<GhDetail>({ configured: false, connected: false });
  const [ghOrgs, setGhOrgs] = useState<GhOrg[]>([]);

  // Refetch the enriched GitHub status + accessible orgs, bypassing the short
  // client cache (called after connect/disconnect/reconnect so the card is fresh).
  const refreshGh = useCallback(async () => {
    invalidate("/api/github/status");
    invalidate("/api/github/orgs");
    try {
      const s = await cachedJson<GhDetail>("/api/github/status", 30_000);
      setGh({ ...s, configured: !!s.configured, connected: !!s.connected });
      if (s.connected && s.hasOrgScope) {
        const o = await cachedJson<{ orgs: GhOrg[] }>("/api/github/orgs", 60_000);
        setGhOrgs(Array.isArray(o.orgs) ? o.orgs : []);
      } else {
        setGhOrgs([]);
      }
    } catch {
      /* leave last-known state */
    }
  }, []);

  // Marketplace catalog (loaded inline).
  const [toolkits, setToolkits] = useState<Toolkit[]>([]);
  const [tkConfigured, setTkConfigured] = useState(true);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [q, setQ] = useState("");
  const [page, setPage] = useState(0);
  const [connecting, setConnecting] = useState<string | null>(null);
  const [linkMsg, setLinkMsg] = useState<{ tone: "ok" | "warn"; text: string } | null>(null);

  // On return from a toolkit OAuth (`?connected=<slug>`), link the connection as a
  // consumable platform resource and auto-inject its env vars into deployments.
  useEffect(() => {
    const params = new URLSearchParams(window.location.search);
    const slug = params.get("connected");
    if (!slug) return;
    params.delete("connected");
    const qs = params.toString();
    window.history.replaceState({}, "", window.location.pathname + (qs ? `?${qs}` : ""));
    // GitHub is a Git integration, not a resource toolkit — it's managed by the
    // GitHub card (status/scopes/orgs), never credential-linked into deployments.
    // Just refresh the card so a completed OAuth reflects immediately.
    if (slug === "github") {
      setLinkMsg({ tone: "ok", text: "GitHub connected." });
      refreshGh();
      return;
    }
    setLinkMsg({ tone: "ok", text: `Linking ${slug} resources…` });
    (async () => {
      try {
        const r = await fetch("/api/integrations/link", {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ slug, team: currentTeam() }),
        });
        const d = await r.json();
        if (d.ok) {
          const n = Array.isArray(d.env) ? d.env.length : 0;
          setLinkMsg({
            tone: "ok",
            text: n
              ? `Linked ${slug} — ${n} env var${n === 1 ? "" : "s"} injected into your deployments (${d.injected} write${d.injected === 1 ? "" : "s"}). Consume them via the SDK with a hive_ key.`
              : `Connected ${slug}. No extractable credentials to inject.`,
          });
        } else {
          setLinkMsg({ tone: "warn", text: d.error || `Connected ${slug}, but linking resources failed.` });
        }
      } catch {
        setLinkMsg({ tone: "warn", text: `Connected ${slug}, but linking resources failed.` });
      }
    })();
  }, [refreshGh]);

  useEffect(() => {
    // Enriched GitHub status + accessible orgs (short cache; reflects connection state).
    refreshGh();
  }, [refreshGh]);

  useEffect(() => {
    let cancelled = false;
    // Catalog: cached for an hour client-side (and on the server route) so the
    // 1,000+ toolkit list isn't refetched every visit — but ONLY when the
    // answer is a real catalog. An unconfigured/empty response describes server
    // config, not data: cache it and the page keeps insisting the key is unset
    // for an hour after an operator actually sets it (exactly what happened on
    // 2026-08-08, while the route itself was already returning 1,088 toolkits).
    cachedJson<{ configured: boolean; toolkits: Toolkit[] }>(
      "/api/composio/toolkits",
      60 * 60_000,
      undefined,
      (d) => !!d.configured && Array.isArray(d.toolkits) && d.toolkits.length > 0,
    )
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

  // The managed hive-cloud data services (always active). GitHub is rendered by the
  // dedicated <GithubCard/> which carries full connection/scope/org management.
  const otherConnected = [
    { kind: "postgres", name: "OpenEdge Postgres", desc: "Serverless SQL database built for the edge", active: true },
    { kind: "kv", name: "OpenEdge KV", desc: "Durable Redis database for caching", active: true },
  ];

  return (
    <div className="pb-16">
      <PageHeader
        title="Integrations"
        desc="Connect your stack — deploy from Git and link 1,000+ tools via Composio."
      />

      {linkMsg && (
        <div
          className={`mb-6 rounded-md border px-3 py-2 text-sm ${
            linkMsg.tone === "ok"
              ? "border-green/30 bg-green/10 text-green"
              : "border-amber-500/30 bg-amber-500/10 text-amber-600 dark:text-amber-400"
          }`}
        >
          {linkMsg.text}
        </div>
      )}

      <h2 className="mb-3 text-base font-semibold">Connected</h2>
      <div className="mb-10 grid grid-cols-1 gap-4 md:grid-cols-3">
        <GithubCard gh={gh} orgs={ghOrgs} onRefresh={refreshGh} />
        {otherConnected.map((i) => (
          <Card key={i.name} className="p-5">
            <div className="mb-3 flex items-start justify-between">
              <div className="flex items-center gap-3">
                <Logo kind={i.kind} />
                <div>
                  <div className="font-semibold">{i.name}</div>
                  <div className="text-xs text-muted">hive-cloud</div>
                </div>
              </div>
              <Badge tone={i.active ? "green" : "default"}>
                {i.active ? <><Check className="h-3 w-3" /> Active</> : "Inactive"}
              </Badge>
            </div>
            <p className="mb-4 text-sm text-secondary">{i.desc}</p>
            <div className="flex items-center gap-3">
              <Button variant="outline">Configure</Button>
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
            {safePage === 0 && !q && <MarketplaceArtCard count={toolkits.length} />}
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

/**
 * The GitHub connection management card — the literal ask: view what's granted,
 * reconnect/adjust scopes, request org approval, disconnect+reconnect.
 *
 * Renders the HONEST connection state (ACTIVE *and* live), the granted scopes as
 * badges (private-repo access via `repo`, org enumeration via `read:org`), the
 * accessible organizations, and wires the previously-dead Disconnect / Configure
 * buttons to real actions. A dead-but-ACTIVE token surfaces a red "reconnect
 * needed" banner instead of a false green.
 */
function GithubCard({ gh, orgs, onRefresh }: { gh: GhDetail; orgs: GhOrg[]; onRefresh: () => void }) {
  const [busy, setBusy] = useState<null | "connect" | "reconnect" | "disconnect">(null);
  const scopes = gh.scopes ?? [];
  // A revoked-but-ACTIVE connection: Composio still lists an account (so scopes are
  // known) but the token no longer works against GitHub (live=false) → reconnect.
  const needsReconnect = gh.configured && !gh.connected && scopes.length > 0;

  async function connect(kind: "connect" | "reconnect") {
    setBusy(kind);
    try {
      // Reconnect = disconnect the stale/limited connection first, so re-consent binds
      // a fresh account with the current scopes (incl. read:org) rather than reusing
      // the old grant. Plain connect just runs OAuth.
      if (kind === "reconnect") await fetch("/api/github/disconnect", { method: "POST" }).catch(() => {});
      const r = await fetch("/api/github/connect", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ returnTo: "/integrations" }),
      });
      const d = await r.json();
      if (d.redirectUrl) { window.location.href = d.redirectUrl; return; }
      setBusy(null);
    } catch {
      setBusy(null);
    }
  }

  async function disconnect() {
    if (!window.confirm("Disconnect GitHub? This removes the authorization from OpenEdge. You can reconnect anytime.")) return;
    setBusy("disconnect");
    try {
      await fetch("/api/github/disconnect", { method: "POST" });
    } finally {
      setBusy(null);
      onRefresh();
    }
  }

  const active = gh.connected;
  const subtitle = !gh.configured
    ? "not configured"
    : active
    ? gh.login
      ? `@${gh.login}${gh.via === "installation" ? " · app installation" : ""}`
      : gh.via === "installation"
      ? "connected via app installation"
      : "connected"
    : needsReconnect
    ? "reconnect needed"
    : "not connected";

  return (
    <Card className="p-5">
      <div className="mb-3 flex items-start justify-between">
        <div className="flex items-center gap-3">
          <Logo kind="github" />
          <div>
            <div className="font-semibold">GitHub</div>
            <div className="text-xs text-muted">{subtitle}</div>
          </div>
        </div>
        <Badge tone={active ? "green" : needsReconnect ? "red" : "default"}>
          {active ? <><Check className="h-3 w-3" /> Active</> : needsReconnect ? "Reconnect" : "Inactive"}
        </Badge>
      </div>

      {/* Not configured: no OAuth path — point at the PAT / public-URL fallback. */}
      {!gh.configured ? (
        <>
          <p className="mb-4 text-sm text-secondary">
            GitHub OAuth needs <code className="rounded bg-subtle px-1 py-0.5 text-xs">COMPOSIO_API_KEY</code>. You
            can still deploy any public repo by URL, or use a personal access token on the Git page.
          </p>
          <div className="flex items-center gap-3">
            <Link href="/new"><Button variant="outline">Deploy by URL</Button></Link>
          </div>
        </>
      ) : (
        <>
          <p className="mb-3 text-sm text-secondary">Deploy projects automatically with Git integration.</p>

          {/* Reconnect-needed banner for a dead-but-ACTIVE token. */}
          {needsReconnect ? (
            <div className="mb-3 flex items-start gap-2 rounded-md border border-red-500/30 bg-red-500/10 px-3 py-2 text-xs text-red-600 dark:text-red-400">
              <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" />
              <span>Your GitHub authorization is no longer valid (the token was revoked or expired). Reconnect to restore access.</span>
            </div>
          ) : null}

          {/* Granted scopes → capability badges (only when live). */}
          {active ? (
            <div className="mb-3 flex flex-wrap gap-1.5">
              <Badge tone={gh.hasPrivateAccess ? "green" : "default"}>
                {gh.hasPrivateAccess ? <><Check className="h-3 w-3" /> Private access</> : "Public only"}
              </Badge>
              <Badge tone={gh.hasOrgScope ? "green" : "default"}>
                <Building2 className="h-3 w-3" /> {gh.hasOrgScope ? "Org access" : "No org access"}
              </Badge>
            </div>
          ) : null}

          {/* Accessible organizations (needs read:org). */}
          {active && gh.hasOrgScope && orgs.length > 0 ? (
            <div className="mb-3 text-xs text-secondary">
              <span className="text-muted">Organizations: </span>
              {orgs.slice(0, 6).map((o) => o.login).join(", ")}
              {orgs.length > 6 ? ` +${orgs.length - 6} more` : ""}
            </div>
          ) : null}

          {/* Missing org access → a NON-disruptive prompt (private content still works).
              github-app: org access = INSTALL the App on the org (no scopes involved).
              composio: org enumeration needs the read:org scope via reconnect. */}
          {active && !gh.hasOrgScope ? (
            <div className="mb-3 flex items-start gap-2 rounded-md border border-amber-500/30 bg-amber-500/10 px-3 py-2 text-xs text-amber-600 dark:text-amber-400">
              <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" />
              {gh.provider === "github-app" ? (
                <span>
                  To use an organization&apos;s repositories,{" "}
                  <a href={gh.installUrl || "https://github.com/settings/installations"} target="_blank" rel="noreferrer" className="font-medium underline underline-offset-2 hover:text-fg">
                    install the GitHub App on that organization
                  </a>
                  . Personal private repos already work.
                </span>
              ) : (
                <span>Listing your organizations needs the <code>read:org</code> scope. Private-repo access already works — reconnect only if you want to pick org repos.</span>
              )}
            </div>
          ) : null}

          {/* App provider: always offer the install/configure surface (adding more orgs). */}
          {active && gh.provider === "github-app" && gh.hasOrgScope ? (
            <div className="mb-3 text-xs text-secondary">
              <a href={gh.installUrl || "https://github.com/settings/installations"} target="_blank" rel="noreferrer" className="underline underline-offset-2 hover:text-fg">
                Install / configure the GitHub App on more organizations
              </a>
            </div>
          ) : null}

          <div className="flex flex-wrap items-center gap-3">
            {active ? (
              <>
                <Button variant="outline" onClick={() => window.dispatchEvent(new Event("hive-open-gitops"))}>
                  <GitBranch className="h-3.5 w-3.5" /> Set up GitOps
                </Button>
                <button
                  onClick={() => connect("reconnect")}
                  disabled={!!busy}
                  className="inline-flex items-center gap-1.5 text-sm text-secondary hover:text-fg disabled:opacity-50"
                >
                  {busy === "reconnect" ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <RefreshCw className="h-3.5 w-3.5" />}
                  Reconnect / adjust access
                </button>
                <button
                  onClick={disconnect}
                  disabled={!!busy}
                  className="inline-flex items-center gap-1.5 text-sm text-secondary hover:text-red-500 disabled:opacity-50"
                >
                  {busy === "disconnect" ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : null}
                  Disconnect
                </button>
              </>
            ) : (
              <>
                <Button onClick={() => connect(needsReconnect ? "reconnect" : "connect")} disabled={!!busy}>
                  {busy ? <Loader2 className="h-4 w-4 animate-spin" /> : <Github className="h-4 w-4" />}
                  {needsReconnect ? "Reconnect GitHub" : "Connect"}
                </Button>
                {needsReconnect ? (
                  <button
                    onClick={disconnect}
                    disabled={!!busy}
                    className="text-sm text-secondary hover:text-red-500 disabled:opacity-50"
                  >
                    Disconnect
                  </button>
                ) : null}
              </>
            )}
          </div>
        </>
      )}
    </Card>
  );
}

