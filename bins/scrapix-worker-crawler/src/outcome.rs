//! Pure classification of a fetch result into what the worker does next.

use std::time::Duration;

use scrapix_core::ScrapixError;
use scrapix_crawler::FetchResult;

/// What the crawler worker does with one fetched URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// 2xx: publish page + discovered links
    Crawled,
    /// 304 under incremental crawling
    NotModified,
    /// Retry later with retry_count + 1 (network errors, retryable status after in-fetch retries)
    Retry { reason: String, delay: Duration },
    /// Terminal failure (4xx, robots, unsupported content, retries exhausted)
    Failed { reason: String, status: Option<u16> },
}

/// HTTP statuses retried by re-queueing (after the fetcher's own in-process
/// retries were exhausted).
pub const RETRYABLE_STATUSES: [u16; 5] = [429, 500, 502, 503, 504];

/// Base delay before a re-queued URL may be dispatched again; doubled per
/// retry (`30s * 2^retry_count`).
pub const BASE_RETRY_DELAY: Duration = Duration::from_secs(30);

/// Upper bound on any re-queue delay, including a server's `Retry-After`.
pub const MAX_RETRY_DELAY: Duration = Duration::from_secs(600);

/// Classify one fetch result.
///
/// - `Ok(Fetched(p))`: 2xx → `Crawled`; 429/500/502/503/504 → `Retry` while
///   `retry_count < max_retries` (delay = `Retry-After` or exponential
///   backoff, capped at [`MAX_RETRY_DELAY`]), else `Failed{status}`; any
///   other status → `Failed{status}`.
/// - `Ok(NotModified)` → `NotModified`.
/// - Transient errors (timeout, connection, network, retryable HTTP error,
///   rate limited) → `Retry`/`Failed` like 5xx.
/// - Everything else (robots, SSRF refusal, unsupported content, body too
///   large, invalid config/URL) → `Failed{status: None}` (terminal).
pub fn classify(
    result: &scrapix_core::Result<FetchResult>,
    retry_count: u32,
    max_retries: u32,
) -> Outcome {
    let retry_or_fail = |reason: String, status: Option<u16>, hint: Option<Duration>| {
        if retry_count < max_retries {
            Outcome::Retry {
                reason,
                delay: hint
                    .unwrap_or_else(|| backoff_delay(retry_count))
                    .min(MAX_RETRY_DELAY),
            }
        } else {
            Outcome::Failed {
                reason: format!("{reason} (retries exhausted after {retry_count} attempts)"),
                status,
            }
        }
    };

    match result {
        Ok(FetchResult::NotModified { .. }) => Outcome::NotModified,
        Ok(FetchResult::Fetched(page)) if page.is_success() => Outcome::Crawled,
        Ok(FetchResult::Fetched(page)) => {
            let status = page.status;
            if RETRYABLE_STATUSES.contains(&status) {
                let hint = page
                    .headers
                    .get("retry-after")
                    .and_then(|v| scrapix_crawler::parse_retry_after(v, chrono::Utc::now()));
                retry_or_fail(format!("HTTP {status}"), Some(status), hint)
            } else {
                Outcome::Failed {
                    reason: format!("HTTP {status}"),
                    status: Some(status),
                }
            }
        }
        Err(e) if is_transient(e) => {
            let status = match e {
                ScrapixError::Http { status, .. } => Some(*status),
                _ => None,
            };
            let hint = match e {
                ScrapixError::RateLimited { retry_after_secs } => {
                    Some(Duration::from_secs(*retry_after_secs))
                }
                _ => None,
            };
            retry_or_fail(e.to_string(), status, hint)
        }
        Err(e) => Outcome::Failed {
            reason: e.to_string(),
            status: match e {
                ScrapixError::Http { status, .. } => Some(*status),
                _ => None,
            },
        },
    }
}

/// Whether `result` is the kind of failure that is retried (so a terminal
/// `Failed` for it means the retry budget was exhausted → dead-letter queue).
pub fn is_retryable(result: &scrapix_core::Result<FetchResult>) -> bool {
    match result {
        Ok(FetchResult::Fetched(page)) => RETRYABLE_STATUSES.contains(&page.status),
        Ok(FetchResult::NotModified { .. }) => false,
        Err(e) => is_transient(e),
    }
}

fn is_transient(e: &ScrapixError) -> bool {
    match e {
        ScrapixError::Timeout(_)
        | ScrapixError::Connection(_)
        | ScrapixError::Network(_)
        | ScrapixError::RateLimited { .. } => true,
        ScrapixError::Http { status, .. } => RETRYABLE_STATUSES.contains(status),
        _ => false,
    }
}

/// `30s * 2^retry_count`, capped at [`MAX_RETRY_DELAY`].
fn backoff_delay(retry_count: u32) -> Duration {
    BASE_RETRY_DELAY
        .checked_mul(2u32.saturating_pow(retry_count.min(31)))
        .unwrap_or(MAX_RETRY_DELAY)
        .min(MAX_RETRY_DELAY)
}

#[cfg(test)]
mod tests {
    use super::*;
    use scrapix_core::{RawPage, ScrapixError};
    use scrapix_crawler::FetchResult;
    use std::collections::HashMap;

    fn page(status: u16, headers: &[(&str, &str)]) -> scrapix_core::Result<FetchResult> {
        Ok(FetchResult::Fetched(RawPage {
            url: "https://a.test/".into(),
            final_url: "https://a.test/".into(),
            status,
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<HashMap<_, _>>(),
            html: String::new(),
            content_type: None,
            js_rendered: false,
            fetched_at: chrono::Utc::now(),
            fetch_duration_ms: 1,
        }))
    }

    #[test]
    fn ok_is_crawled() {
        assert!(matches!(classify(&page(200, &[]), 0, 3), Outcome::Crawled));
    }
    #[test]
    fn not_found_is_terminal_with_status() {
        assert!(matches!(
            classify(&page(404, &[]), 0, 3),
            Outcome::Failed {
                status: Some(404),
                ..
            }
        ));
    }
    #[test]
    fn unavailable_retries_until_exhausted() {
        assert!(matches!(
            classify(&page(503, &[]), 0, 3),
            Outcome::Retry { .. }
        ));
        assert!(matches!(
            classify(&page(503, &[]), 3, 3),
            Outcome::Failed {
                status: Some(503),
                ..
            }
        ));
    }
    #[test]
    fn retry_after_sets_delay() {
        match classify(&page(429, &[("retry-after", "90")]), 0, 3) {
            Outcome::Retry { delay, .. } => assert_eq!(delay, std::time::Duration::from_secs(90)),
            other => panic!("{other:?}"),
        }
    }
    #[test]
    fn timeouts_retry() {
        let r: scrapix_core::Result<FetchResult> = Err(ScrapixError::Timeout("t".into()));
        assert!(matches!(classify(&r, 1, 3), Outcome::Retry { .. }));
    }
    #[test]
    fn robots_is_terminal() {
        let r: scrapix_core::Result<FetchResult> =
            Err(ScrapixError::RobotsDisallowed { url: "u".into() });
        assert!(matches!(
            classify(&r, 0, 3),
            Outcome::Failed { status: None, .. }
        ));
    }

    // Additional coverage beyond the brief.

    #[test]
    fn not_modified_is_not_modified() {
        let r: scrapix_core::Result<FetchResult> = Ok(FetchResult::NotModified {
            url: "u".into(),
            fetch_duration_ms: 1,
        });
        assert_eq!(classify(&r, 0, 3), Outcome::NotModified);
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        let delay = |n| match classify(&page(500, &[]), n, 100) {
            Outcome::Retry { delay, .. } => delay,
            other => panic!("{other:?}"),
        };
        assert_eq!(delay(0), Duration::from_secs(30));
        assert_eq!(delay(1), Duration::from_secs(60));
        assert_eq!(delay(2), Duration::from_secs(120));
        assert_eq!(delay(10), MAX_RETRY_DELAY);
        assert_eq!(delay(60), MAX_RETRY_DELAY);
    }

    #[test]
    fn huge_retry_after_is_capped() {
        match classify(&page(503, &[("retry-after", "86400")]), 0, 3) {
            Outcome::Retry { delay, .. } => assert_eq!(delay, MAX_RETRY_DELAY),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn terminal_errors_are_failed_without_status() {
        for e in [
            ScrapixError::Refused("non-public".into()),
            ScrapixError::Crawl("Unsupported content type: image/png".into()),
            ScrapixError::Crawl("Response body too large: 1 bytes (max: 0)".into()),
            ScrapixError::Config("Invalid proxy URL".into()),
        ] {
            let r: scrapix_core::Result<FetchResult> = Err(e);
            assert!(
                matches!(classify(&r, 0, 3), Outcome::Failed { status: None, .. }),
                "{r:?}"
            );
            assert!(!is_retryable(&r));
        }
    }

    #[test]
    fn connection_and_network_errors_retry_then_exhaust() {
        for e in [
            ScrapixError::Connection("reset".into()),
            ScrapixError::Network("eof".into()),
        ] {
            let r: scrapix_core::Result<FetchResult> = Err(e);
            assert!(matches!(classify(&r, 0, 3), Outcome::Retry { .. }));
            assert!(matches!(
                classify(&r, 3, 3),
                Outcome::Failed { status: None, .. }
            ));
            assert!(is_retryable(&r));
        }
    }

    #[test]
    fn retryable_http_error_keeps_status() {
        let r: scrapix_core::Result<FetchResult> = Err(ScrapixError::Http {
            status: 502,
            url: "u".into(),
        });
        assert!(matches!(classify(&r, 0, 3), Outcome::Retry { .. }));
        assert!(matches!(
            classify(&r, 3, 3),
            Outcome::Failed {
                status: Some(502),
                ..
            }
        ));
        let r: scrapix_core::Result<FetchResult> = Err(ScrapixError::Http {
            status: 403,
            url: "u".into(),
        });
        assert!(matches!(
            classify(&r, 0, 3),
            Outcome::Failed {
                status: Some(403),
                ..
            }
        ));
    }

    #[test]
    fn redirect_status_is_terminal() {
        // A 3xx that was not followed (e.g. redirect policy off) is not indexed.
        assert!(matches!(
            classify(&page(301, &[]), 0, 3),
            Outcome::Failed {
                status: Some(301),
                ..
            }
        ));
    }
}
