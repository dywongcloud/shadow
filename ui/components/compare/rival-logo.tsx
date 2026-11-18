import Image from "next/image";
import { RIVAL_LOGOS } from "@/lib/compare-logos";

/** One box for all four marks, so a wide wordmark and a square icon sit on the
 *  same optical line. `next/image` is unoptimized: an SVG must not go through
 *  the optimizer, and these are small local assets already sized for 2x.
 *  The name is always present as text for screen readers, print and forced
 *  colors, where a white wordmark would vanish. Sizes are px: the dashboard
 *  root scales rem to 83.125%. */
const BOX: Record<RivalLogoSize, { wordmark: string; icon: string }> = {
  hero: { wordmark: "h-[clamp(30px,5vw,56px)] w-[min(52vw,300px)]", icon: "h-[clamp(30px,5vw,56px)] w-[clamp(30px,5vw,56px)]" },
  card: { wordmark: "h-[30px] w-[min(44vw,180px)]", icon: "h-[30px] w-[30px]" },
  nav: { wordmark: "h-[16px] w-[68px]", icon: "h-[16px] w-[16px]" },
  table: { wordmark: "h-[22px] w-[min(34vw,110px)]", icon: "h-[22px] w-[22px]" },
};

export type RivalLogoSize = "hero" | "card" | "nav" | "table";

export function RivalLogo({
  slug,
  rival,
  size = "card",
  showName = true,
  className = "",
}: {
  slug: string;
  rival: string;
  size?: RivalLogoSize;
  /** false where a visible text label already carries the name */
  showName?: boolean;
  className?: string;
}) {
  const logo = RIVAL_LOGOS[slug];
  if (!logo) return <span className="lp-rival-name">{rival}</span>;
  return (
    <span className={`lp-rival-mark inline-flex items-center ${BOX[size][logo.shape]} ${className}`}>
      <Image
        src={logo.src}
        alt=""
        aria-hidden="true"
        width={logo.width}
        height={logo.height}
        unoptimized
        sizes="(max-width: 640px) 52vw, 300px"
        className="lp-rival-logo h-full w-full object-contain"
      />
      {showName && <span className="lp-rival-name">{rival}</span>}
    </span>
  );
}
