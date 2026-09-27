//! Crawl webhook delivery (SCR-72).
//!
//! Maps `CrawlEvent`s emitted by the pipeline to the `WebhookEvent`s a job's
//! `CrawlConfig.webhooks` can subscribe to, and delivers each matching,
//! enabled subscription as a signed HTTP POST off the event-processing
//! path: `enqueue` never awaits or touches the network, it only pushes onto
//! a bounded in-memory queue that a background task drains, spawning a
//! bounded number of concurrent delivery tasks (see "Concurrency" below).
//!
//! ## Event mapping
//!
//! | `CrawlEvent`                        | `WebhookEvent`   |
//! |--------------------------------------|------------------|
//! | `JobStarted`                         | `CrawlStarted`   |
//! | `JobCompleted`                       | `CrawlCompleted` |
//! | `JobFailed`                          | `CrawlFailed`    |
//! | `PageCrawled`                        | `PageCrawled`    |
//! | `DocumentIndexed`                    | `PageIndexed`    |
//! | `PageFailed` / `DocumentFailed`       | `PageError`      |
//! | `FrontierProgress`                   | `ProgressUpdate` (throttled, see below) |
//! | everything else                      | *(not delivered)* |
//!
//! `PageRetried`, `DocumentSkipped`, `AiUsage`, `JobWarning`,
//! `UrlsDiscovered`, `RateLimited`, `PageSkipped` and `SitemapPublished`
//! have no webhook equivalent and are silently dropped by `enqueue`.
//!
//! `WebhookEvent::BatchSent` has no corresponding `CrawlEvent` in this
//! engine (there is no "batch flushed to the store" event today). A hook
//! may subscribe to it — the config schema accepts it — but `enqueue` never
//! maps any event to it, so it will simply never fire.
//!
//! `ProgressUpdate` deliveries are throttled to at most one per job every
//! [`PROGRESS_THROTTLE`], regardless of how many `FrontierProgress` events
//! arrive in that window or how many hooks are subscribed. The throttle
//! bookkeeping (`progress_last_sent`) is only written when at least one
//! enabled hook is actually subscribed to `ProgressUpdate` (no point
//! growing a map entry per job that has no such hook), and is dropped for a
//! job the moment it reaches a terminal `CrawlCompleted`/`CrawlFailed`, so
//! the map never accumulates entries for finished jobs.
//!
//! Job cancellation (`DELETE /job/:id`) does not go through the pipeline
//! and so emits no `CrawlEvent` at all; `lib.rs`'s `cancel()` instead calls
//! `enqueue` directly with a synthetic
//! `CrawlEvent::JobFailed { error: "cancelled", .. }`, which maps to
//! `CrawlFailed` like any other job failure. This was chosen over adding a
//! `crawl.cancelled` `WebhookEvent` variant to avoid a config schema change
//! for a case indistinguishable, from a webhook subscriber's point of view,
//! from any other terminal failure (the `data.error` field says why).
//!
//! ## Delivery
//!
//! Body: `{"event": "<snake_case>", "job_id", "timestamp", "data": <the
//! CrawlEvent as JSON>}`, plus headers `X-Scrapix-Event` (the same
//! snake_case event name) and `X-Scrapix-Delivery` (a fresh UUID per
//! *delivery*, not per attempt — retries of the same delivery reuse one
//! delivery id so a receiver can dedupe retried attempts).
//!
//! Auth (`WebhookConfig.auth`):
//! - `Bearer { token }` sends `Authorization: Bearer <token>`.
//! - `Headers { headers }` sends each header as-is. Header names/values are
//!   validated at job creation (`validate_webhook_config`): must be
//!   syntactically valid HTTP header names/values, and must not try to
//!   override `Content-Type`, `Host`, or any `X-Scrapix-*` header this
//!   module itself sets.
//! - `Hmac { secret, algorithm, header }` sends `header: sha256=<hex hmac-sha256
//!   of the raw JSON body>`. Only `algorithm == "sha256"` is supported;
//!   anything else is rejected at job creation (`validate_crawl_config`), so
//!   by the time a delivery reaches this module it is always sha256.
//!
//! A disabled hook (`enabled: false`) is skipped entirely, as is a hook not
//! subscribed to the mapped event.
//!
//! `timeout_ms` is clamped to `[MIN_TIMEOUT_MS, MAX_TIMEOUT_MS]` (1s..30s)
//! both at job-creation validation (`validate_crawl_config` mutates the
//! config in place) and again at delivery time (belt-and-braces, in case a
//! `JobState.webhooks` entry ever bypasses that path — e.g. a future config
//! source that skips `validate_crawl_config`).
//!
//! Each delivery gets up to 3 attempts (the first, plus 2 retries),
//! separated by the backoff in [`DEFAULT_BACKOFF`] (1s, then 5s). A network
//! error or 5xx response triggers a retry; a 4xx response is terminal
//! (no retry: the receiver is telling us the request itself is wrong).
//! Attempts beyond the 3rd are not made even if a 5xx keeps recurring.
//!
//! ## Concurrency (head-of-line blocking)
//!
//! A single background task drains the queue, but it does not deliver
//! inline: each queued job is handed to its own `tokio::spawn`'ed task
//! (all attempts/retries/backoff sleeps happen inside that task), bounded
//! by a `Semaphore` with `max_concurrent_deliveries` permits (`enqueue`
//! itself never touches the semaphore — only the drain loop does, right
//! before spawning). This means:
//! - A single slow/blackholed endpoint (e.g. `timeout_ms` at its 30s max)
//!   only occupies one of the concurrent slots; every other job's
//!   deliveries proceed independently rather than queueing up behind it.
//! - The drain loop itself never sleeps: retry backoff sleeps happen
//!   inside the spawned tasks, so popping the next queued job is never
//!   delayed by another job's retry schedule (only by the semaphore
//!   reaching its concurrency cap, which is deliberate backpressure).
//!
//! ## Drop-oldest under a full queue
//!
//! At [`QUEUE_CAPACITY`], `enqueue` must make room for a new delivery by
//! dropping an old one. A naive drop-oldest (evict the front of the queue
//! unconditionally) can starve a small number of tenants who happen to
//! have `crawl_completed`/`crawl_failed` deliveries sitting behind a flood
//! of `progress_update`/`page_*` deliveries from a noisy job. Instead:
//! - `push` first looks for the oldest **prunable** queued delivery
//!   (`progress_update`, `page_crawled`, `page_indexed`, `page_error` — see
//!   [`is_prunable`]) and evicts that.
//! - Only when the entire queue is non-prunable (every queued delivery is
//!   `crawl_started`/`crawl_completed`/`crawl_failed`) does it fall back to
//!   evicting the plain oldest entry.
//!
//! A warning is logged at most once a minute while the queue stays full.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use parking_lot::Mutex;
use serde::Serialize;
use sha2::Sha256;
use tokio::sync::{Notify, Semaphore};
use tracing::warn;

use scrapix_core::{WebhookAuth, WebhookConfig, WebhookEvent};
use scrapix_queue::CrawlEvent;

/// Delivery queue capacity. Past this, an old queued delivery is dropped to
/// make room (see the module docs' "Drop-oldest" section).
const QUEUE_CAPACITY: usize = 10_000;

/// Minimum spacing between two delivered `ProgressUpdate` events for the
/// same job.
const PROGRESS_THROTTLE: Duration = Duration::from_secs(5);

/// Backoff waited before the 2nd and 3rd delivery attempts, respectively.
///
/// The spec lists three backoff values (1s, 5s, 25s) but also says "try
/// each delivery 3 times" (3 attempts total, confirmed by this module's own
/// `retries_on_503_then_gives_up_after_3` test) — 3 attempts have only 2
/// gaps between them, so the third (25s) value has nowhere to go. This
/// implementation uses the first two (1s, 5s) and treats "3 times" as
/// authoritative; if a 3rd retry (4 attempts, using all of 1s/5s/25s) was
/// actually intended, only `DEFAULT_BACKOFF` and the attempt-count constant
/// in `deliver_with_retry` need to change.
const DEFAULT_BACKOFF: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(5)];

/// Total delivery attempts per queued webhook (see [`DEFAULT_BACKOFF`]).
const MAX_ATTEMPTS: u32 = 3;

/// Default number of deliveries allowed to be in flight (across all jobs
/// and hooks) at once. Configurable per `WebhookDispatcher::new`.
pub const DEFAULT_MAX_CONCURRENT_DELIVERIES: usize = 64;

/// Minimum allowed `timeout_ms` (clamped at validation and at delivery).
pub(crate) const MIN_TIMEOUT_MS: u64 = 1_000;
/// Maximum allowed `timeout_ms` (clamped at validation and at delivery).
pub(crate) const MAX_TIMEOUT_MS: u64 = 30_000;

/// Clamp a hook's configured `timeout_ms` into `[MIN_TIMEOUT_MS,
/// MAX_TIMEOUT_MS]` (1s..30s): long enough to be useless as a DoS vector
/// against the shared delivery pool, short enough that a legitimate
/// receiver still has room to respond.
pub(crate) fn clamp_timeout_ms(timeout_ms: u64) -> u64 {
    timeout_ms.clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS)
}

/// Delivers `CrawlEvent`s to a job's subscribed webhooks. Cheap to clone
/// (an `Arc` around the shared queue/client); `enqueue` is non-blocking.
#[derive(Clone)]
pub struct WebhookDispatcher {
    inner: Arc<Inner>,
}

struct Inner {
    client: reqwest::Client,
    backoffs: [Duration; 2],
    capacity: usize,
    semaphore: Arc<Semaphore>,
    queue: Mutex<VecDeque<DeliveryJob>>,
    notify: Notify,
    queue_full_warned_at: Mutex<Option<Instant>>,
    progress_last_sent: Mutex<HashMap<String, Instant>>,
}

struct DeliveryJob {
    hook: WebhookConfig,
    job_id: String,
    event: CrawlEvent,
    /// Precomputed by `enqueue` (which already had to compute it to decide
    /// whether/where to deliver), so `push`'s drop-oldest scan and the
    /// delivery task don't need to re-derive it from `event`.
    mapped: WebhookEvent,
}

/// Events cheap to lose under sustained overload: high-frequency,
/// non-terminal, and each one is superseded by the next (a receiver that
/// missed a `page_crawled` still has an accurate picture of the job from
/// the next one, or from `crawl_completed`'s final counts). Kept safe from
/// eviction as long as any prunable delivery is still queued: `CrawlStarted`,
/// `CrawlCompleted`, `CrawlFailed` (and `BatchSent`, though it's never
/// actually enqueued — see module docs).
fn is_prunable(event: &WebhookEvent) -> bool {
    matches!(
        event,
        WebhookEvent::ProgressUpdate
            | WebhookEvent::PageCrawled
            | WebhookEvent::PageIndexed
            | WebhookEvent::PageError
    )
}

impl WebhookDispatcher {
    /// Build a dispatcher backed by `client` (build it with
    /// `scrapix_crawler::safe_client_builder(None, allow_private)` so
    /// webhook targets go through the same SSRF protections as crawling),
    /// allowing up to `max_concurrent_deliveries` deliveries in flight at
    /// once, and spawn its background drain loop.
    pub fn new(client: reqwest::Client, max_concurrent_deliveries: usize) -> Self {
        Self::build(
            client,
            DEFAULT_BACKOFF,
            max_concurrent_deliveries.max(1),
            QUEUE_CAPACITY,
        )
    }

    /// Same as [`Self::new`] but with a caller-supplied backoff sequence —
    /// used by tests so `retries_on_503_then_gives_up_after_3` doesn't
    /// actually wait 1s + 5s.
    #[cfg(test)]
    pub(crate) fn new_with_backoff(client: reqwest::Client, backoffs: [Duration; 2]) -> Self {
        Self::build(
            client,
            backoffs,
            DEFAULT_MAX_CONCURRENT_DELIVERIES,
            QUEUE_CAPACITY,
        )
    }

    /// Full test constructor: shortened backoff, a small queue `capacity`
    /// (so drop-oldest tests don't need to enqueue thousands of jobs to
    /// fill it) and an explicit concurrency cap (so head-of-line-blocking
    /// tests can prove one slow hook doesn't starve another).
    #[cfg(test)]
    pub(crate) fn new_for_test(
        client: reqwest::Client,
        backoffs: [Duration; 2],
        max_concurrent_deliveries: usize,
        capacity: usize,
    ) -> Self {
        Self::build(client, backoffs, max_concurrent_deliveries.max(1), capacity)
    }

    fn build(
        client: reqwest::Client,
        backoffs: [Duration; 2],
        max_concurrent_deliveries: usize,
        capacity: usize,
    ) -> Self {
        let inner = Arc::new(Inner {
            client,
            backoffs,
            capacity,
            semaphore: Arc::new(Semaphore::new(max_concurrent_deliveries)),
            queue: Mutex::new(VecDeque::new()),
            notify: Notify::new(),
            queue_full_warned_at: Mutex::new(None),
            progress_last_sent: Mutex::new(HashMap::new()),
        });
        tokio::spawn(run_worker(inner.clone()));
        Self { inner }
    }

    /// Enqueue a delivery for `event` to every hook in `hooks` that is
    /// enabled and subscribed to `event`'s mapped `WebhookEvent`. A no-op
    /// for events with no webhook mapping (see the module docs) or a
    /// `ProgressUpdate` arriving inside this job's throttle window.
    ///
    /// `job_id` is the job `event` belongs to. Every `CrawlEvent` variant
    /// already carries its own `job_id` field, but there's no cheap generic
    /// accessor across all 16 variants, so callers (which already have the
    /// job id in scope from `process_event`) pass it explicitly instead.
    pub fn enqueue(&self, hooks: &[WebhookConfig], job_id: &str, event: &CrawlEvent) {
        let Some(mapped) = map_event(event) else {
            return;
        };

        // Prune throttle bookkeeping the moment a job reaches a terminal
        // state, regardless of whether it has any webhooks at all — this
        // is the only place that ever removes an entry, so it must not be
        // gated on `hooks` being non-empty.
        if matches!(
            mapped,
            WebhookEvent::CrawlCompleted | WebhookEvent::CrawlFailed
        ) {
            self.inner.progress_last_sent.lock().remove(job_id);
        }

        if hooks.is_empty() {
            return;
        }
        let matching: Vec<&WebhookConfig> = hooks
            .iter()
            .filter(|h| h.enabled && h.events.contains(&mapped))
            .collect();
        if matching.is_empty() {
            return;
        }

        if mapped == WebhookEvent::ProgressUpdate {
            let now = Instant::now();
            let mut last_sent = self.inner.progress_last_sent.lock();
            if let Some(prev) = last_sent.get(job_id) {
                if now.duration_since(*prev) < PROGRESS_THROTTLE {
                    return;
                }
            }
            last_sent.insert(job_id.to_string(), now);
        }

        for hook in matching {
            self.push(DeliveryJob {
                hook: hook.clone(),
                job_id: job_id.to_string(),
                event: event.clone(),
                mapped: mapped.clone(),
            });
        }
    }

    fn push(&self, job: DeliveryJob) {
        let mut queue = self.inner.queue.lock();
        if queue.len() >= self.inner.capacity {
            // Prefer evicting a prunable (high-frequency, non-terminal)
            // delivery; only fall back to the plain oldest entry when the
            // whole queue is terminal events.
            match queue.iter().position(|j| is_prunable(&j.mapped)) {
                Some(idx) => {
                    queue.remove(idx);
                }
                None => {
                    queue.pop_front();
                }
            }
            let now = Instant::now();
            let mut warned_at = self.inner.queue_full_warned_at.lock();
            let should_warn = match *warned_at {
                Some(t) => now.duration_since(t) >= Duration::from_secs(60),
                None => true,
            };
            if should_warn {
                warn!(
                    capacity = self.inner.capacity,
                    "webhook delivery queue is full; dropping a queued delivery \
                     (prunable ones are dropped first)"
                );
                *warned_at = Some(now);
            }
        }
        queue.push_back(job);
        drop(queue);
        self.inner.notify.notify_one();
    }
}

/// Background loop: pop queued jobs and hand each to its own spawned task,
/// bounded by `inner.semaphore`. Waiting for a permit is the only thing
/// that can pause this loop between pops — it never sleeps for a retry
/// backoff itself (those sleeps live inside the spawned tasks), so a job
/// stuck retrying does not delay the next job from being picked up as long
/// as a concurrency slot is free.
async fn run_worker(inner: Arc<Inner>) {
    loop {
        let next = inner.queue.lock().pop_front();
        let Some(job) = next else {
            inner.notify.notified().await;
            continue;
        };
        // `acquire_owned` on an `Arc<Semaphore>` never returns `Err` unless
        // the semaphore is explicitly closed, which this dispatcher never
        // does.
        let permit = inner
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("webhook delivery semaphore is never closed");
        let client = inner.client.clone();
        let backoffs = inner.backoffs;
        tokio::spawn(async move {
            deliver_with_retry(&client, &backoffs, job).await;
            drop(permit);
        });
    }
}

#[derive(Serialize)]
struct WebhookPayload<'a> {
    event: &'static str,
    job_id: &'a str,
    timestamp: i64,
    data: &'a CrawlEvent,
}

async fn deliver_with_retry(client: &reqwest::Client, backoffs: &[Duration; 2], job: DeliveryJob) {
    let wire_name = wire_name(&job.mapped);
    let payload = WebhookPayload {
        event: wire_name,
        job_id: &job.job_id,
        timestamp: chrono::Utc::now().timestamp_millis(),
        data: &job.event,
    };
    let body = match serde_json::to_vec(&payload) {
        Ok(b) => b,
        Err(e) => {
            warn!(
                error = %e,
                webhook = %scheme_and_host(&job.hook.url),
                "failed to serialize webhook payload; dropping delivery"
            );
            return;
        }
    };

    let delivery_id = uuid::Uuid::new_v4().to_string();
    // Defense in depth: `validate_crawl_config` already clamps this at job
    // creation, but re-clamp here in case a `JobState.webhooks` entry ever
    // reaches delivery through a path that skipped that validation.
    let timeout_ms = clamp_timeout_ms(job.hook.timeout_ms);

    for attempt in 0..MAX_ATTEMPTS {
        let mut req = client
            .post(&job.hook.url)
            .header("Content-Type", "application/json")
            .header("X-Scrapix-Event", wire_name)
            .header("X-Scrapix-Delivery", delivery_id.as_str())
            .timeout(Duration::from_millis(timeout_ms));
        req = apply_auth(req, job.hook.auth.as_ref(), &body);
        req = req.body(body.clone());

        // Only the URL's scheme+host is logged, never the full URL: a
        // webhook URL can carry a capability token in its path or query
        // (e.g. `https://hooks.example.com/t/SECRET-TOKEN`), which must
        // never end up in logs.
        let webhook = scheme_and_host(&job.hook.url);

        match req.send().await {
            Ok(resp) if resp.status().is_success() => return,
            Ok(resp) if resp.status().is_client_error() => {
                warn!(
                    webhook = %webhook,
                    status = %resp.status(),
                    job_id = %job.job_id,
                    "webhook delivery rejected (4xx); not retrying"
                );
                return;
            }
            Ok(resp) => {
                warn!(
                    webhook = %webhook,
                    status = %resp.status(),
                    attempt = attempt + 1,
                    job_id = %job.job_id,
                    "webhook delivery failed (server error)"
                );
            }
            Err(e) => {
                warn!(
                    webhook = %webhook,
                    error = %e,
                    attempt = attempt + 1,
                    job_id = %job.job_id,
                    "webhook delivery failed (network error)"
                );
            }
        }

        if attempt + 1 < MAX_ATTEMPTS {
            tokio::time::sleep(backoffs[attempt as usize]).await;
        }
    }
    warn!(
        webhook = %scheme_and_host(&job.hook.url),
        job_id = %job.job_id,
        attempts = MAX_ATTEMPTS,
        "webhook delivery exhausted all retries; giving up"
    );
}

/// `scheme://host[:port]` of a webhook URL, safe to log. A webhook URL's
/// path or query can carry a capability token (many webhook providers put
/// the secret there instead of, or in addition to, an auth header), so the
/// full URL must never be logged.
fn scheme_and_host(url_str: &str) -> String {
    match url::Url::parse(url_str) {
        Ok(u) => match u.port() {
            Some(port) => format!("{}://{}:{port}", u.scheme(), u.host_str().unwrap_or("?")),
            None => format!("{}://{}", u.scheme(), u.host_str().unwrap_or("?")),
        },
        Err(_) => "<invalid-url>".to_string(),
    }
}

fn apply_auth(
    req: reqwest::RequestBuilder,
    auth: Option<&WebhookAuth>,
    body: &[u8],
) -> reqwest::RequestBuilder {
    match auth {
        None => req,
        Some(WebhookAuth::Bearer { token }) => req.bearer_auth(token),
        Some(WebhookAuth::Headers { headers }) => {
            let mut req = req;
            for (name, value) in headers {
                req = req.header(name, value);
            }
            req
        }
        Some(WebhookAuth::Hmac {
            secret,
            algorithm,
            header,
        }) => {
            if algorithm != "sha256" {
                // Validated away at job creation (`validate_crawl_config`);
                // if it somehow gets here, skip the signature rather than
                // panic or send a bogus one.
                warn!(algorithm = %algorithm, "unsupported HMAC algorithm; sending webhook unsigned");
                return req;
            }
            let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret.as_bytes())
                .expect("HMAC-SHA256 accepts any key length");
            mac.update(body);
            let signature = hex::encode(mac.finalize().into_bytes());
            req.header(header.as_str(), format!("sha256={signature}"))
        }
    }
}

fn map_event(event: &CrawlEvent) -> Option<WebhookEvent> {
    match event {
        CrawlEvent::JobStarted { .. } => Some(WebhookEvent::CrawlStarted),
        CrawlEvent::JobCompleted { .. } => Some(WebhookEvent::CrawlCompleted),
        CrawlEvent::JobFailed { .. } => Some(WebhookEvent::CrawlFailed),
        CrawlEvent::PageCrawled { .. } => Some(WebhookEvent::PageCrawled),
        CrawlEvent::DocumentIndexed { .. } => Some(WebhookEvent::PageIndexed),
        CrawlEvent::PageFailed { .. } | CrawlEvent::DocumentFailed { .. } => {
            Some(WebhookEvent::PageError)
        }
        CrawlEvent::FrontierProgress { .. } => Some(WebhookEvent::ProgressUpdate),
        _ => None,
    }
}

fn wire_name(event: &WebhookEvent) -> &'static str {
    match event {
        WebhookEvent::CrawlStarted => "crawl_started",
        WebhookEvent::CrawlCompleted => "crawl_completed",
        WebhookEvent::CrawlFailed => "crawl_failed",
        WebhookEvent::ProgressUpdate => "progress_update",
        WebhookEvent::PageCrawled => "page_crawled",
        WebhookEvent::PageIndexed => "page_indexed",
        WebhookEvent::PageError => "page_error",
        WebhookEvent::BatchSent => "batch_sent",
    }
}

/// Validate a webhook URL the same way proxy URLs are validated
/// (`lib.rs::validate_proxy_config`): must be `http`/`https`, and if the
/// host is a raw IP literal, it must be public. Hostnames are re-checked at
/// delivery time by `SafeResolver` (via `safe_client_builder`).
pub(crate) fn validate_webhook_url(url_str: &str) -> Result<(), String> {
    let parsed =
        url::Url::parse(url_str).map_err(|e| format!("invalid webhook URL '{url_str}': {e}"))?;
    match parsed.scheme() {
        "http" | "https" => {}
        other => {
            return Err(format!(
                "unsupported webhook URL scheme '{other}' in '{url_str}' (use http or https)"
            ))
        }
    }
    let ip = match parsed.host() {
        Some(url::Host::Ipv4(ip)) => Some(std::net::IpAddr::V4(ip)),
        Some(url::Host::Ipv6(ip)) => Some(std::net::IpAddr::V6(ip)),
        Some(url::Host::Domain(_)) => None,
        None => return Err(format!("webhook URL '{url_str}' has no host")),
    };
    if let Some(ip) = ip {
        if !scrapix_crawler::is_public_ip(ip) {
            return Err(format!(
                "webhook URL '{url_str}' points to a non-public address"
            ));
        }
    }
    Ok(())
}

/// Header names (case-insensitive) and prefixes this module itself sets on
/// every delivery, or that would otherwise change the request in ways a
/// webhook author shouldn't be able to (the body's content type, or the
/// destination host). A custom `Headers` auth entry may not override any
/// of these.
fn is_reserved_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "content-type" || lower == "host" || lower.starts_with("x-scrapix-")
}

/// Validate one custom auth header's name and value are syntactically
/// legal HTTP (so an invalid one fails job creation with a clear error,
/// instead of silently failing — and burning 3 retries — at delivery
/// time), and that it isn't one of [`is_reserved_header`]'s reserved names.
fn validate_custom_header(name: &str, value: &str) -> Result<(), String> {
    reqwest::header::HeaderName::from_bytes(name.as_bytes())
        .map_err(|e| format!("invalid webhook header name '{name}': {e}"))?;
    reqwest::header::HeaderValue::from_str(value)
        .map_err(|e| format!("invalid webhook header value for '{name}': {e}"))?;
    if is_reserved_header(name) {
        return Err(format!(
            "webhook header '{name}' is reserved (set by the delivery itself) and cannot be overridden"
        ));
    }
    Ok(())
}

/// Validate a single webhook config at job-creation time: the URL (see
/// [`validate_webhook_url`]), that HMAC auth only ever requests the one
/// algorithm this module implements, and that `Headers` auth doesn't try
/// to smuggle in an invalid or reserved header. Does **not** validate
/// `timeout_ms` — that's clamped, not rejected (see [`clamp_timeout_ms`]),
/// by the caller (`lib.rs::validate_crawl_config`) before this runs.
pub(crate) fn validate_webhook_config(hook: &WebhookConfig) -> Result<(), String> {
    validate_webhook_url(&hook.url)?;
    match &hook.auth {
        Some(WebhookAuth::Hmac { algorithm, .. }) if algorithm != "sha256" => {
            return Err(format!(
                "unsupported HMAC algorithm '{algorithm}' for webhook '{}' (only sha256 is supported)",
                hook.url
            ));
        }
        Some(WebhookAuth::Headers { headers }) => {
            for (name, value) in headers {
                validate_custom_header(name, value)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Redact webhook auth secrets in a JSON-serialized `CrawlConfig` before it
/// is persisted or ever handed back over the API — the same treatment
/// `meilisearch.api_key` already gets. `config_json` is mutated in place;
/// no-op if there's no `webhooks` array or it isn't shaped as expected.
pub(crate) fn redact_webhooks_json(config_json: &mut serde_json::Value) {
    let Some(hooks) = config_json
        .get_mut("webhooks")
        .and_then(|w| w.as_array_mut())
    else {
        return;
    };
    for hook in hooks.iter_mut() {
        let Some(auth) = hook.get_mut("auth").and_then(|a| a.as_object_mut()) else {
            continue;
        };
        if let Some(bearer) = auth.get_mut("bearer").and_then(|b| b.as_object_mut()) {
            bearer.insert(
                "token".to_string(),
                serde_json::Value::String("***".to_string()),
            );
        }
        if let Some(hmac) = auth.get_mut("hmac").and_then(|b| b.as_object_mut()) {
            hmac.insert(
                "secret".to_string(),
                serde_json::Value::String("***".to_string()),
            );
        }
        if let Some(headers_variant) = auth.get_mut("headers").and_then(|b| b.as_object_mut()) {
            if let Some(headers) = headers_variant
                .get_mut("headers")
                .and_then(|h| h.as_object_mut())
            {
                for (_, value) in headers.iter_mut() {
                    *value = serde_json::Value::String("***".to_string());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scrapix_core::WebhookConfig;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    fn client() -> reqwest::Client {
        scrapix_crawler::safe_client_builder(None, true)
            .build()
            .unwrap()
    }

    fn dispatcher() -> WebhookDispatcher {
        WebhookDispatcher::new_with_backoff(
            client(),
            [Duration::from_millis(20), Duration::from_millis(20)],
        )
    }

    fn hook(url: &str, events: Vec<WebhookEvent>) -> WebhookConfig {
        WebhookConfig {
            url: url.to_string(),
            events,
            auth: None,
            enabled: true,
            timeout_ms: 5_000,
            name: None,
        }
    }

    /// Poll `count()` until it reaches `expected`, bounded (2s total)
    /// instead of a fixed sleep — used everywhere a test used to
    /// `sleep(Duration::from_millis(N))` and hope N was enough.
    async fn wait_for_count(count: impl Fn() -> usize, expected: usize) {
        for _ in 0..200 {
            if count() >= expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "timed out waiting for {expected} deliveries, got {}",
            count()
        );
    }

    async fn received_count(server: &MockServer) -> usize {
        server.received_requests().await.unwrap().len()
    }

    async fn wait_until_received(server: &MockServer, expected: usize) {
        for _ in 0..200 {
            if received_count(server).await >= expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "timed out waiting for {expected} requests, got {}",
            received_count(server).await
        );
    }

    fn completed_event(job_id: &str) -> CrawlEvent {
        CrawlEvent::JobCompleted {
            job_id: job_id.to_string(),
            account_id: None,
            pages_crawled: 3,
            documents_indexed: 3,
            errors: 0,
            bytes_downloaded: 100,
            duration_secs: 1,
            timestamp: chrono::Utc::now().timestamp_millis(),
        }
    }

    #[tokio::test]
    async fn delivers_completed_event_with_auth_header() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .and(header("Authorization", "Bearer secret-token"))
            .and(header("X-Scrapix-Event", "crawl_completed"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let dispatcher = dispatcher();
        let mut h = hook(
            &format!("{}/hook", server.uri()),
            vec![WebhookEvent::CrawlCompleted],
        );
        h.auth = Some(WebhookAuth::Bearer {
            token: "secret-token".to_string(),
        });

        dispatcher.enqueue(&[h], "job-1", &completed_event("job-1"));

        wait_until_received(&server, 1).await;
    }

    #[tokio::test]
    async fn hmac_signature_matches_body() {
        let server = MockServer::start().await;
        let secret = "shh";

        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(move |req: &Request| {
                let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret.as_bytes()).unwrap();
                mac.update(&req.body);
                let expected = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
                let got = req
                    .headers
                    .get("X-Scrapix-Signature")
                    .map(|v| v.to_str().unwrap().to_string());
                assert_eq!(got, Some(expected), "HMAC signature must match the body");
                ResponseTemplate::new(200)
            })
            .expect(1)
            .mount(&server)
            .await;

        let dispatcher = dispatcher();
        let mut h = hook(
            &format!("{}/hook", server.uri()),
            vec![WebhookEvent::CrawlCompleted],
        );
        h.auth = Some(WebhookAuth::Hmac {
            secret: secret.to_string(),
            algorithm: "sha256".to_string(),
            header: "X-Scrapix-Signature".to_string(),
        });

        dispatcher.enqueue(&[h], "job-1", &completed_event("job-1"));
        wait_until_received(&server, 1).await;
    }

    #[tokio::test]
    async fn retries_on_503_then_gives_up_after_3() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(503))
            .expect(3)
            .mount(&server)
            .await;

        let dispatcher = dispatcher();
        let h = hook(
            &format!("{}/hook", server.uri()),
            vec![WebhookEvent::CrawlCompleted],
        );
        dispatcher.enqueue(&[h], "job-1", &completed_event("job-1"));

        wait_until_received(&server, 3).await;
    }

    #[tokio::test]
    async fn does_not_retry_4xx() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(400))
            .expect(1)
            .mount(&server)
            .await;

        let dispatcher = dispatcher();
        let h = hook(
            &format!("{}/hook", server.uri()),
            vec![WebhookEvent::CrawlCompleted],
        );
        dispatcher.enqueue(&[h], "job-1", &completed_event("job-1"));

        wait_until_received(&server, 1).await;
        // Give a would-be (wrong) retry a chance to show up before asserting
        // there isn't one.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(received_count(&server).await, 1, "4xx must not be retried");
    }

    #[tokio::test]
    async fn skips_disabled_and_unsubscribed_hooks() {
        let server = MockServer::start().await;
        // No mocks mounted at all: any request is a hard failure (wiremock
        // returns 404 for unmatched requests, but we assert zero calls via
        // the request log instead of relying on that).
        let dispatcher = dispatcher();

        let mut disabled = hook(
            &format!("{}/hook", server.uri()),
            vec![WebhookEvent::CrawlCompleted],
        );
        disabled.enabled = false;
        let unsubscribed = hook(
            &format!("{}/hook", server.uri()),
            vec![WebhookEvent::PageCrawled], // not CrawlCompleted
        );

        dispatcher.enqueue(
            &[disabled, unsubscribed],
            "job-1",
            &completed_event("job-1"),
        );

        // There's nothing to wait for a positive signal on (nothing should
        // ever arrive), so give the (nonexistent) delivery a window to show
        // up before asserting it didn't.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "disabled/unsubscribed hooks must not receive any request"
        );
    }

    #[tokio::test]
    async fn progress_updates_are_throttled() {
        let server = MockServer::start().await;
        let count = Arc::new(AtomicUsize::new(0));
        let count_clone = count.clone();
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(move |_: &Request| {
                count_clone.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(200)
            })
            .mount(&server)
            .await;

        let dispatcher = dispatcher();
        let h = hook(
            &format!("{}/hook", server.uri()),
            vec![WebhookEvent::ProgressUpdate],
        );

        let progress = |job_id: &str| CrawlEvent::FrontierProgress {
            job_id: job_id.to_string(),
            instance_id: "i1".to_string(),
            received: 1,
            admitted: 1,
            dispatched: 1,
            rejected: 0,
            dropped: 0,
            queued: 0,
            timestamp: chrono::Utc::now().timestamp_millis(),
        };

        // Five rapid FrontierProgress events for the same job within the
        // 5s throttle window: only the first is delivered.
        for _ in 0..5 {
            dispatcher.enqueue(std::slice::from_ref(&h), "job-1", &progress("job-1"));
        }

        wait_for_count(|| count.load(Ordering::SeqCst), 1).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "only the first ProgressUpdate in the throttle window should be delivered"
        );
    }

    /// SCR-72 fix round 1, item 1: a hook that never responds must not
    /// delay another job's delivery. Job A's hook sleeps far longer than
    /// this test's window; job B's hook is instant. With per-delivery
    /// spawned tasks (bounded by a semaphore well above 2), B must not wait
    /// behind A.
    #[tokio::test]
    async fn slow_hook_does_not_delay_another_jobs_delivery() {
        let slow_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
            .mount(&slow_server)
            .await;

        let fast_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&fast_server)
            .await;

        let dispatcher = WebhookDispatcher::new_for_test(
            client(),
            [Duration::from_millis(20), Duration::from_millis(20)],
            8,
            100,
        );

        let slow_hook = hook(
            &format!("{}/hook", slow_server.uri()),
            vec![WebhookEvent::CrawlCompleted],
        );
        let fast_hook = hook(
            &format!("{}/hook", fast_server.uri()),
            vec![WebhookEvent::CrawlCompleted],
        );

        dispatcher.enqueue(&[slow_hook], "job-slow", &completed_event("job-slow"));
        dispatcher.enqueue(&[fast_hook], "job-fast", &completed_event("job-fast"));

        // If delivery were still serial/inline, this would have to wait
        // out job-slow's 5s delay first. Bounded well under that proves
        // job-fast wasn't stuck behind it.
        tokio::time::timeout(Duration::from_secs(2), wait_until_received(&fast_server, 1))
            .await
            .expect("job-fast's delivery must not be blocked by job-slow's slow hook");
    }

    /// SCR-72 fix round 1, item 1: under a full queue, `progress_update`
    /// deliveries are dropped before `crawl_completed`/`crawl_failed` ones.
    #[tokio::test]
    async fn full_queue_keeps_terminal_events() {
        // A tiny capacity so the test doesn't need to enqueue thousands of
        // deliveries, and concurrency 0-ish (1, the minimum) so nothing
        // actually drains while we fill the queue up — the dispatcher's
        // client points at a server that never responds in time, keeping
        // whatever *does* get picked up occupied rather than freeing a
        // queue slot mid-test.
        let stalling_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(30)))
            .mount(&stalling_server)
            .await;

        let dispatcher = WebhookDispatcher::new_for_test(
            client(),
            [Duration::from_millis(20), Duration::from_millis(20)],
            1,
            4,
        );

        let progress_hook = hook(
            &format!("{}/hook", stalling_server.uri()),
            vec![WebhookEvent::ProgressUpdate],
        );
        let terminal_hook = hook(
            &format!("{}/hook", stalling_server.uri()),
            vec![WebhookEvent::CrawlCompleted, WebhookEvent::CrawlFailed],
        );

        let progress = |job_id: &str| CrawlEvent::FrontierProgress {
            job_id: job_id.to_string(),
            instance_id: "i1".to_string(),
            received: 1,
            admitted: 1,
            dispatched: 1,
            rejected: 0,
            dropped: 0,
            queued: 0,
            timestamp: chrono::Utc::now().timestamp_millis(),
        };

        // Fill the 4-slot queue: 1 terminal event for job-a, then enough
        // progress_update noise (from other jobs, so the throttle doesn't
        // suppress them) to overflow past capacity.
        dispatcher.enqueue(
            &[terminal_hook],
            "job-a",
            &CrawlEvent::JobCompleted {
                job_id: "job-a".to_string(),
                account_id: None,
                pages_crawled: 1,
                documents_indexed: 1,
                errors: 0,
                bytes_downloaded: 1,
                duration_secs: 1,
                timestamp: chrono::Utc::now().timestamp_millis(),
            },
        );
        for i in 0..10 {
            dispatcher.enqueue(
                std::slice::from_ref(&progress_hook),
                &format!("job-noise-{i}"),
                &progress(&format!("job-noise-{i}")),
            );
        }

        // The queue held at most 4 at once and is fed faster than the
        // single (stalled) worker can drain it, so by now it settled at
        // capacity with progress_update entries evicted first. Assert the
        // terminal job's delivery is still queued (or already picked up —
        // either way it was never evicted) by checking the stalling
        // server eventually receives job-a's request, not one of the
        // later, higher-numbered noise jobs that should have been dropped
        // to make room instead.
        //
        // A direct queue inspection isn't exposed publicly, so this is
        // asserted behaviorally: give the single worker enough pops to get
        // through the queue (it can only have room for a handful given
        // capacity 4), then check job-a's request did arrive.
        wait_until_received(&stalling_server, 1).await;
        let reqs = stalling_server.received_requests().await.unwrap();
        assert!(
            reqs.iter().any(|r| {
                r.headers
                    .get("X-Scrapix-Event")
                    .map(|v| v == "crawl_completed")
                    .unwrap_or(false)
            }),
            "job-a's crawl_completed must have survived the full queue"
        );
    }

    #[test]
    fn validate_webhook_url_rejects_private_ip() {
        for url in [
            "http://127.0.0.1/hook",
            "http://10.0.0.5/hook",
            "http://169.254.169.254/hook",
            "ftp://example.com/hook",
        ] {
            assert!(
                validate_webhook_url(url).is_err(),
                "{url} should have been rejected"
            );
        }
    }

    #[test]
    fn validate_webhook_url_accepts_public_https() {
        assert!(validate_webhook_url("https://example.com/hook").is_ok());
    }

    #[test]
    fn validate_webhook_config_rejects_non_sha256_hmac() {
        let mut h = hook(
            "https://example.com/hook",
            vec![WebhookEvent::CrawlCompleted],
        );
        h.auth = Some(WebhookAuth::Hmac {
            secret: "s".to_string(),
            algorithm: "sha1".to_string(),
            header: "X-Sig".to_string(),
        });
        assert!(validate_webhook_config(&h).is_err());
    }

    #[test]
    fn validate_webhook_config_rejects_reserved_header_override() {
        for name in ["Content-Type", "content-type", "Host", "X-Scrapix-Event"] {
            let mut headers = HashMap::new();
            headers.insert(name.to_string(), "whatever".to_string());
            let mut h = hook(
                "https://example.com/hook",
                vec![WebhookEvent::CrawlCompleted],
            );
            h.auth = Some(WebhookAuth::Headers { headers });
            assert!(
                validate_webhook_config(&h).is_err(),
                "{name} must be rejected as a reserved header"
            );
        }
    }

    #[test]
    fn validate_webhook_config_rejects_invalid_header_name() {
        let mut headers = HashMap::new();
        headers.insert("bad header\nname".to_string(), "v".to_string());
        let mut h = hook(
            "https://example.com/hook",
            vec![WebhookEvent::CrawlCompleted],
        );
        h.auth = Some(WebhookAuth::Headers { headers });
        assert!(validate_webhook_config(&h).is_err());
    }

    #[test]
    fn validate_webhook_config_accepts_custom_non_reserved_header() {
        let mut headers = HashMap::new();
        headers.insert("X-Api-Key".to_string(), "abc".to_string());
        let mut h = hook(
            "https://example.com/hook",
            vec![WebhookEvent::CrawlCompleted],
        );
        h.auth = Some(WebhookAuth::Headers { headers });
        assert!(validate_webhook_config(&h).is_ok());
    }

    #[test]
    fn clamp_timeout_ms_bounds_both_directions() {
        assert_eq!(clamp_timeout_ms(0), MIN_TIMEOUT_MS);
        assert_eq!(clamp_timeout_ms(500), MIN_TIMEOUT_MS);
        assert_eq!(clamp_timeout_ms(5_000), 5_000);
        assert_eq!(clamp_timeout_ms(60_000), MAX_TIMEOUT_MS);
    }

    #[test]
    fn scheme_and_host_strips_path_and_query() {
        assert_eq!(
            scheme_and_host("https://hooks.example.com/t/SECRET-TOKEN?x=1"),
            "https://hooks.example.com"
        );
        assert_eq!(
            scheme_and_host("http://example.com:8080/hook?token=abc"),
            "http://example.com:8080"
        );
    }

    #[test]
    fn redact_webhooks_json_masks_all_auth_variants() {
        let mut v = serde_json::json!({
            "webhooks": [
                {"url": "https://a.test", "events": [], "enabled": true, "timeout_ms": 1000,
                 "auth": {"bearer": {"token": "top-secret"}}},
                {"url": "https://b.test", "events": [], "enabled": true, "timeout_ms": 1000,
                 "auth": {"hmac": {"secret": "top-secret", "algorithm": "sha256", "header": "X-Sig"}}},
                {"url": "https://c.test", "events": [], "enabled": true, "timeout_ms": 1000,
                 "auth": {"headers": {"headers": {"X-Api-Key": "top-secret"}}}},
            ]
        });
        redact_webhooks_json(&mut v);
        let hooks = v["webhooks"].as_array().unwrap();
        assert_eq!(hooks[0]["auth"]["bearer"]["token"], "***");
        assert_eq!(hooks[1]["auth"]["hmac"]["secret"], "***");
        assert_eq!(hooks[2]["auth"]["headers"]["headers"]["X-Api-Key"], "***");
    }

    #[test]
    fn webhook_auth_debug_redacts_secrets() {
        let bearer = WebhookAuth::Bearer {
            token: "top-secret".to_string(),
        };
        assert!(!format!("{bearer:?}").contains("top-secret"));

        let hmac = WebhookAuth::Hmac {
            secret: "top-secret".to_string(),
            algorithm: "sha256".to_string(),
            header: "X-Sig".to_string(),
        };
        let hmac_dbg = format!("{hmac:?}");
        assert!(!hmac_dbg.contains("top-secret"));
        assert!(hmac_dbg.contains("sha256"), "non-secret fields still show");

        let mut headers = HashMap::new();
        headers.insert("X-Api-Key".to_string(), "top-secret".to_string());
        let headers_auth = WebhookAuth::Headers { headers };
        let headers_dbg = format!("{headers_auth:?}");
        assert!(!headers_dbg.contains("top-secret"));
        assert!(
            headers_dbg.contains("X-Api-Key"),
            "header names aren't secret and still show"
        );
    }
}
