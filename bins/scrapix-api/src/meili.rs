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
/// account's engines, from the Lab).
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
    /// Message for the 400 returned when a crawl names a `meilisearch.url`
    /// without an `api_key` and the URL is not a known target. `None` lets
    /// the crawl go on without a key (an unsecured instance).
    fn unknown_url_message(&self) -> Option<&'static str>;
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
    fn unknown_url_message(&self) -> Option<&'static str> {
        None
    }
}

/// Hosted: the account's engines as the Lab reports them
/// (`GET /internal/accounts/{id}/meilisearch`), and nothing else: a
/// tenant's crawl never lands in the operator's Meilisearch.
pub(crate) struct LabMeilisearchResolver {
    pub(crate) lab: std::sync::Arc<crate::lab_client::LabClient>,
}

fn lab_err(e: crate::lab_client::LabError) -> ApiError {
    crate::lab_client::log_lab_error(&e, "Meilisearch lookup");
    ApiError::new(
        "Meilisearch configuration unavailable, retry shortly",
        "service_unavailable",
    )
    .with_retry_after(5)
}

#[async_trait::async_trait]
impl MeilisearchResolver for LabMeilisearchResolver {
    async fn default_target(
        &self,
        account_id: Option<&str>,
    ) -> Result<Option<MeiliTarget>, ApiError> {
        let Some(account) = account_id else {
            return Ok(None);
        };
        self.lab.meilisearch(account, None).await.map_err(lab_err)
    }
    async fn target_for_url(
        &self,
        account_id: Option<&str>,
        url: &str,
    ) -> Result<Option<MeiliTarget>, ApiError> {
        let Some(account) = account_id else {
            return Ok(None);
        };
        self.lab
            .meilisearch(account, Some(url))
            .await
            .map_err(lab_err)
    }
    fn missing_message(&self) -> &'static str {
        "No Meilisearch engine configured. Add one in Settings."
    }
    fn unknown_url_message(&self) -> Option<&'static str> {
        Some(
            "meilisearch.url is not one of this account's Meilisearch instances: \
             add it in Settings, or pass meilisearch.api_key",
        )
    }
}

/// A crawl's own `meilisearch` block wins; else the resolver's default;
/// else a 400. A block with a `url` but no `api_key` gets the key of the
/// known target at that URL (hosted: the account's instance, from the Lab);
/// an unknown URL is a 400 in hosted mode, never a keyless crawl.
pub(crate) async fn resolve_crawl_meilisearch(
    resolver: &dyn MeilisearchResolver,
    account_id: Option<&str>,
    mut cfg: MeilisearchConfig,
) -> Result<MeilisearchConfig, ApiError> {
    if !cfg.url.is_empty() {
        if cfg.api_key.is_empty() {
            match resolver.target_for_url(account_id, &cfg.url).await? {
                Some(target) => cfg.api_key = target.api_key.unwrap_or_default(),
                None => {
                    if let Some(msg) = resolver.unknown_url_message() {
                        return Err(ApiError::new(msg, "validation_error"));
                    }
                }
            }
        }
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

    #[tokio::test]
    async fn hosted_resolver_never_falls_back_to_the_operator_server() {
        use crate::lab_client::{
            testing::{FakeLab, INSTANCE_ID, SECRET},
            LabClient,
        };
        let lab = FakeLab::start().await;
        let acct = "11111111-1111-1111-1111-111111111111";
        lab.state.meili.lock().unwrap().insert(
            format!("{acct}|"),
            serde_json::json!({"id": "e", "url": "http://m:7700", "api_key": "k"}),
        );
        let r = LabMeilisearchResolver {
            lab: std::sync::Arc::new(LabClient::new(&lab.url, INSTANCE_ID, SECRET)),
        };
        assert_eq!(
            r.default_target(Some(acct)).await.unwrap().unwrap().url,
            "http://m:7700"
        );
        assert_eq!(r.default_target(None).await.unwrap(), None);
        assert_eq!(
            r.target_for_url(Some(acct), "http://ops:7700/")
                .await
                .unwrap(),
            None,
            "a URL the Lab does not know is unknown"
        );
        assert_eq!(
            r.target_for_url(None, "http://ops:7700").await.unwrap(),
            None
        );
        let other = "22222222-2222-2222-2222-222222222222";
        assert_eq!(
            r.default_target(Some(other)).await.unwrap(),
            None,
            "no target, no fallback"
        );
        let err = resolve_crawl_meilisearch(&r, Some(other), Default::default())
            .await
            .unwrap_err();
        assert_eq!(err.code, "validation_error");
        assert!(err.error.contains("Settings"), "{}", err.error);
        lab.set_down(true);
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let third = "33333333-3333-3333-3333-333333333333";
        let err = r.default_target(Some(third)).await.unwrap_err();
        let resp = axum::response::IntoResponse::into_response(err);
        assert_eq!(resp.status(), 503);
        assert_eq!(resp.headers().get("retry-after").unwrap(), "5");
    }

    /// A hosted crawl's `meilisearch` block, against an account with a
    /// default instance (`http://m:7700`) and a second one (`http://b:7700`).
    #[tokio::test]
    async fn hosted_crawl_target_by_url() {
        use crate::lab_client::{
            testing::{FakeLab, INSTANCE_ID, SECRET},
            LabClient,
        };
        let lab = FakeLab::start().await;
        let acct = "11111111-1111-1111-1111-111111111111";
        {
            let mut m = lab.state.meili.lock().unwrap();
            m.insert(
                format!("{acct}|"),
                serde_json::json!({"id": "d", "url": "http://m:7700", "api_key": "dkey"}),
            );
            m.insert(
                format!("{acct}|http://b:7700"),
                serde_json::json!({"id": "b", "url": "http://b:7700", "api_key": "bkey"}),
            );
        }
        let r = LabMeilisearchResolver {
            lab: std::sync::Arc::new(LabClient::new(&lab.url, INSTANCE_ID, SECRET)),
        };
        let block = |url: &str, key: &str| scrapix_core::MeilisearchConfig {
            url: url.into(),
            api_key: key.into(),
            ..Default::default()
        };
        let pair = |c: &scrapix_core::MeilisearchConfig| (c.url.clone(), c.api_key.clone());

        // URL + key: used as is, whatever the URL.
        let out = resolve_crawl_meilisearch(&r, Some(acct), block("http://own:7700", "mine"))
            .await
            .unwrap();
        assert_eq!(pair(&out), ("http://own:7700".into(), "mine".into()));

        // URL without key: the key of the account's instance at that URL.
        let out = resolve_crawl_meilisearch(&r, Some(acct), block("http://b:7700/", ""))
            .await
            .unwrap();
        assert_eq!(pair(&out), ("http://b:7700/".into(), "bkey".into()));

        // Unknown URL without key: refused, never crawled unauthenticated.
        let err = resolve_crawl_meilisearch(&r, Some(acct), block("http://own:7700", ""))
            .await
            .unwrap_err();
        assert_eq!(err.code, "validation_error");
        assert!(err.error.contains("meilisearch.url"), "{}", err.error);

        // No URL: the account's default instance, unchanged.
        let out = resolve_crawl_meilisearch(&r, Some(acct), Default::default())
            .await
            .unwrap();
        assert_eq!(pair(&out), ("http://m:7700".into(), "dkey".into()));
    }

    #[tokio::test]
    async fn standalone_url_without_key_uses_env_key_or_goes_keyless() {
        let r = EnvResolver(Some(t("http://env:7700", Some("envkey"))));
        let block = |url: &str| scrapix_core::MeilisearchConfig {
            url: url.into(),
            ..Default::default()
        };
        let out = resolve_crawl_meilisearch(&r, None, block("http://env:7700/"))
            .await
            .unwrap();
        assert_eq!(out.api_key, "envkey");
        // A self-hoster's unsecured instance stays reachable without a key.
        let out = resolve_crawl_meilisearch(&r, None, block("http://other:7700"))
            .await
            .unwrap();
        assert_eq!(
            (out.url.as_str(), out.api_key.as_str()),
            ("http://other:7700", "")
        );
    }

    #[test]
    fn same_url_ignores_trailing_slash() {
        assert!(same_url("http://a:1/", "http://a:1"));
        assert!(!same_url("http://a:1", "http://b:1"));
    }
}
