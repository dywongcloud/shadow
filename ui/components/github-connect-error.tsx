"use client";

import { useEffect, useState } from "react";
import { AlertTriangle, X } from "lucide-react";

/** The OAuth callback reports every failure as `?github_error=<sentence>` on the page the user
 *  started from. Before this, no page read it: on a phone a cancelled or expired authorization
 *  simply returned to a card saying "not connected", with no way to tell what happened.
 *  The parameter is one-shot material: strip it after reading so a reload never replays it. */
export function GithubConnectError() {
  const [message, setMessage] = useState<string | null>(null);
  useEffect(() => {
    const params = new URLSearchParams(window.location.search);
    const raw = params.get("github_error");
    if (!raw) return;
    params.delete("github_error");
    const qs = params.toString();
    window.history.replaceState({}, "", window.location.pathname + (qs ? `?${qs}` : "") + window.location.hash);
    // eslint-disable-next-line react-hooks/set-state-in-effect -- one-time OAuth-callback URL handling on mount, not a subscription
    setMessage(raw.slice(0, 300));
  }, []);
  if (!message) return null;
  return (
    <div className="mb-6 flex items-start gap-2 rounded-md border border-red-500/30 bg-red-500/10 px-3 py-2.5 text-sm text-red-600 dark:text-red-400">
      <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" />
      <span className="min-w-0 flex-1 break-words">GitHub connection failed — {message}</span>
      <button onClick={() => setMessage(null)} aria-label="Dismiss" className="shrink-0 text-secondary hover:text-fg">
        <X className="h-4 w-4" />
      </button>
    </div>
  );
}
