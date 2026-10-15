"use client";

import { useRef, useState } from "react";
import Link from "next/link";
import { ArrowRight, Check, Copy, GitBranch, KeyRound, ScrollText } from "lucide-react";
import { Section, SectionHead } from "./section";
import { delay } from "./lp-utils";

/** The CLI lines are `shadw auth login` and `shadw deploy` (crates/shadw-cli); the requests are the examples the docs publish
 *  (ui/app/docs/page.tsx) against `POST /v1/git/deploy` (`GitDeployRequest.production`, crates/fluid-core). A leading `$ ` is display only. */
const TABS = [
  {
    label: "CLI",
    raw: "# Authenticate once with an API key\n$ shadw auth login --token <your-api-key>\n\n# Deploy a repository and follow the build\n$ shadw deploy https://github.com/you/my-app \\\n    --project my-app --follow\n\n# Re-attach to a build's logs, or rebuild a project\n$ shadw build logs dpl-3f9a21c07b --follow\n$ shadw projects redeploy my-app",
  },
  {
    label: "cURL",
    raw: `# Auth: an API key (hive_…) binds the request to its team.\ncurl -X POST https://api.shadw.cloud/v1/git/deploy \\\n  -H 'content-type: application/json' \\\n  -H 'Authorization: Bearer hive_YOUR_API_KEY' \\\n  -d '{\n    "repo_url": "https://github.com/acme/app",\n    "project": "my-app",\n    "production": true\n  }'`,
  },
  {
    label: "TypeScript",
    raw: `const res = await fetch("https://api.shadw.cloud/v1/git/deploy", {\n  method: "POST",\n  headers: {\n    "content-type": "application/json",\n    Authorization: \`Bearer \${process.env.HIVE_API_KEY}\`,\n  },\n  body: JSON.stringify({\n    repo_url: "https://github.com/acme/app",\n    project: "my-app",\n    production: true,\n  }),\n});\nconst { build_id } = await res.json();`,
  },
  {
    label: "Python",
    raw: `import os\nimport requests\n\nres = requests.post(\n    "https://api.shadw.cloud/v1/git/deploy",\n    headers={"Authorization": f"Bearer {os.environ['HIVE_API_KEY']}"},\n    json={\n        "repo_url": "https://github.com/acme/app",\n        "project": "my-app",\n        "production": True,\n    },\n)\nbuild_id = res.json()["build_id"]`,
  },
];
const MAX_LINES = Math.max(...TABS.map((t) => t.raw.split("\n").length));

const POINTS = [
  { icon: KeyRound, text: "API keys bound to a team, the same for the CLI and the REST API" },
  { icon: GitBranch, text: "Push to deploy from GitHub, or deploy from a URL, a zip or an image" },
  { icon: ScrollText, text: "Follow a build's logs from the terminal with --follow" },
];

/** Strings, URLs, placeholders, flags, comments and a few keywords; a comment starts at line start or after a space, so `https://` is a URL. */
const TOKEN = /"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|`(?:[^`\\]|\\.)*`|(?<=^|\s)(?:#|\/\/).*$|https?:\/\/[^\s\\]+|<[a-z-]+>|(?<![\w-])--?[a-zA-Z][\w-]*|\b(?:const|await|import|True|true)\b/g;

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
    const cls = /^(#|\/\/)/.test(t) ? "lp-dim" : /^(["'`]|https?:|<)/.test(t) ? "lp-str" : "lp-key";
    out.push(
      <span key={at} className={cls}>
        {t}
      </span>,
    );
    last = at + t.length;
  }
  if (last < line.length) out.push(line.slice(last));
  return out;
}

export function Developer() {
  const [tab, setTab] = useState(0);
  const [copied, setCopied] = useState(false);
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
    try {
      await navigator.clipboard.writeText(active.raw.replace(/^\$ /gm, ""));
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1800);
    } catch {
      setCopied(false);
    }
  };
  return (
    <Section className="pt-28 sm:pt-36" wash="violet">
      <div className="grid grid-cols-1 items-center gap-12 lg:grid-cols-12 lg:gap-14">
        <div className="min-w-0 lg:col-span-5">
          <SectionHead eyebrow="For developers and agents" title="One API for you, your CI and your agents." lead="The CLI and the REST API use the same API keys, so a script, a CI job or an agent deploys the same way you do." />
          <ul className="lp-reveal mt-8 space-y-4" style={delay(120)}>
            {POINTS.map(({ icon: Icon, text }) => (
              <li key={text} className="flex items-start gap-3.5 text-[15px] leading-relaxed text-zinc-300">
                <span className="mt-0.5 flex h-8 w-8 shrink-0 items-center justify-center rounded-lg border border-white/10 bg-white/[0.05]">
                  <Icon className="h-4 w-4 text-indigo-300" />
                </span>
                {text}
              </li>
            ))}
          </ul>
          <Link href="/docs/getting-started" className="lp-reveal group mt-6 inline-flex items-center gap-2 py-3 text-[15px] text-white" style={delay(200)}>
            <span className="underline decoration-white/30 underline-offset-[6px] transition-colors group-hover:decoration-white">Getting started guide</span>
            <ArrowRight className="h-4 w-4 transition-transform group-hover:translate-x-1" />
          </Link>
        </div>

        <div className="lp-reveal min-w-0 lg:col-span-7" style={delay(100)}>
          <div className="lp-glass overflow-hidden">
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
                    aria-controls="lp-dev-panel"
                    tabIndex={i === tab ? 0 : -1}
                    onClick={() => setTab(i)}
                    className={`lp-mono relative inline-flex h-[44px] items-center whitespace-nowrap px-3.5 text-[12px] uppercase tracking-[0.14em] transition-colors ${i === tab ? "text-white" : "text-[var(--lp-dim)] hover:text-zinc-200"}`}
                  >
                    {t.label}
                    {i === tab && <span aria-hidden className="absolute inset-x-2.5 -bottom-px h-px bg-gradient-to-r from-teal-300 via-indigo-300 to-fuchsia-300" />}
                  </button>
                ))}
              </div>
              <button
                type="button"
                onClick={copy}
                aria-label={copied ? "Copied" : `Copy the ${active.label} example`}
                className="ml-auto inline-flex h-[44px] min-w-[44px] shrink-0 items-center justify-center gap-2 rounded-md px-2.5 text-[12px] text-[var(--lp-mute)] transition-colors hover:bg-white/[0.07] hover:text-white"
              >
                {copied ? <Check className="h-3.5 w-3.5 text-emerald-300" /> : <Copy className="h-3.5 w-3.5" />}
                <span className="hidden sm:inline">{copied ? "Copied" : "Copy"}</span>
              </button>
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
