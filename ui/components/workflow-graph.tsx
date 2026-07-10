"use client";

import { useEffect, useMemo } from "react";
import {
  ReactFlow,
  Background, BackgroundVariant, Controls, Handle, Position, MiniMap, Panel, MarkerType,
  useNodesState, useEdgesState, type Node, type Edge,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import { useTheme } from "next-themes";
import {
  CheckCircle2, XCircle, Loader2, Circle, PlayCircle, StopCircle, Zap, Box, Bot,
  Wrench, GitBranch, Clock, Link2, type LucideIcon,
} from "lucide-react";
import type { WorkflowRun, StepRun, WorkflowDef } from "@/lib/api";
import { fmtDuration } from "@/components/workflows";

/* ------------------------------------------------------------------------- *
 * Workflow graph viewer — a React Flow canvas that renders a WDK workflow's
 * declared graph (the steps/primitives and their edges), modeled on Vercel's
 * `@vercel/workflow` web dashboard flow-graph, adapted to this app's theme
 * tokens, layout and navbar. Node kinds are colour-coded with icons; the layout
 * flows top→bottom; conditionals render as diamonds.
 * ------------------------------------------------------------------------- */

/** Per-node-kind visual metadata (colour + icon + label), mirroring the WDK set. */
const KINDS: Array<{ key: string; color: string; label: string; Icon: LucideIcon }> = [
  { key: "start", color: "#3b82f6", label: "Start", Icon: PlayCircle },
  { key: "end", color: "#3b82f6", label: "End", Icon: StopCircle },
  { key: "conditional", color: "#ef4444", label: "Conditional", Icon: GitBranch },
  { key: "sleep", color: "#f59e0b", label: "Sleep", Icon: Clock },
  { key: "webhook", color: "#06b6d4", label: "Webhook", Icon: Link2 },
  { key: "primitive", color: "#f97316", label: "Primitive", Icon: Box },
  { key: "agent", color: "#ec4899", label: "Agent", Icon: Bot },
  { key: "tool", color: "#a855f7", label: "Tool", Icon: Wrench },
  { key: "step", color: "#22c55e", label: "Step", Icon: Zap },
];

function kindMeta(kind: string) {
  const k = (kind || "").toLowerCase();
  return KINDS.find((m) => k.includes(m.key)) ?? KINDS[KINDS.length - 1];
}

/** `#rrggbb` → `rgba(r,g,b,a)` for theme-aware tints without extra deps. */
function tint(hex: string, a: number): string {
  const n = parseInt(hex.slice(1), 16);
  return `rgba(${(n >> 16) & 255}, ${(n >> 8) & 255}, ${n & 255}, ${a})`;
}

function useDark() {
  const { resolvedTheme } = useTheme();
  return resolvedTheme === "dark";
}

/** A standard workflow node: rounded card, colour-coded by kind, top/bottom handles. */
function FlowNode({ data, selected }: { data: { label: string; kind: string }; selected?: boolean }) {
  const dark = useDark();
  const m = kindMeta(data.kind);
  const Icon = m.Icon;
  return (
    <div
      className="rounded-lg border p-3 text-fg shadow-card"
      style={{
        width: 220,
        borderColor: m.color,
        background: tint(m.color, dark ? 0.18 : 0.1),
        boxShadow: selected ? `0 0 0 2px ${tint(m.color, 0.6)}` : undefined,
      }}
    >
      <Handle type="target" position={Position.Top} style={{ width: 7, height: 7, background: m.color, border: "none" }} />
      <div className="flex items-center gap-2">
        <span
          className="flex h-6 w-6 shrink-0 items-center justify-center rounded-md"
          style={{ background: tint(m.color, dark ? 0.28 : 0.16), color: m.color }}
        >
          <Icon className="h-3.5 w-3.5" />
        </span>
        <span className="truncate text-sm font-medium">{data.label}</span>
      </div>
      <div className="mt-1.5 text-[10px] font-semibold uppercase tracking-wide" style={{ color: m.color }}>
        {m.label}
      </div>
      <Handle type="source" position={Position.Bottom} style={{ width: 7, height: 7, background: m.color, border: "none" }} />
    </div>
  );
}

/** Conditional node rendered as a diamond (rotated square), branch handles L/R/bottom. */
function DiamondNode({ data, selected }: { data: { label: string; kind: string }; selected?: boolean }) {
  const dark = useDark();
  const m = kindMeta("conditional");
  return (
    <div className="relative flex items-center justify-center" style={{ width: 132, height: 132 }}>
      <Handle type="target" position={Position.Top} style={{ background: m.color, border: "none" }} />
      <div
        className="rotate-45 rounded-md border-2"
        style={{
          width: 84,
          height: 84,
          borderColor: m.color,
          background: tint(m.color, dark ? 0.18 : 0.1),
          boxShadow: selected ? `0 0 0 2px ${tint(m.color, 0.6)}` : undefined,
        }}
      />
      <div className="pointer-events-none absolute inset-0 flex flex-col items-center justify-center gap-1 px-4 text-center">
        <GitBranch className="h-4 w-4" style={{ color: m.color }} />
        <span className="line-clamp-2 text-[11px] font-medium text-fg">{data.label}</span>
      </div>
      <Handle id="bottom" type="source" position={Position.Bottom} style={{ background: m.color, border: "none" }} />
      <Handle id="left" type="source" position={Position.Left} style={{ background: m.color, border: "none" }} />
      <Handle id="right" type="source" position={Position.Right} style={{ background: m.color, border: "none" }} />
    </div>
  );
}

/** A run-trace step node: same card, colour-coded by execution status. */
function StepNode({ data }: { data: { step: StepRun; now: number } }) {
  const dark = useDark();
  const s = data.step;
  const color =
    s.status === "succeeded" ? "#22c55e" :
    s.status === "failed" ? "#ef4444" :
    s.status === "running" ? "#f59e0b" : "#9ca3af";
  const Icon =
    s.status === "succeeded" ? CheckCircle2 :
    s.status === "failed" ? XCircle :
    s.status === "running" ? Loader2 : Circle;
  const dur = (s.finished_ms ?? data.now) - s.started_ms;
  return (
    <div
      className="rounded-lg border p-3 text-fg shadow-card"
      style={{ width: 210, borderColor: color, background: tint(color, dark ? 0.16 : 0.09) }}
    >
      <Handle type="target" position={Position.Top} style={{ width: 7, height: 7, background: color, border: "none" }} />
      <div className="flex items-center gap-2">
        <Icon className={`h-4 w-4 ${s.status === "running" ? "animate-spin" : ""}`} style={{ color }} />
        <span className="truncate font-mono text-sm font-medium">{s.name}</span>
      </div>
      <div className="mt-1 flex items-center justify-between text-[11px] text-muted">
        <span className="capitalize">{s.status}</span>
        <span className="tabular-nums">{fmtDuration(dur)}</span>
      </div>
      <Handle type="source" position={Position.Bottom} style={{ width: 7, height: 7, background: color, border: "none" }} />
    </div>
  );
}

const flowNodeTypes = { flow: FlowNode, diamond: DiamondNode };
const stepNodeTypes = { step: StepNode };

/** Shared React Flow canvas chrome (background grid, controls, minimap), theme-aware. */
function Canvas({
  nodes, edges, onNodesChange, onEdgesChange, nodeTypes, legend, height = 480,
}: {
  nodes: Node[]; edges: Edge[]; onNodesChange: any; onEdgesChange: any;
  nodeTypes: any; legend?: React.ReactNode; height?: number;
}) {
  const dark = useDark();
  return (
    <div className="relative w-full overflow-hidden rounded-xl border border-border bg-bg" style={{ height }}>
      <ReactFlow
        nodes={nodes}
        edges={edges}
        onNodesChange={onNodesChange}
        onEdgesChange={onEdgesChange}
        nodeTypes={nodeTypes}
        fitView
        fitViewOptions={{ padding: 0.28 }}
        minZoom={0.1}
        maxZoom={2}
        defaultViewport={{ x: 0, y: 0, zoom: 1 }}
        proOptions={{ hideAttribution: true }}
      >
        <Background variant={BackgroundVariant.Dots} gap={22} size={1} color={dark ? "#1c1c1c" : "#e8e8e8"} />
        <Controls showInteractive={false} className="!border-border !bg-card !shadow-card [&_button]:!border-border [&_button]:!bg-card [&_button]:!text-fg" />
        <MiniMap
          pannable zoomable
          className="!border !border-border !bg-card"
          maskColor={dark ? "rgba(0,0,0,0.45)" : "rgba(255,255,255,0.55)"}
          nodeColor={(n) => kindMeta((n.data as any)?.kind ?? "").color}
        />
        {legend && <Panel position="top-left">{legend}</Panel>}
      </ReactFlow>
    </div>
  );
}

/* ------------------------------------------------------------------ *
 * WDK manifest graph viewer — the declared workflow graph.
 * ------------------------------------------------------------------ */

function kindOf(n: any): string {
  return n?.data?.nodeKind || n?.data?.kind || n?.type || "step";
}

/** Layered top→bottom layout (longest-path depth → row; siblings centred per row),
 *  honouring any explicit `position` carried in the manifest. */
function layoutWdk(graph: { nodes: any[]; edges: any[] }): { nodes: Node[]; edges: Edge[] } {
  const gnodes = graph?.nodes ?? [];
  const gedges = graph?.edges ?? [];
  const ids = gnodes.map((n) => n.id);
  const indeg: Record<string, number> = Object.fromEntries(ids.map((i) => [i, 0]));
  const adj: Record<string, string[]> = Object.fromEntries(ids.map((i) => [i, []]));
  for (const e of gedges) {
    if (adj[e.source]) adj[e.source].push(e.target);
    if (e.target in indeg) indeg[e.target]++;
  }
  const depth: Record<string, number> = Object.fromEntries(ids.map((i) => [i, 0]));
  const queue = ids.filter((i) => indeg[i] === 0);
  const seen = new Set<string>(queue);
  let head = 0;
  while (head < queue.length) {
    const u = queue[head++];
    for (const v of adj[u] ?? []) {
      depth[v] = Math.max(depth[v], depth[u] + 1);
      if (!seen.has(v)) { seen.add(v); queue.push(v); }
    }
  }
  // Group ids per depth row, then spread them horizontally, centred.
  const rows: Record<number, string[]> = {};
  for (const id of ids) (rows[depth[id]] ??= []).push(id);
  const H = 280, V = 168;
  const placed: Record<string, { x: number; y: number }> = {};
  for (const [d, rowIds] of Object.entries(rows)) {
    rowIds.forEach((id, i) => {
      placed[id] = { x: (i - (rowIds.length - 1) / 2) * H, y: Number(d) * V };
    });
  }
  const nodes: Node[] = gnodes.map((n) => {
    const kind = kindOf(n);
    return {
      id: n.id,
      type: kind.toLowerCase().includes("conditional") ? "diamond" : "flow",
      position: n.position ?? placed[n.id] ?? { x: 0, y: 0 },
      data: { label: n.data?.label || n.id, kind },
    };
  });
  const edges: Edge[] = gedges.map((e, i) => {
    const k = (e.data?.kind || e.type || "").toLowerCase();
    const dashed = k.includes("parallel") || k.includes("conditional") || k.includes("loop");
    return {
      id: e.id || `e${i}`,
      source: e.source,
      target: e.target,
      sourceHandle: e.sourceHandle,
      targetHandle: e.targetHandle,
      type: k.includes("parallel") || k.includes("conditional") ? "smoothstep" : "default",
      label: e.label || e.data?.label,
      labelBgPadding: e.label ? ([6, 3] as [number, number]) : undefined,
      labelBgBorderRadius: 4,
      markerEnd: { type: MarkerType.ArrowClosed, width: 14, height: 14, color: "#6b7280" },
      style: { stroke: "#6b7280", strokeWidth: 1.4, strokeDasharray: dashed ? "5 4" : undefined },
    };
  });
  return { nodes, edges };
}

function Legend() {
  // Only the kinds typically present; compact, theme-aware.
  const shown = ["start", "step", "primitive", "agent", "tool", "conditional"];
  return (
    <div className="flex flex-col gap-1.5 rounded-lg border border-border bg-card/90 p-2.5 text-[11px] shadow-card backdrop-blur">
      {shown.map((key) => {
        const m = KINDS.find((x) => x.key === key)!;
        return (
          <span key={key} className="flex items-center gap-1.5 text-secondary">
            <span className="inline-block h-2.5 w-2.5 rounded-[3px]" style={{ background: tint(m.color, 0.25), border: `1px solid ${m.color}` }} />
            {m.label}
          </span>
        );
      })}
    </div>
  );
}

export function WorkflowDefGraph({ def }: { def: WorkflowDef }) {
  const laid = useMemo(() => layoutWdk(def.graph ?? { nodes: [], edges: [] }), [def.id, def.graph]);
  const [nodes, setNodes, onNodesChange] = useNodesState<Node>(laid.nodes);
  const [edges, setEdges, onEdgesChange] = useEdgesState<Edge>(laid.edges);

  useEffect(() => {
    setNodes(laid.nodes);
    setEdges(laid.edges);
  }, [laid, setNodes, setEdges]);

  if (!def.graph || !def.graph.nodes?.length) {
    return (
      <div className="flex h-[360px] w-full items-center justify-center rounded-xl border border-border bg-bg text-sm text-secondary">
        No graph in this workflow&apos;s manifest.
      </div>
    );
  }
  return (
    <Canvas
      nodes={nodes}
      edges={edges}
      onNodesChange={onNodesChange}
      onEdgesChange={onEdgesChange}
      nodeTypes={flowNodeTypes}
      legend={<Legend />}
      height={520}
    />
  );
}

/* ------------------------------------------------------------------ *
 * Run execution viewer — the realized trace (steps + status) as a graph.
 * ------------------------------------------------------------------ */

export function WorkflowGraph({ run, now }: { run: WorkflowRun; now: number }) {
  const dark = useDark();
  const [nodes, setNodes, onNodesChange] = useNodesState<Node>([]);
  const [edges, setEdges, onEdgesChange] = useEdgesState<Edge>([]);

  const key = useMemo(
    () => JSON.stringify([run.id, run.steps.map((s) => [s.name, s.status])]),
    [run.id, run.steps]
  );

  useEffect(() => {
    const ns: Node[] = [
      {
        id: "root",
        type: "step",
        position: { x: 0, y: 0 },
        data: { step: { name: run.name || run.def_id, status: run.status, output: "", started_ms: run.started_ms, finished_ms: run.finished_ms } as StepRun, now },
      },
      ...run.steps.map((s, i) => ({
        id: `s-${i}`,
        type: "step" as const,
        position: { x: 0, y: (i + 1) * 132 },
        data: { step: s, now },
      })),
    ];
    const es: Edge[] = run.steps.map((_, i) => ({
      id: `e-${i}`,
      source: i === 0 ? "root" : `s-${i - 1}`,
      target: `s-${i}`,
      animated: run.steps[i].status === "running",
      markerEnd: { type: MarkerType.ArrowClosed, width: 14, height: 14, color: dark ? "#6b7280" : "#94a3b8" },
      style: { stroke: dark ? "#4b5563" : "#cbd5e1", strokeWidth: 2 },
    }));
    setNodes(ns);
    setEdges(es);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key, dark]);

  return (
    <Canvas
      nodes={nodes}
      edges={edges}
      onNodesChange={onNodesChange}
      onEdgesChange={onEdgesChange}
      nodeTypes={stepNodeTypes}
      height={460}
    />
  );
}
