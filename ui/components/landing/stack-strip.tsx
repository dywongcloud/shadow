import Image from "next/image";
import { Section, SectionHead } from "./section";
import { delay } from "./lp-utils";

/** The preset catalog the build detects (crates/fluid-build/src/framework.rs: nextjs, nuxtjs, sveltekit, astro, vite, vue, create-react-app,
 *  node, static) plus the Dockerfile lane and the bun toolchain; logos are the dashboard's own (`public/frameworks`). */
const STACK: { src: string; name: string; w: number; h: number }[] = [
  { src: "/frameworks/nextjs.png", name: "Next.js", w: 512, h: 512 },
  { src: "/frameworks/nuxt.svg", name: "Nuxt", w: 61, h: 40 },
  { src: "/frameworks/svelte.svg", name: "SvelteKit", w: 47, h: 48 },
  { src: "/frameworks/astro.png", name: "Astro", w: 480, h: 480 },
  { src: "/frameworks/vite.png", name: "Vite", w: 3840, h: 3784 },
  { src: "/frameworks/vue-js.png", name: "Vue", w: 512, h: 512 },
  { src: "/frameworks/react.png", name: "React", w: 960, h: 855 },
  { src: "/frameworks/node.png", name: "Node.js", w: 88, h: 100 },
  { src: "/frameworks/bun.png", name: "Bun", w: 116, h: 102 },
  { src: "/frameworks/docker.png", name: "Docker", w: 1067, h: 771 },
];

const TOOLCHAINS = ["npm", "pnpm", "yarn", "bun", "Go", "Python"];

export function StackStrip() {
  return (
    <Section className="pt-24 text-center sm:pt-28" wash="duo">
      <SectionHead center eyebrow="Bring what you already build" title="Detects your framework. Runs your container." />
      <ul className="mx-auto mt-12 flex max-w-[980px] flex-wrap items-start justify-center gap-x-3 gap-y-7 sm:gap-x-7">
        {STACK.map((s, i) => (
          <li key={s.name} className="lp-reveal flex w-[74px] flex-col items-center gap-2.5" style={delay(i * 45)}>
            <span className="flex h-[68px] w-[68px] items-center justify-center rounded-[18px] border border-white/10 bg-white/[0.045] transition-all duration-300 hover:-translate-y-1 hover:border-white/25 hover:bg-white/[0.08]">
              <Image src={s.src} alt="" width={s.w} height={s.h} sizes="64px" className="h-[32px] w-auto max-w-[44px] object-contain" />
            </span>
            <span className="lp-mono text-[11px] uppercase tracking-[0.12em] text-[var(--lp-dim)]">{s.name}</span>
          </li>
        ))}
      </ul>
      <p className="lp-reveal lp-mono mt-10 text-[13px] tracking-[0.04em] text-[var(--lp-dim)]" style={delay(200)}>
        Builds with {TOOLCHAINS.map((t, i) => (
          <span key={t}>
            <span className="text-zinc-300">{t}</span>
            {i < TOOLCHAINS.length - 1 ? " · " : ""}
          </span>
        ))}
      </p>
    </Section>
  );
}
