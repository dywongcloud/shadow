"use client";

import { useEffect, useRef, useState } from "react";
import { Bell, Settings2, Archive, AlertTriangle, XCircle, Info, BellRing } from "lucide-react";
import { apiSend, usePoll, type Notification, type NotificationFeed } from "@/lib/api";
import { timeAgo } from "@/lib/utils";

type Tab = "inbox" | "archive" | "comments";

/** Vercel-style notifications bell + slide-down inbox panel.
 *  Notifications are real, derived from deploy failures and traffic anomalies. */
export function NotificationBell() {
  const { data, refresh } = usePoll<NotificationFeed>("/v1/notifications", 8000);
  const [open, setOpen] = useState(false);
  const [tab, setTab] = useState<Tab>("inbox");
  const [pushDismissed, setPushDismissed] = useState(true);
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    setPushDismissed(typeof window !== "undefined" && localStorage.getItem("oe_push_dismissed") === "1");
  }, []);
  useEffect(() => {
    const f = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", f);
    return () => document.removeEventListener("mousedown", f);
  }, []);

  const items = data?.items ?? [];
  const inbox = items.filter((n) => !n.archived);
  const archived = items.filter((n) => n.archived);
  const unread = data?.unread ?? 0;

  async function openPanel() {
    const next = !open;
    setOpen(next);
    // Opening the inbox marks everything read (like Vercel).
    if (next && unread > 0) {
      await apiSend("POST", "/v1/notifications/read").catch(() => {});
      refresh();
    }
  }
  async function archive(id: string) {
    await apiSend("POST", `/v1/notifications/${encodeURIComponent(id)}/archive`).catch(() => {});
    refresh();
  }
  async function archiveAll() {
    await apiSend("POST", "/v1/notifications/archive-all").catch(() => {});
    refresh();
  }
  function enablePush() {
    if (typeof window !== "undefined" && "Notification" in window) {
      window.Notification.requestPermission().catch(() => {});
    }
    dismissPush();
  }
  function dismissPush() {
    setPushDismissed(true);
    if (typeof window !== "undefined") localStorage.setItem("oe_push_dismissed", "1");
  }

  const shown = tab === "inbox" ? inbox : tab === "archive" ? archived : [];

  return (
    <div className="relative" ref={ref}>
      <button
        onClick={openPanel}
        aria-label="Notifications"
        className="relative flex h-8 w-8 items-center justify-center rounded-md text-secondary hover:bg-subtle hover:text-fg"
      >
        <Bell className="h-4 w-4" />
        {unread > 0 && (
          <span className="absolute -right-0.5 -top-0.5 flex h-4 min-w-4 items-center justify-center rounded-full bg-[#0070f3] px-1 text-[10px] font-semibold text-white">
            {unread > 99 ? "99+" : unread}
          </span>
        )}
      </button>

      {open && (
        <div className="absolute right-0 top-full z-50 mt-2 flex max-h-[560px] w-[380px] flex-col overflow-hidden rounded-xl border border-border bg-card shadow-pop">
          {/* Tabs */}
          <div className="flex items-center gap-4 border-b border-border px-4">
            <TabBtn active={tab === "inbox"} onClick={() => setTab("inbox")}>
              Inbox
              <span className="ml-1.5 rounded-full bg-subtle px-1.5 py-0.5 text-[10px] font-semibold text-secondary">{inbox.length}</span>
            </TabBtn>
            <TabBtn active={tab === "archive"} onClick={() => setTab("archive")}>Archive</TabBtn>
            <TabBtn active={tab === "comments"} onClick={() => setTab("comments")}>Comments</TabBtn>
            <a
              href="/settings/notifications"
              className="ml-auto flex h-7 w-7 items-center justify-center rounded-md text-muted hover:bg-subtle hover:text-fg"
              aria-label="Notification settings"
            >
              <Settings2 className="h-4 w-4" />
            </a>
          </div>

          {/* List */}
          <div className="flex-1 overflow-y-auto">
            {shown.length === 0 ? (
              <div className="flex flex-col items-center justify-center gap-2 px-6 py-14 text-center">
                <Bell className="h-6 w-6 text-muted" />
                <div className="text-sm text-secondary">
                  {tab === "comments" ? "No comments yet." : tab === "archive" ? "No archived notifications." : "You're all caught up."}
                </div>
              </div>
            ) : (
              shown.map((n) => <NotificationRow key={n.id} n={n} onArchive={() => archive(n.id)} archivable={tab === "inbox"} />)
            )}
          </div>

          {/* Enable push banner */}
          {tab === "inbox" && !pushDismissed && (
            <div className="border-t border-border bg-subtle/60 p-3">
              <div className="mb-2 flex items-start gap-2 text-sm">
                <BellRing className="mt-0.5 h-4 w-4 shrink-0 text-[#0070f3]" />
                <span className="text-secondary">Enable push notifications to receive updates on desktop or mobile</span>
              </div>
              <div className="flex gap-2">
                <button onClick={dismissPush} className="flex-1 rounded-md border border-border-strong px-3 py-1.5 text-sm font-medium text-fg hover:bg-subtle">Dismiss</button>
                <button onClick={enablePush} className="flex-1 rounded-md bg-fg px-3 py-1.5 text-sm font-medium text-bg hover:opacity-90">Enable</button>
              </div>
            </div>
          )}

          {/* Archive all */}
          {tab === "inbox" && inbox.length > 0 && (
            <button onClick={archiveAll} className="border-t border-border py-2.5 text-center text-sm font-medium text-secondary hover:bg-subtle hover:text-fg">
              Archive All
            </button>
          )}
        </div>
      )}
    </div>
  );
}

function TabBtn({ active, onClick, children }: { active: boolean; onClick: () => void; children: React.ReactNode }) {
  return (
    <button
      onClick={onClick}
      className={`flex items-center border-b-2 py-3 text-sm transition-colors ${active ? "border-fg font-medium text-fg" : "border-transparent text-secondary hover:text-fg"}`}
    >
      {children}
    </button>
  );
}

function SeverityIcon({ severity }: { severity: Notification["severity"] }) {
  if (severity === "error")
    return (
      <span className="flex h-6 w-6 shrink-0 items-center justify-center rounded-md bg-red-500/10 text-red-500">
        <XCircle className="h-4 w-4" />
      </span>
    );
  if (severity === "warning")
    return (
      <span className="flex h-6 w-6 shrink-0 items-center justify-center rounded-md bg-amber-500/10 text-amber-500">
        <AlertTriangle className="h-4 w-4" />
      </span>
    );
  return (
    <span className="flex h-6 w-6 shrink-0 items-center justify-center rounded-md bg-blue-500/10 text-blue-500">
      <Info className="h-4 w-4" />
    </span>
  );
}

/** Render the message, bolding the project name and environment like Vercel. */
function richMessage(n: Notification) {
  const parts: React.ReactNode[] = [];
  let rest = n.message;
  const bolds = [n.project, n.environment].filter(Boolean);
  let key = 0;
  // Greedily bold known tokens wherever they appear.
  const tokenRe = new RegExp(`(${bolds.map((b) => b.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")).join("|")})`, "g");
  if (!bolds.length) return rest;
  rest.split(tokenRe).forEach((seg) => {
    if (bolds.includes(seg)) parts.push(<span key={key++} className="font-semibold text-fg">{seg}</span>);
    else if (seg) parts.push(<span key={key++}>{seg}</span>);
  });
  return parts;
}

function NotificationRow({ n, onArchive, archivable }: { n: Notification; onArchive: () => void; archivable: boolean }) {
  return (
    <div className="group flex items-start gap-3 border-b border-border px-4 py-3 last:border-0 hover:bg-subtle/50">
      <SeverityIcon severity={n.severity} />
      <div className="min-w-0 flex-1">
        <div className="text-sm leading-snug text-secondary">{richMessage(n)}</div>
        <div className="mt-1 flex items-center gap-2 text-xs text-muted">
          {!n.read && <span className="h-1.5 w-1.5 rounded-full bg-[#0070f3]" />}
          {timeAgo(n.ts_ms)} ago
        </div>
      </div>
      {archivable && (
        <button
          onClick={onArchive}
          aria-label="Archive"
          className="opacity-0 transition-opacity hover:text-fg group-hover:opacity-100 text-muted"
        >
          <Archive className="h-4 w-4" />
        </button>
      )}
    </div>
  );
}
