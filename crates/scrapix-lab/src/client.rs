//! Engine → Lab lookups (spec: Lab split §3.2): credential introspection,
//! account tier/balance, and Meilisearch targets over the Lab's internal
//! API, authenticated with the instance credentials (`X-Lab-Instance-Id` +
//! Bearer `LAB_INSTANCE_SECRET`). Answers are cached (TTL from
//! the Lab, clamped); while the Lab is unreachable a cached answer is served
//! for `stale_grace` past its expiry, then everything fails closed.
//!
//! Load on the Lab is bounded: at most `max_in_flight` concurrent calls
//! (a caller waits `permit_wait` for a slot, then gets `Unavailable`), a key
//! just served stale is not retried for `stale_retry`, and negative answers
//! live in their own cache so a spray of unknown credentials cannot evict
//! the positive ones.
//!
//! A cached lookup that has to ask the Lab is a *flight*: one detached task
//! per cache key, whose answer every caller of that key waits for, so an
//! expiring entry costs one Lab call however many requests want it. An
//! entry past [`REFRESH_AHEAD`] of its ttl is still served and refreshed by
//! a background flight (once per entry, on access only), so a key in
//! constant use never waits for the Lab. An entry is never served past the
//! ttl the Lab gave for it because of a refresh: a failed one changes
//! nothing.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Where documents are indexed / searched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeiliTarget {
    pub url: String,
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialKind {
    ApiKey,
    Bearer,
    Session,
}

impl CredentialKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ApiKey => "api_key",
            Self::Bearer => "bearer",
            Self::Session => "session",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub account_id: String,
    pub tier: String,
    pub role: Option<String>,
    pub api_key_id: Option<String>,
    /// The plan's limits; `None` from a Lab that predates them (contract v1).
    pub limits: Option<scrapix_auth::Limits>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LabError {
    Unavailable(String),
    CredentialsRejected,
    /// Lab answered a 3xx (redirects are never followed) or a 4xx other than
    /// 401: reachable but refusing the request. Never served stale.
    BadResponse(u16),
}

impl std::fmt::Display for LabError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(m) => write!(f, "Lab unavailable: {m}"),
            Self::CredentialsRejected => {
                f.write_str("Lab rejected LAB_INSTANCE_ID/LAB_INSTANCE_SECRET")
            }
            Self::BadResponse(code) => write!(f, "Lab answered HTTP {code}"),
        }
    }
}

/// `GET /internal/instances/me` (spec §3.6): what the Lab knows about this
/// deployment. Called once at boot to confirm the credentials and log the
/// identity; `kind` is `"hosted"` (there is no other kind of engine).
/// `instance_id` and `region` are null on the Lab's legacy service-token
/// path (no deployment row).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct InstanceInfo {
    #[serde(default)]
    pub instance_id: Option<String>,
    pub kind: String,
    pub product: String,
    #[serde(default)]
    pub region: Option<String>,
    pub lab_url: String,
}

/// An account's spendable credits as the engine sees them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Available {
    pub credits: i64,
    /// Served from a fresh snapshot without asking the Lab (a refresh may
    /// show a top-up made since).
    pub cached: bool,
}

#[derive(Debug, Clone)]
pub struct Timing {
    pub default_ttl: Duration,
    pub max_ttl: Duration,
    pub negative_ttl: Duration,
    pub stale_grace: Duration,
    /// After serving a stale answer because the Lab was unavailable, how long
    /// that key (or balance) is served stale without asking the Lab again.
    pub stale_retry: Duration,
    pub meili_ttl: Duration,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    /// Positive answers kept per cache.
    pub capacity: usize,
    /// Negative answers kept per cache (separately from positives).
    pub negative_capacity: usize,
    /// Concurrent Lab calls.
    pub max_in_flight: usize,
    /// How long a call waits for a free slot before failing `Unavailable`.
    pub permit_wait: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            default_ttl: Duration::from_secs(30),
            max_ttl: Duration::from_secs(300),
            negative_ttl: Duration::from_secs(5),
            stale_grace: Duration::from_secs(300),
            stale_retry: Duration::from_secs(5),
            meili_ttl: Duration::from_secs(60),
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(5),
            capacity: 10_000,
            negative_capacity: 10_000,
            max_in_flight: 32,
            permit_wait: Duration::from_secs(1),
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
    limits: Option<scrapix_auth::Limits>,
    #[serde(default)]
    cache_ttl: Option<i64>,
}

#[derive(Deserialize)]
struct MeiliAnswer {
    url: String,
    #[serde(default)]
    api_key: Option<String>,
}

/// The Lab's `GET /internal/accounts/{id}/ai` answer.
#[derive(Deserialize)]
struct AiAnswer {
    base_url: String,
    api_key: String,
    key_id: String,
    default_model: String,
    vision_model: String,
    cache_ttl: i64,
}

/// Where an account's LLM calls go: the Lumen gateway, with the key the Lab
/// minted for the account's team.
#[derive(Clone, PartialEq, Eq)]
pub struct AiTarget {
    pub base_url: String,
    pub api_key: String,
    pub key_id: String,
    pub default_model: String,
    pub vision_model: String,
}

impl std::fmt::Debug for AiTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AiTarget")
            .field("base_url", &self.base_url)
            .field("api_key", &"***")
            .field("key_id", &self.key_id)
            .field("default_model", &self.default_model)
            .field("vision_model", &self.vision_model)
            .finish()
    }
}

#[derive(Clone)]
struct Entry<T> {
    value: T,
    fetched: Instant,
    ttl: Duration,
    /// Set when this entry was served stale: don't ask the Lab again before.
    retry_at: Option<Instant>,
    /// A refresh-ahead was started for this entry (at most one per entry: a
    /// failed one is not repeated, the entry just expires on time).
    refresh_started: bool,
}

/// How far into its ttl a fresh entry is refreshed in the background (4/5:
/// with the default 30 s ttl, from 24 s on).
const REFRESH_AHEAD: (u32, u32) = (4, 5);

impl<T> Entry<T> {
    fn fresh(&self) -> bool {
        self.fetched.elapsed() < self.ttl
    }
    /// Fresh, but close enough to its expiry to be refreshed ahead.
    fn refresh_due(&self) -> bool {
        !self.refresh_started
            && !self.ttl.is_zero()
            && self.fetched.elapsed() >= self.ttl / REFRESH_AHEAD.1 * REFRESH_AHEAD.0
    }
    /// Servable while the Lab is unreachable: within `grace` past its ttl,
    /// and only if it was cached with a positive ttl. A `cache_ttl: 0` (or
    /// negative) answer — an expiring session or OAuth token, "do not
    /// cache" — is never served stale.
    fn servable_stale(&self, grace: Duration) -> bool {
        !self.ttl.is_zero() && self.fetched.elapsed() < self.ttl + grace
    }
    fn in_backoff(&self) -> bool {
        self.retry_at.is_some_and(|t| Instant::now() < t)
    }
}

struct Balance {
    balance: i64,
    fetched: Instant,
    ttl: Duration,
    retry_at: Option<Instant>,
}

impl Balance {
    /// Same rule as `Entry::servable_stale`: a snapshot taken from a
    /// `cache_ttl: 0` answer is never served while the Lab is unreachable.
    fn servable_stale(&self, grace: Duration) -> bool {
        !self.ttl.is_zero() && self.fetched.elapsed() < self.ttl + grace
    }
}

/// A size-bounded TTL map. On overflow, entries past `ttl + keep` (no longer
/// usable, even stale) go first, then the oldest-fetched one.
struct Cache<T> {
    map: HashMap<String, Entry<T>>,
    capacity: usize,
    keep: Duration,
}

impl<T: Clone> Cache<T> {
    fn new(capacity: usize, keep: Duration) -> Self {
        Self {
            map: HashMap::new(),
            capacity,
            keep,
        }
    }
    fn get(&self, k: &str) -> Option<Entry<T>> {
        self.map.get(k).cloned()
    }
    fn remove(&mut self, k: &str) {
        self.map.remove(k);
    }
    fn put(&mut self, k: String, value: T, ttl: Duration) {
        if !self.map.contains_key(&k) && self.map.len() >= self.capacity {
            let keep = self.keep;
            self.map
                .retain(|_, e| !e.ttl.is_zero() && e.fetched.elapsed() < e.ttl + keep);
            if self.map.len() >= self.capacity {
                if let Some(oldest) = self
                    .map
                    .iter()
                    .min_by_key(|(_, e)| e.fetched)
                    .map(|(k, _)| k.clone())
                {
                    self.map.remove(&oldest);
                }
            }
        }
        self.map.insert(
            k,
            Entry {
                value,
                fetched: Instant::now(),
                ttl,
                retry_at: None,
                refresh_started: false,
            },
        );
    }
}

/// What a Lab lookup answers: the value (`None` = a negative answer) and how
/// long it may be cached.
type Fetched<T> = Result<(Option<T>, Duration), LabError>;

/// The waiting side of a flight: `None` until the answer lands. A sender
/// dropped without an answer (the flight panicked or was aborted) closes it.
type FlightRx<T> = tokio::sync::watch::Receiver<Option<Fetched<T>>>;

/// One lookup's cache: positive answers (servable stale) and negative ones
/// (never stale), each with its own capacity, and the flights in the air
/// (at most one per key; under the same lock as the answers, so "no fresh
/// answer and no flight" is decided atomically).
struct Lookups<T> {
    positive: Cache<T>,
    negative: Cache<()>,
    flights: HashMap<String, FlightRx<T>>,
}

impl<T: Clone> Lookups<T> {
    fn new(timing: &Timing) -> Self {
        Self {
            positive: Cache::new(timing.capacity, timing.stale_grace),
            negative: Cache::new(timing.negative_capacity, Duration::ZERO),
            flights: HashMap::new(),
        }
    }

    /// Record a Lab answer asked at `started`. An error changes nothing: in
    /// particular a failed refresh leaves the entry it was refreshing with
    /// its original `fetched` and ttl.
    fn store(&mut self, key: &str, started: Instant, result: &Fetched<T>) {
        match result {
            Ok((Some(value), ttl)) => {
                // An answer cached after this one was asked for (the AI key
                // re-resolution writes directly) is the newer one.
                if self
                    .positive
                    .map
                    .get(key)
                    .is_some_and(|e| e.fetched > started)
                {
                    return;
                }
                self.negative.remove(key);
                self.positive.put(key.to_string(), value.clone(), *ttl);
            }
            Ok((None, ttl)) => {
                self.positive.remove(key);
                self.negative.put(key.to_string(), (), *ttl);
            }
            Err(_) => {}
        }
    }
}

/// Selects one of the client's caches (a plain `fn`, so a detached flight
/// can find its cache again without borrowing the caller).
type CacheOf<T> = fn(&Inner) -> &Mutex<Lookups<T>>;

fn identities(i: &Inner) -> &Mutex<Lookups<Identity>> {
    &i.identities
}
fn meili(i: &Inner) -> &Mutex<Lookups<MeiliTarget>> {
    &i.meili
}
fn ai(i: &Inner) -> &Mutex<Lookups<AiTarget>> {
    &i.ai
}

/// The landing side of a flight, owned by its task. However the task ends
/// (answer, panic, abort), the flight leaves the map *before* the sender is
/// dropped, so a waiter that wakes on a closed channel never finds the dead
/// flight again and can start a new one.
struct Landing<T> {
    inner: Arc<Inner>,
    cache: CacheOf<T>,
    key: String,
    started: Instant,
    tx: tokio::sync::watch::Sender<Option<Fetched<T>>>,
    landed: bool,
}

impl<T: Clone> Landing<T> {
    fn land(mut self, result: Fetched<T>) {
        {
            let mut c = (self.cache)(&self.inner).lock().unwrap();
            c.flights.remove(&self.key);
            c.store(&self.key, self.started, &result);
        }
        self.landed = true;
        self.tx.send_replace(Some(result));
    }
}

impl<T> Drop for Landing<T> {
    fn drop(&mut self) {
        if !self.landed {
            if let Ok(mut c) = (self.cache)(&self.inner).lock() {
                c.flights.remove(&self.key);
            }
        }
    }
}

/// The engine's client for the Lab's internal API. Cheap to share: the
/// state lives behind one `Arc`, which the detached flights also hold.
pub struct LabClient {
    inner: Arc<Inner>,
}

struct Inner {
    base: String,
    instance_id: String,
    secret: String,
    http: reqwest::Client,
    timing: Timing,
    permits: tokio::sync::Semaphore,
    /// Background refreshes in the air: a quarter of `max_in_flight`, so
    /// they never take every Lab slot from the requests that are waiting.
    refresh_slots: Arc<tokio::sync::Semaphore>,
    identities: Mutex<Lookups<Identity>>,
    meili: Mutex<Lookups<MeiliTarget>>,
    ai: Mutex<Lookups<AiTarget>>,
    balances: Mutex<HashMap<String, Balance>>,
}

/// Log a failed Lab call. Rejected instance credentials at runtime are an
/// operator error (revoked or mistyped LAB_INSTANCE_ID/LAB_INSTANCE_SECRET),
/// so it is logged at `error`; everything else is transient and logged at
/// `warn`.
pub fn log_lab_error(e: &LabError, during: &str) {
    match e {
        LabError::CredentialsRejected => tracing::error!(
            error = %e,
            during,
            "The Lab rejected this engine's LAB_INSTANCE_ID/LAB_INSTANCE_SECRET: rotate the secret on the Lab (bin/rails lab:hosted_engine:rotate ID=<LAB_INSTANCE_ID>) or mint new credentials (bin/rails lab:hosted_engine:create PRODUCT=scrapix REGION=... URL=... CREDENTIAL=<LAB_SERVICE_TOKEN>)"
        ),
        _ => tracing::warn!(error = %e, during, "Lab call failed"),
    }
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
    pub fn new(base_url: &str, instance_id: &str, instance_secret: &str) -> Self {
        Self::with_timing(base_url, instance_id, instance_secret, Timing::default())
    }

    pub fn with_timing(
        base_url: &str,
        instance_id: &str,
        instance_secret: &str,
        timing: Timing,
    ) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(timing.connect_timeout)
            .timeout(timing.request_timeout)
            // A redirect is a misconfigured LAB_URL, never followed (it
            // would also carry the instance secret elsewhere).
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client");
        let inner = Inner {
            base: base_url.trim_end_matches('/').to_string(),
            instance_id: instance_id.to_string(),
            secret: instance_secret.to_string(),
            http,
            permits: tokio::sync::Semaphore::new(timing.max_in_flight.max(1)),
            refresh_slots: Arc::new(tokio::sync::Semaphore::new(
                (timing.max_in_flight / 4).max(1),
            )),
            identities: Mutex::new(Lookups::new(&timing)),
            meili: Mutex::new(Lookups::new(&timing)),
            ai: Mutex::new(Lookups::new(&timing)),
            balances: Mutex::new(HashMap::new()),
            timing,
        };
        Self {
            inner: Arc::new(inner),
        }
    }

    pub fn base_from_events_url(events_url: &str) -> String {
        let trimmed = events_url.trim_end_matches('/');
        trimmed
            .strip_suffix("/internal/events")
            .unwrap_or(trimmed)
            .to_string()
    }

    /// `GET /internal/instances/me`: confirms the instance credentials and
    /// tells which deployment they belong to (checked once at boot).
    pub async fn instances_me(&self) -> Result<InstanceInfo, LabError> {
        self.inner.instances_me().await
    }

    pub async fn introspect(
        &self,
        kind: CredentialKind,
        credential: &str,
        account_id: Option<&str>,
    ) -> Result<Option<Identity>, LabError> {
        self.inner.introspect(kind, credential, account_id).await
    }

    pub async fn account(&self, account_id: &str) -> Result<Option<Identity>, LabError> {
        self.inner.account(account_id).await
    }

    /// Spendable credits as the Lab last reported them (from a fresh
    /// snapshot, or a new one).
    pub async fn available_credits(&self, account_id: &str) -> Result<Option<Available>, LabError> {
        self.inner.available_credits(account_id).await
    }

    /// Take a new balance snapshot now (directly, not through the identity
    /// cache: the balance must be current). While the Lab is unavailable the
    /// last snapshot is served for `stale_grace` past its expiry (unless it
    /// came from a `cache_ttl: 0` answer), asking the Lab again at most
    /// every `stale_retry`.
    pub async fn refresh_credits(&self, account_id: &str) -> Result<Option<Available>, LabError> {
        self.inner.refresh_credits(account_id).await
    }

    pub async fn meilisearch(
        &self,
        account_id: &str,
        url: Option<&str>,
    ) -> Result<Option<MeiliTarget>, LabError> {
        self.inner.meilisearch(account_id, url).await
    }

    /// `GET /internal/accounts/{id}/ai`: the Lumen target for this account's
    /// LLM calls; `None` when the Lab has none for it (404). A
    /// `rejected_key_id` (the key Lumen just refused) skips the cache, asks
    /// the Lab for a replacement and makes that the cached answer.
    pub async fn ai(
        &self,
        account_id: &str,
        rejected_key_id: Option<&str>,
    ) -> Result<Option<AiTarget>, LabError> {
        self.inner.ai(account_id, rejected_key_id).await
    }

    #[cfg(test)]
    pub fn debug_cache_keys(&self) -> Vec<String> {
        let c = self.inner.identities.lock().unwrap();
        c.positive
            .map
            .keys()
            .chain(c.negative.map.keys())
            .cloned()
            .collect()
    }

    #[cfg(test)]
    pub fn cached_ttl_for_test(
        &self,
        kind: CredentialKind,
        credential: &str,
        account_id: Option<&str>,
    ) -> Option<Duration> {
        let key = hashed(&["i", kind.as_str(), credential, account_id.unwrap_or("")]);
        self.inner
            .identities
            .lock()
            .unwrap()
            .positive
            .get(&key)
            .map(|e| e.ttl)
    }
}

impl Inner {
    fn ttl_from(&self, secs: Option<i64>) -> Duration {
        match secs {
            None => self.timing.default_ttl,
            Some(s) if s <= 0 => Duration::ZERO,
            Some(s) => Duration::from_secs(s as u64).min(self.timing.max_ttl),
        }
    }

    /// One Lab call: status and body, read while holding a concurrency slot.
    async fn request(
        &self,
        endpoint: &'static str,
        req: reqwest::RequestBuilder,
    ) -> Result<(u16, Bytes), LabError> {
        let _permit =
            match tokio::time::timeout(self.timing.permit_wait, self.permits.acquire()).await {
                Ok(Ok(permit)) => permit,
                _ => {
                    count(endpoint, "unavailable");
                    return Err(LabError::Unavailable("Lab client saturated".into()));
                }
            };
        let result = match req
            .header("X-Lab-Instance-Id", &self.instance_id)
            .bearer_auth(&self.secret)
            .send()
            .await
        {
            Err(e) => Err(LabError::Unavailable(e.to_string())),
            Ok(resp) => match resp.status().as_u16() {
                401 => Err(LabError::CredentialsRejected),
                // The Meilisearch and AI lookups' 404 means "no engine" / "no AI",
                // handled by the caller.
                404 if matches!(endpoint, "meilisearch" | "ai") => Ok(resp),
                c @ 300..=499 => Err(LabError::BadResponse(c)),
                s if s >= 500 => Err(LabError::Unavailable(format!("HTTP {s}"))),
                _ => Ok(resp),
            },
        };
        match &result {
            Err(LabError::Unavailable(_)) => count(endpoint, "unavailable"),
            Err(LabError::CredentialsRejected | LabError::BadResponse(_)) => {
                count(endpoint, "rejected")
            }
            Ok(_) => {}
        }
        let resp = result?;
        let status = resp.status().as_u16();
        let body = resp.bytes().await.map_err(|e| {
            count(endpoint, "unavailable");
            LabError::Unavailable(format!("reading the answer: {e}"))
        })?;
        Ok((status, body))
    }

    async fn answer(
        &self,
        endpoint: &'static str,
        req: reqwest::RequestBuilder,
    ) -> Result<Answer, LabError> {
        let (status, body) = self.request(endpoint, req).await?;
        if !(200..300).contains(&status) {
            count(endpoint, "unavailable");
            return Err(LabError::Unavailable(format!("HTTP {status}")));
        }
        let a: Answer = serde_json::from_slice(&body).map_err(|e| {
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

    /// Take a new balance snapshot. Usage this engine recorded since is in
    /// the outbox and is debited by the Lab when delivered; the engine does
    /// not price it (decision B), so the snapshot is the Lab's number.
    fn remember_balance(&self, account_id: &str, credits: &Option<Credits>, ttl: Duration) {
        if let Some(c) = credits {
            self.balances.lock().unwrap().insert(
                account_id.to_string(),
                Balance {
                    balance: c.balance,
                    fetched: Instant::now(),
                    ttl,
                    retry_at: None,
                },
            );
        }
    }

    /// Cached lookup with the stale-on-error rule shared by every endpoint:
    /// only a *positive* cached answer with a positive ttl is ever served
    /// stale (never a `cache_ttl: 0` one), and a key just served stale is
    /// not retried for `stale_retry`.
    ///
    /// Asking the Lab is single-flight per key: the first caller starts a
    /// flight (a detached task, so a caller that goes away neither cancels
    /// the call nor strands the others) and everyone asking for that key
    /// meanwhile waits for its answer. Two exceptions keep "do not cache"
    /// strict: a key whose last answer had `cache_ttl: 0` is asked by every
    /// caller for itself, and a caller that joined a flight someone else
    /// started does not use a `cache_ttl: 0` answer from it (the Lab may
    /// have computed it before this caller asked) but asks again.
    ///
    /// A fresh entry past [`REFRESH_AHEAD`] of its ttl is returned at once
    /// and refreshed by a background flight: at most one per entry, only on
    /// access, and only while a refresh slot is free.
    async fn cached<T, F, Fut>(
        self: &Arc<Self>,
        endpoint: &'static str,
        cache: CacheOf<T>,
        key: String,
        fetch: F,
    ) -> Result<Option<T>, LabError>
    where
        T: Clone + Send + Sync + 'static,
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Fetched<T>> + Send + 'static,
    {
        let grace = self.timing.stale_grace;
        // A flight that ends without an answer (it panicked, or the runtime
        // is shutting down) is replaced once.
        for _ in 0..2 {
            let (hit, flight) = {
                let mut c = cache(self).lock().unwrap();
                if c.negative.get(&key).is_some_and(|e| e.fresh()) {
                    return Ok(None);
                }
                let hit = c.positive.get(&key);
                if let Some(e) = &hit {
                    if e.fresh() {
                        if e.refresh_due() && !c.flights.contains_key(&key) {
                            if let Ok(slot) = self.refresh_slots.clone().try_acquire_owned() {
                                if let Some(entry) = c.positive.map.get_mut(&key) {
                                    entry.refresh_started = true;
                                }
                                self.take_off(&mut c, cache, &key, fetch(), Some(slot));
                            }
                        }
                        return Ok(Some(e.value.clone()));
                    }
                    if e.in_backoff() && e.servable_stale(grace) {
                        count(endpoint, "stale");
                        return Ok(Some(e.value.clone()));
                    }
                }
                let flight = if hit.as_ref().is_some_and(|e| e.ttl.is_zero()) {
                    None
                } else if let Some(rx) = c.flights.get(&key) {
                    Some((rx.clone(), false))
                } else {
                    Some((self.take_off(&mut c, cache, &key, fetch(), None), true))
                };
                (hit, flight)
            };
            let shared = match flight {
                None => None,
                Some((mut rx, mine)) => {
                    let landed = rx
                        .wait_for(|answer| answer.is_some())
                        .await
                        .ok()
                        .and_then(|answer| answer.clone());
                    match landed {
                        None => continue,
                        Some(Ok((Some(_), ttl))) if !mine && ttl.is_zero() => None,
                        Some(result) => Some(result),
                    }
                }
            };
            let result = match shared {
                Some(result) => result,
                None => {
                    let started = Instant::now();
                    let result = fetch().await;
                    cache(self).lock().unwrap().store(&key, started, &result);
                    result
                }
            };
            return match result {
                Ok((value, _)) => Ok(value),
                Err(LabError::Unavailable(m)) => match hit {
                    Some(e) if e.servable_stale(grace) => {
                        tracing::warn!(error = %m, "Lab unreachable; serving a cached answer");
                        count(endpoint, "stale");
                        let mut c = cache(self).lock().unwrap();
                        if let Some(entry) = c.positive.map.get_mut(&key) {
                            // Only the entry we served (not one refreshed meanwhile);
                            // `fetched` is kept, so the stale window still ends on time.
                            if entry.fetched == e.fetched {
                                entry.retry_at = Some(Instant::now() + self.timing.stale_retry);
                            }
                        }
                        Ok(Some(e.value))
                    }
                    _ => Err(LabError::Unavailable(m)),
                },
                Err(e) => Err(e),
            };
        }
        Err(LabError::Unavailable("Lab lookup interrupted".into()))
    }

    /// Start the flight for `key` (the caller holds the cache lock and saw
    /// none in the air). `slot` is a background refresh's slot, held until
    /// it lands.
    fn take_off<T, Fut>(
        self: &Arc<Self>,
        c: &mut Lookups<T>,
        cache: CacheOf<T>,
        key: &str,
        fetch: Fut,
        slot: Option<tokio::sync::OwnedSemaphorePermit>,
    ) -> FlightRx<T>
    where
        T: Clone + Send + Sync + 'static,
        Fut: std::future::Future<Output = Fetched<T>> + Send + 'static,
    {
        let (tx, rx) = tokio::sync::watch::channel(None);
        c.flights.insert(key.to_string(), rx.clone());
        let landing = Landing {
            inner: self.clone(),
            cache,
            key: key.to_string(),
            started: Instant::now(),
            tx,
            landed: false,
        };
        tokio::spawn(async move {
            let _slot = slot;
            let result = fetch.await;
            landing.land(result);
        });
        rx
    }

    async fn to_identity(&self, endpoint: &'static str, a: Answer) -> (Option<Identity>, Duration) {
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
                limits: a.limits,
            }),
            ttl,
        )
    }

    async fn instances_me(&self) -> Result<InstanceInfo, LabError> {
        let (status, body) = self
            .request(
                "instances_me",
                self.http
                    .get(format!("{}/internal/instances/me", self.base)),
            )
            .await?;
        if !(200..300).contains(&status) {
            count("instances_me", "unavailable");
            return Err(LabError::Unavailable(format!("HTTP {status}")));
        }
        let me: InstanceInfo = serde_json::from_slice(&body).map_err(|e| {
            count("instances_me", "unavailable");
            LabError::Unavailable(format!("malformed: {e}"))
        })?;
        count("instances_me", "ok");
        Ok(me)
    }

    async fn introspect(
        self: &Arc<Self>,
        kind: CredentialKind,
        credential: &str,
        account_id: Option<&str>,
    ) -> Result<Option<Identity>, LabError> {
        let key = hashed(&["i", kind.as_str(), credential, account_id.unwrap_or("")]);
        let body = serde_json::json!({"kind": kind.as_str(), "credential": credential, "account_id": account_id});
        self.cached("introspect", identities, key, || {
            let inner = self.clone();
            let req = self
                .http
                .post(format!("{}/internal/auth/introspect", self.base))
                .json(&body);
            async move {
                let a = inner.answer("introspect", req).await?;
                Ok(inner.to_identity("introspect", a).await)
            }
        })
        .await
    }

    async fn account(self: &Arc<Self>, account_id: &str) -> Result<Option<Identity>, LabError> {
        let key = hashed(&["a", account_id]);
        self.cached("account", identities, key, || {
            let inner = self.clone();
            let account_id = account_id.to_string();
            async move {
                let a = inner
                    .answer(
                        "account",
                        inner
                            .http
                            .get(format!("{}/internal/accounts/{account_id}", inner.base)),
                    )
                    .await?;
                let (identity, ttl) = inner.to_identity("account", a).await;
                if identity.is_none() {
                    inner.forget_balance(&account_id);
                }
                Ok((identity, ttl))
            }
        })
        .await
    }

    /// An inactive account must never keep a positive balance snapshot.
    fn forget_balance(&self, account_id: &str) {
        self.balances.lock().unwrap().remove(account_id);
    }

    async fn available_credits(&self, account_id: &str) -> Result<Option<Available>, LabError> {
        if let Some(b) = self.balances.lock().unwrap().get(account_id) {
            if b.fetched.elapsed() < b.ttl {
                return Ok(Some(Available {
                    credits: b.balance,
                    cached: true,
                }));
            }
        }
        self.refresh_credits(account_id).await
    }

    async fn refresh_credits(&self, account_id: &str) -> Result<Option<Available>, LabError> {
        let stale = |b: &Balance| b.servable_stale(self.timing.stale_grace);
        let served = |credits| {
            Ok(Some(Available {
                credits,
                cached: false,
            }))
        };
        if let Some(b) = self.balances.lock().unwrap().get(account_id) {
            if b.retry_at.is_some_and(|t| Instant::now() < t) && stale(b) {
                count("account", "stale");
                return served(b.balance);
            }
        }
        match self
            .answer(
                "account",
                self.http
                    .get(format!("{}/internal/accounts/{account_id}", self.base)),
            )
            .await
        {
            Ok(a) => {
                let (id, _) = self.to_identity("account", a).await;
                if id.is_none() {
                    self.forget_balance(account_id);
                    return Ok(None);
                }
                match self.balances.lock().unwrap().get(account_id) {
                    Some(b) => served(b.balance),
                    None => Ok(None),
                }
            }
            Err(LabError::Unavailable(m)) => {
                let mut guard = self.balances.lock().unwrap();
                match guard.get_mut(account_id) {
                    Some(b) if stale(b) => {
                        count("account", "stale");
                        b.retry_at = Some(Instant::now() + self.timing.stale_retry);
                        served(b.balance)
                    }
                    _ => Err(LabError::Unavailable(m)),
                }
            }
            Err(e) => Err(e),
        }
    }

    async fn meilisearch(
        self: &Arc<Self>,
        account_id: &str,
        url: Option<&str>,
    ) -> Result<Option<MeiliTarget>, LabError> {
        let key = hashed(&[
            "m",
            account_id,
            url.map(|u| u.trim_end_matches('/')).unwrap_or(""),
        ]);
        let meili_ttl = self.timing.meili_ttl;
        let negative_ttl = self.timing.negative_ttl;
        self.cached("meilisearch", meili, key, || {
            let inner = self.clone();
            let mut req = self.http.get(format!(
                "{}/internal/accounts/{account_id}/meilisearch",
                self.base
            ));
            if let Some(u) = url {
                req = req.query(&[("url", u)]);
            }
            async move {
                let (status, body) = inner.request("meilisearch", req).await?;
                if status == 404 {
                    // "No engine" is a negative answer: re-asked after 5 s, so
                    // an engine added in Settings is picked up quickly.
                    count("meilisearch", "inactive");
                    return Ok((None, negative_ttl));
                }
                if !(200..300).contains(&status) {
                    count("meilisearch", "unavailable");
                    return Err(LabError::Unavailable(format!("HTTP {status}")));
                }
                let m: MeiliAnswer = serde_json::from_slice(&body).map_err(|e| {
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
            }
        })
        .await
    }

    async fn ai(
        self: &Arc<Self>,
        account_id: &str,
        rejected_key_id: Option<&str>,
    ) -> Result<Option<AiTarget>, LabError> {
        let key = hashed(&["ai", account_id]);
        let Some(rejected) = rejected_key_id else {
            return self
                .cached("ai", ai, key, || {
                    let inner = self.clone();
                    let account_id = account_id.to_string();
                    async move { inner.fetch_ai(&account_id, None).await }
                })
                .await;
        };
        // Never served stale: the cached entry is the key that was refused.
        // Not a flight either: the answer depends on the refused key, and it
        // replaces whatever a flight asked for earlier lands with.
        let (target, ttl) = self.fetch_ai(account_id, Some(rejected)).await?;
        let mut c = self.ai.lock().unwrap();
        match &target {
            Some(t) => {
                c.negative.remove(&key);
                c.positive.put(key, t.clone(), ttl);
            }
            None => {
                c.positive.remove(&key);
                c.negative.put(key, (), ttl);
            }
        }
        Ok(target)
    }

    async fn fetch_ai(
        &self,
        account_id: &str,
        rejected_key_id: Option<&str>,
    ) -> Result<(Option<AiTarget>, Duration), LabError> {
        let mut req = self
            .http
            .get(format!("{}/internal/accounts/{account_id}/ai", self.base));
        if let Some(id) = rejected_key_id {
            req = req.query(&[("rejected_key_id", id)]);
        }
        let (status, body) = self.request("ai", req).await?;
        if status == 404 {
            // "No AI for this account" is a negative answer, re-asked after
            // 5 s so one enabled in the Lab is picked up quickly.
            count("ai", "inactive");
            return Ok((None, self.timing.negative_ttl));
        }
        if !(200..300).contains(&status) {
            count("ai", "unavailable");
            return Err(LabError::Unavailable(format!("HTTP {status}")));
        }
        let a: AiAnswer = serde_json::from_slice(&body).map_err(|e| {
            count("ai", "unavailable");
            LabError::Unavailable(format!("malformed: {e}"))
        })?;
        count("ai", "ok");
        Ok((
            Some(AiTarget {
                base_url: a.base_url,
                api_key: a.api_key,
                key_id: a.key_id,
                default_model: a.default_model,
                vision_model: a.vision_model,
            }),
            self.ttl_from(Some(a.cache_ttl)),
        ))
    }
}

#[cfg(any(test, feature = "testing"))]
pub mod testing {
    //! In-process fake of the Lab's internal API
    //! (contracts/vendor/lab/lab-internal.openapi.json).
    use super::AiTarget;
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
            atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering},
            Arc, Mutex,
        },
    };

    pub const INSTANCE_ID: &str = "0f0f0f0f-0f0f-4f0f-8f0f-0f0f0f0f0f0f";
    pub const SECRET: &str = "abababababababababababababababababababababababababababababababab";

    #[derive(Default)]
    pub struct FakeLabState {
        /// "kind:credential" → introspection JSON.
        pub credentials: Mutex<HashMap<String, Value>>,
        pub accounts: Mutex<HashMap<String, Value>>,
        /// "account|url" (url "" = default) → {"id","url","api_key"}.
        pub meili: Mutex<HashMap<String, Value>>,
        /// account → `/ai` answer ({base_url, api_key, key_id, default_model,
        /// vision_model, cache_ttl}).
        pub ai: Mutex<HashMap<String, Value>>,
        /// account → the answer served (and then made current) to a call that
        /// carries a `rejected_key_id`.
        pub ai_rotated: Mutex<HashMap<String, Value>>,
        /// The `rejected_key_id` of every `/ai` call, in order.
        pub ai_queries: Mutex<Vec<Option<String>>>,
        /// `cache_ttl` (seconds) of the answers `set_ai` / `rotate_ai` build.
        pub ai_cache_ttl: AtomicI64,
        pub down: AtomicBool,
        pub slow_ms: AtomicUsize,
        pub malformed: AtomicBool,
        /// Non-zero: every gated route answers this status (after the down check).
        pub status_override: AtomicUsize,
        pub calls: AtomicUsize,
        /// `GET /internal/instances/me` answer (`FakeLab::hosted_me()` by default).
        pub me: Mutex<Value>,
    }

    pub struct FakeLab {
        pub url: String,
        pub state: Arc<FakeLabState>,
    }

    fn authorized(h: &HeaderMap) -> bool {
        h.get("authorization").and_then(|v| v.to_str().ok())
            == Some(format!("Bearer {SECRET}").as_str())
            && h.get("x-lab-instance-id").and_then(|v| v.to_str().ok()) == Some(INSTANCE_ID)
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
        pub async fn start() -> FakeLab {
            let state = Arc::new(FakeLabState {
                me: Mutex::new(FakeLab::hosted_me()),
                ai_cache_ttl: AtomicI64::new(30),
                ..Default::default()
            });
            let app =
                Router::new()
                    .route(
                        "/internal/instances/me",
                        get(
                            |State(s): State<Arc<FakeLabState>>, h: HeaderMap| async move {
                                gate(&s, &h).await?;
                                Ok::<_, StatusCode>(Json(s.me.lock().unwrap().clone()))
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
                    .route(
                        "/internal/accounts/{id}/ai",
                        get(
                            |State(s): State<Arc<FakeLabState>>,
                             h: HeaderMap,
                             Path(id): Path<String>,
                             Query(q): Query<HashMap<String, String>>| async move {
                                gate(&s, &h).await?;
                                let rejected = q.get("rejected_key_id").cloned();
                                s.ai_queries.lock().unwrap().push(rejected.clone());
                                if rejected.is_some() {
                                    if let Some(next) = s.ai_rotated.lock().unwrap().get(&id) {
                                        s.ai.lock().unwrap().insert(id.clone(), next.clone());
                                    }
                                }
                                s.ai.lock()
                                    .unwrap()
                                    .get(&id)
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

        pub fn hosted_me() -> Value {
            json!({"instance_id": INSTANCE_ID, "kind": "hosted", "product": "scrapix",
                   "region": "eu-west-1", "lab_url": "http://lab"})
        }
        pub fn identity(account: &str, tier: &str, balance: i64) -> Value {
            let limits = match tier {
                "pro" => {
                    json!({"concurrent_jobs": 10, "rate_limit_rpm": 1200, "max_depth": 10, "js_rendering": true})
                }
                _ => {
                    json!({"concurrent_jobs": 1, "rate_limit_rpm": 60, "max_depth": 3, "js_rendering": false})
                }
            };
            json!({"active": true, "account_id": account, "tier": tier, "role": null, "api_key_id": null,
                   "principal": {"type": "api_key", "user_id": null}, "credits": {"balance": balance},
                   "limits": limits})
        }
        pub fn set_credential(&self, kind: &str, credential: &str, v: Value) {
            self.state
                .credentials
                .lock()
                .unwrap()
                .insert(format!("{kind}:{credential}"), v);
        }
        pub fn set_account(&self, account: &str, v: Value) {
            self.state
                .accounts
                .lock()
                .unwrap()
                .insert(account.to_string(), v);
        }
        fn ai_json(&self, t: &AiTarget) -> Value {
            json!({"base_url": t.base_url, "api_key": t.api_key, "key_id": t.key_id,
                   "default_model": t.default_model, "vision_model": t.vision_model,
                   "cache_ttl": self.state.ai_cache_ttl.load(Ordering::SeqCst)})
        }
        /// The account's `/ai` answer (`None` → 404).
        pub fn set_ai(&self, account: &str, target: Option<AiTarget>) {
            let mut ai = self.state.ai.lock().unwrap();
            match target {
                Some(t) => ai.insert(account.to_string(), self.ai_json(&t)),
                None => ai.remove(account),
            };
        }
        /// What a call carrying a `rejected_key_id` is served (and is current
        /// from then on).
        pub fn rotate_ai(&self, account: &str, target: AiTarget) {
            self.state
                .ai_rotated
                .lock()
                .unwrap()
                .insert(account.to_string(), self.ai_json(&target));
        }
        pub fn set_ai_cache_ttl(&self, secs: i64) {
            self.state.ai_cache_ttl.store(secs, Ordering::SeqCst)
        }
        pub fn ai_queries(&self) -> Vec<Option<String>> {
            self.state.ai_queries.lock().unwrap().clone()
        }
        pub fn calls(&self) -> usize {
            self.state.calls.load(Ordering::SeqCst)
        }
        pub fn set_down(&self, down: bool) {
            self.state.down.store(down, Ordering::SeqCst)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{FakeLab, INSTANCE_ID, SECRET};
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

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
            negative_capacity: 3,
            stale_retry: Duration::from_millis(100),
            max_in_flight: 32,
            permit_wait: Duration::from_millis(100),
        }
    }

    async fn credits(c: &LabClient, account: &str) -> Result<Option<i64>, LabError> {
        c.available_credits(account)
            .await
            .map(|a| a.map(|a| a.credits))
    }

    async fn setup() -> (FakeLab, LabClient) {
        let lab = FakeLab::start().await;
        let client = LabClient::with_timing(&lab.url, INSTANCE_ID, SECRET, fast());
        (lab, client)
    }

    #[tokio::test]
    async fn instances_me_describes_the_deployment_and_a_wrong_secret_is_rejected() {
        let lab = FakeLab::start().await;
        let me = LabClient::with_timing(&lab.url, INSTANCE_ID, SECRET, fast())
            .instances_me()
            .await
            .unwrap();
        assert_eq!(me.instance_id.as_deref(), Some(INSTANCE_ID));
        assert_eq!(me.kind, "hosted");
        assert_eq!(me.product, "scrapix");
        assert_eq!(me.region.as_deref(), Some("eu-west-1"));
        let err = LabClient::with_timing(&lab.url, INSTANCE_ID, "wrong", fast())
            .instances_me()
            .await
            .unwrap_err();
        assert_eq!(err, LabError::CredentialsRejected);
        let err = LabClient::with_timing(
            &lab.url,
            "22222222-2222-4222-8222-222222222222",
            SECRET,
            fast(),
        )
        .instances_me()
        .await
        .unwrap_err();
        assert_eq!(
            err,
            LabError::CredentialsRejected,
            "the id is part of the credential"
        );
    }

    #[test]
    fn instances_me_accepts_a_null_instance_id_and_region() {
        // The Lab's legacy service-token path has no deployment row.
        let me: InstanceInfo = serde_json::from_value(json!({
            "instance_id": null, "kind": "hosted", "product": "scrapix",
            "region": null, "lab_url": "https://lab.example"
        }))
        .unwrap();
        assert_eq!(me.instance_id, None);
        assert_eq!(me.region, None);
        assert_eq!(me.kind, "hosted");
        assert_eq!(me.product, "scrapix");
        assert_eq!(me.lab_url, "https://lab.example");
    }

    #[tokio::test]
    async fn every_call_carries_the_instance_id_header() {
        let (lab, c) = setup().await;
        lab.set_credential("api_key", "sk_live_a", FakeLab::identity(ACCT, "pro", 50));
        assert!(c
            .introspect(CredentialKind::ApiKey, "sk_live_a", None)
            .await
            .unwrap()
            .is_some());
        assert!(c.account(ACCT).await.is_ok());
        // `authorized` in the fake requires X-Lab-Instance-Id on every gated
        // route; a client without it would have been 401 (CredentialsRejected).
        assert_eq!(lab.calls(), 2);
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
        assert_eq!(
            id.limits,
            Some(scrapix_auth::Limits {
                concurrent_jobs: 10,
                rate_limit_rpm: 1200,
                max_depth: 10,
                js_rendering: true
            })
        );
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

    /// An answer with `cache_ttl: 0` (a session or OAuth token about to
    /// expire, "do not cache") is never served stale: with the Lab down the
    /// next call is `Unavailable`, not the cached identity.
    #[tokio::test]
    async fn a_cache_ttl_0_answer_is_never_served_stale() {
        let (lab, c) = setup().await; // grace 400 ms
        let mut v = FakeLab::identity(ACCT, "pro", 50);
        v["cache_ttl"] = json!(0);
        lab.set_credential("session", "jwt_expiring", v);
        assert!(c
            .introspect(CredentialKind::Session, "jwt_expiring", None)
            .await
            .unwrap()
            .is_some());
        lab.set_down(true);
        assert!(
            matches!(
                c.introspect(CredentialKind::Session, "jwt_expiring", None)
                    .await,
                Err(LabError::Unavailable(_))
            ),
            "well inside the stale grace, yet not served"
        );

        // A credential first cached with a positive ttl, then re-answered
        // with cache_ttl 0: the zero-ttl answer replaces it and is not
        // served stale either.
        lab.set_down(false);
        lab.set_credential("bearer", "tok", FakeLab::identity(ACCT, "pro", 50));
        c.introspect(CredentialKind::Bearer, "tok", None)
            .await
            .unwrap()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(250)).await; // past the 200 ms ttl
        let mut v = FakeLab::identity(ACCT, "pro", 50);
        v["cache_ttl"] = json!(0);
        lab.set_credential("bearer", "tok", v);
        c.introspect(CredentialKind::Bearer, "tok", None)
            .await
            .unwrap()
            .unwrap();
        lab.set_down(true);
        assert!(matches!(
            c.introspect(CredentialKind::Bearer, "tok", None).await,
            Err(LabError::Unavailable(_))
        ));
    }

    /// Same rule for the balance snapshot: one taken from a `cache_ttl: 0`
    /// answer is not served while the Lab is down.
    #[tokio::test]
    async fn a_cache_ttl_0_balance_is_never_served_stale() {
        let (lab, c) = setup().await;
        lab.set_account(
            ACCT,
            json!({"active": true, "account_id": ACCT, "tier": "pro",
                   "credits": {"balance": 100}, "cache_ttl": 0}),
        );
        assert_eq!(credits(&c, ACCT).await.unwrap(), Some(100));
        lab.set_down(true);
        assert!(matches!(
            credits(&c, ACCT).await,
            Err(LabError::Unavailable(_))
        ));
        assert!(matches!(
            c.account(ACCT).await,
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
    async fn balance_is_the_labs_number_and_refreshes_after_ttl() {
        let (lab, c) = setup().await;
        lab.set_account(
            ACCT,
            json!({"active": true, "account_id": ACCT, "tier": "pro", "credits": {"balance": 100}}),
        );
        assert_eq!(credits(&c, ACCT).await.unwrap(), Some(100));
        lab.set_account(
            ACCT,
            json!({"active": true, "account_id": ACCT, "tier": "pro", "credits": {"balance": 70}}),
        );
        assert_eq!(credits(&c, ACCT).await.unwrap(), Some(100), "cached");
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(credits(&c, ACCT).await.unwrap(), Some(70), "fresh snapshot");
    }

    #[tokio::test]
    async fn inactive_account_never_regains_a_balance_from_the_stale_snapshot() {
        let (lab, c) = setup().await;
        let active =
            json!({"active": true, "account_id": ACCT, "tier": "pro", "credits": {"balance": 100}});
        lab.set_account(ACCT, active.clone());
        assert_eq!(credits(&c, ACCT).await.unwrap(), Some(100));
        lab.set_account(ACCT, json!({"active": false}));
        tokio::time::sleep(Duration::from_millis(250)).await; // past ttl
        assert_eq!(credits(&c, ACCT).await.unwrap(), None);
        lab.set_down(true);
        let r = credits(&c, ACCT).await;
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
        let r = credits(&c, ACCT).await;
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
    async fn a_404_on_instances_me_is_bad_response() {
        let (lab, c) = setup().await;
        lab.state
            .status_override
            .store(404, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            c.instances_me().await.unwrap_err(),
            LabError::BadResponse(404)
        );
    }

    #[tokio::test]
    async fn unknown_account_has_no_balance() {
        let (_lab, c) = setup().await;
        assert_eq!(credits(&c, ACCT).await.unwrap(), None);
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

    /// Important 3: "no engine" is a negative answer, re-asked after
    /// `negative_ttl`, not the positive `meili_ttl`.
    #[tokio::test]
    async fn missing_meilisearch_engine_is_cached_briefly() {
        let (lab, c) = setup().await; // negative_ttl 100 ms, meili_ttl 200 ms
        assert!(c.meilisearch(ACCT, None).await.unwrap().is_none());
        assert!(c.meilisearch(ACCT, None).await.unwrap().is_none());
        assert_eq!(lab.calls(), 1, "negative cached");
        lab.state.meili.lock().unwrap().insert(
            format!("{ACCT}|"),
            json!({"id": "e1", "url": "http://m:7700", "api_key": "k"}),
        );
        tokio::time::sleep(Duration::from_millis(150)).await; // past negative, inside meili_ttl
        assert_eq!(
            c.meilisearch(ACCT, None).await.unwrap().unwrap().url,
            "http://m:7700"
        );
        assert_eq!(lab.calls(), 2);
        c.meilisearch(ACCT, None).await.unwrap();
        assert_eq!(lab.calls(), 2, "positive cached");
    }

    fn ai_target(key_id: &str) -> AiTarget {
        AiTarget {
            base_url: "https://lumen.test/v1".into(),
            api_key: format!("sk-lumen-{key_id}"),
            key_id: key_id.into(),
            default_model: "gpt-x".into(),
            vision_model: "gpt-x-vision".into(),
        }
    }

    #[tokio::test]
    async fn ai_returns_the_target_and_caches_it() {
        let (lab, c) = setup().await;
        lab.set_ai(ACCT, Some(ai_target("k-1")));
        assert_eq!(c.ai(ACCT, None).await.unwrap(), Some(ai_target("k-1")));
        assert_eq!(c.ai(ACCT, None).await.unwrap(), Some(ai_target("k-1")));
        assert_eq!(lab.calls(), 1, "second call served from the cache");
        assert_eq!(lab.ai_queries(), vec![None]);
    }

    #[tokio::test]
    async fn ai_honours_cache_ttl() {
        let (lab, c) = setup().await;
        lab.set_ai_cache_ttl(0);
        lab.set_ai(ACCT, Some(ai_target("k-1")));
        c.ai(ACCT, None).await.unwrap();
        c.ai(ACCT, None).await.unwrap();
        assert_eq!(lab.calls(), 2, "cache_ttl 0 is never cached");
    }

    #[tokio::test]
    async fn ai_not_found_is_none() {
        let (lab, c) = setup().await;
        assert_eq!(c.ai(ACCT, None).await.unwrap(), None);
        assert_eq!(lab.calls(), 1);
    }

    #[tokio::test]
    async fn ai_rejected_key_bypasses_the_cache() {
        let (lab, c) = setup().await;
        lab.set_ai(ACCT, Some(ai_target("k-1")));
        lab.rotate_ai(ACCT, ai_target("k-2"));
        assert_eq!(c.ai(ACCT, None).await.unwrap(), Some(ai_target("k-1")));
        assert_eq!(
            c.ai(ACCT, Some("k-1")).await.unwrap(),
            Some(ai_target("k-2")),
            "the cached k-1 is bypassed"
        );
        assert_eq!(lab.ai_queries(), vec![None, Some("k-1".to_string())]);
        assert_eq!(c.ai(ACCT, None).await.unwrap(), Some(ai_target("k-2")));
        assert_eq!(lab.calls(), 2, "the replacement is now the cached entry");
    }

    #[tokio::test]
    async fn ai_serves_stale_while_the_lab_is_down() {
        let lab = FakeLab::start().await;
        let timing = Timing {
            stale_grace: Duration::from_secs(5),
            ..fast()
        };
        let c = LabClient::with_timing(&lab.url, INSTANCE_ID, SECRET, timing);
        lab.set_ai_cache_ttl(1);
        lab.set_ai(ACCT, Some(ai_target("k-1")));
        c.ai(ACCT, None).await.unwrap();
        lab.set_down(true);
        tokio::time::sleep(Duration::from_millis(1100)).await; // past the 1 s ttl
        assert_eq!(c.ai(ACCT, None).await.unwrap(), Some(ai_target("k-1")));

        let never_cached = "22222222-2222-2222-2222-222222222222";
        assert!(matches!(
            c.ai(never_cached, None).await,
            Err(LabError::Unavailable(_))
        ));
    }

    #[test]
    fn ai_debug_redacts_the_key() {
        let shown = format!("{:?}", ai_target("k-1"));
        assert!(!shown.contains("sk-lumen-k-1"), "{shown}");
        assert!(shown.contains("***"), "{shown}");
        assert!(shown.contains("k-1") && shown.contains("gpt-x"), "{shown}");
    }

    /// Important 4a: a key just served stale is not retried for
    /// `stale_retry`, and the stale window still ends at the original expiry.
    #[tokio::test]
    async fn a_stale_key_backs_off_before_asking_the_lab_again() {
        let (lab, c) = setup().await; // ttl 200, grace 400, stale_retry 100 (ms)
        lab.set_credential("api_key", "sk_live_a", FakeLab::identity(ACCT, "pro", 50));
        c.introspect(CredentialKind::ApiKey, "sk_live_a", None)
            .await
            .unwrap();
        lab.set_down(true);
        tokio::time::sleep(Duration::from_millis(250)).await; // past ttl
        for _ in 0..5 {
            assert!(c
                .introspect(CredentialKind::ApiKey, "sk_live_a", None)
                .await
                .unwrap()
                .is_some());
        }
        assert_eq!(lab.calls(), 2, "one failed retry, then served stale");
        tokio::time::sleep(Duration::from_millis(120)).await; // backoff over
        assert!(c
            .introspect(CredentialKind::ApiKey, "sk_live_a", None)
            .await
            .unwrap()
            .is_some());
        assert_eq!(lab.calls(), 3, "retried after the backoff");
        tokio::time::sleep(Duration::from_millis(300)).await; // 670 ms: past ttl + grace
        assert!(matches!(
            c.introspect(CredentialKind::ApiKey, "sk_live_a", None)
                .await,
            Err(LabError::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn a_stale_balance_backs_off_before_asking_the_lab_again() {
        let (lab, c) = setup().await;
        lab.set_account(
            ACCT,
            json!({"active": true, "account_id": ACCT, "tier": "pro", "credits": {"balance": 100}}),
        );
        assert_eq!(credits(&c, ACCT).await.unwrap(), Some(100));
        lab.set_down(true);
        tokio::time::sleep(Duration::from_millis(250)).await;
        for _ in 0..5 {
            assert_eq!(credits(&c, ACCT).await.unwrap(), Some(100));
        }
        assert_eq!(lab.calls(), 2);
    }

    /// Important 4b: at most `max_in_flight` concurrent Lab calls; a caller
    /// waits `permit_wait` for a slot, then fails closed.
    #[tokio::test]
    async fn concurrent_lab_calls_are_capped() {
        let lab = FakeLab::start().await;
        let c = Arc::new(LabClient::with_timing(
            &lab.url,
            INSTANCE_ID,
            SECRET,
            Timing {
                max_in_flight: 1,
                request_timeout: Duration::from_secs(2),
                ..fast()
            },
        ));
        lab.state
            .slow_ms
            .store(500, std::sync::atomic::Ordering::SeqCst);
        let first = {
            let c = c.clone();
            tokio::spawn(async move { c.introspect(CredentialKind::ApiKey, "k1", None).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await; // k1 holds the only slot
        let started = Instant::now();
        let second = c.introspect(CredentialKind::ApiKey, "k2", None).await;
        assert_eq!(
            second,
            Err(LabError::Unavailable("Lab client saturated".into()))
        );
        assert!(
            started.elapsed() < Duration::from_millis(400),
            "gave up at permit_wait"
        );
        assert_eq!(
            first.await.unwrap(),
            Ok(None),
            "the call holding the slot completes"
        );
        assert_eq!(lab.calls(), 1, "the saturated call never reached the Lab");
    }

    /// Important 4c: unknown credentials fill the negative cache only.
    #[tokio::test]
    async fn negative_answers_never_evict_positive_ones() {
        let (lab, c) = setup().await; // capacity 3 each
        lab.set_credential("api_key", "sk_live_good", FakeLab::identity(ACCT, "pro", 1));
        c.introspect(CredentialKind::ApiKey, "sk_live_good", None)
            .await
            .unwrap();
        for i in 0..20 {
            assert!(c
                .introspect(CredentialKind::ApiKey, &format!("sk_live_spray{i}"), None)
                .await
                .unwrap()
                .is_none());
        }
        let calls = lab.calls();
        assert!(c
            .introspect(CredentialKind::ApiKey, "sk_live_good", None)
            .await
            .unwrap()
            .is_some());
        assert_eq!(lab.calls(), calls, "the positive answer is still cached");
        assert!(c.debug_cache_keys().len() <= 6);
    }

    #[tokio::test]
    async fn a_negative_answer_replaces_a_positive_one() {
        let (lab, c) = setup().await;
        lab.set_credential("api_key", "sk_live_a", FakeLab::identity(ACCT, "pro", 1));
        c.introspect(CredentialKind::ApiKey, "sk_live_a", None)
            .await
            .unwrap();
        lab.set_credential("api_key", "sk_live_a", json!({"active": false}));
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(c
            .introspect(CredentialKind::ApiKey, "sk_live_a", None)
            .await
            .unwrap()
            .is_none());
        lab.set_down(true);
        tokio::time::sleep(Duration::from_millis(150)).await; // negative expired
        assert!(
            matches!(
                c.introspect(CredentialKind::ApiKey, "sk_live_a", None)
                    .await,
                Err(LabError::Unavailable(_))
            ),
            "a revoked credential is never served stale"
        );
    }

    #[test]
    fn eviction_drops_unusable_entries_before_the_oldest() {
        let mut cache: Cache<u8> = Cache::new(2, Duration::ZERO);
        cache.put("old".into(), 1, Duration::from_secs(60));
        cache.put("dead".into(), 2, Duration::ZERO); // already past ttl + keep
        cache.put("new".into(), 3, Duration::from_secs(60));
        assert!(
            cache.get("old").is_some(),
            "the oldest usable entry is kept"
        );
        assert!(cache.get("dead").is_none());
        cache.put("newer".into(), 4, Duration::from_secs(60));
        assert!(cache.get("old").is_none(), "then the oldest goes");
        assert_eq!(cache.map.len(), 2);
    }

    /// Minor 7: redirects are not followed; a 3xx is a BadResponse.
    #[tokio::test]
    async fn redirects_are_never_followed() {
        let lab = FakeLab::start().await;
        let target = lab.url.clone();
        let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
            let target = target.clone();
            async move {
                (
                    axum::http::StatusCode::TEMPORARY_REDIRECT,
                    [(axum::http::header::LOCATION, format!("{target}{uri}"))],
                )
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let redirector = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let c = LabClient::with_timing(&redirector, INSTANCE_ID, SECRET, fast());
        assert_eq!(c.instances_me().await, Err(LabError::BadResponse(307)));
        lab.set_credential("api_key", "sk_live_a", FakeLab::identity(ACCT, "pro", 1));
        assert_eq!(
            c.introspect(CredentialKind::ApiKey, "sk_live_a", None)
                .await,
            Err(LabError::BadResponse(307))
        );
        assert_eq!(lab.calls(), 0, "the redirect target was never called");
    }

    // ---- single-flight and refresh-ahead (SCR-90) ----

    fn slow(lab: &FakeLab, ms: usize) {
        lab.state
            .slow_ms
            .store(ms, std::sync::atomic::Ordering::SeqCst);
    }

    fn flights_in_the_air(c: &LabClient) -> usize {
        c.inner.identities.lock().unwrap().flights.len()
    }

    /// `n` concurrent introspections of the same key.
    async fn herd(
        c: &Arc<LabClient>,
        kind: CredentialKind,
        credential: &'static str,
        n: usize,
    ) -> Vec<Result<Option<Identity>, LabError>> {
        let tasks: Vec<_> = (0..n)
            .map(|_| {
                let c = c.clone();
                tokio::spawn(async move { c.introspect(kind, credential, None).await })
            })
            .collect();
        let mut out = Vec::new();
        for t in tasks {
            out.push(t.await.unwrap());
        }
        out
    }

    #[tokio::test]
    async fn concurrent_callers_of_a_cold_or_expired_key_share_one_lab_call() {
        let (lab, c) = setup().await; // ttl 200 ms
        let c = Arc::new(c);
        lab.set_credential("api_key", "sk_live_a", FakeLab::identity(ACCT, "pro", 50));
        slow(&lab, 50);
        for answer in herd(&c, CredentialKind::ApiKey, "sk_live_a", 20).await {
            assert_eq!(answer.unwrap().unwrap().account_id, ACCT);
        }
        assert_eq!(lab.calls(), 1, "a cold key: one call for 20 callers");
        tokio::time::sleep(Duration::from_millis(250)).await; // past ttl
        for answer in herd(&c, CredentialKind::ApiKey, "sk_live_a", 20).await {
            assert!(answer.unwrap().is_some());
        }
        assert_eq!(lab.calls(), 2, "an expired key: one more");
        // Negative answers are shared the same way.
        for answer in herd(&c, CredentialKind::ApiKey, "sk_live_unknown", 20).await {
            assert_eq!(answer, Ok(None));
        }
        assert_eq!(lab.calls(), 3);
        assert_eq!(flights_in_the_air(&c), 0, "no flight is left behind");
    }

    #[tokio::test]
    async fn a_failed_flight_fails_its_waiters_and_the_key_stays_usable() {
        let (lab, c) = setup().await;
        let c = Arc::new(c);
        lab.set_credential("api_key", "sk_live_a", FakeLab::identity(ACCT, "pro", 50));
        lab.set_down(true);
        slow(&lab, 50);
        for answer in herd(&c, CredentialKind::ApiKey, "sk_live_a", 10).await {
            assert!(
                matches!(answer, Err(LabError::Unavailable(_))),
                "unknown credential while the Lab is down: {answer:?}"
            );
        }
        assert_eq!(
            lab.calls(),
            1,
            "the error is shared, not retried per caller"
        );
        assert_eq!(flights_in_the_air(&c), 0);
        lab.set_down(false);
        assert!(c
            .introspect(CredentialKind::ApiKey, "sk_live_a", None)
            .await
            .unwrap()
            .is_some());
        assert_eq!(lab.calls(), 2, "the error was not cached");
    }

    /// A stale-servable entry is still served to every waiter of a failed
    /// flight.
    #[tokio::test]
    async fn waiters_of_a_failed_flight_are_served_stale() {
        let (lab, c) = setup().await; // ttl 200, grace 400 (ms)
        let c = Arc::new(c);
        lab.set_credential("api_key", "sk_live_a", FakeLab::identity(ACCT, "pro", 50));
        c.introspect(CredentialKind::ApiKey, "sk_live_a", None)
            .await
            .unwrap();
        lab.set_down(true);
        slow(&lab, 30);
        tokio::time::sleep(Duration::from_millis(250)).await; // past ttl, inside grace
        for answer in herd(&c, CredentialKind::ApiKey, "sk_live_a", 10).await {
            assert!(answer.unwrap().is_some());
        }
        assert_eq!(lab.calls(), 2);
    }

    /// The caller that started a flight goes away (an HTTP client that
    /// disconnected): the others still get the answer, from the same call.
    #[tokio::test]
    async fn a_dropped_first_caller_does_not_hang_the_others() {
        let (lab, c) = setup().await; // request_timeout 300 ms
        let c = Arc::new(c);
        lab.set_credential("api_key", "sk_live_a", FakeLab::identity(ACCT, "pro", 50));
        slow(&lab, 150);
        let first = {
            let c = c.clone();
            tokio::spawn(async move {
                c.introspect(CredentialKind::ApiKey, "sk_live_a", None)
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(30)).await;
        let second = {
            let c = c.clone();
            tokio::spawn(async move {
                c.introspect(CredentialKind::ApiKey, "sk_live_a", None)
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(30)).await;
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        let answer = tokio::time::timeout(Duration::from_secs(1), second)
            .await
            .expect("the second caller is not left waiting")
            .unwrap();
        assert!(answer.unwrap().is_some());
        assert_eq!(lab.calls(), 1);
        assert_eq!(flights_in_the_air(&c), 0);

        // Every caller gone: the call still completes and its answer is cached.
        lab.set_credential("api_key", "sk_live_b", FakeLab::identity(ACCT, "pro", 50));
        let only = {
            let c = c.clone();
            tokio::spawn(async move {
                c.introspect(CredentialKind::ApiKey, "sk_live_b", None)
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(30)).await;
        only.abort();
        tokio::time::sleep(Duration::from_millis(180)).await; // the flight has landed
        assert_eq!(flights_in_the_air(&c), 0);
        slow(&lab, 0);
        assert!(c
            .introspect(CredentialKind::ApiKey, "sk_live_b", None)
            .await
            .unwrap()
            .is_some());
        assert_eq!(lab.calls(), 2, "served from the abandoned flight's answer");
    }

    fn some_identity() -> Identity {
        Identity {
            account_id: ACCT.into(),
            tier: "pro".into(),
            role: None,
            api_key_id: None,
            limits: None,
        }
    }

    /// A flight that panics neither strands its waiters nor poisons the key:
    /// they start a new one (once), then fail `Unavailable`.
    #[tokio::test]
    async fn a_panicking_flight_is_replaced_once_and_leaves_no_trace() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (_lab, c) = setup().await;
        let inner = c.inner.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let lookup = |panics: usize| {
            let (inner, calls) = (inner.clone(), calls.clone());
            async move {
                inner
                    .cached("introspect", identities, "k".to_string(), || {
                        let calls = calls.clone();
                        async move {
                            tokio::time::sleep(Duration::from_millis(30)).await;
                            if calls.fetch_add(1, Ordering::SeqCst) < panics {
                                panic!("flight down (expected in this test)");
                            }
                            Ok((Some(some_identity()), Duration::from_secs(60)))
                        }
                    })
                    .await
            }
        };
        let herd: Vec<_> = (0..5).map(|_| tokio::spawn(lookup(1))).collect();
        for t in herd {
            let answer = tokio::time::timeout(Duration::from_secs(1), t)
                .await
                .expect("no waiter hangs")
                .unwrap();
            assert_eq!(answer, Ok(Some(some_identity())));
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "the panicked flight and one replacement"
        );
        assert_eq!(flights_in_the_air(&c), 0);

        // A lookup that keeps panicking is an error, not a hang or a loop.
        c.inner.identities.lock().unwrap().positive.remove("k");
        calls.store(0, Ordering::SeqCst);
        assert_eq!(
            lookup(usize::MAX).await,
            Err(LabError::Unavailable("Lab lookup interrupted".into()))
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(flights_in_the_air(&c), 0);
        // And the key works again.
        calls.store(0, Ordering::SeqCst);
        assert_eq!(lookup(0).await, Ok(Some(some_identity())));
    }

    /// ttl 2 s: refresh-ahead from 1.6 s on.
    async fn setup_refresh(stale_grace: Duration) -> (FakeLab, Arc<LabClient>) {
        let lab = FakeLab::start().await;
        let timing = Timing {
            default_ttl: Duration::from_secs(2),
            stale_grace,
            request_timeout: Duration::from_secs(1),
            ..fast()
        };
        let c = Arc::new(LabClient::with_timing(
            &lab.url,
            INSTANCE_ID,
            SECRET,
            timing,
        ));
        lab.set_credential("api_key", "sk_live_a", FakeLab::identity(ACCT, "pro", 50));
        c.introspect(CredentialKind::ApiKey, "sk_live_a", None)
            .await
            .unwrap()
            .unwrap();
        (lab, c)
    }

    #[tokio::test]
    async fn an_entry_close_to_expiry_is_served_and_refreshed_in_the_background() {
        let (lab, c) = setup_refresh(Duration::from_millis(400)).await;
        tokio::time::sleep(Duration::from_millis(1000)).await;
        c.introspect(CredentialKind::ApiKey, "sk_live_a", None)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(lab.calls(), 1, "halfway through its ttl: no refresh");

        tokio::time::sleep(Duration::from_millis(600)).await; // 1.7 s: past 4/5
        lab.set_credential("api_key", "sk_live_a", FakeLab::identity(ACCT, "free", 50));
        slow(&lab, 300);
        let started = Instant::now();
        for answer in herd(&c, CredentialKind::ApiKey, "sk_live_a", 10).await {
            assert_eq!(answer.unwrap().unwrap().tier, "pro", "the cached answer");
        }
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "nobody waited for the Lab"
        );
        tokio::time::sleep(Duration::from_millis(500)).await; // 2.2 s: the refresh landed
        assert_eq!(lab.calls(), 2, "exactly one background call");
        assert_eq!(flights_in_the_air(&c), 0);
        // Past the first answer's ttl, the refreshed one is served, without
        // a Lab call on the request path.
        let started = Instant::now();
        let id = c
            .introspect(CredentialKind::ApiKey, "sk_live_a", None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(id.tier, "free");
        assert!(started.elapsed() < Duration::from_millis(200));
        assert_eq!(lab.calls(), 2);
    }

    /// The Lab is asked on access only: an entry nobody reads is never
    /// refreshed.
    #[tokio::test]
    async fn an_unused_entry_is_never_refreshed() {
        let (lab, _c) = setup_refresh(Duration::from_millis(400)).await;
        tokio::time::sleep(Duration::from_millis(2200)).await;
        assert_eq!(lab.calls(), 1);
    }

    /// A refresh that fails changes nothing: the entry is served until the
    /// end of the ttl the Lab gave for it and not a moment longer, and the
    /// failed refresh is not repeated.
    #[tokio::test]
    async fn a_failed_refresh_does_not_extend_the_entry() {
        let (lab, c) = setup_refresh(Duration::ZERO).await; // no stale window
        tokio::time::sleep(Duration::from_millis(1700)).await;
        lab.set_down(true);
        for _ in 0..3 {
            assert!(c
                .introspect(CredentialKind::ApiKey, "sk_live_a", None)
                .await
                .unwrap()
                .is_some());
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(lab.calls(), 2, "one refresh per entry, even when it fails");
        tokio::time::sleep(Duration::from_millis(250)).await; // 2.1 s: past the ttl
        assert!(
            matches!(
                c.introspect(CredentialKind::ApiKey, "sk_live_a", None)
                    .await,
                Err(LabError::Unavailable(_))
            ),
            "the failed refresh did not prolong the entry"
        );
    }

    /// With a stale window, the failed refresh leaves the stale rule as it
    /// was: the window still ends `stale_grace` after the original expiry.
    #[tokio::test]
    async fn a_failed_refresh_leaves_the_stale_window_unchanged() {
        let (lab, c) = setup_refresh(Duration::from_millis(400)).await;
        tokio::time::sleep(Duration::from_millis(1700)).await;
        lab.set_down(true);
        c.introspect(CredentialKind::ApiKey, "sk_live_a", None)
            .await
            .unwrap()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await; // 2.1 s: stale
        assert!(c
            .introspect(CredentialKind::ApiKey, "sk_live_a", None)
            .await
            .unwrap()
            .is_some());
        tokio::time::sleep(Duration::from_millis(400)).await; // 2.5 s: past ttl + grace
        assert!(matches!(
            c.introspect(CredentialKind::ApiKey, "sk_live_a", None)
                .await,
            Err(LabError::Unavailable(_))
        ));
    }

    /// A refresh that learns the credential was revoked stops serving it at
    /// once, before the end of its ttl.
    #[tokio::test]
    async fn a_refresh_that_finds_the_credential_revoked_drops_it_early() {
        let (lab, c) = setup_refresh(Duration::from_millis(400)).await;
        tokio::time::sleep(Duration::from_millis(1700)).await;
        lab.set_credential("api_key", "sk_live_a", json!({"active": false}));
        assert!(c
            .introspect(CredentialKind::ApiKey, "sk_live_a", None)
            .await
            .unwrap()
            .is_some());
        tokio::time::sleep(Duration::from_millis(50)).await; // 1.75 s: refreshed
        assert_eq!(
            c.introspect(CredentialKind::ApiKey, "sk_live_a", None)
                .await,
            Ok(None),
            "still inside the first answer's ttl"
        );
        assert_eq!(lab.calls(), 2);
    }

    /// Background refreshes are capped (a quarter of `max_in_flight`): past
    /// the cap an entry is simply not refreshed ahead.
    #[tokio::test]
    async fn background_refreshes_are_capped() {
        let lab = FakeLab::start().await;
        let timing = Timing {
            default_ttl: Duration::from_secs(2),
            request_timeout: Duration::from_secs(1),
            capacity: 10,
            max_in_flight: 8, // 2 refresh slots
            ..fast()
        };
        let c = LabClient::with_timing(&lab.url, INSTANCE_ID, SECRET, timing);
        let keys: Vec<String> = (0..5).map(|i| format!("sk_live_{i}")).collect();
        for k in &keys {
            lab.set_credential("api_key", k, FakeLab::identity(ACCT, "pro", 50));
            c.introspect(CredentialKind::ApiKey, k, None).await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(1700)).await;
        slow(&lab, 200);
        for k in &keys {
            assert!(c
                .introspect(CredentialKind::ApiKey, k, None)
                .await
                .unwrap()
                .is_some());
        }
        assert_eq!(flights_in_the_air(&c), 2);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(lab.calls(), 5 + 2, "two refreshes in the air, no more");
    }

    /// `cache_ttl: 0` ("do not cache": an expiring session or token) stays
    /// strict under concurrency: a caller never gets an answer the Lab may
    /// have computed before it asked.
    #[tokio::test]
    async fn a_cache_ttl_0_answer_is_fetched_by_every_caller() {
        let (lab, c) = setup().await;
        let c = Arc::new(c);
        let mut v = FakeLab::identity(ACCT, "pro", 50);
        v["cache_ttl"] = json!(0);
        lab.set_credential("session", "jwt_expiring", v);
        slow(&lab, 50);
        // Cold: the callers wait for one flight (its ttl is not known yet),
        // then all but the one that started it ask again for themselves.
        for answer in herd(&c, CredentialKind::Session, "jwt_expiring", 5).await {
            assert!(answer.unwrap().is_some());
        }
        assert_eq!(lab.calls(), 1 + 4);
        // Known to be uncacheable: every caller asks, concurrent or not.
        for answer in herd(&c, CredentialKind::Session, "jwt_expiring", 5).await {
            assert!(answer.unwrap().is_some());
        }
        assert_eq!(lab.calls(), 5 + 5);
        c.introspect(CredentialKind::Session, "jwt_expiring", None)
            .await
            .unwrap();
        assert_eq!(lab.calls(), 11, "a later caller fetches again");
        assert_eq!(flights_in_the_air(&c), 0);
    }

    /// A flight asked before the AI key was re-resolved lands after it: the
    /// replacement key stays the cached answer.
    #[test]
    fn a_flight_never_overwrites_a_newer_answer() {
        let mut cache: Lookups<u8> = Lookups::new(&fast());
        let asked = Instant::now();
        std::thread::sleep(Duration::from_millis(5));
        cache.positive.put("k".into(), 2, Duration::from_secs(60));
        cache.store("k", asked, &Ok((Some(1), Duration::from_secs(60))));
        assert_eq!(cache.positive.get("k").unwrap().value, 2);
        cache.store("k", Instant::now(), &Ok((Some(3), Duration::from_secs(60))));
        assert_eq!(cache.positive.get("k").unwrap().value, 3);
        cache.store("k", asked, &Ok((None, Duration::from_secs(60))));
        assert!(cache.positive.get("k").is_none(), "a negative answer wins");
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
