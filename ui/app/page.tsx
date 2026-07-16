import { HomeClient } from "./home-client";

// The home route flips landing↔dashboard on auth. Keep it dynamic (a server
// shell — route config from a "use client" page is ignored) so Clerk's SSR uses
// the real request auth and signed-in users never see a landing→dashboard flash.
// Every OTHER page is static/ISR or a static shell (see the root layout note).
export const dynamic = "force-dynamic";

export default function Page() {
  return <HomeClient />;
}
