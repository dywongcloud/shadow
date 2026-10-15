"use client";

import { useRef, useState } from "react";
import Link from "next/link";
import { ArrowRight, Check, Copy, GitBranch, KeyRound, ScrollText, X, type LucideIcon } from "lucide-react";
import { Section, SectionHead } from "./section";
import { delay } from "./lp-utils";
import { Tok } from "./tok";

/** The CLI lines are `shadw auth login` and `shadw deploy` (crates/shadw-cli); the requests are the examples the docs publish
 *  (ui/app/docs/page.tsx) against `POST /v1/git/deploy` (`GitDeployRequest.production`, crates/fluid-core). A leading `$ ` is display only. */
const TABS = [
  {
    label: "CLI",
    short: "CLI",
    raw: "# Authenticate once with an API key\n$ shadw auth login --token <your-api-key>\n\n# Deploy a repository and follow the build\n$ shadw deploy https://github.com/you/my-app \\\n    --project my-app --follow\n\n# Re-attach to a build's logs, or rebuild a project\n$ shadw build logs dpl-3f9a21c07b --follow\n$ shadw projects redeploy my-app",
  },
  {
    label: "cURL",
    short: "cURL",
    raw: `# Auth: an API key (hive_…) binds the request to its team.\ncurl -X POST https://api.shadw.cloud/v1/git/deploy \\\n  -H 'content-type: application/json' \\\n  -H 'Authorization: Bearer hive_YOUR_API_KEY' \\\n  -d '{\n    "repo_url": "https://github.com/acme/app",\n    "project": "my-app",\n    "production": true\n  }'`,
  },
  {
    label: "TypeScript",
    short: "TS",
    raw: `const res = await fetch("https://api.shadw.cloud/v1/git/deploy", {\n  method: "POST",\n  headers: {\n    "content-type": "application/json",\n    Authorization: \`Bearer \${process.env.HIVE_API_KEY}\`,\n  },\n  body: JSON.stringify({\n    repo_url: "https://github.com/acme/app",\n    project: "my-app",\n    production: true,\n  }),\n});\nconst { build_id } = await res.json();`,
  },
  {
    label: "Python",
    short: "PY",
    raw: `import os\nimport requests\n\nres = requests.post(\n    "https://api.shadw.cloud/v1/git/deploy",\n    headers={"Authorization": f"Bearer {os.environ['HIVE_API_KEY']}"},\n    json={\n        "repo_url": "https://github.com/acme/app",\n        "project": "my-app",\n        "production": True,\n    },\n)\nbuild_id = res.json()["build_id"]`,
  },
];
const MAX_LINES = Math.max(...TABS.map((t) => t.raw.split("\n").length));

type CopyState = "idle" | "copied" | "failed";
const COPY_LABEL: Record<CopyState, string> = { idle: "Copy", copied: "Copied", failed: "Copy failed" };

const POINTS: { key: string; icon: LucideIcon; text: React.ReactNode }[] = [
  { key: "keys", icon: KeyRound, text: "API keys bound to a team, the same for the CLI and the REST API" },
  { key: "push", icon: GitBranch, text: "Push to deploy from GitHub, or deploy from a URL, a zip or an image" },
  { key: "logs", icon: ScrollText, text: <>{"Follow a build's logs from the terminal with "}<Tok>--follow</Tok></> },
];

/** Strings, URLs, placeholders, flags, comments and a few keywords; a comment starts at line start or after a space, so `https://` is a URL. An index
 *  expression such as `res.json()["build_id"]` is one token, so it never splits at its bracket. */
const TOKEN = /[\w.]+(?:\(\))?\["[^"\\]*"\]|"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|`(?:[^`\\]|\\.)*`|(?<=^|\s)(?:#|\/\/).*$|https?:\/\/[^\s\\]+|<[a-z-]+>|(?<![\w-])--?[a-zA-Z][\w-]*|dpl-[0-9a-f]+|\bmy-app\b|\b(?:const|await|import|True|true)\b/g;

/** A token this short never splits across lines; a longer one (the API URL, a header string) may wrap, and a URL breaks after its slashes. */
const LONG_TOKEN = 30;
const INDEX_EXPR = /^([\w.]+(?:\(\))?\[)("[^"]*")(\])$/;

/** A URL breaks after the slashes of its path, never inside `://` or a name. */
function breakAfterSlashes(url: string): React.ReactNode[] {
  const at = url.indexOf("://") + 3;
  return [url.slice(0, at), ...url.slice(at).split("/").flatMap((part, i, all) => (i < all.length - 1 ? [part, "/", <wbr key={i} />] : [part]))];
}

function tint(line: string): React.ReactNode[] {
  if (line.startsWith("$ ")) {
    return [
      <span key="p" className="lp-prompt">
        {"$ "}
      </span>,
      ...tint(line.slice(2)),
    ];
  }
  const out: React.ReactNode[] = [];
  let last = 0;
  for (const m of line.matchAll(TOKEN)) {
    const at = m.index ?? 0;
    if (at > last) out.push(line.slice(last, at));
    const t = m[0];
    const cls = /^(#|\/\/)/.test(t) ? "lp-dim" : /^(["'`]|https?:|<)/.test(t) ? "lp-str" : /^(dpl-|my-app|[\w.]+(?:\(\))?\[)/.test(t) ? "" : "lp-key";
    const keep = t.length <= LONG_TOKEN && !/\s/.test(t);
    const index = INDEX_EXPR.exec(t);
    const body = index ? [index[1], <span key="s" className="lp-str">{index[2]}</span>, index[3]] : !keep && t.includes("://") ? breakAfterSlashes(t) : t;
    out.push(
      <span key={at} className={cls === "lp-dim" ? cls : `${cls}${keep ? " whitespace-nowrap" : ""}`}>
        {body}
      </span>,
    );
    last = at + t.length;
  }
  if (last < line.length) out.push(line.slice(last));
  return out;
}

export function Developer() {
  const [tab, setTab] = useState(0);
  const [copyState, setCopyState] = useState<CopyState>("idle");
  const tabs = useRef<(HTMLButtonElement | null)[]>([]);
  const active = TABS[tab];
  const onTabKey = (e: React.KeyboardEvent) => {
    const to: Record<string, number> = { ArrowRight: tab + 1, ArrowLeft: tab - 1, Home: 0, End: TABS.length - 1 };
    if (!(e.key in to)) return;
    e.preventDefault();
    const next = (to[e.key] + TABS.length) % TABS.length;
    setTab(next);
    tabs.current[next]?.focus();
  };
  const copy = async () => {
    let outcome: CopyState = "copied";
    try {
      await navigator.clipboard.writeText(active.raw.replace(/^\$ /gm, ""));
    } catch {
      outcome = "failed";
    }
    setCopyState(outcome);
    window.setTimeout(() => setCopyState("idle"), 1800);
  };
  return (
    <Section className="pt-28 sm:pt-36" wash="violet">
      <div className="grid grid-cols-1 items-center gap-12 lg:grid-cols-12 lg:gap-14">
        <div className="min-w-0 lg:col-span-5">
          <SectionHead eyebrow="For developers and agents" title="One API for you, your CI and your agents." lead="The CLI and the REST API use the same API keys, so a script, a CI job or an agent deploys the same way you do." />
          <ul className="lp-reveal mt-8 space-y-4" style={delay(120)}>
            {POINTS.map(({ key, icon: Icon, text }) => (
              <li key={key} className="flex items-start gap-3.5 text-[15px] leading-relaxed text-zinc-300">
                <span className="mt-0.5 flex h-8 w-8 shrink-0 items-center justify-center rounded-lg border border-white/10 bg-white/[0.05]">
                  <Icon className="h-4 w-4 text-[#d79be6]" />
                </span>
                <span className="[text-wrap:pretty]">{text}</span>
              </li>
            ))}
          </ul>
          <Link href="/docs/getting-started" className="lp-reveal group mt-6 inline-flex min-h-[44px] items-center gap-2 py-3 text-[15px] text-white" style={delay(200)}>
            <span className="underline decoration-white/30 underline-offset-[6px] transition-colors group-hover:decoration-white">Getting started guide</span>
            <ArrowRight className="h-4 w-4 transition-transform group-hover:translate-x-1" />
          </Link>
        </div>

        <div className="lp-reveal min-w-0 lg:col-span-7" style={delay(100)}>
          <div className="lp-glass lp-bloom lp-limb-top relative overflow-hidden">
            <div className="lp-glass-bar !gap-0 !py-0 !pl-2 !pr-3">
              <div role="tablist" aria-label="Deploy with" onKeyDown={onTabKey} className="no-scrollbar flex overflow-x-auto">
                {TABS.map((t, i) => (
                  <button
                    key={t.label}
                    ref={(el) => {
                      tabs.current[i] = el;
                    }}
                    id={`lp-dev-tab-${i}`}
                    role="tab"
                    type="button"
                    aria-selected={i === tab}
                    aria-label={t.short === t.label ? t.label : `${t.label} (${t.short})`}
                    aria-controls="lp-dev-panel"
                    tabIndex={i === tab ? 0 : -1}
                    onClick={() => setTab(i)}
                    className={`lp-mono relative inline-flex h-[44px] min-w-[44px] shrink-0 items-center justify-center whitespace-nowrap px-3.5 text-[12px] uppercase tracking-[0.14em] transition-colors ${i === tab ? "text-white underline decoration-2 underline-offset-[10px]" : "text-[var(--lp-dim)] hover:text-zinc-200"}`}
                  >
                    <span className="max-[359px]:hidden">{t.label}</span>
                    <span aria-hidden className="min-[360px]:hidden">
                      {t.short}
                    </span>
                    {i === tab && <span aria-hidden className="absolute inset-x-2.5 -bottom-px h-px bg-gradient-to-r from-[#e92a67] via-[#a853ba] to-[#2a8af6]" />}
                  </button>
                ))}
              </div>
              <button
                type="button"
                onClick={copy}
                aria-label={copyState === "idle" ? `Copy the ${active.label} example` : COPY_LABEL[copyState]}
                className="ml-auto inline-flex h-[44px] min-w-[44px] shrink-0 items-center justify-center gap-2 rounded-md px-2.5 text-[12px] text-[var(--lp-mute)] transition-colors hover:bg-white/[0.07] hover:text-white"
              >
                {copyState === "copied" ? <Check className="h-3.5 w-3.5 text-emerald-300" /> : copyState === "failed" ? <X className="h-3.5 w-3.5 text-rose-300" /> : <Copy className="h-3.5 w-3.5" />}
                <span className="hidden sm:inline">{COPY_LABEL[copyState]}</span>
              </button>
              <span role="status" className="sr-only">
                {copyState === "idle" ? "" : COPY_LABEL[copyState]}
              </span>
            </div>
            <pre
              id="lp-dev-panel"
              aria-labelledby={`lp-dev-tab-${tab}`}
              className="lp-term-body lp-mono !m-0 overflow-x-auto !whitespace-pre !p-5 !text-[13.5px] !leading-[1.75] max-sm:!whitespace-pre-wrap max-sm:[overflow-wrap:anywhere]"
              role="tabpanel"
              style={{ minHeight: `calc(${MAX_LINES} * 1.75em + 40px)` }}
            >
              <code>
                {active.raw.split("\n").map((line, i) => (
                  <div key={i}>{line ? tint(line) : " "}</div>
                ))}
              </code>
            </pre>
          </div>
        </div>
      </div>
    </Section>
  );
}
