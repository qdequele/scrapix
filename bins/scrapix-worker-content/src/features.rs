//! Per-page feature resolution: page filters (`include_pages` /
//! `exclude_pages`), `url_patterns.index_only`, and per-job schema options.

use scrapix_core::url_glob::{matches_glob, matches_include_exclude};
use scrapix_core::FeaturesConfig;
use scrapix_extractor::{merge_microdata, SchemaConfig, SchemaExtractor, SchemaItem};
use std::sync::LazyLock;

/// Whether a feature with these page filters applies to `url`.
///
/// Same glob semantics as the crawler's URL patterns
/// ([`scrapix_core::url_glob`]): exclude wins, an empty include list
/// matches every page.
pub fn feature_applies(pages_include: &[String], pages_exclude: &[String], url: &str) -> bool {
    matches_include_exclude(url, pages_include, pages_exclude)
}

/// Whether `url` should be indexed under the job's `url_patterns.index_only`
/// (empty = index every page).
pub fn index_only_allows(index_only: &[String], url: &str) -> bool {
    index_only.is_empty() || index_only.iter().any(|p| matches_glob(url, p))
}

/// The job's features as they apply to one page: every feature whose page
/// filters exclude `url` is turned off.
pub fn page_features(features: &FeaturesConfig, url: &str) -> FeaturesConfig {
    let mut f = features.clone();
    for toggle in [
        f.metadata.as_mut(),
        f.markdown.as_mut(),
        f.block_split.as_mut(),
        f.ai_summary.as_mut(),
    ]
    .into_iter()
    .flatten()
    {
        if !feature_applies(&toggle.include_pages, &toggle.exclude_pages, url) {
            toggle.enabled = false;
        }
    }
    if let Some(s) = f.schema.as_mut() {
        if !feature_applies(&s.include_pages, &s.exclude_pages, url) {
            s.enabled = false;
        }
    }
    if let Some(c) = f.custom_selectors.as_mut() {
        if !feature_applies(&c.include_pages, &c.exclude_pages, url) {
            c.enabled = false;
        }
    }
    if let Some(a) = f.ai_extraction.as_mut() {
        if !feature_applies(&a.include_pages, &a.exclude_pages, url) {
            a.enabled = false;
        }
    }
    f
}

/// Cache key of a job's schema extractor: `(sorted only_types, convert_dates)`,
/// or `None` when the job uses the parser's default schema output.
pub fn schema_key(features: &FeaturesConfig) -> Option<(Vec<String>, bool)> {
    let schema = features.schema.as_ref().filter(|s| s.enabled)?;
    if schema.only_types.is_empty() && !schema.convert_dates {
        return None;
    }
    let mut types = schema.only_types.clone();
    types.sort();
    types.dedup();
    Some((types, schema.convert_dates))
}

/// Build the schema extractor for a [`schema_key`].
pub fn schema_extractor(key: &(Vec<String>, bool)) -> SchemaExtractor {
    SchemaExtractor::new(SchemaConfig {
        only_types: key.0.iter().cloned().collect(),
        convert_dates: key.1,
        ..Default::default()
    })
}

/// `schema_from_dom` on HTML text (one more DOM parse; tests only).
#[cfg(test)]
pub fn extract_schema(extractor: &SchemaExtractor, html: &str) -> Option<serde_json::Value> {
    schema_from_dom(
        extractor,
        &scrapix_extractor::scraper::Html::parse_document(html),
    )
}

/// The document `schema` field for a page, extracted from its parsed DOM
/// with the job's schema options: the matching JSON-LD items, then the
/// Microdata items of a type the JSON-LD does not have (one object, or an
/// array), or `None`.
pub fn schema_from_dom(
    extractor: &SchemaExtractor,
    dom: &scrapix_extractor::scraper::Html,
) -> Option<serde_json::Value> {
    let extracted = extractor.extract_from_dom(dom).ok()?;
    let mut values: Vec<serde_json::Value> =
        extracted.items.into_iter().map(|item| item.data).collect();
    let json_ld = match values.len() {
        0 => None,
        1 => values.pop(),
        _ => Some(serde_json::Value::Array(values)),
    };
    merge_microdata(json_ld, extracted.microdata)
}

/// The page's Microdata items, for a job without schema options: the parser
/// collects its JSON-LD, and [`merge_microdata`] adds these to it.
pub fn microdata_from_dom(dom: &scrapix_extractor::scraper::Html) -> Vec<SchemaItem> {
    static DEFAULT: LazyLock<SchemaExtractor> = LazyLock::new(SchemaExtractor::default);
    DEFAULT.microdata_from_dom(dom)
}

#[cfg(test)]
mod tests {
    use super::*;
    use scrapix_core::{FeatureToggle, SchemaFeatureConfig};

    #[test]
    fn feature_page_filters() {
        assert!(feature_applies(&[], &[], "https://a.test/x"));
        assert!(feature_applies(
            &["https://a.test/docs/*".into()],
            &[],
            "https://a.test/docs/1"
        ));
        assert!(!feature_applies(
            &["https://a.test/docs/*".into()],
            &[],
            "https://a.test/blog/1"
        ));
        assert!(!feature_applies(
            &[],
            &["https://a.test/docs/private/*".into()],
            "https://a.test/docs/private/k"
        ));
    }

    #[test]
    fn index_only_empty_allows_all() {
        assert!(index_only_allows(&[], "https://a.test/x"));
        let only = vec!["https://a.test/docs/*".to_string()];
        assert!(index_only_allows(&only, "https://a.test/docs/1"));
        assert!(!index_only_allows(&only, "https://a.test/blog/1"));
    }

    #[test]
    fn page_features_disables_filtered_features_only() {
        let features = FeaturesConfig {
            markdown: Some(FeatureToggle {
                enabled: true,
                include_pages: vec!["https://a.test/docs/*".into()],
                exclude_pages: vec![],
            }),
            metadata: Some(FeatureToggle {
                enabled: true,
                include_pages: vec![],
                exclude_pages: vec![],
            }),
            ..Default::default()
        };
        let blog = page_features(&features, "https://a.test/blog/1");
        assert!(!blog.markdown_enabled());
        assert!(blog.metadata_enabled());
        let docs = page_features(&features, "https://a.test/docs/1");
        assert!(docs.markdown_enabled());
    }

    #[test]
    fn schema_only_types_filters_items() {
        let features = FeaturesConfig {
            schema: Some(SchemaFeatureConfig {
                enabled: true,
                only_types: vec!["Product".into()],
                convert_dates: false,
                include_pages: vec![],
                exclude_pages: vec![],
            }),
            ..Default::default()
        };
        let key = schema_key(&features).expect("custom schema options");
        let html = r#"<html><head>
            <script type="application/ld+json">{"@type":"Article","headline":"h"}</script>
            <script type="application/ld+json">{"@type":"Product","name":"p"}</script>
            </head><body></body></html>"#;
        let schema = extract_schema(&schema_extractor(&key), html).unwrap();
        assert_eq!(schema["@type"], "Product");
        assert_eq!(schema["name"], "p");

        let default = FeaturesConfig::from_cli_args(true, true, true, false, false, false, None);
        assert!(schema_key(&default).is_none());
    }

    /// A job with schema options gets the page's Microdata too: filtered by
    /// `only_types`, and not when the JSON-LD already has that type.
    #[test]
    fn schema_options_apply_to_microdata() {
        let html = r#"<html><head>
            <script type="application/ld+json">{"@type":"Product","name":"json-ld"}</script>
            </head><body>
            <div itemscope itemtype="https://schema.org/Product"><b itemprop="name">dup</b></div>
            <div itemscope itemtype="https://schema.org/Recipe"><b itemprop="name">soup</b></div>
            <div itemscope itemtype="https://schema.org/Person"><b itemprop="name">Ada</b></div>
            </body></html>"#;
        let extractor = schema_extractor(&(vec!["Product".into(), "Recipe".into()], false));
        assert_eq!(
            extract_schema(&extractor, html).unwrap(),
            serde_json::json!([
                {"@type": "Product", "name": "json-ld"},
                {"@type": "Recipe", "name": "soup"}
            ])
        );
    }
}
