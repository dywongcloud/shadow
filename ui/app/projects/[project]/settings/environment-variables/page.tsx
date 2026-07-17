import { Suspense } from "react";
import { PageSkeleton } from "@/components/page-skeleton";
import { EnvVarsPage as EnvVarsPageClient } from "./env-vars-client";

export default function EnvVarsPage(props: { params: Promise<{ project: string }> }) {
  return (
    <Suspense fallback={<PageSkeleton />}>
      <EnvVarsPageClient paramsPromise={props.params} />
    </Suspense>
  );
}
