"use client";

import { useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { QRCodeSVG } from "qrcode.react";
import { useMe } from "@/lib/hooks";
import {
  otpSetupStart,
  otpSetupConfirm,
  otpDisable,
  recoveryCodes,
  webauthnRegister,
  type OtpSetupStart,
} from "@/lib/auth";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Badge } from "@/components/ui/badge";
import { Separator } from "@/components/ui/separator";
import { Loader2, ShieldCheck, KeyRound, Fingerprint } from "lucide-react";
import { toast } from "sonner";

export function SecurityCard() {
  const queryClient = useQueryClient();
  const { data: user } = useMe();
  const totpEnabled = user?.mfa?.totp ?? false;
  const passkeys = user?.mfa?.passkeys ?? 0;

  const [busy, setBusy] = useState(false);
  const [password, setPassword] = useState("");
  const [mode, setMode] = useState<
    "idle" | "totp-password" | "totp-confirm" | "totp-disable" | "codes-password" | "passkey-password"
  >("idle");
  const [setup, setSetup] = useState<OtpSetupStart | null>(null);
  const [otp, setOtp] = useState("");
  const [codes, setCodes] = useState<string[] | null>(null);

  const refreshMe = () => queryClient.invalidateQueries({ queryKey: ["me"] });

  const reset = () => {
    setMode("idle");
    setPassword("");
    setOtp("");
    setSetup(null);
    setBusy(false);
  };

  const startTotp = async () => {
    setBusy(true);
    try {
      const s = await otpSetupStart(password, user?.email ?? "");
      setSetup(s);
      setMode("totp-confirm");
    } catch (err) {
      toast.error(err instanceof Error ? err.message : "Could not start setup");
    }
    setBusy(false);
  };

  const confirmTotp = async () => {
    if (!setup) return;
    setBusy(true);
    try {
      await otpSetupConfirm(password, setup, otp.trim());
      const freshCodes = await recoveryCodes(password).catch(() => null);
      toast.success("Two-factor authentication enabled");
      setCodes(freshCodes);
      refreshMe();
      reset();
    } catch (err) {
      toast.error(err instanceof Error ? err.message : "Invalid code");
      setBusy(false);
    }
  };

  const disableTotp = async () => {
    setBusy(true);
    try {
      await otpDisable(password);
      toast.success("Two-factor authentication disabled");
      setCodes(null);
      refreshMe();
      reset();
    } catch (err) {
      toast.error(err instanceof Error ? err.message : "Could not disable TOTP");
      setBusy(false);
    }
  };

  const showCodes = async () => {
    setBusy(true);
    try {
      setCodes(await recoveryCodes(password));
      reset();
    } catch (err) {
      toast.error(err instanceof Error ? err.message : "Could not fetch codes");
      setBusy(false);
    }
  };

  const addPasskey = async () => {
    setBusy(true);
    try {
      await webauthnRegister(password);
      toast.success("Passkey registered");
      refreshMe();
      reset();
    } catch (err) {
      toast.error(err instanceof Error ? err.message : "Passkey registration failed");
      setBusy(false);
    }
  };

  const passwordPrompt = (
    label: string,
    onSubmit: () => void,
    submitLabel: string
  ) => (
    <form
      className="flex items-end gap-2"
      onSubmit={(e) => {
        e.preventDefault();
        onSubmit();
      }}
    >
      <div className="grid gap-1 flex-1">
        <Label htmlFor="sec-password">{label}</Label>
        <Input
          id="sec-password"
          type="password"
          autoComplete="current-password"
          value={password}
          onChange={(e) => setPassword(e.target.value)}
          disabled={busy}
          autoFocus
          required
        />
      </div>
      <Button type="submit" disabled={busy || !password}>
        {busy && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
        {submitLabel}
      </Button>
      <Button type="button" variant="ghost" onClick={reset} disabled={busy}>
        Cancel
      </Button>
    </form>
  );

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <ShieldCheck className="h-5 w-5" /> Security
        </CardTitle>
        <CardDescription>
          Two-factor authentication and passkeys for your account.
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-6">
        {/* TOTP */}
        <div className="space-y-3">
          <div className="flex items-center justify-between">
            <div className="flex items-center gap-2">
              <KeyRound className="h-4 w-4 text-muted-foreground" />
              <span className="text-sm font-medium">Authenticator app (TOTP)</span>
              <Badge variant={totpEnabled ? "default" : "secondary"}>
                {totpEnabled ? "Enabled" : "Off"}
              </Badge>
            </div>
            {mode === "idle" && (
              <div className="flex gap-2">
                {totpEnabled ? (
                  <>
                    <Button size="sm" variant="outline" onClick={() => setMode("codes-password")}>
                      Recovery codes
                    </Button>
                    <Button size="sm" variant="destructive" onClick={() => setMode("totp-disable")}>
                      Disable
                    </Button>
                  </>
                ) : (
                  <Button size="sm" onClick={() => setMode("totp-password")}>
                    Enable
                  </Button>
                )}
              </div>
            )}
          </div>

          {mode === "totp-password" &&
            passwordPrompt("Confirm your password to begin", startTotp, "Continue")}
          {mode === "totp-disable" &&
            passwordPrompt("Confirm your password to disable", disableTotp, "Disable")}
          {mode === "codes-password" &&
            passwordPrompt("Confirm your password to view codes", showCodes, "Show")}

          {mode === "totp-confirm" && setup && (
            <div className="space-y-3 rounded-md border p-4">
              <p className="text-sm text-muted-foreground">
                Scan this QR code with your authenticator app, then enter the
                6-digit code to confirm.
              </p>
              <div className="flex items-center gap-6">
                <div className="rounded bg-white p-2">
                  <QRCodeSVG value={setup.provisioningUri} size={132} />
                </div>
                <div className="space-y-2 text-sm">
                  <p className="text-muted-foreground">Or enter the secret manually:</p>
                  <code className="block rounded bg-muted px-2 py-1 font-mono text-xs break-all">
                    {setup.secret.toUpperCase()}
                  </code>
                </div>
              </div>
              <form
                className="flex items-end gap-2"
                onSubmit={(e) => {
                  e.preventDefault();
                  confirmTotp();
                }}
              >
                <div className="grid gap-1">
                  <Label htmlFor="otp-confirm">Authentication code</Label>
                  <Input
                    id="otp-confirm"
                    inputMode="numeric"
                    autoComplete="one-time-code"
                    className="w-40"
                    value={otp}
                    onChange={(e) => setOtp(e.target.value)}
                    disabled={busy}
                    required
                  />
                </div>
                <Button type="submit" disabled={busy || otp.length < 6}>
                  {busy && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
                  Verify & enable
                </Button>
                <Button type="button" variant="ghost" onClick={reset} disabled={busy}>
                  Cancel
                </Button>
              </form>
            </div>
          )}

          {codes && (
            <div className="space-y-2 rounded-md border p-4">
              <p className="text-sm font-medium">Recovery codes</p>
              <p className="text-xs text-muted-foreground">
                Store these somewhere safe — each can be used once if you lose
                your authenticator.
              </p>
              <div className="grid grid-cols-2 gap-1 font-mono text-xs">
                {codes.map((c) => (
                  <code key={c} className="rounded bg-muted px-2 py-1">
                    {c}
                  </code>
                ))}
              </div>
              <Button size="sm" variant="outline" onClick={() => setCodes(null)}>
                Done
              </Button>
            </div>
          )}
        </div>

        <Separator />

        {/* Passkeys */}
        <div className="space-y-3">
          <div className="flex items-center justify-between">
            <div className="flex items-center gap-2">
              <Fingerprint className="h-4 w-4 text-muted-foreground" />
              <span className="text-sm font-medium">Passkeys</span>
              <Badge variant={passkeys > 0 ? "default" : "secondary"}>
                {passkeys > 0 ? `${passkeys} registered` : "None"}
              </Badge>
            </div>
            {mode === "idle" && (
              <Button size="sm" variant="outline" onClick={() => setMode("passkey-password")}>
                Add passkey
              </Button>
            )}
          </div>
          {mode === "passkey-password" &&
            passwordPrompt("Confirm your password to add a passkey", addPasskey, "Add")}
        </div>
      </CardContent>
    </Card>
  );
}
