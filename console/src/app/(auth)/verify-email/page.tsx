"use client";

import { useEffect, useRef, useState, Suspense } from "react";
import Link from "next/link";
import { useSearchParams } from "next/navigation";
import { verifyEmail } from "@/lib/auth";
import { Loader2, CheckCircle, XCircle } from "lucide-react";

export default function VerifyEmailPage() {
  return (
    <Suspense>
      <VerifyEmailInner />
    </Suspense>
  );
}

function VerifyEmailInner() {
  const token = useSearchParams().get("token") ?? "";
  const [state, setState] = useState<"pending" | "ok" | "error">("pending");
  const [message, setMessage] = useState("");
  const ran = useRef(false);

  useEffect(() => {
    if (ran.current) return;
    ran.current = true;
    if (!token) {
      setState("error");
      setMessage("This verification link is missing its token.");
      return;
    }
    verifyEmail(token)
      .then((msg) => {
        setState("ok");
        setMessage(msg || "Your email has been verified.");
      })
      .catch((err) => {
        setState("error");
        setMessage(err instanceof Error ? err.message : "Verification failed");
      });
  }, [token]);

  return (
    <div className="flex flex-col items-center space-y-4 text-center">
      {state === "pending" && (
        <>
          <Loader2 className="h-10 w-10 animate-spin text-muted-foreground" />
          <h1 className="text-2xl font-semibold tracking-tight">Verifying your email…</h1>
        </>
      )}
      {state === "ok" && (
        <>
          <CheckCircle className="h-10 w-10 text-emerald-500" />
          <h1 className="text-2xl font-semibold tracking-tight">Email verified</h1>
          <p className="text-sm text-muted-foreground">{message}</p>
          <Link href="/dashboard" className="text-sm underline underline-offset-4 hover:text-primary">
            Go to the dashboard
          </Link>
        </>
      )}
      {state === "error" && (
        <>
          <XCircle className="h-10 w-10 text-red-500" />
          <h1 className="text-2xl font-semibold tracking-tight">Verification failed</h1>
          <p className="text-sm text-muted-foreground">{message}</p>
          <Link href="/login" className="text-sm underline underline-offset-4 hover:text-primary">
            Back to sign in
          </Link>
        </>
      )}
    </div>
  );
}
