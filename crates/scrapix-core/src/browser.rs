//! Browser-page request types shared by the API (`/scrape`), the CDP
//! renderer and the HTTP fetcher: page actions run after the page loads,
//! and cookies sent with a one-off fetch.

use serde::{Deserialize, Serialize};

/// Maximum number of actions accepted in one request.
pub const MAX_ACTIONS: usize = 50;

/// Upper bound for a single `wait` action's `ms`.
pub const MAX_WAIT_MS: u64 = 30_000;

/// Maximum size of an `execute_javascript` script, in bytes.
pub const MAX_SCRIPT_BYTES: usize = 100 * 1024;

/// Maximum size of a `write` action's text, in bytes.
pub const MAX_WRITE_TEXT_BYTES: usize = 10 * 1024;

/// Maximum number of cookies accepted in one request.
pub const MAX_COOKIES: usize = 50;

/// Maximum size of one cookie (`name` + `value`), in bytes.
pub const MAX_COOKIE_BYTES: usize = 4096;

/// Scroll direction for a [`Action::Scroll`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScrollDirection {
    Up,
    #[default]
    Down,
}

fn default_scroll_amount() -> f64 {
    1.0
}

/// A browser interaction run on the page after it loads and before content
/// (and any screenshot) is captured. Actions run in order; the first one to
/// fail stops the sequence and fails the request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    /// Wait a fixed time (`ms`) or until an element matching `selector`
    /// exists. Exactly one of the two must be set.
    Wait {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selector: Option<String>,
    },
    /// Click the first element matching `selector` (waits for it to exist).
    Click { selector: String },
    /// Scroll the page by `amount` viewport heights (default 1).
    Scroll {
        #[serde(default)]
        direction: ScrollDirection,
        #[serde(default = "default_scroll_amount")]
        amount: f64,
    },
    /// Focus the first element matching `selector` and type `text` into it.
    Write { selector: String, text: String },
    /// Press a key (e.g. `Enter`, `Tab`, `ArrowDown`, `Escape`, `a`) on the
    /// focused element.
    Press { key: String },
    /// Run JavaScript in the page. The value of the script (or of its
    /// `return` statement; `await` is allowed) is returned in
    /// `actions.javascript_returns`, JSON-serialized.
    ExecuteJavascript { script: String },
}

impl Action {
    /// The action's `type` tag, as used in requests and error messages.
    pub fn type_name(&self) -> &'static str {
        match self {
            Action::Wait { .. } => "wait",
            Action::Click { .. } => "click",
            Action::Scroll { .. } => "scroll",
            Action::Write { .. } => "write",
            Action::Press { .. } => "press",
            Action::ExecuteJavascript { .. } => "execute_javascript",
        }
    }

    /// Validate one action's fields (not its effect on a page).
    pub fn validate(&self) -> Result<(), String> {
        fn selector_ok(selector: &str) -> Result<(), String> {
            if selector.trim().is_empty() {
                return Err("selector must not be empty".to_string());
            }
            if selector.len() > 1024 {
                return Err("selector is too long (max 1024 bytes)".to_string());
            }
            Ok(())
        }
        match self {
            Action::Wait { ms, selector } => match (ms, selector) {
                (Some(_), Some(_)) | (None, None) => {
                    Err("exactly one of `ms` or `selector` must be set".to_string())
                }
                (Some(ms), None) if *ms > MAX_WAIT_MS => {
                    Err(format!("ms must be at most {MAX_WAIT_MS}"))
                }
                (Some(_), None) => Ok(()),
                (None, Some(sel)) => selector_ok(sel),
            },
            Action::Click { selector } => selector_ok(selector),
            Action::Scroll { amount, .. } => {
                if !amount.is_finite() || *amount <= 0.0 || *amount > 100.0 {
                    Err("amount must be a number of screens in (0, 100]".to_string())
                } else {
                    Ok(())
                }
            }
            Action::Write { selector, text } => {
                selector_ok(selector)?;
                if text.len() > MAX_WRITE_TEXT_BYTES {
                    return Err(format!(
                        "text is too long (max {MAX_WRITE_TEXT_BYTES} bytes)"
                    ));
                }
                Ok(())
            }
            Action::Press { key } => {
                if key.is_empty() || key.len() > 32 {
                    Err("key must be a key name such as `Enter` or `a`".to_string())
                } else {
                    Ok(())
                }
            }
            Action::ExecuteJavascript { script } => {
                if script.trim().is_empty() {
                    return Err("script must not be empty".to_string());
                }
                if script.len() > MAX_SCRIPT_BYTES {
                    return Err(format!("script is too long (max {MAX_SCRIPT_BYTES} bytes)"));
                }
                Ok(())
            }
        }
    }
}

/// Validate a whole action list: count cap plus each action's fields. The
/// error names the offending action's index and type.
pub fn validate_actions(actions: &[Action]) -> Result<(), String> {
    if actions.len() > MAX_ACTIONS {
        return Err(format!(
            "too many actions ({}); at most {MAX_ACTIONS} are allowed",
            actions.len()
        ));
    }
    for (index, action) in actions.iter().enumerate() {
        action
            .validate()
            .map_err(|e| format!("actions[{index}] ({}): {e}", action.type_name()))?;
    }
    Ok(())
}

/// A cookie to send with a one-off fetch (HTTP or browser).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RequestCookie {
    pub name: String,
    pub value: String,
    /// Cookie domain. Defaults to the target URL's host (host-only cookie).
    /// Must be the target host or a parent domain of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// Cookie path (default `/`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secure: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_only: Option<bool>,
}

impl RequestCookie {
    /// The domain, lowercased and without a leading dot, if set.
    pub fn normalized_domain(&self) -> Option<String> {
        self.domain
            .as_deref()
            .map(|d| d.trim().trim_start_matches('.').to_ascii_lowercase())
            .filter(|d| !d.is_empty())
    }

    /// Validate the cookie against the URL it will be sent to: name/value
    /// must be header-safe and the domain must cover the target host, so a
    /// request can only set cookies for the site it scrapes.
    pub fn validate_for(&self, target: &url::Url) -> Result<(), String> {
        let is_token_char = |c: char| c.is_ascii_graphic() && !"()<>@,;:\\\"/[]?={}".contains(c);
        if self.name.is_empty() || !self.name.chars().all(is_token_char) {
            return Err(format!("invalid cookie name {:?}", self.name));
        }
        // RFC 6265 cookie-octet, plus space (commonly accepted).
        let is_value_char = |c: char| c == ' ' || (c.is_ascii_graphic() && !"\",;\\".contains(c));
        if !self.value.chars().all(is_value_char) {
            return Err(format!(
                "cookie {:?} has an invalid value (no quotes, commas, semicolons, backslashes or control characters)",
                self.name
            ));
        }
        if self.name.len() + self.value.len() > MAX_COOKIE_BYTES {
            return Err(format!(
                "cookie {:?} is too large (max {MAX_COOKIE_BYTES} bytes)",
                self.name
            ));
        }
        if let Some(path) = &self.path {
            if !path.starts_with('/') || path.contains(';') || path.chars().any(|c| c.is_control())
            {
                return Err(format!(
                    "cookie {:?} has an invalid path (must start with `/`)",
                    self.name
                ));
            }
        }
        if let Some(domain) = self.normalized_domain() {
            let host = target
                .host_str()
                .map(|h| h.to_ascii_lowercase())
                .unwrap_or_default();
            let covers = host == domain || host.ends_with(&format!(".{domain}"));
            if !covers || !domain.contains('.') {
                return Err(format!(
                    "cookie {:?} domain {domain:?} does not match the target host {host:?}",
                    self.name
                ));
            }
        }
        Ok(())
    }

    /// The cookie as a `Set-Cookie` header value (for seeding a cookie jar).
    pub fn to_set_cookie_string(&self) -> String {
        let mut s = format!("{}={}", self.name, self.value);
        if let Some(domain) = self.normalized_domain() {
            s.push_str(&format!("; Domain={domain}"));
        }
        s.push_str(&format!("; Path={}", self.path.as_deref().unwrap_or("/")));
        if self.secure == Some(true) {
            s.push_str("; Secure");
        }
        if self.http_only == Some(true) {
            s.push_str("; HttpOnly");
        }
        s
    }
}

/// Validate a cookie list against its target URL (count cap plus each
/// cookie).
pub fn validate_cookies(cookies: &[RequestCookie], target: &url::Url) -> Result<(), String> {
    if cookies.len() > MAX_COOKIES {
        return Err(format!(
            "too many cookies ({}); at most {MAX_COOKIES} are allowed",
            cookies.len()
        ));
    }
    for cookie in cookies {
        cookie.validate_for(target)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_deserialize_from_tagged_json() {
        let actions: Vec<Action> = serde_json::from_value(serde_json::json!([
            {"type": "wait", "ms": 500},
            {"type": "wait", "selector": "#ready"},
            {"type": "click", "selector": "button.more"},
            {"type": "scroll", "direction": "up", "amount": 2},
            {"type": "scroll"},
            {"type": "write", "selector": "input[name=q]", "text": "hello"},
            {"type": "press", "key": "Enter"},
            {"type": "execute_javascript", "script": "document.title"}
        ]))
        .unwrap();
        assert_eq!(actions.len(), 8);
        assert_eq!(
            actions[3],
            Action::Scroll {
                direction: ScrollDirection::Up,
                amount: 2.0
            }
        );
        assert_eq!(
            actions[4],
            Action::Scroll {
                direction: ScrollDirection::Down,
                amount: 1.0
            }
        );
        assert_eq!(actions[7].type_name(), "execute_javascript");
        assert!(validate_actions(&actions).is_ok());
    }

    #[test]
    fn unknown_action_type_is_rejected() {
        let r: Result<Action, _> = serde_json::from_value(serde_json::json!({"type": "hover"}));
        assert!(r.is_err());
    }

    #[test]
    fn invalid_actions_name_index_and_type() {
        let err = validate_actions(&[
            Action::Wait {
                ms: Some(10),
                selector: None,
            },
            Action::Wait {
                ms: None,
                selector: None,
            },
        ])
        .unwrap_err();
        assert!(err.starts_with("actions[1] (wait)"), "{err}");

        let err = validate_actions(&[Action::Wait {
            ms: Some(MAX_WAIT_MS + 1),
            selector: None,
        }])
        .unwrap_err();
        assert!(err.contains("at most"), "{err}");

        let err = validate_actions(&[Action::Scroll {
            direction: ScrollDirection::Down,
            amount: 0.0,
        }])
        .unwrap_err();
        assert!(err.contains("(scroll)"), "{err}");

        let too_many = vec![Action::Press { key: "a".into() }; MAX_ACTIONS + 1];
        assert!(validate_actions(&too_many)
            .unwrap_err()
            .contains("too many actions"));
    }

    fn cookie(name: &str, value: &str, domain: Option<&str>) -> RequestCookie {
        RequestCookie {
            name: name.into(),
            value: value.into(),
            domain: domain.map(Into::into),
            path: None,
            secure: None,
            http_only: None,
        }
    }

    #[test]
    fn cookie_domain_must_cover_target() {
        let url = url::Url::parse("https://www.example.com/page").unwrap();
        assert!(cookie("sid", "abc", None).validate_for(&url).is_ok());
        assert!(cookie("sid", "abc", Some(".example.com"))
            .validate_for(&url)
            .is_ok());
        assert!(cookie("sid", "abc", Some("www.example.com"))
            .validate_for(&url)
            .is_ok());
        assert!(cookie("sid", "abc", Some("other.com"))
            .validate_for(&url)
            .is_err());
        assert!(cookie("sid", "abc", Some("ample.com"))
            .validate_for(&url)
            .is_err());
        // A bare TLD is never accepted.
        assert!(cookie("sid", "abc", Some("com"))
            .validate_for(&url)
            .is_err());
    }

    #[test]
    fn cookie_name_and_value_are_header_safe() {
        let url = url::Url::parse("https://example.com/").unwrap();
        assert!(cookie("a b", "x", None).validate_for(&url).is_err());
        assert!(cookie("", "x", None).validate_for(&url).is_err());
        assert!(cookie("a", "x;y", None).validate_for(&url).is_err());
        assert!(cookie("a", "x\r\nSet-Cookie: y", None)
            .validate_for(&url)
            .is_err());
        assert!(cookie("a", "x=y%20z", None).validate_for(&url).is_ok());
        let many = vec![cookie("a", "b", None); MAX_COOKIES + 1];
        assert!(validate_cookies(&many, &url).is_err());
    }

    #[test]
    fn set_cookie_string_includes_attributes() {
        let mut c = cookie("sid", "abc", Some(".Example.com"));
        c.path = Some("/app".into());
        c.secure = Some(true);
        c.http_only = Some(true);
        assert_eq!(
            c.to_set_cookie_string(),
            "sid=abc; Domain=example.com; Path=/app; Secure; HttpOnly"
        );
        assert_eq!(cookie("a", "b", None).to_set_cookie_string(), "a=b; Path=/");
    }
}
