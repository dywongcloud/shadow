"use client";

import { useState, type FormEvent } from "react";
import { usePoll, getAiWebSearch, ApiError, type AiGatewayState, type AiWebSearchResponse, type AiWebSearchResult } from "@/lib/api";
import { Card, PageHeader, Badge, StateNotice, Stat } from "@/components/ui";
import { cn } from "@/lib/utils";

const ABSENT = "-";

/** Only a finite numeric API value is a measurement. A rejected poll, an older node, or a
 *  missing optional field stays absent ("-") instead of becoming an invented zero. */
function measured(v: number | null | undefined): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}

function shown(v: unknown): string {
  if (v === null || v === undefined || v === "") return ABSENT;
  if (typeof v === "number" && !Number.isFinite(v)) return ABSENT;
  return String(v);
}

function fmtMeasured(v: number | null | undefined): string {
  const n = measured(v);
  return n === null ? ABSENT : n.toLocaleString("en-US");
}

function perMillion(v: string | null | undefined): string {
  if (typeof v !== "string" || v.trim() === "") return ABSENT;
  const n = Number(v);
  if (!Number.isFinite(n)) return ABSENT;
  return `$${(n * 1000000).toFixed(2)}`;
}

function boolState(v: boolean | null | undefined, yes: string, no: string): string {
  if (v === true) return yes;
  if (v === false) return no;
  return ABSENT;
}

function cellClass(extra?: string): string {
  return cn("min-w-0 w-full max-w-full break-words", extra);
}

function rowClass(cols: string): string {
  return cn("grid min-h-[44px] grid-cols-1 gap-2 border-b border-border py-3 text-sm", cols);
}

export default function AiGatewayView() {
  const { data, error } = usePoll<AiGatewayState>("/v1/ai/state", 30000);
  const models = Array.isArray(data?.models) ? data.models : null;
  const pool = Array.isArray(data?.pool) ? data.pool : null;
  const generations = Array.isArray(data?.generations) ? data.generations : null;
  const residuals = Array.isArray(data?.residuals) ? data.residuals : null;
  const rateRows = Array.isArray(data?.rate_card?.ai_models) ? data.rate_card.ai_models : null;
  const rateFor = (id?: string | null) => {
    if (!rateRows || !id) return null;
    const r = rateRows.find((x) => x.id === id);
    return r ? {
      input: fmtMeasured(r.input_per_million_cents),
      output: fmtMeasured(r.output_per_million_cents),
    } : null;
  };
  const credits = data?.credits ?? null;
  const usage = data?.usage ?? null;
  const webSearch = data?.web_search ?? null;
  const webConfigured = boolState(webSearch?.configured, "Configured", "Not configured");
  const webPricePer1000 = shown(webSearch?.price_per_1000_usd);
  const webMaxResults = fmtMeasured(webSearch?.max_results);
  const webCalls = fmtMeasured(webSearch?.calls);
  const webRatePerCall = shown(data?.rate_card?.web_search_per_call_usd);

  const [webQuery, setWebQuery] = useState("");
  const [webResult, setWebResult] = useState<AiWebSearchResponse | null>(null);
  const [webError, setWebError] = useState<string | null>(null);
  const [webBusy, setWebBusy] = useState(false);
  const webData = webResult?.data ?? null;
  const webRows: AiWebSearchResult[] | null = Array.isArray(webData) ? webData : null;
  const webReason = (() => {
    const reason = webResult?.reason ?? webError;
    return typeof reason === "string" && reason.trim() !== "" ? reason : null;
  })();
  const webBilling = webResult?.billable === false
    ? "Not billed"
    : webResult?.billable === true
      ? shown(webResult?.price_per_1000_usd ?? data?.rate_card?.web_search_per_call_usd)
      : ABSENT;
  const webNotice = webError
    ? (webReason ?? "Web search is unavailable right now.")
    : webSearch?.configured === false
      ? (webReason ?? "Web search is not configured on this node.")
      : webResult && (!webRows || webRows.length === 0)
        ? (webReason ?? "No results were returned for this query.")
        : null;

  async function submitWebSearch(e: FormEvent<HTMLFormElement>) {
    e.preventDefault();
    if (webBusy) return;
    const q = webQuery.trim();
    if (!q) {
      setWebResult(null);
      setWebError("Enter a search query.");
      return;
    }
    setWebBusy(true);
    setWebError(null);
    setWebResult(null);
    try {
      const r = await getAiWebSearch(q, 5);
      setWebResult(r ?? null);
    } catch (err) {
      setWebError(err instanceof ApiError && err.message ? err.message : "Web search is unavailable right now.");
    } finally {
      setWebBusy(false);
    }
  }

  const modelsAvailable = (() => {
    if (!models) return ABSENT;
    const flags = models.map((m) => (m?.available === true ? 1 : m?.available === false ? 0 : null));
    if (!flags.some((f) => f !== null)) return ABSENT;
    return String(flags.reduce<number>((sum, f) => sum + (f ?? 0), 0));
  })();

  const requestValue = fmtMeasured(usage?.ai_requests);
  const inputTokens = measured(usage?.ai_input_tokens);
  const outputTokens = measured(usage?.ai_output_tokens);
  const totalTokens = inputTokens === null || outputTokens === null ? null : inputTokens + outputTokens;
  const tokenValue = totalTokens === null ? ABSENT : totalTokens.toLocaleString("en-US");
  const balance = shown(credits?.balance);
  const currency = shown(credits?.currency);
  const creditValue = balance === ABSENT ? ABSENT : currency === ABSENT ? balance : `${balance} ${currency}`;
  const charging = boolState(data?.charging_enabled, "Charging on", "Charging off");
  const nodeLocal = boolState(data?.node_local, "Node local", "Fleet view");
  const catalogAsOf = shown(data?.catalog_as_of);
  const node = shown(data?.node);
  const totalUsed = shown(credits?.total_used);
  const tokenPass = credits?.token_pass ?? null;
  const tokenPassText = tokenPass ? `${shown(tokenPass.remaining)} remaining, resets ${shown(tokenPass.resets_at)}` : ABSENT;

  const meteringRows = [
    { label: "Public micro-cents", value: fmtMeasured(usage?.ai_public_micro_cents) },
    { label: "COGS micro-cents", value: fmtMeasured(usage?.ai_cogs_micro_cents) },
    { label: "Web search calls", value: fmtMeasured(usage?.ai_web_search_calls) },
  ];

  return (
    <div className="min-w-0 w-full max-w-full">
      <PageHeader
        title="AI Gateway"
        desc="Server-reported model catalog, provider pool, metering and residuals. Missing fields are shown as -."
        action={
          <div className="flex min-w-0 w-full max-w-full flex-wrap items-center gap-2">
            <Badge tone={data?.charging_enabled === true ? "green" : data?.charging_enabled === false ? "default" : "amber"} className={cellClass()}>{charging}</Badge>
            <Badge tone="blue" className={cellClass()}>{node}</Badge>
          </div>
        }
      />

      {error ? (
        <div className="mb-4 min-w-0 w-full max-w-full">
          <StateNotice className={cellClass()}>Could not load AI Gateway state: {error}</StateNotice>
        </div>
      ) : null}

      <div className="mb-6 grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-4">
        <Stat label="Models available" value={modelsAvailable} hint={models ? `${models.length} catalog rows` : "Catalog not reported"} />
        <Stat label="Credits balance" value={creditValue} hint={totalUsed === ABSENT ? "Total used not reported" : `Total used ${totalUsed}`} />
        <Stat label="Requests" value={requestValue} hint="AI inference requests" />
        <Stat label="Tokens" value={tokenValue} hint="Input plus output tokens" />
      </div>

      <Card className="mb-4 min-w-0 w-full max-w-full">
        <h2 className="min-w-0 w-full max-w-full break-words text-lg font-semibold">Node state</h2>
        <div className="mt-3 grid grid-cols-1 gap-2 text-sm sm:grid-cols-2 lg:grid-cols-3">
          <div className={cellClass()}><span className={cellClass("block text-xs text-muted")}>Responding node</span><span className={cellClass()}>{node}</span></div>
          <div className={cellClass()}><span className={cellClass("block text-xs text-muted")}>Charging</span><span className={cellClass()}>{charging}</span></div>
          <div className={cellClass()}><span className={cellClass("block text-xs text-muted")}>Catalog as of</span><span className={cellClass()}>{catalogAsOf}</span></div>
          <div className={cellClass()}><span className={cellClass("block text-xs text-muted")}>View</span><span className={cellClass()}>{nodeLocal}</span></div>
          <div className={cellClass()}><span className={cellClass("block text-xs text-muted")}>Token pass</span><span className={cellClass()}>{tokenPassText}</span></div>
        </div>
        <div className="mt-4 grid grid-cols-1 gap-2 text-sm sm:grid-cols-3">
          {meteringRows.map((r) => (
            <div key={r.label} className={cellClass("rounded-lg border border-border px-3 py-2")}>
              <span className={cellClass("block text-xs text-muted")}>{r.label}</span>
              <span className={cellClass("tabular-nums")}>{r.value}</span>
            </div>
          ))}
        </div>
      </Card>

      <Card className="mb-4 min-w-0 w-full max-w-full">
        <h2 className="min-w-0 w-full max-w-full break-words text-lg font-semibold">Provider pool</h2>
        <p className="mt-1 min-w-0 w-full max-w-full break-words text-sm text-secondary">Configured and available states come from the node; a missing field stays -.</p>
        {!pool || pool.length === 0 ? (
          <div className="mt-4"><StateNotice tone="muted" className={cellClass()}>The provider pool is not reported by this node.</StateNotice></div>
        ) : (
          <div className="mt-4 min-w-0 w-full max-w-full">
            <div className="hidden grid-cols-2 gap-2 border-b border-border pb-2 text-xs font-medium uppercase tracking-wide text-muted sm:grid sm:grid-cols-3 lg:grid-cols-5">
              <span className={cellClass("text-xs font-medium uppercase tracking-wide text-muted")}>Name</span><span className={cellClass("text-xs font-medium uppercase tracking-wide text-muted")}>Protocol</span><span className={cellClass("text-xs font-medium uppercase tracking-wide text-muted")}>Configured</span><span className={cellClass("text-xs font-medium uppercase tracking-wide text-muted")}>Available</span><span className={cellClass("text-xs font-medium uppercase tracking-wide text-muted")}>Reason</span>
            </div>
            {pool.map((p, i) => (
              <div key={p?.name ?? `pool-${i}`} className={rowClass("sm:grid-cols-3 lg:grid-cols-5")}>
                <div className={cellClass()}><span className={cellClass("block text-xs text-muted sm:hidden")}>Name</span><span className={cellClass()}>{shown(p?.name)}</span></div>
                <div className={cellClass()}><span className={cellClass("block text-xs text-muted sm:hidden")}>Protocol</span><span className={cellClass()}>{shown(p?.protocol)}</span></div>
                <div className={cellClass()}><span className={cellClass("block text-xs text-muted sm:hidden")}>Configured</span><span className={cellClass()}>{boolState(p?.configured, "Configured", "Not configured")}</span></div>
                <div className={cellClass()}><span className={cellClass("block text-xs text-muted sm:hidden")}>Available</span><span className={cellClass()}>{boolState(p?.available, "Available", "Unavailable")}</span></div>
                <div className={cellClass()}><span className={cellClass("block text-xs text-muted sm:hidden")}>Reason</span><span className={cellClass()}>{shown(p?.reason)}</span></div>
              </div>
            ))}
          </div>
        )}
      </Card>

      <Card className="mb-4 min-w-0 w-full max-w-full">
        <h2 className="min-w-0 w-full max-w-full break-words text-lg font-semibold">Models</h2>
        <p className="mt-1 min-w-0 w-full max-w-full break-words text-sm text-secondary">Owned by is the real provider. Prices are per one million tokens and come only from the server wire.</p>
        {!models || models.length === 0 ? (
          <div className="mt-4"><StateNotice tone="muted" className={cellClass()}>The model catalog is not reported by this node.</StateNotice></div>
        ) : (
          <div className="mt-4 min-w-0 w-full max-w-full">
            <div className="hidden grid-cols-2 gap-2 border-b border-border pb-2 text-xs font-medium uppercase tracking-wide text-muted sm:grid sm:grid-cols-3 lg:grid-cols-6">
              <span className={cellClass("text-xs font-medium uppercase tracking-wide text-muted")}>ID</span><span className={cellClass("text-xs font-medium uppercase tracking-wide text-muted")}>Name</span><span className={cellClass("text-xs font-medium uppercase tracking-wide text-muted")}>Owned by</span><span className={cellClass("text-xs font-medium uppercase tracking-wide text-muted")}>Upstream model</span><span className={cellClass("text-xs font-medium uppercase tracking-wide text-muted")}>Status</span><span className={cellClass("text-xs font-medium uppercase tracking-wide text-muted")}>Input / output per 1M</span>
            </div>
            {models.map((m, i) => (
              <div key={m?.id ?? `model-${i}`} className={rowClass("sm:grid-cols-3 lg:grid-cols-6")}>
                <div className={cellClass()}><span className={cellClass("block text-xs text-muted sm:hidden")}>ID</span><span className={cellClass()}>{shown(m?.id)}</span></div>
                <div className={cellClass()}><span className={cellClass("block text-xs text-muted sm:hidden")}>Name</span><span className={cellClass()}>{shown(m?.name)}</span></div>
                <div className={cellClass()}><span className={cellClass("block text-xs text-muted sm:hidden")}>Owned by</span><span className={cellClass()}>{shown(m?.owned_by)}</span></div>
                <div className={cellClass()}><span className={cellClass("block text-xs text-muted sm:hidden")}>Upstream model</span><span className={cellClass()}>{shown(m?.upstream_model)}</span></div>
                <div className={cellClass()}><span className={cellClass("block text-xs text-muted sm:hidden")}>Status</span><span className={cellClass()}><Badge tone={m?.available === true ? "green" : m?.available === false ? "red" : "amber"} className={cellClass()}>{shown(m?.status)}</Badge></span></div>
                <div className={cellClass()}><span className={cellClass("block text-xs text-muted sm:hidden")}>Input / output per 1M</span><span className={cellClass("tabular-nums")}>{rateFor(m?.id)?.input ?? ABSENT} / {rateFor(m?.id)?.output ?? ABSENT}</span></div>
              </div>
            ))}
          </div>
        )}
      </Card>

      <Card className="mb-4 min-w-0 w-full max-w-full">
        <h2 className="min-w-0 w-full max-w-full break-words text-lg font-semibold">Generations</h2>
        {!generations || generations.length === 0 ? (
          <div className="mt-4"><StateNotice tone="muted" className={cellClass()}>No generation records are reported by this node.</StateNotice></div>
        ) : (
          <div className="mt-4 grid grid-cols-1 gap-3">
            {generations.map((g, i) => (
              <div key={g?.id ?? `generation-${i}`} className={cn("min-h-[44px] min-w-0 w-full max-w-full rounded-lg border border-border p-3 text-sm", "flex min-w-0 w-full max-w-full flex-col gap-2")}>
                <div className={cellClass()}><span className={cellClass("text-xs text-muted")}>ID </span><span className={cellClass()}>{shown(g?.id)}</span></div>
                <div className="grid grid-cols-1 gap-2 sm:grid-cols-2 lg:grid-cols-3">
                  <div className={cellClass()}><span className={cellClass("block text-xs text-muted")}>Created at</span><span className={cellClass()}>{shown(g?.created_at ?? g?.created_ms)}</span></div>
                  <div className={cellClass()}><span className={cellClass("block text-xs text-muted")}>Model</span><span className={cellClass()}>{shown(g?.model)}</span></div>
                  <div className={cellClass()}><span className={cellClass("block text-xs text-muted")}>Provider</span><span className={cellClass()}>{shown(g?.provider_name)}</span></div>
                  <div className={cellClass()}><span className={cellClass("block text-xs text-muted")}>Prompt tokens</span><span className={cellClass("tabular-nums")}>{fmtMeasured(g?.tokens_prompt)}</span></div>
                  <div className={cellClass()}><span className={cellClass("block text-xs text-muted")}>Completion tokens</span><span className={cellClass("tabular-nums")}>{fmtMeasured(g?.tokens_completion)}</span></div>
                  <div className={cellClass()}><span className={cellClass("block text-xs text-muted")}>Total cost (USD)</span><span className={cellClass("tabular-nums")}>{fmtMeasured(g?.total_cost)}</span></div>
                  <div className={cellClass()}><span className={cellClass("block text-xs text-muted")}>Finish reason</span><span className={cellClass()}>{shown(g?.finish_reason)}</span></div>
                  <div className={cellClass()}><span className={cellClass("block text-xs text-muted")}>Latency ms</span><span className={cellClass("tabular-nums")}>{fmtMeasured(g?.latency ?? g?.latency_ms)}</span></div>
                  <div className={cellClass()}><span className={cellClass("block text-xs text-muted")}>Error</span><span className={cellClass()}>{shown(g?.error)}</span></div>
                </div>
              </div>
            ))}
          </div>
        )}
      </Card>

      <Card className="min-w-0 w-full max-w-full">
        <h2 className="min-w-0 w-full max-w-full break-words text-lg font-semibold">Residuals</h2>
        {!residuals || residuals.length === 0 ? (
          <div className="mt-4"><StateNotice tone="muted" className={cellClass()}>No residuals are reported by this node.</StateNotice></div>
        ) : (
          <div className="mt-4 grid grid-cols-1 gap-2">
            {residuals.map((r, i) => (
              <p key={`residual-${i}`} className={cellClass("whitespace-pre-wrap text-sm")}>{r}</p>
            ))}
          </div>
        )}
      </Card>

      <Card className="mt-4 min-w-0 w-full max-w-full">
        <h2 className="min-w-0 w-full max-w-full break-words text-lg font-semibold">Web search</h2>
        <p className="mt-1 min-w-0 w-full max-w-full break-words text-sm text-secondary">
          Server-side search with measured billing. Missing configuration stays -.
        </p>
        <div className="mt-3 grid grid-cols-1 gap-2 text-sm sm:grid-cols-2 lg:grid-cols-4">
          <div className={cellClass()}><span className={cellClass("block text-xs text-muted")}>Configured</span><span className={cellClass()}>{webConfigured}</span></div>
          <div className={cellClass()}><span className={cellClass("block text-xs text-muted")}>Price per 1,000 searches</span><span className={cellClass("tabular-nums")}>{webPricePer1000}</span></div>
          <div className={cellClass()}><span className={cellClass("block text-xs text-muted")}>Max results</span><span className={cellClass("tabular-nums")}>{webMaxResults}</span></div>
          <div className={cellClass()}><span className={cellClass("block text-xs text-muted")}>Billable calls</span><span className={cellClass("tabular-nums")}>{webCalls}</span></div>
        </div>
        <div className={cellClass("mt-3 text-xs text-secondary")}>Rate card price per call: {webRatePerCall}</div>
        <form onSubmit={submitWebSearch} className="mt-4 flex min-w-0 w-full max-w-full flex-wrap items-center gap-2">
          <label htmlFor="ai-web-search-query" className="sr-only">Web search query</label>
          <input
            id="ai-web-search-query"
            type="text"
            value={webQuery}
            onChange={(e) => setWebQuery(e.target.value)}
            maxLength={512}
            required
            disabled={webBusy}
            placeholder="Search the web"
            className="min-h-[44px] min-w-0 w-full max-w-full flex-1 rounded-md border border-border bg-card px-3 py-2 text-sm"
          />
          <button
            type="submit"
            disabled={webBusy}
            className="min-h-[44px] min-w-0 max-w-full rounded-md border border-border px-4 py-2 text-sm"
          >
            {webBusy ? "Searching" : "Search"}
          </button>
        </form>
        {webNotice ? (
          <div className="mt-4"><StateNotice tone="muted" className={cellClass()}>{webNotice}</StateNotice></div>
        ) : null}
        {webResult ? (
          <div className={cellClass("mt-4 text-sm")}>
            <span className={cellClass("text-xs text-muted")}>Billing </span>
            <span className={cellClass("tabular-nums")}>{webBilling}</span>
          </div>
        ) : null}
        {webRows && webRows.length > 0 ? (
          <ul className="mt-4 grid min-w-0 w-full max-w-full grid-cols-1 gap-3">
            {webRows.map((r, i) => (
              <li key={String(r?.url ?? r?.title ?? "result") + "-" + i} className={cn("min-h-[44px] min-w-0 w-full max-w-full rounded-lg border border-border p-3 text-sm", "flex min-w-0 w-full max-w-full flex-col gap-2")}>
                <div className={cellClass()}><span className={cellClass("text-xs text-muted")}>Title </span><span className={cellClass()}>{shown(r?.title)}</span></div>
                <div className={cellClass()}><span className={cellClass("text-xs text-muted")}>URL </span><span className={cellClass("text-secondary")}>{shown(r?.url)}</span></div>
                <div className={cellClass()}><span className={cellClass("text-xs text-muted")}>Snippet </span><span className={cellClass("text-secondary")}>{shown(r?.snippet)}</span></div>
              </li>
            ))}
          </ul>
        ) : null}
      </Card>
    </div>
  );
}
