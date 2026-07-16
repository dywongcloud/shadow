import * as React from "react";
import Image from "next/image";

// Providers we have real logo PNGs for (in /public/providers).
const PNG = new Set([
  "upstash", "redis", "aws", "supabase", "nile", "motherduck",
  "convex", "drizzle", "turso", "prisma", "postgres",
]);

function Tile({ color, name }: { color: string; name?: string }) {
  return (
    <span
      className="flex h-9 w-9 shrink-0 items-center justify-center rounded-full text-sm font-bold text-white"
      style={{ background: color }}
    >
      {name?.slice(0, 1)}
    </span>
  );
}

/** Brand icon for a marketplace storage provider — real logo PNG when we have
 *  one, otherwise a clean brand-colored lettermark. */
export function BrandIcon({ id, name, color }: { id: string; name: string; color: string }) {
  if (PNG.has(id)) {
    return (
      <span className="flex h-9 w-9 shrink-0 items-center justify-center overflow-hidden rounded-lg bg-white/5">
        <Image src={`/providers/${id}.png`} alt={name} width={28} height={28} className="h-7 w-7 object-contain" />
      </span>
    );
  }
  if (id === "neon") {
    return (
      <span className="flex h-9 w-9 shrink-0 items-center justify-center rounded-lg" style={{ background: "#0b0f17" }}>
        <svg width="20" height="20" viewBox="0 0 24 24" fill="none" aria-hidden>
          <path d="M5 5h9.5C16.4 5 18 6.6 18 8.5V19l-3-2.6V9.5c0-.6-.4-1-1-1H9.4L19 17V5.5h.6V19h-2.3L8 11.2V19H5V5Z" fill="#00e599" />
        </svg>
      </span>
    );
  }
  if (id === "mongodb") {
    return (
      <span className="flex h-9 w-9 shrink-0 items-center justify-center rounded-lg bg-white">
        <svg width="20" height="20" viewBox="0 0 24 24" fill="none" aria-hidden>
          <path d="M12 2c2.5 4 5 6.4 5 10.5 0 3.6-2.2 6.3-4.3 7.3l-.2 2.2h-1l-.2-2.2C9 18.8 7 16.1 7 12.5 7 8.4 9.5 6 12 2Z" fill="#13aa52" />
        </svg>
      </span>
    );
  }
  return <Tile color={color} name={name} />;
}

/** Real logo for a native kind, when we have one (Postgres/Redis). */
export function nativePng(kind: string): string | null {
  if (kind === "postgres") return "/providers/postgres.png";
  if (kind === "redis") return "/providers/redis.png";
  return null;
}

/** Exact geist-style icon for the native Blob primitive. */
export function BlobIcon() {
  return (
    <svg fill="none" height="24" viewBox="0 0 48 48" width="24" aria-hidden className="text-amber-500">
      <path
        d="M3 10.125C3 6.18997 6.18997 3 10.125 3H37.875C41.81 3 45 6.18997 45 10.125V24H42.75V10.125C42.75 7.43261 40.5674 5.25 37.875 5.25H10.125C7.43261 5.25 5.25 7.43261 5.25 10.125V37.875C5.25 40.5674 7.43261 42.75 10.125 42.75H26.25V45H10.125C6.18997 45 3 41.81 3 37.875V10.125ZM15.2901 29.5552C14.8912 28.8898 14.625 27.9463 14.625 26.5739C14.625 23.8125 15.7061 20.7786 17.4725 18.4441C19.2451 16.1014 21.5734 14.625 24 14.625C26.4266 14.625 28.7549 16.1014 30.5275 18.4441C31.9114 20.273 32.8746 22.5313 33.2269 24.7514C33.9286 24.5517 34.6669 24.392 35.4285 24.272C34.9986 21.7045 33.8864 19.1542 32.3217 17.0864C30.2941 14.4066 27.3724 12.375 24 12.375C20.6276 12.375 17.7059 14.4066 15.6782 17.0864C13.6445 19.7743 12.375 23.2774 12.375 26.5739C12.375 28.2304 12.6964 29.6046 13.3602 30.712C14.0306 31.8304 14.995 32.5845 16.1163 33.0857C18.2733 34.0498 21.1488 34.125 24 34.125C24.88 34.125 25.7622 34.1178 26.625 34.0796V31.8257C25.8109 31.8641 24.9343 31.875 24 31.875C21.0522 31.875 18.6777 31.766 17.0344 31.0316C16.2556 30.6835 15.6824 30.2097 15.2901 29.5552Z"
        fill="currentColor"
      />
      <path
        d="M46.875 31.6681V43.7069C46.875 45.4566 43.3492 46.875 39 46.875C34.6508 46.875 31.125 45.4566 31.125 43.7069V31.6681M46.875 31.6681C46.875 29.9184 43.3492 28.5 39 28.5C34.6508 28.5 31.125 29.9184 31.125 31.6681M46.875 31.6681C46.875 33.4178 43.3492 34.8362 39 34.8362C34.6508 34.8362 31.125 33.4178 31.125 31.6681M46.875 37.6875C46.875 39.4372 43.3492 40.8556 39 40.8556C34.6508 40.8556 31.125 39.4372 31.125 37.6875"
        stroke="currentColor"
        strokeWidth="2"
      />
    </svg>
  );
}
