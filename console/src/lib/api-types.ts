// From GET /stats
export interface SystemStats {
  meilisearch: {
    available: boolean;
    url: string;
  };
  jobs: {
    total: number;
    running: number;
    completed: number;
    failed: number;
    pending: number;
  };
  diagnostics: {
    recent_errors_count: number;
    tracked_domains: number;
    total_requests: number;
    total_successes: number;
    total_failures: number;
  };
  collected_at: string;
}

// From GET /jobs — array of JobStatusResponse
// Also used by GET /job/{id}/status
export interface Job {
  job_id: string;
  /** `crawl`, `batch_scrape` or `extract` (absent from older engines) */
  job_type?: "crawl" | "batch_scrape" | "extract";
  status: string;
  index_uid: string;
  pages_crawled: number;
  pages_indexed: number;
  documents_sent: number;
  errors: number;
  started_at?: string;
  completed_at?: string;
  duration_seconds?: number;
  error_message?: string;
  crawl_rate: number;
  eta_seconds?: number;
  start_urls?: string[];
  max_pages?: number;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  config?: Record<string, any>;
}

// Alias for backwards compat
export type JobStatus = Job;

export interface CrawlConfig {
  start_urls: string[];
  max_depth?: number;
  max_pages?: number;
  allowed_domains?: string[];
  index_uid: string;
}

// WebSocket envelope messages from /ws/job/{id}
// The server wraps events in WsServerMessage envelopes.
export type WsServerMessage =
  | { type: "event"; job_id: string; event: CrawlEvent }
  | { type: "status"; job_id: string; status: Job }
  | { type: "subscribed"; job_id: string }
  | { type: "unsubscribed"; job_id: string }
  | { type: "error"; message: string; code: string }
  | { type: "pong"; timestamp: number };

// Inner CrawlEvent — matches Rust CrawlEvent serde output
export type CrawlEvent =
  | { type: "job_started"; job_id: string; index_uid: string; start_urls: string[]; timestamp: number }
  | { type: "page_crawled"; job_id: string; url: string; status: number; content_length: number; duration_ms: number; timestamp: number }
  | { type: "page_failed"; job_id: string; url: string; error: string; retry_count: number; timestamp: number }
  | { type: "document_indexed"; job_id: string; url: string; document_id: string; timestamp: number }
  | { type: "urls_discovered"; job_id: string; source_url: string; count: number; timestamp: number }
  | { type: "job_completed"; job_id: string; pages_crawled: number; documents_indexed: number; errors: number; bytes_downloaded: number; duration_secs: number; timestamp: number }
  | { type: "job_failed"; job_id: string; error: string; timestamp: number }
  | { type: "page_skipped"; job_id: string; url: string; reason: string; timestamp: number }
  | { type: "rate_limited"; job_id: string; domain: string; wait_ms: number; timestamp: number };

// From GET /health/services
export interface ServiceHealth {
  services: ServiceStatus[];
}

export interface ServiceStatus {
  name: string;
  status: "up" | "idle" | "down";
  last_seen_secs_ago?: number;
}

// From GET /errors
export interface RecentErrors {
  errors: ErrorEntry[];
  total_count: number;
  by_status: Record<string, number>;
  by_domain: Array<{ domain: string; count: number }>;
  source: string;
}

export interface ErrorEntry {
  url: string;
  status?: number;
  error: string;
  domain: string;
  timestamp: string;
}

// From GET /configs, POST /configs, etc.
export interface SavedConfig {
  id: string;
  account_id: string;
  name: string;
  description: string | null;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  config: Record<string, any>;
  cron_expression: string | null;
  cron_enabled: boolean;
  last_run_at: string | null;
  next_run_at: string | null;
  last_job_id: string | null;
  last_error: string | null;
  created_at: string;
  updated_at: string;
}

export interface CreateConfigRequest {
  name: string;
  description?: string;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  config: Record<string, any>;
  cron_expression?: string;
  cron_enabled?: boolean;
}

export interface UpdateConfigRequest {
  name?: string;
  description?: string | null;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  config?: Record<string, any>;
  cron_expression?: string | null;
  cron_enabled?: boolean;
}

export interface TriggerResponse {
  job_id: string;
  config_id: string;
  message: string;
}

// From POST /scrape
export interface ScrapeResult {
  success: boolean;
  url: string;
  status_code: number;
  scrape_duration_ms: number;
  markdown?: string;
  html?: string;
  raw_html?: string;
  content?: string;
  links?: string[];
  language?: string;
  metadata?: ScrapeMetadata;
  schema?: Record<string, unknown>;
  blocks?: ContentBlock[];
  extract?: Record<string, unknown>;
  ai?: AiResult;
  /** Base64-encoded PNG (format `screenshot`) */
  screenshot?: string;
  /** Results of the request's `actions` (present when actions were sent) */
  actions?: ScrapeActionsResult;
  warning?: string;
  /** Present when the URL served (or the upload is) a PDF or office document. */
  document?: DocumentInfo;
  /** Present when `parsers.ocr` requested OCR. */
  ocr?: OcrInfo;
}

export type OcrMode = "off" | "auto" | "force";

/** Document parsing options for /scrape and /parse. */
export interface ParserOptions {
  ocr?: OcrMode;
  ocr_max_pages?: number;
  max_pages?: number;
}

export interface DocumentInfo {
  /** `pdf`, `docx`, `xlsx`, `pptx`, `doc`, `ppt`, `odt`, `ods`, `odp`, `rtf`, `epub`, `csv`, `image`. */
  format: string;
  content_type: string;
  /** `pdf-inspector`, `anydoc`, `image`. */
  parser: string;
  bytes: number;
  page_count?: number;
  pages_processed?: number;
  /** `text_based`, `scanned`, `image_based`, `mixed`. */
  pdf_type?: string;
  needs_ocr: boolean;
  pages_needing_ocr?: number[];
  has_tables: boolean;
}

export interface OcrInfo {
  mode: OcrMode;
  backend?: string;
  pages_processed: number;
  pages_cached: number;
  pages_skipped: number;
  pages_capped: number;
  pages_failed: number;
  pages: number[];
  warning?: string;
}

export interface ScrapeActionsResult {
  /** Values of the `execute_javascript` actions, in order (`undefined` is `null`) */
  javascript_returns: unknown[];
}

/** A browser interaction run after the page loads (POST /scrape `actions`). */
export type ScrapeAction =
  | { type: "wait"; ms?: number; selector?: string }
  | { type: "click"; selector: string }
  | { type: "scroll"; direction?: "up" | "down"; amount?: number }
  | { type: "write"; selector: string; text: string }
  | { type: "press"; key: string }
  | { type: "execute_javascript"; script: string };

/** A cookie sent with a scrape (POST /scrape `cookies`). */
export interface RequestCookie {
  name: string;
  value: string;
  /** Target host or a parent domain of it; defaults to the target host */
  domain?: string;
  path?: string;
  secure?: boolean;
  http_only?: boolean;
}

/** Error body returned by the engine (`ApiError`). */
export interface ApiErrorBody {
  error: string;
  code: string;
  details?: unknown;
}

/** `details` of a 422 `action_error`. */
export interface ActionErrorDetails {
  action_index: number;
  action_type: string;
  message: string;
}

export interface ContentBlock {
  heading?: string;
  heading_level?: number;
  content: string;
}

export interface AiResult {
  summary?: string;
  extract?: Record<string, unknown>;
}

// From GET /engines, POST /engines, etc.
export interface MeilisearchEngine {
  id: string;
  account_id: string;
  name: string;
  url: string;
  /** Masked hint ("••••" + last 4 characters), "" when no key is set. */
  api_key: string;
  has_api_key: boolean;
  is_default: boolean;
  created_at: string;
  updated_at: string;
}

export interface CreateEngineRequest {
  name: string;
  url: string;
  api_key?: string;
  is_default?: boolean;
}

export interface UpdateEngineRequest {
  name?: string;
  url?: string;
  api_key?: string;
}

export interface MeilisearchIndex {
  uid: string;
  primaryKey: string | null;
  createdAt: string;
  updatedAt: string;
}

export interface MeilisearchSearchResponse {
  hits: MeilisearchHit[];
  query: string;
  processingTimeMs: number;
  limit: number;
  offset: number;
  estimatedTotalHits: number;
}

export interface MeilisearchHit {
  uid?: string;
  url?: string;
  domain?: string;
  title?: string;
  content?: string;
  markdown?: string;
  h1?: string;
  h2?: string;
  h3?: string;
  language?: string;
  crawled_at?: string;
  metadata?: Record<string, string>;
  _formatted?: Record<string, string>;
  [key: string]: unknown;
}

export interface ScrapeMetadata {
  title?: string;
  description?: string;
  author?: string;
  keywords: string[];
  canonical_url?: string;
  published_date?: string;
  open_graph: Record<string, string>;
  twitter: Record<string, string>;
}

// From POST /map
export interface MapLink {
  url: string;
  title?: string;
  description?: string;
  lastmod?: string;
  priority?: number;
  changefreq?: string;
}

export interface MapResult {
  success: boolean;
  links: MapLink[];
  total: number;
  duration_ms: number;
}

// ============================================================================
// Billing
// ============================================================================

export interface BillingInfo {
  tier: string;
  stripe_customer_id: string | null;
  credits_balance: number;
  auto_topup_enabled: boolean;
  auto_topup_amount: number;
  auto_topup_threshold: number;
  monthly_spend_limit: number | null;
}

export interface Transaction {
  id: string;
  type: string;
  amount: number;
  balance_after: number;
  description: string | null;
  created_at: string;
}

export interface TransactionsListResponse {
  transactions: Transaction[];
  total: number;
}

export interface TopupResponse {
  credits_balance: number;
  transaction_id: string;
  message: string;
}

// ============================================================================
// Stripe / Payment Methods
// ============================================================================

export interface SetupIntentResponse {
  client_secret: string;
}

export interface PaymentMethodInfo {
  id: string;
  brand: string | null;
  last4: string | null;
  exp_month: number | null;
  exp_year: number | null;
  is_default: boolean;
}

export interface PurchaseCreditsRequest {
  credits: number;
  payment_method_id?: string;
}

export interface PurchaseResponse {
  status: "succeeded" | "requires_action";
  client_secret: string | null;
  credits: number;
  amount_cents: number;
  message: string;
}

export interface InvoiceInfo {
  id: string;
  number: string | null;
  amount_cents: number;
  credits: number | null;
  status: string;
  description: string | null;
  created_at: string;
  invoice_pdf: string | null;
  hosted_invoice_url: string | null;
}

// ============================================================================
// Team / Multi-tenancy
// ============================================================================

export interface AccountListItem {
  id: string;
  name: string;
  tier: string;
  active: boolean;
  role: string;
  credits_balance: number;
}

export interface MemberInfo {
  user_id: string;
  email: string;
  full_name: string | null;
  role: string;
  joined_at: string;
}

export interface InviteInfo {
  id: string;
  email: string;
  role: string;
  status: string;
  invited_by: string;
  expires_at: string;
  created_at: string;
}

// ============================================================================
// Analytics (Tinybird-style responses)
// ============================================================================

export interface AnalyticsResponse<T> {
  meta: { name: string; type: string }[];
  data: T[];
  rows: number;
  statistics: { elapsed: number; rows_read: number; bytes_read: number };
}

export interface HourlyStatsRow {
  hour: string;
  requests: number;
  successes: number;
  failures: number;
  success_rate: number;
  avg_duration_ms: number;
  total_bytes: number;
}

export interface DailyStatsRow {
  date: string;
  requests: number;
  successes: number;
  failures: number;
  success_rate: number;
  avg_duration_ms: number;
  total_bytes: number;
}

export interface KpisRow {
  total_crawls: number;
  total_bytes: number;
  unique_domains: number;
  success_rate: number;
  avg_duration_ms: number;
  errors_count: number;
}

export interface AccountUsageRow {
  account_id: string;
  total_requests: number;
  successful_requests: number;
  failed_requests: number;
  total_bytes: number;
  avg_duration_ms: number;
  unique_domains: number;
  js_renders: number;
  ai_prompt_tokens: number;
  ai_completion_tokens: number;
}

export interface DailyUsageRow {
  date: string;
  requests: number;
  bytes: number;
  js_renders: number;
  ai_prompt_tokens: number;
  ai_completion_tokens: number;
}

export interface DailyUsageByOpRow {
  date: string;
  operation: string;
  requests: number;
  bytes: number;
  js_renders: number;
  ai_prompt_tokens: number;
  ai_completion_tokens: number;
}

export interface TopDomainRow {
  domain: string;
  total_requests: number;
  successful_requests: number;
  failed_requests: number;
  success_rate: number;
  avg_duration_ms: number;
  total_bytes: number;
}

// From GET /job/{id}/events/history
export interface PageEventRecord {
  event_type: "page_crawled" | "page_failed" | "document_indexed" | "urls_discovered" | "page_skipped" | "rate_limited";
  url: string;
  status_code: number;
  content_length: number;
  duration_ms: number;
  error: string;
  retry_count: number;
  document_id: string;
  urls_count: number;
  source_url: string;
  reason: string;
  domain: string;
  wait_ms: number;
  /** Unix timestamp in milliseconds */
  timestamp: number;
}

export interface JobEventsHistoryResponse {
  events: PageEventRecord[];
  returned: number;
  limit: number;
  offset: number;
}

// ============================================================================
// Job results (GET /job/{id}/results), batch scrape, extract
// ============================================================================

export type JobType = "crawl" | "batch_scrape" | "extract";

export interface JobResultError {
  code: string;
  message: string;
}

/** One job result: a `/scrape` response plus job-specific fields. */
export interface JobResultItem extends Partial<Omit<ScrapeResult, "success" | "url">> {
  success: boolean;
  url: string;
  source_url?: string;
  index?: number;
  error?: JobResultError;
  document_id?: string;
  crawled_at?: string;
  page_block?: number;
  block_url?: string;
}

export interface JobResultsPage {
  job_id: string;
  job_type: JobType;
  status: string;
  total: number;
  next: string | null;
  data: JobResultItem[];
}

/** POST /batch/scrape. Any other `/scrape` option is also accepted. */
export interface BatchScrapeRequest {
  urls: string[];
  /** URLs scraped at once (default 10, max 25) */
  concurrency?: number;
  formats?: string[];
  only_main_content?: boolean;
  include_links?: boolean;
  render_js?: boolean;
  timeout_ms?: number;
}

export interface BatchScrapeResponse {
  job_id: string;
  status: string;
  urls_count: number;
  message: string;
}

export interface ExtractFieldDefinition {
  name: string;
  description?: string;
  field_type?: string;
  required?: boolean;
}

export interface ExtractRequest {
  urls: string[];
  prompt?: string;
  schema?: Record<string, unknown> | ExtractFieldDefinition[];
  render_js?: boolean;
  only_main_content?: boolean;
  timeout_ms?: number;
  headers?: Record<string, string>;
}

export interface ExtractSource {
  url: string;
  from_glob?: string;
  success: boolean | null;
  error?: string;
}

export interface ExtractStatus {
  job_id: string;
  status: string;
  data: unknown;
  sources: ExtractSource[];
  warning?: string;
  error?: string;
}
