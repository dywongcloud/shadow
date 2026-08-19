"use client";

import { useEffect, useMemo, useState } from "react";
import { X, Copy, Check, Loader2, ArrowLeft, ArrowRight, ShieldCheck, AlertTriangle, Globe2 } from "lucide-react";
import { Card, Button, Badge } from "@/components/ui";
import { toast } from "@/components/toast";
import { copyText } from "@/lib/utils";
import {
  apiGet, attachDomain, verifyDomainNow, fetchNsRoster,
  type NsRoster, type DomainDetail, type DomainAttachResult,
} from "@/lib/api";
import { deploymentHost } from "@/lib/deploy-url";

/**
 * The custom-domain migration wizard — a guided, step-by-step flow for
 * attaching a tenant-owned domain to a project and (optionally) migrating it
 * off another registrar's DNS. The flow is deliberately honest about the
 * three genuinely different moves, which every registrar doc names but
 * users conflate constantly:
 *   A. POINT RECORDS — the domain keeps its current DNS; the user adds
 *      records (A at apex / CNAME for subdomains) plus our ownership TXT.
 *   B. DELEGATE DNS — the user changes nameservers to ours; we serve the
 *      zone (record import offered first), manage certs, verify via TXT.
 *   C. TRANSFER REGISTRATION — billing/renewal moves registrars (5–7 days,
 *      60-day ICANN rules). We are not a registrar: this step is guidance,
 *      never a promise we execute it.
 */

type Path = "records" | "delegate" | "transfer";
type Step = "domain" | "path" | "preflight" | "registrar" | "verify" | "done";

const REGISTRARS = ["Namecheap", "GoDaddy", "Route 53", "Vercel Domains", "Other"] as const;
type Registrar = (typeof REGISTRARS)[number];

interface ScanRecord { name: string; type: string; value: string; ttl: number; priority: number | null }

/** Registrar click-paths. Only called once the platform nameserver set is
 *  ready, so `ns` always holds real hostnames — never placeholder "ns1"/"ns2". */
function registrarNsCopy(reg: Registrar, ns: string[]): string {
  const a = ns[0] ?? "";
  const b = ns[1] ?? a;
  switch (reg) {
    case "Namecheap":
      return `Domain List → Manage → Nameservers section → choose "Custom DNS" from the dropdown → enter ${a} and ${b} (names only, no IP addresses) → click the green checkmark. Existing host records do NOT carry over — that's why the import step matters.`;
    case "GoDaddy":
      return `Domain Portfolio → your domain → DNS → Nameservers → "I'll use my own nameservers" → enter ${a} and ${b} → Save → Continue. If prompted for a code, that's GoDaddy's Domain Protection identity check.`;
    case "Route 53":
      return `Route 53 console → Registered domains → your domain → Actions → Edit name servers → replace the four awsdns entries with ${a} and ${b} → Update. Do NOT delete the old hosted zone yet — keep it ~2 days until resolvers stop using the old nameservers, or the domain goes dark.`;
    case "Vercel Domains":
      return `Vercel dashboard → Domains → your domain → add ${a} and ${b} as custom nameservers (up to 4, revertable anytime).`;
    default:
      return `At your DNS provider, set the domain's nameservers to ${a} and ${b}. Nameserver changes take minutes to 48h to propagate.`;
  }
}

function registrarTransferCopy(reg: Registrar): string {
  switch (reg) {
    case "Namecheap":
      return `Manage → Sharing & Transfer → Transfer Out: unlock the domain, then request the Auth Code — it's emailed to the REGISTRANT email (may differ from your account email). Namecheap can hold the transfer up to 5 days; there is no fast-approve.`;
    case "GoDaddy":
      return `Domain Portfolio → your domain → Transfer → Transfer to Another Registrar. First complete the checklist: unlock, turn OFF Domain Privacy, downgrade Domain Protection to none. Copy the Authorization Code. After the transfer starts, watch for GoDaddy's approval email — approving it cuts the wait from 5–7 days to minutes.`;
    case "Route 53":
      return `Registered domains → your domain → Actions → Turn off transfer lock → Transfer out → Transfer to another registrar → Copy the auth code. The confirmation email comes from registrar.amazon or domainnameverification.net — it's legitimate. Ignoring it lets the transfer proceed automatically.`;
    case "Vercel Domains":
      return `Vercel dashboard → Domains → the ⋯ menu on the domain (Team Owner only) → Transfer out → copy the authorization code from the modal. No further confirmation needed on Vercel's side; allow up to a week.`;
    default:
      return `At your current registrar: unlock the domain, request the authorization (EPP) code, and start the transfer at the receiving registrar. ICANN rules: the domain must be 60+ days old, not transferred in the last 60 days, and gTLD transfers add one year to the registration.`;
  }
}

export function DomainWizard({
  projects,
  defaultProject,
  onClose,
}: {
  projects: string[];
  defaultProject?: string;
  onClose: () => void;
}) {
  const [step, setStep] = useState<Step>("domain");
  const [domain, setDomain] = useState("");
  const [project, setProject] = useState(defaultProject ?? projects[0] ?? "");
  const [path, setPath] = useState<Path>("records");
  const [registrar, setRegistrar] = useState<Registrar>("Namecheap");
  const [roster, setRoster] = useState<NsRoster | null>(null);
  const [scan, setScan] = useState<ScanRecord[] | null>(null);
  const [scanning, setScanning] = useState(false);
  const [attach, setAttach] = useState<DomainAttachResult | null>(null);
  const [busy, setBusy] = useState(false);
  const [copied, setCopied] = useState("");
  const clean = domain.trim().toLowerCase().replace(/\.$/, "");
  const isApex = clean.split(".").length === 2;

  useEffect(() => {
    fetchNsRoster().then(setRoster).catch(() => setRoster(null));
  }, []);

  // The domain detail is only worth fetching during the verify step, so gate
  // it client-side — no placeholder request ever fires for a junk name.
  const [detail, setDetail] = useState<DomainDetail | null>(null);
  function refreshDetail() {
    if (!clean) return;
    apiGet<DomainDetail>(`/v1/domains/${encodeURIComponent(clean)}`).then(setDetail).catch(() => {});
  }
  useEffect(() => {
    if (step !== "verify" || !clean) return;
    let stop = false;
    const load = () => apiGet<DomainDetail>(`/v1/domains/${encodeURIComponent(clean)}`).then((d) => { if (!stop) setDetail(d); }).catch(() => {});
    load();
    const t = setInterval(load, 5000);
    return () => { stop = true; clearInterval(t); };
  }, [step, clean]);
  const verifyStatus = detail?.domain?.verify?.status ?? attach?.status ?? "pending";

  useEffect(() => {
    if (step === "verify" && verifyStatus === "verified") {
      const t = setTimeout(() => setStep("done"), 1200);
      return () => clearTimeout(t);
    }
  }, [step, verifyStatus]);

  async function copy(key: string, value: string) {
    if (await copyText(value)) {
      setCopied(key);
      setTimeout(() => setCopied(""), 1200);
      toast(`Copied ${key}`, { tone: "blue" });
    }
  }

  async function runScan() {
    setScanning(true);
    try {
      const r = await apiGet<{ records: ScanRecord[] }>(`/v1/domains/${encodeURIComponent(clean)}/scan`, { fresh: true });
      setScan(r.records ?? []);
    } catch (e) {
      toast(`Scan failed: ${String(e).replace(/^Error:\s*/, "")}`, {});
      setScan([]);
    } finally {
      setScanning(false);
    }
  }

  async function doAttach() {
    if (!project) {
      toast("Pick a project first", {});
      return;
    }
    setBusy(true);
    try {
      const r = await attachDomain(project, clean);
      setAttach(r);
      setStep("verify");
      refreshDetail();
    } catch (e) {
      toast(`Couldn't attach: ${String(e).replace(/^Error:\s*/, "")}`, {});
    } finally {
      setBusy(false);
    }
  }

  async function reVerify() {
    setBusy(true);
    try {
      const r = await verifyDomainNow(clean);
      setAttach(r);
      refreshDetail();
      if (r.status !== "verified") {
        toast(`Not verified yet — ${r.probe ?? "DNS still propagating"}`, {});
      }
    } catch (e) {
      toast(`Verify failed: ${String(e).replace(/^Error:\s*/, "")}`, {});
    } finally {
      setBusy(false);
    }
  }

  const ns = roster?.nameservers ?? [];
  const edgeV4 = roster?.edge_ipv4 ?? [];
  const edgeV6 = roster?.edge_ipv6 ?? [];
  const verify = detail?.domain?.verify ?? attach?.verify;
  // Never print nameserver hostnames until the roster says the set has
  // converged — anything earlier would be a guess presented as fact.
  const nsReady = roster?.delegation_ready === true && ns.length > 0;
  // The CNAME target is the project's real deployment host on the apps
  // domain, resolved per-session — never a hardcoded domain string.
  const cnameTarget = project ? deploymentHost(`${project}.localhost`) : "";

  return (
    <div className="fixed inset-0 z-50 flex items-start justify-center overflow-y-auto bg-black/60 p-4 backdrop-blur-sm sm:items-center">
      <Card className="my-8 w-full max-w-2xl p-0">
        <div className="flex items-center justify-between border-b border-border px-5 py-4">
          <div className="flex items-center gap-2">
            <Globe2 className="h-4 w-4 text-muted" />
            <h2 className="text-sm font-semibold">Set up a custom domain</h2>
            <Badge tone="default">{step}</Badge>
          </div>
          <button onClick={onClose} className="text-muted hover:text-fg" title="Close"><X className="h-4 w-4" /></button>
        </div>

        <div className="flex flex-col gap-4 px-5 py-4 text-sm">
          {step === "domain" && (
            <>
              <p className="text-secondary">
                Enter the domain you want on <strong className="text-fg">{project || "your project"}</strong>. We'll
                check its current DNS, then walk you through exactly what to change — at your registrar, with real
                steps for where it lives today.
              </p>
              <label className="flex flex-col gap-1">
                <span className="text-xs font-medium uppercase tracking-wide text-muted">Domain</span>
                <input
                  value={domain}
                  onChange={(e) => setDomain(e.target.value)}
                  placeholder="numo.gg or app.numo.gg"
                  className="rounded-md border border-border bg-subtle/50 px-3 py-2 font-mono text-sm text-fg outline-none focus:border-fg/40"
                  autoFocus
                />
              </label>
              <label className="flex flex-col gap-1">
                <span className="text-xs font-medium uppercase tracking-wide text-muted">Project</span>
                <select
                  value={project}
                  onChange={(e) => setProject(e.target.value)}
                  className="rounded-md border border-border bg-subtle/50 px-3 py-2 text-sm text-fg outline-none"
                >
                  {projects.map((p) => <option key={p} value={p}>{p}</option>)}
                </select>
              </label>
            </>
          )}

          {step === "path" && (
            <>
              <p className="text-secondary">
                <strong className="text-fg">{clean}</strong> — pick how you want to run its DNS. All three work with
                HTTPS managed for you; they differ in who answers DNS for the domain.
              </p>
              <div className="grid gap-2">
                <PathCard
                  active={path === "records"}
                  onClick={() => setPath("records")}
                  title="Point DNS records — fastest"
                  body="Keep your current registrar and DNS. Add one or two records (an A record or CNAME for this name) plus a short ownership TXT. Your email and other DNS records stay untouched — only the records for this name point at us. You manage DNS in two places from now on."
                />
                <PathCard
                  active={path === "delegate"}
                  onClick={() => setPath("delegate")}
                  title="Delegate DNS to us — recommended for apex + wildcards"
                  body="Change 2–4 nameservers at your registrar. We serve your whole zone and manage records here — the records your project needs are created automatically once ownership verifies. Registration and renewal stay at your current registrar. We import the records we can detect first."
                />
                <PathCard
                  active={path === "transfer"}
                  onClick={() => setPath("transfer")}
                  title="Transfer registration — optional, slow"
                  body="Move billing and renewal to another registrar of your choice. Takes 5–7 days, needs the domain 60+ days old and unlocked, adds a year to the registration. Not required for hosting: do 'Delegate DNS' now and this later if you want it."
                />
              </div>
            </>
          )}

          {step === "preflight" && (
            <>
              <div className="flex items-start gap-2 rounded-md border border-amber-500/40 bg-amber-500/5 p-3 text-xs">
                <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0 text-amber-500" />
                <p className="text-secondary">
                  Changing nameservers moves where DNS is managed. <strong className="text-fg">Email and verification
                  records that exist only at your old provider will stop resolving.</strong> Review what we can see
                  publicly, and re-create anything you still need here after the switch. If DNSSEC is enabled at your
                  registrar, disable it first and wait ~24h before continuing.
                </p>
              </div>
              <div className="flex items-center gap-2">
                <Button variant="outline" onClick={runScan} disabled={scanning}>
                  {scanning ? <Loader2 className="h-4 w-4 animate-spin" /> : <RefreshCwIcon />}
                  {scan ? "Re-scan public DNS" : "Scan public DNS now"}
                </Button>
                {scan && <span className="text-xs text-muted">{scan.length} record{scan.length === 1 ? "" : "s"} visible</span>}
              </div>
              {scan && (
                <div className="max-h-56 overflow-auto rounded-md border border-border">
                  {scan.length === 0 && <p className="p-3 text-xs text-muted">Nothing publicly visible (or the domain doesn't resolve yet).</p>}
                  {scan.map((r, i) => (
                    <div key={i} className="flex items-center gap-3 border-b border-border px-3 py-1.5 font-mono text-xs last:border-0">
                      <span className="w-14 shrink-0 text-muted">{r.type}</span>
                      <span className="w-32 shrink-0 truncate text-secondary">{r.name || "@"}</span>
                      <span className="truncate text-fg">{r.priority != null ? `${r.priority} ${r.value}` : r.value}</span>
                    </div>
                  ))}
                </div>
              )}
            </>
          )}

          {step === "registrar" && (
            <>
              <p className="text-secondary">
                {path === "records"
                  ? <>Add these records where <strong className="text-fg">{clean}</strong>'s DNS is managed today:</>
                  : <>Change the nameservers for <strong className="text-fg">{clean}</strong> at <strong className="text-fg">{registrar}</strong>:</>}
              </p>
              <div className="flex flex-wrap gap-1.5">
                {REGISTRARS.map((r) => (
                  <button
                    key={r}
                    onClick={() => setRegistrar(r)}
                    className={`rounded-full border px-3 py-1 text-xs ${registrar === r ? "border-fg/60 bg-fg/10 text-fg" : "border-border text-secondary hover:text-fg"}`}
                  >
                    {r}
                  </button>
                ))}
              </div>

              {path === "records" && (
                <div className="flex flex-col gap-2">
                  {isApex ? (
                    <RecordRow
                      label="A"
                      name="@"
                      value={edgeV4.join("  ·  ") || "loading…"}
                      hint="One A record per address if your provider allows it, else the first one."
                      onCopy={() => copy("A record", edgeV4.join("\n"))}
                      copied={copied === "A record"}
                    />
                  ) : (
                    <RecordRow
                      label="CNAME"
                      name={clean.split(".")[0]}
                      value={cnameTarget || "…"}
                      hint="Subdomains point at the project alias."
                      onCopy={() => copy("CNAME", cnameTarget)}
                      copied={copied === "CNAME"}
                    />
                  )}
                  {edgeV6.length > 0 && (
                    <p className="text-[11px] text-muted">The fleet has IPv6 edge nodes — AAAA records are served automatically, so no AAAA record is needed from you.</p>
                  )}
                  <p className="text-xs text-secondary">
                    On the next step we'll also give you a short TXT record to prove ownership — required before the
                    domain goes live here.
                  </p>
                </div>
              )}

              {path !== "records" && (
                <div className="flex flex-col gap-2">
                  {nsReady ? (
                    <>
                      <RecordRow
                        label="NS"
                        name="@"
                        value={ns.join("  ·  ")}
                        hint="Set ALL of the listed nameservers."
                        onCopy={() => copy("nameservers", ns.join("\n"))}
                        copied={copied === "nameservers"}
                      />
                      <p className="text-xs text-secondary">{registrarNsCopy(registrar, ns)}</p>
                    </>
                  ) : (
                    <p className="rounded-md border border-border bg-subtle/30 p-3 text-xs text-secondary">
                      The platform nameserver set is still converging — check back in a minute.
                    </p>
                  )}
                </div>
              )}

              {path === "transfer" && (
                <div className="rounded-md border border-border bg-subtle/30 p-3 text-xs text-secondary">
                  <strong className="text-fg">Transferring registration ({registrar}):</strong>{" "}
                  {registrarTransferCopy(registrar)}
                  <div className="mt-2">
                    This is a registrar change, not a DNS change — hosting works today via the two paths above while
                    the transfer completes in the background (5–7 days typical).
                  </div>
                </div>
              )}
            </>
          )}

          {step === "verify" && (
            <>
              <div className="flex items-center gap-2">
                {verifyStatus === "verified"
                  ? <Badge tone="green"><ShieldCheck className="h-3.5 w-3.5" /> Ownership verified</Badge>
                  : <Badge tone="amber"><Loader2 className="h-3.5 w-3.5 animate-spin" /> Waiting for the ownership TXT</Badge>}
              </div>
              {verifyStatus !== "verified" && verify && (
                <>
                  <p className="text-secondary">
                    Last piece: prove <strong className="text-fg">{clean}</strong> is yours. Add this TXT record at
                    your DNS provider — verification completes automatically (we check continuously).
                  </p>
                  <RecordRow
                    label="TXT"
                    name={verify.txt_name}
                    value={verify.txt_value}
                    hint="Exact name and value, both required."
                    onCopy={() => copy("verify-txt", `${verify.txt_name}  ${verify.txt_value}`)}
                    copied={copied === "verify-txt"}
                  />
                  <div className="flex items-center gap-3 text-xs text-muted">
                    <span>Last check: {verify.last_probe || "not yet"}</span>
                    <Button variant="outline" onClick={reVerify} disabled={busy}>
                      {busy ? <Loader2 className="h-4 w-4 animate-spin" /> : null}
                      Verify now
                    </Button>
                  </div>
                  <p className="text-[11px] text-muted">
                    DNS changes can take up to 48h. To check a change immediately, flush your resolver:
                    developers.google.com/speed/public-dns/cache or 1.1.1.1/purge-cache.
                  </p>
                </>
              )}
            </>
          )}

          {step === "done" && (
            <div className="flex flex-col items-start gap-2 py-4">
              <Badge tone="green"><ShieldCheck className="h-3.5 w-3.5" /> {clean} is attached to {project}</Badge>
              <p className="text-sm text-secondary">
                Ownership is verified — DNS and the TLS certificate take a few minutes to propagate, so the domain
                may not answer immediately. {path === "records"
                  ? "Once your records resolve, the domain answers from the platform edge."
                  : "Once the nameserver change propagates, your zone is served here and the records above answer."}
                {" "}You can watch it on the domain's page.
              </p>
              <Button variant="outline" onClick={onClose}>Open the domain page</Button>
            </div>
          )}
        </div>

        {/* Footer nav */}
        {step !== "done" && (
          <div className="flex items-center justify-between border-t border-border px-5 py-3">
            <Button
              variant="outline"
              onClick={() => {
                const order: Step[] = path === "records"
                  ? ["domain", "path", "registrar", "verify"]
                  : ["domain", "path", "preflight", "registrar", "verify"];
                const i = order.indexOf(step);
                setStep(order[Math.max(0, i - 1)]);
              }}
              disabled={step === "domain"}
            >
              <ArrowLeft className="h-4 w-4" /> Back
            </Button>
            {step !== "verify" ? (
              <Button
                onClick={() => {
                  if (step === "domain") {
                    if (!clean.includes(".") || clean.startsWith(".") || clean.endsWith(".")) {
                      toast("Enter a valid domain first", {});
                      return;
                    }
                    setStep("path");
                  } else if (step === "path") {
                    setStep(path === "records" ? "registrar" : "preflight");
                  } else if (step === "preflight") {
                    setStep("registrar");
                  } else if (step === "registrar") {
                    doAttach();
                  }
                }}
                disabled={busy}
              >
                {step === "registrar" ? (busy ? "Attaching…" : "Attach domain") : <>Next <ArrowRight className="h-4 w-4" /></>}
              </Button>
            ) : null}
          </div>
        )}
      </Card>
    </div>
  );
}

function RefreshCwIcon() {
  return <Loader2 className="h-4 w-4" />;
}

function PathCard({ active, onClick, title, body }: { active: boolean; onClick: () => void; title: string; body: string }) {
  return (
    <button
      onClick={onClick}
      className={`rounded-lg border p-3 text-left transition-colors ${active ? "border-fg/60 bg-fg/5" : "border-border hover:border-fg/30"}`}
    >
      <div className="text-sm font-medium text-fg">{title}</div>
      <div className="mt-1 text-xs text-secondary">{body}</div>
    </button>
  );
}

function RecordRow({
  label, name, value, hint, onCopy, copied,
}: {
  label: string; name: string; value: string; hint: string; onCopy: () => void; copied: boolean;
}) {
  return (
    <div className="flex items-start gap-3 rounded-md border border-border bg-subtle/30 p-3">
      <div className="w-14 shrink-0 pt-0.5 font-mono text-xs text-muted">{label}</div>
      <div className="min-w-0 flex-1">
        <div className="font-mono text-xs text-secondary">{name}</div>
        <div className="break-all font-mono text-xs text-fg">{value}</div>
        <div className="mt-0.5 text-[11px] text-muted">{hint}</div>
      </div>
      <button onClick={onCopy} className="shrink-0 text-muted hover:text-fg" title="Copy">
        {copied ? <Check className="h-3.5 w-3.5 text-green" /> : <Copy className="h-3.5 w-3.5" />}
      </button>
    </div>
  );
}
