import { redirect } from "next/navigation";

// Renamed to "Observability".
export default function MonitoringRedirect() {
  redirect("/observability");
}
