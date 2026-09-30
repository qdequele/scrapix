//! Engine → Lab lookups (spec: Lab split §3.2): credential introspection,
//! account tier/balance, and Meilisearch targets over the Lab's internal
//! API, authenticated with LAB_SERVICE_TOKEN. Answers are cached (TTL from
//! the Lab, clamped); while the Lab is unreachable a cached answer is served
//! for `stale_grace` past its expiry, then everything fails closed.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::meili::MeiliTarget;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CredentialKind {
    ApiKey,
    Bearer,
    Session,
}

impl CredentialKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ApiKey => "api_key",
            Self::Bearer => "bearer",
            Self::Session => "session",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Identity {
    pub account_id: String,
    pub tier: String,
    pub role: Option<String>,
    pub api_key_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LabError {
    Unavailable(String),
    ServiceTokenRejected,
    /// Lab answered a 4xx other than 401: reachable but refusing the request.
    /// Never served stale.
    BadResponse(u16),
}

impl std::fmt::Display for LabError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(m) => write!(f, "Lab unavailable: {m}"),
            Self::ServiceTokenRejected => f.write_str("Lab rejected LAB_SERVICE_TOKEN"),
            Self::BadResponse(code) => write!(f, "Lab answered HTTP {code}"),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Timing {
    pub default_ttl: Duration,
    pub max_ttl: Duration,
    pub negative_ttl: Duration,
    pub stale_grace: Duration,
    pub meili_ttl: Duration,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub capacity: usize,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            default_ttl: Duration::from_secs(30),
            max_ttl: Duration::from_secs(300),
            negative_ttl: Duration::from_secs(5),
            stale_grace: Duration::from_secs(300),
            meili_ttl: Duration::from_secs(60),
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(5),
            capacity: 10_000,
        }
    }
}

#[derive(Deserialize)]
struct Credits {
    balance: i64,
}

#[derive(Deserialize)]
struct Answer {
    active: bool,
    #[serde(default)]
    account_id: Option<String>,
    #[serde(default)]
    tier: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    api_key_id: Option<String>,
    #[serde(default)]
    credits: Option<Credits>,
    #[serde(default)]
    cache_ttl: Option<i64>,
}

#[derive(Deserialize)]
struct MeiliAnswer {
    url: String,
    #[serde(default)]
    api_key: Option<String>,
}

#[derive(Clone)]
struct Entry<T> {
    value: T,
    fetched: Instant,
    ttl: Duration,
}

struct Balance {
    balance: i64,
    used_since: i64,
    fetched: Instant,
    ttl: Duration,
}

/// A size-bounded TTL map; on overflow the oldest-fetched entry is evicted.
struct Cache<T> {
    map: HashMap<String, Entry<T>>,
    capacity: usize,
}

impl<T: Clone> Cache<T> {
    fn new(capacity: usize) -> Self {
        Self {
            map: HashMap::new(),
            capacity,
        }
    }
    fn get(&self, k: &str) -> Option<Entry<T>> {
        self.map.get(k).cloned()
    }
    fn put(&mut self, k: String, value: T, ttl: Duration) {
        if !self.map.contains_key(&k) && self.map.len() >= self.capacity {
            if let Some(oldest) = self
                .map
                .iter()
                .min_by_key(|(_, e)| e.fetched)
                .map(|(k, _)| k.clone())
            {
                self.map.remove(&oldest);
            }
        }
        self.map.insert(
            k,
            Entry {
                value,
                fetched: Instant::now(),
                ttl,
            },
        );
    }
}

pub(crate) struct LabClient {
    base: String,
    token: String,
    http: reqwest::Client,
    timing: Timing,
    identities: Mutex<Cache<Option<Identity>>>,
    meili: Mutex<Cache<Option<MeiliTarget>>>,
    balances: Mutex<HashMap<String, Balance>>,
}

/// `scrapix_lab_requests_total{endpoint,outcome}`.
fn count(endpoint: &str, outcome: &str) {
    scrapix_core::metrics::lab_requests_total()
        .with_label_values(&[endpoint, outcome])
        .inc();
}

/// Cache keys never contain a raw credential.
fn hashed(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0u8]);
    }
    hex::encode(h.finalize())
}

impl LabClient {
    pub(crate) fn new(base_url: &str, service_token: &str) -> Self {
        Self::with_timing(base_url, service_token, Timing::default())
    }

    pub(crate) fn with_timing(base_url: &str, service_token: &str, timing: Timing) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(timing.connect_timeout)
            .timeout(timing.request_timeout)
            .build()
            .expect("reqwest client");
        Self {
            base: base_url.trim_end_matches('/').to_string(),
            token: service_token.to_string(),
            http,
            identities: Mutex::new(Cache::new(timing.capacity)),
            meili: Mutex::new(Cache::new(timing.capacity)),
            balances: Mutex::new(HashMap::new()),
            timing,
        }
    }

    pub(crate) fn base_from_events_url(events_url: &str) -> String {
        let trimmed = events_url.trim_end_matches('/');
        trimmed
            .strip_suffix("/internal/events")
            .unwrap_or(trimmed)
            .to_string()
    }

    fn ttl_from(&self, secs: Option<i64>) -> Duration {
        match secs {
            None => self.timing.default_ttl,
            Some(s) if s <= 0 => Duration::ZERO,
            Some(s) => Duration::from_secs(s as u64).min(self.timing.max_ttl),
        }
    }

    async fn send(
        &self,
        endpoint: &'static str,
        req: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, LabError> {
        let result = match req.bearer_auth(&self.token).send().await {
            Err(e) => Err(LabError::Unavailable(e.to_string())),
            Ok(resp) => match resp.status().as_u16() {
                401 => Err(LabError::ServiceTokenRejected),
                // The Meilisearch lookup's 404 means "no engine", handled by the caller.
                404 if endpoint == "meilisearch" => Ok(resp),
                c @ 400..=499 => Err(LabError::BadResponse(c)),
                s if s >= 500 => Err(LabError::Unavailable(format!("HTTP {s}"))),
                _ => Ok(resp),
            },
        };
        match &result {
            Err(LabError::Unavailable(_)) => count(endpoint, "unavailable"),
            Err(LabError::ServiceTokenRejected | LabError::BadResponse(_)) => {
                count(endpoint, "rejected")
            }
            Ok(_) => {}
        }
        result
    }

    async fn answer(
        &self,
        endpoint: &'static str,
        req: reqwest::RequestBuilder,
    ) -> Result<Answer, LabError> {
        let resp = self.send(endpoint, req).await?;
        if !resp.status().is_success() {
            count(endpoint, "unavailable");
            return Err(LabError::Unavailable(format!("HTTP {}", resp.status())));
        }
        let a: Answer = resp.json().await.map_err(|e| {
            count(endpoint, "unavailable");
            LabError::Unavailable(format!("malformed: {e}"))
        })?;
        if a.active && (a.account_id.is_none() || a.tier.is_none()) {
            count(endpoint, "unavailable");
            return Err(LabError::Unavailable(
                "malformed: active answer without account".into(),
            ));
        }
        Ok(a)
    }

    fn remember_balance(&self, account_id: &str, credits: &Option<Credits>, ttl: Duration) {
        if let Some(c) = credits {
            self.balances.lock().unwrap().insert(
                account_id.to_string(),
                Balance {
                    balance: c.balance,
                    used_since: 0,
                    fetched: Instant::now(),
                    ttl,
                },
            );
        }
    }

    /// Cached lookup with the stale-on-error rule shared by every endpoint:
    /// only a *positive* cached answer is ever served stale.
    async fn cached<T, F, Fut>(
        &self,
        endpoint: &'static str,
        cache: &Mutex<Cache<Option<T>>>,
        key: String,
        fetch: F,
    ) -> Result<Option<T>, LabError>
    where
        T: Clone,
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<(Option<T>, Duration), LabError>>,
    {
        let hit = cache.lock().unwrap().get(&key);
        if let Some(e) = &hit {
            if e.fetched.elapsed() < e.ttl {
                return Ok(e.value.clone());
            }
        }
        match fetch().await {
            Ok((value, ttl)) => {
                cache.lock().unwrap().put(key, value.clone(), ttl);
                Ok(value)
            }
            Err(LabError::Unavailable(m)) => match hit {
                Some(e)
                    if e.value.is_some()
                        && e.fetched.elapsed() < e.ttl + self.timing.stale_grace =>
                {
                    tracing::warn!(error = %m, "Lab unreachable; serving a cached answer");
                    count(endpoint, "stale");
                    Ok(e.value)
                }
                _ => Err(LabError::Unavailable(m)),
            },
            Err(e) => Err(e),
        }
    }

    fn to_identity(&self, endpoint: &'static str, a: Answer) -> (Option<Identity>, Duration) {
        if !a.active {
            count(endpoint, "inactive");
            return (None, self.timing.negative_ttl);
        }
        count(endpoint, "ok");
        let ttl = self.ttl_from(a.cache_ttl);
        let account_id = a.account_id.unwrap_or_default();
        self.remember_balance(&account_id, &a.credits, ttl);
        (
            Some(Identity {
                account_id,
                tier: a.tier.unwrap_or_default(),
                role: a.role,
                api_key_id: a.api_key_id,
            }),
            ttl,
        )
    }

    pub(crate) async fn ping(&self) -> Result<(), LabError> {
        let resp = self
            .send(
                "ping",
                self.http.get(format!("{}/internal/ping", self.base)),
            )
            .await?;
        if resp.status().is_success() {
            count("ping", "ok");
            Ok(())
        } else {
            count("ping", "unavailable");
            Err(LabError::Unavailable(format!("HTTP {}", resp.status())))
        }
    }

    pub(crate) async fn introspect(
        &self,
        kind: CredentialKind,
        credential: &str,
        account_id: Option<&str>,
    ) -> Result<Option<Identity>, LabError> {
        let key = hashed(&["i", kind.as_str(), credential, account_id.unwrap_or("")]);
        self.cached("introspect", &self.identities, key, || async {
            let body = serde_json::json!({"kind": kind.as_str(), "credential": credential, "account_id": account_id});
            let a = self.answer("introspect", self.http.post(format!("{}/internal/auth/introspect", self.base)).json(&body)).await?;
            Ok(self.to_identity("introspect", a))
        }).await
    }

    pub(crate) async fn account(&self, account_id: &str) -> Result<Option<Identity>, LabError> {
        let key = hashed(&["a", account_id]);
        self.cached("account", &self.identities, key, || async {
            let a = self
                .answer(
                    "account",
                    self.http
                        .get(format!("{}/internal/accounts/{account_id}", self.base)),
                )
                .await?;
            let (identity, ttl) = self.to_identity("account", a);
            if identity.is_none() {
                self.forget_balance(account_id);
            }
            Ok((identity, ttl))
        })
        .await
    }

    /// An inactive account must never keep a positive balance snapshot.
    fn forget_balance(&self, account_id: &str) {
        self.balances.lock().unwrap().remove(account_id);
    }

    pub(crate) fn note_usage(&self, account_id: &str, credits: i64) {
        if let Some(b) = self.balances.lock().unwrap().get_mut(account_id) {
            b.used_since += credits;
        }
    }

    pub(crate) async fn available_credits(
        &self,
        account_id: &str,
    ) -> Result<Option<i64>, LabError> {
        if let Some(b) = self.balances.lock().unwrap().get(account_id) {
            if b.fetched.elapsed() < b.ttl {
                return Ok(Some(b.balance - b.used_since));
            }
        }
        // Refresh directly (not through the identity cache): the balance must be current.
        match self
            .answer(
                "account",
                self.http
                    .get(format!("{}/internal/accounts/{account_id}", self.base)),
            )
            .await
        {
            Ok(a) => {
                let (id, _) = self.to_identity("account", a);
                if id.is_none() {
                    self.forget_balance(account_id);
                    return Ok(None);
                }
                Ok(self
                    .balances
                    .lock()
                    .unwrap()
                    .get(account_id)
                    .map(|b| b.balance - b.used_since))
            }
            Err(LabError::Unavailable(m)) => {
                let guard = self.balances.lock().unwrap();
                match guard.get(account_id) {
                    Some(b) if b.fetched.elapsed() < b.ttl + self.timing.stale_grace => {
                        Ok(Some(b.balance - b.used_since))
                    }
                    _ => Err(LabError::Unavailable(m)),
                }
            }
            Err(e) => Err(e),
        }
    }

    pub(crate) async fn meilisearch(
        &self,
        account_id: &str,
        url: Option<&str>,
    ) -> Result<Option<MeiliTarget>, LabError> {
        let key = hashed(&[
            "m",
            account_id,
            url.map(|u| u.trim_end_matches('/')).unwrap_or(""),
        ]);
        let meili_ttl = self.timing.meili_ttl;
        self.cached("meilisearch", &self.meili, key, || async move {
            let mut req = self.http.get(format!(
                "{}/internal/accounts/{account_id}/meilisearch",
                self.base
            ));
            if let Some(u) = url {
                req = req.query(&[("url", u)]);
            }
            let resp = self.send("meilisearch", req).await?;
            if resp.status().as_u16() == 404 {
                count("meilisearch", "inactive");
                return Ok((None, meili_ttl));
            }
            if !resp.status().is_success() {
                count("meilisearch", "unavailable");
                return Err(LabError::Unavailable(format!("HTTP {}", resp.status())));
            }
            let m: MeiliAnswer = resp.json().await.map_err(|e| {
                count("meilisearch", "unavailable");
                LabError::Unavailable(format!("malformed: {e}"))
            })?;
            count("meilisearch", "ok");
            Ok((
                Some(MeiliTarget {
                    url: m.url.trim_end_matches('/').to_string(),
                    api_key: m.api_key.filter(|k| !k.is_empty()),
                }),
                meili_ttl,
            ))
        })
        .await
    }

    #[cfg(test)]
    pub(crate) fn debug_cache_keys(&self) -> Vec<String> {
        self.identities
            .lock()
            .unwrap()
            .map
            .keys()
            .cloned()
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn cached_ttl_for_test(
        &self,
        kind: CredentialKind,
        credential: &str,
        account_id: Option<&str>,
    ) -> Option<Duration> {
        let key = hashed(&["i", kind.as_str(), credential, account_id.unwrap_or("")]);
        self.identities.lock().unwrap().get(&key).map(|e| e.ttl)
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! In-process fake of the Lab's internal API (contracts/lab-internal.openapi.json).
    use axum::{
        extract::{Path, Query, State},
        http::{HeaderMap, StatusCode},
        routing::{get, post},
        Json, Router,
    };
    use serde_json::{json, Value};
    use std::{
        collections::HashMap,
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc, Mutex,
        },
    };

    pub(crate) const TOKEN: &str = "fake-lab-service-token-0123456789abcdef";

    #[derive(Default)]
    pub(crate) struct FakeLabState {
        /// "kind:credential" → introspection JSON.
        pub credentials: Mutex<HashMap<String, Value>>,
        pub accounts: Mutex<HashMap<String, Value>>,
        /// "account|url" (url "" = default) → {"id","url","api_key"}.
        pub meili: Mutex<HashMap<String, Value>>,
        pub down: AtomicBool,
        pub slow_ms: AtomicUsize,
        pub malformed: AtomicBool,
        /// Non-zero: every gated route answers this status (after the down check).
        pub status_override: AtomicUsize,
        pub calls: AtomicUsize,
    }

    pub(crate) struct FakeLab {
        pub url: String,
        pub state: Arc<FakeLabState>,
    }

    fn authorized(h: &HeaderMap) -> bool {
        h.get("authorization").and_then(|v| v.to_str().ok())
            == Some(format!("Bearer {TOKEN}").as_str())
    }

    async fn gate(s: &FakeLabState, h: &HeaderMap) -> Result<(), StatusCode> {
        s.calls.fetch_add(1, Ordering::SeqCst);
        let ms = s.slow_ms.load(Ordering::SeqCst);
        if ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(ms as u64)).await;
        }
        if s.down.load(Ordering::SeqCst) {
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }
        let forced = s.status_override.load(Ordering::SeqCst);
        if forced != 0 {
            return Err(StatusCode::from_u16(forced as u16).unwrap());
        }
        if !authorized(h) {
            return Err(StatusCode::UNAUTHORIZED);
        }
        Ok(())
    }

    impl FakeLab {
        pub(crate) async fn start() -> FakeLab {
            let state = Arc::new(FakeLabState::default());
            let app =
                Router::new()
                    .route(
                        "/internal/ping",
                        get(
                            |State(s): State<Arc<FakeLabState>>, h: HeaderMap| async move {
                                gate(&s, &h).await.map(|_| Json(json!({"ok": true})))
                            },
                        ),
                    )
                    .route(
                        "/internal/auth/introspect",
                        post(
                            |State(s): State<Arc<FakeLabState>>,
                             h: HeaderMap,
                             Json(b): Json<Value>| async move {
                                gate(&s, &h).await?;
                                if s.malformed.load(Ordering::SeqCst) {
                                    return Ok::<_, StatusCode>(Json(json!({"active": "yes"})));
                                }
                                let key = format!(
                                    "{}:{}",
                                    b["kind"].as_str().unwrap_or(""),
                                    b["credential"].as_str().unwrap_or("")
                                );
                                Ok(Json(
                                    s.credentials
                                        .lock()
                                        .unwrap()
                                        .get(&key)
                                        .cloned()
                                        .unwrap_or(json!({"active": false})),
                                ))
                            },
                        ),
                    )
                    .route(
                        "/internal/accounts/{id}",
                        get(
                            |State(s): State<Arc<FakeLabState>>,
                             h: HeaderMap,
                             Path(id): Path<String>| async move {
                                gate(&s, &h).await?;
                                Ok::<_, StatusCode>(Json(
                                    s.accounts
                                        .lock()
                                        .unwrap()
                                        .get(&id)
                                        .cloned()
                                        .unwrap_or(json!({"active": false})),
                                ))
                            },
                        ),
                    )
                    .route(
                        "/internal/accounts/{id}/meilisearch",
                        get(
                            |State(s): State<Arc<FakeLabState>>,
                             h: HeaderMap,
                             Path(id): Path<String>,
                             Query(q): Query<HashMap<String, String>>| async move {
                                gate(&s, &h).await?;
                                let key = format!(
                                    "{id}|{}",
                                    q.get("url").map(|u| u.trim_end_matches('/')).unwrap_or("")
                                );
                                s.meili
                                    .lock()
                                    .unwrap()
                                    .get(&key)
                                    .cloned()
                                    .map(Json)
                                    .ok_or(StatusCode::NOT_FOUND)
                            },
                        ),
                    )
                    .with_state(state.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            FakeLab { url, state }
        }

        pub(crate) fn identity(account: &str, tier: &str, balance: i64) -> Value {
            json!({"active": true, "account_id": account, "tier": tier, "role": null, "api_key_id": null,
                   "principal": {"type": "api_key", "user_id": null}, "credits": {"balance": balance}})
        }
        pub(crate) fn set_credential(&self, kind: &str, credential: &str, v: Value) {
            self.state
                .credentials
                .lock()
                .unwrap()
                .insert(format!("{kind}:{credential}"), v);
        }
        pub(crate) fn set_account(&self, account: &str, v: Value) {
            self.state
                .accounts
                .lock()
                .unwrap()
                .insert(account.to_string(), v);
        }
        pub(crate) fn calls(&self) -> usize {
            self.state.calls.load(Ordering::SeqCst)
        }
        pub(crate) fn set_down(&self, down: bool) {
            self.state.down.store(down, Ordering::SeqCst)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{FakeLab, TOKEN};
    use super::*;
    use serde_json::json;

    const ACCT: &str = "11111111-1111-1111-1111-111111111111";

    fn fast() -> Timing {
        Timing {
            default_ttl: Duration::from_millis(200),
            max_ttl: Duration::from_secs(300),
            negative_ttl: Duration::from_millis(100),
            stale_grace: Duration::from_millis(400),
            meili_ttl: Duration::from_millis(200),
            connect_timeout: Duration::from_millis(200),
            request_timeout: Duration::from_millis(300),
            capacity: 3,
        }
    }

    async fn setup() -> (FakeLab, LabClient) {
        let lab = FakeLab::start().await;
        let client = LabClient::with_timing(&lab.url, TOKEN, fast());
        (lab, client)
    }

    #[tokio::test]
    async fn ping_ok_and_wrong_token_is_rejected() {
        let lab = FakeLab::start().await;
        LabClient::with_timing(&lab.url, TOKEN, fast())
            .ping()
            .await
            .unwrap();
        let err = LabClient::with_timing(&lab.url, "wrong", fast())
            .ping()
            .await
            .unwrap_err();
        assert_eq!(err, LabError::ServiceTokenRejected);
    }

    #[tokio::test]
    async fn introspection_is_cached_until_ttl() {
        let (lab, c) = setup().await;
        lab.set_credential("api_key", "sk_live_a", FakeLab::identity(ACCT, "pro", 50));
        let id = c
            .introspect(CredentialKind::ApiKey, "sk_live_a", None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((id.account_id.as_str(), id.tier.as_str()), (ACCT, "pro"));
        c.introspect(CredentialKind::ApiKey, "sk_live_a", None)
            .await
            .unwrap();
        assert_eq!(lab.calls(), 1, "second call served from cache");
        tokio::time::sleep(Duration::from_millis(250)).await;
        c.introspect(CredentialKind::ApiKey, "sk_live_a", None)
            .await
            .unwrap();
        assert_eq!(lab.calls(), 2, "refetched after ttl");
    }

    #[tokio::test]
    async fn inactive_is_cached_briefly() {
        let (lab, c) = setup().await;
        assert!(c
            .introspect(CredentialKind::ApiKey, "sk_live_x", None)
            .await
            .unwrap()
            .is_none());
        assert!(c
            .introspect(CredentialKind::ApiKey, "sk_live_x", None)
            .await
            .unwrap()
            .is_none());
        assert_eq!(lab.calls(), 1);
        tokio::time::sleep(Duration::from_millis(150)).await;
        c.introspect(CredentialKind::ApiKey, "sk_live_x", None)
            .await
            .unwrap();
        assert_eq!(lab.calls(), 2);
    }

    #[tokio::test]
    async fn kind_and_account_are_part_of_the_cache_key() {
        let (lab, c) = setup().await;
        lab.set_credential("session", "jwt", FakeLab::identity(ACCT, "free", 1));
        assert!(c
            .introspect(CredentialKind::Bearer, "jwt", None)
            .await
            .unwrap()
            .is_none());
        assert!(c
            .introspect(CredentialKind::Session, "jwt", None)
            .await
            .unwrap()
            .is_some());
        c.introspect(CredentialKind::Session, "jwt", Some(ACCT))
            .await
            .unwrap();
        assert_eq!(lab.calls(), 3);
    }

    #[tokio::test]
    async fn lab_down_serves_stale_then_fails_closed() {
        let (lab, c) = setup().await;
        lab.set_credential("api_key", "sk_live_a", FakeLab::identity(ACCT, "pro", 50));
        c.introspect(CredentialKind::ApiKey, "sk_live_a", None)
            .await
            .unwrap();
        lab.set_down(true);
        tokio::time::sleep(Duration::from_millis(250)).await; // past ttl, inside grace
        let stale =
            scrapix_core::metrics::lab_requests_total().with_label_values(&["introspect", "stale"]);
        let stale_before = stale.get();
        assert!(c
            .introspect(CredentialKind::ApiKey, "sk_live_a", None)
            .await
            .unwrap()
            .is_some());
        assert!(
            stale.get() > stale_before,
            "serving a stale answer is counted"
        );
        tokio::time::sleep(Duration::from_millis(450)).await; // past grace
        assert!(matches!(
            c.introspect(CredentialKind::ApiKey, "sk_live_a", None)
                .await,
            Err(LabError::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn unknown_credential_while_down_is_unavailable_not_inactive() {
        let (lab, c) = setup().await;
        lab.set_down(true);
        assert!(matches!(
            c.introspect(CredentialKind::ApiKey, "sk_live_new", None)
                .await,
            Err(LabError::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn slow_lab_counts_as_unavailable() {
        let (lab, c) = setup().await;
        lab.state
            .slow_ms
            .store(1_000, std::sync::atomic::Ordering::SeqCst);
        let started = std::time::Instant::now();
        assert!(matches!(
            c.introspect(CredentialKind::ApiKey, "sk_live_a", None)
                .await,
            Err(LabError::Unavailable(_))
        ));
        assert!(
            started.elapsed() < Duration::from_millis(900),
            "gave up at request_timeout"
        );
    }

    #[tokio::test]
    async fn malformed_response_is_never_a_positive_answer() {
        let (lab, c) = setup().await;
        lab.state
            .malformed
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(
            c.introspect(CredentialKind::ApiKey, "sk_live_a", None)
                .await,
            Err(LabError::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn cache_ttl_is_clamped() {
        let (lab, c) = setup().await;
        let mut v = FakeLab::identity(ACCT, "pro", 1);
        v["cache_ttl"] = json!(-5);
        lab.set_credential("api_key", "sk_live_neg", v.clone());
        c.introspect(CredentialKind::ApiKey, "sk_live_neg", None)
            .await
            .unwrap();
        c.introspect(CredentialKind::ApiKey, "sk_live_neg", None)
            .await
            .unwrap();
        assert_eq!(lab.calls(), 2, "ttl <= 0 means no caching");
        v["cache_ttl"] = json!(1_000_000_000u64);
        lab.set_credential("api_key", "sk_live_big", v);
        c.introspect(CredentialKind::ApiKey, "sk_live_big", None)
            .await
            .unwrap();
        assert!(
            c.cached_ttl_for_test(CredentialKind::ApiKey, "sk_live_big", None)
                .unwrap()
                <= Duration::from_secs(300)
        );
    }

    #[tokio::test]
    async fn only_a_hash_of_the_credential_is_kept() {
        let (lab, c) = setup().await;
        lab.set_credential(
            "api_key",
            "sk_live_secret_value",
            FakeLab::identity(ACCT, "pro", 1),
        );
        c.introspect(CredentialKind::ApiKey, "sk_live_secret_value", None)
            .await
            .unwrap();
        assert!(!c
            .debug_cache_keys()
            .iter()
            .any(|k| k.contains("sk_live_secret_value")));
    }

    #[tokio::test]
    async fn cache_is_bounded() {
        let (lab, c) = setup().await;
        for i in 0..10 {
            lab.set_credential(
                "api_key",
                &format!("k{i}"),
                FakeLab::identity(ACCT, "pro", 1),
            );
            c.introspect(CredentialKind::ApiKey, &format!("k{i}"), None)
                .await
                .unwrap();
        }
        assert!(c.debug_cache_keys().len() <= 3);
    }

    #[tokio::test]
    async fn balance_subtracts_local_usage_and_resets_on_refresh() {
        let (lab, c) = setup().await;
        lab.set_account(
            ACCT,
            json!({"active": true, "account_id": ACCT, "tier": "pro", "credits": {"balance": 100}}),
        );
        assert_eq!(c.available_credits(ACCT).await.unwrap(), Some(100));
        c.note_usage(ACCT, 30);
        assert_eq!(c.available_credits(ACCT).await.unwrap(), Some(70));
        lab.set_account(
            ACCT,
            json!({"active": true, "account_id": ACCT, "tier": "pro", "credits": {"balance": 70}}),
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(
            c.available_credits(ACCT).await.unwrap(),
            Some(70),
            "fresh snapshot, local usage reset"
        );
    }

    #[tokio::test]
    async fn inactive_account_never_regains_a_balance_from_the_stale_snapshot() {
        let (lab, c) = setup().await;
        let active =
            json!({"active": true, "account_id": ACCT, "tier": "pro", "credits": {"balance": 100}});
        lab.set_account(ACCT, active.clone());
        assert_eq!(c.available_credits(ACCT).await.unwrap(), Some(100));
        lab.set_account(ACCT, json!({"active": false}));
        tokio::time::sleep(Duration::from_millis(250)).await; // past ttl
        assert_eq!(c.available_credits(ACCT).await.unwrap(), None);
        lab.set_down(true);
        let r = c.available_credits(ACCT).await;
        assert!(
            matches!(r, Ok(None) | Err(LabError::Unavailable(_))),
            "deactivated account must not get a balance back: {r:?}"
        );
    }

    #[tokio::test]
    async fn account_inactive_answer_drops_the_balance_snapshot() {
        let (lab, c) = setup().await;
        lab.set_account(
            ACCT,
            json!({"active": true, "account_id": ACCT, "tier": "pro", "credits": {"balance": 100}}),
        );
        assert!(c.account(ACCT).await.unwrap().is_some());
        lab.set_account(ACCT, json!({"active": false}));
        tokio::time::sleep(Duration::from_millis(250)).await; // past ttl
        assert!(c.account(ACCT).await.unwrap().is_none());
        lab.set_down(true);
        let r = c.available_credits(ACCT).await;
        assert!(
            matches!(r, Ok(None) | Err(LabError::Unavailable(_))),
            "{r:?}"
        );
    }

    #[tokio::test]
    async fn lab_4xx_is_bad_response_and_never_served_stale() {
        let (lab, c) = setup().await;
        lab.set_credential("api_key", "sk_live_a", FakeLab::identity(ACCT, "pro", 50));
        c.introspect(CredentialKind::ApiKey, "sk_live_a", None)
            .await
            .unwrap();
        lab.state
            .status_override
            .store(400, std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(250)).await; // past ttl, inside grace
        assert_eq!(
            c.introspect(CredentialKind::ApiKey, "sk_live_a", None)
                .await,
            Err(LabError::BadResponse(400))
        );
        assert_eq!(
            LabError::BadResponse(400).to_string(),
            "Lab answered HTTP 400"
        );
    }

    #[tokio::test]
    async fn ping_404_is_bad_response() {
        let (lab, c) = setup().await;
        lab.state
            .status_override
            .store(404, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(c.ping().await, Err(LabError::BadResponse(404)));
    }

    #[tokio::test]
    async fn unknown_account_has_no_balance() {
        let (_lab, c) = setup().await;
        assert_eq!(c.available_credits(ACCT).await.unwrap(), None);
    }

    #[tokio::test]
    async fn meilisearch_lookup_default_and_by_url() {
        let (lab, c) = setup().await;
        lab.state.meili.lock().unwrap().insert(
            format!("{ACCT}|"),
            json!({"id": "e1", "url": "http://m:7700", "api_key": "k"}),
        );
        lab.state.meili.lock().unwrap().insert(
            format!("{ACCT}|http://b:7700"),
            json!({"id": "e2", "url": "http://b:7700/", "api_key": ""}),
        );
        let d = c.meilisearch(ACCT, None).await.unwrap().unwrap();
        assert_eq!(
            (d.url.as_str(), d.api_key.as_deref()),
            ("http://m:7700", Some("k"))
        );
        let b = c
            .meilisearch(ACCT, Some("http://b:7700/"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(b.api_key, None, "empty key maps to None");
        assert!(c
            .meilisearch(ACCT, Some("http://none:7700"))
            .await
            .unwrap()
            .is_none());
    }

    #[test]
    fn base_from_events_url() {
        assert_eq!(
            LabClient::base_from_events_url("http://127.0.0.1:8091/internal/events"),
            "http://127.0.0.1:8091"
        );
        assert_eq!(
            LabClient::base_from_events_url("http://lab/internal/events/"),
            "http://lab"
        );
        assert_eq!(LabClient::base_from_events_url("http://lab"), "http://lab");
    }
}
