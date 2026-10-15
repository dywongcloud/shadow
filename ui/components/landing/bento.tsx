"use client";

import { useEffect, useRef } from "react";
import { Section, SectionHead } from "./section";
import { delay, trackSpotlight } from "./lp-utils";
import { Tok } from "./tok";
import { Dns, LogsMetrics, ManagedData, MeshKeystone, Promote, PullRequests, ScaleToZero } from "./bento-visuals";

/** Every card names a feature the code has: pull-request previews, the iroh QUIC mesh with hybrid post-quantum key exchange
 *  (CHANGELOG 2026-09-22), scale to zero, DNS + ACME, the `DbKind` engines, promote, logs and metrics. */
const CARDS: { title: string; body: string; visual: React.ReactNode; span: string; well: string }[] = [
  {
    title: "A live URL for every pull request",
    body: "Each pull request gets its own preview, separate from production. Nothing is promoted until you choose.",
    visual: <PullRequests />,
    span: "lg:col-span-7",
    well: "h-[236px]",
  },
  {
    title: "Functions that sleep",
    body: "Serverless functions scale to zero when idle and wake on the next request.",
    visual: <ScaleToZero />,
    span: "lg:col-span-5",
    well: "h-[236px]",
  },
  {
    title: "Domains, DNS and TLS",
    body: "Attach a custom domain, manage its DNS records and get HTTPS certificates, from the dashboard or the CLI.",
    visual: <Dns />,
    span: "lg:col-span-4",
    well: "h-[178px]",
  },
  {
    title: "Managed data",
    body: "Provision Postgres, Redis, SQLite, queues, blobs, vectors, pub/sub and realtime channels from the same dashboard.",
    visual: <ManagedData />,
    span: "md:col-span-2 lg:col-span-4",
    well: "h-[178px]",
  },
  {
    title: "Promote any earlier build",
    body: "Builds are kept, so when a release goes wrong you can promote an earlier one back to production.",
    visual: <Promote />,
    span: "lg:col-span-4",
    well: "h-[178px]",
  },
  {
    title: "Logs and metrics",
    body: "Build and runtime logs, and metrics polled from the dashboard, in one place.",
    visual: <LogsMetrics />,
    span: "md:col-span-2 lg:col-span-12",
    well: "h-[190px]",
  },
];

export function Bento() {
  const grid = useRef<HTMLDivElement>(null);
  useEffect(() => (grid.current ? trackSpotlight(grid.current) : undefined), []);
  return (
    <Section className="pt-28 sm:pt-36" wash="duo">
      <SectionHead eyebrow="What you get" title="Everything between git push and a live URL." lead="Builds, previews, domains, data and logs in one platform, on one mesh." />
      <div ref={grid} className="mt-14 grid gap-5 md:grid-cols-2 lg:grid-cols-12">
        <article className="lp-card lp-card-keystone lp-reveal grid md:col-span-2 lg:col-span-12 lg:grid-cols-12" style={delay(0)}>
          <div className="flex flex-col justify-center p-7 lg:col-span-4 lg:p-10">
            <h3 className="lp-card-title !text-[28px]">A peer-to-peer mesh</h3>
            <p className="lp-card-copy mt-3 max-w-[400px]">
              Nodes connect to each other over QUIC, and mesh links offer hybrid post-quantum key exchange (X25519 + <Tok>ML-KEM-768</Tok>).
            </p>
          </div>
          <div className="relative h-[230px] sm:h-[290px] lg:col-span-8 lg:h-[400px] lg:[clip-path:inset(-70px_0_0_0)]">
            <div className="absolute inset-x-3 bottom-0 top-8 lg:hidden">
              <MeshKeystone compact />
            </div>
            <div className="absolute bottom-0 left-0 right-0 hidden h-[470px] lg:block">
              <MeshKeystone />
            </div>
            <span className="lp-chip lp-mono absolute left-4 top-4 !text-[11px] uppercase tracking-[0.14em] text-[var(--lp-dim)] lg:left-1 lg:top-1.5">Illustrative</span>
          </div>
        </article>
        {CARDS.map((c, i) => (
          <article key={c.title} className={`lp-card lp-reveal flex flex-col ${c.span}`} style={delay((i % 3) * 90)}>
            <div className={`lp-well m-3.5 mb-0 shrink-0 overflow-hidden ${c.well}`}>{c.visual}</div>
            <div className="p-6 pt-5">
              <h3 className="lp-card-title">{c.title}</h3>
              <p className="lp-card-copy mt-2.5">{c.body}</p>
            </div>
          </article>
        ))}
      </div>
      <p className="lp-mono mt-5 text-right text-[12px] uppercase tracking-[0.16em] text-[var(--lp-dim)]">Illustrations use sample data.</p>
    </Section>
  );
}
