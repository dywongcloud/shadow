"use client";

import { useEffect, useMemo, useRef, useState } from "react";
import { useRouter } from "next/navigation";
import {
  Search, ArrowRight, Plus, LayoutGrid, Boxes, Database, Activity, Globe2,
  ShieldHalf, Plug, Network, BarChart3, CreditCard, Settings, Users, User,
  BookOpen, Triangle, CornerDownLeft,
} from "lucide-react";
import { apiGet, type Deployment } from "@/lib/api";
import { useIsPlatformOwner } from "@/lib/owner";

/* The platform command bar (⌘K / Ctrl-K). Opens from the global shortcut or via
 * the `open-command-bar` window event (dispatched by the Projects search bar).
 * Navigates pages, runs quick actions, and jumps to a project. Theme-aware. */

interface Cmd {
  id: string;
  label: string;
  group: string;
  hint?: string;
  icon: React.ReactNode;
  keywords?: string;
  run: () => void;
}

const ic = "h-4 w-4";

export function CommandBar() {
  const router = useRouter();
  const isOwner = useIsPlatformOwner();
  const [open, setOpen] = useState(false);
  const [q, setQ] = useState("");
  const [idx, setIdx] = useState(0);
  const [deps, setDeps] = useState<Deployment[]>([]);
  const inputRef = useRef<HTMLInputElement>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const isMac = typeof navigator !== "undefined" && /mac/i.test(navigator.platform);

  // Open via ⌘K / Ctrl-K, the projects search bar event, and close via Esc.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        setOpen((o) => !o);
      } else if (e.key === "Escape") {
        setOpen(false);
      }
    };
    const onEvt = () => setOpen(true);
    window.addEventListener("keydown", onKey);
    window.addEventListener("open-command-bar", onEvt);
    return () => {
      window.removeEventListener("keydown", onKey);
      window.removeEventListener("open-command-bar", onEvt);
    };
  }, []);

  // On open: reset, focus, and load projects for jump-to.
  useEffect(() => {
    if (!open) return;
    setQ("");
    setIdx(0);
    setTimeout(() => inputRef.current?.focus(), 0);
    apiGet<Deployment[]>("/deployments").then(setDeps).catch(() => {});
  }, [open]);

  const go = (href: string) => () => {
    setOpen(false);
    router.push(href);
  };

  const commands = useMemo<Cmd[]>(() => {
    const nav: [string, string, React.ReactNode][] = [
      ["Projects", "/", <LayoutGrid key="p" className={ic} />],
      ["Workflows", "/workflows", <Activity key="w" className={ic} />],
      ["Storage", "/storage", <Database key="s" className={ic} />],
      ["Observability", "/observability", <Activity key="o" className={ic} />],
      ["CDN", "/cdn", <Globe2 key="c" className={ic} />],
      ["Integrations", "/integrations", <Plug key="i" className={ic} />],
      ["Domains", "/domains", <Globe2 key="d" className={ic} />],
      ["Firewall", "/firewall", <ShieldHalf key="f" className={ic} />],
      ["Constellation", "/network", <Network key="n" className={ic} />],
      ["Usage", "/usage", <BarChart3 key="u" className={ic} />],
      ["Billing", "/billing", <CreditCard key="b" className={ic} />],
      ["Settings", "/settings", <Settings key="se" className={ic} />],
      ["Teams", "/teams", <Users key="t" className={ic} />],
      ["Account", "/account", <User key="a" className={ic} />],
    ];
    const actions: Cmd[] = [
      { id: "new", label: "New Project", group: "Actions", hint: "Deploy", icon: <Plus className={ic} />, keywords: "create deploy import", run: go("/new") },
      { id: "docs", label: "Documentation", group: "Actions", icon: <BookOpen className={ic} />, keywords: "docs api guide help", run: go("/docs") },
      // Ops entry — platform owner only (middleware enforces; this just hides it).
      ...(isOwner
        ? [{ id: "ops", label: "Operations Console", group: "Actions", icon: <ShieldHalf className={ic} />, keywords: "admin ops", run: go("/admin") } as Cmd]
        : []),
    ];
    const pages: Cmd[] = nav.map(([label, href, icon]) => ({
      id: "nav-" + href,
      label,
      group: "Go to",
      icon,
      run: go(href),
    }));
    const projects = Array.from(new Set(deps.map((d) => d.project))).map((p) => ({
      id: "proj-" + p,
      label: p,
      group: "Projects",
      hint: "Open",
      icon: <Triangle className={ic} />,
      keywords: "project deployment " + p,
      run: go(`/projects/${encodeURIComponent(p)}`),
    }));
    return [...actions, ...pages, ...projects];
  }, [deps, isOwner]); // eslint-disable-line react-hooks/exhaustive-deps

  const filtered = useMemo(() => {
    const s = q.trim().toLowerCase();
    if (!s) return commands;
    return commands.filter((c) => (c.label + " " + c.group + " " + (c.keywords ?? "")).toLowerCase().includes(s));
  }, [q, commands]);

  useEffect(() => { setIdx(0); }, [q]);

  if (!open) return null;

  const run = (c?: Cmd) => c?.run();
  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "ArrowDown") { e.preventDefault(); setIdx((i) => Math.min(i + 1, filtered.length - 1)); }
    else if (e.key === "ArrowUp") { e.preventDefault(); setIdx((i) => Math.max(i - 1, 0)); }
    else if (e.key === "Enter") { e.preventDefault(); run(filtered[idx]); }
  };

  // Group while preserving order, tracking a flat index for highlight/scroll.
  const groups: { name: string; items: { c: Cmd; i: number }[] }[] = [];
  filtered.forEach((c, i) => {
    let g = groups.find((x) => x.name === c.group);
    if (!g) { g = { name: c.group, items: [] }; groups.push(g); }
    g.items.push({ c, i });
  });

  return (
    <div
      className="fixed inset-0 z-[200] flex items-start justify-center bg-black/50 p-4 pt-[12vh] backdrop-blur-sm"
      onMouseDown={() => setOpen(false)}
    >
      <div
        className="w-full max-w-xl overflow-hidden rounded-xl border border-border bg-card shadow-2xl"
        onMouseDown={(e) => e.stopPropagation()}
      >
        <div className="flex items-center gap-2.5 border-b border-border px-4">
          <Search className="h-4 w-4 shrink-0 text-muted" />
          <input
            ref={inputRef}
            value={q}
            onChange={(e) => setQ(e.target.value)}
            onKeyDown={onKeyDown}
            placeholder="Search projects, pages and actions…"
            className="w-full bg-transparent py-3.5 text-sm outline-none placeholder:text-muted"
          />
          <kbd className="hidden rounded border border-border bg-subtle px-1.5 py-0.5 font-mono text-[10px] text-muted sm:inline">esc</kbd>
        </div>

        <div ref={listRef} className="max-h-[55vh] overflow-y-auto py-2">
          {filtered.length === 0 && (
            <div className="px-4 py-10 text-center text-sm text-muted">No results for “{q}”.</div>
          )}
          {groups.map((g) => (
            <div key={g.name} className="px-2 pb-1">
              <div className="px-2 pb-1 pt-2 text-[11px] font-medium uppercase tracking-wide text-muted">{g.name}</div>
              {g.items.map(({ c, i }) => (
                <button
                  key={c.id}
                  onMouseEnter={() => setIdx(i)}
                  onClick={() => run(c)}
                  className={`flex w-full items-center gap-3 rounded-lg px-2.5 py-2 text-left text-sm ${
                    i === idx ? "bg-subtle text-fg" : "text-secondary hover:bg-subtle/60"
                  }`}
                >
                  <span className="text-muted">{c.icon}</span>
                  <span className="flex-1 truncate text-fg">{c.label}</span>
                  {c.hint && <span className="text-xs text-muted">{c.hint}</span>}
                  {i === idx && <CornerDownLeft className="h-3.5 w-3.5 text-muted" />}
                </button>
              ))}
            </div>
          ))}
        </div>

        <div className="flex items-center justify-between border-t border-border px-4 py-2 text-[11px] text-muted">
          <span className="flex items-center gap-1.5"><ArrowRight className="h-3 w-3" /> Navigate the platform</span>
          <span>{isMac ? "⌘" : "Ctrl"}K to toggle</span>
        </div>
      </div>
    </div>
  );
}
