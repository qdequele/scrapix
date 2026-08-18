"use client";

import { useState, Suspense } from "react";
import Link from "next/link";
import { useRouter, useSearchParams } from "next/navigation";
import { resetPassword } from "@/lib/auth";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Loader2 } from "lucide-react";
import { toast } from "sonner";

export default function ResetPasswordPage() {
  return (
    <Suspense>
      <ResetPasswordInner />
    </Suspense>
  );
}

function ResetPasswordInner() {
  const [password, setPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const router = useRouter();
  const token = useSearchParams().get("token") ?? "";

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    setError(null);
    if (password !== confirm) {
      setError("Passwords do not match");
      return;
    }
    setLoading(true);
    try {
      await resetPassword(token, password);
      toast.success("Password reset. Please sign in with your new password.");
      router.push("/login");
    } catch (err) {
      setError(err instanceof Error ? err.message : "Reset failed");
      setLoading(false);
    }
  };

  if (!token) {
    return (
      <div className="flex flex-col items-center space-y-4 text-center">
        <h1 className="text-2xl font-semibold tracking-tight">Invalid link</h1>
        <p className="text-sm text-muted-foreground">
          This reset link is missing its token. Request a new one.
        </p>
        <Link href="/forgot-password" className="text-sm underline underline-offset-4 hover:text-primary">
          Request a new link
        </Link>
      </div>
    );
  }

  return (
    <>
      <div className="flex flex-col space-y-2 text-center">
        <h1 className="text-2xl font-semibold tracking-tight">Choose a new password</h1>
        <p className="text-sm text-muted-foreground">At least 12 characters</p>
      </div>
      <form onSubmit={handleSubmit}>
        <div className="grid gap-4">
          {error && (
            <div className="p-3 text-sm text-red-500 bg-red-50 dark:bg-red-900/20 rounded-md">
              {error}
            </div>
          )}
          <div className="grid gap-2">
            <Label htmlFor="password">New password</Label>
            <Input
              id="password"
              type="password"
              autoComplete="new-password"
              minLength={12}
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              disabled={loading}
              required
            />
          </div>
          <div className="grid gap-2">
            <Label htmlFor="confirm">Confirm password</Label>
            <Input
              id="confirm"
              type="password"
              autoComplete="new-password"
              minLength={12}
              value={confirm}
              onChange={(e) => setConfirm(e.target.value)}
              disabled={loading}
              required
            />
          </div>
          <Button type="submit" disabled={loading}>
            {loading && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
            Reset password
          </Button>
        </div>
      </form>
    </>
  );
}
