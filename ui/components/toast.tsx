"use client";

import { useEffect, useState } from "react";
import { createPortal } from "react-dom";
import { X } from "lucide-react";
import { ApiError, ROLE_REFUSED } from "@/lib/api";

export type ToastTone = "blue" | "default";

const DEFAULT_TOAST_MS = 6000;

interface ToastDetail {
  message: string;
  tone: ToastTone;
  /** Auto-dismiss after this many ms (0 = sticky). */
  duration: number;
}

/** Fire a toast from anywhere: it dispatches the `hive-toast` window event, so callers need no context or provider and toasts survive client navigation (mount `<Toaster/>` once in
 *  the persistent root layout). Defaults to the blue "info" tone shown bottom-right. */
export function toast(message: string, opts: { tone?: ToastTone; duration?: number } = {}) {
  if (typeof window === "undefined") return;
  const detail: ToastDetail = {
    message,
    tone: opts.tone ?? "blue",
    duration: opts.duration ?? DEFAULT_TOAST_MS,
  };
  window.dispatchEvent(new CustomEvent("hive-toast", { detail }));
}

/** How a failed action is shown where there is no inline slot for it: a role-matrix refusal is a toast with its sentence ("Your role can't do this. Ask a team admin."), anything else keeps the browser alert. */
export function reportError(e: unknown) {
  if (e instanceof ApiError && e.code === ROLE_REFUSED) toast(e.message, { tone: "default" });
  else alert(String(e));
}

interface ToastItem extends ToastDetail {
  id: number;
}

export function Toaster() {
  const [items, setItems] = useState<ToastItem[]>([]);

  useEffect(() => {
    let seq = 0;
    const onToast = (e: Event) => {
      const d = (e as CustomEvent<ToastDetail>).detail;
      const id = ++seq + Date.now();
      setItems((xs) => [...xs, { id, ...d }]);
      if (d.duration > 0) {
        window.setTimeout(() => setItems((xs) => xs.filter((x) => x.id !== id)), d.duration);
      }
    };
    window.addEventListener("hive-toast", onToast as EventListener);
    return () => window.removeEventListener("hive-toast", onToast as EventListener);
  }, []);

  const dismiss = (id: number) => setItems((xs) => xs.filter((x) => x.id !== id));

  if (typeof document === "undefined" || items.length === 0) return null;

  return createPortal(
    <div className="pointer-events-none fixed bottom-4 right-4 z-[300] flex w-[min(92vw,440px)] flex-col gap-2">
      {items.map((t) => (
        <div
          key={t.id}
          role="status"
          className={`pointer-events-auto flex items-center gap-4 rounded-xl px-5 py-4 shadow-pop animate-[toast-in_220ms_cubic-bezier(0.16,1,0.3,1)] ${
            t.tone === "blue" ? "bg-[#0070f3] text-white" : "bg-fg text-bg"
          }`}
        >
          <span className="flex-1 text-sm font-medium">{t.message}</span>
          <button
            onClick={() => dismiss(t.id)}
            aria-label="Dismiss"
            className="shrink-0 opacity-80 transition-opacity hover:opacity-100"
          >
            <X className="h-4 w-4" />
          </button>
        </div>
      ))}
    </div>,
    document.body
  );
}
