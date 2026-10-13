"use client";

import { useState } from "react";
import { Copy, Check, Server } from "lucide-react";
import { Badge } from "@/components/ui";
import { type Deployment, type RawPortBinding } from "@/lib/api";
import { rawPortAddress } from "@/lib/deploy-url";

/** A raw TCP/UDP/gRPC deployment has no Host header to connect through: this renders its allocated public port as a ready-to-copy `host:port` address (the
 *  literal form every raw client accepts); a pending state (no alias yet) renders a disabled placeholder instead of a broken row. Shared by the
 *  deployment-detail page and the project Network settings. */
function RawPortRow({ binding, alias, regionCode }: { binding: RawPortBinding; alias?: string | null; regionCode?: string | null }) {
  const [copied, setCopied] = useState(false);
  const address = alias ? rawPortAddress(alias, binding.public_port, regionCode) : "";
  const label = binding.label || (binding.function !== "web" && binding.function !== "api" ? binding.function : "");
  function copy() {
    if (!address) return;
    navigator.clipboard.writeText(address);
    setCopied(true);
    setTimeout(() => setCopied(false), 1500);
  }
  return (
    <div className="flex items-center gap-1.5 text-sm">
      <Server className="h-3.5 w-3.5 shrink-0 text-muted" />
      {address ? (
        <button
          type="button"
          onClick={copy}
          title="Copy connection address"
          className="inline-flex items-center gap-1.5 font-mono text-xs text-link hover:underline"
        >
          {address}
          {copied ? <Check className="h-3 w-3 text-green" /> : <Copy className="h-3 w-3" />}
        </button>
      ) : (
        <span className="font-mono text-xs text-muted">pending…</span>
      )}
      <Badge>{binding.protocol}</Badge>
      {label && <span className="text-xs text-muted">{label}</span>}
    </div>
  );
}

/** The "Connection" block for one deployment's raw port bindings: a copy-to-clipboard address per binding, formatted per protocol; it renders nothing for an
 *  HTTP-only deployment (no `raw_ports`), so it is safe to drop in unconditionally. */
export function RawPortConnections({ deployment, alias }: { deployment: Deployment | null | undefined; alias?: string | null }) {
  const bindings = deployment?.raw_ports;
  if (!bindings || bindings.length === 0) return null;
  const effectiveAlias = alias ?? deployment?.alias;
  return (
    <div className="flex flex-col gap-1.5">
      {bindings.map((b) => (
        <RawPortRow key={`${b.function}-${b.container_port}-${b.protocol}`} binding={b} alias={effectiveAlias} regionCode={deployment?.region_code} />
      ))}
    </div>
  );
}

/** Compact treatment for tight space (project tiles, list rows): a terse `protocol:port` for one raw binding, or `N raw ports` for several; the full
 *  copy-to-clipboard list stays in `RawPortConnections`. Renders nothing for an HTTP-only deployment. */
export function RawPortsBadge({ deployment }: { deployment: Deployment | null | undefined }) {
  const bindings = deployment?.raw_ports;
  if (!bindings || bindings.length === 0) return null;
  const label = bindings.length === 1 ? `${bindings[0].protocol}:${bindings[0].public_port}` : `${bindings.length} raw ports`;
  return (
    <span
      className="inline-flex shrink-0 items-center gap-1 rounded-md bg-subtle px-1.5 py-0.5 font-mono text-xs text-secondary"
      title="Raw TCP/UDP connection exposed"
    >
      <Server className="h-3 w-3" />
      {label}
    </span>
  );
}
