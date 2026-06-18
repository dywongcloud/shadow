"use client";

import { Suspense, useEffect, useState } from "react";
import { useRouter, useSearchParams } from "next/navigation";
import { Lock, Loader2 } from "lucide-react";
import { Button, Input } from "@/components/ui";
import { apiGet, apiSend } from "@/lib/api";

interface Checkout {
  id: string;
  tenant: string;
  kind: string;
  plan: string;
  amount_cents: number;
}

function usd(c: number) {
  return `$${(c / 100).toFixed(2)}`;
}

function CheckoutInner() {
  const router = useRouter();
  const session = useSearchParams().get("session") || "";
  const [co, setCo] = useState<Checkout | null>(null);
  const [paying, setPaying] = useState(false);
  const [err, setErr] = useState("");

  useEffect(() => {
    if (!session) return;
    apiGet<Checkout>(`/v1/billing/checkout/${encodeURIComponent(session)}`).then(setCo).catch(() => setErr("Checkout session not found."));
  }, [session]);

  async function pay() {
    setPaying(true);
    try {
      await apiSend("POST", "/v1/billing/confirm", { session });
      router.replace("/billing?success=1");
    } catch (e) {
      setErr(String(e));
      setPaying(false);
    }
  }

  const title = co?.kind === "credits" ? "Add compute credits" : `${co?.plan ? co.plan[0].toUpperCase() + co.plan.slice(1) : ""} plan`;

  return (
    <div className="fixed inset-0 z-50 grid place-items-center bg-bg px-4">
      <div className="w-full max-w-[420px] overflow-hidden rounded-2xl border border-border bg-card shadow-pop">
        <div className="border-b border-border px-6 py-4">
          <div className="flex items-center gap-2 text-sm text-muted">
            <Lock className="h-3.5 w-3.5" /> Mock secure checkout · OpenEdge
          </div>
        </div>
        {err ? (
          <div className="px-6 py-8 text-center text-sm text-red-500">{err}</div>
        ) : !co ? (
          <div className="flex items-center justify-center px-6 py-12 text-muted"><Loader2 className="h-5 w-5 animate-spin" /></div>
        ) : (
          <div className="px-6 py-6">
            <div className="mb-1 text-sm text-secondary">{title}</div>
            <div className="mb-6 text-3xl font-semibold tabular-nums">{usd(co.amount_cents)}</div>

            <label className="mb-1.5 block text-xs font-medium text-secondary">Card number</label>
            <Input defaultValue="4242 4242 4242 4242" className="mb-3 font-mono" />
            <div className="mb-5 grid grid-cols-2 gap-3">
              <div>
                <label className="mb-1.5 block text-xs font-medium text-secondary">Expiry</label>
                <Input defaultValue="12 / 34" className="font-mono" />
              </div>
              <div>
                <label className="mb-1.5 block text-xs font-medium text-secondary">CVC</label>
                <Input defaultValue="123" className="font-mono" />
              </div>
            </div>

            <Button onClick={pay} disabled={paying} className="w-full justify-center bg-fg py-2.5 text-bg">
              {paying ? <Loader2 className="h-4 w-4 animate-spin" /> : `Pay ${usd(co.amount_cents)}`}
            </Button>
            <button onClick={() => router.replace("/billing?canceled=1")} className="mt-3 w-full text-center text-xs text-muted hover:text-fg">
              Cancel
            </button>
            <p className="mt-4 text-center text-[11px] text-muted">
              Test mode — no real charge. Set <span className="font-mono">STRIPE_SECRET_KEY</span> for live checkout.
            </p>
          </div>
        )}
      </div>
    </div>
  );
}

export default function CheckoutPage() {
  return (
    <Suspense fallback={null}>
      <CheckoutInner />
    </Suspense>
  );
}
