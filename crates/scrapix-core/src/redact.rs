//! Redaction of secrets embedded in URLs before they reach logs, error
//! messages or persisted configs.
//!
//! Proxy URLs routinely carry credentials in their userinfo
//! (`http://user:pass@proxy:3128`), and webhook URLs often carry tokens in
//! their query string. Keep the real value only where it is used (fetching,
//! delivery); display the redacted form everywhere else.

use url::Url;

/// `url` with its userinfo (user and password) replaced by `***`; the URL
/// unchanged if it has none.
pub fn redact_userinfo(url: &Url) -> String {
    if url.username().is_empty() && url.password().is_none() {
        return url.to_string();
    }
    let mut redacted = url.clone();
    // Only fails for URLs that cannot have userinfo (`cannot-be-a-base`),
    // which then had none to begin with.
    if redacted.set_username("***").is_err() || redacted.set_password(None).is_err() {
        return url.to_string();
    }
    redacted.to_string()
}

/// [`redact_userinfo`] for a string that may not parse as a URL: anything
/// between `scheme://` and the last `@` of the authority is replaced by
/// `***`. A string with no userinfo is returned as is.
pub fn redact_userinfo_str(raw: &str) -> String {
    if let Ok(url) = Url::parse(raw) {
        // `user:pass@host` parses as a `cannot-be-a-base` URL of scheme
        // `user`: handle it textually below.
        if !url.cannot_be_a_base() {
            if url.username().is_empty() && url.password().is_none() {
                // Nothing to hide: keep the caller's spelling.
                return raw.to_string();
            }
            return redact_userinfo(&url);
        }
    }
    let (prefix, rest) = match raw.find("://") {
        Some(i) => raw.split_at(i + 3),
        None => ("", raw),
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    match rest[..authority_end].rfind('@') {
        Some(at) => format!("{prefix}***{}", &rest[at..]),
        None => raw.to_string(),
    }
}

/// `scheme://host[:port]` of `url` only: no userinfo, path, query or
/// fragment (for error messages about URLs that may carry tokens).
pub fn url_origin_for_display(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default();
    match url.port() {
        Some(port) => format!("{}://{host}:{port}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn userinfo_is_replaced() {
        let u = Url::parse("http://alice:s3cret@proxy.test:3128/").unwrap();
        let r = redact_userinfo(&u);
        assert_eq!(r, "http://***@proxy.test:3128/");
        let u = Url::parse("https://tokenonly@proxy.test/").unwrap();
        assert_eq!(redact_userinfo(&u), "https://***@proxy.test/");
    }

    #[test]
    fn url_without_userinfo_is_unchanged() {
        let u = Url::parse("http://proxy.test:3128/").unwrap();
        assert_eq!(redact_userinfo(&u), "http://proxy.test:3128/");
    }

    #[test]
    fn unparsable_strings_are_redacted_too() {
        let r = redact_userinfo_str("http://alice:s3cret@exa mple:99999/x");
        assert!(!r.contains("s3cret") && !r.contains("alice"), "{r}");
        assert!(r.starts_with("http://***@"), "{r}");
        assert_eq!(redact_userinfo_str("not a url"), "not a url");
        let r = redact_userinfo_str("alice:s3cret@host:1");
        assert!(!r.contains("s3cret"), "{r}");
    }

    #[test]
    fn origin_drops_path_query_and_userinfo() {
        let u = Url::parse("https://u:p@hooks.test:8443/cb?token=abc#f").unwrap();
        assert_eq!(url_origin_for_display(&u), "https://hooks.test:8443");
        let u = Url::parse("http://10.0.0.1/cb?token=abc").unwrap();
        assert_eq!(url_origin_for_display(&u), "http://10.0.0.1");
    }
}
