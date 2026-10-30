"use client";

import { useEffect, useRef, useState } from "react";
import Link from "next/link";
import { Bell, AtSign, Smartphone, Phone, BellOff, ArrowLeft, Trash2, Send, CheckCircle2, XCircle } from "lucide-react";
import { Card, Switch, Badge, Button, Input } from "@/components/ui";
import { WithIdentity } from "@/components/identity";
import { RoleGate } from "@/components/role-gate";
import { apiSend, usePoll } from "@/lib/api";
import {
  currentPushSubscription,
  pushSupported,
  subscribePush,
  type PushSettings,
  type PushTestResult,
  type SmsKeyPutResult,
  type SmsPutResult,
} from "@/lib/push";
import { timeAgo } from "@/lib/utils";

const CHANNELS = [
  { key: "web", icon: <Bell className="h-4 w-4" />, title: "Web", desc: "Receive notifications in the OpenEdge dashboard." },
  { key: "email", icon: <AtSign className="h-4 w-4" />, title: "Email", desc: "" },
  { key: "push", icon: <Smartphone className="h-4 w-4" />, title: "Push", desc: "" },
  { key: "sms", icon: <Phone className="h-4 w-4" />, title: "SMS", desc: "" },
];

/** Masks a phone number for display: country prefix + last 4, the rest dotted, so the summary never prints the full number. */
function maskPhone(p: string): string {
  const digits = p.replace(/[^\d]/g, "");
  if (digits.length < 5) return p;
  const last4 = digits.slice(-4);
  return `+${digits.slice(0, 1)}${"·".repeat(Math.max(3, digits.length - 5))}${last4}`;
}

const MATRIX: { group: string; rows: { label: string; cols: string[] }[]; cols: string[] }[] = [
  { group: "Anomaly Alerts", cols: ["Push", "Email", "Web"], rows: [{ label: "Default Alert Rule", cols: ["push", "email", "web"] }] },
  { group: "Usage", cols: ["Push", "Email", "Web"], rows: [
    { label: "75% of included credits", cols: ["push", "email", "web"] },
    { label: "On-Demand Usage Summary", cols: ["push", "email"] },
  ]},
  { group: "Team", cols: ["SMS", "Push", "Email", "Web"], rows: [
    { label: "Feature Requests", cols: ["push", "email", "web"] },
    { label: "Member Joined", cols: ["push", "email", "web"] },
  ]},
];

export default function NotificationsPage() {
  const [chan, setChan] = useState<Record<string, boolean>>({ web: true, email: true, push: false, sms: false });
  const [grid, setGrid] = useState<Record<string, boolean>>({});

  useEffect(() => {
    const s = localStorage.getItem("hive_notif");
    if (s) try {
      const v = JSON.parse(s);
      // eslint-disable-next-line react-hooks/set-state-in-effect
      setChan((c) => v.chan ?? c);
      setGrid(v.grid ?? {});
    } catch {}
  }, []);
  function persist(nextChan = chan, nextGrid = grid) {
    localStorage.setItem("hive_notif", JSON.stringify({ chan: nextChan, grid: nextGrid }));
  }
  function setChannel(k: string, v: boolean) { const n = { ...chan, [k]: v }; setChan(n); persist(n, grid); }

  const { data: pushData, refresh: refreshPush } = usePoll<PushSettings>("/v1/push/settings", 15000);
  const [ownEndpoint, setOwnEndpoint] = useState<string | null>(null);
  useEffect(() => { currentPushSubscription().then((s) => setOwnEndpoint(s?.endpoint ?? null)); }, []);
  const [chanBusy, setChanBusy] = useState<string | null>(null);
  const [chanErr, setChanErr] = useState<string | null>(null);

  const pushDevices = pushData?.devices ?? [];
  const thisDeviceSubscribed = !!ownEndpoint && pushDevices.some((d) => d.endpoint === ownEndpoint);
  const sms = pushData?.sms ?? null;
  const pushSupportedHere = pushSupported();

  function channelDesc(k: string): React.ReactNode {
    if (k === "push") {
      if (!pushSupportedHere) return "Not supported in this browser.";
      if (pushDevices.length === 0) return "Receive notifications on desktop or mobile.";
      return `${pushDevices.length} device${pushDevices.length === 1 ? "" : "s"} registered${thisDeviceSubscribed ? " · incl. this one" : ""}.`;
    }
    if (k === "sms") {
      if (!sms) return "No phone number.";
      if (!sms.verified) return `${maskPhone(sms.phone)} · pending verification`;
      return `${maskPhone(sms.phone)} · verified`;
    }
    return "";
  }

  async function togglePushChannel(on: boolean) {
    setChanBusy("push"); setChanErr(null);
    try {
      if (on) {
        const sub = await subscribePush();
        setOwnEndpoint(sub.endpoint);
      } else {
        const sub = await currentPushSubscription();
        if (sub) { await apiSend("DELETE", "/v1/push/subscribe", { endpoint: sub.endpoint }).catch(() => {}); await sub.unsubscribe().catch(() => {}); setOwnEndpoint(null); }
      }
      refreshPush();
    } catch (e) {
      setChanErr(String(e instanceof Error ? e.message : e));
    } finally { setChanBusy(null); }
  }
  async function toggleSmsChannel(on: boolean) {
    if (!sms?.verified) { setChanErr("Add and verify a phone number below to enable SMS."); return; }
    setChanBusy("sms"); setChanErr(null);
    try {
      await apiSend("PUT", "/v1/push/sms", { phone: sms.phone, enabled: on });
      refreshPush();
    } catch (e) {
      setChanErr(String(e instanceof Error ? e.message : e));
    } finally { setChanBusy(null); }
  }
  const channelChecked = (k: string): boolean =>
    k === "push" ? thisDeviceSubscribed : k === "sms" ? !!(sms?.verified && sms.enabled) : !!chan[k];
  function onChannelToggle(k: string, v: boolean) {
    if (k === "push") return void togglePushChannel(v);
    if (k === "sms") return void toggleSmsChannel(v);
    setChannel(k, v);
  }
  function cell(rowKey: string, col: string) {
    const k = `${rowKey}.${col}`;
    return grid[k] ?? true;
  }
  function setCell(rowKey: string, col: string, v: boolean) {
    const n = { ...grid, [`${rowKey}.${col}`]: v }; setGrid(n); persist(chan, n);
  }

  return (
    <div>
      <Link href="/settings" className="mb-3 inline-flex items-center gap-1.5 text-sm text-link hover:underline"><ArrowLeft className="h-4 w-4" /> Team Settings</Link>
      <h1 className="text-2xl font-semibold tracking-tight">Notifications</h1>
      <p className="mb-6 mt-1 text-sm text-secondary">Manage your personal notification settings for this team.</p>

      <Card className="p-0">
        {CHANNELS.map((c, i) => (
          <div key={c.key} className={`flex items-center justify-between px-5 py-4 ${i ? "border-t border-border" : ""}`}>
            <div className="flex items-center gap-3">
              <span className="flex h-9 w-9 items-center justify-center rounded-full bg-subtle text-secondary">{c.icon}</span>
              <div>
                <div className="flex items-center gap-2 text-sm font-medium">
                  {c.title}
                  {c.key === "push" && (thisDeviceSubscribed
                    ? <Badge tone="green">This device</Badge>
                    : <Badge tone="amber">Subscribe device</Badge>)}
                  {c.key === "sms" && sms && (sms.verified
                    ? <Badge tone="green">Verified</Badge>
                    : <Badge tone="amber">Unverified</Badge>)}
                </div>
                <div className="text-xs text-secondary">
                  {c.key === "email"
                    ? <WithIdentity>{(id) => <>{id.email || "your email"}</>}</WithIdentity>
                    : (c.key === "push" || c.key === "sms") ? channelDesc(c.key) : c.desc}
                </div>
              </div>
            </div>
            <Switch
              checked={channelChecked(c.key)}
              onChange={(v) => onChannelToggle(c.key, v)}
              disabled={chanBusy === c.key || (c.key === "push" && !pushSupportedHere)}
              label={c.title}
            />
          </div>
        ))}
        {chanErr && <div className="border-t border-border px-5 py-2 text-xs text-red-500">{chanErr}</div>}
        <div className="flex items-center justify-between border-t border-border px-5 py-4">
          <div className="flex items-center gap-3">
            <span className="flex h-9 w-9 items-center justify-center rounded-full bg-subtle text-secondary"><BellOff className="h-4 w-4" /></span>
            <div>
              <div className="text-sm font-medium">Mute</div>
              <div className="text-xs text-secondary">Select projects to mute notifications for.</div>
            </div>
          </div>
          <span className="text-sm text-muted">No projects</span>
        </div>
      </Card>

      <PushSmsDelivery data={pushData} refresh={refreshPush} ownEndpoint={ownEndpoint} setOwnEndpoint={setOwnEndpoint} />

      <div className="mt-8 space-y-8">
        {MATRIX.map((m) => (
          <div key={m.group}>
            <div className="mb-2 flex items-center justify-between border-b border-border pb-2">
              <span className="text-sm font-semibold">{m.group}</span>
              <div className="flex gap-6 text-xs font-medium text-muted">{m.cols.map((c) => <span key={c} className="w-10 text-center">{c}</span>)}</div>
            </div>
            {m.rows.map((row) => {
              const rowKey = `${m.group}:${row.label}`;
              return (
                <div key={row.label} className="flex items-center justify-between border-b border-border py-2.5 text-sm last:border-0">
                  <span className="text-secondary">{row.label}</span>
                  <div className="flex gap-6">
                    {m.cols.map((col) => {
                      const lc = col.toLowerCase();
                      const available = row.cols.includes(lc);
                      return (
                        <div key={col} className="flex w-10 justify-center">
                          {available ? (
                            <input type="checkbox" checked={cell(rowKey, lc)} onChange={(e) => setCell(rowKey, lc, e.target.checked)} />
                          ) : (
                            <span className="h-4 w-4 rounded border border-border-strong/40" />
                          )}
                        </div>
                      );
                    })}
                  </div>
                </div>
              );
            })}
          </div>
        ))}
      </div>
    </div>
  );
}

/** E.164: leading +, then 7–15 digits, no leading zero (e.g. +15551234567). */
const E164_RE = /^\+[1-9]\d{6,14}$/;

/** Push & SMS delivery management: registered push devices (caller-scoped to the current tenant), SMS number config and a real end-to-end test
 *  send; enrollment reuses `subscribePush` from lib/push. */
function PushSmsDelivery({
  data,
  refresh,
  ownEndpoint,
  setOwnEndpoint,
}: {
  data: PushSettings | null;
  refresh: () => void;
  ownEndpoint: string | null;
  setOwnEndpoint: (e: string | null) => void;
}) {
  const loading = data === null;

  const [err, setErr] = useState<string | null>(null);
  const [enabling, setEnabling] = useState(false);
  const [removing, setRemoving] = useState<string | null>(null);

  const [phone, setPhone] = useState("");
  const [smsEnabled, setSmsEnabled] = useState(false);
  const [smsBusy, setSmsBusy] = useState(false);
  const [smsSaved, setSmsSaved] = useState(false);
  const [codeSent, setCodeSent] = useState(false);
  const [code, setCode] = useState("");
  const [verifyBusy, setVerifyBusy] = useState(false);
  const [smsVerified, setSmsVerified] = useState(false);
  const [smsKey, setSmsKey] = useState("");
  const [smsKeyBusy, setSmsKeyBusy] = useState(false);
  const [smsKeyMsg, setSmsKeyMsg] = useState<string | null>(null);
  const isOwner = typeof window !== "undefined" && localStorage.getItem("hive_is_owner") === "1";

  async function saveSmsKey() {
    setSmsKeyBusy(true);
    setSmsKeyMsg(null);
    try {
      const r = await apiSend<SmsKeyPutResult>("POST", "/v1/push/sms-key", { key: smsKey.trim() });
      setSmsKeyMsg(
        r.sms_key_source === "env"
          ? "Override cleared — using the server-configured key."
          : r.sms_quota != null && r.sms_quota > 0
            ? `Key saved — ${r.sms_quota} SMS remaining.`
            : "Key saved, but it has NO remaining quota — fund this key at textbelt.com/purchase."
      );
      setSmsKey("");
      refresh();
    } catch (e) {
      setSmsKeyMsg(String(e instanceof Error ? e.message : e));
    } finally {
      setSmsKeyBusy(false);
    }
  }
  const smsSeeded = useRef(false);
  useEffect(() => {
    if (!data || smsSeeded.current) return;
    smsSeeded.current = true;
    if (data.sms) {
      // eslint-disable-next-line react-hooks/set-state-in-effect
      setPhone(data.sms.phone);
      setSmsEnabled(data.sms.enabled);
      setSmsVerified(data.sms.verified);
    }
  }, [data]);

  const [testing, setTesting] = useState(false);
  const [test, setTest] = useState<PushTestResult | null>(null);
  const [testErr, setTestErr] = useState<string | null>(null);

  const devices = data?.devices ?? [];
  const supported = pushSupported();
  const thisDeviceRegistered = !!ownEndpoint && devices.some((d) => d.endpoint === ownEndpoint);
  const phoneValid = E164_RE.test(phone.trim());

  async function enableHere() {
    setEnabling(true);
    setErr(null);
    try {
      const sub = await subscribePush();
      setOwnEndpoint(sub.endpoint);
      refresh();
    } catch (e) {
      console.warn("push: enable failed —", e);
      setErr(String(e instanceof Error ? e.message : e));
    } finally {
      setEnabling(false);
    }
  }

  async function removeDevice(endpoint: string) {
    setRemoving(endpoint);
    setErr(null);
    try {
      await apiSend("DELETE", "/v1/push/subscribe", { endpoint });
      if (endpoint === ownEndpoint) {
        const sub = await currentPushSubscription();
        if (sub && sub.endpoint === endpoint) await sub.unsubscribe().catch(() => {});
        setOwnEndpoint(null);
      }
      refresh();
    } catch (e) {
      setErr(String(e instanceof Error ? e.message : e));
    } finally {
      setRemoving(null);
    }
  }

  async function saveSms() {
    setSmsBusy(true);
    setErr(null);
    setCodeSent(false);
    try {
      const r = await apiSend<SmsPutResult>("PUT", "/v1/push/sms", { phone: phone.trim(), enabled: smsEnabled });
      if (r.verified) {
        setSmsVerified(true);
        setSmsSaved(true);
        setTimeout(() => setSmsSaved(false), 2500);
      } else if (r.code_sent) {
        setCodeSent(true);
        setSmsVerified(false);
      } else if (r.note) {
        setErr(r.note);
      }
      refresh();
    } catch (e) {
      setErr(String(e instanceof Error ? e.message : e));
    } finally {
      setSmsBusy(false);
    }
  }

  async function verifySms() {
    setVerifyBusy(true);
    setErr(null);
    try {
      await apiSend("POST", "/v1/push/sms/verify", { code: code.trim() });
      setSmsVerified(true);
      setCodeSent(false);
      setCode("");
      setSmsSaved(true);
      setTimeout(() => setSmsSaved(false), 2500);
      refresh();
    } catch (e) {
      setErr(String(e instanceof Error ? e.message : e));
    } finally {
      setVerifyBusy(false);
    }
  }

  async function sendTest() {
    setTesting(true);
    setTest(null);
    setTestErr(null);
    try {
      setTest(await apiSend<PushTestResult>("POST", "/v1/push/test"));
    } catch (e) {
      setTestErr(String(e instanceof Error ? e.message : e));
    } finally {
      setTesting(false);
    }
  }

  return (
    <div className="mt-8">
      <div className="mb-2 flex items-center justify-between border-b border-border pb-2">
        <span className="text-sm font-semibold">Push &amp; SMS delivery</span>
        {data?.sms_quota != null && (
          <span className="text-xs text-muted">{data.sms_quota > 0 ? `${data.sms_quota} SMS remaining` : "SMS quota exhausted — refill Textbelt"}</span>
        )}
      </div>

      <Card className="p-0">
        <div className="px-5 py-4">
          <div className="flex items-center justify-between gap-3">
            <div className="flex items-center gap-3">
              <span className="flex h-9 w-9 items-center justify-center rounded-full bg-subtle text-secondary">
                <Smartphone className="h-4 w-4" />
              </span>
              <div>
                <div className="text-sm font-medium">Registered devices</div>
                <div className="text-xs text-secondary">Browsers subscribed to push for this team.</div>
              </div>
            </div>
            {supported && !thisDeviceRegistered && (
              <Button variant="outline" onClick={enableHere} disabled={enabling}>
                {enabling ? "Enabling…" : "Enable on this device"}
              </Button>
            )}
          </div>

          {!supported ? (
            <div className="mt-3 text-sm text-muted">Push notifications are not supported in this browser.</div>
          ) : devices.length === 0 ? (
            <div className="mt-3 text-sm text-muted">{loading ? "Loading devices…" : "No devices registered yet."}</div>
          ) : (
            <div className="mt-3 divide-y divide-border rounded-md border border-border">
              {devices.map((d) => (
                <div key={d.endpoint} className="flex items-center justify-between gap-3 px-3 py-2.5">
                  <div className="min-w-0">
                    <div className="flex items-center gap-2 text-sm">
                      <span className="truncate font-medium">{d.label || "Unknown device"}</span>
                      {d.endpoint === ownEndpoint && <Badge tone="blue">This device</Badge>}
                    </div>
                    <div className="text-xs text-muted">Added {timeAgo(d.created_ms)}</div>
                  </div>
                  <button
                    onClick={() => removeDevice(d.endpoint)}
                    disabled={removing === d.endpoint}
                    aria-label={`Remove ${d.label || "device"}`}
                    className="shrink-0 rounded-md p-1.5 text-muted hover:bg-subtle hover:text-red-500 disabled:opacity-50"
                  >
                    <Trash2 className="h-4 w-4" />
                  </button>
                </div>
              ))}
            </div>
          )}
        </div>

        <div className="border-t border-border px-5 py-4">
          <div className="flex items-center gap-3">
            <span className="flex h-9 w-9 items-center justify-center rounded-full bg-subtle text-secondary">
              <Phone className="h-4 w-4" />
            </span>
            <div>
              <div className="flex items-center gap-2">
                <span className="text-sm font-medium">SMS</span>
                {smsVerified && <Badge tone="green">Verified</Badge>}
              </div>
              <div className="text-xs text-secondary">Critical alerts by text message.</div>
            </div>
            <div className="ml-auto">
              <Switch checked={smsEnabled} onChange={setSmsEnabled} label="SMS notifications" />
            </div>
          </div>
          <div className="mt-3 flex flex-col gap-2 sm:flex-row sm:items-center">
            <Input
              type="tel"
              value={phone}
              onChange={(e) => { setPhone(e.target.value); setCodeSent(false); setSmsVerified(false); }}
              placeholder="+15551234567"
              aria-label="Phone number (E.164)"
              className="sm:max-w-60"
            />
            <Button
              variant="outline"
              onClick={saveSms}
              disabled={smsBusy || (phone.trim() !== "" && !phoneValid) || (smsEnabled && !phoneValid)}
            >
              {smsBusy ? "Sending…" : smsSaved ? "Saved" : smsVerified ? "Save" : "Send code"}
            </Button>
            {phone.trim() !== "" && !phoneValid && (
              <span className="text-xs text-red-500">Use E.164 format, e.g. +15551234567</span>
            )}
            {phone.trim() === "" && smsEnabled && (
              <span className="text-xs text-red-500">A phone number is required to enable SMS.</span>
            )}
          </div>
          {codeSent && (
            <div className="mt-3 rounded-md border border-border bg-subtle/40 p-3">
              <div className="text-xs text-secondary">
                We texted a 6-digit code to {phone.trim()}. Enter it to verify this number — SMS alerts stay off until it&apos;s confirmed.
              </div>
              <div className="mt-2 flex flex-col gap-2 sm:flex-row sm:items-center">
                <Input
                  inputMode="numeric"
                  value={code}
                  onChange={(e) => setCode(e.target.value.replace(/\D/g, "").slice(0, 6))}
                  placeholder="123456"
                  aria-label="Verification code"
                  className="sm:max-w-40"
                />
                <Button variant="outline" onClick={verifySms} disabled={verifyBusy || code.trim().length !== 6}>
                  {verifyBusy ? "Verifying…" : "Verify"}
                </Button>
              </div>
            </div>
          )}
        </div>

        <div className="border-t border-border px-5 py-4">
          <div className="flex items-center justify-between gap-3">
            <div className="flex items-center gap-3">
              <span className="flex h-9 w-9 items-center justify-center rounded-full bg-subtle text-secondary">
                <Send className="h-4 w-4" />
              </span>
              <div>
                <div className="text-sm font-medium">Test notification</div>
                <div className="text-xs text-secondary">Sends a real notification to your registered devices and SMS.</div>
              </div>
            </div>
            <Button variant="outline" onClick={sendTest} disabled={testing}>
              {testing ? "Sending…" : "Send test notification"}
            </Button>
          </div>

          {testErr && <div className="mt-3 text-sm text-red-500">Test failed: {testErr}</div>}
          {test && (
            <div className="mt-3 space-y-1.5 text-sm">
              {(() => {
                const wp = test.web_push;
                const pruned = wp.pruned ?? 0;
                const ok = wp.sent > 0;
                const onlyPruned = wp.sent === 0 && wp.failed === 0 && pruned > 0;
                return (
                  <>
                    <div className="flex items-center gap-2">
                      {ok || onlyPruned ? (
                        <CheckCircle2 className="h-4 w-4 text-emerald-500" />
                      ) : (
                        <XCircle className={`h-4 w-4 ${wp.sent === 0 && wp.failed === 0 ? "text-muted" : "text-red-500"}`} />
                      )}
                      <span className="text-secondary">
                        {wp.sent > 0 && `Web push: ${wp.sent} sent`}
                        {wp.sent > 0 && wp.failed > 0 && `, ${wp.failed} failed`}
                        {wp.sent === 0 && wp.failed > 0 && `Web push: ${wp.failed} failed`}
                        {wp.sent === 0 && wp.failed === 0 && pruned === 0 && "Web push: no devices registered — enable push on this device above"}
                        {onlyPruned && `Removed ${pruned} expired device${pruned === 1 ? "" : "s"} — enable push on this device to receive notifications`}
                        {wp.sent > 0 && pruned > 0 && ` (removed ${pruned} expired)`}
                      </span>
                    </div>
                    {wp.errors.length > 0 && (
                      <div className="pl-6 text-xs text-red-500">{wp.errors.join("; ")}</div>
                    )}
                  </>
                );
              })()}
              <div className="flex items-center gap-2">
                {!test.sms.attempted ? (
                  <XCircle className="h-4 w-4 text-muted" />
                ) : test.sms.ok ? (
                  <CheckCircle2 className="h-4 w-4 text-emerald-500" />
                ) : (
                  <XCircle className="h-4 w-4 text-red-500" />
                )}
                <span className="text-secondary">
                  SMS: {!test.sms.attempted ? "not attempted (no enabled number)" : test.sms.ok ? "sent" : `failed${test.sms.error ? ` — ${test.sms.error}` : ""}`}
                </span>
              </div>
            </div>
          )}
        </div>

        {isOwner && data?.sms_key_source && (
          <div className="border-t border-border px-5 py-4">
            <div className="flex items-center gap-3">
              <span className="flex h-9 w-9 items-center justify-center rounded-full bg-subtle text-secondary">
                <Phone className="h-4 w-4" />
              </span>
              <div>
                <div className="text-sm font-medium">SMS provider key (operator)</div>
                <div className="text-xs text-secondary">
                  {data.sms_key_source === "none"
                    ? "No Textbelt key configured."
                    : `Active key ${data.sms_key ?? ""} (${data.sms_key_source === "override" ? "set here" : "server env"})`}
                  {" · a Textbelt purchase funds a specific key — paste the funded key here."}
                </div>
              </div>
            </div>
            <div className="mt-3">
              <RoleGate action="sms.provider" note>
                <div className="flex flex-col gap-2 sm:flex-row sm:items-center">
                  <Input
                    type="password"
                    value={smsKey}
                    onChange={(e) => setSmsKey(e.target.value)}
                    placeholder="Textbelt API key (empty to revert to server env)"
                    aria-label="Textbelt API key"
                    className="sm:max-w-96"
                  />
                  <Button variant="outline" onClick={saveSmsKey} disabled={smsKeyBusy}>
                    {smsKeyBusy ? "Saving…" : "Save key"}
                  </Button>
                </div>
              </RoleGate>
            </div>
            {smsKeyMsg && <div className="mt-2 text-sm text-secondary">{smsKeyMsg}</div>}
          </div>
        )}
      </Card>

      {err && <div className="mt-2 text-sm text-red-500">{err}</div>}
    </div>
  );
}
