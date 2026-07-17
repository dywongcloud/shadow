import { Suspense } from "react";
import { PageSkeleton } from "@/components/page-skeleton";
import { SecureComputeSettings as SecureComputeSettingsClient } from "./secure-compute-settings-client";

export default function SecureComputeSettings(props: { params: Promise<{ project: string }> }) {
  return (
    <Suspense fallback={<PageSkeleton />}>
      <SecureComputeSettingsClient paramsPromise={props.params} />
    </Suspense>
  );
}
