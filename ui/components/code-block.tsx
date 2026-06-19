"use client";

import { useState } from "react";
import { Copy, Check } from "lucide-react";

/* Lightweight, dependency-free, theme-aware syntax highlighter + code panel
 * (filename header, language tabs, copy button). Good enough for the snippets
 * shown in the docs (ts/js, python, bash/curl, json). Colors use theme tokens so
 * it reads well in light and dark. */

type Lang = "ts" | "tsx" | "js" | "python" | "bash" | "json" | "text";

const KEYWORDS: Record<string, Set<string>> = {
  ts: new Set("import export from const let var function return await async if else new type interface extends implements class default of in for while do switch case break continue typeof instanceof try catch finally throw yield".split(" ")),
  python: new Set("import from def class return await async if elif else for while with as try except finally raise yield lambda pass not and or is in None True False".split(" ")),
  bash: new Set("curl cd export sudo cargo npm npx git node".split(" ")),
  json: new Set("true false null".split(" ")),
};
KEYWORDS.tsx = KEYWORDS.ts;
KEYWORDS.js = KEYWORDS.ts;

const cls = {
  comment: "text-zinc-500",
  string: "text-emerald-600 dark:text-emerald-400",
  number: "text-amber-600 dark:text-amber-400",
  keyword: "text-fuchsia-600 dark:text-fuchsia-400",
  type: "text-sky-600 dark:text-sky-400",
  fn: "text-violet-600 dark:text-violet-400",
  flag: "text-amber-600 dark:text-amber-400",
};

const TOKEN = /(\/\/[^\n]*|#[^\n]*)|("(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|`(?:[^`\\]|\\.)*`)|(\b\d[\d._]*\b)|(--?[A-Za-z][\w-]*)|([A-Za-z_$][\w$]*)/g;

function highlight(code: string, lang: Lang): React.ReactNode {
  const kw = KEYWORDS[lang] ?? new Set<string>();
  const out: React.ReactNode[] = [];
  let last = 0;
  let m: RegExpExecArray | null;
  let i = 0;
  TOKEN.lastIndex = 0;
  while ((m = TOKEN.exec(code))) {
    if (m.index > last) out.push(code.slice(last, m.index));
    const [whole, comment, str, num, flag, word] = m;
    if (comment) out.push(<span key={i++} className={cls.comment}>{comment}</span>);
    else if (str) out.push(<span key={i++} className={cls.string}>{str}</span>);
    else if (num) out.push(<span key={i++} className={cls.number}>{num}</span>);
    else if (flag) out.push(<span key={i++} className={cls.flag}>{flag}</span>);
    else if (word) {
      const after = code[m.index + whole.length];
      let c: string | null = null;
      if (kw.has(word)) c = cls.keyword;
      else if (after === "(") c = cls.fn;
      else if (/^[A-Z]/.test(word) && lang !== "bash") c = cls.type;
      out.push(c ? <span key={i++} className={c}>{word}</span> : word);
    } else out.push(whole);
    last = m.index + whole.length;
  }
  if (last < code.length) out.push(code.slice(last));
  return out;
}

export interface Tab { label: string; lang: Lang; filename?: string; code: string }

export function CodeBlock({ tabs, className = "" }: { tabs: Tab[]; className?: string }) {
  const [active, setActive] = useState(0);
  const [copied, setCopied] = useState(false);
  const tab = tabs[active] ?? tabs[0];

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(tab.code);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {/* ignore */}
  };

  return (
    <div className={`overflow-hidden rounded-xl border border-border bg-card ${className}`}>
      {/* tabs (only when >1) */}
      {tabs.length > 1 && (
        <div className="flex items-center gap-1 border-b border-border px-2 pt-1.5">
          {tabs.map((t, i) => (
            <button
              key={t.label}
              onClick={() => setActive(i)}
              className={`relative rounded-md px-3 py-1.5 text-[13px] transition-colors ${
                i === active ? "text-fg" : "text-secondary hover:text-fg"
              }`}
            >
              {t.label}
              {i === active && <span className="absolute inset-x-2 -bottom-[7px] h-0.5 rounded-full bg-fg" />}
            </button>
          ))}
        </div>
      )}
      {/* filename header + copy */}
      <div className="flex items-center justify-between border-b border-border px-4 py-2">
        <span className="flex items-center gap-2 font-mono text-xs text-secondary">
          {tab.filename && (
            <span className="rounded bg-subtle px-1.5 py-0.5 text-[10px] font-semibold uppercase text-muted">{tab.lang}</span>
          )}
          {tab.filename ?? tab.lang}
        </span>
        <button onClick={copy} className="text-muted transition-colors hover:text-fg" aria-label="Copy code">
          {copied ? <Check className="h-3.5 w-3.5 text-emerald-500" /> : <Copy className="h-3.5 w-3.5" />}
        </button>
      </div>
      {/* code */}
      <pre className="no-scrollbar overflow-x-auto p-4 font-mono text-[13px] leading-relaxed text-fg">
        <code>{highlight(tab.code, tab.lang)}</code>
      </pre>
    </div>
  );
}
