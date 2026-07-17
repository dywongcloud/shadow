import { Suspense } from "react";
import { PageSkeleton } from "@/components/page-skeleton";
import { GeneralSettings as GeneralSettingsClient } from "./general-settings-client";

export default function GeneralSettings(props: { params: Promise<{ project: string }> }) {
  return (
    <Suspense fallback={<PageSkeleton />}>
      <GeneralSettingsClient paramsPromise={props.params} />
    </Suspense>
  );
}
