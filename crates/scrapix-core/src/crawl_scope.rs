//! Which documents of a Meilisearch index a crawl owns.
//!
//! Several crawls can write to one index (two saved configs, one for a blog
//! and one for docs, both into `my-crawl`). The Replace index strategy
//! deletes, after a crawl, the documents the crawl did not re-index. That
//! cleanup must only touch the crawl's own documents, never another crawl's.
//!
//! Every crawled document carries [`CRAWL_SCOPE_FIELD`]: a stable id derived
//! from the crawl's start URLs (scheme, a leading `www.`, the query, the
//! fragment and a trailing `/` ignored, order and duplicates ignored). Two
//! runs of the same config share it; two configs with different start URLs
//! do not. The cleanup deletes `_crawl_scope = <scope> AND _crawl_job_id !=
//! <job>`.
//!
//! Documents indexed before the field existed have no scope. For those the
//! cleanup falls back to [`CrawlScope::contains`]: a document whose `url`
//! lies under one of the start URLs (same host, path prefix on a `/`
//! boundary) is the crawl's.

use url::Url;
use uuid::Uuid;

/// Field holding the id of the crawl scope that indexed a document.
pub const CRAWL_SCOPE_FIELD: &str = "_crawl_scope";

/// Field holding the id of the crawl job that last indexed a document.
pub const CRAWL_JOB_ID_FIELD: &str = "_crawl_job_id";

/// Namespace of the UUID v5 scope ids (stable across releases: changing it
/// orphans every scoped document).
const SCOPE_NAMESPACE: Uuid = Uuid::from_bytes([
    0x5c, 0x72, 0x61, 0x70, 0x69, 0x78, 0x2d, 0x73, 0x63, 0x6f, 0x70, 0x65, 0x2d, 0x76, 0x31, 0x00,
]);

/// The pages a crawl owns: everything under its start URLs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrawlScope {
    id: String,
    /// `(host, path)` per start URL; host lowercase without `www.`, path
    /// without a trailing `/` (`""` for the root).
    prefixes: Vec<(String, String)>,
}

impl CrawlScope {
    /// Scope of a crawl started from `start_urls`. Unparsable URLs are
    /// ignored; with none left the scope contains nothing.
    pub fn from_start_urls<S: AsRef<str>>(start_urls: &[S]) -> Self {
        let mut prefixes: Vec<(String, String)> = start_urls
            .iter()
            .filter_map(|u| host_and_path(u.as_ref()))
            .collect();
        prefixes.sort();
        prefixes.dedup();
        let key = prefixes
            .iter()
            .map(|(h, p)| format!("{h}{p}"))
            .collect::<Vec<_>>()
            .join("\n");
        let id = Uuid::new_v5(&SCOPE_NAMESPACE, key.as_bytes())
            .simple()
            .to_string();
        Self { id, prefixes }
    }

    /// The value written to [`CRAWL_SCOPE_FIELD`].
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Whether `url` lies under one of the start URLs (any scheme).
    pub fn contains(&self, url: &str) -> bool {
        let Some((host, path)) = host_and_path(url) else {
            return false;
        };
        self.prefixes.iter().any(|(h, p)| {
            *h == host
                && (p.is_empty()
                    || path == *p
                    || path
                        .strip_prefix(p.as_str())
                        .is_some_and(|rest| rest.starts_with('/')))
        })
    }
}

/// `(host, path)` of an http(s) URL: host lowercase without a leading
/// `www.`, path without trailing `/`.
fn host_and_path(url: &str) -> Option<(String, String)> {
    let parsed = Url::parse(url.trim()).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    let host = parsed.host_str()?.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host).to_string();
    let host = match parsed.port() {
        Some(port) => format!("{host}:{port}"),
        None => host,
    };
    let path = parsed.path().trim_end_matches('/').to_string();
    Some((host, path))
}

/// A URL with its `http:` / `https:` scheme removed (`//host/path`), so the
/// two schemes of one page compare equal. Other URLs are returned as is.
pub fn without_http_scheme(url: &str) -> &str {
    for scheme in ["https:", "http:"] {
        if url.len() >= scheme.len() && url[..scheme.len()].eq_ignore_ascii_case(scheme) {
            return &url[scheme.len()..];
        }
    }
    url
}

/// The URL to record for a page requested as `requested` and served from
/// `final_url` after redirects: `final_url`, except that a redirect from
/// `https` to `http` on the same host keeps `https`. Some sites (behind a
/// TLS proxy) redirect `https://h/x` to `http://h/x/`; without this every
/// page of an `https` crawl ends up indexed (and its links followed) as
/// `http`, next to the `https` copies.
pub fn keep_https(requested: &str, final_url: &str) -> String {
    let (Ok(req), Ok(mut fin)) = (Url::parse(requested), Url::parse(final_url)) else {
        return final_url.to_string();
    };
    if req.scheme() == "https"
        && fin.scheme() == "http"
        && req.host_str().map(str::to_ascii_lowercase)
            == fin.host_str().map(str::to_ascii_lowercase)
        && fin.set_scheme("https").is_ok()
    {
        // `http://h:80/` carries no port after the switch; an explicit
        // non-default port is kept as is.
        return fin.to_string();
    }
    final_url.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_id_is_stable_across_scheme_order_and_trailing_slash() {
        let a = CrawlScope::from_start_urls(&[
            "https://www.meilisearch.com/blog",
            "https://example.com/",
        ]);
        let b = CrawlScope::from_start_urls(&[
            "http://example.com",
            "https://meilisearch.com/blog/",
            "https://example.com",
        ]);
        assert_eq!(a.id(), b.id());
        assert_eq!(a.id().len(), 32);
    }

    #[test]
    fn different_start_urls_have_different_scopes() {
        let blog = CrawlScope::from_start_urls(&["https://www.meilisearch.com/blog"]);
        let docs = CrawlScope::from_start_urls(&["https://www.meilisearch.com/docs"]);
        assert_ne!(blog.id(), docs.id());
    }

    #[test]
    fn scope_id_is_pinned() {
        // Changing this id orphans every scoped document in production.
        let s = CrawlScope::from_start_urls(&["https://www.meilisearch.com/blog"]);
        assert_eq!(
            s.id(),
            Uuid::new_v5(&SCOPE_NAMESPACE, b"meilisearch.com/blog")
                .simple()
                .to_string()
        );
    }

    #[test]
    fn contains_matches_on_path_segment_boundaries() {
        let blog = CrawlScope::from_start_urls(&["https://www.meilisearch.com/blog"]);
        assert!(blog.contains("https://www.meilisearch.com/blog"));
        assert!(blog.contains("https://www.meilisearch.com/blog/"));
        assert!(blog.contains("http://meilisearch.com/blog/rag-vs-mcp"));
        assert!(blog.contains("https://www.meilisearch.com/blog/rag?x=1#h"));
        assert!(!blog.contains("https://www.meilisearch.com/blogging"));
        assert!(!blog.contains("https://www.meilisearch.com/docs/learn"));
        assert!(!blog.contains("https://blog.meilisearch.com/blog/x"));
        assert!(!blog.contains("not a url"));
    }

    #[test]
    fn root_start_url_contains_the_whole_host() {
        let s = CrawlScope::from_start_urls(&["https://quotes.toscrape.com"]);
        assert!(s.contains("http://quotes.toscrape.com/login"));
        assert!(s.contains("https://quotes.toscrape.com/"));
        assert!(!s.contains("https://toscrape.com/"));
    }

    #[test]
    fn empty_scope_contains_nothing() {
        let s = CrawlScope::from_start_urls::<&str>(&[]);
        assert!(!s.contains("https://a.test/"));
    }

    #[test]
    fn without_http_scheme_strips_either_scheme() {
        assert_eq!(without_http_scheme("https://a.test/x"), "//a.test/x");
        assert_eq!(without_http_scheme("http://a.test/x"), "//a.test/x");
        assert_eq!(without_http_scheme("HTTP://a.test/x"), "//a.test/x");
        assert_eq!(without_http_scheme("ftp://a.test/x"), "ftp://a.test/x");
    }

    #[test]
    fn keep_https_refuses_a_same_host_downgrade_only() {
        assert_eq!(
            keep_https(
                "https://quotes.toscrape.com/author/A",
                "http://quotes.toscrape.com/author/A/"
            ),
            "https://quotes.toscrape.com/author/A/"
        );
        // An upgrade is kept.
        assert_eq!(
            keep_https("http://a.test/x", "https://a.test/x"),
            "https://a.test/x"
        );
        // Another host is someone else's URL.
        assert_eq!(
            keep_https("https://a.test/x", "http://b.test/x"),
            "http://b.test/x"
        );
        assert_eq!(
            keep_https("https://a.test/", "https://a.test/y"),
            "https://a.test/y"
        );
    }
}
