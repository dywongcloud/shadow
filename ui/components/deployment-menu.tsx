"use client";

import { useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { MoreHorizontal, RotateCcw, RefreshCw, Trash2, Loader2, XCircle } from "lucide-react";

/** The "⋯" menu at the end of a deployment row — Cancel Build / Instant Rollback
 *  / Redeploy / Delete. Mirrors ProjectMenu: the menu is portaled and
 *  fixed-positioned at the trigger so it overlays everything and is never
 *  clipped by the table's overflow.
 *
 *  `canCancel`/`onCancel` are OPTIONAL so every existing call site keeps
 *  compiling unchanged — a row that never passes them simply renders the menu
 *  it always did. Cancel is listed FIRST when present: it is only offered while
 *  a build is actually in flight (queued/building), where it is the only action
 *  that is time-sensitive, and where Rollback/Redeploy are meaningless because
 *  there is no finished artifact yet. */
export function DeploymentRowMenu({
  canRollback,
  canRedeploy,
  canCancel = false,
  busy,
  onRollback,
  onRedeploy,
  onCancel,
  onDelete,
}: {
  canRollback: boolean;
  canRedeploy: boolean;
  /** True only while the deployment is queued/building — see `onCancel`. */
  canCancel?: boolean;
  busy: boolean;
  onRollback: () => void;
  onRedeploy: () => void;
  /** Required in practice whenever `canCancel` is true; the item is not
   *  rendered without it, so a caller can never surface a dead Cancel. */
  onCancel?: () => void;
  /** Optional for the same reason as `onCancel`: an in-flight build has no
   *  deployment artifact to delete yet, so its menu omits Delete entirely
   *  rather than offering an action that cannot apply. */
  onDelete?: () => void;
}) {
  const [open, setOpen] = useState(false);
  const [pos, setPos] = useState<{ top: number; right: number } | null>(null);
  const btnRef = useRef<HTMLButtonElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);

  // Close on outside click (the portaled menu is not a DOM child of the trigger).
  useEffect(() => {
    const f = (e: MouseEvent) => {
      const t = e.target as Node;
      if (btnRef.current?.contains(t) || menuRef.current?.contains(t)) return;
      setOpen(false);
    };
    document.addEventListener("mousedown", f);
    return () => document.removeEventListener("mousedown", f);
  }, []);

  // Position the portaled menu under the trigger; keep it pinned on scroll/resize.
  useEffect(() => {
    if (!open) return;
    const place = () => {
      const r = btnRef.current?.getBoundingClientRect();
      if (r) setPos({ top: r.bottom + 6, right: Math.max(8, window.innerWidth - r.right) });
    };
    place();
    window.addEventListener("scroll", place, true);
    window.addEventListener("resize", place);
    return () => {
      window.removeEventListener("scroll", place, true);
      window.removeEventListener("resize", place);
    };
  }, [open]);

  function stop(e: React.MouseEvent) {
    e.preventDefault();
    e.stopPropagation();
  }
  function run(e: React.MouseEvent, fn: () => void) {
    stop(e);
    setOpen(false);
    fn();
  }

  const item = "flex w-full items-center gap-2 px-3 py-2 text-left text-sm hover:bg-subtle";

  const menu =
    open && pos && typeof document !== "undefined"
      ? createPortal(
          <div
            ref={menuRef}
            style={{ position: "fixed", top: pos.top, right: pos.right, zIndex: 70 }}
            className="w-52 overflow-hidden rounded-lg border border-border bg-card py-1 shadow-pop"
            onClick={stop}
          >
            {canCancel && onCancel && (
              <button onClick={(e) => run(e, onCancel)} className={`${item} text-amber-600 dark:text-amber-400`}>
                <XCircle className="h-3.5 w-3.5" /> Cancel Build
              </button>
            )}
            {canRollback && (
              <button onClick={(e) => run(e, onRollback)} className={item}>
                <RotateCcw className="h-3.5 w-3.5" /> Instant Rollback
              </button>
            )}
            {canRedeploy && (
              <button onClick={(e) => run(e, onRedeploy)} className={item}>
                <RefreshCw className="h-3.5 w-3.5" /> Redeploy
              </button>
            )}
            {onDelete && (
              <button onClick={(e) => run(e, onDelete)} className={`${item} text-red-600 dark:text-red-400`}>
                <Trash2 className="h-3.5 w-3.5" /> Delete Deployment
              </button>
            )}
          </div>,
          document.body
        )
      : null;

  return (
    <div className="flex items-center justify-end">
      <button
        ref={btnRef}
        aria-label="Deployment actions"
        disabled={busy}
        onClick={(e) => { stop(e); setOpen((o) => !o); }}
        className="flex h-7 w-7 items-center justify-center rounded-md text-secondary hover:bg-subtle hover:text-fg disabled:opacity-50"
      >
        {busy ? <Loader2 className="h-4 w-4 animate-spin" /> : <MoreHorizontal className="h-4 w-4" />}
      </button>
      {menu}
    </div>
  );
}
