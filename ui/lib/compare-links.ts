/** Just the four comparison destinations, for nav and the landing link strip. Kept separate
 *  from `compare-data.ts` so the landing's client bundle does not carry every page's copy. */
export const COMPARE_LINKS = [
  { slug: "vercel", rival: "Vercel" },
  { slug: "railway", rival: "Railway" },
  { slug: "fly-io", rival: "Fly.io" },
  { slug: "cloudflare", rival: "Cloudflare" },
] as const;
