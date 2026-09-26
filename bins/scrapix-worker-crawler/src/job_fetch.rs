//! Per-job request shaping: user-agent rotation, extra headers, proxy
//! selection and the robots.txt opt-out, all derived from the `JobSpec` that
//! travels on every `UrlMessage`.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use scrapix_core::{JobSpec, ProxyRotation};
use scrapix_crawler::{FetchOptions, ProxyPool, RotationStrategy};

/// Maximum number of jobs whose proxy pools are cached. Past it the least
/// recently *inserted* job is evicted (its pool is rebuilt on next use,
/// losing only failure/rotation state).
pub const MAX_CACHED_JOBS: usize = 256;

/// Consecutive failures after which a proxy is put in cooldown (and a tiered
/// config falls back to the next tier).
pub const PROXY_MAX_FAILURES: u32 = 3;

/// How long a failed proxy stays out of rotation.
const PROXY_FAILURE_COOLDOWN: Duration = Duration::from_secs(300);

/// A job's proxies, in fallback order: `proxy.urls` first (when non-empty),
/// then each `proxy.tiered` tier. A request uses the first tier that still
/// has an available proxy.
struct JobProxies {
    tiers: Vec<ProxyPool>,
}

impl JobProxies {
    fn from_config(config: &scrapix_core::ProxyConfig) -> Self {
        let rotation = match config.rotation {
            ProxyRotation::RoundRobin => RotationStrategy::RoundRobin,
            ProxyRotation::Random => RotationStrategy::Random,
            ProxyRotation::LeastUsed => RotationStrategy::LeastRecentlyUsed,
        };
        let tiers = std::iter::once(config.urls.clone())
            .chain(config.tiered.clone().unwrap_or_default())
            .filter(|urls| !urls.is_empty())
            .map(|proxies| {
                ProxyPool::new(scrapix_crawler::ProxyConfig {
                    proxies,
                    rotation,
                    failure_cooldown: PROXY_FAILURE_COOLDOWN,
                    max_failures: PROXY_MAX_FAILURES,
                })
            })
            .collect();
        Self { tiers }
    }

    fn pick(&self) -> Option<String> {
        self.tiers.iter().find_map(|pool| pool.get_proxy())
    }

    fn report(&self, proxy_url: &str, ok: bool) {
        for pool in &self.tiers {
            if ok {
                pool.report_success(proxy_url);
            } else {
                pool.report_failure(proxy_url);
            }
        }
    }
}

#[derive(Default)]
struct ProxyCache {
    by_job: HashMap<String, Arc<JobProxies>>,
    insertion_order: VecDeque<String>,
}

/// Shapes each request from the job's `JobSpec`.
#[derive(Default)]
pub struct JobFetchShaper {
    rr: AtomicUsize,
    proxies: Mutex<ProxyCache>,
}

impl JobFetchShaper {
    /// Picks the UA (round-robin across job.user_agents), extra headers and proxy
    /// for one request. Proxy selection uses scrapix_crawler::ProxyPool built from
    /// job.proxy, cached per (job_id) in a small LRU (max 256 jobs).
    ///
    /// With no `JobSpec` (or a default one) `base` is returned unchanged, so
    /// jobs that set none of these fields behave exactly as before.
    pub fn options_for(
        &self,
        job_id: &str,
        job: Option<&JobSpec>,
        mut base: FetchOptions,
    ) -> FetchOptions {
        let Some(job) = job else {
            return base;
        };

        if !job.user_agents.is_empty() {
            let i = self.rr.fetch_add(1, Ordering::Relaxed) % job.user_agents.len();
            base.user_agent = Some(job.user_agents[i].clone());
        }

        if !job.headers.is_empty() {
            let mut headers: Vec<(String, String)> = job
                .headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            headers.sort();
            base.extra_headers.extend(headers);
        }

        if let Some(ref proxy) = job.proxy {
            base.proxy = self.proxies_for(job_id, proxy).pick();
        }

        if !job.respect_robots_txt {
            base.respect_robots = Some(false);
        }

        base
    }

    /// Record whether a request through `proxy_url` for `job_id` succeeded,
    /// so failing proxies go into cooldown and tiers fall back.
    pub fn report_proxy(&self, job_id: &str, proxy_url: &str, ok: bool) {
        let entry = self.proxies.lock().by_job.get(job_id).cloned();
        if let Some(proxies) = entry {
            proxies.report(proxy_url, ok);
        }
    }

    /// Number of jobs with a cached proxy pool.
    pub fn cached_jobs(&self) -> usize {
        self.proxies.lock().by_job.len()
    }

    fn proxies_for(&self, job_id: &str, config: &scrapix_core::ProxyConfig) -> Arc<JobProxies> {
        let mut cache = self.proxies.lock();
        if let Some(p) = cache.by_job.get(job_id) {
            return p.clone();
        }
        while cache.by_job.len() >= MAX_CACHED_JOBS {
            match cache.insertion_order.pop_front() {
                Some(oldest) => {
                    cache.by_job.remove(&oldest);
                }
                None => break,
            }
        }
        let proxies = Arc::new(JobProxies::from_config(config));
        cache.by_job.insert(job_id.to_string(), proxies.clone());
        cache.insertion_order.push_back(job_id.to_string());
        proxies
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_user_agents_and_applies_headers() {
        let shaper = JobFetchShaper::default();
        let job = JobSpec {
            user_agents: vec!["A".into(), "B".into()],
            headers: [("X-Test".to_string(), "1".to_string())].into(),
            ..Default::default()
        };
        let a = shaper.options_for("j", Some(&job), FetchOptions::default());
        let b = shaper.options_for("j", Some(&job), FetchOptions::default());
        assert_ne!(a.user_agent, b.user_agent);
        assert!(a
            .extra_headers
            .contains(&("X-Test".to_string(), "1".to_string())));
    }

    #[test]
    fn no_job_spec_means_worker_defaults() {
        let o = JobFetchShaper::default().options_for("j", None, FetchOptions::default());
        assert!(o.user_agent.is_none() && o.extra_headers.is_empty() && o.proxy.is_none());
    }

    // Additional coverage beyond the brief.

    fn proxy_job(urls: &[&str], tiered: Option<Vec<Vec<&str>>>) -> JobSpec {
        JobSpec {
            proxy: Some(scrapix_core::ProxyConfig {
                urls: urls.iter().map(|s| s.to_string()).collect(),
                rotation: scrapix_core::ProxyRotation::RoundRobin,
                tiered: tiered.map(|t| {
                    t.into_iter()
                        .map(|tier| tier.into_iter().map(String::from).collect())
                        .collect()
                }),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn default_job_spec_keeps_worker_defaults_and_base_options() {
        let base = FetchOptions::with_pdf(Some(10));
        let o = JobFetchShaper::default().options_for("j", Some(&JobSpec::default()), base);
        assert!(o.user_agent.is_none() && o.extra_headers.is_empty() && o.proxy.is_none());
        assert_eq!(o.respect_robots, None);
        assert!(o.allow_pdf);
        assert_eq!(o.pdf_max_size_bytes, Some(10));
    }

    #[test]
    fn robots_opt_out_is_forwarded() {
        let job = JobSpec {
            respect_robots_txt: false,
            ..Default::default()
        };
        let o = JobFetchShaper::default().options_for("j", Some(&job), FetchOptions::default());
        assert_eq!(o.respect_robots, Some(false));
    }

    #[test]
    fn proxies_rotate_round_robin() {
        let shaper = JobFetchShaper::default();
        let job = proxy_job(&["http://p1:8080", "http://p2:8080"], None);
        let picks: Vec<_> = (0..4)
            .map(|_| {
                shaper
                    .options_for("j", Some(&job), FetchOptions::default())
                    .proxy
                    .unwrap()
            })
            .collect();
        assert_ne!(picks[0], picks[1]);
        assert_eq!(picks[0], picks[2]);
        assert_eq!(picks[1], picks[3]);
    }

    #[test]
    fn tiered_proxies_fall_back_when_first_tier_fails() {
        let shaper = JobFetchShaper::default();
        let job = proxy_job(&[], Some(vec![vec!["http://t1:1"], vec!["http://t2:1"]]));
        let first = shaper
            .options_for("j", Some(&job), FetchOptions::default())
            .proxy
            .unwrap();
        assert_eq!(first, "http://t1:1");
        for _ in 0..PROXY_MAX_FAILURES {
            shaper.report_proxy("j", &first, false);
        }
        let next = shaper
            .options_for("j", Some(&job), FetchOptions::default())
            .proxy
            .unwrap();
        assert_eq!(next, "http://t2:1");
    }

    #[test]
    fn proxy_pool_cache_is_bounded() {
        let shaper = JobFetchShaper::default();
        let job = proxy_job(&["http://p1:8080"], None);
        for i in 0..(MAX_CACHED_JOBS + 10) {
            shaper.options_for(&format!("job-{i}"), Some(&job), FetchOptions::default());
        }
        assert_eq!(shaper.cached_jobs(), MAX_CACHED_JOBS);
    }
}
