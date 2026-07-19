import { Suspense } from "react";
import { PageSkeleton } from "@/components/page-skeleton";
import { NetworkSettings as NetworkSettingsClient } from "./network-settings-client";

export default function NetworkSettings(props: { params: Promise<{ project: string }> }) {
  return (
    <Suspense fallback={<PageSkeleton />}>
      <NetworkSettingsClient paramsPromise={props.params} />
    </Suspense>
  );
}
