"use client";

import Link from "next/link";
import { useRouter } from "next/navigation";
import { Fragment, useState } from "react";
import { Badge, Button, Card, Stat, Table, Td, Th } from "@/components/ui";
import { OpsFetchError } from "@/components/ops-fetch-error";
import { copyText } from "@/lib/utils";
import { reportError } from "@/components/toast";
import { EXPLORER_CONTRACTS, explorerQuery, type ExplorerView } from "./types";

const ABSENT = "—";

function short(hash: string | null | undefined, n = 12): string {
  if (!hash) return ABSENT;
  return hash.length > n ? `${hash.slice(0, n)}…` : hash;
}

function fmtTime(ms: number): string {
  if (!Number.isFinite(ms) || ms <= 0) return "time not reported";
  const d = new Date(ms);
  return Number.isNaN(d.getTime()) ? "time not reported" : d.toLocaleString();
}

/** A hash is a content identity, never a URL or an href: it is copied, not navigated. */
function Hash({ value }: { value: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <button
      type="button"
      title={value}
      aria-label={`Copy ${value}`}
      className="font-mono text-xs text-secondary underline decoration-dotted hover:text-fg"
      onClick={async () => {
        const ok = await copyText(value);
        if (!ok) reportError("Clipboard is unavailable. Copy the hash by hand.");
        else {
          setCopied(true);
          setTimeout(() => setCopied(false), 1200);
        }
      }}
    >
      {copied ? "copied" : short(value)}
    </button>
  );
}

function Verdict({ ok, label }: { ok: boolean | null | undefined; label: string }) {
  if (ok === null || ok === undefined) return <Badge tone="amber">{label}: not checked</Badge>;
  return <Badge tone={ok ? "green" : "red"}>{label}: {ok ? "consistent" : "BREAK"}</Badge>;
}

export function ExplorerClient({
  view,
  error,
  query,
}: {
  view: ExplorerView | null;
  error: string | null;
  query: { contract?: string; beforeSeq?: string; limit?: string };
}) {
  const router = useRouter();
  const [entryOpen, setEntryOpen] = useState<number | null>(null);
  const selected = EXPLORER_CONTRACTS.find((c) => c === query.contract) ?? EXPLORER_CONTRACTS[1];
  const pageLink = (opts: { contract?: string; beforeSeq?: number | null; limit?: string }) =>
    `/admin/explorer?${explorerQuery({
      contract: opts.contract ?? selected,
      beforeSeq: opts.beforeSeq === null || opts.beforeSeq === undefined ? undefined : String(opts.beforeSeq),
      limit: opts.limit ?? query.limit,
    })}`;

  return (
    <div>
      <div className="mb-6 flex flex-wrap items-start justify-between gap-3">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">Contract log explorer</h1>
          <p className="mt-1 text-sm text-secondary">
            Observe recorded smart-contract operations and the append-only hash chain behind them.
          </p>
        </div>
        <Button variant="outline" onClick={() => router.refresh()}>Refresh</Button>
      </div>

      <OpsFetchError error={error} label="the contract oplog" />

      {view ? (
        <>
          {/* The honesty boundary is the FIRST panel, not a footnote. This platform has no blocks. */}
          <Card className="mb-4 border-amber-500/40 p-4 text-sm">
            <div className="mb-2 font-medium text-amber-600 dark:text-amber-400">What this explorer shows</div>
            <p className="text-secondary">{view.scope}</p>
            <p className="mt-1 text-secondary">{view.seq_note}</p>
            <p className="mt-1 text-secondary">{view.labels.entry}</p>
            <p className="mt-1 text-secondary">{view.labels.replay}</p>
            <p className="mt-1 text-secondary">{view.verification.trust_limit}</p>
            <p className="mt-1 text-secondary">
              This is a NODE-LOCAL view: <span className="font-medium text-fg">{view.responding_node}</span>. A
              public host is round-robin DNS, so Refresh may reach another node with another shard.
            </p>
          </Card>

          <div className="mb-4 flex flex-wrap items-center gap-2">
            {EXPLORER_CONTRACTS.map((c) => (
              <Link key={c} href={pageLink({ contract: c })}>
                <Badge tone={c === selected ? "blue" : "default"}>{c}</Badge>
              </Link>
            ))}
            <span className="ml-auto text-xs text-secondary">
              contract v{view.contract.version} · {view.contract.declared_kinds.length} declared kinds
            </span>
          </div>

          <div className="mb-4 grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
            <Stat label="Responding node" value={view.responding_node} hint="this shard only" />
            <Stat label="Retained entries" value={String(view.shard.retained)} hint={`window starts at seq ${view.shard.base}`} />
            <Stat label="Next seq" value={String(view.shard.next_seq)} hint="append position, not a block height" />
            <Stat label="Chain tip" value={short(view.shard.head, 10)} hint={view.shard.head || ABSENT} />
          </div>

          <Card className="mb-4 p-4">
            <div className="mb-2 text-sm font-medium">Verification (structural self-check by replay)</div>
            <div className="flex flex-wrap gap-2">
              <Verdict ok={view.verification.chain.ok} label="hash chain" />
              <Verdict
                ok={view.verification.index_replay.available ? (view.verification.index_replay.ok ?? false) : null}
                label="index replay"
              />
              <Badge tone="amber">entry signatures: not verified</Badge>
              <Badge tone="amber">identity unpinned</Badge>
            </div>
            <p className="mt-3 text-xs text-secondary">{view.verification.kind}</p>
            {!view.verification.chain.ok && view.verification.chain.ok !== null ? (
              <p className="mt-1 text-xs text-red-600 dark:text-red-400">
                chain break at seq {view.verification.chain.break?.seq}: stored {short(view.verification.chain.break?.stored)}, expected {short(view.verification.chain.break?.expected)}
              </p>
            ) : null}
            {view.verification.chain.reason ? (
              <p className="mt-1 text-xs text-secondary">{view.verification.chain.reason}</p>
            ) : null}
            {view.verification.index_replay.breach ? (
              <p className="mt-1 text-xs text-red-600 dark:text-red-400">replay: {view.verification.index_replay.breach}</p>
            ) : null}
            {view.verification.index_replay.reason ? (
              <p className="mt-1 text-xs text-secondary">replay: {view.verification.index_replay.reason}</p>
            ) : null}
            <p className="mt-2 text-xs text-secondary">Proves: {view.verification.proves.join(" · ")}.</p>
            <p className="mt-1 text-xs text-secondary">Does NOT prove: {view.verification.does_not_prove.join(" · ")}.</p>
            <p className="mt-1 text-xs text-secondary">{view.verification.entry_signature.reason}</p>
          </Card>

          {view.shard.compacted_prefix ? (
            <Card className="mb-4 p-4 text-xs text-secondary">
              {view.shard.compaction_note} base_prev {short(view.shard.base_prev)}.
            </Card>
          ) : null}

          <Table>
            <thead>
              <tr>
                <Th>Seq</Th><Th>Kind</Th><Th>Effect</Th><Th>Recorded outcome</Th><Th>Tenant / actor</Th>
                <Th>Entry hash</Th><Th>Prev</Th><Th>Chain</Th>
              </tr>
            </thead>
            <tbody>
              {view.entries.map((e) => {
                const open = entryOpen === e.seq;
                return (
                  <Fragment key={e.seq}>
                    <tr className="cursor-pointer" onClick={() => setEntryOpen(open ? null : e.seq)}>
                      <Td className="font-mono tabular-nums">{e.seq}</Td>
                      <Td><Badge tone={e.kind.includes("refus") || e.kind.includes("unattributed") ? "amber" : "blue"}>{e.kind}</Badge></Td>
                      <Td className="text-xs text-secondary">{e.declared_effect}</Td>
                      <Td>
                        <Badge tone={e.execution.recorded_success ? "green" : "red"}>
                          {e.execution.recorded_success ? "recorded ok" : "REFUSED (a record)"}
                        </Badge>
                      </Td>
                      <td className="max-w-[12rem] truncate border-b border-border px-4 py-3 text-xs text-secondary" title={`${e.tenant} / ${e.actor}`}>{e.tenant} / {e.actor}</td>
                      <Td><Hash value={e.entry_hash} /></Td>
                      <Td><Hash value={e.prev_hash} /></Td>
                      <Td>
                        <div className="flex flex-wrap gap-1">
                          <Verdict ok={e.hash_ok} label="hash" />
                          <Verdict ok={e.prev_link_ok} label="link" />
                        </div>
                      </Td>
                    </tr>
                    {open ? (
                      <tr>
                        <td colSpan={8} className="border-b border-border px-4 py-3">
                          <div className="grid gap-3 py-2 text-xs sm:grid-cols-2">
                            <div>
                              <div className="mb-1 font-medium">Recorded execution</div>
                              <p className="text-secondary">time: {fmtTime(e.execution.ts_ms)}</p>
                              <p className="text-secondary">target: {e.target || ABSENT}</p>
                              <p className="text-secondary">contract: {e.contract}</p>
                              <p className="text-secondary">error: {e.execution.recorded_error || ABSENT}</p>
                              <p className="text-secondary">index hash: {short(e.execution.index_hash, 16)}</p>
                              <p className="text-secondary">log head: {short(e.execution.log_head, 16)}</p>
                              <Verdict ok={e.log_head_ok} label="ack head" />
                              <p className="mt-1 text-secondary">{e.execution.recording}</p>
                            </div>
                            <div>
                              <div className="mb-1 font-medium">Payload (bounded display, not canonical bytes)</div>
                              {Object.keys(e.payload).length === 0 ? (
                                <p className="text-secondary">{ABSENT}</p>
                              ) : (
                                <div className="space-y-1">
                                  {Object.entries(e.payload).map(([k, v]) => (
                                    <div key={k} className="flex gap-2">
                                      <code className="shrink-0 text-secondary">{k}</code>
                                      <span className="min-w-0 break-all">{v || ABSENT}</span>
                                    </div>
                                  ))}
                                  {e.payload_omitted_keys > 0 ? (
                                    <p className="text-secondary">{e.payload_omitted_keys} key(s) omitted</p>
                                  ) : null}
                                </div>
                              )}
                              <p className="mt-2 text-secondary">recomputed entry hash: {short(e.recomputed_entry_hash, 16)}</p>
                              <p className="text-secondary">{view.canonical_note}</p>
                              <p className="text-secondary">{view.labels.absent}</p>
                            </div>
                          </div>
                        </td>
                      </tr>
                    ) : null}
                  </Fragment>
                );
              })}
            </tbody>
          </Table>

          {view.window.returned === 0 ? (
            <Card className="mt-4 py-10 text-center text-sm text-secondary">
              No retained entries in this window. {view.labels.absent}
            </Card>
          ) : null}

          <div className="mt-4 flex flex-wrap items-center justify-between gap-3 text-xs text-secondary">
            <span>
              showing {view.window.returned} entr{view.window.returned === 1 ? "y" : "ies"}
              {view.window.from_seq !== null && view.window.to_seq !== null
                ? ` · seq ${view.window.to_seq}–${view.window.from_seq}`
                : ""} · limit {view.window.limit}
            </span>
            {view.window.has_more && view.window.next_before_seq !== null ? (
              <Link href={pageLink({ beforeSeq: view.window.next_before_seq })}>
                <Button variant="outline">Older entries</Button>
              </Link>
            ) : null}
          </div>
          <p className="mt-3 text-xs text-secondary">{view.labels.success} · {view.labels.refused}</p>
        </>
      ) : null}
    </div>
  );
}
