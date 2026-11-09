//! The URL normalization the crawl entry points share.
//!
//! Start URLs, sitemap entries and discovered links all reach the frontier,
//! which dedupes on the URL string. They must be spelled the same way or one
//! page is fetched (and counted against `max_pages`) once per spelling:
//! `https://a.test/post/` from a sitemap and `https://a.test/post` from a
//! link.
//!
//! - [`normalize_link_url`] (discovered links): http(s) only, fragment,
//!   whole query string and trailing `/` of a non-root path removed. The
//!   query is dropped on purpose: it keeps crawler traps out (MediaWiki
//!   `action=edit`/`oldid`/`diff`, faceted `?color=&sort=`, `?month=`
//!   calendars, `replytocom`) and keeps the `uid` of already-indexed pages
//!   (derived from the URL) stable.
//! - [`normalize_entry_url`] (start URLs, sitemap entries, and every URL the
//!   frontier admits): the same, but the query is kept. A start URL or a
//!   sitemap entry with a query names that exact page; on a link-derived
//!   URL (already query-free) it changes nothing.
//!
//! The host is lowercased and a default port removed by the `url` crate.

use url::Url;

/// Normalize a discovered link (see the module docs). `None` for a
/// non-http(s) URL.
pub fn normalize_link_url(url: &Url) -> Option<String> {
    normalize(url, false)
}

/// Normalize a start URL, sitemap entry or frontier URL: like
/// [`normalize_link_url`], but the query string is kept. `None` when it does
/// not parse or is not http(s).
pub fn normalize_entry_url(url: &str) -> Option<String> {
    normalize(&Url::parse(url.trim()).ok()?, true)
}

fn normalize(url: &Url, keep_query: bool) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let mut url = url.clone();
    url.set_fragment(None);
    if !keep_query {
        url.set_query(None);
    }
    let path = url.path();
    if path.len() > 1 && path.ends_with('/') {
        let trimmed = path.trim_end_matches('/');
        let trimmed = if trimmed.is_empty() { "/" } else { trimmed }.to_string();
        url.set_path(&trimmed);
    }
    Some(url.to_string())
}

/// Extensions of an index page file (`index.html`, `default.aspx`, ...).
const INDEX_PAGE_EXTENSIONS: &[&str] = &["html", "htm", "shtml", "php", "asp", "aspx", "jsp"];

/// Whether the last path segment `name` is a directory index page:
/// `index.<ext>` or `default.<ext>` with a server page extension.
fn is_index_page(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(stem, ext)| {
        matches!(stem.to_ascii_lowercase().as_str(), "index" | "default")
            && INDEX_PAGE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
    })
}

/// The section path of a start URL path: its trailing `/` removed, and a
/// trailing directory index page (`/manual/index.html` -> `/manual`)
/// removed. Any other file name is kept (`/viewtopic.php` stays itself, so
/// its prefix pattern does not widen to the whole host). `""` for the site
/// root.
pub fn section_path(path: &str) -> &str {
    let path = path.trim_end_matches('/');
    match path.rsplit_once('/') {
        Some((dir, last)) if is_index_page(last) => dir.trim_end_matches('/'),
        _ => path,
    }
}

/// Include patterns keeping a crawl inside its start URLs' sections, for a
/// job that sets none.
///
/// Each start URL `scheme://host/section/...` gives `<s>://<h>/section/*`
/// for `<s>` in `http`/`https` and `<h>` with and without `www.`: a start
/// URL that redirects to `https` or to the `www.` host (or away from it)
/// still matches its own links. A trailing index page is not part of the
/// section (`/manual/index.html` gives `/manual/*`); any other path is
/// prefixed as is (`/viewtopic.php` gives `/viewtopic.php/*`, as before).
/// A start URL at the site root (or its index page) gives `<s>://<h>/*`,
/// its whole host.
///
/// Empty when every start URL is at a site root: the inferred
/// `allowed_domains` already scopes such a crawl.
pub fn auto_include_patterns<S: AsRef<str>>(start_urls: &[S]) -> Vec<String> {
    let parsed: Vec<Url> = start_urls
        .iter()
        .filter_map(|u| Url::parse(u.as_ref().trim()).ok())
        .filter(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some())
        .collect();
    if parsed.iter().all(|u| section_path(u.path()).is_empty()) {
        return Vec::new();
    }
    let mut patterns = Vec::new();
    for u in &parsed {
        let host = u.host_str().unwrap_or_default().to_ascii_lowercase();
        let bare = host.strip_prefix("www.").unwrap_or(&host).to_string();
        let port = u.port().map(|p| format!(":{p}")).unwrap_or_default();
        let section = section_path(u.path());
        for scheme in ["http", "https"] {
            for h in [bare.clone(), format!("www.{bare}")] {
                patterns.push(format!("{scheme}://{h}{port}{section}/*"));
            }
        }
    }
    patterns.sort();
    patterns.dedup();
    patterns
}

/// `url` in each spelling a site treats as the same page: `http` and
/// `https`, with and without a leading `www.` (four URLs, `url` first).
/// Used to match a redirect target against user-written patterns the way a
/// redirect to `https://www.` should be judged: as the same site.
pub fn scheme_and_www_variants(url: &str) -> Vec<String> {
    let Ok(parsed) = Url::parse(url) else {
        return vec![url.to_string()];
    };
    let Some(host) = parsed.host_str().map(str::to_ascii_lowercase) else {
        return vec![url.to_string()];
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return vec![url.to_string()];
    }
    let bare = host.strip_prefix("www.").unwrap_or(&host).to_string();
    let mut out = vec![url.to_string()];
    for scheme in ["http", "https"] {
        for h in [bare.clone(), format!("www.{bare}")] {
            let mut v = parsed.clone();
            if v.set_scheme(scheme).is_err() || v.set_host(Some(&h)).is_err() {
                continue;
            }
            let v = v.to_string();
            if !out.contains(&v) {
                out.push(v);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::url_glob::matches_include_exclude;

    fn link(u: &str) -> String {
        normalize_link_url(&Url::parse(u).unwrap()).unwrap()
    }

    #[test]
    fn links_lose_fragment_query_and_trailing_slash() {
        assert_eq!(link("https://A.test/post/"), "https://a.test/post");
        assert_eq!(link("https://a.test/post#top"), "https://a.test/post");
        assert_eq!(link("https://a.test/"), "https://a.test/");
        assert_eq!(link("https://a.test:443/x/"), "https://a.test/x");
        assert_eq!(
            link("https://a.test/w/index.php?action=edit"),
            "https://a.test/w/index.php"
        );
        assert_eq!(link("https://a.test/dir/?page=2"), "https://a.test/dir");
        assert!(normalize_link_url(&Url::parse("mailto:x@a.test").unwrap()).is_none());
    }

    #[test]
    fn entries_keep_their_query() {
        let e = |u: &str| normalize_entry_url(u).unwrap();
        assert_eq!(e("https://a.test/post/"), "https://a.test/post");
        assert_eq!(
            e("https://forum.test/viewtopic.php?t=1#p2"),
            "https://forum.test/viewtopic.php?t=1"
        );
        // A link-normalized URL is unchanged.
        assert_eq!(e(&link("https://a.test/x/?q=1")), "https://a.test/x");
        assert!(normalize_entry_url("not a url").is_none());
    }

    #[test]
    fn section_path_strips_index_pages_only() {
        assert_eq!(section_path("/manual/index.html"), "/manual");
        assert_eq!(section_path("/app/Default.aspx"), "/app");
        assert_eq!(section_path("/docs/"), "/docs");
        assert_eq!(section_path("/docs/v1.2"), "/docs/v1.2");
        assert_eq!(section_path("/index.php"), "");
        assert_eq!(section_path("/viewtopic.php"), "/viewtopic.php");
        assert_eq!(section_path("/docs/intro.html"), "/docs/intro.html");
        assert_eq!(section_path("/"), "");
    }

    #[test]
    fn auto_patterns_survive_https_and_www_redirects() {
        let p = auto_include_patterns(&["http://meilisearch.com/docs"]);
        let ok = |u: &str| matches_include_exclude(u, &p, &[]);
        assert!(ok("https://www.meilisearch.com/docs/learn"));
        assert!(ok("https://meilisearch.com/docs/learn"));
        assert!(ok("http://www.meilisearch.com/docs/"));
        assert!(!ok("https://www.meilisearch.com/blog/x"));

        let p = auto_include_patterns(&["https://www.a.test/manual/index.html"]);
        assert!(matches_include_exclude(
            "https://a.test/manual/intro.html",
            &p,
            &[]
        ));
    }

    #[test]
    fn auto_patterns_keep_a_non_index_file_as_a_prefix() {
        // A query-routed start URL does not widen to the whole host.
        let p = auto_include_patterns(&["https://forum.test/viewtopic.php?t=1"]);
        assert!(p.contains(&"https://forum.test/viewtopic.php/*".to_string()));
        assert!(!matches_include_exclude(
            "https://forum.test/memberlist.php",
            &p,
            &[]
        ));
        assert!(!matches_include_exclude(
            "https://forum.test/anything",
            &p,
            &[]
        ));
    }

    #[test]
    fn auto_patterns_root_start_urls() {
        assert!(auto_include_patterns(&["https://a.test", "https://b.test/index.html"]).is_empty());
        // A root start URL next to a sectioned one keeps its whole host.
        let p = auto_include_patterns(&["https://a.test/", "https://b.test/docs"]);
        assert!(matches_include_exclude("https://a.test/anything", &p, &[]));
        assert!(matches_include_exclude("https://b.test/docs/x", &p, &[]));
        assert!(!matches_include_exclude("https://b.test/blog", &p, &[]));
    }

    #[test]
    fn variants_cover_scheme_and_www() {
        let v = scheme_and_www_variants("https://www.a.test:8443/docs/a?x=1");
        assert_eq!(v[0], "https://www.a.test:8443/docs/a?x=1");
        assert!(v.contains(&"http://a.test:8443/docs/a?x=1".to_string()));
        assert!(v.contains(&"https://a.test:8443/docs/a?x=1".to_string()));
        assert_eq!(v.len(), 4);
    }
}
