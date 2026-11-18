"use client";

import { useEffect, useRef, useState } from "react";
import { Check, Copy } from "lucide-react";
import { Section, SectionHead } from "./section";
import { TIPS_ADDRESS, TIPS_NETWORKS } from "@/lib/tips";

type CopyState = "idle" | "copied" | "manual";

/** One address, eight networks. The clipboard is optional: when it is refused
 *  the address is selected instead, and the visible status says what happened.
 *  No alert, no URL built from the address, no copy of it stored anywhere. */
function AddressCopy() {
  const [state, setState] = useState<CopyState>("idle");
  const addressRef = useRef<HTMLElement>(null);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  useEffect(() => () => { if (timer.current) clearTimeout(timer.current); }, []);

  async function copy() {
    if (timer.current) clearTimeout(timer.current);
    try {
      await navigator.clipboard.writeText(TIPS_ADDRESS);
      setState("copied");
    } catch {
      const node = addressRef.current;
      if (node) {
        const range = document.createRange();
        range.selectNodeContents(node);
        const selection = window.getSelection();
        selection?.removeAllRanges();
        selection?.addRange(range);
      }
      setState("manual");
    }
    timer.current = setTimeout(() => setState("idle"), 4000);
  }

  return (
    <div className="lp-tips-well lp-well mt-8 p-5 sm:p-7">
      <div className="flex flex-col gap-4 sm:flex-row sm:items-start sm:justify-between">
        <div className="min-w-0 flex-1">
          <p className="lp-eyebrow lp-mono">Address</p>
          <code ref={addressRef} className="lp-tips-address select-all">{TIPS_ADDRESS}</code>
        </div>
        <button
          type="button"
          onClick={copy}
          className="tap-44 inline-flex min-h-[44px] shrink-0 items-center justify-center gap-2 rounded-full border border-white/20 bg-white/5 px-5 text-[13px] font-medium text-white transition-colors hover:border-white/40 hover:bg-white/10"
        >
          {state === "copied" ? <Check className="h-4 w-4" aria-hidden="true" /> : <Copy className="h-4 w-4" aria-hidden="true" />}
          {state === "copied" ? "Copied" : "Copy address"}
        </button>
      </div>
      <p className="lp-tips-status" role="status" aria-live="polite">
        {state === "copied" ? "Address copied to the clipboard." : state === "manual" ? "Clipboard unavailable. The address is selected; copy it with your keyboard." : ""}
      </p>
    </div>
  );
}

/** The network list and address, without a heading: the landing section and
 *  the /tips page each supply their own h2/h1. */
export function TipsPanel() {
  return (
    <div className="lp-tips">
      <ul className="lp-tips-networks mt-8 grid grid-cols-2 gap-2 sm:grid-cols-4">
        {TIPS_NETWORKS.map((name) => (
          <li key={name} className="lp-chip min-w-0 justify-center">{name}</li>
        ))}
      </ul>
      <AddressCopy />
      <p className="lp-caption mt-5 text-[13px] leading-relaxed text-zinc-400">
        Use this address only on the networks listed. Choose the right network in your wallet before you send.
      </p>
    </div>
  );
}

/** Quiet, footer-adjacent section on the landing page. */
export function TipsSection() {
  return (
    <Section id="tips" className="pb-24 pt-10 sm:pb-28">
      <SectionHead
        eyebrow="Support"
        title="If shadw is useful, leave a tip."
        lead="One address is listed for the networks below. Tips are optional."
      />
      <div className="lp-reveal"><TipsPanel /></div>
    </Section>
  );
}
