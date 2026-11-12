//! `/scrape` fetch policy (retries, timeout, headers) and its HTML stage:
//! the single-parse pipeline must return what the per-format one did.

use super::*;
use scrapix_queue::ChannelBus;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::results::test_support::test_state;

fn request(body: serde_json::Value) -> ScrapeRequest {
    serde_json::from_value(body).unwrap()
}

fn local(server: &MockServer, path: &str) -> String {
    format!("{}{path}", server.uri().replace("127.0.0.1", "localhost"))
}

const PAGE: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
  <title>Pricing &amp; plans</title>
  <meta name="description" content="What it costs.">
  <meta property="og:title" content="Pricing">
  <base href="/docs/">
  <script type="application/ld+json">{"@context":"https://schema.org","@type":"Product","name":"Widget"}</script>
  <style>body { color: red }</style>
</head>
<body>
  <header><a href="/">Home</a></header>
  <nav class="menu"><ul><li><a href="/a">A</a></li><li><a href="b">B</a></li></ul></nav>
  <main>
    <article>
      <h1>Pricing</h1>
      <p>The <strong>Widget</strong> costs <span class="price">42 euros</span> a month, and the
      bigger one <span class="price">70 euros</span>. See the <a href="terms">terms</a>.</p>
      <div class="ad"><p>Buy now!</p><a href="https://ads.example/x">ad</a></div>
      <h2>Details</h2>
      <ul><li>Fast</li><li>Small</li></ul>
      <table><tr><th>Plan</th><th>Price</th></tr><tr><td>Basic</td><td>42</td></tr></table>
      <pre><code>let x = 1;</code></pre>
      <p>Contact <a href="mailto:sales@example.com">sales</a> or <a href="#top">go up</a>.</p>
    </article>
  </main>
  <aside>Related: <a href="/other">other</a></aside>
  <footer><p>Footer text</p><a href="/legal">Legal</a></footer>
  <script>console.log("<p>not content</p>")</script>
</body>
</html>"##;

/// Include/exclude selectors as `/scrape` applied them before the HTML
/// stage parsed the page once: one parse per exclude selector, matched
/// elements removed from the source text.
fn legacy_preprocess_html(html: &str, include: &[String], exclude: &[String]) -> String {
    use scraper::{Html, Selector};

    if include.is_empty() && exclude.is_empty() {
        return html.to_string();
    }
    let document = Html::parse_document(html);
    let working_html = if !include.is_empty() {
        let mut parts = Vec::new();
        for sel_str in include {
            if let Ok(selector) = Selector::parse(sel_str) {
                for element in document.select(&selector) {
                    parts.push(element.html());
                }
            }
        }
        if parts.is_empty() {
            return html.to_string();
        }
        format!("<html><body>{}</body></html>", parts.join(""))
    } else {
        html.to_string()
    };
    let mut result = working_html;
    for sel_str in exclude {
        if let Ok(selector) = Selector::parse(sel_str) {
            let doc = Html::parse_document(&result);
            for element in doc.select(&selector) {
                let outer = element.html();
                result = result.replacen(&outer, "", 1);
            }
        }
    }
    result
}

/// The HTML stage as it was: every format converted from the HTML text on
/// its own (one parse each).
fn legacy_html_stage(input: &HtmlStageInput) -> serde_json::Value {
    let formats = &input.formats;
    let original_html = input.html.clone();
    let final_url = &input.final_url;
    let processed_html = legacy_preprocess_html(
        &original_html,
        &input.include_selectors,
        &input.exclude_selectors,
    );

    let needs_extraction = formats.contains(&ScrapeFormat::Blocks) || !input.selectors.is_empty();
    let extraction_result = if needs_extraction {
        let mut extractor = Extractor::new();
        if formats.contains(&ScrapeFormat::Blocks) {
            extractor = extractor.with_blocks();
        }
        if !input.selectors.is_empty() {
            extractor = extractor
                .with_selectors(SelectorExtractor::with_definitions(input.selectors.clone()));
        }
        extractor.extract(&processed_html).ok()
    } else {
        None
    };
    let page_extraction =
        if formats.contains(&ScrapeFormat::Metadata) || formats.contains(&ScrapeFormat::Schema) {
            let mut extractor = Extractor::new();
            if formats.contains(&ScrapeFormat::Metadata) {
                extractor = extractor.with_metadata();
            }
            if formats.contains(&ScrapeFormat::Schema) {
                extractor = extractor.with_schema();
            }
            extractor.extract(&original_html).ok()
        } else {
            None
        };
    let metadata = page_extraction
        .as_ref()
        .and_then(|r| r.metadata.clone())
        .map(ScrapeMetadata::from);
    let schema = page_extraction.as_ref().and_then(|r| r.schema.clone());
    let blocks = extraction_result
        .as_ref()
        .and_then(|r| r.blocks.clone())
        .map(|b| b.blocks);
    let custom_extract = extraction_result
        .as_ref()
        .and_then(|r| r.custom.clone())
        .map(|c| c.values);

    let page_base = scrapix_parser::document_base_url(&original_html, final_url);
    let markdown = if formats.contains(&ScrapeFormat::Markdown) {
        let md = if input.only_main_content {
            html_to_main_content_markdown(&processed_html)
        } else {
            html_to_markdown(&processed_html)
        };
        Some(match &page_base {
            Some(base) => scrapix_parser::absolutize_markdown_links(&md, base),
            None => md,
        })
    } else {
        None
    };
    let content = if formats.contains(&ScrapeFormat::Content) {
        if input.only_main_content {
            Some(scrapix_parser::extract_content(&processed_html))
        } else {
            Some(scrapix_parser::markdown_to_text(&html_to_markdown(
                &processed_html,
            )))
        }
    } else {
        None
    };
    let html_output = if formats.contains(&ScrapeFormat::Html) {
        if input.only_main_content {
            Some(html_to_main_content_minihtml(&processed_html))
        } else {
            Some(html_to_minihtml(&processed_html))
        }
    } else {
        None
    };
    let raw_html = formats
        .contains(&ScrapeFormat::RawHtml)
        .then(|| original_html.clone());
    let links = if formats.contains(&ScrapeFormat::Links) || input.include_links {
        Some(extract_links_from_dom(
            &scraper::Html::parse_document(&processed_html),
            final_url,
        ))
    } else {
        None
    };
    let language = content
        .as_ref()
        .or(markdown.as_ref())
        .and_then(|text| detect_language_info(text))
        .map(|info| info.code);
    let ai_only_text = match (input.ai_wants_text, &content, &markdown) {
        (true, None, None) => Some(if input.only_main_content {
            html_to_main_content_markdown(&processed_html)
        } else {
            html_to_markdown(&processed_html)
        }),
        _ => None,
    };

    serde_json::json!({
        "markdown": markdown,
        "html": html_output,
        "raw_html": raw_html,
        "content": content,
        "metadata": metadata,
        "links": links,
        "language": language,
        "schema": schema,
        "blocks": blocks,
        "extract": custom_extract,
        "ai_only_text": ai_only_text,
    })
}

fn stage_json(output: HtmlStageOutput) -> serde_json::Value {
    serde_json::json!({
        "markdown": output.markdown,
        "html": output.html,
        "raw_html": output.raw_html,
        "content": output.content,
        "metadata": output.metadata,
        "links": output.links,
        "language": output.language,
        "schema": output.schema,
        "blocks": output.blocks,
        "extract": output.extract,
        "ai_only_text": output.ai_only_text,
    })
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|v| v.to_string()).collect()
}

/// Every combination of formats and options returns exactly what the
/// per-format pipeline returned.
#[test]
fn html_stage_matches_the_per_format_pipeline() {
    use ScrapeFormat::*;

    let format_sets: Vec<Vec<ScrapeFormat>> = vec![
        vec![Markdown, Content, Metadata],
        vec![Markdown],
        vec![Content],
        vec![Markdown, Content],
        vec![Html],
        vec![RawHtml],
        vec![Links],
        vec![Metadata],
        vec![Schema],
        vec![Blocks],
        vec![Screenshot],
        vec![
            Markdown, Html, RawHtml, Content, Links, Metadata, Schema, Blocks,
        ],
    ];
    let selector_sets: Vec<(Vec<String>, Vec<String>)> = vec![
        (vec![], vec![]),
        (strings(&["main"]), vec![]),
        (strings(&["article h1", "article p"]), vec![]),
        (vec![], strings(&["nav", ".ad"])),
        (vec![], strings(&["header", ".ad", "footer", "header a"])),
        (strings(&["main"]), strings(&[".ad", "table"])),
        (strings(&[".nonexistent"]), strings(&["nav"])),
        (strings(&["[[[invalid"]), strings(&["[[[invalid", "footer"])),
    ];
    let extract: HashMap<String, SelectorDefinition> = request(serde_json::json!({
        "url": "https://example.com/",
        "extract": {"prices": [".price"], "title": "h1"}
    }))
    .extract
    .iter()
    .map(|(field, sel)| (field.clone(), sel.to_definition()))
    .collect();

    let mut compared = 0;
    for formats in &format_sets {
        for (include, exclude) in &selector_sets {
            for only_main_content in [true, false] {
                for include_links in [false, true] {
                    for selectors in [HashMap::new(), extract.clone()] {
                        for ai_wants_text in [false, true] {
                            let input = || HtmlStageInput {
                                html: PAGE.to_string(),
                                final_url: "https://example.com/pricing/index.html".to_string(),
                                formats: formats.clone(),
                                include_selectors: include.clone(),
                                exclude_selectors: exclude.clone(),
                                selectors: selectors.clone(),
                                only_main_content,
                                include_links,
                                ai_wants_text,
                            };
                            let expected = legacy_html_stage(&input());
                            let actual = stage_json(run_html_stage(input()));
                            assert_eq!(
                                actual, expected,
                                "formats {formats:?}, include {include:?}, exclude {exclude:?}, \
                                 only_main_content {only_main_content}, include_links \
                                 {include_links}, ai_wants_text {ai_wants_text}"
                            );
                            compared += 1;
                        }
                    }
                }
            }
        }
    }
    assert!(compared > 1000);
}

/// The fixture exercises what it is meant to: a wrong pipeline would not
/// pass by returning nothing.
#[test]
fn html_stage_returns_every_requested_format() {
    use ScrapeFormat::*;
    let output = run_html_stage(HtmlStageInput {
        html: PAGE.to_string(),
        final_url: "https://example.com/pricing/index.html".to_string(),
        formats: vec![
            Markdown, Html, RawHtml, Content, Links, Metadata, Schema, Blocks,
        ],
        include_selectors: vec![],
        exclude_selectors: strings(&[".ad"]),
        selectors: HashMap::new(),
        only_main_content: false,
        include_links: false,
        ai_wants_text: false,
    });
    let markdown = output.markdown.unwrap();
    assert!(markdown.contains("42 euros"), "{markdown}");
    assert!(!markdown.contains("Buy now"), "{markdown}");
    // Relative links resolve against the original page's <base href>.
    assert!(
        markdown.contains("https://example.com/docs/terms"),
        "{markdown}"
    );
    // With `only_main_content: false`, `content` is the markdown's text.
    let content = output.content.unwrap();
    assert!(
        content.contains("Footer text") && content.contains("42 euros"),
        "{content}"
    );
    assert!(!content.contains("Buy now") && !content.contains("**"));
    assert_eq!(output.raw_html.as_deref(), Some(PAGE));
    assert!(output.html.unwrap().contains("Pricing"));
    let links = output.links.unwrap();
    assert!(links.contains(&"https://example.com/legal".to_string()));
    assert!(
        !links.iter().any(|l| l.contains("ads.example")),
        "{links:?}"
    );
    assert_eq!(
        output.metadata.unwrap().title.as_deref(),
        Some("Pricing & plans")
    );
    assert!(output.schema.is_some());
    assert!(!output.blocks.unwrap().is_empty());
}

/// An excluded element is cut out of the source text: the rest of the page
/// is byte for byte what the site sent.
#[test]
fn exclude_selectors_keep_the_source_text_of_the_rest() {
    let html = "<html><body>\n  <P CLASS=keep>Tom &amp; Jerry<br/>\n  <nav class=\"menu\"><a href=\"/a\">A</a></nav>\n  <footer><p>bye</p></footer></body></html>";
    let result = preprocess_html(html, &[], &strings(&["nav", "footer p"]));
    assert_eq!(
        result,
        "<html><body>\n  <P CLASS=keep>Tom &amp; Jerry<br/>\n  \n  <footer></footer></body></html>"
    );
}

/// An element whose serialization is not its source text (unquoted
/// attribute, unclosed tag) used to be kept silently; it is removed now.
#[test]
fn exclude_selectors_remove_elements_written_unlike_their_serialization() {
    let html = "<html><body><p>Keep</p><nav class=menu><a href=/a>Remove</a></nav>\
                <div id='ad'>Remove too<p>unclosed</div><p>Also keep</p></body></html>";
    assert!(
        legacy_preprocess_html(html, &[], &strings(&["nav", "#ad"])).contains("Remove"),
        "the per-selector text removal kept them"
    );
    let result = preprocess_html(html, &[], &strings(&["nav", "#ad"]));
    assert!(!result.contains("Remove"), "{result}");
    assert!(
        result.contains("<p>Keep</p>") && result.contains("<p>Also keep</p>"),
        "{result}"
    );
}

/// A match inside another match goes with it, and an identical element
/// elsewhere that no selector matches stays.
#[test]
fn exclude_selectors_remove_nested_matches_once() {
    let html = "<html><body><p><a class=\"x\">same</a></p>\
                <div class=\"side\"><a class=\"x\">same</a></div></body></html>";
    let result = preprocess_html(html, &[], &strings(&[".side", ".side a"]));
    assert_eq!(
        result,
        "<html><body><p><a class=\"x\">same</a></p></body></html>"
    );
}

#[test]
fn preprocess_without_a_change_borrows_the_source() {
    use std::borrow::Cow;
    let html = "<html><body><p>Hello</p></body></html>";
    assert!(matches!(preprocess_html(html, &[], &[]), Cow::Borrowed(_)));
    assert!(matches!(
        preprocess_html(html, &strings(&[".none"]), &strings(&["p"])),
        Cow::Borrowed(_)
    ));
    assert!(matches!(
        preprocess_html(html, &[], &strings(&[".none"])),
        Cow::Borrowed(_)
    ));
}

/// With `only_main_content: false`, `markdown` and `content` come from one
/// conversion and agree with each other.
#[tokio::test]
async fn full_page_markdown_and_content_through_the_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/pricing"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(PAGE, "text/html"))
        .expect(1)
        .mount(&server)
        .await;
    let bus = ChannelBus::new();
    let state = test_state(&bus);
    let url = local(&server, "/pricing");
    let req = request(serde_json::json!({
        "url": url,
        "formats": ["markdown", "content", "links"],
        "only_main_content": false,
    }));
    let response = perform_scrape(&state, &None, &req).await.unwrap();
    assert!(response.success);
    let markdown = response.markdown.unwrap();
    let content = response.content.unwrap();
    assert!(markdown.contains("Pricing"), "{markdown}");
    assert!(markdown.contains("Footer text"), "{markdown}");
    assert_eq!(
        content,
        scrapix_parser::markdown_to_text(&html_to_markdown(PAGE))
    );
    assert!(response.links.unwrap().contains(&local(&server, "/terms")));
}

/// A page answering 503 is fetched twice (one retry), not four times.
#[tokio::test]
async fn a_failing_page_is_retried_once() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/down"))
        .respond_with(ResponseTemplate::new(503))
        .expect(2)
        .mount(&server)
        .await;
    let bus = ChannelBus::new();
    let state = test_state(&bus);
    let started = std::time::Instant::now();
    let req = request(serde_json::json!({"url": local(&server, "/down")}));
    let response = perform_scrape(&state, &None, &req).await.unwrap();
    assert!(!response.success);
    assert_eq!(response.status_code, 503);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "one backoff, not three: {:?}",
        started.elapsed()
    );
}

/// Requests with headers go through the shared fetcher: the headers are
/// sent (replacing the fetcher's own of the same name), blocked ones are
/// not, and the retry policy is the same.
#[tokio::test]
async fn headers_are_sent_through_the_shared_fetcher() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/private"))
        .and(header("authorization", "Bearer s3cr3t"))
        .and(header("accept-language", "fr"))
        .and(header("user-agent", "custom-agent/1.0"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw("<html><body><p>in</p></body></html>", "text/html"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let bus = ChannelBus::new();
    let state = test_state(&bus);
    let req = request(serde_json::json!({
        "url": local(&server, "/private"),
        "formats": ["rawhtml"],
        "headers": {
            "Authorization": "Bearer s3cr3t",
            "Accept-Language": "fr",
            "User-Agent": "custom-agent/1.0",
            "Connection": "close",
        },
    }));
    let response = perform_scrape(&state, &None, &req).await.unwrap();
    assert!(response.success, "status {}", response.status_code);
    let sent = &server.received_requests().await.unwrap()[0];
    assert_eq!(
        sent.headers.get_all("accept-language").iter().count(),
        1,
        "the header replaces the fetcher's default"
    );
    // No one-off fetcher was needed: nothing built its shared parts.
    assert!(state.cookie_fetch.robots.get().is_none());
    assert!(state.cookie_fetch.dns.get().is_none());
}

#[test]
fn one_header_per_name_whatever_its_case() {
    let headers = HashMap::from([
        ("X-Token".to_string(), "a".to_string()),
        ("x-token".to_string(), "b".to_string()),
        ("Accept".to_string(), "text/html".to_string()),
    ]);
    assert_eq!(
        one_per_header_name(forwardable_headers(&headers)),
        vec![
            ("Accept".to_string(), "text/html".to_string()),
            ("x-token".to_string(), "b".to_string()),
        ]
    );
}

/// `timeout_ms` bounds the whole fetch of a request with cookies too (it
/// was only each attempt's timeout).
#[tokio::test]
async fn a_slow_page_times_out_with_cookies_as_without() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/slow"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw("<html><body>late</body></html>", "text/html")
                .set_delay(Duration::from_secs(5)),
        )
        .mount(&server)
        .await;
    let bus = ChannelBus::new();
    let state = test_state(&bus);
    for extra in [
        serde_json::json!({}),
        serde_json::json!({"headers": {"X-Test": "1"}}),
        serde_json::json!({"cookies": [{"name": "sid", "value": "abc"}]}),
        serde_json::json!({"headers": {"Cookie": "sid=abc"}}),
    ] {
        let mut body = serde_json::json!({"url": local(&server, "/slow"), "timeout_ms": 1000});
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let started = std::time::Instant::now();
        let err = perform_scrape(&state, &None, &request(body))
            .await
            .err()
            .unwrap_or_else(|| panic!("{extra} must time out"));
        assert_eq!(err.code, "timeout", "{extra}: {}", err.error);
        assert!(
            started.elapsed() < Duration::from_millis(2500),
            "{extra}: {:?}",
            started.elapsed()
        );
    }
}

/// A `Cookie` header and `cookies` both use a one-off fetcher whose DNS
/// and robots caches are shared, and reach the server.
#[tokio::test]
async fn cookies_use_a_one_off_fetcher_with_shared_caches() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/account"))
        .and(header("cookie", "sid=abc"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw("<html><body><p>in</p></body></html>", "text/html"),
        )
        .expect(2)
        .mount(&server)
        .await;
    let bus = ChannelBus::new();
    let state = test_state(&bus);
    for extra in [
        serde_json::json!({"cookies": [{"name": "sid", "value": "abc"}]}),
        serde_json::json!({"headers": {"Cookie": "sid=abc"}}),
    ] {
        let mut body = serde_json::json!({"url": local(&server, "/account")});
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let response = perform_scrape(&state, &None, &request(body)).await.unwrap();
        assert!(response.success, "{extra}: status {}", response.status_code);
    }
    let dns = state.cookie_fetch.dns.get().expect("built once").clone();
    assert!(Arc::ptr_eq(&dns, &state.cookie_fetch.dns().unwrap()));
}
