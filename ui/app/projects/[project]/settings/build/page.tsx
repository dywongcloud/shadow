import { Suspense } from "react";
import { PageSkeleton } from "@/components/page-skeleton";
import { BuildSettings as BuildSettingsClient } from "./build-settings-client";

export default function BuildSettings(props: { params: Promise<{ project: string }> }) {
  return (
    <Suspense fallback={<PageSkeleton />}>
      <BuildSettingsClient paramsPromise={props.params} />
    </Suspense>
  );
}
