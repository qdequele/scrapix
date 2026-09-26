//! Job-scoped settings shared by pipeline messages.

use crate::config::{CrawlConfig, CrawlerType, MeilisearchSettings, ProxyConfig, SitemapConfig};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Job-scoped settings that workers need but that are not per-URL.
///
/// Travels on every `UrlMessage`/`RawPageMessage` (a few hundred bytes) so
/// that job configuration is not lost as messages flow through the
/// distributed pipeline (frontier, crawler, content workers).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobSpec {
    #[serde(default)]
    pub crawler_type: CrawlerType,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub user_agents: Vec<String>,
    #[serde(default)]
    pub proxy: Option<ProxyConfig>,
    #[serde(default = "default_true")]
    pub respect_robots_txt: bool,
    #[serde(default)]
    pub per_domain_delay_ms: u64,
    #[serde(default)]
    pub default_crawl_delay_ms: u64,
    #[serde(default)]
    pub requests_per_second: Option<f64>,
    #[serde(default)]
    pub max_concurrent_requests: Option<u32>,
    #[serde(default)]
    pub sitemap: SitemapConfig,
    #[serde(default)]
    pub index_only: Vec<String>,
    #[serde(default)]
    pub primary_key: Option<String>,
    #[serde(default)]
    pub batch_size: Option<u32>,
    #[serde(default)]
    pub index_settings: Option<MeilisearchSettings>,
    #[serde(default)]
    pub keep_settings: bool,
}

fn default_true() -> bool {
    true
}

/// Matches the serde defaults (an empty `{}` deserializes to this), so a
/// `JobSpec { ..Default::default() }` respects robots.txt like a job that
/// never set `rate_limit.respect_robots_txt`.
impl Default for JobSpec {
    fn default() -> Self {
        Self {
            crawler_type: CrawlerType::default(),
            headers: HashMap::new(),
            user_agents: Vec::new(),
            proxy: None,
            respect_robots_txt: true,
            per_domain_delay_ms: 0,
            default_crawl_delay_ms: 0,
            requests_per_second: None,
            max_concurrent_requests: None,
            sitemap: SitemapConfig::default(),
            index_only: Vec::new(),
            primary_key: None,
            batch_size: None,
            index_settings: None,
            keep_settings: false,
        }
    }
}

impl JobSpec {
    /// Build a `JobSpec` from a job's `CrawlConfig`, mapping every field that
    /// downstream workers need but that isn't per-URL.
    pub fn from_config(c: &CrawlConfig) -> Self {
        let rps = c
            .rate_limit
            .requests_per_second
            .or(c.rate_limit.requests_per_minute.map(|m| m as f64 / 60.0));
        Self {
            crawler_type: c.crawler_type.clone(),
            headers: c.headers.clone(),
            user_agents: c.user_agents.clone(),
            proxy: c.proxy.clone(),
            respect_robots_txt: c.rate_limit.respect_robots_txt,
            per_domain_delay_ms: c.rate_limit.per_domain_delay_ms,
            default_crawl_delay_ms: c.rate_limit.default_crawl_delay_ms,
            requests_per_second: rps,
            max_concurrent_requests: Some(c.concurrency.max_concurrent_requests).filter(|n| *n > 0),
            sitemap: c.sitemap.clone(),
            index_only: c.url_patterns.index_only.clone(),
            primary_key: c.meilisearch.primary_key.clone(),
            batch_size: Some(c.meilisearch.batch_size).filter(|n| *n > 0),
            index_settings: c.meilisearch.settings.clone(),
            keep_settings: c.meilisearch.keep_settings,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_config_maps_headers_user_agents_rate_limit_and_primary_key() {
        let mut config = CrawlConfig {
            start_urls: vec!["https://example.com".to_string()],
            ..default_crawl_config()
        };
        config
            .headers
            .insert("X-Api-Key".to_string(), "secret".to_string());
        config.user_agents = vec!["MyBot/1.0".to_string()];
        config.rate_limit.requests_per_minute = Some(120);
        config.meilisearch.primary_key = Some("uid".to_string());

        let spec = JobSpec::from_config(&config);

        assert_eq!(spec.headers.get("X-Api-Key"), Some(&"secret".to_string()));
        assert_eq!(spec.user_agents, vec!["MyBot/1.0".to_string()]);
        assert_eq!(spec.requests_per_second, Some(2.0));
        assert_eq!(spec.primary_key, Some("uid".to_string()));
    }

    #[test]
    fn default_matches_empty_json_and_respects_robots() {
        let from_json: JobSpec = serde_json::from_str("{}").unwrap();
        assert_eq!(JobSpec::default(), from_json);
        assert!(JobSpec::default().respect_robots_txt);
    }

    #[test]
    fn from_config_default_config_yields_default_respect_robots_txt() {
        let config = default_crawl_config();
        let spec = JobSpec::from_config(&config);

        assert_eq!(
            spec.respect_robots_txt,
            crate::config::RateLimitConfig::default().respect_robots_txt
        );
    }

    /// R-12: a job built from a `CrawlConfig` that never set a `sitemap`
    /// section (the common case — the field defaults on `CrawlConfig`
    /// itself, exercised here via `default_crawl_config()`) still carries a
    /// `JobSpec` with sitemap discovery enabled, so it behaves like a job
    /// with no `JobSpec` at all used to (worker-level `SITEMAP_DISCOVERY`
    /// default, which is on).
    #[test]
    fn from_config_without_sitemap_section_still_enables_discovery() {
        let config = default_crawl_config();
        let spec = JobSpec::from_config(&config);

        assert!(spec.sitemap.enabled);
        assert!(spec.sitemap.urls.is_empty());
    }

    fn default_crawl_config() -> CrawlConfig {
        CrawlConfig {
            start_urls: vec!["https://example.com".to_string()],
            source: None,
            index_uid: String::new(),
            crawler_type: CrawlerType::default(),
            max_depth: None,
            max_pages: None,
            url_patterns: Default::default(),
            allowed_domains: Vec::new(),
            sitemap: SitemapConfig::default(),
            concurrency: Default::default(),
            rate_limit: Default::default(),
            proxy: None,
            features: Default::default(),
            meilisearch: Default::default(),
            webhooks: Vec::new(),
            headers: HashMap::new(),
            user_agents: Vec::new(),
            index_strategy: Default::default(),
        }
    }
}
