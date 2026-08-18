import { NextResponse } from "next/server";

// Server-side proxy for the iptv-org catalog. The browser cannot fetch
// iptv-org.github.io directly — the app's CSP `connect-src` is deliberately
// tight ('self' + a short allowlist), and iptv is not on it (nor could the
// hundreds of arbitrary stream hosts ever be). Fetching here, same-origin,
// keeps the CSP untouched and also sidesteps CORS. The join + region shaping
// runs server-side too, so the client downloads only the small per-region
// result instead of two multi-MB JSON blobs.

export const dynamic = "force-dynamic";
// The catalog changes rarely; cache the joined result for an hour so repeat
// visits (and every node) don't re-pull iptv-org.
export const revalidate = 3600;

const CHANNELS_URL = "https://iptv-org.github.io/api/channels.json";
const STREAMS_URL = "https://iptv-org.github.io/api/streams.json";

// Serving regions → the country whose national catalog represents them. Must
// match `lib/tv.ts`'s REGION_TV_GEO countries.
const WANTED_COUNTRIES = new Set(["TH", "HK", "US", "BR", "DE"]);

interface IptvChannel {
  id: string;
  name: string;
  country: string;
  categories?: string[];
  is_nsfw?: boolean;
  closed?: string | null;
}
interface IptvStream {
  channel: string | null;
  url: string;
}

export async function GET() {
  try {
    const [chRes, stRes] = await Promise.all([
      fetch(CHANNELS_URL, { next: { revalidate: 3600 } }),
      fetch(STREAMS_URL, { next: { revalidate: 3600 } }),
    ]);
    if (!chRes.ok || !stRes.ok) {
      return NextResponse.json({ error: "upstream catalog unavailable" }, { status: 502 });
    }
    const channels = (await chRes.json()) as IptvChannel[];
    const streams = (await stRes.json()) as IptvStream[];

    // First https HLS stream per channel id.
    const streamByChannel = new Map<string, string>();
    for (const s of streams) {
      if (!s.channel || streamByChannel.has(s.channel)) continue;
      if (!s.url?.startsWith("https://") || !s.url.includes(".m3u8")) continue;
      streamByChannel.set(s.channel, s.url);
    }

    // country → channels with a playable stream.
    const byCountry: Record<string, { id: string; name: string; country: string; categories: string[]; url: string }[]> = {};
    for (const c of channels) {
      if (!WANTED_COUNTRIES.has(c.country) || c.is_nsfw || c.closed) continue;
      const url = streamByChannel.get(c.id);
      if (!url) continue;
      (byCountry[c.country] ??= []).push({
        id: c.id,
        name: c.name,
        country: c.country,
        categories: c.categories ?? [],
        url,
      });
    }
    return NextResponse.json(
      { byCountry },
      { headers: { "cache-control": "public, max-age=3600, s-maxage=3600" } },
    );
  } catch (e) {
    return NextResponse.json({ error: String(e instanceof Error ? e.message : e) }, { status: 502 });
  }
}
