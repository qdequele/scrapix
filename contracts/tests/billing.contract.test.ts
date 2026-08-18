import { describe, expect, it } from "vitest";

import { signupFresh } from "../src/client";
import { assertShape } from "../src/shape";
import { BILLING, MESSAGE, TRANSACTIONS_LIST } from "../src/shapes";

describe("billing contract", () => {
  it("GET /account/billing returns the billing snapshot", async () => {
    const { session } = await signupFresh();
    const res = await session.get("/account/billing");
    expect(res.status).toBe(200);
    assertShape(res.body, BILLING);
    expect(res.body.tier).toBe("free");
  });

  it("GET /account/billing/transactions returns the credit ledger", async () => {
    const { session } = await signupFresh();
    const res = await session.get("/account/billing/transactions");
    expect(res.status).toBe(200);
    assertShape(res.body, TRANSACTIONS_LIST);
  });

  it("PATCH /account/billing/auto-topup returns a message; settings visible on re-fetch", async () => {
    const { session } = await signupFresh();
    const res = await session.patch("/account/billing/auto-topup", {
      enabled: true,
      amount: 10_000,
      threshold: 1_000,
    });
    expect(res.status).toBe(200);
    assertShape(res.body, MESSAGE);

    const billing = await session.get("/account/billing");
    expect(billing.body.auto_topup_enabled).toBe(true);
    expect(billing.body.auto_topup_amount).toBe(10_000);
    expect(billing.body.auto_topup_threshold).toBe(1_000);
  });

  it("PATCH /account/billing/spend-limit sets and clears the monthly limit", async () => {
    const { session } = await signupFresh();
    const set = await session.patch("/account/billing/spend-limit", {
      monthly_spend_limit: 50_000,
    });
    expect(set.status).toBe(200);
    assertShape(set.body, MESSAGE);
    const afterSet = await session.get("/account/billing");
    expect(afterSet.body.monthly_spend_limit).toBe(50_000);

    const cleared = await session.patch("/account/billing/spend-limit", {
      monthly_spend_limit: null,
    });
    expect(cleared.status).toBe(200);
    const afterClear = await session.get("/account/billing");
    expect(afterClear.body.monthly_spend_limit).toBe(null);
  });
});
