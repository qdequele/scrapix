//! Real-browser tests for the Lightpanda renderer, against local fixture
//! pages.
//!
//! Needs a running Lightpanda (1.0 or later) reachable over CDP. Set
//! `SCRAPIX_TEST_LIGHTPANDA` to its endpoint and build with the
//! `browser-lightpanda` feature:
//!
//! ```bash
//! lightpanda serve --host 127.0.0.1 --port 9222 &
//! SCRAPIX_TEST_LIGHTPANDA=ws://127.0.0.1:9222 \
//!   cargo test -p scrapix-crawler --features browser-lightpanda --test lightpanda_browser
//! ```
//!
//! Without it every test prints a skip note and passes, so CI without a
//! browser stays green. Do not start this test server with
//! `--block-private-networks`: fixtures are served by wiremock on
//! 127.0.0.1, addressed as `localhost`, with `allow_private_ips`.

#![cfg(feature = "browser-lightpanda")]

use std::sync::Arc;
use std::time::Duration;

use scrapix_core::ScrapixError;
use scrapix_crawler::{LightpandaConfig, LightpandaRenderer};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A renderer on the test server, or `None` (skip) when
/// `SCRAPIX_TEST_LIGHTPANDA` is not set.
async fn renderer(max_connections: usize) -> Option<LightpandaRenderer> {
    let Some(cdp_url) = std::env::var("SCRAPIX_TEST_LIGHTPANDA")
        .ok()
        .filter(|s| !s.is_empty())
    else {
        eprintln!("SCRAPIX_TEST_LIGHTPANDA not set; skipping Lightpanda test");
        return None;
    };
    let config = LightpandaConfig {
        cdp_url,
        max_connections,
        timeout: Duration::from_secs(20),
        allow_private_ips: true,
        ..LightpandaConfig::default()
    };
    Some(
        LightpandaRenderer::new(config, None)
            .await
            .expect("connect to the test Lightpanda"),
    )
}

/// Serve `html` with `status` at `route` and return the page URL.
async fn serve(server: &MockServer, route: &str, status: u16, html: &str) -> String {
    Mock::given(method("GET"))
        .and(path(route))
        .respond_with(
            ResponseTemplate::new(status)
                .set_body_raw(html.to_string(), "text/html; charset=utf-8"),
        )
        .mount(server)
        .await;
    format!("{}{route}", server.uri().replace("127.0.0.1", "localhost"))
}

/// A client-rendered page: the content exists only after the script runs.
const SPA_PAGE: &str = r#"<!doctype html><html><head><title>SPA</title></head>
<body><div id="root"></div><script>
  const root = document.getElementById('root');
  const items = ['alpha', 'beta', 'gamma'].map(n => {
    const li = document.createElement('li');
    li.textContent = 'Rendered by JavaScript: item ' + n + ' with enough words to count as content.';
    return li;
  });
  const ul = document.createElement('ul');
  ul.id = 'list';
  items.forEach(li => ul.appendChild(li));
  root.appendChild(ul);
</script></body></html>"#;

#[tokio::test]
async fn renders_javascript_content() {
    let Some(r) = renderer(2).await else { return };
    let server = MockServer::start().await;
    let url = serve(&server, "/spa", 200, SPA_PAGE).await;

    let render = r.render(&url, None, &[], None).await.unwrap();
    assert!(
        render.result.html.contains("id=\"list\""),
        "script output in the DOM: {}",
        render.result.html
    );
    assert!(render.result.html.contains("item gamma"));
    assert_eq!(render.result.status, 200);
    assert_eq!(render.result.final_url, url);
    assert!(render
        .result
        .content_type
        .as_deref()
        .unwrap_or_default()
        .starts_with("text/html"));
    assert!(
        render.text_chars > 100,
        "text_chars = {}",
        render.text_chars
    );
    assert_eq!(render.fallback_reason(), None);
    let status = render.status.as_ref().expect("LP.getPageStatus answered");
    assert_eq!(status.js_errors, 0);
}

#[tokio::test]
async fn empty_app_shell_asks_for_chrome() {
    let Some(r) = renderer(1).await else { return };
    let server = MockServer::start().await;
    // The app script throws before rendering anything.
    let html = r#"<!doctype html><html><head><title>App</title></head>
        <body><div id="root"></div><script>
        throw new Error('boom');
        </script></body></html>"#;
    let url = serve(&server, "/shell", 200, html).await;

    let render = r.render(&url, None, &[], None).await.unwrap();
    assert_eq!(render.fallback_reason(), Some("page has almost no text"));
    if let Some(status) = &render.status {
        assert!(status.js_errors >= 1, "the thrown error is reported");
    }
}

#[tokio::test]
async fn reports_error_status_without_fallback() {
    let Some(r) = renderer(1).await else { return };
    let server = MockServer::start().await;
    let url = serve(&server, "/gone", 404, "<html><body>Not here</body></html>").await;

    let render = r.render(&url, None, &[], None).await.unwrap();
    assert_eq!(render.result.status, 404);
    assert_eq!(render.fallback_reason(), None);
}

#[tokio::test]
async fn sends_the_job_user_agent_and_headers() {
    let Some(r) = renderer(1).await else { return };
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/whoami"))
        .and(header("user-agent", "ScrapixTest/1.0"))
        .and(header("x-scrapix-test", "yes"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "<html><body><p>Matched the job's user agent and header, plenty of text here.</p></body></html>",
            "text/html",
        ))
        .mount(&server)
        .await;
    // Anything else (wrong UA or header) is a 418.
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(418))
        .mount(&server)
        .await;
    let url = format!("{}/whoami", server.uri().replace("127.0.0.1", "localhost"));

    let render = r
        .render(
            &url,
            Some("ScrapixTest/1.0"),
            &[("x-scrapix-test".to_string(), "yes".to_string())],
            None,
        )
        .await
        .unwrap();
    assert_eq!(render.result.status, 200, "{}", render.result.html);
    assert!(render.result.html.contains("Matched"));
}

#[tokio::test]
async fn refuses_raw_ip_targets_before_navigating() {
    let Some(r) = renderer(1).await else { return };
    let Err(err) = r
        .render("http://169.254.169.254/latest/meta-data/", None, &[], None)
        .await
    else {
        panic!("a raw-IP target must be refused");
    };
    assert!(matches!(err, ScrapixError::Refused(_)), "{err:?}");
}

#[tokio::test]
async fn reuses_connections_and_renders_in_parallel() {
    let Some(r) = renderer(3).await else { return };
    let r = Arc::new(r);
    let server = MockServer::start().await;
    let url = serve(&server, "/spa", 200, SPA_PAGE).await;

    // Sequential renders on the pool (connection reuse).
    for _ in 0..4 {
        let render = r.render(&url, None, &[], None).await.unwrap();
        assert!(render.result.html.contains("item gamma"));
    }

    // More renders than connections at once.
    let tasks: Vec<_> = (0..9)
        .map(|_| {
            let r = r.clone();
            let url = url.clone();
            tokio::spawn(async move { r.render(&url, None, &[], None).await })
        })
        .collect();
    for task in tasks {
        let render = task.await.unwrap().unwrap();
        assert!(render.result.html.contains("item gamma"));
    }
}
