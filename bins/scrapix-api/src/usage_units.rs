//! Raw unit counters reported on usage events.
//!
//! Permanent: these are units the Lab stores (and a future `pricing.yml`
//! prices), independent of the transition-release `credits` field. The
//! pre-v2 credit formula (`legacy_credits`) reuses them; deleting that
//! module leaves these intact.

use crate::ScrapeFormat;
use scrapix_core::FeaturesConfig;

/// Feature formats requested for one page of `/scrape`, a document scrape
/// or a `/parse` upload: `markdown`, `links`, `metadata`, `screenshot`,
/// `schema`, `blocks`. Base formats (`html`, `rawhtml`, `content`) are not
/// counted. Reported as that page's `feature_pages` unit.
pub(crate) fn feature_format_count(formats: &[ScrapeFormat]) -> u64 {
    formats
        .iter()
        .filter(|f| {
            matches!(
                f,
                ScrapeFormat::Markdown
                    | ScrapeFormat::Links
                    | ScrapeFormat::Metadata
                    | ScrapeFormat::Screenshot
                    | ScrapeFormat::Schema
                    | ScrapeFormat::Blocks
            )
        })
        .count() as u64
}

/// Enabled non-AI page features of a crawl (`metadata`, `markdown`,
/// `block_split`, `schema`, `custom_selectors`). A crawl reports
/// `feature_pages = (pages_http + pages_browser) × this`.
pub(crate) fn page_feature_count(features: &FeaturesConfig) -> u64 {
    [
        features.metadata.as_ref().is_some_and(|f| f.enabled),
        features.markdown.as_ref().is_some_and(|f| f.enabled),
        features.block_split.as_ref().is_some_and(|f| f.enabled),
        features.schema.as_ref().is_some_and(|s| s.enabled),
        features
            .custom_selectors
            .as_ref()
            .is_some_and(|s| s.enabled),
    ]
    .into_iter()
    .filter(|on| *on)
    .count() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_formats_are_not_feature_formats() {
        assert_eq!(feature_format_count(&[]), 0);
        assert_eq!(
            feature_format_count(&[
                ScrapeFormat::Html,
                ScrapeFormat::RawHtml,
                ScrapeFormat::Content
            ]),
            0
        );
        assert_eq!(
            feature_format_count(&[
                ScrapeFormat::Markdown,
                ScrapeFormat::Links,
                ScrapeFormat::Metadata,
                ScrapeFormat::Screenshot,
                ScrapeFormat::Schema,
                ScrapeFormat::Blocks,
                ScrapeFormat::Html,
            ]),
            6
        );
    }

    #[test]
    fn page_features_count_only_enabled_non_ai_features() {
        assert_eq!(page_feature_count(&FeaturesConfig::default()), 0);
        let f = FeaturesConfig::from_cli_args(true, true, true, true, false, false, None);
        assert_eq!(page_feature_count(&f), 4);
        let ai_only = FeaturesConfig::from_cli_args(
            false,
            false,
            false,
            false,
            true,
            true,
            Some("extract".to_string()),
        );
        assert_eq!(
            page_feature_count(&ai_only),
            0,
            "AI features are not counted"
        );
    }
}
