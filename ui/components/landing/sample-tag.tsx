/** The first row of an illustration that shows sample states or values. It sits in the flow, above the content, so it can never lie on top of it; its margin keeps the content a little clear of it at any text size. */
export function SampleTag() {
  return <span className="lp-chip mb-2 shrink-0 self-end !text-[11px] uppercase tracking-[0.12em] text-[var(--lp-dim)]">Sample</span>;
}
