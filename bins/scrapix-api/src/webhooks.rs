//! Crawl webhook delivery (SCR-72).
//!
//! Maps `CrawlEvent`s emitted by the pipeline to the `WebhookEvent`s a job's
//! `CrawlConfig.webhooks` can subscribe to, and delivers each matching,
//! enabled subscription as a signed HTTP POST off the event-processing
//! path: `enqueue` never awaits or touches the network, it only pushes onto
//! a bounded in-memory queue that a background task drains.
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
//! arrive in that window or how many hooks are subscribed.
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
//! attempt... note: per *delivery*, not per attempt — retries of the same
//! delivery reuse one delivery id so a receiver can dedupe retried
//! attempts).
//!
//! Auth (`WebhookConfig.auth`):
//! - `Bearer { token }` sends `Authorization: Bearer <token>`.
//! - `Headers { headers }` sends each header as-is.
//! - `Hmac { secret, algorithm, header }` sends `header: sha256=<hex hmac-sha256
//!   of the raw JSON body>`. Only `algorithm == "sha256"` is supported;
//!   anything else is rejected at job creation (`validate_crawl_config`), so
//!   by the time a delivery reaches this module it is always sha256.
//!
//! A disabled hook (`enabled: false`) is skipped entirely, as is a hook not
//! subscribed to the mapped event.
//!
//! Each delivery gets up to 3 attempts (the first, plus 2 retries),
//! separated by the backoff in [`DEFAULT_BACKOFF`] (1s, then 5s). A network
//! error or 5xx response triggers a retry; a 4xx response is terminal
//! (no retry: the receiver is telling us the request itself is wrong).
//! Attempts beyond the 3rd are not made even if a 5xx keeps recurring.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use parking_lot::Mutex;
use serde::Serialize;
use sha2::Sha256;
use tokio::sync::Notify;
use tracing::warn;

use scrapix_core::{WebhookAuth, WebhookConfig, WebhookEvent};
use scrapix_queue::CrawlEvent;

/// Delivery queue capacity. Past this, the oldest queued delivery is
/// dropped (drop-oldest) to make room, with a warning logged at most once a
/// minute.
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

/// Delivers `CrawlEvent`s to a job's subscribed webhooks. Cheap to clone
/// (an `Arc` around the shared queue/client); `enqueue` is non-blocking.
#[derive(Clone)]
pub struct WebhookDispatcher {
    inner: Arc<Inner>,
}

struct Inner {
    client: reqwest::Client,
    backoffs: [Duration; 2],
    queue: Mutex<VecDeque<DeliveryJob>>,
    notify: Notify,
    queue_full_warned_at: Mutex<Option<Instant>>,
    progress_last_sent: Mutex<HashMap<String, Instant>>,
}

struct DeliveryJob {
    hook: WebhookConfig,
    job_id: String,
    event: CrawlEvent,
}

impl WebhookDispatcher {
    /// Build a dispatcher backed by `client` (build it with
    /// `scrapix_crawler::safe_client_builder(None, allow_private)` so
    /// webhook targets go through the same SSRF protections as crawling)
    /// and spawn its background delivery worker.
    pub fn new(client: reqwest::Client) -> Self {
        Self::new_with_backoff(client, DEFAULT_BACKOFF)
    }

    /// Same as [`Self::new`] but with a caller-supplied backoff sequence —
    /// used by tests so `retries_on_503_then_gives_up_after_3` doesn't
    /// actually wait 1s + 5s.
    pub(crate) fn new_with_backoff(client: reqwest::Client, backoffs: [Duration; 2]) -> Self {
        let inner = Arc::new(Inner {
            client,
            backoffs,
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
        if hooks.is_empty() {
            return;
        }
        let Some(mapped) = map_event(event) else {
            return;
        };

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

        for hook in hooks {
            if !hook.enabled || !hook.events.contains(&mapped) {
                continue;
            }
            self.push(DeliveryJob {
                hook: hook.clone(),
                job_id: job_id.to_string(),
                event: event.clone(),
            });
        }
    }

    fn push(&self, job: DeliveryJob) {
        let mut queue = self.inner.queue.lock();
        if queue.len() >= QUEUE_CAPACITY {
            queue.pop_front();
            let now = Instant::now();
            let mut warned_at = self.inner.queue_full_warned_at.lock();
            let should_warn = match *warned_at {
                Some(t) => now.duration_since(t) >= Duration::from_secs(60),
                None => true,
            };
            if should_warn {
                warn!(
                    capacity = QUEUE_CAPACITY,
                    "webhook delivery queue is full; dropping the oldest queued delivery"
                );
                *warned_at = Some(now);
            }
        }
        queue.push_back(job);
        drop(queue);
        self.inner.notify.notify_one();
    }
}

/// Background loop: pop and deliver queued jobs one at a time, sleeping
/// (via `Notify`) when the queue is empty. A single worker keeps delivery
/// ordering roughly FIFO and simple; delivery latency does not affect event
/// processing since `enqueue` never waits on this loop.
async fn run_worker(inner: Arc<Inner>) {
    loop {
        let next = inner.queue.lock().pop_front();
        let Some(job) = next else {
            inner.notify.notified().await;
            continue;
        };
        deliver_with_retry(&inner.client, &inner.backoffs, job).await;
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
    let mapped = match map_event(&job.event) {
        Some(m) => m,
        None => return, // unreachable: enqueue already filtered this out
    };
    let wire_name = wire_name(&mapped);
    let payload = WebhookPayload {
        event: wire_name,
        job_id: &job.job_id,
        timestamp: chrono::Utc::now().timestamp_millis(),
        data: &job.event,
    };
    let body = match serde_json::to_vec(&payload) {
        Ok(b) => b,
        Err(e) => {
            warn!(error = %e, url = %job.hook.url, "failed to serialize webhook payload; dropping delivery");
            return;
        }
    };

    let delivery_id = uuid::Uuid::new_v4().to_string();

    for attempt in 0..MAX_ATTEMPTS {
        let mut req = client
            .post(&job.hook.url)
            .header("Content-Type", "application/json")
            .header("X-Scrapix-Event", wire_name)
            .header("X-Scrapix-Delivery", delivery_id.as_str())
            .timeout(Duration::from_millis(job.hook.timeout_ms));
        req = apply_auth(req, job.hook.auth.as_ref(), &body);
        req = req.body(body.clone());

        match req.send().await {
            Ok(resp) if resp.status().is_success() => return,
            Ok(resp) if resp.status().is_client_error() => {
                warn!(
                    url = %job.hook.url,
                    status = %resp.status(),
                    job_id = %job.job_id,
                    "webhook delivery rejected (4xx); not retrying"
                );
                return;
            }
            Ok(resp) => {
                warn!(
                    url = %job.hook.url,
                    status = %resp.status(),
                    attempt = attempt + 1,
                    job_id = %job.job_id,
                    "webhook delivery failed (server error)"
                );
            }
            Err(e) => {
                warn!(
                    url = %job.hook.url,
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
    warn!(url = %job.hook.url, job_id = %job.job_id, attempts = MAX_ATTEMPTS, "webhook delivery exhausted all retries; giving up");
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

/// Validate a single webhook config at job-creation time: the URL (see
/// [`validate_webhook_url`]) and, for HMAC auth, that the algorithm is the
/// only one this module implements.
pub(crate) fn validate_webhook_config(hook: &WebhookConfig) -> Result<(), String> {
    validate_webhook_url(&hook.url)?;
    if let Some(WebhookAuth::Hmac { algorithm, .. }) = &hook.auth {
        if algorithm != "sha256" {
            return Err(format!(
                "unsupported HMAC algorithm '{algorithm}' for webhook '{}' (only sha256 is supported)",
                hook.url
            ));
        }
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

    async fn wait_for(count: impl Fn() -> usize, expected: usize) {
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

        // wiremock verifies `.expect(1)` on drop; give the worker time to
        // deliver before the server (and its expectation check) goes away.
        tokio::time::sleep(Duration::from_millis(300)).await;
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
        tokio::time::sleep(Duration::from_millis(300)).await;
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

        // 3 attempts, ~20ms backoff between each: plenty of margin.
        tokio::time::sleep(Duration::from_millis(500)).await;
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

        tokio::time::sleep(Duration::from_millis(300)).await;
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

        tokio::time::sleep(Duration::from_millis(200)).await;
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

        wait_for(|| count.load(Ordering::SeqCst), 1).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "only the first ProgressUpdate in the throttle window should be delivered"
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
}
