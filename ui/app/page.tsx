import { HomeClient } from "./home-client";

/** Static shell: HomeClient is entirely client-side with no server data access (no cookies()/headers(), no per-request fetch), so the prerendered HTML is identical
 *  for every visitor and the landing/dashboard flip happens in the browser once Clerk settles. */

export default function Page() {
  return <HomeClient />;
}
