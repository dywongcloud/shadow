import { Suspense } from "react";
import { PageSkeleton } from "@/components/page-skeleton";
import { ContainerSettings as ContainerSettingsClient } from "./container-settings-client";

export default function ContainerSettings(props: { params: Promise<{ project: string }> }) {
  return (
    <Suspense fallback={<PageSkeleton />}>
      <ContainerSettingsClient paramsPromise={props.params} />
    </Suspense>
  );
}
