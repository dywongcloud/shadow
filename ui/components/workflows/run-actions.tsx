"use client";

// ---------------------------------------------------------------------------
// RunActions — the upstream @workflow/web console's run-operations control,
// ported (menu items, wording, tooltips, confirm dialogs, and the
// `hasPendingSleeps` gating all mirror packages/web/app/components/
// run-actions.tsx). Wired to hive's run-op endpoints via lib/api `runOp`.
//
//   • Replay Run            → recreateRun  (POST …/replay)   → new run, navigate
//   • Re-enqueue            → reenqueueRun (POST …/reenqueue)
//   • Cancel Active Sleeps  → wakeUpRun    (POST …/wakeup)   → {stoppedCount}
//   • Cancel                → cancelRun    (POST …/cancel)
// ---------------------------------------------------------------------------

import { useState } from "react";
import { useRouter } from "next/navigation";
import { RotateCw, Zap, AlarmClockOff, XCircle, MoreHorizontal, Loader2 } from "lucide-react";
import { runOp } from "@/lib/api";

type Busy = null | "replay" | "reenqueue" | "wakeup" | "cancel";

export interface RunActionsProps {
  runId: string;
  project: string;
  /** Current run status (upstream vocabulary or hive internal). */
  status: string;
  /** Whether the run has active (uncompleted) sleeps — gates "Cancel Active Sleeps". */
  hasPendingSleeps: boolean;
  /** Compact = the runs-table row 3-dots; full = the run-detail header (buttons + dialogs). */
  variant?: "menu" | "header";
  onChanged?: () => void;
}

function isActive(status: string): boolean {
  const s = (status || "").toLowerCase();
  return s === "pending" || s === "running";
}

export function RunActions({ runId, project, status, hasPendingSleeps, variant = "menu", onChanged }: RunActionsProps) {
  const router = useRouter();
  const [busy, setBusy] = useState<Busy>(null);
  const [open, setOpen] = useState(false);
  const [confirm, setConfirm] = useState<null | "cancel" | "replay">(null);
  const [toast, setToast] = useState<string | null>(null);
  const active = isActive(status);

  const flash = (m: string) => {
    setToast(m);
    setTimeout(() => setToast(null), 4000);
  };

  async function doReplay() {
    setBusy("replay");
    setConfirm(null);
    try {
      const r = await runOp("replay", runId, project);
      flash("New run started");
      onChanged?.();
      if (r.runId) router.push(`/workflows/runs/${encodeURIComponent(r.runId)}?project=${encodeURIComponent(project)}`);
    } catch (e) {
      flash(`Replay failed: ${String(e)}`);
    } finally {
      setBusy(null);
    }
  }
  async function doReenqueue() {
    setBusy("reenqueue");
    try {
      await runOp("reenqueue", runId, project);
      flash("Run re-enqueued");
      onChanged?.();
    } catch (e) {
      flash(`Re-enqueue failed: ${String(e)}`);
    } finally {
      setBusy(null);
    }
  }
  async function doWakeup() {
    setBusy("wakeup");
    try {
      const r = await runOp("wakeup", runId, project);
      flash(`Cancelled ${r.stoppedCount ?? 0} active sleep${r.stoppedCount === 1 ? "" : "s"}`);
      onChanged?.();
    } catch (e) {
      flash(`Cancel sleeps failed: ${String(e)}`);
    } finally {
      setBusy(null);
    }
  }
  async function doCancel() {
    setBusy("cancel");
    setConfirm(null);
    try {
      await runOp("cancel", runId, project);
      flash("Run cancelled");
      onChanged?.();
    } catch (e) {
      flash(`Cancel failed: ${String(e)}`);
    } finally {
      setBusy(null);
    }
  }

  const Item = ({
    icon,
    label,
    onClick,
    disabled,
    danger,
  }: {
    icon: React.ReactNode;
    label: string;
    onClick: () => void;
    disabled?: boolean;
    danger?: boolean;
  }) => (
    <button
      type="button"
      disabled={disabled}
      onMouseDown={(e) => {
        e.preventDefault();
        if (disabled) return;
        setOpen(false);
        onClick();
      }}
      className={`flex w-full items-center gap-2 px-3 py-1.5 text-left text-sm disabled:cursor-not-allowed disabled:opacity-40 ${danger ? "text-red-600 hover:bg-red-500/10 dark:text-red-400" : "text-fg hover:bg-subtle"}`}
    >
      {icon} {label}
    </button>
  );

  const menu = (
    <div className="absolute right-0 z-40 mt-1 w-56 overflow-hidden rounded-lg border border-border bg-card py-1 shadow-pop">
      <Item icon={<RotateCw className="h-4 w-4" />} label={busy === "replay" ? "Replaying…" : "Replay Run"} onClick={() => setConfirm("replay")} disabled={!!busy} />
      <Item icon={<Zap className="h-4 w-4" />} label={busy === "reenqueue" ? "Re-enqueuing…" : "Re-enqueue"} onClick={doReenqueue} disabled={!!busy} />
      <Item icon={<AlarmClockOff className="h-4 w-4" />} label={busy === "wakeup" ? "Cancelling sleeps…" : "Cancel Active Sleeps"} onClick={doWakeup} disabled={!!busy || !hasPendingSleeps} />
      <Item icon={<XCircle className="h-4 w-4" />} label={busy === "cancel" ? "Cancelling…" : "Cancel"} onClick={doCancel} disabled={!!busy || !active} danger />
    </div>
  );

  return (
    <>
      {variant === "header" ? (
        <div className="flex items-center gap-2">
          <button
            type="button"
            onClick={() => setConfirm("replay")}
            disabled={!!busy}
            className="flex items-center gap-1.5 rounded-md border border-border-strong px-2.5 py-1.5 text-sm text-fg hover:bg-subtle disabled:opacity-50"
          >
            {busy === "replay" ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <RotateCw className="h-3.5 w-3.5" />} Replay
          </button>
          <button
            type="button"
            onClick={() => setConfirm("cancel")}
            disabled={!!busy || !active}
            className="flex items-center gap-1.5 rounded-md border border-red-500/40 px-2.5 py-1.5 text-sm text-red-600 hover:bg-red-500/10 disabled:opacity-40 dark:text-red-400"
          >
            {busy === "cancel" ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <XCircle className="h-3.5 w-3.5" />} Cancel
          </button>
          <div className="relative">
            <button
              type="button"
              onClick={() => setOpen((o) => !o)}
              onBlur={() => setTimeout(() => setOpen(false), 160)}
              className="flex h-8 w-8 items-center justify-center rounded-md border border-border-strong text-fg hover:bg-subtle"
              title="More"
            >
              <MoreHorizontal className="h-4 w-4" />
            </button>
            {open && (
              <div className="absolute right-0 z-40 mt-1 w-56 overflow-hidden rounded-lg border border-border bg-card py-1 shadow-pop">
                <Item icon={<Zap className="h-4 w-4" />} label={busy === "reenqueue" ? "Re-enqueuing…" : "Re-enqueue"} onClick={doReenqueue} disabled={!!busy} />
                <Item icon={<AlarmClockOff className="h-4 w-4" />} label={busy === "wakeup" ? "Cancelling sleeps…" : "Cancel Active Sleeps"} onClick={doWakeup} disabled={!!busy || !hasPendingSleeps} />
              </div>
            )}
          </div>
        </div>
      ) : (
        <div className="relative">
          <button
            type="button"
            onClick={(e) => {
              e.preventDefault();
              e.stopPropagation();
              setOpen((o) => !o);
            }}
            onBlur={() => setTimeout(() => setOpen(false), 160)}
            className="flex h-7 w-7 items-center justify-center rounded-md text-muted hover:bg-subtle hover:text-fg"
            title="Run actions"
          >
            <MoreHorizontal className="h-4 w-4" />
          </button>
          {open && menu}
        </div>
      )}

      {/* Confirm dialogs — verbatim wording from the upstream run-detail-view. */}
      {confirm === "cancel" && (
        <ConfirmDialog
          title="Cancel Workflow Run?"
          body="This will stop the workflow execution immediately, and no further steps will be executed. Partial workflow execution may occur."
          cancelLabel="Keep Running"
          confirmLabel="Cancel Run"
          danger
          onCancel={() => setConfirm(null)}
          onConfirm={doCancel}
        />
      )}
      {confirm === "replay" && (
        <ConfirmDialog
          title="Replay Run?"
          body="This can potentially re-run code that is meant to only execute once. A new run will be created from this run's original input."
          cancelLabel="Cancel"
          confirmLabel="Replay Run"
          onCancel={() => setConfirm(null)}
          onConfirm={doReplay}
        />
      )}

      {toast && (
        <div className="fixed bottom-4 right-4 z-50 rounded-lg border border-border bg-card px-4 py-2.5 text-sm shadow-pop">{toast}</div>
      )}
    </>
  );
}

function ConfirmDialog({
  title,
  body,
  cancelLabel,
  confirmLabel,
  danger,
  onCancel,
  onConfirm,
}: {
  title: string;
  body: string;
  cancelLabel: string;
  confirmLabel: string;
  danger?: boolean;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4" onMouseDown={onCancel}>
      <div className="w-full max-w-md rounded-xl border border-border bg-card p-5 shadow-pop" onMouseDown={(e) => e.stopPropagation()}>
        <div className="mb-1.5 text-base font-semibold">{title}</div>
        <div className="mb-4 text-sm text-secondary">{body}</div>
        <div className="flex justify-end gap-2">
          <button type="button" onClick={onCancel} className="rounded-md px-3 py-1.5 text-sm text-secondary hover:bg-subtle hover:text-fg">
            {cancelLabel}
          </button>
          <button
            type="button"
            onClick={onConfirm}
            className={`rounded-md px-3 py-1.5 text-sm font-medium ${danger ? "bg-red-600 text-white hover:bg-red-700" : "bg-accent text-accent-fg hover:opacity-90"}`}
          >
            {confirmLabel}
          </button>
        </div>
      </div>
    </div>
  );
}
