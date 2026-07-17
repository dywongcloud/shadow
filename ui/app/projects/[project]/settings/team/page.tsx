import { Suspense } from "react";
import { PageSkeleton } from "@/components/page-skeleton";
import { TeamPrivacySettings as TeamPrivacySettingsClient } from "./team-privacy-settings-client";

export default function TeamPrivacySettings(props: { params: Promise<{ project: string }> }) {
  return (
    <Suspense fallback={<PageSkeleton />}>
      <TeamPrivacySettingsClient paramsPromise={props.params} />
    </Suspense>
  );
}
