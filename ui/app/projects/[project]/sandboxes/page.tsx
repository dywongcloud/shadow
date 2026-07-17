import { Suspense } from "react";
import { PageSkeleton } from "@/components/page-skeleton";
import { SandboxesPage as SandboxesPageClient } from "./sandboxes-page-client";

export default function SandboxesPage(props: { params: Promise<{ project: string }> }) {
  return (
    <Suspense fallback={<PageSkeleton />}>
      <SandboxesPageClient paramsPromise={props.params} />
    </Suspense>
  );
}
