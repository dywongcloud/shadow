import { Suspense } from "react";
import { PageSkeleton } from "@/components/page-skeleton";
import { fetchOpsResult } from "@/lib/ops-data";
import { ExplorerClient } from "./explorer-client";
import { explorerQuery, isExplorerView, type ExplorerView } from "./types";

type Search = { [key: string]: string | string[] | undefined };

function one(v: string | string[] | undefined): string | undefined {
  return typeof v === "string" ? v : undefined;
}

/** Server prefetch behind proxy.ts's `/admin` owner gate. `fetchOpsResult` carries the operator
 *  bearer server-side; no client poll (a browser `/ops` hop would carry only the tenant cookie,
 *  which an operator-only route refuses). Refresh re-runs this server render. */
async function ExplorerData({ searchParams }: { searchParams: Promise<Search> }) {
  const p = await searchParams;
  const query = {
    contract: one(p.contract),
    beforeSeq: one(p.before_seq),
    limit: one(p.limit),
  };
  const { data, error: fetchError } = await fetchOpsResult<ExplorerView>(
    `/v1/admin/contract/explorer?${explorerQuery(query)}`,
  );
  const shapeError = data !== null && !isExplorerView(data)
    ? "ops GET returned an unrecognised contract explorer payload"
    : null;
  return (
    <ExplorerClient
      view={isExplorerView(data) ? data : null}
      error={fetchError ?? shapeError}
      query={query}
    />
  );
}

export default function ExplorerPage({ searchParams }: { searchParams: Promise<Search> }) {
  return (
    <Suspense fallback={<PageSkeleton />}>
      <ExplorerData searchParams={searchParams} />
    </Suspense>
  );
}
