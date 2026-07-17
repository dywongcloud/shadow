import { Suspense } from "react";
import { PageSkeleton } from "@/components/page-skeleton";
import { SettingsLayout as SettingsLayoutClient } from "./settings-layout-client";

export default function SettingsLayout(props: {
  children: React.ReactNode;
  params: Promise<{ project: string }>;
}) {
  return (
    <Suspense fallback={<PageSkeleton />}>
      <SettingsLayoutClient paramsPromise={props.params}>{props.children}</SettingsLayoutClient>
    </Suspense>
  );
}
