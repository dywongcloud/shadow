import { Suspense } from "react";
import { PageSkeleton } from "@/components/page-skeleton";
import { MicrofrontendsSettings as MicrofrontendsSettingsClient } from "./microfrontends-settings-client";

export default function MicrofrontendsSettings(props: { params: Promise<{ project: string }> }) {
  return (
    <Suspense fallback={<PageSkeleton />}>
      <MicrofrontendsSettingsClient paramsPromise={props.params} />
    </Suspense>
  );
}
