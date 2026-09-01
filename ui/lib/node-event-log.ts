"use client";

import { useEffect, useRef, useState } from "react";
import type { NodeInfo } from "@/lib/api";

/**
 * A real, live-derived event log for the /regions map's "Log" panel — there's
 * no backend join/leave audit trail to read (no `/v1/events`), so this diffs
 * successive `/v1/nodes` poll snapshots and reports only genuinely-observed
 * transitions (first-seen id, healthy flipping either direction, or an id
 * dropping out of the list). Every line corresponds to a real state change
 * this client actually witnessed — never a fabricated placeholder entry.
 */

export interface NodeLogEntry {
  key: string;
  ts: number;
  message: string;
  tone: "join" | "leave" | "warn";
}

const MAX_ENTRIES = 50;

export function useNodeEventLog(nodes: NodeInfo[] | null | undefined): NodeLogEntry[] {
  const [log, setLog] = useState<NodeLogEntry[]>([]);
  const knownRef = useRef<Map<string, { name: string; healthy: boolean | undefined }>>(new Map());
  const seededRef = useRef(false);

  useEffect(() => {
    if (!nodes) return;
    const known = knownRef.current;
    const seenIds = new Set(nodes.map((n) => n.id));
    const events: NodeLogEntry[] = [];
    const now = Date.now();

    // First poll only seeds state — a full node list on mount is "already
    // there", not N simultaneous joins.
    if (!seededRef.current) {
      for (const n of nodes) known.set(n.id, { name: n.name, healthy: n.healthy });
      seededRef.current = true;
      return;
    }

    for (const n of nodes) {
      const prev = known.get(n.id);
      if (!prev) {
        events.push({ key: `${n.id}-join-${now}`, ts: now, message: `${n.name} joined the mesh`, tone: "join" });
      } else if (prev.healthy !== false && n.healthy === false) {
        events.push({ key: `${n.id}-down-${now}`, ts: now, message: `${n.name} went unreachable`, tone: "warn" });
      } else if (prev.healthy === false && n.healthy !== false) {
        events.push({ key: `${n.id}-up-${now}`, ts: now, message: `${n.name} is back online`, tone: "join" });
      }
      known.set(n.id, { name: n.name, healthy: n.healthy });
    }
    for (const [id, prev] of known) {
      if (!seenIds.has(id)) {
        events.push({ key: `${id}-leave-${now}`, ts: now, message: `${prev.name} left the mesh`, tone: "leave" });
        known.delete(id);
      }
    }

    if (events.length) {
      // Genuinely derived from diffing this render's `nodes` against the
      // ref-held previous snapshot — cannot be a lazy initializer (needs the
      // cross-render comparison), and only fires when a real transition was
      // observed. The documented "synchronize with an external system" case.
      // eslint-disable-next-line react-hooks/set-state-in-effect
      setLog((cur) => [...events.reverse(), ...cur].slice(0, MAX_ENTRIES));
    }
  }, [nodes]);

  return log;
}
