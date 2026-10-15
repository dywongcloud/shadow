import type { Metadata, Viewport } from "next";

/** The share card of every public page (`app/opengraph-image.tsx`). A page's own `openGraph` or `twitter` object REPLACES the root layout's, images
 *  included, so each page names the card itself; `metadataBase` makes the URL absolute. */
const CARD = { url: "/opengraph-image", width: 1200, height: 630, alt: "shadw — push to Git, get a live URL" };

type PageMeta = { title: string; socialTitle: string; description: string; twitterDescription?: string; path: string };

/** Metadata for a public page. `title` is the page's own name: the root layout's template adds " · shadw", so the brand prints once in a tab;
 *  `socialTitle` carries it for a link preview. */
export function pageMeta({ title, socialTitle, description, twitterDescription, path }: PageMeta): Metadata {
  return {
    title,
    description,
    alternates: { canonical: path },
    openGraph: { title: socialTitle, description, url: path, type: "website", siteName: "shadw", images: [CARD] },
    twitter: { card: "summary_large_image", title: socialTitle, description: twitterDescription ?? description, images: [{ url: CARD.url, alt: CARD.alt }] },
  };
}

/** The pages that sit in `MarketingShell` are dark whatever the visitor's scheme: the browser bar and the default canvas say so from the first byte,
 *  not after hydration (`/` is a static shell shared with the dashboard, so the shell sets its own bar from an effect). */
export const darkViewport: Viewport = { themeColor: "#000000", colorScheme: "dark" };

