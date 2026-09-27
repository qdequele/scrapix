//! Page actions (`/scrape` `actions`) and the per-page request guard for
//! the CDP renderer.
//!
//! Actions run on the loaded page, in order, before content and screenshot
//! are captured. Each action has its own timeout and the whole sequence has
//! a shared budget, so a bad selector fails fast with an error naming the
//! action instead of hanging the request.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chromiumoxide::cdp::browser_protocol::fetch::{
    ContinueRequestParams, EnableParams as FetchEnableParams, EventRequestPaused,
    FailRequestParams, RequestPattern,
};
use chromiumoxide::cdp::browser_protocol::input::{
    DispatchKeyEventParams, DispatchKeyEventType, InsertTextParams,
};
use chromiumoxide::cdp::browser_protocol::network::ErrorReason;
use chromiumoxide::cdp::js_protocol::runtime::EvaluateParams;
use chromiumoxide::keys;
use chromiumoxide::Page;
use futures::StreamExt;
use parking_lot::Mutex;
use tracing::debug;

use scrapix_core::browser::Action;

use crate::safe_dns::is_public_ip;

/// Timeout for one action (a `wait` with `ms` gets `ms` plus a margin).
pub const ACTION_TIMEOUT: Duration = Duration::from_secs(10);

/// Budget for all actions of one render.
pub const ACTIONS_BUDGET: Duration = Duration::from_secs(30);

/// How often `wait`/`click`/`write` poll for their selector.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Longest a click/press/script waits for a navigation it triggered to
/// finish loading.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(5);

/// JSON-encode a string for splicing into a JS expression.
fn js_str(s: &str) -> String {
    serde_json::to_string(s).expect("strings always serialize")
}

/// Evaluate `expression` in the page's main world and return its value
/// (JSON round-tripped; a promise is awaited; `undefined` becomes `null`,
/// non-JSON numbers such as `NaN` become their string form). A thrown
/// exception is an `Err` with the exception's message.
async fn eval(page: &Page, expression: String) -> Result<serde_json::Value, String> {
    eval_with(page, expression, false).await
}

/// [`eval`], optionally in REPL mode (top-level `await` allowed, the
/// completion value is returned — but a promise *value* is not awaited).
async fn eval_with(
    page: &Page,
    expression: String,
    repl_mode: bool,
) -> Result<serde_json::Value, String> {
    let params = EvaluateParams::builder()
        .expression(expression)
        .return_by_value(true)
        .await_promise(true)
        .user_gesture(true)
        .repl_mode(repl_mode)
        .build()
        .map_err(|e| format!("invalid evaluation: {e}"))?;
    let resp = page.execute(params).await.map_err(|e| e.to_string())?;
    let returns = resp.result;
    if let Some(ex) = returns.exception_details {
        let message = ex
            .exception
            .as_ref()
            .and_then(|o| o.description.clone())
            .unwrap_or(ex.text);
        // First line only: descriptions carry the stack trace.
        return Err(message.lines().next().unwrap_or_default().to_string());
    }
    let object = returns.result;
    if let Some(value) = object.value {
        return Ok(value);
    }
    if let Some(unserializable) = object.unserializable_value {
        return Ok(serde_json::Value::String(unserializable.inner().clone()));
    }
    Ok(serde_json::Value::Null)
}

/// Poll until an element matches `selector` or `deadline` passes.
async fn wait_for_selector(page: &Page, selector: &str, deadline: Instant) -> Result<(), String> {
    let probe = format!(
        "(() => {{ try {{ return document.querySelector({}) !== null; }} catch (e) {{ return 'invalid'; }} }})()",
        js_str(selector)
    );
    loop {
        match eval(page, probe.clone()).await {
            Ok(serde_json::Value::Bool(true)) => return Ok(()),
            Ok(serde_json::Value::String(s)) if s == "invalid" => {
                return Err(format!("invalid CSS selector `{selector}`"));
            }
            // Not there yet, or the context is being replaced by a navigation.
            _ => {}
        }
        if Instant::now() + POLL_INTERVAL > deadline {
            return Err(format!("no element matches `{selector}`"));
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// After an action that may have navigated: wait (bounded) for the current
/// document to finish loading.
async fn settle(page: &Page, deadline: Instant) {
    tokio::time::sleep(Duration::from_millis(100)).await;
    let until = deadline.min(Instant::now() + SETTLE_TIMEOUT);
    while Instant::now() < until {
        if let Ok(serde_json::Value::String(state)) =
            eval(page, "document.readyState".to_string()).await
        {
            if state == "complete" {
                return;
            }
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Press (key down + key up) a named key on the focused element.
async fn press_key(page: &Page, key: &str) -> Result<(), String> {
    let def = keys::get_key_definition(key).ok_or_else(|| format!("unknown key `{key}`"))?;
    let mut cmd = DispatchKeyEventParams::builder()
        .key(def.key)
        .code(def.code)
        .windows_virtual_key_code(def.key_code)
        .native_virtual_key_code(def.key_code);
    let down = if let Some(text) = def.text {
        cmd = cmd.text(text);
        DispatchKeyEventType::KeyDown
    } else if def.key.len() == 1 {
        cmd = cmd.text(def.key);
        DispatchKeyEventType::KeyDown
    } else {
        DispatchKeyEventType::RawKeyDown
    };
    for kind in [down, DispatchKeyEventType::KeyUp] {
        let params = cmd
            .clone()
            .r#type(kind)
            .build()
            .map_err(|e| format!("key event: {e}"))?;
        page.execute(params).await.map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Run one action; `Ok(Some(value))` for a script's return value.
async fn run_action(
    page: &Page,
    action: &Action,
    deadline: Instant,
) -> Result<Option<serde_json::Value>, String> {
    match action {
        Action::Wait { ms: Some(ms), .. } => {
            tokio::time::sleep(Duration::from_millis(*ms)).await;
            Ok(None)
        }
        Action::Wait {
            selector: Some(selector),
            ..
        } => wait_for_selector(page, selector, deadline)
            .await
            .map(|_| None),
        Action::Wait { .. } => Err("exactly one of `ms` or `selector` must be set".to_string()),
        Action::Click { selector } => {
            wait_for_selector(page, selector, deadline).await?;
            // A real (trusted) mouse click at the element's center; elements
            // without a clickable box (hidden, zero-size) fall back to a DOM
            // `click()`.
            let clicked = match page.find_element(selector.as_str()).await {
                Ok(el) => el.click().await.is_ok(),
                Err(_) => false,
            };
            if !clicked {
                eval(
                    page,
                    format!(
                        "(() => {{ const el = document.querySelector({}); if (!el) throw new Error('element disappeared'); el.click(); }})()",
                        js_str(selector)
                    ),
                )
                .await?;
            }
            settle(page, deadline).await;
            Ok(None)
        }
        Action::Scroll { direction, amount } => {
            let sign = match direction {
                scrapix_core::browser::ScrollDirection::Up => -1.0,
                scrapix_core::browser::ScrollDirection::Down => 1.0,
            };
            eval(
                page,
                format!("window.scrollBy(0, {} * window.innerHeight)", sign * amount),
            )
            .await?;
            // Let scroll handlers (lazy loading, infinite scroll) react.
            tokio::time::sleep(Duration::from_millis(250)).await;
            Ok(None)
        }
        Action::Write { selector, text } => {
            wait_for_selector(page, selector, deadline).await?;
            eval(
                page,
                format!(
                    "(() => {{ const el = document.querySelector({}); if (!el) throw new Error('element disappeared'); el.focus(); }})()",
                    js_str(selector)
                ),
            )
            .await?;
            // Keystrokes for characters with a key definition (so key
            // handlers fire), `Input.insertText` for the rest (non-ASCII).
            for c in text.chars() {
                let key = if c == '\n' {
                    "Enter".to_string()
                } else {
                    c.to_string()
                };
                if keys::get_key_definition(&key).is_some() {
                    press_key(page, &key).await?;
                } else {
                    page.execute(InsertTextParams::new(key))
                        .await
                        .map_err(|e| e.to_string())?;
                }
            }
            Ok(None)
        }
        Action::Press { key } => {
            press_key(page, key).await?;
            settle(page, deadline).await;
            Ok(None)
        }
        Action::ExecuteJavascript { script } => {
            // Evaluate as an expression (its value, awaited if a promise, is
            // returned). A syntax error means nothing ran, so: a script with
            // a top-level `return` is re-run as an async function body, one
            // with a top-level `await` in REPL mode (completion value).
            let value = match eval(page, script.clone()).await {
                Err(e) if e.contains("Illegal return statement") => {
                    eval(page, format!("(async () => {{\n{script}\n}})()")).await?
                }
                Err(e) if e.contains("await is only valid") => {
                    eval_with(page, script.clone(), true).await?
                }
                other => other?,
            };
            settle(page, deadline).await;
            Ok(Some(value))
        }
    }
}

/// A failed action: its index in the request and its type.
#[derive(Debug, Clone)]
pub struct ActionFailure {
    pub index: usize,
    pub action: &'static str,
    pub message: String,
}

/// Run `actions` in order on `page` within `budget`, calling `after_each`
/// (e.g. the SSRF check of the page's current URL) after every action.
/// Returns the values of the `execute_javascript` actions, in order.
pub async fn execute_actions<F, Fut>(
    page: &Page,
    actions: &[Action],
    budget: Duration,
    mut after_each: F,
) -> Result<Vec<serde_json::Value>, ActionFailure>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    let started = Instant::now();
    let budget_end = started + budget;
    let mut javascript_returns = Vec::new();
    for (index, action) in actions.iter().enumerate() {
        let fail = |message: String| ActionFailure {
            index,
            action: action.type_name(),
            message,
        };
        let now = Instant::now();
        if now >= budget_end {
            return Err(fail(format!(
                "the {}s budget for all actions was exhausted before this action ran",
                budget.as_secs_f64()
            )));
        }
        if let Action::Wait { ms: Some(ms), .. } = action {
            let remaining = budget_end.saturating_duration_since(now);
            if Duration::from_millis(*ms) > remaining {
                return Err(fail(format!(
                    "waiting {ms}ms exceeds the remaining {}ms of the {}s actions budget",
                    remaining.as_millis(),
                    budget.as_secs_f64()
                )));
            }
        }
        let own = match action {
            Action::Wait { ms: Some(ms), .. } => {
                Duration::from_millis(*ms) + Duration::from_millis(500)
            }
            _ => ACTION_TIMEOUT,
        };
        let limited_by_budget = now + own > budget_end;
        let deadline = if limited_by_budget {
            budget_end
        } else {
            now + own
        };
        // Selector waits stop at `deadline` with their own error; the outer
        // timeout (slightly later, unless that would overrun the budget)
        // catches a hung CDP call.
        let margin = if limited_by_budget {
            Duration::ZERO
        } else {
            Duration::from_millis(500)
        };
        let outcome = tokio::time::timeout(
            deadline.saturating_duration_since(now) + margin,
            run_action(page, action, deadline),
        )
        .await;
        let result = match outcome {
            Ok(result) => result,
            Err(_) => Err("did not complete".to_string()),
        };
        match result {
            Ok(Some(value)) => javascript_returns.push(value),
            Ok(None) => {}
            Err(message) => {
                let limit = deadline.saturating_duration_since(now);
                let suffix = if limited_by_budget {
                    format!(
                        " (within the remaining {:.1}s of the {}s actions budget)",
                        limit.as_secs_f64(),
                        budget.as_secs_f64()
                    )
                } else if message.starts_with("no element") || message == "did not complete" {
                    format!(" within {:.1}s", limit.as_secs_f64())
                } else {
                    String::new()
                };
                return Err(fail(format!("{message}{suffix}")));
            }
        }
        after_each().await.map_err(fail)?;
    }
    Ok(javascript_returns)
}

/// Whether the browser may request `url`: non-network schemes pass; IP
/// literals must be public (always — even with `allow_private_ips`, like
/// raw-IP seeds); hostnames must resolve only to public addresses unless
/// `allow_private_ips`.
async fn request_allowed(
    url: &str,
    allow_private_ips: bool,
    verdicts: &Mutex<HashMap<String, bool>>,
) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    if !matches!(parsed.scheme(), "http" | "https" | "ws" | "wss") {
        return true;
    }
    match parsed.host() {
        Some(url::Host::Ipv4(ip)) => return is_public_ip(ip.into()),
        Some(url::Host::Ipv6(ip)) => return is_public_ip(ip.into()),
        Some(url::Host::Domain(_)) if allow_private_ips => return true,
        Some(url::Host::Domain(_)) => {}
        None => return false,
    }
    let host = parsed.host_str().unwrap_or_default().to_string();
    let port = parsed.port_or_known_default().unwrap_or(443);
    let key = format!("{host}:{port}");
    if let Some(&verdict) = verdicts.lock().get(&key) {
        return verdict;
    }
    let verdict = match tokio::net::lookup_host((host.as_str(), port)).await {
        Ok(addrs) => {
            let addrs: Vec<_> = addrs.collect();
            !addrs.is_empty() && addrs.iter().all(|a| is_public_ip(a.ip()))
        }
        Err(_) => false,
    };
    verdicts.lock().insert(key, verdict);
    verdict
}

/// Pause every request `page` makes (`Fetch.enable`) and fail the ones to
/// non-public addresses — navigations triggered by clicks or scripts,
/// redirects, subresources, `fetch`/XHR — so the page can never *request*
/// an internal URL, not just never *end* on one. Returns the task answering
/// the paused requests; abort it once the page is closed.
pub(crate) async fn install_request_guard(
    page: &Page,
    allow_private_ips: bool,
) -> Result<tokio::task::JoinHandle<()>, String> {
    let mut paused = page
        .event_listener::<EventRequestPaused>()
        .await
        .map_err(|e| format!("request guard listener: {e}"))?;
    let guard_page = page.clone();
    let task = tokio::spawn(async move {
        let verdicts = Arc::new(Mutex::new(HashMap::new()));
        while let Some(event) = paused.next().await {
            let page = guard_page.clone();
            let verdicts = verdicts.clone();
            tokio::spawn(async move {
                let url = event.request.url.clone();
                if request_allowed(&url, allow_private_ips, &verdicts).await {
                    let _ = page
                        .execute(ContinueRequestParams::new(event.request_id.clone()))
                        .await;
                } else {
                    debug!(url = %url, "Browser request to a non-public address blocked");
                    let _ = page
                        .execute(FailRequestParams::new(
                            event.request_id.clone(),
                            ErrorReason::BlockedByClient,
                        ))
                        .await;
                }
            });
        }
    });
    let enable = FetchEnableParams::builder()
        .pattern(RequestPattern::builder().url_pattern("*").build())
        .build();
    if let Err(e) = page.execute(enable).await {
        task.abort();
        return Err(format!("request guard: {e}"));
    }
    Ok(task)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn request_guard_rules() {
        let v = Mutex::new(HashMap::new());
        // Non-network schemes pass.
        assert!(request_allowed("data:text/plain,hi", false, &v).await);
        assert!(request_allowed("blob:https://example.com/uuid", false, &v).await);
        // Non-public IP literals never pass, even with the opt-out.
        for url in [
            "http://127.0.0.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://[::1]:8080/",
            "http://10.0.0.1/",
        ] {
            assert!(!request_allowed(url, false, &v).await, "{url}");
            assert!(!request_allowed(url, true, &v).await, "{url}");
        }
        // Public IP literals pass.
        assert!(request_allowed("http://8.8.8.8/", false, &v).await);
        // Hostnames resolving to loopback: refused unless opted out.
        assert!(!request_allowed("http://localhost:1234/", false, &v).await);
        assert!(request_allowed("http://localhost:1234/", true, &v).await);
        assert!(!request_allowed("not a url", false, &v).await);
    }
}
