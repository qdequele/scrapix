"use client";

import { useState } from "react";
import Link from "next/link";
import { forgotPassword } from "@/lib/auth";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Loader2, MailCheck } from "lucide-react";

export default function ForgotPasswordPage() {
  const [email, setEmail] = useState("");
  const [loading, setLoading] = useState(false);
  const [sent, setSent] = useState(false);

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    setLoading(true);
    await forgotPassword(email);
    setSent(true);
    setLoading(false);
  };

  if (sent) {
    return (
      <div className="flex flex-col items-center space-y-4 text-center">
        <MailCheck className="h-10 w-10 text-emerald-500" />
        <h1 className="text-2xl font-semibold tracking-tight">Check your inbox</h1>
        <p className="text-sm text-muted-foreground">
          If an account exists for {email}, we sent a link to reset your password.
        </p>
        <Link href="/login" className="text-sm underline underline-offset-4 hover:text-primary">
          Back to sign in
        </Link>
      </div>
    );
  }

  return (
    <>
      <div className="flex flex-col space-y-2 text-center">
        <h1 className="text-2xl font-semibold tracking-tight">Reset your password</h1>
        <p className="text-sm text-muted-foreground">
          Enter your email and we&apos;ll send you a reset link
        </p>
      </div>
      <form onSubmit={handleSubmit}>
        <div className="grid gap-4">
          <div className="grid gap-2">
            <Label htmlFor="email">Email</Label>
            <Input
              id="email"
              type="email"
              placeholder="name@example.com"
              autoComplete="email"
              value={email}
              onChange={(e) => setEmail(e.target.value)}
              disabled={loading}
              required
            />
          </div>
          <Button type="submit" disabled={loading}>
            {loading && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
            Send reset link
          </Button>
        </div>
      </form>
      <p className="text-center text-sm text-muted-foreground">
        Remembered it?{" "}
        <Link href="/login" className="underline underline-offset-4 hover:text-primary">
          Sign in
        </Link>
      </p>
    </>
  );
}
