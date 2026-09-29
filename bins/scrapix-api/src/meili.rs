//! Meilisearch connection targets and how the engine picks one.

use crate::ApiError;
use scrapix_core::MeilisearchConfig; // the crawl config's `meilisearch` block type

/// Where documents are indexed / searched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeiliTarget {
    pub url: String,
    pub api_key: Option<String>,
}

/// Compares two Meilisearch URLs ignoring a trailing slash.
pub fn same_url(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

/// Resolves which Meilisearch instance a crawl/search/results request
/// should use, in standalone mode (env-configured) or hosted mode (the
/// account's rows in the Rails-owned `meilisearch_engines` table).
#[async_trait::async_trait]
pub(crate) trait MeilisearchResolver: Send + Sync {
    /// The default target for `account` (None when unauthenticated/standalone).
    async fn default_target(
        &self,
        account_id: Option<&str>,
    ) -> Result<Option<MeiliTarget>, ApiError>;
    /// A known target whose URL matches `url` (to recover its key); None if unknown.
    async fn target_for_url(
        &self,
        account_id: Option<&str>,
        url: &str,
    ) -> Result<Option<MeiliTarget>, ApiError>;
    /// Message for the 400 returned when a crawl has no target.
    fn missing_message(&self) -> &'static str;
}

/// Standalone: the server's own `MEILISEARCH_URL` / `MEILISEARCH_API_KEY`.
pub struct EnvResolver(pub Option<MeiliTarget>);

#[async_trait::async_trait]
impl MeilisearchResolver for EnvResolver {
    async fn default_target(&self, _: Option<&str>) -> Result<Option<MeiliTarget>, ApiError> {
        Ok(self.0.clone())
    }
    async fn target_for_url(
        &self,
        _: Option<&str>,
        url: &str,
    ) -> Result<Option<MeiliTarget>, ApiError> {
        Ok(self.0.clone().filter(|t| same_url(&t.url, url)))
    }
    fn missing_message(&self) -> &'static str {
        "No Meilisearch configured: set MEILISEARCH_URL or pass meilisearch.url"
    }
}

/// Hosted: the account's rows in the Rails-owned `meilisearch_engines`
/// table, falling back to the operator's own server only when its URL
/// matches the one being looked up (never as a tenant's default — a
/// tenant's crawl must not land in the operator's Meilisearch).
pub struct EngineTableResolver {
    pub pool: sqlx::PgPool,
    pub server: Option<MeiliTarget>,
}

fn db_err(e: sqlx::Error) -> ApiError {
    ApiError::new(format!("Database error: {e}"), "internal_error")
}

fn row_target(row: sqlx::postgres::PgRow) -> MeiliTarget {
    use sqlx::Row as _;
    let api_key: Option<String> = row.try_get("api_key").ok();
    MeiliTarget {
        url: row.get("url"),
        api_key: api_key.filter(|k| !k.is_empty()),
    }
}

/// Table-miss decision, factored out so it can be unit tested without a
/// database: fall back to the operator's own server only when its URL is
/// the one being looked up.
fn server_fallback(server: &Option<MeiliTarget>, url: &str) -> Option<MeiliTarget> {
    server.as_ref().filter(|t| same_url(&t.url, url)).cloned()
}

#[async_trait::async_trait]
impl MeilisearchResolver for EngineTableResolver {
    async fn default_target(
        &self,
        account_id: Option<&str>,
    ) -> Result<Option<MeiliTarget>, ApiError> {
        let Some(account) = account_id.and_then(|a| a.parse::<uuid::Uuid>().ok()) else {
            return Ok(None);
        };
        sqlx::query(
            "SELECT url, api_key FROM meilisearch_engines WHERE account_id = $1 AND is_default = true LIMIT 1",
        )
        .bind(account)
        .fetch_optional(&self.pool)
        .await
        .map(|r| r.map(row_target))
        .map_err(db_err)
    }
    async fn target_for_url(
        &self,
        account_id: Option<&str>,
        url: &str,
    ) -> Result<Option<MeiliTarget>, ApiError> {
        let Some(account) = account_id.and_then(|a| a.parse::<uuid::Uuid>().ok()) else {
            return Ok(server_fallback(&self.server, url));
        };
        let row = sqlx::query(
            "SELECT url, api_key FROM meilisearch_engines \
             WHERE account_id = $1 AND rtrim(url, '/') = rtrim($2, '/') \
             ORDER BY is_default DESC LIMIT 1",
        )
        .bind(account)
        .bind(url)
        .fetch_optional(&self.pool)
        .await
        .map(|r| r.map(row_target))
        .map_err(db_err)?;
        Ok(row.or_else(|| server_fallback(&self.server, url)))
    }
    fn missing_message(&self) -> &'static str {
        "No Meilisearch engine configured. Add one in Settings."
    }
}

/// A crawl's own `meilisearch` block wins; else the resolver's default;
/// else a 400.
pub(crate) async fn resolve_crawl_meilisearch(
    resolver: &dyn MeilisearchResolver,
    account_id: Option<&str>,
    mut cfg: MeilisearchConfig,
) -> Result<MeilisearchConfig, ApiError> {
    if !cfg.url.is_empty() {
        return Ok(cfg);
    }
    let target = resolver
        .default_target(account_id)
        .await?
        .ok_or_else(|| ApiError::new(resolver.missing_message(), "validation_error"))?;
    cfg.url = target.url;
    cfg.api_key = target.api_key.unwrap_or_default();
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(url: &str, key: Option<&str>) -> MeiliTarget {
        MeiliTarget {
            url: url.into(),
            api_key: key.map(Into::into),
        }
    }

    #[tokio::test]
    async fn env_resolver_default_and_url_match() {
        let r = EnvResolver(Some(t("http://m:7700", Some("k"))));
        assert_eq!(
            r.default_target(None).await.unwrap(),
            Some(t("http://m:7700", Some("k")))
        );
        assert_eq!(
            r.target_for_url(None, "http://m:7700/").await.unwrap(),
            Some(t("http://m:7700", Some("k")))
        );
        assert_eq!(
            r.target_for_url(None, "http://other:7700").await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn env_resolver_without_env_has_no_target() {
        let r = EnvResolver(None);
        assert_eq!(r.default_target(None).await.unwrap(), None);
        assert!(r.missing_message().contains("MEILISEARCH_URL"));
    }

    #[tokio::test]
    async fn request_block_wins_over_default() {
        let r = EnvResolver(Some(t("http://env:7700", Some("envkey"))));
        let cfg = scrapix_core::MeilisearchConfig {
            url: "http://req:7700".into(),
            api_key: "reqkey".into(),
            ..Default::default()
        };
        let out = resolve_crawl_meilisearch(&r, None, cfg).await.unwrap();
        assert_eq!(
            (out.url.as_str(), out.api_key.as_str()),
            ("http://req:7700", "reqkey")
        );
    }

    #[tokio::test]
    async fn empty_request_block_uses_default_else_400() {
        let r = EnvResolver(Some(t("http://env:7700", None)));
        let out = resolve_crawl_meilisearch(&r, None, Default::default())
            .await
            .unwrap();
        assert_eq!(out.url, "http://env:7700");
        assert_eq!(out.api_key, "");
        let err = resolve_crawl_meilisearch(&EnvResolver(None), None, Default::default())
            .await
            .unwrap_err();
        assert_eq!(err.code, "validation_error");
    }

    #[test]
    fn same_url_ignores_trailing_slash() {
        assert!(same_url("http://a:1/", "http://a:1"));
        assert!(!same_url("http://a:1", "http://b:1"));
    }

    #[test]
    fn server_fallback_only_matches_same_url() {
        let server = Some(t("http://server:7700", Some("serverkey")));
        assert_eq!(
            server_fallback(&server, "http://server:7700/"),
            Some(t("http://server:7700", Some("serverkey")))
        );
        assert_eq!(server_fallback(&server, "http://tenant:7700"), None);
        assert_eq!(server_fallback(&None, "http://server:7700"), None);
    }
}
