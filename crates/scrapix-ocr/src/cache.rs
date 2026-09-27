//! OCR output cache keyed by page-image hash, so re-crawling (or
//! re-uploading) the same document never recognizes — or bills — a page
//! twice.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use tracing::debug;

/// Cache key for one page image under one backend: a different backend or
/// model never reuses another's output.
pub fn cache_key(backend: &str, image: &[u8]) -> String {
    let digest = Sha256::digest(image);
    format!("{}:{}", backend, hex::encode(digest))
}

/// Stores recognized page text.
#[async_trait]
pub trait OcrCache: Send + Sync {
    async fn get(&self, key: &str) -> Option<String>;
    async fn put(&self, key: &str, text: &str);
}

/// In-process LRU-ish cache (bounded by entry count; oldest insert evicted).
pub struct MemoryOcrCache {
    capacity: usize,
    inner: Mutex<(HashMap<String, String>, VecDeque<String>)>,
}

impl MemoryOcrCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            inner: Mutex::new((HashMap::new(), VecDeque::new())),
        }
    }
}

#[async_trait]
impl OcrCache for MemoryOcrCache {
    async fn get(&self, key: &str) -> Option<String> {
        self.inner.lock().0.get(key).cloned()
    }

    async fn put(&self, key: &str, text: &str) {
        let mut guard = self.inner.lock();
        let (map, order) = &mut *guard;
        if map.insert(key.to_string(), text.to_string()).is_none() {
            order.push_back(key.to_string());
            while order.len() > self.capacity {
                if let Some(old) = order.pop_front() {
                    map.remove(&old);
                }
            }
        }
    }
}

/// Redis-backed cache shared by every API and content-worker instance, so a
/// page recognized by one is never recognized again by another.
pub struct RedisOcrCache {
    conn: redis::aio::ConnectionManager,
    prefix: String,
    ttl: Duration,
}

impl RedisOcrCache {
    pub async fn connect(
        url: &str,
        prefix: &str,
        ttl: Duration,
    ) -> Result<Self, redis::RedisError> {
        let client = redis::Client::open(url)?;
        let conn = redis::aio::ConnectionManager::new(client).await?;
        Ok(Self {
            conn,
            prefix: prefix.to_string(),
            ttl,
        })
    }

    fn key(&self, key: &str) -> String {
        format!("{}:cache:{}", self.prefix, key)
    }
}

#[async_trait]
impl OcrCache for RedisOcrCache {
    async fn get(&self, key: &str) -> Option<String> {
        let mut conn = self.conn.clone();
        match redis::cmd("GET")
            .arg(self.key(key))
            .query_async::<Option<String>>(&mut conn)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                debug!(error = %e, "OCR cache read failed");
                None
            }
        }
    }

    async fn put(&self, key: &str, text: &str) {
        let mut conn = self.conn.clone();
        if let Err(e) = redis::cmd("SET")
            .arg(self.key(key))
            .arg(text)
            .arg("EX")
            .arg(self.ttl.as_secs().max(1))
            .query_async::<()>(&mut conn)
            .await
        {
            debug!(error = %e, "OCR cache write failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn memory_cache_round_trips_and_evicts() {
        let cache = MemoryOcrCache::new(2);
        cache.put("a", "1").await;
        cache.put("b", "2").await;
        assert_eq!(cache.get("a").await.as_deref(), Some("1"));
        cache.put("c", "3").await;
        assert_eq!(cache.get("a").await, None, "oldest evicted");
        assert_eq!(cache.get("c").await.as_deref(), Some("3"));
    }

    #[test]
    fn keys_depend_on_backend_and_image() {
        assert_ne!(
            cache_key("tesseract:eng", b"x"),
            cache_key("vision:a/b", b"x")
        );
        assert_ne!(cache_key("t", b"x"), cache_key("t", b"y"));
        assert_eq!(cache_key("t", b"x"), cache_key("t", b"x"));
    }
}
