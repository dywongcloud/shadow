import { EyeOff, Lock, ShieldCheck, Users } from "lucide-react";
import { Section, SectionHead } from "./section";
import { delay } from "./lp-utils";
import { Tok } from "./tok";

/** Each tile restates a fact the repository records: previews reachable only by the team by default (`project_settings.rs`, `preview_protection`), the
 *  ed25519-signed deployment integrity chain (`integrity_signer.rs`), secrets sealed with ChaCha20-Poly1305 (`secrets.rs`) and the owner, admin, member and
 *  viewer roles (`teams.rs`, `roles.rs`). */
const FACTS = [
  { icon: EyeOff, title: "Private previews", body: "Previews are reachable only by your team by default." },
  { icon: ShieldCheck, title: "Signed deployments", body: "Each deployment carries an integrity chain signed with a node's ed25519 key." },
  { icon: Lock, title: "Encrypted secrets", body: <>Sensitive variables are sealed with <Tok>ChaCha20-Poly1305</Tok> before they touch disk.</> },
  { icon: Users, title: "Team roles", body: "Teams have owner, admin, member and viewer roles." },
];

export function UnderTheHood() {
  return (
    <Section className="pt-28 sm:pt-36" wash="cyan">
      <SectionHead eyebrow="Under the hood" title="Built like infrastructure." lead="How deployments are signed, secrets are sealed and access is gated." />
      <ul className="lp-limb-top relative mt-14 grid gap-px overflow-hidden rounded-3xl border border-white/10 bg-white/10 sm:grid-cols-2 lg:grid-cols-4">
        {FACTS.map(({ icon: Icon, title, body }, i) => (
          <li key={title} className="lp-reveal bg-[#0d0e13] p-7 transition-colors duration-300 hover:bg-[#12131a]" style={delay(i * 80)}>
            <span className="flex h-11 w-11 items-center justify-center rounded-xl border border-white/10 bg-gradient-to-br from-indigo-400/20 to-cyan-300/10">
              <Icon className="h-5 w-5 text-indigo-200" />
            </span>
            <h3 className="lp-card-title mt-5 !text-[19px]">{title}</h3>
            <p className="lp-card-copy mt-2.5">{body}</p>
          </li>
        ))}
      </ul>
    </Section>
  );
}
