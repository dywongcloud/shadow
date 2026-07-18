import { Suspense } from "react";
import { PageSkeleton } from "@/components/page-skeleton";
import { GitSettings as GitSettingsClient } from "./git-settings-client";

export default function GitSettings(props: { params: Promise<{ project: string }> }) {
  return (
    <Suspense fallback={<PageSkeleton />}>
      <GitSettingsClient paramsPromise={props.params} />
    </Suspense>
  );
}
