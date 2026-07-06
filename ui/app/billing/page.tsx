"use client";

import { Suspense, useEffect, useState } from "react";
import { useRouter, useSearchParams } from "next/navigation";
import { Check, CreditCard, Coins, Zap, Loader2 } from "lucide-react";
import { Card, Button, PageHeader, Badge } from "@/components/ui";
import { apiSend, usePoll, type BillingInfo, type LedgerEntry, type Invoice } from "@/lib/api";
import { timeAgo } from "@/lib/utils";

function usd(cents: number) {
  return `$${(cents / 100).toFixed(2)}`;
}

export default function BillingPage() {
  return (
    <Suspense fallback={null}>
      <BillingInner />
    </Suspense>
  );
}

function BillingInner() {
  const router = useRouter();
  const params = useSearchParams();
  const { data, refresh } = usePoll<BillingInfo>("/v1/billing", 5000);
  const { data: ledger } = usePoll<LedgerEntry[]>("/v1/billing/ledger", 5000);
  const { data: invoices } = usePoll<Invoice[]>("/v1/billing/invoices", 10000);
  const [busy, setBusy] = useState("");
  const [msg, setMsg] = useState("");

  // Complete a returning checkout (?success=<session>) — applies the plan/credits.
  useEffect(() => {
    const success = params.get("success");
    if (success && success !== "1") {
      apiSend("POST", "/v1/billing/confirm", { session: success })
        .then(() => { setMsg("Payment successful — your plan is updated."); refresh(); router.replace("/billing"); })
        .catch(() => {});
    } else if (success === "1") {
      setMsg("Payment successful.");
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  async function checkout(kind: "plan" | "credits", body: Record<string, unknown>) {
    setBusy(kind + JSON.stringify(body));
    try {
      const r = await apiSend<{ url: string; mock: boolean }>("POST", "/v1/billing/checkout", { kind, ...body });
      window.location.href = r.url;
    } catch (e) {
      setMsg(String(e));
      setBusy("");
    }
  }

  const acc = data?.account;
  const plans = data?.plans ?? [];

  return (
    <div className="pb-16">
      <PageHeader
        title="Billing"
        desc="Manage your plan, compute credits and pay-as-you-go usage."
      />

      {msg && (
        <div className="mb-5 rounded-md border border-emerald-500/30 bg-emerald-500/10 px-3 py-2 text-sm text-emerald-600 dark:text-emerald-400">{msg}</div>
      )}

      {/* Current account */}
      {acc && (
        <Card className="mb-6 p-5">
          <div className="flex flex-wrap items-center justify-between gap-4">
            <div>
              <div className="flex items-center gap-2">
                <span className="text-lg font-semibold capitalize">{acc.plan} plan</span>
                <Badge tone={acc.plan === "pro" ? "blue" : "default"}>{acc.status}</Badge>
                {data?.stripe ? <Badge tone="green">Stripe</Badge> : <Badge tone="amber">Mock checkout</Badge>}
              </div>
              <div className="mt-1 text-sm text-secondary">
                Resets {timeAgo(acc.period_end_ms)} · tenant <span className="font-mono">{acc.tenant}</span>
              </div>
            </div>
            <div className="flex gap-8">
              <Meter label="Included used" value={usd(acc.used_cents)} sub={`of ${usd(acc.included_cents)}`} pct={acc.included_cents ? acc.used_cents / acc.included_cents : 0} />
              <div>
                <div className="text-xs text-muted">Credit balance</div>
                <div className={`text-2xl font-semibold tabular-nums ${acc.balance_cents < 0 ? "text-red-500" : ""}`}>{usd(acc.balance_cents)}</div>
              </div>
            </div>
          </div>
        </Card>
      )}

      {/* Plan limits / quota (business locking) */}
      {data?.limits && (
        <Card className="mb-6 p-5">
          <div className="mb-4 flex items-center justify-between">
            <span className="text-base font-semibold">Plan limits</span>
            {!data.limits.can_deploy && (
              <Badge tone="red">Deploys locked — add credits or upgrade</Badge>
            )}
          </div>
          <div className="grid grid-cols-2 gap-5 md:grid-cols-4">
            <Quota
              label="Projects"
              used={data.limits.projects_used}
              max={data.limits.max_projects}
            />
            <Quota
              label="Team members"
              used={data.limits.members_used}
              max={data.limits.max_members}
            />
            <Feature label="Max function runtime" value={`${data.limits.max_duration_secs}s`} on />
            <Feature label="Auto multi-region failover" value={data.limits.allows_failover ? "Enabled" : "Locked"} on={data.limits.allows_failover} />
            <Feature label="Team / Org SSO" value={data.limits.allows_sso ? "Enabled" : "Locked"} on={data.limits.allows_sso} />
          </div>
          {data.rate_card && (
            <div className="mt-5 border-t border-border pt-4 text-xs text-secondary">
              <span className="font-medium text-fg">Usage rate card:</span>{" "}
              ${(data.rate_card.active_cpu_hr_cents / 100).toFixed(3)}/active-CPU-hr ·{" "}
              ${(data.rate_card.mem_gb_hr_cents / 100).toFixed(4)}/GB-hr ·{" "}
              ${(data.rate_card.requests_per_million_cents / 100).toFixed(2)}/M requests ·{" "}
              ${(data.rate_card.waf_per_million_cents / 100).toFixed(2)}/M WAF-blocked
            </div>
          )}
        </Card>
      )}

      {/* Plans */}
      <div className="mb-3 text-base font-semibold">Plans</div>
      <div className="mb-8 grid grid-cols-1 gap-4 md:grid-cols-2">
        {plans.map((p) => {
          const current = acc?.plan === p.id;
          return (
            <Card key={p.id} className={`flex flex-col p-5 ${current ? "ring-2 ring-fg" : ""}`}>
              <div className="mb-1 flex items-center justify-between">
                <span className="text-lg font-semibold">{p.name}</span>
                {current && <Badge tone="blue">Current</Badge>}
              </div>
              <div className="mb-4 text-2xl font-semibold tabular-nums">
                {p.price_cents === 0 ? "Free" : usd(p.price_cents)}<span className="text-sm font-normal text-muted">{p.price_cents ? " / mo" : ""}</span>
              </div>
              <ul className="mb-5 flex flex-1 flex-col gap-2 text-sm">
                {p.features.map((f) => (
                  <li key={f} className="flex items-start gap-2 text-secondary">
                    <Check className="mt-0.5 h-4 w-4 shrink-0 text-emerald-500" /> {f}
                  </li>
                ))}
              </ul>
              {current ? (
                <Button variant="outline" disabled className="w-full justify-center">Current plan</Button>
              ) : (
                <Button
                  className="w-full justify-center"
                  disabled={!!busy}
                  onClick={() => checkout("plan", { plan: p.id })}
                >
                  {busy.startsWith("plan") ? <Loader2 className="h-4 w-4 animate-spin" /> : <Zap className="h-4 w-4" />}
                  {p.price_cents === 0 ? "Switch to Hobby" : `Upgrade to ${p.name}`}
                </Button>
              )}
            </Card>
          );
        })}
      </div>

      {/* Buy credits */}
      <div className="mb-3 text-base font-semibold">Compute credits</div>
      <Card className="mb-8 flex flex-wrap items-center gap-3 p-5">
        <Coins className="h-5 w-5 text-amber-500" />
        <span className="text-sm text-secondary">Top up pay-as-you-go credits:</span>
        {[1000, 2500, 5000].map((amt) => (
          <Button key={amt} variant="outline" disabled={!!busy} onClick={() => checkout("credits", { amount_cents: amt })}>
            <CreditCard className="h-4 w-4" /> Add {usd(amt)}
          </Button>
        ))}
      </Card>

      {/* Invoices */}
      <div className="mb-3 text-base font-semibold">Invoices</div>
      <div className="mb-8 overflow-hidden rounded-xl border border-border bg-card">
        <div className="grid grid-cols-[1fr_auto_auto_auto] border-b border-border px-4 py-2.5 text-xs font-medium uppercase tracking-wide text-muted">
          <span>Invoice</span><span>Period</span><span>Status</span><span>Total</span>
        </div>
        {(invoices ?? []).length === 0 ? (
          <div className="px-4 py-10 text-center text-sm text-secondary">No invoices yet.</div>
        ) : (
          (invoices ?? []).map((inv) => (
            <details key={inv.id} className="border-b border-border last:border-0">
              <summary className="grid cursor-pointer grid-cols-[1fr_auto_auto_auto] items-center gap-4 px-4 py-2.5 text-sm hover:bg-subtle/50">
                <span className="font-mono text-xs">{inv.number}</span>
                <span className="text-xs text-secondary">
                  {new Date(inv.period_start_ms).toLocaleDateString()} – {new Date(inv.period_end_ms).toLocaleDateString()}
                </span>
                <span>
                  <Badge tone={inv.status === "paid" ? "green" : inv.status === "draft" ? "default" : "amber"}>
                    {inv.status}
                  </Badge>
                </span>
                <span className="tabular-nums font-medium">{usd(inv.total_cents)}</span>
              </summary>
              <div className="border-t border-border bg-subtle/30 px-4 py-3">
                {inv.lines.map((l, i) => (
                  <div key={i} className="flex items-center justify-between py-1 text-sm">
                    <span className="text-secondary">{l.description}</span>
                    <span className="tabular-nums">{usd(l.amount_cents)}</span>
                  </div>
                ))}
                <div className="mt-2 flex items-center justify-between border-t border-border pt-2 text-sm font-semibold">
                  <span>Total</span>
                  <span className="tabular-nums">{usd(inv.total_cents)}</span>
                </div>
              </div>
            </details>
          ))
        )}
      </div>

      {/* Ledger */}
      <div className="mb-3 text-base font-semibold">Transaction history</div>
      <div className="overflow-hidden rounded-xl border border-border bg-card">
        <div className="grid grid-cols-[1fr_auto_auto_auto] border-b border-border px-4 py-2.5 text-xs font-medium uppercase tracking-wide text-muted">
          <span>Description</span><span>Type</span><span>Amount</span><span>When</span>
        </div>
        {(ledger ?? []).length === 0 ? (
          <div className="px-4 py-10 text-center text-sm text-secondary">No transactions yet.</div>
        ) : (
          (ledger ?? []).map((l) => (
            <div key={l.id} className="grid grid-cols-[1fr_auto_auto_auto] items-center gap-4 border-b border-border px-4 py-2.5 text-sm last:border-0">
              <span className="truncate">{l.note}</span>
              <span className="text-xs capitalize text-secondary">{l.kind.replace("_", " ")}</span>
              <span className={`tabular-nums ${l.amount_cents < 0 ? "text-red-500" : "text-emerald-600 dark:text-emerald-400"}`}>
                {l.amount_cents < 0 ? "" : "+"}{usd(l.amount_cents)}
              </span>
              <span className="text-muted">{timeAgo(l.ts_ms)} ago</span>
            </div>
          ))
        )}
      </div>
    </div>
  );
}

function Quota({ label, used, max }: { label: string; used: number; max: number }) {
  const unlimited = max === 0;
  const pct = unlimited ? 0 : Math.min(100, (used / max) * 100);
  const over = !unlimited && used >= max;
  return (
    <div>
      <div className="text-xs text-muted">{label}</div>
      <div className="text-lg font-semibold tabular-nums">
        {used}
        <span className="text-sm font-normal text-muted"> / {unlimited ? "∞" : max}</span>
      </div>
      <div className="mt-1 h-1.5 w-full overflow-hidden rounded-full bg-subtle">
        <div className={`h-full rounded-full ${over ? "bg-red-500" : "bg-fg"}`} style={{ width: `${pct}%` }} />
      </div>
    </div>
  );
}

function Feature({ label, value, on }: { label: string; value: string; on: boolean }) {
  return (
    <div>
      <div className="text-xs text-muted">{label}</div>
      <div className={`text-lg font-semibold ${on ? "" : "text-muted"}`}>{value}</div>
    </div>
  );
}

function Meter({ label, value, sub, pct }: { label: string; value: string; sub: string; pct: number }) {
  return (
    <div>
      <div className="text-xs text-muted">{label}</div>
      <div className="text-2xl font-semibold tabular-nums">{value}</div>
      <div className="text-xs text-muted">{sub}</div>
      <div className="mt-1 h-1.5 w-28 overflow-hidden rounded-full bg-subtle">
        <div className="h-full rounded-full bg-fg" style={{ width: `${Math.min(100, Math.max(0, pct * 100))}%` }} />
      </div>
    </div>
  );
}
