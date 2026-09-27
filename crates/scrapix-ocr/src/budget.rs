//! Per-account daily OCR page budget.
//!
//! OCR pages are reserved *before* recognition: a reservation grants at
//! most what is left of the account's budget for the current UTC day, and
//! pages that end up not being recognized (backend error) are released.

use std::collections::HashMap;

use async_trait::async_trait;
use parking_lot::Mutex;
use tracing::warn;

/// Budget bucket for work not attributed to an account (self-hosted, no
/// auth): it shares one daily budget.
pub const ANONYMOUS_ACCOUNT: &str = "_anonymous";

/// Tracks OCR pages spent per account per UTC day.
#[async_trait]
pub trait OcrBudget: Send + Sync {
    /// Reserve up to `pages` pages for `account` today; returns how many
    /// were granted (`pages` when the budget is unlimited).
    async fn reserve(&self, account: &str, pages: u32) -> u32;

    /// Give back reserved pages that were not used.
    async fn release(&self, account: &str, pages: u32);
}

fn today() -> String {
    chrono::Utc::now().format("%Y%m%d").to_string()
}

/// In-process budget (per instance; use [`RedisOcrBudget`] when several
/// instances share accounts). `daily_limit == 0` means unlimited.
pub struct MemoryOcrBudget {
    daily_limit: u64,
    used: Mutex<HashMap<(String, String), u64>>,
}

impl MemoryOcrBudget {
    pub fn new(daily_limit: u64) -> Self {
        Self {
            daily_limit,
            used: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl OcrBudget for MemoryOcrBudget {
    async fn reserve(&self, account: &str, pages: u32) -> u32 {
        if self.daily_limit == 0 {
            return pages;
        }
        let day = today();
        let mut used = self.used.lock();
        // Forget previous days.
        used.retain(|(_, d), _| *d == day);
        let spent = used.entry((account.to_string(), day)).or_insert(0);
        let granted = self.daily_limit.saturating_sub(*spent).min(pages as u64);
        *spent += granted;
        granted as u32
    }

    async fn release(&self, account: &str, pages: u32) {
        if self.daily_limit == 0 || pages == 0 {
            return;
        }
        let mut used = self.used.lock();
        if let Some(spent) = used.get_mut(&(account.to_string(), today())) {
            *spent = spent.saturating_sub(pages as u64);
        }
    }
}

/// Redis-backed budget shared by every API and content-worker instance
/// (`INCRBY` on a per-account, per-day key that expires after two days).
pub struct RedisOcrBudget {
    conn: redis::aio::ConnectionManager,
    prefix: String,
    daily_limit: u64,
}

impl RedisOcrBudget {
    pub async fn connect(
        url: &str,
        prefix: &str,
        daily_limit: u64,
    ) -> Result<Self, redis::RedisError> {
        let client = redis::Client::open(url)?;
        let conn = redis::aio::ConnectionManager::new(client).await?;
        Ok(Self {
            conn,
            prefix: prefix.to_string(),
            daily_limit,
        })
    }

    fn key(&self, account: &str) -> String {
        format!("{}:budget:{}:{}", self.prefix, account, today())
    }
}

#[async_trait]
impl OcrBudget for RedisOcrBudget {
    async fn reserve(&self, account: &str, pages: u32) -> u32 {
        if self.daily_limit == 0 || pages == 0 {
            return pages;
        }
        let key = self.key(account);
        let mut conn = self.conn.clone();
        let total: i64 = match redis::pipe()
            .atomic()
            .cmd("INCRBY")
            .arg(&key)
            .arg(pages)
            .cmd("EXPIRE")
            .arg(&key)
            .arg(2 * 24 * 3600)
            .ignore()
            .query_async::<(i64,)>(&mut conn)
            .await
        {
            Ok((total,)) => total,
            Err(e) => {
                // Fail closed: without the shared counter we cannot tell
                // whether the account still has budget.
                warn!(error = %e, account, "OCR budget unavailable; granting no pages");
                return 0;
            }
        };
        let limit = self.daily_limit as i64;
        let overflow = (total - limit).clamp(0, pages as i64);
        if overflow > 0 {
            let _ = redis::cmd("DECRBY")
                .arg(&key)
                .arg(overflow)
                .query_async::<i64>(&mut conn)
                .await;
        }
        (pages as i64 - overflow) as u32
    }

    async fn release(&self, account: &str, pages: u32) {
        if self.daily_limit == 0 || pages == 0 {
            return;
        }
        let mut conn = self.conn.clone();
        let _ = redis::cmd("DECRBY")
            .arg(self.key(account))
            .arg(pages)
            .query_async::<i64>(&mut conn)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn memory_budget_caps_per_account() {
        let budget = MemoryOcrBudget::new(5);
        assert_eq!(budget.reserve("a", 3).await, 3);
        assert_eq!(budget.reserve("a", 3).await, 2, "only 2 left today");
        assert_eq!(budget.reserve("a", 1).await, 0);
        assert_eq!(budget.reserve("b", 4).await, 4, "accounts are independent");
        budget.release("a", 2).await;
        assert_eq!(budget.reserve("a", 5).await, 2);
    }

    #[tokio::test]
    async fn zero_limit_is_unlimited() {
        let budget = MemoryOcrBudget::new(0);
        assert_eq!(budget.reserve("a", 1_000).await, 1_000);
    }
}
