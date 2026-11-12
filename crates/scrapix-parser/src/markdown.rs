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
    // Strip non-content tags (script, style, etc.) and, optionally, the
    // boilerplate sections
    let stripped_tags = if config.only_main_content {
        STRIPPED_TAGS.len()
    } else {
        NON_CONTENT_TAGS
    };
    let cleaned_html = strip_tags(Cow::Borrowed(html), 0..stripped_tags);

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
        // A link or an image has a `](`: a line without one is kept as it is.
        if fence.is_some() || !line.contains("](") {
            out.push_str(line);
            continue;
        }
        // Rewrite outside inline code spans only: odd segments of a
        // backtick split are code.
        for (j, segment) in line.split('`').enumerate() {
            if j > 0 {
                out.push('`');
            }
            if j % 2 == 0 && segment.contains("](") {
                push_rewritten_links(&mut out, segment, base);
            } else {
                out.push_str(segment);
            }
        }
    }
    out
}

/// The pattern of a link or an image: (!?) image marker, [text] (which may
/// hold an image), (target "title").
fn link_regex() -> &'static Regex {
    static LINK_RE: OnceLock<Regex> = OnceLock::new();
    LINK_RE.get_or_init(|| {
        Regex::new(
            r#"(!?)\[((?:!\[[^\]]*\]\([^)]*\)|[^\]])*)\]\(\s*([^)\s]*)((?:\s+"[^"]*")?)\s*\)"#,
        )
        .unwrap()
    })
}

/// Append one line of markdown (no code in it) to `out`, its links and
/// images rewritten.
///
/// What is a link is decided by [`link_regex`], and the capture search of
/// the regex engine was a quarter of the conversion of a page with hundreds
/// of links. On a plain line the same matches are found by hand
/// ([`push_rewritten_plain_links`]); every other line goes through the
/// regex.
///
/// A line is plain when it has no `"` (no link title) and either no `![`
/// (no image) or a single `[`. Then a link text cannot hold an image or a
/// `]`, and a target cannot hold a quoted `)`: each `[` has at most one way
/// to be a link, ending at the first `)` after its `](`, so there is one
/// set of matches whatever the order they are searched in. That matters:
/// on the other lines the regex engine (regex 1.12) does not always return
/// the leftmost match (in `[![](a [b](c)d](e)` it finds `[b](c)`), and the
/// Markdown has to stay what it was.
fn push_rewritten_links(out: &mut String, line: &str, base: &url::Url) {
    let plain = !line.contains('"')
        && (!line.contains("![") || line.bytes().filter(|&b| b == b'[').count() == 1);
    if plain {
        push_rewritten_plain_links(out, line, base);
    } else {
        out.push_str(&rewrite_links_with_regex(line, base));
    }
}

/// [`push_rewritten_links`] for a plain line: `!?[text](target)`, where
/// the text runs to the first `]` and the target, with whitespace allowed
/// around it, to the `)`.
fn push_rewritten_plain_links(out: &mut String, line: &str, base: &url::Url) {
    let bytes = line.as_bytes();
    let find = |byte: u8, from: usize| {
        bytes[from..]
            .iter()
            .position(|&b| b == byte)
            .map(|offset| from + offset)
    };
    // Up to here the line is in `out`.
    let mut copied = 0;
    let mut at = 0;
    // Where a target that was not followed by `)` ended: a later target
    // reaching it ends the same way.
    let mut dead_target_end = 0;
    while let Some(open) = find(b'[', at) {
        // The text of a link at `open`, and of every `[` inside it, ends
        // at the first `]`: they all are links or none is.
        let Some(close) = find(b']', open + 1) else {
            break;
        };
        at = close + 1;
        if bytes.get(close + 1) != Some(&b'(') {
            continue;
        }
        let target_start = skip_whitespace(line, close + 2);
        if target_start < dead_target_end {
            continue;
        }
        let rest = &line[target_start..];
        let target_end = target_start
            + rest
                .find(|c: char| c == ')' || c.is_whitespace())
                .unwrap_or(rest.len());
        let closing = skip_whitespace(line, target_end);
        if bytes.get(closing) != Some(&b')') {
            dead_target_end = target_end;
            continue;
        }

        let is_image = open > copied && bytes[open - 1] == b'!';
        let start = open - usize::from(is_image);
        let text = &line[open + 1..close];
        let target = &line[target_start..target_end];
        out.push_str(&line[copied..start]);
        if !target.is_empty() {
            out.push_str(&line[start..=open]);
            out.push_str(text);
            out.push_str("](");
            push_resolved_target(out, target, base);
            out.push(')');
        } else if !is_image {
            // A link with no target is replaced by its text, an image
            // with none is removed.
            out.push_str(text);
        }
        copied = closing + 1;
        at = copied;
    }
    out.push_str(&line[copied..]);
}

/// The offset after the whitespace at `at`.
fn skip_whitespace(line: &str, at: usize) -> usize {
    line.len() - line[at..].trim_start().len()
}

/// Append `target` to `out`, resolved against `base` unless it is absolute
/// or a fragment.
fn push_resolved_target(out: &mut String, target: &str, base: &url::Url) {
    // An absolute URL has a scheme, so a `:`: without one the target is
    // relative and parsing it alone can only fail.
    let is_absolute =
        target.starts_with('#') || (target.contains(':') && url::Url::parse(target).is_ok());
    if is_absolute {
        out.push_str(target);
        return;
    }
    // `//cdn/x.png` is relative to the scheme: `join` handles it.
    match base.join(target) {
        Ok(resolved) => out.push_str(resolved.as_str()),
        Err(_) => out.push_str(target),
    }
}

/// Rewrite the links and images of one line of markdown (no code in it)
/// with [`link_regex`].
fn rewrite_links_with_regex(line: &str, base: &url::Url) -> String {
    link_regex()
        .replace_all(line, |caps: &regex::Captures| {
            let is_image = !caps[1].is_empty();
            // The text of a link may be an image: rewrite it too.
            let text = if is_image || !caps[2].contains("](") {
                caps[2].to_string()
            } else {
                rewrite_links_with_regex(&caps[2], base)
            };
            let target = &caps[3];
            let title = &caps[4];
            if target.is_empty() {
                return if is_image { String::new() } else { text };
            }
            let mut resolved = String::new();
            push_resolved_target(&mut resolved, target, base);
            format!("{}[{}]({}{})", &caps[1], text, resolved, title)
        })
        .into_owned()
}

/// Tags removed with their content, in the order they are removed: the
/// first [`NON_CONTENT_TAGS`] from every page, the rest (nav, footer,
/// header, aside) when only the main content is wanted.
const STRIPPED_TAGS: [&str; 9] = [
    "script", "style", "noscript", "svg", "head", "nav", "footer", "header", "aside",
];
/// How many of [`STRIPPED_TAGS`] are removed from every page.
const NON_CONTENT_TAGS: usize = 5;

/// Strip script, style, noscript, svg, and head tags (and their content) from HTML.
#[cfg(test)]
fn strip_non_content_tags(html: &str) -> Cow<'_, str> {
    strip_tags(Cow::Borrowed(html), 0..NON_CONTENT_TAGS)
}

/// Strip boilerplate HTML sections: nav, header, footer, aside.
#[cfg(test)]
fn strip_boilerplate_tags(html: Cow<'_, str>) -> Cow<'_, str> {
    strip_tags(html, NON_CONTENT_TAGS..STRIPPED_TAGS.len())
}

/// The regex a tag's blocks are defined by.
fn tag_block_regex(tag: usize) -> &'static Regex {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    &PATTERNS.get_or_init(|| {
        STRIPPED_TAGS
            .iter()
            .map(|tag| Regex::new(&format!(r"(?si)<{tag}[\s>].*?</{tag}\s*>")).unwrap())
            .collect()
    })[tag]
}

/// Remove every `<tag …>…</tag>` block of the tags `STRIPPED_TAGS[tags]`,
/// one tag after the other: a later tag sees the text without the earlier
/// ones' blocks, which is what makes `<style>a <script>b</style> c</script>`
/// come out as it does. The text is only copied by a tag that has a block.
///
/// A block is what the regex `(?si)<tag[\s>].*?</tag\s*>` matches. Running
/// nine such regexes over the page was a fifth of the conversion, so the
/// page is scanned once for the places where one of the nine names follows
/// a `<` or a `</`, and each tag then walks that short list instead of the
/// page. The one thing the scanner does not reproduce is Unicode case
/// folding, where `ſ` (U+017F) equals `s`: a page holding that character
/// goes through the regexes.
fn strip_tags(mut text: Cow<'_, str>, tags: std::ops::Range<usize>) -> Cow<'_, str> {
    if text.contains('\u{17f}') {
        for tag in tags {
            let stripped = match tag_block_regex(tag).replace_all(&text, "") {
                Cow::Owned(stripped) => Some(stripped),
                Cow::Borrowed(_) => None,
            };
            if let Some(stripped) = stripped {
                text = Cow::Owned(stripped);
            }
        }
        return text;
    }

    let mut marks = tag_marks(text.as_bytes());
    let mut blocks = Vec::new();
    for tag in tags {
        if marks.is_empty() {
            break;
        }
        blocks.clear();
        find_tag_blocks(&text, &marks, tag, &mut blocks);
        if !blocks.is_empty() {
            let stripped = remove_blocks(&text, &blocks);
            remap_marks(&mut marks, &blocks, stripped.as_bytes());
            debug_assert_eq!(marks, tag_marks(stripped.as_bytes()));
            text = Cow::Owned(stripped);
        }
    }
    text
}

/// A `<` or `</` followed by the name of a stripped tag, in any case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TagMark {
    /// Byte offset of the `<`.
    lt: usize,
    /// `</name` rather than `<name`.
    closing: bool,
    /// Bit `i` is set when the name starts with `STRIPPED_TAGS[i]`
    /// (`<header` has both `head` and `header`).
    tags: u16,
}

/// The longest thing a [`TagMark`] is read from, minus one: `</noscript`.
/// A `<` further than this before a point cannot see past it.
const TAG_MARK_REACH: usize = 9;

/// The mark of the `<` at `lt`, if a stripped tag's name follows it.
fn tag_mark(bytes: &[u8], lt: usize) -> Option<TagMark> {
    let closing = bytes.get(lt + 1) == Some(&b'/');
    let name = lt + 1 + usize::from(closing);
    // Every stripped tag starts with one of these letters.
    if !matches!(
        bytes.get(name).map(u8::to_ascii_lowercase),
        Some(b's' | b'n' | b'h' | b'f' | b'a')
    ) {
        return None;
    }
    let mut tags = 0u16;
    for (i, tag) in STRIPPED_TAGS.iter().enumerate() {
        let named = bytes
            .get(name..name + tag.len())
            .is_some_and(|found| found.eq_ignore_ascii_case(tag.as_bytes()));
        if named {
            tags |= 1 << i;
        }
    }
    (tags != 0).then_some(TagMark { lt, closing, tags })
}

/// The marks of a text, in order.
fn tag_marks(bytes: &[u8]) -> Vec<TagMark> {
    let mut marks = Vec::new();
    for_each_byte(bytes, b'<', |lt| marks.extend(tag_mark(bytes, lt)));
    marks
}

/// Call `found` with the offset of every `byte` of `bytes`, in order.
/// Eight bytes at a time: a page has a `<` every few dozen bytes, where a
/// search restarted after each one spends its time starting.
fn for_each_byte(bytes: &[u8], byte: u8, mut found: impl FnMut(usize)) {
    const LOW_BITS: u64 = 0x7f7f_7f7f_7f7f_7f7f;
    let pattern = u64::from_ne_bytes([byte; 8]);
    let (chunks, remainder) = bytes.as_chunks::<8>();
    let mut offset = 0;
    for chunk in chunks {
        // Little-endian, so the lowest byte of the word is the first one.
        let word = u64::from_le_bytes(*chunk) ^ pattern;
        // The high bit of each byte of `word` that is zero, exactly.
        let mut zeros = !(((word & LOW_BITS) + LOW_BITS) | word | LOW_BITS);
        while zeros != 0 {
            found(offset + (zeros.trailing_zeros() / 8) as usize);
            zeros &= zeros - 1;
        }
        offset += 8;
    }
    for (i, &b) in remainder.iter().enumerate() {
        if b == byte {
            found(offset + i);
        }
    }
}

/// The blocks of `STRIPPED_TAGS[tag]` in `text`, as the regex
/// `(?si)<tag[\s>].*?</tag\s*>` finds them one after the other: the
/// leftmost `<tag` followed by whitespace or `>`, up to the first `</tag`,
/// optional whitespace, `>` after it, then again from there. With no
/// closing tag after an opening one there is none after a later one
/// either, so the search is over.
fn find_tag_blocks(text: &str, marks: &[TagMark], tag: usize, blocks: &mut Vec<(usize, usize)>) {
    let name_len = STRIPPED_TAGS[tag].len();
    let bit = 1u16 << tag;
    let mut marks = marks.iter().filter(|mark| mark.tags & bit != 0);
    let mut from = 0;
    loop {
        let Some(start) = marks.by_ref().find_map(|mark| {
            let opens = !mark.closing
                && mark.lt >= from
                && text[mark.lt + 1 + name_len..]
                    .starts_with(|c: char| c == '>' || c.is_whitespace());
            opens.then_some(mark.lt)
        }) else {
            return;
        };
        // No `<` sits between an opening tag's `<` and the end of its
        // name and delimiter, so every later mark is after them.
        let Some(end) = marks.by_ref().find_map(|mark| {
            if !mark.closing {
                return None;
            }
            let after_name = text[mark.lt + 2 + name_len..].trim_start();
            after_name
                .starts_with('>')
                .then(|| text.len() - after_name.len() + 1)
        }) else {
            return;
        };
        blocks.push((start, end));
        from = end;
    }
}

/// `text` without `blocks` (sorted, not overlapping, not empty).
fn remove_blocks(text: &str, blocks: &[(usize, usize)]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut kept_from = 0;
    for &(start, end) in blocks {
        out.push_str(&text[kept_from..start]);
        kept_from = end;
    }
    out.push_str(&text[kept_from..]);
    out
}

/// Bring `marks` from a text to that text without `blocks`, whose bytes are
/// `stripped`: the marks inside a block go, the ones after it move, and the
/// `<` just before the place a block was are read again, because the text
/// that now follows them can complete a name (`<na<script></script>v>`).
fn remap_marks(marks: &mut Vec<TagMark>, blocks: &[(usize, usize)], stripped: &[u8]) {
    let mut remapped = Vec::with_capacity(marks.len());
    let mut old = marks.iter().peekable();
    let mut removed = 0;
    // Where the previous block was, in `stripped`.
    let mut reread_from = 0;
    for &(start, end) in blocks {
        let seam = start - removed;
        let window = seam.saturating_sub(TAG_MARK_REACH).max(reread_from);
        // Marks before the window are untouched by this block.
        while let Some(mark) = old.next_if(|mark| mark.lt < start) {
            let lt = mark.lt - removed;
            if lt < window {
                remapped.push(TagMark { lt, ..*mark });
            }
        }
        for lt in window..seam {
            if stripped[lt] == b'<' {
                remapped.extend(tag_mark(stripped, lt));
            }
        }
        while old.next_if(|mark| mark.lt < end).is_some() {}
        removed += end - start;
        reread_from = seam;
    }
    remapped.extend(old.map(|mark| TagMark {
        lt: mark.lt - removed,
        ..*mark
    }));
    *marks = remapped;
}

/// Strip any remaining HTML tags from markdown output.
///
/// Converts `<img>` tags to `![alt](src)` before stripping, so image info is preserved.
/// All other HTML tags are removed, keeping only their text content.
fn strip_remaining_html(markdown: &str) -> Cow<'_, str> {
    // Both patterns need a `<` and a `>`: most Markdown has neither.
    if !markdown.contains('<') || !markdown.contains('>') {
        return Cow::Borrowed(markdown);
    }

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

    let stripped = match tag_re.replace_all(&result, "") {
        Cow::Owned(stripped) => Some(stripped),
        Cow::Borrowed(_) => None,
    };
    match stripped {
        Some(stripped) => Cow::Owned(stripped),
        None => result,
    }
}

/// Whether some `[` is directly followed by whitespace, which a link text
/// spread over several lines starts with.
fn has_bracket_then_whitespace(markdown: &str) -> bool {
    let mut pos = 0;
    while let Some(offset) = markdown[pos..].find('[') {
        pos += offset + 1;
        if markdown[pos..].starts_with(char::is_whitespace) {
            return true;
        }
    }
    false
}

/// Clean up the markdown output
fn clean_markdown(markdown: &str) -> String {
    let mut markdown = Cow::Borrowed(markdown);

    // First pass: fix up links and images. Each of the four patterns needs
    // a `](`, and each is skipped when the text it would see cannot match.
    if markdown.contains("](") {
        // Clean up multiline link/image syntax
        // [\n\nText\n\n](url) → [Text](url)
        // ![\n\nAlt\n\n](url) → ![Alt](url)
        static MULTILINE_LINK_RE: OnceLock<Regex> = OnceLock::new();
        if has_bracket_then_whitespace(&markdown) {
            let multiline_link_re = MULTILINE_LINK_RE
                .get_or_init(|| Regex::new(r"(!?\[)\s*\n\s*(.*?)\s*\n\s*\]\(([^)]*)\)").unwrap());
            replace_in_place(&mut markdown, multiline_link_re, "$1$2]($3)");
        }

        // Remove empty links: [](url)
        static EMPTY_LINK_RE: OnceLock<Regex> = OnceLock::new();
        if markdown.contains("[](") {
            let empty_link_re = EMPTY_LINK_RE.get_or_init(|| Regex::new(r"\[]\([^)]*\)").unwrap());
            replace_in_place(&mut markdown, empty_link_re, "");
        }

        // Remove images with relative/asset URLs (decorative)
        static DECORATIVE_IMG_RE: OnceLock<Regex> = OnceLock::new();
        if markdown.contains("](/_next/") {
            let decorative_img_re = DECORATIVE_IMG_RE
                .get_or_init(|| Regex::new(r"!\[[^\]]*\]\(/_next/[^)]*\)").unwrap());
            replace_in_place(&mut markdown, decorative_img_re, "");
        }

        // Remove images with no alt text
        static NO_ALT_IMG_RE: OnceLock<Regex> = OnceLock::new();
        if markdown.contains("![](") {
            let no_alt_img_re =
                NO_ALT_IMG_RE.get_or_init(|| Regex::new(r"!\[\]\([^)]*\)").unwrap());
            replace_in_place(&mut markdown, no_alt_img_re, "");
        }
    }

    // Second pass: line-by-line cleanup
    let mut result = String::with_capacity(markdown.len());
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
        push_clean_line(&mut result, trimmed);
        result.push('\n');
    }

    // Trim trailing whitespace
    let trimmed_len = result.trim_end().len();
    result.truncate(trimmed_len);
    result
}

/// Replace every match of `re` in `text`, copying it only when one matches.
fn replace_in_place(text: &mut Cow<'_, str>, re: &Regex, replacement: &str) {
    let replaced = match re.replace_all(text, replacement) {
        Cow::Owned(replaced) => Some(replaced),
        Cow::Borrowed(_) => None,
    };
    if let Some(replaced) = replaced {
        *text = Cow::Owned(replaced);
    }
}

/// Append one cleaned line of markdown to `out`: runs of spaces collapsed
/// to one, and a space after the `#`s of a heading that lacks it. `line` is
/// trimmed and not empty.
fn push_clean_line(out: &mut String, line: &str) {
    // Fix heading spacing
    let hashes = line.len() - line.trim_start_matches('#').len();
    let mut rest = line;
    if hashes > 0 && hashes < line.len() && line.as_bytes()[hashes] != b' ' {
        out.push_str(&line[..hashes]);
        out.push(' ');
        rest = &line[hashes..];
    }

    // Collapse consecutive spaces
    while let Some(i) = rest.find("  ") {
        out.push_str(&rest[..=i]);
        rest = rest[i + 1..].trim_start_matches(' ');
    }
    out.push_str(rest);
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
mod differential;
#[cfg(test)]
mod legacy;

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
