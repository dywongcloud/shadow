"use client";

import Link from "next/link";
import { Radio, RadioTower, RotateCw, WifiOff, TriangleAlert } from "lucide-react";
import { useRunNode } from "@/lib/use-run-node";
import { lifecycleLabel } from "@/lib/run-node-status";

/** "Run a node" navbar control, immediately beside the notifications bell.
 *  Renders the live SharedWorker-owned status (stopped/starting/online/
 *  degraded/error) and links out to the full /run-node control page — this
 *  button itself never starts/stops the node (that requires the consent flow
 *  on the dedicated page). */
export function RunNodeControl() {
  const { status, supported } = useRunNode();

  if (!supported) return null; // no SharedWorker in this browser — nothing to show here

  const { icon, tone, label } = visual(status.lifecycle);

  return (
    <Link
      href="/run-node"
      aria-label={`Run a node — ${label}`}
      title={`Run a node — ${label}`}
      className="relative flex h-8 items-center gap-1.5 rounded-md px-2 text-secondary hover:bg-subtle hover:text-fg"
    >
      <span className={tone} aria-hidden="true">{icon}</span>
      <span className="hidden text-xs font-medium sm:inline">Run a node</span>
      {status.lifecycle === "online" && (
        <span className="absolute right-1 top-1.5 h-1.5 w-1.5 rounded-full bg-emerald-400" aria-hidden="true" />
      )}
      {(status.lifecycle === "degraded" || status.lifecycle === "error") && (
        <span className="absolute right-1 top-1.5 h-1.5 w-1.5 rounded-full bg-amber-500" aria-hidden="true" />
      )}
    </Link>
  );
}

function visual(state: ReturnType<typeof useRunNode>["status"]["lifecycle"]) {
  const label = lifecycleLabel(state);
  switch (state) {
    case "online":
      return { icon: <RadioTower className="h-4 w-4" />, tone: "text-emerald-500", label };
    case "starting":
      return { icon: <RotateCw className="h-4 w-4 animate-spin" />, tone: "text-blue-500", label };
    case "degraded":
    case "suspended":
      return { icon: <TriangleAlert className="h-4 w-4" />, tone: "text-amber-500", label };
    case "error":
      return { icon: <WifiOff className="h-4 w-4" />, tone: "text-red-500", label };
    default:
      return { icon: <Radio className="h-4 w-4" />, tone: "text-secondary", label };
  }
}
