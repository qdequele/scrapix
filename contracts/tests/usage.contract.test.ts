/**
 * Usage billing contract (engine → lab events → Rails ledger).
 *
 * The engine reports each billable request as a signed `usage.recorded`
 * event; Rails debits it asynchronously (within seconds), exactly once, with
 * the description the engine supplied. Needs the hosted stack: `just dev`
 * with LAB_EVENTS_URL / LAB_EVENTS_SECRET / LAB_SERVICE_TOKEN set (see
 * .env.example) and outbound access to example.com.
 */
import { describe, expect, it } from "vitest";

import { signupFresh } from "../src/client";

async function waitFor<T>(
  fn: () => Promise<T | undefined>,
  ms = 20_000,
): Promise<T> {
  const end = Date.now() + ms;
  for (;;) {
    const v = await fn();
    if (v !== undefined) return v;
    if (Date.now() > end) throw new Error("timed out");
    await new Promise((r) => setTimeout(r, 250));
  }
}

interface Txn {
  type: string;
  amount: number;
  description: string | null;
}

describe("usage billing (engine → lab events → Rails ledger)", () => {
  it("a scrape is debited once, within seconds, with the engine's description", async () => {
    // The session cookie authenticates against both Rails and the engine.
    const { session } = await signupFresh();
    const balance = async () =>
      (await session.get("/account/billing")).body.credits_balance as number;
    const transactions = async () =>
      (await session.get("/account/billing/transactions")).body
        .transactions as Txn[];

    const before = await balance();
    const res = await session.post("/scrape", {
      url: "https://example.com",
      formats: ["markdown"],
    });
    expect(res.status).toBe(200);

    const txn = await waitFor(async () =>
      (await transactions()).find(
        (t) =>
          t.type === "usage_deduction" &&
          String(t.description).startsWith("scrape: https://example.com"),
      ),
    );
    expect(txn.amount).toBeLessThan(0);
    expect(await balance()).toBe(before + txn.amount);

    // Redelivery / retries must not double-debit.
    await new Promise((r) => setTimeout(r, 2_000));
    const deductions = (await transactions()).filter(
      (t) => t.type === "usage_deduction",
    );
    expect(deductions.length).toBe(1);
  });
});
