//! Meilisearch connection targets and how the engine picks one.

/// Where documents are indexed / searched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeiliTarget {
    pub url: String,
    pub api_key: Option<String>,
}
