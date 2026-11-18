/** The four rival marks used on the comparison views. They live outside the
 *  `/compare` route namespace so a static asset is never mistaken for a page.
 *  Wordmarks are trimmed to their ink and held at 2x the largest display
 *  height; Cloudflare's supplied asset is its square app icon. All four are
 *  white ink on transparent (or a white tile) and need the dark surface. */
export type RivalLogoSpec = {
  src: string;
  width: number;
  height: number;
  shape: "wordmark" | "icon";
};

export const RIVAL_LOGOS: Record<string, RivalLogoSpec> = {
  vercel: { src: "/brands/vercel.png", width: 535, height: 128, shape: "wordmark" },
  railway: { src: "/brands/railway.png", width: 486, height: 128, shape: "wordmark" },
  "fly-io": { src: "/brands/fly-io.svg", width: 300, height: 89, shape: "wordmark" },
  cloudflare: { src: "/brands/cloudflare.webp", width: 256, height: 256, shape: "icon" },
};
