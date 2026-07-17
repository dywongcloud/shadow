import { Suspense } from "react";
import { PageSkeleton } from "@/components/page-skeleton";
import { RoutingSettings as RoutingSettingsClient } from "./routing-settings-client";

export default function RoutingSettings(props: { params: Promise<{ project: string }> }) {
  return (
    <Suspense fallback={<PageSkeleton />}>
      <RoutingSettingsClient paramsPromise={props.params} />
    </Suspense>
  );
}
