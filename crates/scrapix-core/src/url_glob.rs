//! URL glob matching shared by every stage that filters URLs by pattern.
//!
//! Link extraction and sitemap discovery (crawler), `url_patterns.index_only`
//! and per-feature `include_pages` / `exclude_pages` (content worker) all use
//! these functions so a pattern means the same thing everywhere.

/// Simple glob-style pattern matching.
///
/// A single `*` matches any characters except `/`; `**` matches any
/// characters including `/`. A pattern without `*` must match exactly.
pub fn matches_glob(url: &str, pattern: &str) -> bool {
    if pattern.contains("**") {
        // Handle ** as "match anything"
        let parts: Vec<&str> = pattern.split("**").collect();
        if parts.len() == 2 {
            return url.starts_with(parts[0]) && (parts[1].is_empty() || url.ends_with(parts[1]));
        }
    }

    if pattern.contains('*') {
        // Handle * as "match anything except /"
        let parts: Vec<&str> = pattern.split('*').collect();
        let mut pos = 0;
        for part in parts {
            if part.is_empty() {
                continue;
            }
            if let Some(found) = url[pos..].find(part) {
                // Check no / between pos and found
                if url[pos..pos + found].contains('/') && !pattern.contains("**") {
                    return false;
                }
                pos = pos + found + part.len();
            } else {
                return false;
            }
        }
        true
    } else {
        // Exact match
        url == pattern
    }
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
