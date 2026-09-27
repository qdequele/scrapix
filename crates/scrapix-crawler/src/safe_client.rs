//! Shared, SSRF-safe `reqwest::ClientBuilder` construction.
//!
//! Every HTTP client the crawler builds — the main page fetcher, the
//! robots.txt clients, and the sitemap client — must apply the same three
//! protections: resolve hostnames through [`SafeResolver`] (refusing
//! non-public addresses unless explicitly allowed), refuse raw-IP redirect
//! targets unconditionally, and bypass any environment-configured proxy
//! (which would otherwise let `HTTP_PROXY`/`HTTPS_PROXY` smuggle a request
//! around the checks above). This module is the one place that builds that
//! policy so every client gets it identically.

use std::sync::Arc;

use reqwest::{redirect::Policy, ClientBuilder};
use url::Url;

use scrapix_core::{Result, ScrapixError};

use crate::dns::CachingDnsResolver;
use crate::safe_dns::SafeResolver;

/// Redirect cap applied by [`safe_client_builder`]'s default policy. Callers
/// that need a configurable cap (namely `HttpFetcher`, via
/// `FetcherConfig::max_redirects`) call [`safe_redirect_policy`] directly
/// afterwards to override it — `ClientBuilder::redirect` is a plain setter,
/// so the later call wins.
pub const DEFAULT_MAX_REDIRECTS: usize = 10;

/// Reject URLs whose host is a raw IP address (v4 or v6) to prevent SSRF.
/// Only hostnames (domain names) are allowed — raw IPs bypass
/// [`SafeResolver`] entirely (reqwest never calls the DNS resolver for a URL
/// whose host is already an address), so this check is the only thing that
/// refuses them. Applies uniformly to seed URLs, robots.txt URLs, and
/// sitemap URLs — anywhere a URL comes from a crawl target or user input.
pub(crate) fn reject_ip_host(url: &Url) -> Result<()> {
    match url.host() {
        Some(url::Host::Ipv4(_)) | Some(url::Host::Ipv6(_)) => Err(ScrapixError::Refused(format!(
            "Raw IP addresses are not allowed, use a hostname instead: {url}"
        ))),
        Some(url::Host::Domain(_)) => Ok(()),
        None => Err(ScrapixError::Crawl(format!("URL has no host: {url}"))),
    }
}

/// Build the redirect policy shared by every SSRF-safe HTTP client: enforces
/// `max_redirects` and refuses any redirect whose target host is not a
/// `url::Host::Domain`. Raw-IP redirect targets are refused unconditionally
/// — the same as raw-IP seeds via [`reject_ip_host`] — regardless of
/// `allow_private_ips`; that flag only relaxes the private-IP check on
/// *resolved hostnames* inside [`SafeResolver`]. This means a redirect to a
/// link-local address such as the cloud metadata endpoint
/// (169.254.169.254) is refused even when `allow_private_ips` is set to let
/// the client reach a local test server.
pub(crate) fn safe_redirect_policy(max_redirects: usize) -> Policy {
    Policy::custom(move |attempt| {
        if attempt.previous().len() >= max_redirects {
            return attempt.error("too many redirects");
        }
        match attempt.url().host() {
            Some(url::Host::Domain(_)) => attempt.follow(),
            _ => attempt.error("redirect to a raw IP address refused"),
        }
    })
}

/// Build a `reqwest::ClientBuilder` pre-configured with SSRF protection,
/// shared by `HttpFetcher`, `RobotsCache`, `PersistentRobotsCache`, and
/// `SitemapParser` (and exported for a later task's webhook client):
///
/// - DNS resolution goes through [`SafeResolver`], using `dns_cache` when
///   given (so this client and the crawler's own DNS cache don't
///   double-resolve) or a plain `tokio::net::lookup_host` otherwise. It
///   refuses to hand back non-public addresses unless `allow_private` is
///   set.
/// - The redirect policy (see [`safe_redirect_policy`]) refuses raw-IP
///   redirect targets unconditionally and caps the hop count at
///   [`DEFAULT_MAX_REDIRECTS`] — callers needing a different cap should call
///   `.redirect(safe_redirect_policy(n))` on the returned builder afterwards.
/// - `.no_proxy()` so an `HTTP_PROXY`/`HTTPS_PROXY` environment variable
///   can't be used to route a request around the resolver/redirect checks
///   above.
pub fn safe_client_builder(
    dns_cache: Option<Arc<CachingDnsResolver>>,
    allow_private: bool,
) -> ClientBuilder {
    reqwest::ClientBuilder::new()
        .redirect(safe_redirect_policy(DEFAULT_MAX_REDIRECTS))
        .dns_resolver(Arc::new(SafeResolver {
            cache: dns_cache,
            allow_private,
        }))
        .no_proxy()
}
