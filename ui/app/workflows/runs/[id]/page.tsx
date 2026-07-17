import { Suspense } from "react";
import { PageSkeleton } from "@/components/page-skeleton";
import { Page as PageClient } from "./page-client";

export default function Page(props: { params: Promise<{ id: string }> }) {
  return (
    <Suspense fallback={<PageSkeleton />}>
      <PageClient paramsPromise={props.params} />
    </Suspense>
  );
}
