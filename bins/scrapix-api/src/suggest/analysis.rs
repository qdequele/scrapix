//! What `POST /crawl/suggest` reads from a site, with no I/O: the map's URLs
//! grouped into sections, what each sampled page shows (schema.org types,
//! length, platform, signs of a JavaScript-only page, linked documents),
//! and the kind of each section and of the whole site.
//!
//! Everything here is deterministic: the AI pass (`super::ai`) only
//! confirms or corrects the kinds this module proposes.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// At most this many sections are reported; the smallest past it merge into
/// [`OTHER_KEY`].
pub(crate) const MAX_SECTIONS: usize = 15;
/// The section of pages right under the site's base path.
pub(crate) const HOME_KEY: &str = "/";
/// The merged section of the smallest sections.
pub(crate) const OTHER_KEY: &str = "*";
/// Page titles kept per section (for the AI pass and the response).
const TITLES_PER_SECTION: usize = 5;
/// URLs kept per section (the sample candidates).
const URLS_PER_SECTION: usize = 20;
/// How much of a page's raw HTML is read for platform fingerprints.
const FINGERPRINT_BYTES: usize = 300_000;

// ============================================================================
// Kinds
// ============================================================================

/// What a whole site is.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SiteType {
    /// An online shop: product pages and category listings.
    Ecommerce,
    /// A blog, news site or magazine: dated articles.
    Blog,
    /// Product or API documentation.
    Docs,
    /// A company or product website: landing, feature and pricing pages.
    Marketing,
    /// A help center, knowledge base or forum.
    Support,
    /// Several of the above with no dominant one (e.g. a marketing site
    /// with a blog, docs and a shop).
    Mixed,
}

impl SiteType {
    pub(crate) fn parse(s: &str) -> Option<Self> {
        serde_json::from_value(Value::String(s.trim().to_ascii_lowercase())).ok()
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            SiteType::Ecommerce => "ecommerce",
            SiteType::Blog => "blog",
            SiteType::Docs => "docs",
            SiteType::Marketing => "marketing",
            SiteType::Support => "support",
            SiteType::Mixed => "mixed",
        }
    }
}

/// What the pages of one section are.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SectionKind {
    /// Product detail pages.
    Product,
    /// Category, collection or brand pages listing products.
    Listing,
    /// Blog posts, news, press releases.
    Article,
    /// Documentation, guides, API reference.
    Docs,
    /// Help center articles, FAQs, forum threads.
    Support,
    /// Landing, feature, pricing and company pages.
    Marketing,
    /// Tag, author and pagination pages: copies of other pages' content.
    Archive,
    /// Privacy, terms, cookies, imprint.
    Legal,
    /// Login, sign-up, cart, checkout, account, search results.
    Account,
}

impl SectionKind {
    pub(crate) fn parse(s: &str) -> Option<Self> {
        serde_json::from_value(Value::String(s.trim().to_ascii_lowercase())).ok()
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            SectionKind::Product => "product",
            SectionKind::Listing => "listing",
            SectionKind::Article => "article",
            SectionKind::Docs => "docs",
            SectionKind::Support => "support",
            SectionKind::Marketing => "marketing",
            SectionKind::Archive => "archive",
            SectionKind::Legal => "legal",
            SectionKind::Account => "account",
        }
    }

    /// Whether pages of this kind are left out of the crawl.
    pub(crate) fn is_excluded(self) -> bool {
        matches!(
            self,
            SectionKind::Archive | SectionKind::Legal | SectionKind::Account
        )
    }

    /// Long-form text worth splitting into blocks and summarizing.
    pub(crate) fn is_prose(self) -> bool {
        matches!(
            self,
            SectionKind::Article
                | SectionKind::Docs
                | SectionKind::Support
                | SectionKind::Marketing
        )
    }
}

/// The site builder or framework a page was made with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SitePlatform {
    Shopify,
    Woocommerce,
    Magento,
    Prestashop,
    Bigcommerce,
    SalesforceCommerce,
    Docusaurus,
    Mintlify,
    Gitbook,
    Readthedocs,
    Mkdocs,
    Vitepress,
    Nextra,
    Ghost,
    Zendesk,
    Discourse,
    Webflow,
    Framer,
    Wix,
    Squarespace,
    Wordpress,
}

/// SitePlatform fingerprints, most specific first (WooCommerce before
/// WordPress, which it runs on). Matched against lowercased raw HTML.
const PLATFORMS: &[(SitePlatform, &[&str])] = &[
    (
        SitePlatform::Shopify,
        &["cdn.shopify.com", "shopify.theme", "myshopify.com"],
    ),
    (SitePlatform::Woocommerce, &["woocommerce"]),
    (SitePlatform::Magento, &["data-mage-init", "mage/cookies"]),
    (SitePlatform::Prestashop, &["prestashop"]),
    (SitePlatform::Bigcommerce, &["bigcommerce.com"]),
    (SitePlatform::SalesforceCommerce, &["demandware"]),
    (SitePlatform::Docusaurus, &["docusaurus"]),
    (SitePlatform::Mintlify, &["mintlify"]),
    (SitePlatform::Gitbook, &["gitbook"]),
    (
        SitePlatform::Readthedocs,
        &["readthedocs", "sphinx-rtd", "_sphinx"],
    ),
    (SitePlatform::Mkdocs, &["mkdocs"]),
    (SitePlatform::Vitepress, &["vitepress"]),
    (SitePlatform::Nextra, &["nextra"]),
    (SitePlatform::Ghost, &["content=\"ghost"]),
    (SitePlatform::Zendesk, &["zdassets.com", "zendesk"]),
    (
        SitePlatform::Discourse,
        &["discourse-cdn", "data-discourse"],
    ),
    (SitePlatform::Webflow, &["webflow"]),
    (SitePlatform::Framer, &["framerusercontent.com"]),
    (SitePlatform::Wix, &["wixstatic.com", "x-wix-"]),
    (SitePlatform::Squarespace, &["squarespace"]),
    (SitePlatform::Wordpress, &["wp-content/", "wp-includes/"]),
];

impl SitePlatform {
    /// The site type a platform implies on its own, if any.
    pub(crate) fn site_type(self) -> Option<SiteType> {
        use SitePlatform::*;
        match self {
            Shopify | Woocommerce | Magento | Prestashop | Bigcommerce | SalesforceCommerce => {
                Some(SiteType::Ecommerce)
            }
            Docusaurus | Mintlify | Gitbook | Readthedocs | Mkdocs | Vitepress | Nextra => {
                Some(SiteType::Docs)
            }
            Ghost => Some(SiteType::Blog),
            Zendesk | Discourse => Some(SiteType::Support),
            Webflow | Framer | Wix | Squarespace => Some(SiteType::Marketing),
            Wordpress => None,
        }
    }
}

/// The platform `raw_html` was built with, if a fingerprint matches.
pub(crate) fn detect_platform(raw_html: &str) -> Option<SitePlatform> {
    let mut end = raw_html.len().min(FINGERPRINT_BYTES);
    while !raw_html.is_char_boundary(end) {
        end -= 1;
    }
    let html = raw_html[..end].to_ascii_lowercase();
    PLATFORMS
        .iter()
        .find(|(_, marks)| marks.iter().any(|m| html.contains(m)))
        .map(|(platform, _)| *platform)
}

// ============================================================================
// Sections
// ============================================================================

/// A group of the site's pages: those under one first path segment (after
/// the base path and an optional language segment).
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub(crate) struct Section {
    /// `/blog/` for pages under `/blog`, [`HOME_KEY`] for top-level pages,
    /// [`OTHER_KEY`] for the merged smallest sections.
    pub key: String,
    /// Pages the map found in it.
    pub pages: usize,
    /// A few page titles from the map.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub titles: Vec<String>,
    /// Some of its URLs (the sample candidates).
    #[serde(skip)]
    #[schema(ignore)]
    pub urls: Vec<String>,
}

/// A mapped page: its URL and the title `/map` read, if any.
#[derive(Debug, Clone)]
pub(crate) struct MappedPage {
    pub url: String,
    pub title: Option<String>,
}

/// ISO 639-1 codes of the languages websites are commonly translated into
/// (a two-letter segment that is not one, such as `eu` for Europe or `us`,
/// is not taken for a language).
const LANGUAGES: &[&str] = &[
    "af", "am", "ar", "az", "be", "bg", "bn", "bs", "ca", "cs", "cy", "da", "de", "el", "en", "es",
    "et", "fa", "fi", "fr", "ga", "gl", "gu", "he", "hi", "hr", "hu", "hy", "id", "is", "it", "ja",
    "ka", "kk", "km", "kn", "ko", "lo", "lt", "lv", "mk", "ml", "mn", "mr", "ms", "my", "nb", "ne",
    "nl", "nn", "no", "pa", "pl", "pt", "ro", "ru", "si", "sk", "sl", "sq", "sr", "sv", "sw", "ta",
    "te", "th", "tl", "tr", "uk", "ur", "uz", "vi", "zh", "zu",
];

/// Whether `segment` is a language code (`fr`, `en-us`, `pt_BR`).
pub(crate) fn is_language(segment: &str) -> bool {
    let lower = segment.to_ascii_lowercase();
    let (language, region) = match lower.split_once(['-', '_']) {
        Some((language, region)) => (language, Some(region)),
        None => (lower.as_str(), None),
    };
    LANGUAGES.contains(&language)
        && region.is_none_or(|r| {
            (r.len() == 2 && r.bytes().all(|b| b.is_ascii_alphabetic()))
                || matches!(r, "hans" | "hant" | "latn" | "cyrl")
        })
}

/// Whether a section key is a version of versioned docs (`/3.8.1/`,
/// `/v2/`, `/2.x/`, `/next/`).
pub(crate) fn is_version_key(key: &str) -> bool {
    let segment = key.trim_matches('/').to_ascii_lowercase();
    if matches!(
        segment.as_str(),
        "next" | "canary" | "nightly" | "unreleased"
    ) {
        return true;
    }
    let digits = segment.strip_prefix('v').unwrap_or(&segment);
    let digits = digits.strip_suffix(".x").unwrap_or(digits);
    !digits.is_empty()
        && digits.starts_with(|c: char| c.is_ascii_digit())
        && digits
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}

/// Whether the version sections are older copies to leave out: the site
/// has several versions and pages outside them (the current docs).
pub(crate) fn has_older_versions(sections: &[Section]) -> bool {
    let versions: Vec<&Section> = sections.iter().filter(|s| is_version_key(&s.key)).collect();
    let largest = versions.iter().map(|s| s.pages).max().unwrap_or(0);
    let others: usize = sections
        .iter()
        .filter(|s| !is_version_key(&s.key) && s.key != OTHER_KEY)
        .map(|s| s.pages)
        .sum();
    versions.len() >= 2 && others * 5 >= largest
}

/// The path segments of `url` under `base_path`, `None` when it is not
/// under it.
fn segments_under<'a>(path: &'a str, base_path: &str) -> Option<Vec<&'a str>> {
    let rest = path.strip_prefix(base_path.trim_end_matches('/'))?;
    if !rest.is_empty() && !rest.starts_with('/') {
        return None;
    }
    Some(rest.split('/').filter(|s| !s.is_empty()).collect())
}

/// The parent of a language root with other languages next to it: `/en/`
/// gives `/` (and `/docs/fr-ca/` gives `/docs/`) when `pages` has pages
/// under another language of that parent (`/fr/…`, `/de/…`), the same site
/// in several languages rather than a part of it. `None` otherwise.
pub(crate) fn language_root_parent(base_path: &str, pages: &[MappedPage]) -> Option<String> {
    let (parent, last) = base_path.trim_end_matches('/').rsplit_once('/')?;
    if !is_language(last) {
        return None;
    }
    let parent = format!("{parent}/");
    let language = normalize_language(last);
    pages
        .iter()
        .filter_map(|p| url::Url::parse(&p.url).ok())
        .filter_map(|u| section_of(u.path(), &parent).and_then(|(l, _)| l))
        .any(|other| other != language)
        .then_some(parent)
}

/// The language segment of a path (lowercased) and its section key.
pub(crate) fn section_of(path: &str, base_path: &str) -> Option<(Option<String>, String)> {
    section_with(path, base_path, is_language)
}

/// [`section_of`] with only the languages of `languages` taken for
/// language segments (those [`group_sections`] accepted).
pub(crate) fn section_in(
    path: &str,
    base_path: &str,
    languages: &BTreeSet<String>,
) -> Option<(Option<String>, String)> {
    section_with(path, base_path, |segment| {
        languages.contains(&normalize_language(segment))
    })
}

fn normalize_language(segment: &str) -> String {
    segment.to_ascii_lowercase().replace('_', "-")
}

fn section_with(
    path: &str,
    base_path: &str,
    is_language: impl Fn(&str) -> bool,
) -> Option<(Option<String>, String)> {
    let mut segments = segments_under(path, base_path)?;
    let mut language = None;
    if segments.first().is_some_and(|s| is_language(s)) {
        language = Some(normalize_language(segments.remove(0)));
    }
    let key = match segments.as_slice() {
        [] | [_] => HOME_KEY.to_string(),
        [first, ..] => format!("/{}/", first.to_ascii_lowercase()),
    };
    Some((language, key))
}

/// The site's sections (largest first, ties in map order) and the language
/// segments found, from the map's pages under `base_path`. A one-page
/// section counts as top-level; past [`MAX_SECTIONS`] the smallest merge
/// into [`OTHER_KEY`].
pub(crate) fn group_sections(
    pages: &[MappedPage],
    base_path: &str,
) -> (Vec<Section>, BTreeSet<String>) {
    // Language segments: a lone two-letter code is often a section
    // (`/news/uk`), so it counts only next to other languages; a code with
    // a region (`es-US`) always does.
    let candidates: BTreeSet<String> = pages
        .iter()
        .filter_map(|p| url::Url::parse(&p.url).ok())
        .filter_map(|u| section_of(u.path(), base_path).and_then(|(language, _)| language))
        .collect();
    let accepted: BTreeSet<String> = if candidates.len() >= 2 {
        candidates
    } else {
        candidates.into_iter().filter(|l| l.contains('-')).collect()
    };

    let mut languages = BTreeSet::new();
    // key -> (first seen, section)
    let mut by_key: BTreeMap<String, (usize, Section)> = BTreeMap::new();
    for (order, page) in pages.iter().enumerate() {
        let Ok(url) = url::Url::parse(&page.url) else {
            continue;
        };
        let Some((language, key)) = section_in(url.path(), base_path, &accepted) else {
            continue;
        };
        languages.extend(language);
        let entry = by_key.entry(key.clone()).or_insert_with(|| {
            (
                order,
                Section {
                    key,
                    pages: 0,
                    titles: Vec::new(),
                    urls: Vec::new(),
                },
            )
        });
        add_page(&mut entry.1, page);
    }

    // A section of one page (e.g. `/pricing/annual`) reads as a top-level
    // page: fold it into home.
    let singles: Vec<String> = by_key
        .iter()
        .filter(|(key, (_, s))| s.pages == 1 && key.as_str() != HOME_KEY)
        .map(|(key, _)| key.clone())
        .collect();
    for key in singles {
        if let Some((order, single)) = by_key.remove(&key) {
            let home = by_key.entry(HOME_KEY.to_string()).or_insert_with(|| {
                (
                    order,
                    Section {
                        key: HOME_KEY.to_string(),
                        pages: 0,
                        titles: Vec::new(),
                        urls: Vec::new(),
                    },
                )
            });
            home.0 = home.0.min(order);
            merge_into(&mut home.1, single);
        }
    }

    let mut sections: Vec<(usize, Section)> = by_key.into_values().collect();
    sections.sort_by(|(a_order, a), (b_order, b)| b.pages.cmp(&a.pages).then(a_order.cmp(b_order)));
    let mut sections: Vec<Section> = sections.into_iter().map(|(_, s)| s).collect();
    if sections.len() > MAX_SECTIONS {
        let rest = sections.split_off(MAX_SECTIONS);
        let mut other = Section {
            key: OTHER_KEY.to_string(),
            pages: 0,
            titles: Vec::new(),
            urls: Vec::new(),
        };
        for section in rest {
            merge_into(&mut other, section);
        }
        sections.push(other);
    }
    (sections, languages)
}

fn add_page(section: &mut Section, page: &MappedPage) {
    section.pages += 1;
    if section.urls.len() < URLS_PER_SECTION {
        section.urls.push(page.url.clone());
    }
    if section.titles.len() < TITLES_PER_SECTION {
        if let Some(title) = page
            .title
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            section.titles.push(title.chars().take(120).collect());
        }
    }
}

fn merge_into(into: &mut Section, from: Section) {
    into.pages += from.pages;
    let room = URLS_PER_SECTION.saturating_sub(into.urls.len());
    into.urls.extend(from.urls.into_iter().take(room));
    let room = TITLES_PER_SECTION.saturating_sub(into.titles.len());
    into.titles.extend(from.titles.into_iter().take(room));
}

/// The kind a section key implies on its own, if any.
pub(crate) fn kind_from_key(key: &str) -> Option<SectionKind> {
    let segment = key.trim_matches('/');
    use SectionKind::*;
    let kind = match segment {
        "product" | "products" | "p" | "dp" | "shop" | "store" | "item" | "items" | "buy" => {
            Product
        }
        "collections" | "collection" | "category" | "categories" | "c" | "cat" | "catalog"
        | "catalogue" | "brands" | "brand" | "department" | "departments" => Listing,
        "blog" | "blogs" | "news" | "article" | "articles" | "post" | "posts" | "journal"
        | "stories" | "story" | "insights" | "press" | "magazine" | "updates" | "changelog" => {
            Article
        }
        "docs" | "doc" | "documentation" | "guide" | "guides" | "api" | "reference"
        | "developers" | "developer" | "learn" | "manual" | "handbook" | "tutorial"
        | "tutorials" | "sdk" | "sdks" => Docs,
        "help" | "support" | "faq" | "faqs" | "kb" | "knowledge-base" | "knowledgebase"
        | "community" | "forum" | "forums" | "answers" | "questions" | "hc" => Support,
        "tag" | "tags" | "author" | "authors" | "page" | "archive" | "archives" => Archive,
        "legal" | "privacy" | "privacy-policy" | "terms" | "terms-of-service" | "tos"
        | "cookies" | "cookie-policy" | "imprint" | "impressum" | "gdpr" | "disclaimer" => Legal,
        "login" | "signin" | "sign-in" | "logout" | "signup" | "sign-up" | "register"
        | "account" | "accounts" | "my-account" | "cart" | "checkout" | "basket" | "wishlist"
        | "auth" | "oauth" | "password" | "search" => Account,
        _ => return None,
    };
    Some(kind)
}

/// Whether a URL path holds a date (`/2024/05/…`): a dated article.
pub(crate) fn has_date_path(url: &str) -> bool {
    let Ok(url) = url::Url::parse(url) else {
        return false;
    };
    let segments: Vec<&str> = url.path().split('/').filter(|s| !s.is_empty()).collect();
    segments.windows(2).any(|w| {
        w[0].len() == 4
            && (w[0].starts_with("19") || w[0].starts_with("20"))
            && w[0].bytes().all(|b| b.is_ascii_digit())
            && w[1].len() <= 2
            && w[1].bytes().all(|b| b.is_ascii_digit())
    })
}

// ============================================================================
// Page signals
// ============================================================================

/// schema.org types worth indexing: the rest (`WebPage`, `WebSite`,
/// `Organization`, `ImageObject`, …) sit on every page and say nothing.
pub(crate) const USEFUL_SCHEMA_TYPES: &[&str] = &[
    "Product",
    "ProductGroup",
    "Offer",
    "AggregateOffer",
    "AggregateRating",
    "Review",
    "Brand",
    "BreadcrumbList",
    "Article",
    "BlogPosting",
    "NewsArticle",
    "TechArticle",
    "Report",
    "FAQPage",
    "QAPage",
    "HowTo",
    "Recipe",
    "Event",
    "JobPosting",
    "Course",
    "SoftwareApplication",
    "LocalBusiness",
    "Book",
    "Movie",
    "VideoObject",
    "Podcast",
    "PodcastEpisode",
];

/// Types whose dates (published, start, valid through) are worth turning
/// into sortable timestamps.
const DATED_SCHEMA_TYPES: &[&str] = &[
    "Article",
    "BlogPosting",
    "NewsArticle",
    "TechArticle",
    "Report",
    "Event",
    "JobPosting",
    "PodcastEpisode",
];

/// Whether any of `types` carries dates.
pub(crate) fn has_dated_types(types: &BTreeSet<String>) -> bool {
    types
        .iter()
        .any(|t| DATED_SCHEMA_TYPES.contains(&t.as_str()))
}

/// Office document extensions the `documents` feature parses.
const OFFICE_EXTENSIONS: &[&str] = &[
    "doc", "docx", "docm", "xls", "xlsx", "xlsm", "ppt", "pptx", "pptm", "odt", "ods", "odp",
    "rtf", "epub",
];

/// What one sampled page shows.
#[derive(Debug, Clone, Default)]
pub(crate) struct PageSignals {
    pub url: String,
    pub section: String,
    pub title: Option<String>,
    /// Words of the page's main content (Markdown).
    pub words: usize,
    /// Markdown headings (`#` to `######`).
    pub headings: usize,
    /// Useful schema.org types found (JSON-LD and Microdata).
    pub schema_types: BTreeSet<String>,
    pub platform: Option<SitePlatform>,
    /// The HTTP response looks like a JavaScript app shell.
    pub js_suspect: bool,
    /// Distinct PDF links on the site's host.
    pub pdf_links: BTreeSet<String>,
    /// Distinct office document links on the site's host.
    pub document_links: BTreeSet<String>,
    /// The first part of the page's Markdown (for the AI pass).
    pub excerpt: String,
}

/// Characters of Markdown kept per page for the AI pass.
pub(crate) const EXCERPT_CHARS: usize = 1_500;

/// The signals of one `/scrape` result (`payload`: its JSON, with the
/// `markdown`, `rawhtml`, `schema`, `links` and `metadata` formats).
pub(crate) fn page_signals(url: &str, section: &str, payload: &Value, host: &str) -> PageSignals {
    let markdown = payload
        .get("markdown")
        .and_then(Value::as_str)
        .unwrap_or("");
    let raw_html = payload
        .get("raw_html")
        .and_then(Value::as_str)
        .unwrap_or("");
    let words = word_count(markdown);
    let mut signals = PageSignals {
        url: url.to_string(),
        section: section.to_string(),
        title: payload
            .pointer("/metadata/title")
            .and_then(Value::as_str)
            .map(str::to_string),
        words,
        headings: heading_count(markdown),
        schema_types: schema_types(payload.get("schema").unwrap_or(&Value::Null)),
        platform: detect_platform(raw_html),
        js_suspect: looks_like_js_shell(raw_html, words),
        excerpt: markdown.chars().take(EXCERPT_CHARS).collect(),
        ..Default::default()
    };
    for link in payload
        .get("links")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        let Ok(parsed) = url::Url::parse(link) else {
            continue;
        };
        if !same_site(parsed.host_str().unwrap_or(""), host) {
            continue;
        }
        let path = parsed.path().to_ascii_lowercase();
        let Some(ext) = path.rsplit_once('.').map(|(_, ext)| ext) else {
            continue;
        };
        let mut clean = parsed.clone();
        clean.set_fragment(None);
        if ext == "pdf" {
            signals.pdf_links.insert(clean.to_string());
        } else if OFFICE_EXTENSIONS.contains(&ext) {
            signals.document_links.insert(clean.to_string());
        }
    }
    signals
}

/// Whether `link_host` is the site's host or one of its subdomains (PDFs
/// often live on `cdn.` or `assets.`), `www.` ignored.
pub(crate) fn same_site(link_host: &str, host: &str) -> bool {
    let site = host.trim_start_matches("www.");
    let link = link_host.trim_start_matches("www.");
    link == site || link.ends_with(&format!(".{site}"))
}

/// Markdown headings: ATX (`## Part`) and setext (a line underlined with
/// `===` or `---`, as the HTML to Markdown converter writes `h1` and `h2`).
pub(crate) fn heading_count(markdown: &str) -> usize {
    let mut count = 0;
    let mut previous = "";
    for line in markdown.lines() {
        let trimmed = line.trim();
        let hashes = trimmed.bytes().take_while(|b| *b == b'#').count();
        let atx = (1..=6).contains(&hashes) && trimmed.as_bytes().get(hashes) == Some(&b' ');
        let underline = trimmed.len() >= 2
            && (trimmed.bytes().all(|b| b == b'=') || trimmed.bytes().all(|b| b == b'-'));
        let setext = underline && !previous.is_empty() && !previous.starts_with(['-', '*', '>']);
        if atx || setext {
            count += 1;
        }
        previous = trimmed;
    }
    count
}

/// Words of a text (whitespace-separated runs holding a letter or digit).
pub(crate) fn word_count(text: &str) -> usize {
    text.split_whitespace()
        .filter(|w| w.chars().any(char::is_alphanumeric))
        .count()
}

/// Useful schema.org types in a `/scrape` `schema` result: the items'
/// `@type`s and those found anywhere in the raw JSON-LD (`@graph`
/// included), without the `https://schema.org/` prefix.
pub(crate) fn schema_types(schema: &Value) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    collect_types(schema, &mut found, 0);
    found
        .into_iter()
        .filter(|t| USEFUL_SCHEMA_TYPES.contains(&t.as_str()))
        .collect()
}

fn collect_types(value: &Value, found: &mut BTreeSet<String>, depth: usize) {
    if depth > 12 {
        return;
    }
    match value {
        Value::Object(map) => {
            for (key, v) in map {
                if key == "@type" {
                    let types: Vec<&str> = match v {
                        Value::String(s) => vec![s.as_str()],
                        Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
                        _ => Vec::new(),
                    };
                    for t in types {
                        let t = t.rsplit('/').next().unwrap_or(t).trim();
                        if !t.is_empty() {
                            found.insert(t.to_string());
                        }
                    }
                } else {
                    collect_types(v, found, depth + 1);
                }
            }
        }
        Value::Array(items) => {
            for v in items {
                collect_types(v, found, depth + 1);
            }
        }
        _ => {}
    }
}

/// Whether an HTTP response looks like a JavaScript app shell: almost no
/// text, with an empty mount point, a framework marker or a `<noscript>`
/// asking for JavaScript.
pub(crate) fn looks_like_js_shell(raw_html: &str, words: usize) -> bool {
    if raw_html.is_empty() {
        return false;
    }
    if words >= 80 {
        return false;
    }
    if words < 15 {
        return true;
    }
    let html = raw_html.to_ascii_lowercase();
    const SHELL_MARKS: &[&str] = &[
        "<div id=\"root\"></div>",
        "<div id=\"app\"></div>",
        "<div id=\"__next\"></div>",
        "<div id=\"__nuxt\"></div>",
        "<app-root",
        "ng-version",
        "enable javascript",
        "javascript is required",
        "javascript to run this app",
    ];
    SHELL_MARKS.iter().any(|m| html.contains(m)) || html.matches("<script").count() >= 10
}

/// Whether rendering the page with a browser showed much more text than
/// its HTTP response: the site needs JavaScript to show its content.
pub(crate) fn rendering_reveals_content(http_words: usize, rendered_words: usize) -> bool {
    rendered_words >= 100 && rendered_words >= http_words.saturating_mul(2) + 50
}

// ============================================================================
// Classification
// ============================================================================

/// A section, classified.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub(crate) struct SiteSection {
    pub key: String,
    pub pages: usize,
    pub kind: SectionKind,
    /// Why it got this kind.
    pub because: String,
    /// The useful schema.org types its sampled pages carry.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub schema_types: BTreeSet<String>,
    /// Average words of its sampled pages (`null` when none was sampled).
    pub avg_words: Option<usize>,
    /// Average Markdown headings of its sampled pages.
    pub avg_headings: Option<usize>,
    /// Pages of it that were sampled.
    pub sampled: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub titles: Vec<String>,
}

/// What a section is classified against, besides its own pages.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SiteContext {
    /// The platform most of the site's sampled pages were built with.
    pub platform: Option<SitePlatform>,
    /// The site sells things: a shop platform, or `Product` markup on a
    /// sampled page.
    pub commerce: bool,
    /// Version sections are older copies of the docs ([`has_older_versions`]).
    pub older_versions: bool,
}

/// The platform most of `samples` were built with (half of them at least).
pub(crate) fn majority_platform(samples: &[&PageSignals]) -> Option<SitePlatform> {
    let mut counts: Vec<(SitePlatform, usize)> = Vec::new();
    for platform in samples.iter().filter_map(|p| p.platform) {
        match counts.iter_mut().find(|(p, _)| *p == platform) {
            Some((_, n)) => *n += 1,
            None => counts.push((platform, 1)),
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .filter(|(_, n)| n * 2 >= samples.len())
        .map(|(platform, _)| platform)
}

fn platform_name(platform: SitePlatform) -> String {
    serde_json::to_value(platform)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// The kind of a section from its key, URLs and sampled pages, in the
/// site's context, with the reason.
pub(crate) fn classify_section(
    section: &Section,
    samples: &[&PageSignals],
    site: SiteContext,
) -> (SectionKind, String) {
    let key = section.key.as_str();
    let by_key = kind_from_key(key);
    if let Some(kind) = by_key.filter(|k| k.is_excluded()) {
        return (kind, format!("`{key}` pages are {}", kind.as_str()));
    }
    if site.older_versions && is_version_key(key) {
        return (
            SectionKind::Archive,
            format!("`{key}` is another version of the docs, a copy of the current pages"),
        );
    }

    let types: BTreeSet<&str> = samples
        .iter()
        .flat_map(|s| s.schema_types.iter().map(String::as_str))
        .collect();
    let has = |names: &[&'static str]| -> Option<&'static str> {
        names.iter().find(|n| types.contains(*n)).copied()
    };
    if let Some(t) = has(&["Product", "ProductGroup"]) {
        return (
            SectionKind::Product,
            format!("pages carry schema.org `{t}`"),
        );
    }

    // The platform of the section's own pages decides over generic markup
    // (a help center's articles are `TechArticle`s); the site's platform
    // only for unsampled sections, never for top-level pages.
    let own = majority_platform(samples).filter(|_| !samples.is_empty());
    let platform = own.or(site
        .platform
        .filter(|_| samples.is_empty() && key != HOME_KEY));
    if let Some(p) = platform {
        let kind = match p.site_type() {
            Some(SiteType::Docs) => Some(SectionKind::Docs),
            Some(SiteType::Support) => Some(SectionKind::Support),
            Some(SiteType::Blog) => Some(SectionKind::Article),
            _ => None,
        };
        if let Some(kind) = kind {
            return (kind, format!("pages built with {}", platform_name(p)));
        }
    }

    if let Some(t) = has(&["Article", "BlogPosting", "NewsArticle", "Report"]) {
        if by_key != Some(SectionKind::Docs) {
            return (
                SectionKind::Article,
                format!("pages carry schema.org `{t}`"),
            );
        }
    }
    if let Some(t) = has(&["TechArticle"]) {
        return (SectionKind::Docs, format!("pages carry schema.org `{t}`"));
    }
    if let Some(t) = has(&["QAPage"]) {
        return (
            SectionKind::Support,
            format!("pages carry schema.org `{t}`"),
        );
    }
    // FAQ markup is common on landing pages: it makes a support section only
    // where nothing says otherwise.
    if let Some(t) = has(&["FAQPage"]) {
        if key != HOME_KEY && matches!(by_key, None | Some(SectionKind::Support)) {
            return (
                SectionKind::Support,
                format!("pages carry schema.org `{t}`"),
            );
        }
    }

    let dated = section.urls.iter().filter(|u| has_date_path(u)).count();
    if !section.urls.is_empty() && dated * 2 >= section.urls.len() {
        return (
            SectionKind::Article,
            "URLs are dated (`/YYYY/MM/`)".to_string(),
        );
    }
    match by_key {
        Some(SectionKind::Product) if !site.commerce => {
            return (
                SectionKind::Marketing,
                format!("`{key}` pages present products but carry no product markup"),
            )
        }
        Some(SectionKind::Listing) if !site.commerce => {
            return (
                SectionKind::Archive,
                format!("`{key}` pages list the site's other pages"),
            )
        }
        Some(kind) => {
            return (kind, format!("`{key}` pages are usually {}", kind.as_str()));
        }
        None => {}
    }
    let because = if key == HOME_KEY {
        "top-level pages".to_string()
    } else {
        "no stronger signal".to_string()
    };
    (SectionKind::Marketing, because)
}

/// The site's type from its classified sections (weighted by pages, the
/// excluded ones left out) and its platform, with a confidence in `[0, 1]`
/// and the reason.
pub(crate) fn classify_site(
    sections: &[SiteSection],
    platform: Option<SitePlatform>,
) -> (SiteType, f32, String) {
    let mut by_type: BTreeMap<SiteType, usize> = BTreeMap::new();
    for section in sections.iter().filter(|s| !s.kind.is_excluded()) {
        let site = match section.kind {
            SectionKind::Product | SectionKind::Listing => SiteType::Ecommerce,
            SectionKind::Article => SiteType::Blog,
            SectionKind::Docs => SiteType::Docs,
            SectionKind::Support => SiteType::Support,
            _ => SiteType::Marketing,
        };
        *by_type.entry(site).or_default() += section.pages;
    }
    let total: usize = by_type.values().sum();
    if total == 0 {
        let site = platform
            .and_then(SitePlatform::site_type)
            .unwrap_or(SiteType::Marketing);
        return (site, 0.3, "too few pages to tell".to_string());
    }
    let share = |t: SiteType| by_type.get(&t).copied().unwrap_or(0) as f32 / total as f32;
    let mut ranked: Vec<(SiteType, f32)> = by_type.keys().map(|t| (*t, share(*t))).collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    let (top, top_share) = ranked[0];
    let pct = |s: f32| (s * 100.0).round() as u32;

    if let Some(implied) = platform.and_then(SitePlatform::site_type) {
        let commerce_seen = implied == SiteType::Ecommerce
            && sections
                .iter()
                .any(|s| matches!(s.kind, SectionKind::Product | SectionKind::Listing));
        if (implied != SiteType::Ecommerce && share(implied) >= 0.4) || commerce_seen {
            let platform_name = platform.map(platform_name).unwrap_or_default();
            return (
                implied,
                share(implied).clamp(0.7, 0.95),
                format!(
                    "built with {platform_name}, {}% of the pages are {}",
                    pct(share(implied)),
                    implied.as_str()
                ),
            );
        }
    }
    // Product markup and a quarter of the pages in products and listings:
    // a shop, even with many content pages.
    let product_markup = sections.iter().any(|s| {
        s.kind == SectionKind::Product
            && s.schema_types
                .iter()
                .any(|t| matches!(t.as_str(), "Product" | "ProductGroup"))
    });
    if product_markup && top != SiteType::Ecommerce && share(SiteType::Ecommerce) >= 0.25 {
        let commerce = share(SiteType::Ecommerce);
        return (
            SiteType::Ecommerce,
            commerce.max(0.6),
            format!(
                "pages carry schema.org `Product` and {}% of the pages are products or listings",
                pct(commerce)
            ),
        );
    }
    if top_share >= 0.6 {
        return (
            top,
            top_share,
            format!("{}% of the pages are {}", pct(top_share), top.as_str()),
        );
    }
    let second = ranked.get(1).copied();
    if let Some((second_type, second_share)) = second {
        if second_share >= 0.2 {
            return (
                SiteType::Mixed,
                top_share,
                format!(
                    "{}% {} and {}% {} pages",
                    pct(top_share),
                    top.as_str(),
                    pct(second_share),
                    second_type.as_str()
                ),
            );
        }
    }
    (
        top,
        top_share,
        format!("{}% of the pages are {}", pct(top_share), top.as_str()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn page(url: &str) -> MappedPage {
        MappedPage {
            url: url.to_string(),
            title: Some(format!("T {url}")),
        }
    }

    #[test]
    fn sections_group_by_first_segment_after_language_and_base() {
        let pages = [
            "https://a.test/",
            "https://a.test/pricing",
            "https://a.test/blog/one",
            "https://a.test/blog/two",
            "https://a.test/fr/blog/trois",
            "https://a.test/es-mx/blog/cuatro",
            "https://a.test/docs/a/b",
            "https://a.test/docs/c",
            "https://a.test/careers/one-job",
        ]
        .map(page);
        let (sections, languages) = group_sections(&pages, "/");
        let keys: Vec<(&str, usize)> = sections.iter().map(|s| (s.key.as_str(), s.pages)).collect();
        // `/careers/` has one page: folded into home.
        assert_eq!(keys, [("/blog/", 4), ("/", 3), ("/docs/", 2)]);
        assert_eq!(languages.into_iter().collect::<Vec<_>>(), ["es-mx", "fr"]);
    }

    /// A lone two-letter segment is a section (`/news/uk/...` on a news
    /// site), not a language.
    #[test]
    fn a_lone_language_code_is_a_section() {
        let pages = [
            "https://a.test/news/uk/a",
            "https://a.test/news/uk/b",
            "https://a.test/news/world/c",
            "https://a.test/news/world/d",
        ]
        .map(page);
        let (sections, languages) = group_sections(&pages, "/news/");
        let keys: Vec<&str> = sections.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(keys, ["/uk/", "/world/"]);
        assert!(languages.is_empty());
    }

    /// `/en` of a site also in `/fr` and `/de` (nous-larochelle.fr) is the
    /// whole site in one of its languages: the crawl takes its parent.
    #[test]
    fn a_language_root_next_to_other_languages_widens_to_its_parent() {
        let site = [
            "https://a.test/fr",
            "https://a.test/fr/information-touristique/hotel/la-paix",
            "https://a.test/en/turistic-information/hotel/la-paix-en",
            "https://a.test/de/touristen-information/hotel/la-paix-de",
        ]
        .map(page);
        assert_eq!(language_root_parent("/en/", &site).as_deref(), Some("/"));
        assert_eq!(language_root_parent("/fr/", &site).as_deref(), Some("/"));

        let docs = [
            "https://a.test/docs/fr-ca/start",
            "https://a.test/docs/en-us/start",
        ]
        .map(page);
        assert_eq!(
            language_root_parent("/docs/en-us/", &docs).as_deref(),
            Some("/docs/")
        );

        // Only English pages: nothing next to it.
        let english = ["https://a.test/en/a", "https://a.test/en/b"].map(page);
        assert_eq!(language_root_parent("/en/", &english), None);
        // Not a language root.
        assert_eq!(language_root_parent("/blog/", &site), None);
        assert_eq!(language_root_parent("/", &site), None);
        // `/news/uk/` next to `/news/world/` is a section, not a language.
        let news = ["https://a.test/news/uk/a", "https://a.test/news/world/b"].map(page);
        assert_eq!(language_root_parent("/news/uk/", &news), None);
    }

    #[test]
    fn sections_respect_the_base_path() {
        let pages = [
            "https://a.test/docs/",
            "https://a.test/docs/guides/a",
            "https://a.test/docs/guides/b",
            "https://a.test/blog/x",
        ]
        .map(page);
        let (sections, _) = group_sections(&pages, "/docs/");
        let keys: Vec<&str> = sections.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(keys, ["/guides/", "/"]);
        // `/docsearch` is not under `/docs`.
        assert!(section_of("/docsearch/x", "/docs/").is_none());
    }

    #[test]
    fn sections_past_the_cap_merge_into_other() {
        let mut pages = Vec::new();
        for i in 0..(MAX_SECTIONS + 3) {
            for j in 0..(2 + MAX_SECTIONS + 3 - i) {
                pages.push(page(&format!("https://a.test/s{i}/p{j}")));
            }
        }
        let (sections, _) = group_sections(&pages, "/");
        assert_eq!(sections.len(), MAX_SECTIONS + 1);
        let other = sections.last().unwrap();
        assert_eq!(other.key, OTHER_KEY);
        // The three smallest sections: 5, 4 and 3 pages.
        assert_eq!(other.pages, 12);
    }

    #[test]
    fn languages_are_known_codes_with_an_optional_region() {
        assert!(is_language("fr"));
        assert!(is_language("en-US"));
        assert!(is_language("pt_BR"));
        assert!(is_language("zh-Hans"));
        assert!(!is_language("api"));
        assert!(!is_language("e1"));
        assert!(!is_language("eu"));
        assert!(!is_language("us"));
        assert!(!is_language("en-1"));
    }

    #[test]
    fn dated_paths_are_detected() {
        assert!(has_date_path("https://a.test/2024/05/hello"));
        assert!(has_date_path("https://a.test/blog/1999/1/x"));
        assert!(!has_date_path("https://a.test/products/2024"));
        assert!(!has_date_path("https://a.test/v2/12/x"));
    }

    #[test]
    fn schema_types_are_found_in_items_and_json_ld_graphs() {
        let schema = json!({
            "json_ld": [{"@graph": [
                {"@type": "WebPage"},
                {"@type": ["Product", "Thing"], "offers": {"@type": "https://schema.org/Offer"}}
            ]}],
            "items": [{"@type": "BreadcrumbList"}],
            "microdata": [{"@type": "Review"}]
        });
        let types: Vec<String> = schema_types(&schema).into_iter().collect();
        assert_eq!(types, ["BreadcrumbList", "Offer", "Product", "Review"]);
    }

    #[test]
    fn platforms_are_fingerprinted_most_specific_first() {
        let woo = r#"<link href="/wp-content/plugins/woocommerce/style.css">"#;
        assert_eq!(detect_platform(woo), Some(SitePlatform::Woocommerce));
        let wp = r#"<script src="/wp-includes/js/x.js"></script>"#;
        assert_eq!(detect_platform(wp), Some(SitePlatform::Wordpress));
        let shop = r#"<script src="//cdn.shopify.com/s/files/x.js"></script>"#;
        assert_eq!(detect_platform(shop), Some(SitePlatform::Shopify));
        let docs = r#"<meta name="generator" content="Docusaurus v3">"#;
        assert_eq!(detect_platform(docs), Some(SitePlatform::Docusaurus));
        assert_eq!(detect_platform("<html><body>hi</body></html>"), None);
    }

    #[test]
    fn js_shells_are_suspected_only_with_little_text() {
        let shell =
            r#"<html><body><div id="root"></div><script src="/a.js"></script></body></html>"#;
        assert!(looks_like_js_shell(shell, 20));
        assert!(looks_like_js_shell("<html><body></body></html>", 3));
        assert!(!looks_like_js_shell(shell, 500));
        let plain = "<html><body><p>Short but server rendered.</p></body></html>";
        assert!(!looks_like_js_shell(plain, 40));
        assert!(!looks_like_js_shell("", 0));
    }

    #[test]
    fn headings_are_counted_in_both_markdown_styles() {
        let markdown = "Title\n=====\n\nIntro\n\nPart 1\n------\n\ntext\n\n### Detail\n\n---\n\n- item\n---\n#nospace";
        assert_eq!(heading_count(markdown), 3);
    }

    #[test]
    fn rendering_must_reveal_much_more_text() {
        assert!(rendering_reveals_content(10, 600));
        assert!(!rendering_reveals_content(400, 600));
        assert!(!rendering_reveals_content(0, 60));
    }

    #[test]
    fn page_signals_read_the_scrape_payload() {
        let payload = json!({
            "markdown": "# Title\n\nSome words here.\n\n## Part\n\nMore words.",
            "raw_html": "<html><script src=\"//cdn.shopify.com/x.js\"></script></html>",
            "metadata": {"title": "Shoe"},
            "schema": {"items": [{"@type": "Product"}, {"@type": "Organization"}]},
            "links": [
                "https://shop.test/files/manual.pdf#page=2",
                "https://cdn.shop.test/catalog.PDF",
                "https://other.test/x.pdf",
                "https://shop.test/sheet.xlsx",
                "https://shop.test/about"
            ]
        });
        let s = page_signals(
            "https://shop.test/products/shoe",
            "/products/",
            &payload,
            "www.shop.test",
        );
        assert_eq!(s.title.as_deref(), Some("Shoe"));
        assert_eq!(s.headings, 2);
        assert_eq!(s.words, 7);
        assert_eq!(s.platform, Some(SitePlatform::Shopify));
        assert_eq!(s.schema_types.iter().collect::<Vec<_>>(), ["Product"]);
        assert_eq!(s.pdf_links.len(), 2);
        assert_eq!(s.document_links.len(), 1);
    }

    fn section(key: &str, pages: usize, urls: &[&str]) -> Section {
        Section {
            key: key.to_string(),
            pages,
            titles: Vec::new(),
            urls: urls.iter().map(|u| u.to_string()).collect(),
        }
    }

    fn signals(types: &[&str]) -> PageSignals {
        PageSignals {
            schema_types: types.iter().map(|t| t.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn sections_are_classified_by_schema_then_dates_then_key() {
        let product = signals(&["Product", "BreadcrumbList"]);
        let (kind, _) = classify_section(
            &section("/x/", 10, &[]),
            &[&product],
            SiteContext::default(),
        );
        assert_eq!(kind, SectionKind::Product);

        let dated = section(
            "/x/",
            2,
            &["https://a.test/x/2024/01/a", "https://a.test/x/2023/12/b"],
        );
        assert_eq!(
            classify_section(&dated, &[], SiteContext::default()).0,
            SectionKind::Article
        );

        assert_eq!(
            classify_section(&section("/blog/", 3, &[]), &[], SiteContext::default()).0,
            SectionKind::Article
        );
        assert_eq!(
            classify_section(
                &section("/cart/", 3, &[]),
                &[&product],
                SiteContext::default()
            )
            .0,
            SectionKind::Account
        );
        assert_eq!(
            classify_section(&section("/", 3, &[]), &[], SiteContext::default()).0,
            SectionKind::Marketing
        );
        assert_eq!(
            classify_section(
                &section("/getting-started/", 3, &[]),
                &[],
                SiteContext {
                    platform: Some(SitePlatform::Mintlify),
                    ..Default::default()
                }
            )
            .0,
            SectionKind::Docs
        );
        // An `Article` under `/docs/` stays docs.
        let article = signals(&["Article"]);
        assert_eq!(
            classify_section(
                &section("/docs/", 3, &[]),
                &[&article],
                SiteContext::default()
            )
            .0,
            SectionKind::Docs
        );
    }

    fn with_platform(types: &[&str], platform: SitePlatform) -> PageSignals {
        PageSignals {
            platform: Some(platform),
            ..signals(types)
        }
    }

    /// What the live runs got wrong (SCR-95 review): FAQ markup on blog
    /// posts, Offer on a SaaS pricing page, `/products/` on a SaaS site,
    /// help center articles marked `TechArticle`, a docs platform seen on
    /// one section only.
    #[test]
    fn markup_seen_on_real_sites_is_weighed_right() {
        let none = SiteContext::default();
        let post = signals(&["Article", "BlogPosting", "FAQPage"]);
        assert_eq!(
            classify_section(&section("/", 300, &[]), &[&post], none).0,
            SectionKind::Article
        );

        let pricing = signals(&["Offer", "SoftwareApplication"]);
        assert_eq!(
            classify_section(&section("/", 20, &[]), &[&pricing], none).0,
            SectionKind::Marketing
        );

        let faq = signals(&["BreadcrumbList", "FAQPage"]);
        assert_eq!(
            classify_section(&section("/products/", 8, &[]), &[&faq], none).0,
            SectionKind::Marketing
        );
        let shop = SiteContext {
            commerce: true,
            ..none
        };
        assert_eq!(
            classify_section(&section("/products/", 8, &[]), &[], shop).0,
            SectionKind::Product
        );
        assert_eq!(
            classify_section(&section("/category/", 8, &[]), &[], none).0,
            SectionKind::Archive
        );
        assert_eq!(
            classify_section(&section("/cat/", 8, &[]), &[], shop).0,
            SectionKind::Listing
        );

        let help = with_platform(&["TechArticle"], SitePlatform::Zendesk);
        assert_eq!(
            classify_section(&section("/articles/", 240, &[]), &[&help], none).0,
            SectionKind::Support
        );

        // Mintlify on the docs pages only: the site is not a docs site, and
        // an unsampled `/integrations/` is not docs.
        let site = SiteContext {
            platform: None,
            ..none
        };
        assert_eq!(
            classify_section(&section("/integrations/", 23, &[]), &[], site).0,
            SectionKind::Marketing
        );
        let sphinx = with_platform(&[], SitePlatform::Readthedocs);
        assert_eq!(
            classify_section(&section("/", 22, &[]), &[&sphinx, &sphinx], none).0,
            SectionKind::Docs
        );
    }

    #[test]
    fn older_doc_versions_are_archived_only_next_to_current_docs() {
        assert!(is_version_key("/3.8.1/"));
        assert!(is_version_key("/v2/"));
        assert!(is_version_key("/2.x/"));
        assert!(is_version_key("/next/"));
        assert!(!is_version_key("/v/"));
        assert!(!is_version_key("/2024-report/"));
        assert!(!is_version_key("/api/"));

        let docs = [
            section("/3.8.1/", 91, &[]),
            section("/3.9.2/", 91, &[]),
            section("/", 38, &[]),
            section("/api/", 35, &[]),
        ];
        assert!(has_older_versions(&docs));
        let ctx = SiteContext {
            older_versions: true,
            ..Default::default()
        };
        assert_eq!(classify_section(&docs[0], &[], ctx).0, SectionKind::Archive);
        // Versions only: they are the docs.
        assert!(!has_older_versions(&docs[..2]));
        // One version: nothing to compare with.
        assert!(!has_older_versions(&docs[1..]));
    }

    #[test]
    fn a_shop_without_a_platform_is_recognized_by_its_product_markup() {
        let mut product = classified("/p/", 18, SectionKind::Product);
        product.schema_types = ["Product".to_string()].into();
        let sections = [
            classified("/cat/", 37, SectionKind::Listing),
            classified("/customer-service/", 29, SectionKind::Marketing),
            classified("/", 22, SectionKind::Marketing),
            product,
            classified("/rooms/", 8, SectionKind::Marketing),
        ];
        assert_eq!(classify_site(&sections, None).0, SiteType::Ecommerce);
    }

    fn classified(key: &str, pages: usize, kind: SectionKind) -> SiteSection {
        SiteSection {
            key: key.to_string(),
            pages,
            kind,
            because: String::new(),
            schema_types: BTreeSet::new(),
            avg_words: None,
            avg_headings: None,
            sampled: 0,
            titles: Vec::new(),
        }
    }

    #[test]
    fn site_type_follows_the_dominant_kind_or_is_mixed() {
        let shop = [
            classified("/products/", 800, SectionKind::Product),
            classified("/collections/", 100, SectionKind::Listing),
            classified("/", 40, SectionKind::Marketing),
            classified("/account/", 500, SectionKind::Account),
        ];
        assert_eq!(classify_site(&shop, None).0, SiteType::Ecommerce);

        let mixed = [
            classified("/", 50, SectionKind::Marketing),
            classified("/blog/", 120, SectionKind::Article),
            classified("/docs/", 100, SectionKind::Docs),
        ];
        assert_eq!(classify_site(&mixed, None).0, SiteType::Mixed);

        let small = [classified("/", 12, SectionKind::Marketing)];
        assert_eq!(classify_site(&small, None).0, SiteType::Marketing);
    }

    #[test]
    fn a_shop_platform_with_products_makes_an_ecommerce_site() {
        let sections = [
            classified("/", 300, SectionKind::Marketing),
            classified("/products/", 60, SectionKind::Product),
        ];
        let (site, confidence, because) = classify_site(&sections, Some(SitePlatform::Shopify));
        assert_eq!(site, SiteType::Ecommerce);
        assert!(confidence >= 0.7);
        assert!(because.contains("shopify"), "{because}");
    }

    #[test]
    fn kinds_round_trip_through_their_names() {
        for kind in [
            SectionKind::Product,
            SectionKind::Archive,
            SectionKind::Account,
        ] {
            assert_eq!(SectionKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(SiteType::parse(" Ecommerce "), Some(SiteType::Ecommerce));
        assert_eq!(SiteType::parse("shop"), None);
    }
}
