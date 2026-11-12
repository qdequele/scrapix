//! HTML to Markdown conversion

use html2md::parse_html;
use regex::Regex;
use std::borrow::Cow;
use std::sync::OnceLock;

/// Configuration for markdown conversion
#[derive(Debug, Clone)]
pub struct MarkdownConfig {
    /// Maximum line length for wrapping
    pub max_line_length: Option<usize>,
    /// Whether to preserve link titles
    pub preserve_link_titles: bool,
    /// Whether to convert images to markdown
    pub convert_images: bool,
    /// Whether to convert code blocks
    pub convert_code_blocks: bool,
    /// Whether to strip non-content sections (nav, footer, header, aside)
    pub only_main_content: bool,
}

impl Default for MarkdownConfig {
    fn default() -> Self {
        Self {
            max_line_length: None,
            preserve_link_titles: true,
            convert_images: true,
            convert_code_blocks: true,
            only_main_content: false,
        }
    }
}

/// Convert HTML to Markdown (full page)
pub fn html_to_markdown(html: &str) -> String {
    html_to_markdown_with_config(html, &MarkdownConfig::default())
}

/// Convert HTML to clean Markdown, stripping boilerplate (nav, footer, etc.)
pub fn html_to_main_content_markdown(html: &str) -> String {
    let config = MarkdownConfig {
        only_main_content: true,
        ..MarkdownConfig::default()
    };
    html_to_markdown_with_config(html, &config)
}

/// Convert HTML to Markdown with custom configuration
pub fn html_to_markdown_with_config(html: &str, config: &MarkdownConfig) -> String {
    // Strip non-content tags (script, style, etc.)
    let mut cleaned_html = strip_non_content_tags(html);

    // Optionally strip boilerplate sections
    if config.only_main_content {
        cleaned_html = strip_boilerplate_tags(cleaned_html);
    }

    // Use html2md for the basic conversion
    let markdown = parse_html(&cleaned_html);

    // Strip any remaining HTML tags that html2md didn't convert
    let stripped = strip_remaining_html(&markdown);

    // Post-process the markdown
    clean_markdown(&stripped)
}

/// Convert HTML to Markdown whose links and images point at absolute URLs.
///
/// Relative targets (`/login`, `intro`, `../img.png`) resolve against the
/// page's `<base href>` when it has one, else against `page_url`. With
/// `only_main_content`, nav/header/footer/aside are dropped as in
/// [`html_to_main_content_markdown`].
pub fn html_to_markdown_for_page(html: &str, page_url: &str, only_main_content: bool) -> String {
    let markdown = if only_main_content {
        html_to_main_content_markdown(html)
    } else {
        html_to_markdown(html)
    };
    match document_base_url(html, page_url) {
        Some(base) => absolutize_markdown_links(&markdown, &base),
        None => markdown,
    }
}

/// The URL a page's relative links resolve against: its `<base href>`
/// (itself resolved against `page_url`) when present, else `page_url`.
/// `None` when `page_url` is not an absolute URL.
pub fn document_base_url(html: &str, page_url: &str) -> Option<url::Url> {
    static BASE_RE: OnceLock<Regex> = OnceLock::new();
    let base_re = BASE_RE.get_or_init(|| {
        Regex::new(r#"(?is)<base\s[^>]*?href\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))"#).unwrap()
    });
    static HEAD_END_RE: OnceLock<Regex> = OnceLock::new();
    let head_end_re = HEAD_END_RE.get_or_init(|| Regex::new(r"(?i)</head|<body").unwrap());

    let page = url::Url::parse(page_url).ok()?;
    // Only a <base> in the head counts: look before `</head>` / `<body>`
    // (any case), else in the first 16 KiB, cut on a char boundary.
    let head_end = match head_end_re.find(html) {
        Some(m) => m.start(),
        None => {
            let mut end = html.len().min(16 * 1024);
            while !html.is_char_boundary(end) {
                end -= 1;
            }
            end
        }
    };
    let base = base_re.captures(&html[..head_end]).and_then(|c| {
        c.get(1)
            .or_else(|| c.get(2))
            .or_else(|| c.get(3))
            .map(|m| m.as_str().trim().to_string())
    });
    let resolved = base
        .filter(|href| !href.is_empty())
        .and_then(|href| page.join(&href).ok())
        // A `javascript:` or other non-web base is ignored.
        .filter(|u| matches!(u.scheme(), "http" | "https"));
    Some(resolved.unwrap_or(page))
}

/// Rewrite the targets of markdown links and images to absolute URLs.
///
/// - `[text](/path)` → `[text](https://host/path)`, resolved against `base`;
/// - a link with no target, `[text]()` (from an `<a>` with no `href`), is
///   replaced by its text;
/// - an image with no target, `![alt]()`, is removed;
/// - absolute (`https:`, `mailto:`, `data:` …) and fragment-only (`#id`)
///   targets are kept as they are. A link wrapping an image
///   (`[![alt](src)](href)`) has both rewritten. Fenced code blocks (```` ``` ````
///   and `~~~`) and inline code spans are left untouched.
pub fn absolutize_markdown_links(markdown: &str, base: &url::Url) -> String {
    let mut out = String::with_capacity(markdown.len() + 64);
    let mut fence: Option<&str> = None;
    for (i, line) in markdown.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let trimmed = line.trim_start();
        let marker = ["```", "~~~"].into_iter().find(|m| trimmed.starts_with(m));
        match (fence, marker) {
            (None, Some(m)) => {
                fence = Some(m);
                out.push_str(line);
                continue;
            }
            (Some(open), Some(m)) if open == m => {
                fence = None;
                out.push_str(line);
                continue;
            }
            _ => {}
        }
        if fence.is_some() {
            out.push_str(line);
            continue;
        }
        // Rewrite outside inline code spans only: odd segments of a
        // backtick split are code.
        for (j, segment) in line.split('`').enumerate() {
            if j > 0 {
                out.push('`');
            }
            if j % 2 == 0 {
                out.push_str(&rewrite_links(segment, base));
            } else {
                out.push_str(segment);
            }
        }
    }
    out
}

/// Rewrite the links and images of one line of markdown (no code in it).
fn rewrite_links(line: &str, base: &url::Url) -> String {
    static LINK_RE: OnceLock<Regex> = OnceLock::new();
    // (!?) image marker, [text] (which may hold an image), (target "title")
    let link_re = LINK_RE.get_or_init(|| {
        Regex::new(
            r#"(!?)\[((?:!\[[^\]]*\]\([^)]*\)|[^\]])*)\]\(\s*([^)\s]*)((?:\s+"[^"]*")?)\s*\)"#,
        )
        .unwrap()
    });
    link_re
        .replace_all(line, |caps: &regex::Captures| {
            let is_image = !caps[1].is_empty();
            // The text of a link may be an image: rewrite it too.
            let text = if is_image {
                caps[2].to_string()
            } else {
                rewrite_links(&caps[2], base)
            };
            let target = &caps[3];
            let title = &caps[4];
            if target.is_empty() {
                return if is_image { String::new() } else { text };
            }
            // `//cdn/x.png` is relative to the scheme: `join` handles it.
            let is_absolute = target.starts_with('#') || url::Url::parse(target).is_ok();
            let resolved = if is_absolute {
                target.to_string()
            } else {
                base.join(target)
                    .map(|u| u.to_string())
                    .unwrap_or_else(|_| target.to_string())
            };
            format!("{}[{}]({}{})", &caps[1], text, resolved, title)
        })
        .into_owned()
}

/// Strip script, style, noscript, svg, and head tags (and their content) from HTML.
fn strip_non_content_tags(html: &str) -> Cow<'_, str> {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        ["script", "style", "noscript", "svg", "head"]
            .iter()
            .map(|tag| Regex::new(&format!(r"(?si)<{tag}[\s>].*?</{tag}\s*>")).unwrap())
            .collect()
    });
    strip_matches(Cow::Borrowed(html), patterns)
}

/// Strip boilerplate HTML sections: nav, header, footer, aside.
fn strip_boilerplate_tags(html: Cow<'_, str>) -> Cow<'_, str> {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        ["nav", "footer", "header", "aside"]
            .iter()
            .map(|tag| Regex::new(&format!(r"(?si)<{tag}[\s>].*?</{tag}\s*>")).unwrap())
            .collect()
    });
    strip_matches(html, patterns)
}

/// Remove every match of each pattern, one pattern after the other (a
/// later pattern sees the text without the earlier ones' matches). The text
/// is only copied by a pattern that matches.
fn strip_matches<'a>(mut text: Cow<'a, str>, patterns: &[Regex]) -> Cow<'a, str> {
    for re in patterns {
        let stripped = match re.replace_all(&text, "") {
            Cow::Owned(stripped) => Some(stripped),
            Cow::Borrowed(_) => None,
        };
        if let Some(stripped) = stripped {
            text = Cow::Owned(stripped);
        }
    }
    text
}

/// Strip any remaining HTML tags from markdown output.
///
/// Converts `<img>` tags to `![alt](src)` before stripping, so image info is preserved.
/// All other HTML tags are removed, keeping only their text content.
fn strip_remaining_html(markdown: &str) -> String {
    // Convert <img> tags to markdown images: ![alt](src)
    static IMG_RE: OnceLock<Regex> = OnceLock::new();
    let img_re = IMG_RE.get_or_init(|| Regex::new(r#"(?i)<img\s[^>]*?>"#).unwrap());

    static ALT_RE: OnceLock<Regex> = OnceLock::new();
    let alt_re = ALT_RE.get_or_init(|| Regex::new(r#"(?i)alt\s*=\s*"([^"]*)""#).unwrap());

    static SRC_RE: OnceLock<Regex> = OnceLock::new();
    let src_re = SRC_RE.get_or_init(|| Regex::new(r#"(?i)src\s*=\s*"([^"]*)""#).unwrap());

    let result = img_re.replace_all(markdown, |caps: &regex::Captures| {
        let tag = &caps[0];
        let alt = alt_re
            .captures(tag)
            .map(|c| c[1].to_string())
            .unwrap_or_default();
        let src = src_re
            .captures(tag)
            .map(|c| c[1].to_string())
            .unwrap_or_default();
        if alt.is_empty() && src.is_empty() {
            String::new()
        } else {
            format!("![{alt}]({src})")
        }
    });

    // Strip all remaining HTML tags (opening, closing, self-closing)
    static TAG_RE: OnceLock<Regex> = OnceLock::new();
    let tag_re = TAG_RE.get_or_init(|| Regex::new(r"<[^>]+>").unwrap());

    tag_re.replace_all(&result, "").into_owned()
}

/// Clean up the markdown output
fn clean_markdown(markdown: &str) -> String {
    // First pass: clean up multiline link/image syntax
    // [\n\nText\n\n](url) → [Text](url)
    // ![\n\nAlt\n\n](url) → ![Alt](url)
    static MULTILINE_LINK_RE: OnceLock<Regex> = OnceLock::new();
    let multiline_link_re = MULTILINE_LINK_RE
        .get_or_init(|| Regex::new(r"(!?\[)\s*\n\s*(.*?)\s*\n\s*\]\(([^)]*)\)").unwrap());
    let markdown = multiline_link_re.replace_all(markdown, "$1$2]($3)");

    // Remove empty links: [](url)
    static EMPTY_LINK_RE: OnceLock<Regex> = OnceLock::new();
    let empty_link_re = EMPTY_LINK_RE.get_or_init(|| Regex::new(r"\[]\([^)]*\)").unwrap());
    let markdown = empty_link_re.replace_all(&markdown, "");

    // Remove images with relative/asset URLs (decorative)
    static DECORATIVE_IMG_RE: OnceLock<Regex> = OnceLock::new();
    let decorative_img_re =
        DECORATIVE_IMG_RE.get_or_init(|| Regex::new(r"!\[[^\]]*\]\(/_next/[^)]*\)").unwrap());
    let markdown = decorative_img_re.replace_all(&markdown, "");

    // Remove images with no alt text
    static NO_ALT_IMG_RE: OnceLock<Regex> = OnceLock::new();
    let no_alt_img_re = NO_ALT_IMG_RE.get_or_init(|| Regex::new(r"!\[\]\([^)]*\)").unwrap());
    let markdown = no_alt_img_re.replace_all(&markdown, "");

    // Second pass: line-by-line cleanup
    let mut result = String::new();
    let mut prev_blank = false;
    let mut in_code_block = false;

    for line in markdown.lines() {
        // Track code blocks
        if line.starts_with("```") {
            in_code_block = !in_code_block;
        }

        // In code blocks, preserve everything
        if in_code_block {
            result.push_str(line);
            result.push('\n');
            prev_blank = false;
            continue;
        }

        let trimmed = line.trim();

        // Skip multiple blank lines
        if trimmed.is_empty() {
            if !prev_blank {
                result.push('\n');
                prev_blank = true;
            }
            continue;
        }

        prev_blank = false;

        // Clean up the line
        let cleaned = clean_line(trimmed);
        if !cleaned.is_empty() {
            result.push_str(&cleaned);
            result.push('\n');
        }
    }

    // Trim trailing whitespace
    result.trim_end().to_string()
}

/// Clean a single line of markdown
fn clean_line(line: &str) -> String {
    // Collapse consecutive spaces in a single pass - O(n) with one allocation
    let mut result = String::with_capacity(line.len());
    let mut prev_space = false;
    for ch in line.chars() {
        if ch == ' ' {
            if !prev_space {
                result.push(' ');
            }
            prev_space = true;
        } else {
            result.push(ch);
            prev_space = false;
        }
    }

    // Fix heading spacing
    if result.starts_with('#') {
        let hash_end = result.find(|c: char| c != '#').unwrap_or(result.len());
        if hash_end < result.len() && !result[hash_end..].starts_with(' ') {
            result.insert(hash_end, ' ');
        }
    }

    result
}

/// Extract plain text from markdown (strip all formatting)
pub fn markdown_to_text(markdown: &str) -> String {
    let mut result = String::new();
    let mut in_code_block = false;

    for line in markdown.lines() {
        // Track code blocks
        if line.starts_with("```") {
            in_code_block = !in_code_block;
            continue;
        }

        if in_code_block {
            result.push_str(line);
            result.push('\n');
            continue;
        }

        let mut cleaned = line.to_string();

        // Remove heading markers
        while cleaned.starts_with('#') {
            cleaned = cleaned.trim_start_matches('#').to_string();
        }

        // Remove emphasis markers
        cleaned = cleaned
            .replace("**", "")
            .replace("__", "")
            .replace(['*', '_'], "");

        // Remove link syntax but keep text
        // [text](url) -> text
        while let Some(start) = cleaned.find('[') {
            if let Some(end) = cleaned[start..].find("](") {
                let link_end = cleaned[start + end..].find(')');
                if let Some(link_end) = link_end {
                    let text = &cleaned[start + 1..start + end];
                    cleaned = format!(
                        "{}{}{}",
                        &cleaned[..start],
                        text,
                        &cleaned[start + end + link_end + 1..]
                    );
                    continue;
                }
            }
            break;
        }

        // Remove image syntax
        cleaned = cleaned.replace("![", "[").replace("](", " ");

        // Remove list markers
        if cleaned.starts_with("- ") || cleaned.starts_with("* ") {
            cleaned = cleaned[2..].to_string();
        }

        // Remove numbered list markers
        if let Some(pos) = cleaned.find(". ") {
            if cleaned[..pos].chars().all(|c| c.is_ascii_digit()) {
                cleaned = cleaned[pos + 2..].to_string();
            }
        }

        let trimmed = cleaned.trim();
        if !trimmed.is_empty() {
            if !result.is_empty() {
                result.push(' ');
            }
            result.push_str(trimmed);
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_html_to_markdown() {
        let html = "<h1>Title</h1><p>Paragraph text</p>";
        let md = html_to_markdown(html);

        // html2md may format headings differently, check for title presence
        assert!(md.contains("Title"), "Expected 'Title' in markdown: {}", md);
        assert!(md.contains("Paragraph text"));
    }

    #[test]
    fn test_links() {
        let html = r#"<a href="https://example.com">Link text</a>"#;
        let md = html_to_markdown(html);

        assert!(md.contains("[Link text]"));
        assert!(md.contains("https://example.com"));
    }

    #[test]
    fn test_lists() {
        let html = "<ul><li>Item 1</li><li>Item 2</li></ul>";
        let md = html_to_markdown(html);

        assert!(md.contains("Item 1"));
        assert!(md.contains("Item 2"));
    }

    #[test]
    fn test_code_blocks() {
        let html = "<pre><code>fn main() {}</code></pre>";
        let md = html_to_markdown(html);

        assert!(md.contains("fn main()"));
    }

    #[test]
    fn test_strips_nav_footer_in_main_content_mode() {
        let html = r#"
            <nav><a href="/">Home</a><a href="/about">About</a></nav>
            <main><h1>Article</h1><p>Content here</p></main>
            <footer><p>Copyright 2026</p></footer>
        "#;
        let md = html_to_main_content_markdown(html);

        assert!(md.contains("Article"));
        assert!(md.contains("Content here"));
        assert!(!md.contains("Home"));
        assert!(!md.contains("Copyright"));
    }

    #[test]
    fn test_strips_decorative_images() {
        let html = r#"
            <img src="/_next/static/media/bg.svg" alt="">
            <h1>Title</h1>
            <img src="https://example.com/photo.jpg" alt="A photo">
        "#;
        let md = html_to_main_content_markdown(html);

        assert!(md.contains("Title"));
        assert!(!md.contains("/_next/"));
        assert!(md.contains("![A photo](https://example.com/photo.jpg)"));
    }

    #[test]
    fn test_removes_empty_links() {
        let html = r#"
            <a href="https://github.com"></a>
            <a href="https://example.com">Real link</a>
        "#;
        let md = html_to_markdown(html);

        assert!(md.contains("[Real link]"));
        assert!(!md.contains("[]("));
    }

    #[test]
    fn links_resolve_against_the_page_url() {
        let html = r##"<h1><a href="/">Quotes</a></h1>
            <p><a href="/login">Login</a> <a href="intro">Intro</a>
            <a href="https://other.example/x">Other</a> <a href="#top">Top</a>
            <img src="img/a.png" alt="A"></p>"##;
        let md = html_to_markdown_for_page(html, "https://quotes.example/docs/page/", false);
        assert!(md.contains("[Quotes](https://quotes.example/)"), "{md}");
        assert!(md.contains("[Login](https://quotes.example/login)"), "{md}");
        assert!(
            md.contains("[Intro](https://quotes.example/docs/page/intro)"),
            "{md}"
        );
        assert!(md.contains("[Other](https://other.example/x)"), "{md}");
        assert!(md.contains("[Top](#top)"), "{md}");
        assert!(
            md.contains("![A](https://quotes.example/docs/page/img/a.png)"),
            "{md}"
        );
    }

    #[test]
    fn links_honour_base_href() {
        let html = r#"<html><head><base href="/app/"></head>
            <body><p><a href="settings">Settings</a></p></body></html>"#;
        let md = html_to_markdown_for_page(html, "https://x.example/app/users/1", false);
        assert!(
            md.contains("[Settings](https://x.example/app/settings)"),
            "{md}"
        );
    }

    #[test]
    fn anchors_without_href_become_plain_text() {
        // quotes.toscrape.com/js renders its tags as `<a class='tag'>t</a>`.
        let html = r#"<div>Tags: <a class="tag">change</a> <a class="tag">world</a></div>"#;
        let md = html_to_markdown_for_page(html, "https://quotes.example/js/", false);
        assert!(!md.contains("]()"), "{md}");
        assert!(md.contains("change") && md.contains("world"), "{md}");
    }

    #[test]
    fn code_blocks_keep_their_link_syntax() {
        let base = url::Url::parse("https://x.example/").unwrap();
        let md = "```\n[a](/b)\n```\n[c](/d)\n~~~\n[e](/f)\n~~~\nsee `[g](/h)` and [i](/j)";
        let out = absolutize_markdown_links(md, &base);
        assert_eq!(
            out,
            "```\n[a](/b)\n```\n[c](https://x.example/d)\n~~~\n[e](/f)\n~~~\n\
             see `[g](/h)` and [i](https://x.example/j)"
        );
    }

    #[test]
    fn a_link_wrapping_an_image_has_both_rewritten() {
        let base = url::Url::parse("https://x.example/docs/").unwrap();
        let out = absolutize_markdown_links("[![Logo](logo.png)](/home)", &base);
        assert_eq!(
            out,
            "[![Logo](https://x.example/docs/logo.png)](https://x.example/home)"
        );
    }

    #[test]
    fn base_url_lookup_is_safe_on_uppercase_tags_and_multibyte_text() {
        // No lowercase </head> or <body>, > 16 KiB, a multi-byte char
        // straddling byte 16384: this used to panic on the slice.
        let mut html = String::from("<HTML><HEAD><TITLE>t</TITLE></HEAD><BODY>");
        while html.len() < 16 * 1024 - 1 {
            html.push('a');
        }
        html.push_str("日本語のテキスト</BODY></HTML>");
        let base = document_base_url(&html, "https://x.example/a/b").unwrap();
        assert_eq!(base.as_str(), "https://x.example/a/b");

        let upper = r#"<HTML><HEAD><BASE HREF="/app/"></HEAD><BODY><a href="s">S</a></BODY>"#;
        let base = document_base_url(upper, "https://x.example/app/users/1").unwrap();
        assert_eq!(base.as_str(), "https://x.example/app/");
    }

    #[test]
    fn non_web_base_href_is_ignored() {
        let html = r#"<head><base href="javascript:alert(1)//"></head>"#;
        let base = document_base_url(html, "https://x.example/p/q").unwrap();
        assert_eq!(base.as_str(), "https://x.example/p/q");
    }

    #[test]
    fn test_markdown_to_text() {
        let md = "# Title\n\nSome **bold** and *italic* text.\n\n[Link](http://example.com)";
        let text = markdown_to_text(md);

        assert!(text.contains("Title"));
        assert!(text.contains("bold"));
        assert!(text.contains("italic"));
        assert!(text.contains("Link"));
        assert!(!text.contains("**"));
        assert!(!text.contains("http://"));
    }

    #[test]
    fn test_clean_multiple_blanks() {
        let md = "Line 1\n\n\n\n\nLine 2";
        let cleaned = clean_markdown(md);

        // Should have at most one blank line between content
        assert!(!cleaned.contains("\n\n\n"));
    }

    /// The previous implementation: every pattern copies the whole text.
    fn strip_copying(html: &str, tags: &[&str]) -> String {
        let mut result = html.to_string();
        for tag in tags {
            let re = Regex::new(&format!(r"(?si)<{tag}[\s>].*?</{tag}\s*>")).unwrap();
            result = re.replace_all(&result, "").into_owned();
        }
        result
    }

    #[test]
    fn stripping_tags_matches_the_copying_implementation() {
        let non_content = ["script", "style", "noscript", "svg", "head"];
        let boilerplate = ["nav", "footer", "header", "aside"];
        let pages = [
            "<p>plain paragraph, nothing to strip</p>",
            "<html><head><title>t</title><style>a{}</style></head><body><p>x</p></body></html>",
            "<body><SCRIPT type='a'>var s = '</style>';</SCRIPT><p>kept</p><style>p{}</style></body>",
            "<style>a{} <script>b</style> c</script><p>interleaved</p>",
            "<nav>menu</nav><header><h1>T</h1></header><main>body</main><aside>x</aside><footer>f</footer>",
            "<nav><svg><path d=''/></svg></nav><p>text</p><noscript><img src='p.gif'></noscript>",
            "<script>unclosed <p>text</p>",
            "<navigation>not a nav</navigation><headers>nor a header</headers>",
            "",
        ];
        for page in pages {
            let stripped = strip_non_content_tags(page);
            assert_eq!(stripped, strip_copying(page, &non_content), "{page}");
            let main_only = strip_boilerplate_tags(stripped.clone());
            assert_eq!(
                main_only,
                strip_copying(&strip_copying(page, &non_content), &boilerplate),
                "{page}"
            );
        }
    }

    #[test]
    fn stripping_does_not_copy_a_page_with_nothing_to_strip() {
        let page = "<body><main><p>plain paragraph</p></main></body>";
        let stripped = strip_boilerplate_tags(strip_non_content_tags(page));
        assert!(matches!(stripped, Cow::Borrowed(_)));
        assert!(matches!(
            strip_non_content_tags("<p>a</p><script>b</script>"),
            Cow::Owned(_)
        ));
    }
}
