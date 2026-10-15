import { Fingerprint, KeyRound, Network, ShieldCheck } from "lucide-react";
import { Section, SectionHead } from "./section";
import { delay } from "./lp-utils";
import { Tok } from "./tok";

/** Each tile restates a fact the repository records: the iroh QUIC transport, the hybrid X25519MLKEM768 group (CHANGELOG 2026-09-22; the browser
 *  client cannot speak it, so the tile says "mesh links"), BLAKE3 content identity and ed25519 node identity (AGENTS.md). */
const FACTS = [
  { icon: Network, title: "QUIC mesh", body: "Nodes connect peer to peer over QUIC, built on iroh." },
  { icon: ShieldCheck, title: "Post-quantum key exchange", body: <>Mesh links offer a hybrid X25519 + <Tok>ML-KEM-768</Tok> handshake.</> },
  { icon: Fingerprint, title: "Content-addressed builds", body: "New artifacts are identified by their BLAKE3 hash." },
  { icon: KeyRound, title: "Signed node identity", body: "Every node has an ed25519 identity, and deployments carry a signed integrity chain." },
];

export function UnderTheHood() {
  return (
    <Section className="pt-28 sm:pt-36" wash="cyan">
      <SectionHead eyebrow="Under the hood" title="Built like infrastructure." lead="The details that make a peer-to-peer cloud safe to run production on." />
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
