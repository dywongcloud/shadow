"use client";

import { useEffect, useRef, useState } from "react";
import Link from "next/link";
import { ArrowLeft, Check, Copy, ShieldCheck, TriangleAlert } from "lucide-react";
import { Badge, Button, Card, Input, PageHeader } from "@/components/ui";
import { ApiError, ROLE_REFUSED, apiGet, apiSend, currentTeam, refreshTwoFactorStepUp } from "@/lib/api";
import { reportError } from "@/components/toast";
import { qrcodegen } from "@/lib/qrcodegen";

/** The contract's enroll/confirm/disable/status shapes, with a tolerant reader for the landed backend:
 *  enroll answers `secret_base32` (or `secret`) and OPTIONALLY `otpauth_uri`; the backend deliberately
 *  emits NO otpauth URL, so the value below is built LOCALLY as QR data, never as a href or attribute. */
interface EnrollResult {
  secret_base32?: string;
  secret?: string;
  otpauth_uri?: string;
  digits?: number;
  period?: number;
  algorithm?: string;
  otp_required?: boolean;
}
interface ConfirmResult { recovery_codes?: string[]; otp_required?: boolean; ok?: boolean }
interface TwoFactorStatus { otp_required?: boolean; enrolled?: boolean }

const ISSUER = "shadw";

/** A provisioning payload for the QR encoder ONLY: it is never a navigation URL, an HTML attribute,
 *  a log line, storage or argv. React renders all account-derived text as text, never as HTML. */
function otpauthFromSecret(secret: string, digits: number, period: number): string {
  const label = encodeURIComponent(`${ISSUER}:${currentTeam()}`);
  return `otpauth://totp/${label}?secret=${encodeURIComponent(secret)}&issuer=${encodeURIComponent(ISSUER)}&algorithm=SHA1&digits=${digits}&period=${period}`;
}

/** Client-side QR: the otpauth value is encoded into modules and painted into a canvas. No URI ever
 *  reaches an `img src`, a URL, or an HTML attribute. Vendored upstream encoder (MIT, ui/lib/qrcodegen.ts). */
function QrCanvas({ value }: { value: string }) {
  const ref = useRef<HTMLCanvasElement | null>(null);
  useEffect(() => {
    const canvas = ref.current;
    if (!canvas) return;
    try {
      const qr = qrcodegen.QrCode.encodeText(value, qrcodegen.QrCode.Ecc.LOW);
      const quiet = 4;
      const scale = 5;
      const side = (qr.size + quiet * 2) * scale;
      canvas.width = side;
      canvas.height = side;
      const ctx = canvas.getContext("2d");
      if (!ctx) return;
      ctx.fillStyle = "#ffffff";
      ctx.fillRect(0, 0, side, side);
      ctx.fillStyle = "#000000";
      for (let y = 0; y < qr.size; y++) {
        for (let x = 0; x < qr.size; x++) {
          if (qr.getModule(x, y)) ctx.fillRect((x + quiet) * scale, (y + quiet) * scale, scale, scale);
        }
      }
    } catch {
      // A URI too long for a QR leaves the base32 secret visible for manual entry.
    }
  }, [value]);
  return <canvas ref={ref} role="img" aria-label="QR code to scan with your authenticator app" className="rounded-md border border-border bg-white p-1" />;
}

export default function TwoFactorPage() {
  const [status, setStatus] = useState<TwoFactorStatus | null>(null);
  const [statusUnavailable, setStatusUnavailable] = useState(false);
  const [secret, setSecret] = useState("");
  const [otpauthUri, setOtpauthUri] = useState("");
  const [digits, setDigits] = useState(6);
  const [period, setPeriod] = useState(30);
  const [enrollCode, setEnrollCode] = useState("");
  const [recoveryCodes, setRecoveryCodes] = useState<string[] | null>(null);
  const [disableCode, setDisableCode] = useState("");
  const [copied, setCopied] = useState<"secret" | "codes" | null>(null);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState("");
  const [note, setNote] = useState("");

  useEffect(() => {
    let mounted = true;
    apiGet<TwoFactorStatus>("/v1/account/2fa/status", { fresh: true })
      .then((s) => { if (mounted) { setStatus(s); setStatusUnavailable(false); } })
      .catch(() => { if (mounted) setStatusUnavailable(true); });
    return () => { mounted = false; };
  }, []);

  function fail(e: unknown) {
    if (e instanceof ApiError && e.code === ROLE_REFUSED) { reportError(e); return; }
    setErr(e instanceof Error ? e.message : String(e));
  }

  async function copy(kind: "secret" | "codes") {
    const text = kind === "secret" ? secret : (recoveryCodes ?? []).join("\n");
    if (!text) return;
    try {
      await navigator.clipboard.writeText(text);
      setCopied(kind);
      setTimeout(() => setCopied(null), 1500);
    } catch { setErr("Clipboard is unavailable. Copy the value by hand."); }
  }

  async function startEnroll() {
    setBusy(true); setErr(""); setNote(""); setRecoveryCodes(null);
    try {
      const d = await apiSend<EnrollResult>("POST", "/v1/account/2fa/enroll");
      const s = (d.secret_base32 ?? d.secret ?? "").trim();
      if (!s) throw new Error("The server did not return a secret.");
      const dg = typeof d.digits === "number" ? d.digits : 6;
      const pd = typeof d.period === "number" ? d.period : 30;
      setSecret(s);
      setDigits(dg);
      setPeriod(pd);
      setOtpauthUri((d.otpauth_uri ?? "").trim() || otpauthFromSecret(s, dg, pd));
      setEnrollCode("");
    } catch (e) { fail(e); } finally { setBusy(false); }
  }

  async function confirmEnroll() {
    if (!/^\d{6}$/.test(enrollCode)) { setErr("Enter the 6-digit code from your app."); return; }
    setBusy(true); setErr("");
    try {
      const d = await apiSend<ConfirmResult>("POST", "/v1/account/2fa/confirm", { otp: enrollCode, code: enrollCode });
      setRecoveryCodes(Array.isArray(d.recovery_codes) ? d.recovery_codes : []);
      setStatus({ otp_required: true, enrolled: true });
      // One-time plaintext: discard the secret and the provisioning value after the confirm step.
      setSecret(""); setOtpauthUri(""); setEnrollCode("");
    } catch (e) { fail(e); } finally { setBusy(false); }
  }

  async function refreshStepUp() {
    setBusy(true); setErr(""); setNote("");
    try {
      const ok = await refreshTwoFactorStepUp();
      setNote(ok ? "Sign-in refreshed. Use a NEW code (or a recovery code) to disable." : "Could not refresh the sign-in.");
    } catch (e) { fail(e); } finally { setBusy(false); }
  }

  async function disable() {
    const code = disableCode.trim();
    if (!code) { setErr("Enter a fresh code from your app."); return; }
    if (!confirm("Disable two-factor authentication for this account? Your recovery codes will stop working.")) return;
    setBusy(true); setErr(""); setNote("");
    try {
      await apiSend("POST", "/v1/account/2fa/disable", { otp: code, code });
      setDisableCode("");
      setStatus({ otp_required: false, enrolled: false });
      setRecoveryCodes(null);
      setNote("Two-factor authentication is off.");
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      if (/step.?up/i.test(msg)) setErr("Your sign-in is too old for this change. Refresh your sign-in, then use a NEW code.");
      else fail(e);
    } finally { setBusy(false); }
  }

  const enabled = status?.otp_required === true || recoveryCodes !== null;
  const showEnrollment = !!secret && recoveryCodes === null;

  return (
    <div>
      <Link href="/settings" className="mb-3 inline-flex items-center gap-1.5 text-sm text-link hover:underline"><ArrowLeft className="h-4 w-4" /> Team Settings</Link>
      <PageHeader title="Two-Factor Authentication" desc="Optional TOTP for your account: enrollment, recovery codes, disable." />

      <Card className="mb-6">
        <div className="flex items-center gap-3">
          <span className="flex h-9 w-9 items-center justify-center rounded-lg bg-subtle text-secondary"><ShieldCheck className="h-4 w-4" /></span>
          <div className="flex-1">
            <div className="text-sm font-medium">Account protection</div>
            <div className="text-xs text-secondary">otp_required: {enabled ? "true" : status === null ? "unknown" : "false"}</div>
          </div>
          {enabled ? <Badge tone="green">On</Badge> : <Badge tone={status === null ? "amber" : "default"}>{status === null ? "Unknown" : "Off"}</Badge>}
        </div>
        {statusUnavailable ? (
          <p className="mt-3 text-xs text-secondary">This node did not answer GET /v1/account/2fa/status; the status is derived from your actions on this page until the route is deployed.</p>
        ) : null}
      </Card>

      {recoveryCodes !== null ? (
        <Card className="mb-6 border-amber-500/40 bg-amber-500/[0.04]">
          <div className="mb-2 flex items-center gap-2 text-sm font-medium text-amber-600 dark:text-amber-400"><TriangleAlert className="h-4 w-4" /> Save these recovery codes now — they are shown once.</div>
          <div className="mb-3 grid grid-cols-2 gap-2 font-mono text-sm sm:grid-cols-3">
            {recoveryCodes.map((c) => <code key={c} className="rounded-md border border-border bg-card px-2 py-1">{c}</code>)}
          </div>
          <div className="flex gap-2">
            <Button variant="outline" onClick={() => void copy("codes")}>{copied === "codes" ? <><Check className="h-4 w-4 text-green" /> Copied</> : <><Copy className="h-4 w-4" /> Copy all</>}</Button>
            <Button variant="ghost" onClick={() => setRecoveryCodes(null)}>Done</Button>
          </div>
        </Card>
      ) : null}

      {showEnrollment ? (
        <Card className="mb-6">
          <div className="mb-3 text-sm font-medium">1. Scan or copy the secret</div>
          <div className="flex flex-col gap-4 sm:flex-row">
            <QrCanvas value={otpauthUri} />
            <div className="min-w-0 flex-1">
              <p className="mb-2 text-xs text-secondary">Scan the QR with your authenticator app, or paste the base32 secret. {digits} digits, {period} s, SHA1.</p>
              <div className="flex items-center gap-2">
                <code className="min-w-0 flex-1 truncate rounded-md border border-border bg-card px-3 py-2 font-mono text-sm">{secret}</code>
                <Button variant="outline" onClick={() => void copy("secret")}>{copied === "secret" ? <><Check className="h-4 w-4 text-green" /> Copied</> : <><Copy className="h-4 w-4" /> Copy</>}</Button>
              </div>
            </div>
          </div>
          <div className="mt-4 text-sm font-medium">2. Confirm the first code</div>
          <div className="mt-2 flex flex-wrap items-center gap-2">
            <Input value={enrollCode} onChange={(e) => setEnrollCode(e.target.value.replace(/[^0-9]/g, "").slice(0, 6))} inputMode="numeric" pattern="[0-9]*" maxLength={6} autoComplete="one-time-code" placeholder="000000" className="w-40 font-mono tracking-widest" />
            <Button onClick={() => void confirmEnroll()} disabled={busy}>Confirm</Button>
            <Button variant="ghost" onClick={() => { setSecret(""); setOtpauthUri(""); setEnrollCode(""); }}>Cancel</Button>
          </div>
        </Card>
      ) : null}

      {!showEnrollment && recoveryCodes === null && !enabled ? (
        <Card className="mb-6">
          <div className="mb-2 text-sm font-medium">Enable two-factor authentication</div>
          <p className="mb-3 text-sm text-secondary">You will scan a QR code, confirm one 6-digit code, and receive ten single-use recovery codes. Enrollment is optional and affects only your account.</p>
          <Button onClick={() => void startEnroll()} disabled={busy}><ShieldCheck className="h-4 w-4" /> Start enrollment</Button>
        </Card>
      ) : null}

      {enabled && !showEnrollment && recoveryCodes === null ? (
        <Card className="mb-6">
          <div className="mb-1 text-sm font-medium text-red-600">Danger Zone</div>
          <p className="mb-3 text-sm text-secondary">Disabling needs a fresh code from your app (or an unused recovery code). If your sign-in is older than five minutes, refresh it first, then use a NEW code.</p>
          <div className="flex flex-wrap items-center gap-2">
            <Input value={disableCode} onChange={(e) => setDisableCode(e.target.value)} autoComplete="off" placeholder="Code or recovery code" className="w-56 font-mono" />
            <Button variant="danger" onClick={() => void disable()} disabled={busy}>Disable 2FA</Button>
            <Button variant="outline" onClick={() => void refreshStepUp()} disabled={busy}>Refresh sign-in</Button>
          </div>
        </Card>
      ) : null}

      {err ? <p className="mb-4 text-sm text-red-500">{err}</p> : null}
      {note ? <p className="mb-4 text-sm text-secondary">{note}</p> : null}
    </div>
  );
}
