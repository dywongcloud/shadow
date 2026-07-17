import { Suspense } from "react";
import { PageSkeleton } from "@/components/page-skeleton";
import { WebhooksSettings as WebhooksSettingsClient } from "./webhooks-settings-client";

export default function WebhooksSettings(props: { params: Promise<{ project: string }> }) {
  return (
    <Suspense fallback={<PageSkeleton />}>
      <WebhooksSettingsClient paramsPromise={props.params} />
    </Suspense>
  );
}
