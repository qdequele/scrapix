//! Per-page history for incremental crawling.
//!
//! After a successful fetch the crawler records, per `(index_uid, url)`, the
//! page's validators (`ETag`, `Last-Modified`), its final URL and its
//! in-scope links (after the job's filters, capped). The next
//! crawl of that index sends them as conditional headers; a page that
//! answers 304 Not Modified is not downloaded again, but its stored links
//! are followed, so an unchanged page never cuts the crawl off from the
//! pages below it.
//!
//! A record without links (written before links were stored) is never
//! used for a conditional request: the page is fetched in full once, which
//! stores them.

use std::collections::HashMap;
use std::time::Duration;

use parking_lot::Mutex;
use scrapix_core::{Result, ScrapixError};
use scrapix_storage::RedisStorage;
use serde::{Deserialize, Serialize};

/// What the crawler remembers about a page between crawls.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PageRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<String>,
    /// Epoch millis of the fetch that wrote the record.
    #[serde(default)]
    pub crawled_at: i64,
    /// Where the fetch landed after redirects: the page the stored links
    /// were found on, and the URL they are judged from on a 304.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_url: Option<String>,
    /// Key of the link filters the stored links passed (the crawler's
    /// `link_scope_key`). The record is used for a conditional request only
    /// by a job with the same key. `None` for a record written before keys
    /// were stored (never matches).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_scope: Option<String>,
    /// The page's in-scope links (the job's domain, include/exclude and
    /// non-page filters, any depth), at most
    /// `scrapix_crawler::MAX_STORED_LINKS`
    /// (`UrlExtractor::extract_with_outlinks`). `None` for a record written
    /// before links were stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outlinks: Option<Vec<String>>,
}

impl PageRecord {
    /// Whether a conditional request may be sent for this page: it has a
    /// validator and its outlinks are known (a 304 must not lose them).
    pub fn usable_for_conditional_request(&self) -> bool {
        (self.etag.is_some() || self.last_modified.is_some()) && self.outlinks.is_some()
    }
}

/// Where page records live: Redis in production, memory in tests.
pub enum PageHistory {
    Redis {
        storage: Box<RedisStorage>,
        ttl: Duration,
    },
    Memory(Mutex<HashMap<String, PageRecord>>),
}

/// Records expire after 30 days without a re-crawl.
const DEFAULT_TTL: Duration = Duration::from_secs(86400 * 30);

impl PageHistory {
    pub fn redis(storage: RedisStorage) -> Self {
        Self::Redis {
            storage: Box::new(storage),
            ttl: DEFAULT_TTL,
        }
    }

    pub fn memory() -> Self {
        Self::Memory(Mutex::new(HashMap::new()))
    }

    /// Stable key of `(index_uid, url)`.
    fn key(index_uid: &str, url: &str) -> String {
        let id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, url.as_bytes());
        format!("crawl_page_history:{index_uid}:{}", id.simple())
    }

    pub async fn get(&self, index_uid: &str, url: &str) -> Result<Option<PageRecord>> {
        let key = Self::key(index_uid, url);
        match self {
            Self::Redis { storage, .. } => match storage.get(&key).await? {
                Some(json) => serde_json::from_str(&json).map(Some).map_err(|e| {
                    ScrapixError::Storage(format!("Failed to deserialize page history: {e}"))
                }),
                None => Ok(None),
            },
            Self::Memory(map) => Ok(map.lock().get(&key).cloned()),
        }
    }

    pub async fn save(&self, index_uid: &str, url: &str, record: &PageRecord) -> Result<()> {
        let key = Self::key(index_uid, url);
        match self {
            Self::Redis { storage, ttl } => {
                let json = serde_json::to_string(record).map_err(|e| {
                    ScrapixError::Storage(format!("Failed to serialize page history: {e}"))
                })?;
                storage.set(&key, &json, Some(*ttl)).await
            }
            Self::Memory(map) => {
                map.lock().insert(key, record.clone());
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn records_round_trip_per_index_and_url() {
        let h = PageHistory::memory();
        let rec = PageRecord {
            etag: Some("\"e\"".into()),
            outlinks: Some(vec!["https://a.test/x".into()]),
            ..Default::default()
        };
        h.save("idx", "https://a.test/", &rec).await.unwrap();
        assert_eq!(h.get("idx", "https://a.test/").await.unwrap(), Some(rec));
        assert_eq!(h.get("other", "https://a.test/").await.unwrap(), None);
    }

    #[test]
    fn a_record_without_outlinks_is_not_used_for_conditional_requests() {
        let old: PageRecord = serde_json::from_str(r#"{"etag":"\"e\"","crawled_at":1}"#).unwrap();
        assert!(!old.usable_for_conditional_request());
        let new = PageRecord {
            outlinks: Some(vec![]),
            ..old
        };
        assert!(new.usable_for_conditional_request());
        assert!(!PageRecord {
            outlinks: Some(vec![]),
            ..Default::default()
        }
        .usable_for_conditional_request());
    }
}
