"use client";

import { useEffect, useMemo } from "react";
import ReactFlow, {
  Background, BackgroundVariant, Controls, Handle, Position, MiniMap,
  useNodesState, useEdgesState, type Node, type Edge,
} from "reactflow";
import "reactflow/dist/style.css";
import { useTheme } from "next-themes";
import { CheckCircle2, XCircle, Loader2, Circle } from "lucide-react";
import type { WorkflowRun, StepRun } from "@/lib/api";
import { fmtDuration } from "@/components/workflows";

/**
 * The WDK output-manifest viewer: a deployed/compiled workflow ships an output
 * manifest describing its steps (nodes) + ordering (edges); the dashboard maps
 * that manifest onto a canvas. Here we build the manifest from the run's steps
 * (the trace IS the realized manifest) and render the DAG.
 */
function StepNode({ data }: { data: { step: StepRun; now: number } }) {
  const s = data.step;
  const tone =
    s.status === "succeeded" ? "border-emerald-500/50" :
    s.status === "failed" ? "border-red-500/50" :
    s.status === "running" ? "border-amber-500/50" : "border-border";
  const Icon =
    s.status === "succeeded" ? CheckCircle2 :
    s.status === "failed" ? XCircle :
    s.status === "running" ? Loader2 : Circle;
  const iconColor =
    s.status === "succeeded" ? "text-emerald-500" :
    s.status === "failed" ? "text-red-500" :
    s.status === "running" ? "text-amber-500 animate-spin" : "text-muted";
  const dur = (s.finished_ms ?? data.now) - s.started_ms;
  return (
    <div className={`w-[200px] rounded-xl border-2 ${tone} bg-card p-3 shadow-card`}>
      <Handle type="target" position={Position.Left} style={{ width: 7, height: 7, background: "#888" }} />
      <div className="flex items-center gap-2">
        <Icon className={`h-4 w-4 ${iconColor}`} />
        <span className="truncate font-mono text-sm font-medium">{s.name}</span>
      </div>
      <div className="mt-1 flex items-center justify-between text-[11px] text-muted">
        <span className="capitalize">{s.status}</span>
        <span className="tabular-nums">{fmtDuration(dur)}</span>
      </div>
      <Handle type="source" position={Position.Right} style={{ width: 7, height: 7, background: "#888" }} />
    </div>
  );
}

const nodeTypes = { step: StepNode };

export function WorkflowGraph({ run, now }: { run: WorkflowRun; now: number }) {
  const { resolvedTheme } = useTheme();
  const dark = resolvedTheme === "dark";
  const [nodes, setNodes, onNodesChange] = useNodesState([]);
  const [edges, setEdges, onEdgesChange] = useEdgesState([]);

  const key = useMemo(
    () => JSON.stringify([run.id, run.steps.map((s) => [s.name, s.status])]),
    [run.id, run.steps]
  );

  useEffect(() => {
    // Root node (the workflow itself) → step chain.
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
        position: { x: (i + 1) * 260, y: (i % 2) * 120 },
        data: { step: s, now },
      })),
    ];
    const es: Edge[] = run.steps.map((_, i) => ({
      id: `e-${i}`,
      source: i === 0 ? "root" : `s-${i - 1}`,
      target: `s-${i}`,
      animated: run.steps[i].status === "running",
      style: { stroke: dark ? "#444" : "#cbd5e1", strokeWidth: 2 },
    }));
    setNodes(ns);
    setEdges(es);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key, dark]);

  return (
    <div className="relative h-[460px] w-full overflow-hidden rounded-xl border border-border bg-bg">
      <ReactFlow
        nodes={nodes}
        edges={edges}
        onNodesChange={onNodesChange}
        onEdgesChange={onEdgesChange}
        nodeTypes={nodeTypes}
        fitView
        fitViewOptions={{ padding: 0.25 }}
        minZoom={0.3}
        maxZoom={1.6}
        proOptions={{ hideAttribution: true }}
      >
        <Background variant={BackgroundVariant.Dots} gap={22} size={1} color={dark ? "#151515" : "#eee"} />
        <Controls showInteractive={false} className="!border-border !bg-card" />
        <MiniMap pannable zoomable className="!border !border-border !bg-card" maskColor={dark ? "rgba(0,0,0,0.4)" : "rgba(255,255,255,0.5)"} />
      </ReactFlow>
    </div>
  );
}
