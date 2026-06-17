import * as React from "react";
import { cn } from "@/lib/utils";

export function Card({ className, ...props }: React.HTMLAttributes<HTMLDivElement>) {
  return (
    <div
      className={cn("rounded-xl border border-border bg-card p-5 shadow-card", className)}
      {...props}
    />
  );
}

export function Stat({
  label,
  value,
  hint,
}: {
  label: string;
  value: React.ReactNode;
  hint?: React.ReactNode;
}) {
  return (
    <Card className="flex flex-col gap-1">
      <span className="text-xs font-medium uppercase tracking-wide text-muted">{label}</span>
      <span className="text-3xl font-semibold tabular-nums text-fg">{value}</span>
      {hint ? <span className="text-xs text-secondary">{hint}</span> : null}
    </Card>
  );
}

export function Badge({
  children,
  tone = "default",
  className,
}: {
  children: React.ReactNode;
  tone?: "default" | "green" | "red" | "blue" | "amber";
  className?: string;
}) {
  const tones: Record<string, string> = {
    default: "border-border text-secondary bg-subtle",
    green: "border-emerald-500/30 text-emerald-600 bg-emerald-500/10 dark:text-emerald-400",
    red: "border-red-500/30 text-red-600 bg-red-500/10 dark:text-red-400",
    blue: "border-blue-500/30 text-blue-600 bg-blue-500/10 dark:text-blue-400",
    amber: "border-amber-500/30 text-amber-600 bg-amber-500/10 dark:text-amber-400",
  };
  return (
    <span
      className={cn(
        "inline-flex items-center gap-1 rounded-full border px-2 py-0.5 text-xs font-medium",
        tones[tone],
        className
      )}
    >
      {children}
    </span>
  );
}

export function Button({
  className,
  variant = "default",
  ...props
}: React.ButtonHTMLAttributes<HTMLButtonElement> & {
  variant?: "default" | "outline" | "ghost" | "danger";
}) {
  const variants: Record<string, string> = {
    default: "bg-accent text-accent-fg hover:opacity-90",
    outline: "border border-border-strong text-fg hover:bg-subtle",
    ghost: "text-secondary hover:text-fg hover:bg-subtle",
    danger: "border border-red-500/40 text-red-600 hover:bg-red-500/10 dark:text-red-400",
  };
  return (
    <button
      className={cn(
        "inline-flex items-center justify-center gap-2 rounded-md px-3 py-1.5 text-sm font-medium transition-colors disabled:opacity-50",
        variants[variant],
        className
      )}
      {...props}
    />
  );
}

export function Input({ className, ...props }: React.InputHTMLAttributes<HTMLInputElement>) {
  return (
    <input
      className={cn(
        "w-full rounded-md border border-border bg-card px-3 py-2 text-sm text-fg placeholder:text-muted focus:border-border-strong focus:outline-none focus:ring-2 focus:ring-border",
        className
      )}
      {...props}
    />
  );
}

export function Switch({
  checked,
  onChange,
  label,
}: {
  checked: boolean;
  onChange: (v: boolean) => void;
  label?: string;
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={label}
      onClick={() => onChange(!checked)}
      className={cn(
        "relative inline-flex h-5 w-9 shrink-0 cursor-pointer items-center rounded-full transition-colors focus:outline-none focus-visible:ring-2 focus-visible:ring-border",
        checked ? "bg-fg" : "bg-border-strong"
      )}
    >
      <span
        className={cn(
          "inline-block h-4 w-4 transform rounded-full bg-bg shadow transition-transform",
          checked ? "translate-x-[18px]" : "translate-x-0.5"
        )}
      />
    </button>
  );
}

export function PageHeader({
  title,
  desc,
  action,
}: {
  title: string;
  desc?: string;
  action?: React.ReactNode;
}) {
  return (
    <div className="mb-6 flex flex-col gap-3 sm:flex-row sm:items-end sm:justify-between">
      <div>
        <h1 className="text-2xl font-semibold tracking-tight text-fg">{title}</h1>
        {desc ? <p className="mt-1 text-sm text-secondary">{desc}</p> : null}
      </div>
      {action}
    </div>
  );
}

export function Table({ children }: { children: React.ReactNode }) {
  return (
    <div className="overflow-x-auto rounded-xl border border-border bg-card">
      <table className="w-full text-sm">{children}</table>
    </div>
  );
}
export function Th({ children, className }: { children?: React.ReactNode; className?: string }) {
  return (
    <th
      className={cn(
        "whitespace-nowrap border-b border-border px-4 py-2.5 text-left text-xs font-medium uppercase tracking-wide text-muted",
        className
      )}
    >
      {children}
    </th>
  );
}
export function Td({ children, className }: { children?: React.ReactNode; className?: string }) {
  return <td className={cn("border-b border-border px-4 py-3 text-fg", className)}>{children}</td>;
}

/** Vercel-style settings card: title + body, with a footer (hint + action). */
export function SettingCard({
  title,
  desc,
  children,
  footer,
  footerAction,
}: {
  title: string;
  desc?: React.ReactNode;
  children?: React.ReactNode;
  footer?: React.ReactNode;
  footerAction?: React.ReactNode;
}) {
  return (
    <div className="overflow-hidden rounded-xl border border-border bg-card shadow-card">
      <div className="p-5 sm:p-6">
        <h3 className="text-lg font-semibold">{title}</h3>
        {desc ? <div className="mt-1.5 text-sm text-secondary">{desc}</div> : null}
        {children ? <div className="mt-5">{children}</div> : null}
      </div>
      {footer || footerAction ? (
        <div className="flex items-center justify-between gap-3 border-t border-border bg-subtle px-5 py-3 sm:px-6">
          <div className="text-xs text-secondary">{footer}</div>
          {footerAction}
        </div>
      ) : null}
    </div>
  );
}

/** Triangle avatar like Vercel project icons — inverts with the theme. */
export function Triangle({ className }: { className?: string }) {
  return (
    <span
      className={cn(
        "flex h-9 w-9 shrink-0 items-center justify-center rounded-full border border-border bg-fg text-bg",
        className
      )}
    >
      <svg width="13" height="11" viewBox="0 0 24 22" aria-hidden>
        <path d="M12 0 L24 22 L0 22 Z" fill="currentColor" />
      </svg>
    </span>
  );
}
