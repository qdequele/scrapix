//! The crawl config `POST /crawl/suggest` proposes for what the analysis
//! found, with one reason per decision. Each feature is scoped to the
//! sections it suits through its `include_pages` (or `exclude_pages`), so
//! one config serves a mixed site: product schema on `/products/**`, block
//! splitting on `/docs/**`.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use super::analysis::{has_dated_types, SectionKind, SiteSection, HOME_KEY, OTHER_KEY};

/// Below this average, a section's pages are short: one document each.
pub(crate) const BLOCK_SPLIT_MIN_WORDS: usize = 300;
/// Block splitting cuts at headings: pages need a few to split.
pub(crate) const BLOCK_SPLIT_MIN_HEADINGS: usize = 2;
/// From this average, a page is long enough for an AI summary to help.
pub(crate) const AI_SUMMARY_MIN_WORDS: usize = 800;
/// Above this many pages, AI features are proposed only as a warning (one
/// LLM call per page).
pub(crate) const AI_MAX_PAGES: usize = 2_000;

/// The share of the map's URLs with a query string below which, on a shop,
/// query URLs are taken for faceted navigation (filters, sorting) and left
/// out.
const FACET_QUERY_SHARE: f32 = 0.05;

/// What the analysis found, as the presets need it.
#[derive(Debug, Clone)]
pub(crate) struct Findings {
    /// The URL the crawl starts from (after redirects).
    pub start_url: String,
    /// Its host (`allowed_domains`).
    pub host: String,
    /// Its host and port, when not the scheme's default (URL globs).
    pub authority: String,
    /// The base path pages were kept under (`/` for the whole site).
    pub base_path: String,
    pub sections: Vec<SiteSection>,
    pub languages: BTreeSet<String>,
    /// Pages the map found.
    pub pages_found: usize,
    /// The page count is not known: the sitemaps do not list the pages, or
    /// the map stopped at its limit.
    pub map_truncated: bool,
    /// Share of the map's URLs with a query string.
    pub query_share: f32,
    /// JavaScript rendering decision.
    pub js: JsFinding,
    /// Distinct PDF / office document links seen on the sampled pages.
    pub pdf_links: usize,
    pub document_links: usize,
    /// The AI pass's opinion on AI summaries, if it gave one.
    pub ai_summary_hint: Option<bool>,
}

/// What the JavaScript check found.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct JsFinding {
    /// Sampled pages whose HTTP response looked like an app shell.
    pub suspects: usize,
    /// Pages sampled.
    pub sampled: usize,
    /// Pages rendered with a browser, and how many showed much more text.
    pub rendered: usize,
    pub revealed: usize,
    /// Words over HTTP and rendered, for the first page that revealed
    /// content (the reason quotes it).
    pub example: Option<(usize, usize)>,
}

impl JsFinding {
    /// Whether the crawl should render JavaScript: a rendered page showed
    /// much more text, or (when nothing could be rendered) most sampled
    /// pages look like app shells.
    pub(crate) fn needs_browser(&self) -> bool {
        if self.rendered > 0 {
            return self.revealed > 0;
        }
        self.sampled > 0 && self.suspects * 2 > self.sampled
    }
}

/// Why the config holds a value.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
pub(crate) struct SuggestReason {
    /// The config field (dotted path), e.g. `features.block_split`.
    pub field: String,
    pub reason: String,
}

/// What crawling with the config is expected to cost.
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub(crate) struct SuggestEstimate {
    /// Pages the map found.
    pub pages_found: usize,
    /// Pages the config crawls (`null` when the number of pages is not
    /// known: the sitemaps do not list them, or the site has more pages
    /// than a map lists).
    pub pages_to_crawl: Option<usize>,
    /// Whether pages are rendered with a browser (slower, billed higher).
    pub js_rendering: bool,
    /// Pages that get an AI summary (one LLM call each).
    pub ai_summary_pages: usize,
    /// Pages that get an AI extraction (one LLM call each).
    pub ai_extraction_pages: usize,
}

/// The proposal: a compact crawl config (only the fields it sets; every
/// other field takes its default), the reasons, the estimate and warnings.
#[derive(Debug, Clone)]
pub(crate) struct Proposal {
    pub config: Value,
    pub reasons: Vec<SuggestReason>,
    pub estimate: SuggestEstimate,
    pub warnings: Vec<String>,
}

/// The globs of a section's pages, on any scheme and under each language
/// segment. `None` for the sections no glob describes: top-level pages and
/// the merged smallest sections.
pub(crate) fn section_globs(
    key: &str,
    host: &str,
    base_path: &str,
    languages: &BTreeSet<String>,
) -> Option<Vec<String>> {
    if key == HOME_KEY || key == OTHER_KEY {
        return None;
    }
    let base = base_path.trim_end_matches('/');
    let segment = key.trim_matches('/');
    let prefixes = std::iter::once(String::new()).chain(languages.iter().map(|l| format!("/{l}")));
    Some(
        prefixes
            .flat_map(|lang| {
                let path = format!("*://{host}{base}{lang}/{segment}");
                [path.clone(), format!("{path}/**"), format!("{path}?**")]
            })
            .collect(),
    )
}

/// How a feature is limited to `chosen` among the crawled sections:
/// `None` when it applies to none, empty filters when it applies to all,
/// else `include_pages` (or, when a chosen section has no glob,
/// `exclude_pages` of the others).
fn scope(
    chosen: &[&SiteSection],
    crawled: &[&SiteSection],
    f: &Findings,
) -> Option<(Vec<String>, Vec<String>)> {
    if chosen.is_empty() {
        return None;
    }
    let globs = |s: &SiteSection| section_globs(&s.key, &f.authority, &f.base_path, &f.languages);
    if chosen.len() == crawled.len() {
        return Some((Vec::new(), Vec::new()));
    }
    if let Some(include) = chosen.iter().map(|s| globs(s)).collect::<Option<Vec<_>>>() {
        return Some((include.concat(), Vec::new()));
    }
    let others: Vec<&&SiteSection> = crawled
        .iter()
        .filter(|s| !chosen.iter().any(|c| c.key == s.key))
        .collect();
    match others.iter().map(|s| globs(s)).collect::<Option<Vec<_>>>() {
        Some(exclude) => Some((Vec::new(), exclude.concat())),
        // Neither side can be described: apply it everywhere.
        None => Some((Vec::new(), Vec::new())),
    }
}

/// `{ "enabled": true, "include_pages": …, "exclude_pages": … }` with the
/// empty filters left out.
fn toggle(include: Vec<String>, exclude: Vec<String>) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert("enabled".into(), Value::Bool(true));
    if !include.is_empty() {
        map.insert("include_pages".into(), json!(include));
    }
    if !exclude.is_empty() {
        map.insert("exclude_pages".into(), json!(exclude));
    }
    map
}

fn keys(sections: &[&SiteSection]) -> String {
    let keys: Vec<String> = sections
        .iter()
        .map(|s| match s.key.as_str() {
            HOME_KEY => "top-level pages".to_string(),
            OTHER_KEY => "other pages".to_string(),
            key => format!("`{key}`"),
        })
        .collect();
    keys.join(", ")
}

fn pages(sections: &[&SiteSection]) -> usize {
    sections.iter().map(|s| s.pages).sum()
}

/// The config for `f`.
pub(crate) fn propose(f: &Findings) -> Proposal {
    let mut reasons = Vec::new();
    let mut warnings = Vec::new();
    let mut reason = |field: &str, text: String| {
        reasons.push(SuggestReason {
            field: field.to_string(),
            reason: text,
        })
    };

    let crawled: Vec<&SiteSection> = f
        .sections
        .iter()
        .filter(|s| !s.kind.is_excluded())
        .collect();
    let skipped: Vec<&SiteSection> = f.sections.iter().filter(|s| s.kind.is_excluded()).collect();
    let of_kind = |kinds: &[SectionKind]| -> Vec<&SiteSection> {
        crawled
            .iter()
            .copied()
            .filter(|s| kinds.contains(&s.kind))
            .collect()
    };

    let mut config = Map::new();
    config.insert("start_urls".into(), json!([f.start_url]));
    config.insert(
        "index_uid".into(),
        json!(scrapix_core::config::url_to_index_uid(&format!(
            "{}{}",
            f.authority,
            f.base_path.trim_end_matches('/')
        ))),
    );

    // ── Rendering ────────────────────────────────────────────────────────
    let js = &f.js;
    if js.needs_browser() {
        config.insert("crawler_type".into(), json!("browser"));
        let text = match js.example {
            Some((http, rendered)) => format!(
                "pages need JavaScript: a sampled page shows {http} words over HTTP and {rendered} once rendered"
            ),
            None => format!(
                "{} of {} sampled pages look like JavaScript app shells (they could not be rendered to confirm)",
                js.suspects, js.sampled
            ),
        };
        reason("crawler_type", text);
        if js.rendered == 0 {
            warnings.push(
                "Browser rendering was not verified: try a few pages with /scrape and render_js \
                 before a full crawl"
                    .to_string(),
            );
        }
    } else if js.rendered > 0 {
        reason(
            "crawler_type",
            format!(
                "HTTP is enough: rendering {} suspicious page(s) with a browser showed no more text",
                js.rendered
            ),
        );
    } else {
        reason(
            "crawler_type",
            "HTTP is enough: sampled pages carry their text without JavaScript".to_string(),
        );
    }

    // ── Scope ────────────────────────────────────────────────────────────
    let mut url_patterns = Map::new();
    url_patterns.insert("allowed_domains".into(), json!([f.host]));
    if f.base_path != "/" {
        let base = f.base_path.trim_end_matches('/');
        url_patterns.insert(
            "include".into(),
            json!([
                format!("*://{}{base}", f.authority),
                format!("*://{}{base}/**", f.authority)
            ]),
        );
        reason(
            "url_patterns.include",
            format!("the crawl stays under `{}`", f.base_path),
        );
    }
    let mut exclude: Vec<String> = Vec::new();
    let excluded_named: Vec<&SiteSection> = skipped
        .iter()
        .copied()
        .filter(|s| section_globs(&s.key, &f.authority, &f.base_path, &f.languages).is_some())
        .collect();
    for s in &excluded_named {
        exclude.extend(
            section_globs(&s.key, &f.authority, &f.base_path, &f.languages).unwrap_or_default(),
        );
    }
    if !excluded_named.is_empty() {
        let what: Vec<String> = excluded_named
            .iter()
            .map(|s| format!("`{}` ({})", s.key, s.kind.as_str()))
            .collect();
        reason(
            "url_patterns.exclude",
            format!(
                "skips pages not worth searching: {} ({} pages)",
                what.join(", "),
                pages(&excluded_named)
            ),
        );
    }
    let commerce = of_kind(&[SectionKind::Product, SectionKind::Listing]);
    if !commerce.is_empty() && f.query_share < FACET_QUERY_SHARE {
        exclude.push(format!("*://{}/**?**", f.authority));
        reason(
            "url_patterns.exclude",
            "skips URLs with a query string: on a shop they are filters and sorting of the same \
             listings, and the site's own URLs do not use them"
                .to_string(),
        );
    }
    if !exclude.is_empty() {
        url_patterns.insert("exclude".into(), json!(exclude));
    }
    config.insert("url_patterns".into(), Value::Object(url_patterns));

    let to_crawl = pages(&crawled);
    let pages_to_crawl = (!f.map_truncated).then_some(to_crawl);
    match pages_to_crawl {
        Some(n) if n > 0 => {
            // A margin for pages the sitemap does not list.
            let cap = n + n / 10 + 10;
            config.insert("max_pages".into(), json!(cap));
            reason(
                "max_pages",
                format!("{n} pages found in the crawled sections, plus a 10% margin"),
            );
        }
        _ => warnings.push(
            "The number of pages is not known (the sitemaps do not list them, or the site has \
             more pages than a map lists): no max_pages was set"
                .to_string(),
        ),
    }

    // ── Features ─────────────────────────────────────────────────────────
    let mut features = Map::new();
    features.insert("metadata".into(), json!({"enabled": true}));
    features.insert("markdown".into(), json!({"enabled": true}));
    reason(
        "features.markdown",
        "every page is indexed as Markdown with its metadata (title, description, Open Graph)"
            .to_string(),
    );

    // Block split: long prose pages.
    let long_prose: Vec<&SiteSection> = crawled
        .iter()
        .copied()
        .filter(|s| {
            s.kind.is_prose()
                && s.avg_words.is_some_and(|w| w >= BLOCK_SPLIT_MIN_WORDS)
                && s.avg_headings
                    .is_some_and(|h| h >= BLOCK_SPLIT_MIN_HEADINGS)
        })
        .collect();
    match scope(&long_prose, &crawled, f) {
        Some((include, exclude)) => {
            features.insert(
                "block_split".into(),
                Value::Object(toggle(include, exclude)),
            );
            reason(
                "features.block_split",
                format!(
                    "{} hold long pages with headings (≥ {BLOCK_SPLIT_MIN_WORDS} words): split by heading so a search lands on the right part",
                    keys(&long_prose)
                ),
            );
        }
        None => reason(
            "features.block_split",
            "off: pages are short, have no headings to split at, or are items (one document per page)".to_string(),
        ),
    }

    // schema.org: the sections whose pages carry useful types.
    let with_schema: Vec<&SiteSection> = crawled
        .iter()
        .copied()
        .filter(|s| !s.schema_types.is_empty())
        .collect();
    let types: BTreeSet<String> = with_schema
        .iter()
        .flat_map(|s| s.schema_types.iter().cloned())
        .collect();
    if let Some((include, exclude)) = scope(&with_schema, &crawled, f) {
        let mut schema = toggle(include, exclude);
        schema.insert("only_types".into(), json!(types));
        let dated = has_dated_types(&types);
        if dated {
            schema.insert("convert_dates".into(), Value::Bool(true));
        }
        features.insert("schema".into(), Value::Object(schema));
        let types_list: Vec<String> = types.iter().map(|t| format!("`{t}`")).collect();
        reason(
            "features.schema",
            format!(
                "{} carry schema.org {}{}",
                keys(&with_schema),
                types_list.join(", "),
                if dated {
                    ": indexed as fields, dates as timestamps to sort and filter on"
                } else {
                    ": indexed as fields to filter and facet on"
                }
            ),
        );
    }

    // AI extraction: product pages without Product markup.
    let products = of_kind(&[SectionKind::Product]);
    let bare_products: Vec<&SiteSection> = products
        .iter()
        .copied()
        .filter(|s| {
            s.sampled > 0
                && !s
                    .schema_types
                    .iter()
                    .any(|t| matches!(t.as_str(), "Product" | "ProductGroup" | "Offer"))
        })
        .collect();
    let mut ai_extraction_pages = 0;
    // AI features cost one LLM call per page: proposed only when the page
    // count is known and small enough.
    let affordable = |n: usize| n <= AI_MAX_PAGES && !f.map_truncated;
    let ai_cost = |n: usize| {
        if f.map_truncated {
            "the number of pages is not known".to_string()
        } else {
            format!("{n} pages")
        }
    };
    if !bare_products.is_empty() {
        let n = pages(&bare_products);
        if affordable(n) {
            if let Some((include, exclude)) = scope(&bare_products, &crawled, f) {
                let mut extraction = toggle(include, exclude);
                extraction.insert(
                    "prompt".into(),
                    json!("Extract the product's name, brand, price, currency, availability, SKU and main category."),
                );
                features.insert("ai_extraction".into(), Value::Object(extraction));
                ai_extraction_pages = n;
                reason(
                    "features.ai_extraction",
                    format!(
                        "{} are product pages without schema.org `Product` markup: the AI extracts price, brand and availability ({n} LLM calls)",
                        keys(&bare_products)
                    ),
                );
            }
        } else {
            warnings.push(format!(
                "{} are product pages without schema.org `Product` markup ({}): set \
                 features.custom_selectors for price and brand, AI extraction would cost one \
                 LLM call per page",
                keys(&bare_products),
                ai_cost(n)
            ));
        }
    }

    // AI summary: long articles and docs.
    let summarizable: Vec<&SiteSection> = crawled
        .iter()
        .copied()
        .filter(|s| {
            matches!(s.kind, SectionKind::Article | SectionKind::Docs)
                && s.avg_words.is_some_and(|w| w >= AI_SUMMARY_MIN_WORDS)
        })
        .collect();
    let mut ai_summary_pages = 0;
    let wanted = f.ai_summary_hint.unwrap_or(true);
    if !summarizable.is_empty() && wanted {
        let n = pages(&summarizable);
        if affordable(n) {
            if let Some((include, exclude)) = scope(&summarizable, &crawled, f) {
                features.insert("ai_summary".into(), Value::Object(toggle(include, exclude)));
                ai_summary_pages = n;
                reason(
                    "features.ai_summary",
                    format!(
                        "{} hold long pages (≥ {AI_SUMMARY_MIN_WORDS} words): a short summary makes results clearer ({n} LLM calls)",
                        keys(&summarizable)
                    ),
                );
            }
        } else {
            warnings.push(format!(
                "{} hold long pages, but an AI summary is one LLM call per page ({}): enable \
                 features.ai_summary yourself if it is worth it",
                keys(&summarizable),
                ai_cost(n)
            ));
        }
    }

    // Documents linked from the pages.
    if f.pdf_links > 0 {
        features.insert("pdf".into(), json!({"enabled": true}));
        reason(
            "features.pdf",
            format!(
                "{} PDF link(s) on the sampled pages: PDFs are fetched and indexed as Markdown",
                f.pdf_links
            ),
        );
    }
    if f.document_links > 0 {
        features.insert("documents".into(), json!({"enabled": true}));
        reason(
            "features.documents",
            format!(
                "{} office document link(s) (Word, Excel, PowerPoint, …) on the sampled pages",
                f.document_links
            ),
        );
    }
    config.insert("features".into(), Value::Object(features));

    Proposal {
        config: Value::Object(config),
        reasons,
        estimate: SuggestEstimate {
            pages_found: f.pages_found,
            pages_to_crawl,
            js_rendering: js.needs_browser(),
            ai_summary_pages,
            ai_extraction_pages,
        },
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suggest::analysis::SiteType;
    use scrapix_core::config::CrawlConfig;
    use scrapix_core::url_glob::matches_glob;

    fn section(
        key: &str,
        pages: usize,
        kind: SectionKind,
        avg_words: Option<usize>,
        types: &[&str],
    ) -> SiteSection {
        SiteSection {
            key: key.to_string(),
            pages,
            kind,
            because: String::new(),
            schema_types: types.iter().map(|t| t.to_string()).collect(),
            avg_words,
            avg_headings: avg_words.map(|w| w / 150),
            sampled: usize::from(avg_words.is_some()),
            titles: Vec::new(),
        }
    }

    fn findings(_site_type: SiteType, sections: Vec<SiteSection>) -> Findings {
        Findings {
            start_url: "https://acme.test/".to_string(),
            host: "acme.test".to_string(),
            authority: "acme.test".to_string(),
            base_path: "/".to_string(),
            pages_found: sections.iter().map(|s| s.pages).sum(),
            sections,
            languages: BTreeSet::new(),
            map_truncated: false,
            query_share: 0.0,
            js: JsFinding {
                sampled: 6,
                ..Default::default()
            },
            pdf_links: 0,
            document_links: 0,
            ai_summary_hint: None,
        }
    }

    fn feature<'a>(p: &'a Proposal, name: &str) -> Option<&'a Value> {
        p.config.pointer(&format!("/features/{name}"))
    }

    fn deserializes(p: &Proposal) -> CrawlConfig {
        serde_json::from_value(p.config.clone()).expect("a valid CrawlConfig")
    }

    #[test]
    fn a_shop_gets_product_schema_and_no_block_split() {
        let p = propose(&findings(
            SiteType::Ecommerce,
            vec![
                section(
                    "/products/",
                    900,
                    SectionKind::Product,
                    Some(120),
                    &["Product", "Offer", "BreadcrumbList"],
                ),
                section(
                    "/collections/",
                    60,
                    SectionKind::Listing,
                    Some(80),
                    &["BreadcrumbList"],
                ),
                section("/", 20, SectionKind::Marketing, Some(200), &[]),
                section("/cart/", 5, SectionKind::Account, None, &[]),
            ],
        ));
        let config = deserializes(&p);
        assert_eq!(config.crawler_type, scrapix_core::config::CrawlerType::Http);
        assert!(feature(&p, "block_split").is_none());
        let schema = feature(&p, "schema").unwrap();
        assert_eq!(
            schema["only_types"],
            json!(["BreadcrumbList", "Offer", "Product"])
        );
        // Products and collections carry types, home does not.
        let included = &config.features.schema.as_ref().unwrap().include_pages;
        assert!(included
            .iter()
            .any(|g| matches_glob("https://acme.test/products/shoe", g)));
        assert!(included
            .iter()
            .any(|g| matches_glob("https://acme.test/collections/men", g)));
        assert!(!included
            .iter()
            .any(|g| matches_glob("https://acme.test/about", g)));
        // Cart and facet URLs are skipped.
        let exclude = &config.url_patterns.exclude;
        assert!(exclude
            .iter()
            .any(|g| matches_glob("https://acme.test/cart/x", g)));
        assert!(exclude
            .iter()
            .any(|g| matches_glob("https://acme.test/collections/shoes?sort=price", g)));
        assert!(!exclude
            .iter()
            .any(|g| matches_glob("https://acme.test/products/shoe", g)));
        assert_eq!(config.max_pages, Some(980 + 98 + 10));
        assert!(feature(&p, "ai_extraction").is_none());
    }

    #[test]
    fn a_mixed_site_scopes_each_feature_to_its_sections() {
        let p = propose(&findings(
            SiteType::Mixed,
            vec![
                section("/docs/", 300, SectionKind::Docs, Some(1200), &[]),
                section(
                    "/blog/",
                    120,
                    SectionKind::Article,
                    Some(900),
                    &["BlogPosting"],
                ),
                section(
                    "/products/",
                    40,
                    SectionKind::Product,
                    Some(150),
                    &["Product"],
                ),
                section("/", 30, SectionKind::Marketing, Some(250), &[]),
            ],
        ));
        let config = deserializes(&p);
        let block = config.features.block_split.as_ref().unwrap();
        assert!(block.enabled);
        assert!(block
            .include_pages
            .iter()
            .any(|g| matches_glob("https://acme.test/docs/install", g)));
        assert!(block
            .include_pages
            .iter()
            .any(|g| matches_glob("http://acme.test/blog/post", g)));
        assert!(!block
            .include_pages
            .iter()
            .any(|g| matches_glob("https://acme.test/products/x", g)));

        let schema = config.features.schema.as_ref().unwrap();
        assert!(schema.convert_dates);
        assert!(schema
            .include_pages
            .iter()
            .any(|g| matches_glob("https://acme.test/products/x", g)));
        assert!(!schema
            .include_pages
            .iter()
            .any(|g| matches_glob("https://acme.test/docs/x", g)));

        let summary = config.features.ai_summary.as_ref().unwrap();
        assert!(summary
            .include_pages
            .iter()
            .any(|g| matches_glob("https://acme.test/docs/x", g)));
        assert_eq!(p.estimate.ai_summary_pages, 420);
        assert!(p.reasons.iter().any(|r| r.field == "features.ai_summary"));
    }

    #[test]
    fn the_ai_can_turn_summaries_off_and_large_sites_get_a_warning() {
        let docs = || vec![section("/docs/", 300, SectionKind::Docs, Some(1500), &[])];
        let mut f = findings(SiteType::Docs, docs());
        f.ai_summary_hint = Some(false);
        assert!(feature(&propose(&f), "ai_summary").is_none());

        let mut unknown = findings(SiteType::Docs, docs());
        unknown.map_truncated = true;
        let p = propose(&unknown);
        assert!(feature(&p, "ai_summary").is_none());
        assert!(p
            .warnings
            .iter()
            .any(|w| w.contains("not known") && w.contains("ai_summary")));

        let f = findings(
            SiteType::Docs,
            vec![section("/docs/", 5000, SectionKind::Docs, Some(1500), &[])],
        );
        let p = propose(&f);
        assert!(feature(&p, "ai_summary").is_none());
        assert!(p.warnings.iter().any(|w| w.contains("ai_summary")));
    }

    #[test]
    fn products_without_markup_get_an_ai_extraction() {
        let p = propose(&findings(
            SiteType::Ecommerce,
            vec![
                section("/p/", 200, SectionKind::Product, Some(100), &[]),
                section("/", 10, SectionKind::Marketing, Some(100), &[]),
            ],
        ));
        let config = deserializes(&p);
        let extraction = config.features.ai_extraction.as_ref().unwrap();
        assert!(extraction
            .include_pages
            .iter()
            .any(|g| matches_glob("https://acme.test/p/123", g)));
        assert_eq!(p.estimate.ai_extraction_pages, 200);
    }

    #[test]
    fn javascript_rendering_needs_evidence() {
        let mut f = findings(
            SiteType::Marketing,
            vec![section("/", 10, SectionKind::Marketing, Some(20), &[])],
        );
        f.js = JsFinding {
            suspects: 4,
            sampled: 6,
            rendered: 2,
            revealed: 1,
            example: Some((12, 640)),
        };
        let p = propose(&f);
        assert_eq!(p.config["crawler_type"], "browser");
        assert!(p.reasons.iter().any(|r| r.reason.contains("640")));

        f.js = JsFinding {
            suspects: 4,
            sampled: 6,
            rendered: 2,
            revealed: 0,
            example: None,
        };
        assert!(propose(&f).config.get("crawler_type").is_none());

        // Not rendered: a majority of shells is enough, with a warning.
        f.js = JsFinding {
            suspects: 4,
            sampled: 6,
            rendered: 0,
            revealed: 0,
            example: None,
        };
        let p = propose(&f);
        assert_eq!(p.config["crawler_type"], "browser");
        assert!(!p.warnings.is_empty());
    }

    #[test]
    fn documents_base_path_languages_and_truncated_maps() {
        let mut f = findings(
            SiteType::Docs,
            vec![
                section("/guides/", 50, SectionKind::Docs, Some(500), &[]),
                section("/legal/", 3, SectionKind::Legal, None, &[]),
            ],
        );
        f.base_path = "/docs/".to_string();
        f.languages = ["fr".to_string()].into();
        f.pdf_links = 3;
        f.document_links = 1;
        f.map_truncated = true;
        let p = propose(&f);
        let config = deserializes(&p);
        assert!(config.features.is_pdf_enabled());
        assert!(config.features.is_documents_enabled());
        assert!(config.max_pages.is_none());
        assert!(p.estimate.pages_to_crawl.is_none());
        let include = &config.url_patterns.include;
        assert!(include
            .iter()
            .any(|g| matches_glob("https://acme.test/docs/guides/x", g)));
        assert!(!include
            .iter()
            .any(|g| matches_glob("https://acme.test/blog/x", g)));
        let exclude = &config.url_patterns.exclude;
        assert!(exclude
            .iter()
            .any(|g| matches_glob("https://acme.test/docs/fr/legal/terms", g)));
        assert!(exclude
            .iter()
            .any(|g| matches_glob("https://acme.test/docs/legal", g)));
        assert!(!exclude
            .iter()
            .any(|g| matches_glob("https://acme.test/docs/legality-guide", g)));
        assert_eq!(config.index_uid, "acme-test-docs");
    }
}
