"use client";

import dynamic from "next/dynamic";
import Link from "next/link";
import { Landing } from "@/components/landing";
import { useSettledAuth, resetAuthSettle } from "@/lib/auth-settle";

const Dashboard = dynamic(() => import("./home-dashboard").then((m) => m.Dashboard), {
  ssr: false,
});

const clerkEnabled = !!process.env.NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY;

/** The root route body: the Landing for signed-out and not-yet-resolved visitors (so `/` prerenders it), the dashboard for signed-in users (local
 *  no-Clerk mode: dashboard only); it renders from `useSettledAuth()` so a flapping Clerk signal cannot thrash the two. */
export function HomeClient() {
  if (!clerkEnabled) return <Dashboard />;
  return <GuardedHome />;
}

function GuardedHome() {
  const view = useSettledAuth();
  if (view === "in") return <Dashboard />;
  if (view === "degraded") return <AuthDegraded />;
  return <Landing />;
}

/** Terminal state after the guard's flip bound trips (the auth signal on this browser is flapping): the stable signed-out Landing plus an honest
 *  explanation; "Try again" is the ONLY way out, since an automatic retry would re-enter the loop. */
function AuthDegraded() {
  return (
    <>
      <Landing />
      <div
        role="alert"
        className="fixed inset-x-0 bottom-0 z-[200] border-t border-amber-500/40 bg-[#141519]/95 px-4 py-3 text-sm text-zinc-200 backdrop-blur"
      >
        <div className="mx-auto flex max-w-3xl flex-wrap items-center justify-center gap-x-4 gap-y-2 text-center">
          <span>
            We couldn&apos;t keep you signed in on this browser — it may be blocking the
            cookies sign-in needs (common in private browsing on iPhone). You can keep
            browsing signed out, or try again.
          </span>
          <span className="flex shrink-0 items-center gap-4">
            <Link href="/sign-in" className="font-semibold text-white underline underline-offset-2">
              Sign in
            </Link>
            <button
              type="button"
              onClick={resetAuthSettle}
              className="font-semibold text-white underline underline-offset-2"
            >
              Try again
            </button>
          </span>
        </div>
      </div>
    </>
  );
}
