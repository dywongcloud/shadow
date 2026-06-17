"use client";

import { useEffect, useRef, useState } from "react";
import { useRouter } from "next/navigation";
import { MoreHorizontal, ExternalLink, Settings, Trash2, Loader2 } from "lucide-react";
import { apiSend } from "@/lib/api";

/** The "⋯" menu on a project card — Visit / Settings / Delete Project. */
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
  const ref = useRef<HTMLDivElement>(null);
  const router = useRouter();

  useEffect(() => {
    const f = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", f);
    return () => document.removeEventListener("mousedown", f);
  }, []);

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

  return (
    <div className="relative" ref={ref}>
      <button
        aria-label="Project menu"
        onClick={(e) => { stop(e); setOpen((o) => !o); }}
        className="flex h-7 w-7 items-center justify-center rounded-md text-muted hover:bg-subtle hover:text-fg"
      >
        {busy ? <Loader2 className="h-4 w-4 animate-spin" /> : <MoreHorizontal className="h-4 w-4" />}
      </button>
      {open && (
        <div
          className="absolute right-0 top-full z-40 mt-1 w-48 overflow-hidden rounded-lg border border-border bg-card py-1 shadow-pop"
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
        </div>
      )}
    </div>
  );
}
