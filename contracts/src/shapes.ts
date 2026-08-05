/**
 * Frozen response shapes for the SaaS API surface (SCR-85).
 *
 * Source of truth: the Rust handlers in `bins/scrapix-api/src/auth/handlers/`,
 * `configs.rs`, `engines.rs`, `analytics.rs`, mirrored by the console types in
 * `console/src/lib/api-types.ts`. The Rails app must reproduce these exactly —
 * snake_case keys, same optionality, same nesting.
 */

import type { Spec } from "./shape";

export const ACCOUNT: Spec = {
  id: "string",
  name: "string",
  tier: "string",
  active: "boolean",
  role: "string",
  credits_balance: "number",
};

export const USER: Spec = {
  id: "string",
  email: "string",
  full_name: "string|null",
  email_verified: "boolean",
  notify_job_emails: "boolean",
  account: ACCOUNT,
};

export const ERROR_BODY: Spec = {
  error: "string",
  code: "string",
};

export const MESSAGE: Spec = {
  message: "string",
};

export const API_KEY: Spec = {
  id: "string",
  name: "string",
  prefix: "string",
  active: "boolean",
  last_used_at: "string|null",
  created_at: "string",
};

export const CREATED_API_KEY: Spec = {
  id: "string",
  name: "string",
  prefix: "string",
  key: "string",
};

export const BILLING: Spec = {
  tier: "string",
  stripe_customer_id: "string|null",
  credits_balance: "number",
  auto_topup_enabled: "boolean",
  auto_topup_amount: "number",
  auto_topup_threshold: "number",
  monthly_spend_limit: "number|null",
};

export const TRANSACTION: Spec = {
  id: "string",
  type: "string",
  amount: "number",
  balance_after: "number",
  description: "string|null",
  created_at: "string",
};

export const TRANSACTIONS_LIST: Spec = {
  transactions: [TRANSACTION],
  total: "number",
};

export const TOPUP: Spec = {
  credits_balance: "number",
  transaction_id: "string",
  message: "string",
};

export const MEMBER: Spec = {
  user_id: "string",
  email: "string",
  full_name: "string|null",
  role: "string",
  joined_at: "string",
};

export const INVITE: Spec = {
  id: "string",
  email: "string",
  role: "string",
  status: "string",
  invited_by: "string",
  expires_at: "string",
  created_at: "string",
};

export const SAVED_CONFIG: Spec = {
  id: "string",
  account_id: "string",
  name: "string",
  description: "string|null",
  config: "any",
  cron_expression: "string|null",
  cron_enabled: "boolean",
  last_run_at: "string|null",
  next_run_at: "string|null",
  last_job_id: "string|null",
  created_at: "string",
  updated_at: "string",
};

export const ENGINE: Spec = {
  id: "string",
  account_id: "string",
  name: "string",
  url: "string",
  api_key: "string",
  is_default: "boolean",
  created_at: "string",
  updated_at: "string",
};

export const PIPE_INFO: Spec = {
  name: "string",
  description: "string",
  endpoint: "string",
  parameters: [
    {
      name: "string",
      type: "string",
      required: "boolean",
      default: "string|null",
    },
  ],
};

export const ANALYTICS_RESPONSE: Spec = {
  meta: [{ name: "string", type: "string" }],
  data: ["any"],
  rows: "number",
  statistics: { elapsed: "number", rows_read: "number", bytes_read: "number" },
};
