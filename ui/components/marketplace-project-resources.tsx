"use client";

import { useCallback, useEffect, useMemo, useState } from "react";
import Link from "next/link";
import { Loader2, RefreshCw, Server, HardDrive } from "lucide-react";
import { Button, Card, Badge } from "@/components/ui";
import {
  attachMarketplaceOrderToProject,
  fetchMarketplaceProjectResources,
  MarketplaceBrowserError,
} from "@/lib/marketplace-browser";
import {
  marketplaceListingsUrl,
  projectMarketplaceReturnUrl,
  type MarketplaceResource,
  type MarketplaceResourceType,
} from "@/lib/marketplace";

type LoadState = "loading" | "ready" | "unauthenticated" | "unavailable" | "error";

function capacityLabel(capacity: MarketplaceResource["capacity"]): string {
  const entries = Object.entries(capacity);
  return entries.length ? entries.map(([key, value]) => `${key.replaceAll("_", " ")}: ${value}`).join(" · ") : "Specification pending";
}

export function MarketplaceProjectResources({ project }: { project: string }) {
  const [resources, setResources] = useState<MarketplaceResource[]>([]);
  const [state, setState] = useState<LoadState>("loading");
  const [message, setMessage] = useState("");
  const [attachOrder, setAttachOrder] = useState<string | null>(null);
  const returnUrl = useMemo(
    () => projectMarketplaceReturnUrl(project, typeof window === "undefined" ? "https://autheo.dev" : window.location.origin),
    [project],
  );

  const load = useCallback(async () => {
    setState("loading");
    try {
      const result = await fetchMarketplaceProjectResources(project);
      setResources(result.resources);
      setState("ready");
    } catch (error) {
      if (error instanceof MarketplaceBrowserError) {
        setState(error.status === 401 ? "unauthenticated" : error.status === 404 || error.status === 503 ? "unavailable" : "error");
        setMessage(error.code);
      } else {
        setState("error");
        setMessage("REQUEST_FAILED");
      }
    }
  }, [project]);

  useEffect(() => { queueMicrotask(() => { void load(); }); }, [load]);

  useEffect(() => {
    const query = new URLSearchParams(window.location.search);
    const orderId = query.get("marketplace_order");
    if (!orderId) return;
    query.delete("marketplace_order");
    window.history.replaceState({}, "", `${window.location.pathname}${query.size ? `?${query}` : ""}`);
    void Promise.resolve().then(() => {
      setAttachOrder(orderId);
      return attachMarketplaceOrderToProject(project, orderId);
    })
      .then(({ resource }) => {
        setResources((current) => [resource, ...current.filter((item) => item.order_id !== resource.order_id)]);
        setState("ready");
      })
      .catch((error) => {
        if (error instanceof MarketplaceBrowserError) {
          setState(error.status === 401 ? "unauthenticated" : error.status === 404 || error.status === 503 ? "unavailable" : "error");
          setMessage(error.code);
        } else {
          setState("error");
          setMessage("REQUEST_FAILED");
        }
      })
      .finally(() => setAttachOrder(null));
  }, [project]);

  const browse = (type: MarketplaceResourceType) => marketplaceListingsUrl(project, type, returnUrl);
  return (
    <Card className="mb-6 p-5">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h2 className="font-medium">Marketplace resources</h2>
          <p className="mt-1 text-sm text-secondary">Add Marketplace compute or storage to this project.</p>
        </div>
        <div className="flex gap-2">
          <a href={browse("compute")} target="_blank" rel="noreferrer">
            <Button variant="outline"><Server className="h-4 w-4" /> Browse compute</Button>
          </a>
          <a href={browse("storage")} target="_blank" rel="noreferrer">
            <Button variant="outline"><HardDrive className="h-4 w-4" /> Browse storage</Button>
          </a>
        </div>
      </div>

      {attachOrder && <div className="mt-4 flex items-center gap-2 text-sm text-secondary"><Loader2 className="h-4 w-4 animate-spin" /> Applying Marketplace order {attachOrder}…</div>}
      {state === "loading" && !attachOrder && <div className="mt-4 flex items-center gap-2 text-sm text-secondary"><Loader2 className="h-4 w-4 animate-spin" /> Loading Marketplace resources…</div>}
      {state === "unauthenticated" && (
        <div className="mt-4 rounded-md border border-amber-500/30 bg-amber-500/10 p-3 text-sm text-secondary">
          Sign in to view and apply Marketplace resources. <Link href="/sign-in" className="font-medium text-link hover:underline">Sign in</Link>
        </div>
      )}
      {state === "unavailable" && <Status message="Marketplace resources are currently unavailable. Your existing project configuration is unchanged." onRetry={load} />}
      {state === "error" && <Status message={`Could not load Marketplace resources (${message || "REQUEST_FAILED"}).`} onRetry={load} />}
      {state === "ready" && !resources.length && (
        <div className="mt-4 rounded-md border border-dashed border-border p-4 text-sm text-secondary">
          No Marketplace resources are applied to this project yet.
        </div>
      )}
      {state === "ready" && resources.length > 0 && (
        <div className="mt-4 divide-y divide-border rounded-md border border-border">
          {resources.map((resource) => (
            <div key={resource.order_id} className="flex flex-wrap items-center justify-between gap-3 p-3 text-sm">
              <div>
                <div className="font-medium">{resource.listing_name}</div>
                <div className="mt-0.5 text-xs text-secondary">{resource.provider_name} · Order {resource.order_id}</div>
              </div>
              <div className="flex flex-wrap items-center gap-2 text-xs">
                <Badge tone={resource.resource_type === "compute" ? "green" : "blue"}>{resource.resource_type}</Badge>
                <Badge>{resource.status}</Badge>
                <span className="max-w-[260px] truncate text-secondary" title={capacityLabel(resource.capacity)}>{capacityLabel(resource.capacity)}</span>
              </div>
            </div>
          ))}
        </div>
      )}
    </Card>
  );
}

function Status({ message, onRetry }: { message: string; onRetry: () => void }) {
  return <div className="mt-4 flex flex-wrap items-center gap-3 rounded-md border border-red-500/30 bg-red-500/10 p-3 text-sm text-secondary"><span>{message}</span><Button variant="outline" onClick={onRetry}><RefreshCw className="h-3.5 w-3.5" /> Retry</Button></div>;
}
