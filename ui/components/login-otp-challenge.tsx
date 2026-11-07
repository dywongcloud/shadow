"use client";

import { useEffect, useRef, useState } from "react";
import { Button, Card, Input } from "@/components/ui";
import { registerOtpPrompt } from "@/lib/api";

/** The ONLY login-time 2FA form. It appears on the typed `otp_required` refusal from `/api/token`,
 *  holds NO challenge id (that stays in the in-flight mint in `ui/lib/api.ts`), and returns just the
 *  code. A 6-digit TOTP or a one-use recovery code both travel in the `otp` re-POST field. */
export function LoginOtpChallenge() {
  const [open, setOpen] = useState(false);
  const [error, setError] = useState("");
  const [code, setCode] = useState("");
  const [recovery, setRecovery] = useState("");
  const [useRecovery, setUseRecovery] = useState(false);
  const resolver = useRef<((value: string | null) => void) | null>(null);

  useEffect(() => {
    registerOtpPrompt((err) => {
      setError(err);
      setCode("");
      setRecovery("");
      setUseRecovery(false);
      setOpen(true);
      return new Promise<string | null>((resolve) => { resolver.current = resolve; });
    });
    return () => {
      registerOtpPrompt(null);
      resolver.current?.(null);
    };
  }, []);

  function finish(value: string | null) {
    const resolve = resolver.current;
    resolver.current = null;
    setOpen(false);
    setError("");
    setCode("");
    setRecovery("");
    setUseRecovery(false);
    resolve?.(value);
  }

  if (!open) return null;
  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 px-4">
      <Card className="w-full max-w-sm">
        <h2 className="text-lg font-semibold">Two-factor verification</h2>
        <p className="mb-4 mt-1 text-sm text-secondary">Enter the code from your authenticator app to finish signing in.</p>
        <form
          onSubmit={(e) => {
            e.preventDefault();
            const value = (useRecovery ? recovery : code).trim();
            if (value) finish(value);
          }}
        >
          {useRecovery ? (
            <label className="block text-xs font-medium text-secondary">
              Recovery code
              <Input value={recovery} onChange={(e) => setRecovery(e.target.value)} autoFocus autoComplete="off" className="mt-1 font-mono" />
            </label>
          ) : (
            <label className="block text-xs font-medium text-secondary">
              6-digit code
              <Input
                value={code}
                onChange={(e) => setCode(e.target.value.replace(/[^0-9]/g, "").slice(0, 6))}
                inputMode="numeric"
                pattern="[0-9]*"
                maxLength={6}
                autoFocus
                autoComplete="one-time-code"
                className="mt-1 font-mono tracking-widest"
              />
            </label>
          )}
          {error ? <p className="mt-2 text-sm text-red-500">{error}</p> : null}
          <div className="mt-4 flex flex-wrap items-center gap-2">
            <Button type="submit" disabled={!(useRecovery ? recovery.trim() : /^\d{6}$/.test(code))}>Verify</Button>
            <Button type="button" variant="ghost" onClick={() => { setUseRecovery(!useRecovery); setError(""); }}>
              {useRecovery ? "Use a 6-digit code" : "Use a recovery code"}
            </Button>
            <Button type="button" variant="ghost" onClick={() => finish(null)}>Cancel</Button>
          </div>
        </form>
      </Card>
    </div>
  );
}
