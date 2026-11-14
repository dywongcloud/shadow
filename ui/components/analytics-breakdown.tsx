"use client";

import { useId, useState } from "react";
import { Button, Card } from "@/components/ui";
import {
  ANALYTICS_CARD_CLASS,
  ANALYTICS_PILL_CLASS,
  ANALYTICS_TAB_CLASS,
  ANALYTICS_TEXT,
  ANALYTICS_TOKENS,
} from "@/components/analytics-theme";

export interface BreakdownRow {
  id: string;
  label: string;
  count: number;
  color: string;
  lit: string;
}

export interface BreakdownTab {
  id: string;
  label: string;
  rows: BreakdownRow[];
  empty: string;
  caption?: string;
}

const COUNT = new Intl.NumberFormat("en-US", { notation: "compact", maximumFractionDigits: 1 });

function finite(value: number | null | undefined): number {
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}

function fmtCount(value: number): string {
  return COUNT.format(finite(value));
}

export function AnalyticsBreakdown({
  title,
  tabs,
  labelHeading,
  countLabel = "Requests",
}: {
  title: string;
  tabs: BreakdownTab[];
  labelHeading: string;
  countLabel?: string;
}) {
  const uid = useId().replace(/[^a-zA-Z0-9_-]/g, "");
  const tableId = uid + "-breakdown-table";
  const [tabId, setTabId] = useState(tabs[0]?.id ?? "");
  const [sortKey, setSortKey] = useState<"count" | "label">("count");
  const [ascending, setAscending] = useState(false);
  const [selected, setSelected] = useState<string | null>(null);
  const [tableOpen, setTableOpen] = useState(false);
  const fallback: BreakdownTab = { id: "", label: "", rows: [], empty: "No breakdown rows are returned." };
  const active = tabs.find((tab) => tab.id === tabId) ?? tabs[0] ?? fallback;
  const sorted = active.rows.slice().sort((a, b) => {
    const cmp = sortKey === "count" ? finite(a.count) - finite(b.count) : a.label.localeCompare(b.label);
    return ascending ? cmp : -cmp;
  });
  let max = 0;
  for (const row of sorted) if (finite(row.count) > max) max = finite(row.count);
  const selectedRow = sorted.find((row) => row.id === selected) ?? null;

  function toggleSort(key: "count" | "label") {
    if (sortKey === key) setAscending((value) => !value);
    else {
      setSortKey(key);
      setAscending(key === "label");
    }
  }

  return (
    <Card className={ANALYTICS_CARD_CLASS}>
      <div className="flex min-w-0 items-center justify-between gap-2 border-b border-border px-4 py-3 sm:px-5">
        <h2 className={ANALYTICS_TEXT.title}>{title}</h2>
        <div className="flex min-w-0 items-center gap-2">
          <span className={"shrink-0 " + ANALYTICS_TEXT.axis}>{countLabel}</span>
          <Button
            variant="ghost"
            aria-label={"View all " + title}
            aria-expanded={tableOpen}
            aria-controls={tableId}
            className={ANALYTICS_PILL_CLASS}
            onClick={() => setTableOpen((value) => !value)}
          >
            View All
          </Button>
        </div>
      </div>
      <div role="tablist" aria-label={title} className="flex min-w-0 flex-wrap gap-1 border-b border-border px-2 sm:px-3">
        {tabs.map((tab) => {
          const isActive = tab.id === active.id;
          return (
            <Button
              key={tab.id}
              id={uid + "-tab-" + tab.id}
              role="tab"
              aria-selected={isActive}
              aria-controls={uid + "-panel"}
              variant="ghost"
              className={ANALYTICS_TAB_CLASS + (isActive ? " font-medium text-fg" : " text-secondary")}
              style={isActive ? { borderBottomColor: ANALYTICS_TOKENS.underline } : undefined}
              onClick={() => {
                setTabId(tab.id);
                setSelected(null);
              }}
            >
              {tab.label}
            </Button>
          );
        })}
      </div>
      <div id={uid + "-panel"} role="tabpanel" aria-labelledby={uid + "-tab-" + active.id} className="min-w-0 p-4 sm:p-5">
        {active.caption ? <p className={"mb-3 " + ANALYTICS_TEXT.caption}>{active.caption}</p> : null}
        <div className="grid min-w-0 grid-cols-[minmax(0,1fr)_auto] gap-2 border-b border-border pb-2">
          <div role="columnheader" aria-sort={sortKey === "label" ? (ascending ? "ascending" : "descending") : "none"}>
            <Button
              variant="ghost"
              className={ANALYTICS_TAB_CLASS + " justify-start px-2"}
              aria-label={"Sort by " + labelHeading}
              onClick={() => toggleSort("label")}
            >
              {sortKey === "label" ? (ascending ? "A to Z" : "Z to A") : labelHeading}
            </Button>
          </div>
          <div role="columnheader" aria-sort={sortKey === "count" ? (ascending ? "ascending" : "descending") : "none"}>
            <Button
              variant="ghost"
              className={ANALYTICS_TAB_CLASS + " justify-end px-2"}
              aria-label={"Sort by " + countLabel}
              onClick={() => toggleSort("count")}
            >
              {sortKey === "count" ? (ascending ? "Least first" : "Most first") : countLabel}
            </Button>
          </div>
        </div>
        {sorted.length === 0 ? (
          <div className="mt-3 flex min-h-[132px] min-w-0 items-center gap-3 rounded-md border border-dashed border-border p-4">
            <span className="h-10 w-px shrink-0 bg-border" aria-hidden="true" />
            <p className={ANALYTICS_TEXT.caption}>{active.empty}</p>
          </div>
        ) : (
          <div className="mt-2 min-w-0">
            {sorted.map((row) => {
              const count = finite(row.count);
              const width = max > 0 ? (count / max) * 100 : 0;
              const isSelected = selected === row.id;
              return (
                <button
                  key={row.id}
                  type="button"
                  aria-pressed={isSelected}
                  onClick={() => setSelected(isSelected ? null : row.id)}
                  className={"relative mb-1 flex min-h-[44px] min-w-0 w-full max-w-full items-center justify-between gap-3 overflow-hidden rounded-md border px-3 text-left " + (isSelected ? "border-fg" : "border-transparent")}
                >
                  <span
                    className="analytics-bar pointer-events-none absolute inset-y-1 left-0 rounded-md"
                    style={{ width: width + "%", background: ANALYTICS_TOKENS.bar }}
                    role="meter"
                    aria-label={row.label + " " + countLabel}
                    aria-valuemin={0}
                    aria-valuemax={max}
                    aria-valuenow={count}
                    aria-valuetext={fmtCount(count) + " " + countLabel}
                  />
                  <span className="relative z-10 min-w-0 break-words text-sm text-fg">{row.label}</span>
                  <span className="relative z-10 shrink-0 text-sm font-medium tabular-nums text-fg">{fmtCount(count)}</span>
                </button>
              );
            })}
          </div>
        )}
        {selectedRow ? (
          <div className={"mt-3 flex min-w-0 flex-wrap items-center gap-2 rounded-md border border-border bg-subtle p-3 " + ANALYTICS_TEXT.captionTight}>
            <span className="text-secondary">Selected</span>
            <span className="min-w-0 break-words text-secondary">{selectedRow.label}</span>
            <strong className="tabular-nums text-fg">{fmtCount(selectedRow.count)}</strong>
            <span className="text-muted">{countLabel}</span>
          </div>
        ) : null}
        {tableOpen ? (
          <div id={tableId} className="mt-3 max-h-64 min-w-0 w-full max-w-full overflow-auto rounded-md border border-border">
            <table className="w-full table-fixed border-collapse text-xs" aria-label={title + " table view"}>
              <thead className="bg-subtle">
                <tr className="border-b border-border">
                  <th scope="col" className="min-w-0 break-words p-3 text-left font-medium text-fg">{labelHeading}</th>
                  <th scope="col" className="min-w-0 break-words p-3 text-right font-medium text-fg">{countLabel}</th>
                </tr>
              </thead>
              <tbody>
                {sorted.map((row) => (
                  <tr key={"table-" + row.id} className="border-b border-border last:border-b-0">
                    <th scope="row" className="min-w-0 break-words p-3 text-left font-normal text-fg">{row.label}</th>
                    <td className="min-w-0 break-words p-3 text-right tabular-nums text-fg">{fmtCount(finite(row.count))}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        ) : null}
      </div>
    </Card>
  );
}
