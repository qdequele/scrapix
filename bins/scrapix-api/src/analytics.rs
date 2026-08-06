//! ClickHouse analytics backend for the crawl engine.
//!
//! The Tinybird-style pipes API (`/analytics/v0/pipes/*`) moved to the Rails
//! app (`saas/app/controllers/analytics_controller.rb`, SCR-85 phase 3). The
//! engine keeps the ClickHouse connection: it *writes* the analytics events
//! (request/page/job batchers in `lib.rs`) and reads page-event history for
//! `GET /job/{id}/events/history`.

use scrapix_storage::clickhouse::{ClickHouseConfig, ClickHouseStorage};
use tracing::info;

/// Analytics configuration
#[derive(Debug, Clone)]
pub struct AnalyticsConfig {
    pub clickhouse_url: String,
    pub clickhouse_database: String,
    pub clickhouse_user: Option<String>,
    pub clickhouse_password: Option<String>,
}

impl AnalyticsConfig {
    /// Create from environment variables
    pub fn from_env() -> Option<Self> {
        let url = std::env::var("CLICKHOUSE_URL").ok()?;
        Some(Self {
            clickhouse_url: url,
            clickhouse_database: std::env::var("CLICKHOUSE_DATABASE")
                .unwrap_or_else(|_| "scrapix".to_string()),
            clickhouse_user: std::env::var("CLICKHOUSE_USER").ok(),
            clickhouse_password: std::env::var("CLICKHOUSE_PASSWORD").ok(),
        })
    }
}

/// Shared ClickHouse state (event persistence + history queries)
pub struct AnalyticsState {
    /// ClickHouse storage client (public for sharing with event persistence)
    pub storage: ClickHouseStorage,
}

impl AnalyticsState {
    /// Create analytics state with provided storage
    pub fn with_storage(storage: ClickHouseStorage) -> Self {
        Self { storage }
    }

    /// Create analytics state from config
    #[allow(dead_code)]
    pub async fn new(config: AnalyticsConfig) -> Result<Self, String> {
        let ch_config = ClickHouseConfig {
            url: config.clickhouse_url,
            database: config.clickhouse_database,
            username: config.clickhouse_user,
            password: config.clickhouse_password,
            auto_create_tables: true,
            ..Default::default()
        };

        let storage = ClickHouseStorage::new(ch_config)
            .await
            .map_err(|e| format!("Failed to connect to ClickHouse: {}", e))?;

        info!("Analytics backend connected to ClickHouse");
        Ok(Self { storage })
    }
}
