import { Suspense } from "react";
import { PageSkeleton } from "@/components/page-skeleton";
import { FunctionsSettings as FunctionsSettingsClient } from "./functions-settings-client";

export default function FunctionsSettings(props: { params: Promise<{ project: string }> }) {
  return (
    <Suspense fallback={<PageSkeleton />}>
      <FunctionsSettingsClient paramsPromise={props.params} />
    </Suspense>
  );
}
