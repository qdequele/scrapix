//! What produced a job: a distributed crawl, a batch scrape (SCR-74) or an
//! extraction (SCR-73).
//!
//! All three share the job lifecycle (`JobState` in memory + the `jobs`
//! table, `GET /job/{id}/status`, `/jobs`, SSE/WebSocket events, webhooks,
//! `DELETE /job/{id}`), but only crawl jobs run through the Kafka pipeline
//! and its work accounting. The kind is stored in the job's `config` blob
//! under [`JOB_TYPE_KEY`] (absent = crawl), so it survives a restart without
//! a schema change and is visible to the console as part of `config`.

use scrapix_core::JobState;
use serde::Serialize;

/// Key of the job kind inside `JobState::config`.
pub(crate) const JOB_TYPE_KEY: &str = "job_type";

/// The kind of a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum JobKind {
    /// Distributed crawl (`POST /crawl`): pipeline-driven, results live in
    /// the job's Meilisearch index.
    Crawl,
    /// `POST /batch/scrape`: the API scrapes each URL itself, results are
    /// stored by the engine (`job_results`).
    BatchScrape,
    /// `POST /extract`: pages are scraped by the API and one structured
    /// extraction is produced over all of them.
    Extract,
}

impl JobKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            JobKind::Crawl => "crawl",
            JobKind::BatchScrape => "batch_scrape",
            JobKind::Extract => "extract",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "crawl" => Some(JobKind::Crawl),
            "batch_scrape" => Some(JobKind::BatchScrape),
            "extract" => Some(JobKind::Extract),
            _ => None,
        }
    }

    /// The kind of `job` (crawl unless its config says otherwise).
    pub(crate) fn of(job: &JobState) -> Self {
        job.config
            .as_ref()
            .and_then(|c| c.get(JOB_TYPE_KEY))
            .and_then(|v| v.as_str())
            .and_then(Self::parse)
            .unwrap_or(JobKind::Crawl)
    }

    /// Whether jobs of this kind are driven by the Kafka pipeline (frontier,
    /// workers, work accounting, completion loop, per-page crawl billing).
    pub(crate) fn is_pipeline(self) -> bool {
        self == JobKind::Crawl
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_defaults_to_crawl() {
        let mut job = JobState::new("j", "idx");
        assert_eq!(JobKind::of(&job), JobKind::Crawl);
        job.config = Some(serde_json::json!({ "start_urls": ["https://a.test"] }));
        assert_eq!(JobKind::of(&job), JobKind::Crawl);
        job.config = Some(serde_json::json!({ "job_type": "nonsense" }));
        assert_eq!(JobKind::of(&job), JobKind::Crawl);
    }

    #[test]
    fn kind_round_trips_through_config() {
        for kind in [JobKind::Crawl, JobKind::BatchScrape, JobKind::Extract] {
            let mut job = JobState::new("j", "");
            let name = serde_json::to_value(kind).unwrap();
            job.config = Some(serde_json::json!({ JOB_TYPE_KEY: name }));
            assert_eq!(JobKind::of(&job), kind);
            assert_eq!(name, kind.as_str());
        }
        assert!(JobKind::Crawl.is_pipeline());
        assert!(!JobKind::BatchScrape.is_pipeline());
        assert!(!JobKind::Extract.is_pipeline());
    }
}
