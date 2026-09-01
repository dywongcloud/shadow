import UsageView from "./usage-view";

// The root layout's `dynamic = "force-dynamic"` on "/" doesn't apply here (each
// route sets its own segment config) -- force-static + revalidate gives this page
// real ISR/prerender benefit, and it's auth-safe since UsageView carries no server
// data itself (see [[m-usage-static-auth-safe]]).
export default function UsagePage() {
  return <UsageView />;
}
