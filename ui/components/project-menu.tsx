"use client";

import { useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { useRouter } from "next/navigation";
import { MoreHorizontal, ExternalLink, Settings, Trash2, Loader2 } from "lucide-react";
import { apiSend } from "@/lib/api";

/** The "⋯" menu on a project card/row — Visit / Settings / Delete Project.
 *
 *  The menu is rendered in a PORTAL (fixed-positioned at the trigger) so it
 *  overlays everything and is never clipped by an ancestor's `overflow-hidden`
 *  (which is what made it appear inline inside the list row). */
export function ProjectMenu({
  project,
  alias,
  onChange,
}: {
  project: string;
  alias?: string;
  onChange?: () => void;
}) {
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [pos, setPos] = useState<{ top: number; right: number } | null>(null);
  const btnRef = useRef<HTMLButtonElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  const router = useRouter();

  // Close on outside click (accounting for the portaled menu, which is NOT a DOM
  // descendant of the trigger).
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

  async function del(e: React.MouseEvent) {
    stop(e);
    if (!confirm(`Delete "${project}" and ALL its deployments? This cannot be undone.`)) return;
    setBusy(true);
    try {
      await apiSend("DELETE", `/v1/projects/${encodeURIComponent(project)}`);
      setOpen(false);
      onChange?.();
      router.refresh();
    } catch (err) {
      alert(String(err));
    } finally {
      setBusy(false);
    }
  }

  const item = "flex w-full items-center gap-2 px-3 py-2 text-left text-sm hover:bg-subtle";

  const menu =
    open && pos && typeof document !== "undefined"
      ? createPortal(
          <div
            ref={menuRef}
            style={{ position: "fixed", top: pos.top, right: pos.right, zIndex: 70 }}
            className="w-48 overflow-hidden rounded-lg border border-border bg-card py-1 shadow-pop"
            onClick={stop}
          >
            {alias && (
              <a href={`http://${alias}:8787/`} target="_blank" rel="noreferrer" className={item}>
                <ExternalLink className="h-3.5 w-3.5" /> Visit
              </a>
            )}
            <button onClick={(e) => { stop(e); setOpen(false); router.push(`/projects/${encodeURIComponent(project)}/settings`); }} className={item}>
              <Settings className="h-3.5 w-3.5" /> Settings
            </button>
            <button onClick={del} className={`${item} text-red-600 dark:text-red-400`}>
              <Trash2 className="h-3.5 w-3.5" /> Delete Project
            </button>
          </div>,
          document.body
        )
      : null;

  return (
    <>
      <button
        ref={btnRef}
        aria-label="Project menu"
        onClick={(e) => { stop(e); setOpen((o) => !o); }}
        className="flex h-7 w-7 items-center justify-center rounded-md text-muted hover:bg-subtle hover:text-fg"
      >
        {busy ? <Loader2 className="h-4 w-4 animate-spin" /> : <MoreHorizontal className="h-4 w-4" />}
      </button>
      {menu}
    </>
  );
}
