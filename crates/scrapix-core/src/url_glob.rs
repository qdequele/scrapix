//! URL glob matching shared by every stage that filters URLs by pattern.
//!
//! Link extraction and sitemap discovery (crawler), `url_patterns.index_only`
//! and per-feature `include_pages` / `exclude_pages` (content worker) all use
//! these functions so a pattern means the same thing everywhere.

/// Glob-style URL matching, anchored at both ends.
///
/// `**` matches any characters, `/` included; `*` matches any characters
/// except `/`; everything else (`?` included) is literal. A pattern without
/// `*` must match exactly.
///
/// A `*` that ends the pattern matches like `**`: `https://a.test/docs/*`
/// takes `https://a.test/docs/1/2` and `**/page/*` takes `/page/2/`. This
/// keeps the crawler's long-standing meaning of a trailing `*`.
///
/// A `*` that starts the pattern matches like `**` too: `*/blog/*` takes
/// `https://a.test/blog/post`. Every URL starts with `scheme://host`, so a
/// leading `*` that stopped at a `/` could never match one.
pub fn matches_glob(url: &str, pattern: &str) -> bool {
    let url = url.as_bytes();
    let pattern = pattern.as_bytes();

    // The literal part before the first `*` is a plain prefix comparison:
    // most patterns are `https://host/path/*`, and most URLs that do not
    // match differ there. Nothing is allocated on this path.
    let literal = pattern
        .iter()
        .position(|&b| b == b'*')
        .unwrap_or(pattern.len());
    if url.len() < literal || url[..literal] != pattern[..literal] {
        return false;
    }
    if literal == pattern.len() {
        return url.len() == literal;
    }
    // Only `*`s are left: one run that ends the pattern matches anything.
    if pattern[literal..].iter().all(|&b| b == b'*') {
        return true;
    }

    let url = &url[literal..];
    SCRATCH.with(|scratch| {
        let mut scratch = scratch.borrow_mut();
        let (reach, next) = &mut *scratch;
        // `reach[j]`: the tokens seen so far can match `url[..j]`.
        reach.clear();
        reach.resize(url.len() + 1, false);
        next.clear();
        next.resize(url.len() + 1, false);
        reach[0] = true;

        let mut i = literal;
        while i < pattern.len() {
            let (token, after) = token_at(pattern, i);
            i = after;
            match token {
                Token::Byte(b) => {
                    next[0] = false;
                    for j in 0..url.len() {
                        next[j + 1] = reach[j] && url[j] == b;
                    }
                }
                Token::Any => {
                    let mut seen = false;
                    for j in 0..=url.len() {
                        seen |= reach[j];
                        next[j] = seen;
                    }
                }
                Token::AnyInSegment => {
                    for j in 0..=url.len() {
                        next[j] = reach[j] || (j > 0 && next[j - 1] && url[j - 1] != b'/');
                    }
                }
            }
            if !next.contains(&true) {
                return false;
            }
            std::mem::swap(reach, next);
        }
        reach[url.len()]
    })
}

thread_local! {
    /// The two rows of [`matches_glob`]'s matching table, kept per thread so
    /// a match allocates nothing once they have grown to the longest URL.
    static SCRATCH: std::cell::RefCell<(Vec<bool>, Vec<bool>)> =
        const { std::cell::RefCell::new((Vec::new(), Vec::new())) };
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Token {
    Byte(u8),
    /// `**`, or a trailing `*`.
    Any,
    /// `*`: anything but `/`.
    AnyInSegment,
}

/// The token that starts at `pattern[i]`, and the index after it. A run of
/// `*` is one token: `Any` when it is `**` or longer, starts the pattern or
/// ends it, else `AnyInSegment`.
fn token_at(pattern: &[u8], i: usize) -> (Token, usize) {
    if pattern[i] != b'*' {
        return (Token::Byte(pattern[i]), i + 1);
    }
    let run = pattern[i..].iter().take_while(|&&b| b == b'*').count();
    let after = i + run;
    let token = if run >= 2 || i == 0 || after == pattern.len() {
        Token::Any
    } else {
        Token::AnyInSegment
    };
    (token, after)
}

/// Include/exclude filter: exclude patterns win over include patterns, and
/// an empty include list allows everything (subject to exclude).
pub fn matches_include_exclude(url: &str, include: &[String], exclude: &[String]) -> bool {
    if exclude.iter().any(|p| matches_glob(url, p)) {
        return false;
    }
    include.is_empty() || include.iter().any(|p| matches_glob(url, p))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The matcher as it was before it stopped allocating: a token list per
    /// call and a table row per token.
    fn reference_matches_glob(url: &str, pattern: &str) -> bool {
        let bytes = pattern.as_bytes();
        let mut tokens = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'*' {
                let run = bytes[i..].iter().take_while(|&&b| b == b'*').count();
                let leading = i == 0;
                i += run;
                tokens.push(if run >= 2 || leading || i == bytes.len() {
                    Token::Any
                } else {
                    Token::AnyInSegment
                });
            } else {
                tokens.push(Token::Byte(bytes[i]));
                i += 1;
            }
        }
        let url = url.as_bytes();
        let mut reach = vec![false; url.len() + 1];
        reach[0] = true;
        for token in &tokens {
            let mut next = vec![false; url.len() + 1];
            match *token {
                Token::Byte(b) => {
                    for j in 0..url.len() {
                        next[j + 1] = reach[j] && url[j] == b;
                    }
                }
                Token::Any => {
                    let mut seen = false;
                    for j in 0..=url.len() {
                        seen |= reach[j];
                        next[j] = seen;
                    }
                }
                Token::AnyInSegment => {
                    for j in 0..=url.len() {
                        next[j] = reach[j] || (j > 0 && next[j - 1] && url[j - 1] != b'/');
                    }
                }
            }
            if !next.contains(&true) {
                return false;
            }
            reach = next;
        }
        reach[url.len()]
    }

    #[test]
    fn matches_like_the_reference_implementation() {
        let patterns = [
            "",
            "*",
            "**",
            "***",
            "https://a.test/x",
            "https://a.test/docs/*",
            "https://a.test/docs/**",
            "https://a.test/*/x",
            "https://a.test/**/x",
            "https://a.test/docs/**/api",
            "https://a.test/a*b*c",
            "https://a.test/a**b*",
            "*/blog/*",
            "*/blog/tag/*",
            "**/page/*",
            "**/*.pdf",
            "*.pdf",
            "**?lang=*",
            "**/login*",
            "https://a.test/*",
            "https://a.test/*/",
            "https://a.test/*/*",
            "https://a.test/*/*/",
            "https://*.test/x",
            "a*",
            "*a",
            "a**",
            "**a",
            "a*a",
            "a**a",
            "https://a.test/caf\u{e9}/*",
        ];
        let urls = [
            "",
            "a",
            "aa",
            "aba",
            "a/a",
            "https://a.test",
            "https://a.test/",
            "https://a.test/x",
            "https://a.test/y",
            "https://a.test/a/x",
            "https://a.test/a/b/x",
            "https://a.test/a/xyz",
            "https://a.test/docs",
            "https://a.test/docs/",
            "https://a.test/docs/1",
            "https://a.test/docs/1/2",
            "https://a.test/docs/v2/x/api",
            "https://a.test/abc",
            "https://a.test/a1b2c",
            "https://a.test/a/b/c",
            "https://a.test/files/report.pdf",
            "https://a.test/files/report.pdf.html",
            "https://a.test/w?lang=fr",
            "https://a.test/w?language",
            "https://a.test/login?next=/",
            "https://a.test/caf\u{e9}/1",
            "https://b.test/x",
            "https://www.meilisearch.com/blog/rag-vs-mcp",
            "https://www.meilisearch.com/blog/tag/ai",
            "https://q.test/tag/love/page/1/",
        ];
        for pattern in patterns {
            for url in urls {
                assert_eq!(
                    matches_glob(url, pattern),
                    reference_matches_glob(url, pattern),
                    "url={url:?} pattern={pattern:?}"
                );
            }
        }
    }

    #[test]
    fn single_star_does_not_cross_slashes_double_star_does() {
        assert!(matches_glob(
            "https://a.test/docs/1",
            "https://a.test/docs/*"
        ));
        // A `*` between literals does not span a `/` (a trailing `*` is not
        // end-anchored: the pre-existing crawler semantics, kept as is).
        assert!(!matches_glob("https://a.test/a/b/x", "https://a.test/*/x"));
        assert!(matches_glob("https://a.test/a/x", "https://a.test/*/x"));
        assert!(matches_glob(
            "https://a.test/docs/1/2",
            "https://a.test/docs/**"
        ));
        assert!(matches_glob("https://a.test/x", "https://a.test/x"));
        assert!(!matches_glob("https://a.test/y", "https://a.test/x"));
    }

    #[test]
    fn leading_star_spans_scheme_and_host() {
        // The "Meilisearch Blog" saved config: include `*/blog/*`, exclude
        // `*/blog/tag/*`. A leading `*` that stopped at `/` matched no URL,
        // so the crawl never went past its start page.
        let inc = vec!["*/blog/*".to_string()];
        let exc = vec!["*/blog/tag/*".to_string()];
        let post = "https://www.meilisearch.com/blog/rag-vs-mcp";
        assert!(matches_glob(post, "*/blog/*"));
        assert!(matches_include_exclude(post, &inc, &exc));
        assert!(matches_include_exclude(
            "https://www.meilisearch.com/blog/page/2",
            &inc,
            &exc
        ));
        assert!(!matches_include_exclude(
            "https://www.meilisearch.com/blog/tag/ai",
            &inc,
            &exc
        ));
        assert!(!matches_include_exclude(
            "https://www.meilisearch.com/docs/learn",
            &inc,
            &exc
        ));
        // A `*` that is not leading still stops at `/`.
        assert!(!matches_glob("https://a.test/a/b/x", "https://a.test/*/x"));
        assert!(matches_glob("https://a.test/x.pdf", "*.pdf"));
    }

    #[test]
    fn double_star_mixed_with_single_star() {
        // `**` then a `*`: the `*` is a wildcard, not a literal.
        assert!(matches_glob("https://q.test/page/2/", "**/page/*"));
        assert!(matches_glob("https://q.test/tag/love/page/1/", "**/page/*"));
        assert!(!matches_glob("https://q.test/author/x", "**/page/*"));
        assert!(matches_glob("https://q.test/login", "**/login*"));
        assert!(matches_glob("https://q.test/login?next=/", "**/login*"));
        assert!(matches_glob("https://a.test/files/report.pdf", "**/*.pdf"));
        assert!(!matches_glob(
            "https://a.test/files/report.pdf.html",
            "**/*.pdf"
        ));
        assert!(matches_glob("https://a.test/w?lang=fr", "**?lang=*"));
        assert!(!matches_glob("https://a.test/w?language", "**?lang=*"));
        assert!(matches_glob(
            "https://a.test/docs/v2/x/api",
            "https://a.test/docs/**/api"
        ));
        assert!(matches_glob("https://a.test/x/blog/y/z", "**/blog/**"));
    }

    #[test]
    fn literal_parts_are_anchored() {
        // A pattern ending in a literal must end the URL.
        assert!(!matches_glob("https://a.test/a/xyz", "https://a.test/*/x"));
        assert!(!matches_glob(
            "https://a.test/docs/1/2",
            "https://a.test/docs/**/1"
        ));
        // `?` and `.` are literal.
        assert!(!matches_glob("https://a.test/aXb", "https://a.test/a?b"));
        assert!(!matches_glob("https://aXtest/", "https://a.test/"));
        // Non-ASCII URLs.
        assert!(matches_glob("https://a.test/café/1", "https://a.test/*/1"));
    }

    #[test]
    fn trailing_star_keeps_its_crawler_meaning() {
        assert!(matches_glob(
            "https://a.test/docs/1/2",
            "https://a.test/docs/*"
        ));
        assert!(matches_glob(
            "https://a.test/learn",
            "https://a.test/learn*"
        ));
        assert!(matches_glob(
            "https://a.test/learn/x",
            "https://a.test/learn*"
        ));
        assert!(!matches_glob(
            "https://b.test/learn",
            "https://a.test/learn*"
        ));
    }

    #[test]
    fn exclude_wins_and_empty_include_allows_all() {
        let inc = vec!["https://a.test/**".to_string()];
        let exc = vec!["https://a.test/private/*".to_string()];
        assert!(matches_include_exclude("https://a.test/p", &inc, &exc));
        assert!(!matches_include_exclude(
            "https://a.test/private/k",
            &inc,
            &exc
        ));
        assert!(matches_include_exclude("https://b.test/p", &[], &[]));
        assert!(!matches_include_exclude("https://b.test/p", &inc, &[]));
    }
}
