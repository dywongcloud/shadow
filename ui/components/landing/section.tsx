import { delay } from "./lp-utils";

/** The page's one section frame: a 1200 px column with the shared gutter; `wash` lays a full-bleed gradient of light behind it. */
export function Section({ id, className = "", wash, children }: { id?: string; className?: string; wash?: "violet" | "cyan" | "duo"; children: React.ReactNode }) {
  return (
    <section id={id} className={`relative mx-auto w-full max-w-[1200px] px-6 ${className}`}>
      {wash && <div aria-hidden="true" className="lp-wash" data-tone={wash} />}
      {children}
    </section>
  );
}

/** Eyebrow, headline and lead for a section; it reveals as one unit. */
export function SectionHead({ eyebrow, title, lead, center = false }: { eyebrow: string; title: React.ReactNode; lead?: React.ReactNode; center?: boolean }) {
  return (
    <div className={`lp-reveal max-w-[760px] ${center ? "mx-auto text-center" : ""}`} style={delay(0)}>
      <p className="lp-eyebrow lp-mono">{eyebrow}</p>
      <h2 className="lp-h2 mt-4">{title}</h2>
      {lead && <p className={`lp-lead mt-5 max-w-[620px] ${center ? "mx-auto" : ""}`}>{lead}</p>}
    </div>
  );
}
