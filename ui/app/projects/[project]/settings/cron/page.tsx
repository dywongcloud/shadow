import { Suspense } from "react";
import { PageSkeleton } from "@/components/page-skeleton";
import { CronSettings as CronSettingsClient } from "./cron-settings-client";

export default function CronSettings(props: { params: Promise<{ project: string }> }) {
  return (
    <Suspense fallback={<PageSkeleton />}>
      <CronSettingsClient paramsPromise={props.params} />
    </Suspense>
  );
}
