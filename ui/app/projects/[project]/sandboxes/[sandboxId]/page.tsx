import { Suspense } from "react";
import { PageSkeleton } from "@/components/page-skeleton";
import { SandboxDetail as SandboxDetailClient } from "./sandbox-detail-client";

export default function SandboxDetail(props: { params: Promise<{ project: string; sandboxId: string }> }) {
  return (
    <Suspense fallback={<PageSkeleton />}>
      <SandboxDetailClient paramsPromise={props.params} />
    </Suspense>
  );
}
