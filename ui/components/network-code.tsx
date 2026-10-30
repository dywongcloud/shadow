import Image from "next/image";

const C = {
  kw: "text-violet-400",
  str: "text-emerald-300",
  fn: "text-sky-300",
  com: "text-zinc-500",
  pl: "text-zinc-400",
  id: "text-zinc-200",
};

/** A small, hand-highlighted shadw deploy snippet. */
function CodeSample() {
  return (
    <pre className="no-scrollbar overflow-x-auto px-5 py-4 font-mono text-[10px] leading-[1.75] sm:text-[11px]">
      <code>
        <span className={C.kw}>const</span> <span className={C.id}>res</span> <span className={C.pl}>=</span>{" "}
        <span className={C.kw}>await</span> <span className={C.fn}>fetch</span>
        <span className={C.pl}>(</span>
        <span className={C.str}>&apos;https://api.shadw.cloud/v1/git/deploy&apos;</span>
        <span className={C.pl}>, {"{"}</span>
        {"\n"}
        {"  "}<span className={C.id}>method</span>
        <span className={C.pl}>:</span> <span className={C.str}>&apos;POST&apos;</span>
        <span className={C.pl}>,</span>
        {"\n"}
        {"  "}<span className={C.id}>headers</span>
        <span className={C.pl}>: {"{"} </span>
        <span className={C.id}>Authorization</span>
        <span className={C.pl}>:</span> <span className={C.str}>{"`Bearer ${token}`"}</span>
        <span className={C.pl}> {"}"},</span>
        {"\n"}
        {"  "}<span className={C.id}>body</span>
        <span className={C.pl}>:</span> <span className={C.fn}>JSON</span>
        <span className={C.pl}>.</span>
        <span className={C.fn}>stringify</span>
        <span className={C.pl}>({"{"} </span>
        <span className={C.id}>repo</span>
        <span className={C.pl}>:</span> <span className={C.str}>&apos;github.com/acme/app&apos;</span>
        <span className={C.pl}> {"}"}),</span>
        {"\n"}
        <span className={C.pl}>{"}"});</span>
        {"\n\n"}
        <span className={C.kw}>const</span> <span className={C.pl}>{"{"} </span>
        <span className={C.id}>build_id</span>
        <span className={C.pl}> {"}"} =</span> <span className={C.kw}>await</span> <span className={C.id}>res</span>
        <span className={C.pl}>.</span>
        <span className={C.fn}>json</span>
        <span className={C.pl}>();</span>
        {"\n"}
        <span className={C.com}>{"// → shadw is deploying across the edge…"}</span>
      </code>
    </pre>
  );
}

/** The P2P network flowchart image (a mesh of circuit nodes) with a glowing deploy-snippet card layered above it as "code art". */
export function NetworkCode() {
  return (
    <div className="relative mx-auto w-full max-w-6xl px-4">
      <Image
        src="/network-nodes.png"
        alt=""
        aria-hidden="true"
        width={2059}
        height={764}
        sizes="(max-width: 1152px) 100vw, 1152px"
        className="pointer-events-none h-auto w-full select-none"
      />

      <div className="absolute inset-0 z-10 flex items-center justify-center p-4">
        <div
          className="w-full max-w-[22rem] overflow-hidden rounded-xl border border-white/15 bg-[#0a0a0f]/90 shadow-[0_0_70px_-10px_rgba(37,99,235,0.5)] backdrop-blur-sm sm:max-w-xl"
          style={{ transform: "translateY(48%)" }}
        >
          <div className="flex items-center gap-2 border-b border-white/10 px-4 py-2.5">
            <span className="h-2.5 w-2.5 rounded-full bg-red-400/80" />
            <span className="h-2.5 w-2.5 rounded-full bg-amber-400/80" />
            <span className="h-2.5 w-2.5 rounded-full bg-emerald-400/80" />
            <span className="ml-2 font-mono text-xs text-zinc-500">deploy.ts</span>
          </div>
          <CodeSample />
        </div>
      </div>
    </div>
  );
}
