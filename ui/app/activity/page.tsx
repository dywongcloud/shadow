import { redirect } from "next/navigation";

// Activity now lives under Team Settings → Activity.
export default function ActivityRedirect() {
  redirect("/settings");
}
