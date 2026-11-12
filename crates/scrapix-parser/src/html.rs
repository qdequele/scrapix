//! HTML parsing and document extraction

use std::collections::HashMap;

use async_trait::async_trait;
use scraper::{Html, Selector};
use tracing::{debug, instrument};
use url::Url;

use scrapix_core::{Document, RawPage, Result, ScrapixError};

use crate::language::detect_language_sampled;
use crate::markdown::html_to_markdown_for_page;
use crate::readability::{extract_content_from_dom, ReadabilityConfig};

/// HTML parser configuration
#[derive(Debug, Clone)]
pub struct HtmlParserConfig {
    /// Whether to extract content using readability
    pub extract_content: bool,
    /// Whether to convert content to markdown
    pub convert_to_markdown: bool,
    /// Whether to detect language
    pub detect_language: bool,
    /// Whether to extract schema.org/JSON-LD data
    pub extract_schema: bool,
    /// Whether to extract Open Graph meta tags
    pub extract_og_tags: bool,
    /// Minimum content length (characters) to consider valid
    pub min_content_length: usize,
}

impl Default for HtmlParserConfig {
    fn default() -> Self {
        Self {
            extract_content: true,
            convert_to_markdown: true,
            detect_language: true,
            extract_schema: true,
            extract_og_tags: true,
            min_content_length: 100,
        }
    }
}

/// Per-call options of [`HtmlParser::parse_with_options`] and
/// [`HtmlParser::parse_with_dom`]. Each one can only turn off a step the
/// parser's [`HtmlParserConfig`] enables, for a page that will not use its
/// result; the default keeps every configured step, as [`HtmlParser::parse`]
/// does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HtmlParseOptions {
    /// Convert the page to Markdown (`Document::markdown`). The conversion
    /// re-parses the HTML and is the most expensive step of a parse.
    pub convert_to_markdown: bool,
    /// Collect the page's JSON-LD (`Document::schema`).
    pub extract_schema: bool,
}

impl Default for HtmlParseOptions {
    fn default() -> Self {
        Self {
            convert_to_markdown: true,
            extract_schema: true,
        }
    }
}

/// HTML parser for extracting documents from raw pages
pub struct HtmlParser {
    config: HtmlParserConfig,
    // Pre-compiled selectors
    title_selector: Selector,
    meta_selector: Selector,
    script_ld_selector: Selector,
    h1_selector: Selector,
    link_selector: Selector,
}

impl HtmlParser {
    /// Create a new HTML parser
    pub fn new(config: HtmlParserConfig) -> Self {
        Self {
            config,
            title_selector: Selector::parse("title").unwrap(),
            meta_selector: Selector::parse("meta").unwrap(),
            script_ld_selector: Selector::parse("script[type='application/ld+json']").unwrap(),
            h1_selector: Selector::parse("h1").unwrap(),
            link_selector: Selector::parse("a[href]").unwrap(),
        }
    }

    /// Create a parser with default configuration
    pub fn with_defaults() -> Self {
        Self::new(HtmlParserConfig::default())
    }

    /// Parse a raw page into a document
    pub fn parse(&self, page: &RawPage) -> Result<Document> {
        self.parse_with_options(page, &HtmlParseOptions::default())
    }

    /// [`parse`](Self::parse), skipping the steps `options` turns off. The
    /// other fields of the document are the same as `parse`'s.
    pub fn parse_with_options(
        &self,
        page: &RawPage,
        options: &HtmlParseOptions,
    ) -> Result<Document> {
        self.parse_with_dom(page, options, |_, _| ())
            .map(|(doc, ())| doc)
    }

    /// [`parse_with_options`](Self::parse_with_options), then call
    /// `with_dom` with the page's parsed DOM and the document, so further
    /// extractions (schema options, custom selectors, block splitting) reuse
    /// the DOM instead of parsing the HTML again. Returns the document and
    /// what `with_dom` returned. (`scraper::Html` is not `Send`: the DOM
    /// never leaves this call.)
    #[instrument(name = "parse", skip(self, page, options, with_dom), fields(url = %page.url))]
    pub fn parse_with_dom<T>(
        &self,
        page: &RawPage,
        options: &HtmlParseOptions,
        with_dom: impl FnOnce(&Html, &Document) -> T,
    ) -> Result<(Document, T)> {
        let url = Url::parse(&page.final_url).map_err(|e| {
            ScrapixError::Parse(format!("Failed to parse URL '{}': {}", page.final_url, e))
        })?;
        let domain = url
            .host_str()
            .ok_or_else(|| ScrapixError::Parse(format!("URL has no host: '{}'", page.final_url)))?;

        let mut doc = Document::new(&page.final_url, domain);

        // Parse HTML
        let html = Html::parse_document(&page.html);

        // Extract title
        doc.title = self.extract_title(&html);

        // Extract metadata
        if self.config.extract_og_tags {
            doc.metadata = Some(self.extract_metadata(&html));
        }

        // Extract schema.org data
        if self.config.extract_schema && options.extract_schema {
            doc.schema = self.extract_schema(&html);
        }

        // Extract main content - reuse the already-parsed DOM
        if self.config.extract_content {
            let content = extract_content_from_dom(&html, &ReadabilityConfig::default());
            if content.len() >= self.config.min_content_length {
                // Markdown comes from the page's HTML (main content only, as
                // `/scrape` does by default): readability's output is plain
                // text, so converting it gave the text back with escapes.
                if self.config.convert_to_markdown && options.convert_to_markdown {
                    doc.markdown =
                        Some(html_to_markdown_for_page(&page.html, &page.final_url, true));
                }

                // Readability output is already plain text, just normalize whitespace
                doc.content = Some(clean_extracted_text(&content));
            }
        }

        // Detect language
        if self.config.detect_language {
            if let Some(ref content) = doc.content {
                doc.language = detect_language_sampled(content);
            }
        }

        // Extract URL tags (path segments)
        doc.urls_tags = Some(self.extract_url_tags(&url));

        debug!(
            title = ?doc.title,
            content_len = doc.content.as_ref().map(|c| c.len()).unwrap_or(0),
            "Parsed document"
        );

        let extracted = with_dom(&html, &doc);
        Ok((doc, extracted))
    }

    /// Extract page title
    fn extract_title(&self, html: &Html) -> Option<String> {
        // Try <title> tag first
        if let Some(title_el) = html.select(&self.title_selector).next() {
            let title = collapse_whitespace(&title_el.text().collect::<String>());
            if !title.is_empty() {
                return Some(title);
            }
        }

        // Try og:title
        for meta in html.select(&self.meta_selector) {
            if meta.value().attr("property") == Some("og:title") {
                if let Some(content) = meta.value().attr("content") {
                    let content = collapse_whitespace(content);
                    if !content.is_empty() {
                        return Some(content);
                    }
                }
            }
        }

        // Try h1
        if let Some(h1) = html.select(&self.h1_selector).next() {
            let text = collapse_whitespace(&h1.text().collect::<String>());
            if !text.is_empty() {
                return Some(text);
            }
        }

        None
    }

    /// Extract metadata from meta tags
    fn extract_metadata(&self, html: &Html) -> HashMap<String, String> {
        let mut metadata = HashMap::new();

        for meta in html.select(&self.meta_selector) {
            let element = meta.value();

            // Get name or property
            let key = element.attr("name").or_else(|| element.attr("property"));

            // Get content
            let content = element.attr("content");

            if let (Some(key), Some(content)) = (key, content) {
                let key = key.trim().to_string();
                let content = content.trim().to_string();

                if !key.is_empty() && !content.is_empty() {
                    // Normalize common keys
                    let normalized_key = match key.as_str() {
                        "og:title" => "title".to_string(),
                        "og:description" => "description".to_string(),
                        "og:image" => "image".to_string(),
                        "og:url" => "url".to_string(),
                        "og:type" => "type".to_string(),
                        "og:site_name" => "site_name".to_string(),
                        "twitter:title" => "twitter_title".to_string(),
                        "twitter:description" => "twitter_description".to_string(),
                        "twitter:image" => "twitter_image".to_string(),
                        _ => key,
                    };

                    metadata.insert(normalized_key, content);
                }
            }
        }

        metadata
    }

    /// Extract schema.org JSON-LD data
    fn extract_schema(&self, html: &Html) -> Option<serde_json::Value> {
        let mut schemas = Vec::new();

        for script in html.select(&self.script_ld_selector) {
            let json_text = script.text().collect::<String>();
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&json_text) {
                schemas.push(parsed);
            }
        }

        match schemas.len() {
            0 => None,
            1 => Some(schemas.remove(0)),
            _ => Some(serde_json::Value::Array(schemas)),
        }
    }

    /// Extract URL path segments as tags
    fn extract_url_tags(&self, url: &Url) -> Vec<String> {
        let path = url.path();
        let segments: Vec<String> = path
            .split('/')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();

        // Build hierarchical tags
        let mut tags = Vec::new();
        let mut current = String::new();

        for segment in segments {
            if !current.is_empty() {
                current.push('/');
            }
            current.push_str(&segment);
            tags.push(format!("/{}", current));
        }

        tags
    }

    /// Extract links from the page
    pub fn extract_links(&self, page: &RawPage) -> Vec<String> {
        let base_url = match Url::parse(&page.final_url) {
            Ok(url) => url,
            Err(_) => return vec![],
        };

        let html = Html::parse_document(&page.html);
        let mut links = Vec::new();

        for element in html.select(&self.link_selector) {
            if let Some(href) = element.value().attr("href") {
                // Skip non-HTTP links
                if href.starts_with("javascript:")
                    || href.starts_with("mailto:")
                    || href.starts_with("tel:")
                    || href.starts_with('#')
                {
                    continue;
                }

                // Resolve relative URLs
                if let Ok(resolved) = base_url.join(href) {
                    if resolved.scheme() == "http" || resolved.scheme() == "https" {
                        links.push(resolved.to_string());
                    }
                }
            }
        }

        links
    }

    /// Extract links from a pre-parsed DOM, avoiding redundant parsing
    pub fn extract_links_from_dom(&self, document: &Html, base_url: &str) -> Vec<String> {
        let base = match Url::parse(base_url) {
            Ok(url) => url,
            Err(_) => return vec![],
        };

        let mut links = Vec::new();

        for element in document.select(&self.link_selector) {
            if let Some(href) = element.value().attr("href") {
                if href.starts_with("javascript:")
                    || href.starts_with("mailto:")
                    || href.starts_with("tel:")
                    || href.starts_with('#')
                {
                    continue;
                }

                if let Ok(resolved) = base.join(href) {
                    if resolved.scheme() == "http" || resolved.scheme() == "https" {
                        links.push(resolved.to_string());
                    }
                }
            }
        }

        links
    }
}

/// Clean extracted text content (normalize whitespace only, no HTML parsing).
/// Readability output is already plain text, so re-parsing as HTML is wasteful.
/// `text` with each run of whitespace (newlines, indentation) as one space.
fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clean_extracted_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Implementation of core Parser trait
#[async_trait]
impl scrapix_core::traits::Parser for HtmlParser {
    async fn parse(&self, page: &RawPage) -> Result<Document> {
        HtmlParser::parse(self, page)
    }

    async fn extract_links(&self, page: &RawPage) -> Result<Vec<String>> {
        Ok(self.extract_links(page))
    }
}

/// Builder for HtmlParser
pub struct HtmlParserBuilder {
    config: HtmlParserConfig,
}

impl HtmlParserBuilder {
    pub fn new() -> Self {
        Self {
            config: HtmlParserConfig::default(),
        }
    }

    pub fn extract_content(mut self, extract: bool) -> Self {
        self.config.extract_content = extract;
        self
    }

    pub fn convert_to_markdown(mut self, convert: bool) -> Self {
        self.config.convert_to_markdown = convert;
        self
    }

    pub fn detect_language(mut self, detect: bool) -> Self {
        self.config.detect_language = detect;
        self
    }

    pub fn extract_schema(mut self, extract: bool) -> Self {
        self.config.extract_schema = extract;
        self
    }

    pub fn min_content_length(mut self, length: usize) -> Self {
        self.config.min_content_length = length;
        self
    }

    pub fn build(self) -> HtmlParser {
        HtmlParser::new(self.config)
    }
}

impl Default for HtmlParserBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_page(url: &str, html: &str) -> RawPage {
        RawPage {
            url: url.to_string(),
            final_url: url.to_string(),
            status: 200,
            headers: Default::default(),
            html: html.to_string(),
            content_type: Some("text/html".to_string()),
            js_rendered: false,
            fetched_at: chrono::Utc::now(),
            fetch_duration_ms: 100,
        }
    }

    #[test]
    fn test_extract_title() {
        let parser = HtmlParser::with_defaults();
        let html = Html::parse_document("<html><head><title>Test Title</title></head></html>");

        assert_eq!(parser.extract_title(&html), Some("Test Title".to_string()));
    }

    #[test]
    fn title_whitespace_is_collapsed() {
        let parser = HtmlParser::with_defaults();
        let html = Html::parse_document(
            "<html><head><title>Poetry |\n     Books to Scrape - Sandbox\n</title></head></html>",
        );
        assert_eq!(
            parser.extract_title(&html),
            Some("Poetry | Books to Scrape - Sandbox".to_string())
        );
    }

    #[test]
    fn crawl_markdown_keeps_the_page_structure() {
        let parser = HtmlParser::with_defaults();
        let html = r#"<html><head><title>Guide</title></head><body>
            <nav><a href="/">Home</a></nav>
            <main><article>
              <h1>Install the engine</h1>
              <p>Run the <strong>installer</strong> and read the
                 <a href="https://a.test/docs">docs</a> for the details you need.</p>
              <ul><li>First step to follow</li><li>Second step to follow</li></ul>
              <p>Another paragraph with enough text to be the main content here.</p>
            </article></main>
            <footer>Copyright</footer></body></html>"#;
        let doc = parser
            .parse(&make_page("https://a.test/guide", html))
            .unwrap();
        let md = doc.markdown.unwrap();
        assert!(md.contains("Install the engine\n==="), "{md}");
        assert!(md.contains("**installer**"), "{md}");
        assert!(md.contains("[docs](https://a.test/docs)"), "{md}");
        assert!(md.contains("First step to follow"), "{md}");
        assert!(
            md.lines().any(|l| l.trim_start().starts_with(['*', '-'])),
            "{md}"
        );
        assert!(!md.contains("Copyright"), "{md}");
        // `content` stays plain text.
        assert!(!doc.content.unwrap().contains("**"));
    }

    #[test]
    fn test_extract_title_from_og() {
        let parser = HtmlParser::with_defaults();
        let html = Html::parse_document(
            r#"<html><head><meta property="og:title" content="OG Title"></head></html>"#,
        );

        assert_eq!(parser.extract_title(&html), Some("OG Title".to_string()));
    }

    #[test]
    fn test_extract_metadata() {
        let parser = HtmlParser::with_defaults();
        let html = Html::parse_document(
            r#"<html><head>
                <meta name="description" content="A test page">
                <meta property="og:image" content="https://example.com/image.png">
            </head></html>"#,
        );

        let metadata = parser.extract_metadata(&html);
        assert_eq!(
            metadata.get("description"),
            Some(&"A test page".to_string())
        );
        assert_eq!(
            metadata.get("image"),
            Some(&"https://example.com/image.png".to_string())
        );
    }

    #[test]
    fn test_extract_schema() {
        let parser = HtmlParser::with_defaults();
        let html = Html::parse_document(
            r#"<html><head>
                <script type="application/ld+json">
                    {"@type": "Article", "name": "Test"}
                </script>
            </head></html>"#,
        );

        let schema = parser.extract_schema(&html);
        assert!(schema.is_some());
        let schema = schema.unwrap();
        assert_eq!(schema["@type"], "Article");
    }

    #[test]
    fn test_extract_url_tags() {
        let parser = HtmlParser::with_defaults();
        let url = Url::parse("https://example.com/docs/api/reference").unwrap();

        let tags = parser.extract_url_tags(&url);
        assert_eq!(tags, vec!["/docs", "/docs/api", "/docs/api/reference"]);
    }

    #[test]
    fn test_extract_links() {
        let parser = HtmlParser::with_defaults();
        let page = make_page(
            "https://example.com/page",
            r#"<html><body>
                <a href="/other">Other</a>
                <a href="https://example.com/absolute">Absolute</a>
                <a href="javascript:void(0)">JS</a>
            </body></html>"#,
        );

        let links = parser.extract_links(&page);
        assert_eq!(links.len(), 2);
        assert!(links.contains(&"https://example.com/other".to_string()));
        assert!(links.contains(&"https://example.com/absolute".to_string()));
    }

    const RICH_PAGE: &str = r#"<html><head><title>Guide</title>
        <meta name="description" content="How the pipeline works">
        <script type="application/ld+json">{"@type":"Article","headline":"Guide"}</script>
        </head><body><nav><a href="/home">Home</a></nav><main><h1>Guide</h1>
        <p>This page explains in detail how the crawler, the frontier and the content worker
        cooperate to index documents, with <a href="intro">relative links</a> and lists.</p>
        <h2 id="steps">Steps</h2><ul><li>Fetch the page</li><li>Parse it once</li></ul>
        <p>Every step is described at length so that the extracted content is long enough
        to be kept and its language detected reliably.</p></main>
        <footer>Footer text</footer></body></html>"#;

    /// A document as JSON, without the parse timestamp.
    fn comparable(doc: &Document) -> serde_json::Value {
        let mut value = serde_json::to_value(doc).unwrap();
        value.as_object_mut().unwrap().remove("crawled_at");
        value
    }

    #[test]
    fn default_options_parse_like_parse() {
        let parser = HtmlParser::with_defaults();
        let page = make_page("https://example.com/docs/guide", RICH_PAGE);
        let reference = parser.parse(&page).unwrap();
        assert!(reference.markdown.is_some() && reference.schema.is_some());
        let doc = parser
            .parse_with_options(&page, &HtmlParseOptions::default())
            .unwrap();
        assert_eq!(comparable(&doc), comparable(&reference));
    }

    #[test]
    fn options_only_drop_the_steps_they_turn_off() {
        let parser = HtmlParser::with_defaults();
        let page = make_page("https://example.com/docs/guide", RICH_PAGE);
        let reference = parser.parse(&page).unwrap();

        let no_markdown = parser
            .parse_with_options(
                &page,
                &HtmlParseOptions {
                    convert_to_markdown: false,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(no_markdown.markdown.is_none());
        let mut expected = reference.clone();
        expected.markdown = None;
        assert_eq!(comparable(&no_markdown), comparable(&expected));

        let no_schema = parser
            .parse_with_options(
                &page,
                &HtmlParseOptions {
                    extract_schema: false,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(no_schema.schema.is_none());
        let mut expected = reference.clone();
        expected.schema = None;
        assert_eq!(comparable(&no_schema), comparable(&expected));
    }

    #[test]
    fn options_never_enable_a_step_the_config_disables() {
        let parser = HtmlParserBuilder::new()
            .convert_to_markdown(false)
            .extract_schema(false)
            .build();
        let page = make_page("https://example.com/docs/guide", RICH_PAGE);
        let doc = parser
            .parse_with_options(&page, &HtmlParseOptions::default())
            .unwrap();
        assert!(doc.markdown.is_none() && doc.schema.is_none());
    }

    #[test]
    fn parse_with_dom_shares_the_parsed_dom() {
        let parser = HtmlParser::with_defaults();
        let page = make_page("https://example.com/docs/guide", RICH_PAGE);
        let reference = parser.parse(&page).unwrap();
        let (doc, (items, had_content)) = parser
            .parse_with_dom(&page, &HtmlParseOptions::default(), |dom, doc| {
                let li = Selector::parse("li").unwrap();
                let items: Vec<String> = dom
                    .select(&li)
                    .map(|el| el.text().collect::<String>())
                    .collect();
                (items, doc.content.is_some())
            })
            .unwrap();
        assert_eq!(items, vec!["Fetch the page", "Parse it once"]);
        assert!(had_content, "the closure sees the parsed document");
        assert_eq!(comparable(&doc), comparable(&reference));
    }
}
