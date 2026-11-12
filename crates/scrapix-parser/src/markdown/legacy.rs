//! The Markdown conversion as it was before SCR-90, kept verbatim as the
//! reference of the differential tests (`super::differential`): the
//! optimized pipeline in `markdown.rs` must give byte-identical output.
//! Do not "improve" this file.

#![allow(dead_code)]

use super::MarkdownConfig;
use html2md::parse_html;
use regex::Regex;
use std::borrow::Cow;
use std::sync::OnceLock;

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
pub(super) fn rewrite_links(line: &str, base: &url::Url) -> String {
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
pub(super) fn strip_non_content_tags(html: &str) -> Cow<'_, str> {
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
pub(super) fn strip_boilerplate_tags(html: Cow<'_, str>) -> Cow<'_, str> {
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
pub(super) fn strip_matches<'a>(mut text: Cow<'a, str>, patterns: &[Regex]) -> Cow<'a, str> {
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
pub(super) fn strip_remaining_html(markdown: &str) -> String {
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
pub(super) fn clean_markdown(markdown: &str) -> String {
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
pub(super) fn clean_line(line: &str) -> String {
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
