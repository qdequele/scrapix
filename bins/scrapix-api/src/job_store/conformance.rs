//! One behavioural suite, run against every `JobStore` backend.

use std::sync::Arc;

use scrapix_core::{JobState, JobStatus};
use serde_json::json;

use super::JobStore;

pub(crate) fn job(id: &str) -> JobState {
    let mut j = JobState::new(id, "idx");
    j.start_urls = vec!["https://example.com".into()];
    j.max_pages = Some(10);
    j.config = Some(json!({"start_urls": ["https://example.com"]}));
    j
}

pub(super) async fn insert_get_roundtrip(s: Arc<dyn JobStore>) {
    let j = job("rt-1");
    s.insert_job(&j).await.unwrap();
    let got = s.get_job("rt-1", None).await.expect("stored");
    assert_eq!(got.job_id, "rt-1");
    assert_eq!(got.index_uid, "idx");
    assert_eq!(got.start_urls, vec!["https://example.com".to_string()]);
    assert_eq!(got.max_pages, Some(10));
    assert_eq!(got.config, j.config);
    // insert is idempotent
    s.insert_job(&j).await.unwrap();
}

pub(super) async fn counters_and_terminal_update(s: Arc<dyn JobStore>) {
    let mut j = job("ct-1");
    s.insert_job(&j).await.unwrap();
    j.start();
    j.pages_crawled = 5;
    s.flush_job_counters(std::slice::from_ref(&j))
        .await
        .unwrap();
    assert_eq!(s.get_job("ct-1", None).await.unwrap().pages_crawled, 5);
    j.pages_crawled = 7;
    j.status = JobStatus::Completed;
    j.completed_at = Some(chrono::Utc::now());
    s.update_job_full(&j).await.unwrap();
    let got = s.get_job("ct-1", None).await.unwrap();
    assert!(matches!(got.status, JobStatus::Completed));
    assert_eq!(got.pages_crawled, 7);
    assert!(got.completed_at.is_some());
}

pub(super) async fn stale_counters_never_regress_terminal(s: Arc<dyn JobStore>) {
    let mut j = job("st-1");
    s.insert_job(&j).await.unwrap();
    j.start();
    let stale = {
        let mut x = j.clone();
        x.pages_crawled = 3;
        x
    };
    j.pages_crawled = 9;
    j.status = JobStatus::Completed;
    s.update_job_full(&j).await.unwrap();
    s.flush_job_counters(&[stale]).await.unwrap(); // arrives late
    let got = s.get_job("st-1", None).await.unwrap();
    assert!(matches!(got.status, JobStatus::Completed));
    assert_eq!(got.pages_crawled, 9);
}

/// A terminal snapshot in a counter flush never writes the terminal status
/// (only `update_job_full` does, after the job's Lab events are recorded).
pub(super) async fn terminal_snapshot_counters_leave_the_row_active(s: Arc<dyn JobStore>) {
    let mut j = job("tc-1");
    j.start();
    s.insert_job(&j).await.unwrap();
    j.status = JobStatus::Cancelled;
    j.pages_crawled = 4;
    s.flush_job_counters(std::slice::from_ref(&j))
        .await
        .unwrap();
    let got = s.get_job("tc-1", None).await.unwrap();
    assert!(matches!(got.status, JobStatus::Running));
}

pub(super) async fn accounting_and_active_recovery(s: Arc<dyn JobStore>) {
    let mut running = job("ac-run");
    running.start();
    s.insert_job(&running).await.unwrap();
    let mut done = job("ac-done");
    done.status = JobStatus::Completed;
    s.insert_job(&done).await.unwrap();
    s.flush_job_accounting(&[("ac-run".into(), json!({"dispatched": 3}))])
        .await
        .unwrap();
    let active: Vec<String> = s
        .load_active_jobs()
        .await
        .into_iter()
        .map(|j| j.job_id)
        .collect();
    assert!(active.contains(&"ac-run".to_string()));
    assert!(!active.contains(&"ac-done".to_string()));
    let acc = s.load_active_job_accounting().await;
    assert!(acc
        .iter()
        .any(|(id, v)| id == "ac-run" && v == &json!({"dispatched": 3})));
}

pub(super) async fn list_newest_first_and_account_filter(s: Arc<dyn JobStore>) {
    // Same-second inserts must still order newest first (Review Focus 5).
    for i in 0..3 {
        s.insert_job(&job(&format!("ls-{i}"))).await.unwrap();
    }
    let ids: Vec<String> = s
        .list_jobs(None, None, 100, 0)
        .await
        .into_iter()
        .map(|j| j.job_id)
        .filter(|id| id.starts_with("ls-"))
        .collect();
    assert_eq!(ids, vec!["ls-2", "ls-1", "ls-0"]);
    let page: Vec<String> = s
        .list_jobs(None, None, 1, 0)
        .await
        .into_iter()
        .map(|j| j.job_id)
        .collect();
    assert_eq!(page.len(), 1);

    let account = "7f1c2a8e-0000-4000-8000-000000000001";
    let mut owned = job("ls-owned");
    owned.account_id = Some(account.into());
    s.insert_job(&owned).await.unwrap();
    let mine: Vec<String> = s
        .list_jobs(Some(account), None, 100, 0)
        .await
        .into_iter()
        .map(|j| j.job_id)
        .collect();
    assert_eq!(mine, vec!["ls-owned"]);
    assert!(s.get_job("ls-0", Some(account)).await.is_none());
    assert!(s.get_job("ls-owned", Some(account)).await.is_some());
    assert_eq!(s.active_job_ids(account).await.unwrap(), vec!["ls-owned"]);
}

pub(super) async fn list_filters_by_status(s: Arc<dyn JobStore>) {
    let mut done = job("lf-done");
    done.status = JobStatus::Completed;
    s.insert_job(&done).await.unwrap();
    let mut running = job("lf-run");
    running.start();
    s.insert_job(&running).await.unwrap();
    let ids = |jobs: Vec<JobState>| jobs.into_iter().map(|j| j.job_id).collect::<Vec<_>>();
    assert_eq!(
        ids(s.list_jobs(None, Some("completed"), 100, 0).await),
        vec!["lf-done"]
    );
    assert_eq!(
        ids(s.list_jobs(None, Some("running"), 100, 0).await),
        vec!["lf-run"]
    );
    assert!(s.list_jobs(None, Some("paused"), 100, 0).await.is_empty());
}

pub(super) async fn delete_only_finished_owned_jobs(s: Arc<dyn JobStore>) {
    let account = "7f1c2a8e-0000-4000-8000-000000000001";
    let mut done = job("dl-done");
    done.status = JobStatus::Completed;
    done.account_id = Some(account.into());
    s.insert_job(&done).await.unwrap();
    s.store_result_page("dl-done", 1, "https://e.com/1", true, &json!({"n": 1}))
        .await
        .unwrap();
    let mut running = job("dl-run");
    running.start();
    s.insert_job(&running).await.unwrap();

    // Not finished, another account's, unknown: nothing deleted.
    assert!(!s.delete_job("dl-run", None).await.unwrap());
    assert!(s.get_job("dl-run", None).await.is_some());
    let other = "7f1c2a8e-0000-4000-8000-0000000000ff";
    assert!(!s.delete_job("dl-done", Some(other)).await.unwrap());
    assert!(!s.delete_job("nope", None).await.unwrap());

    assert!(s.delete_job("dl-done", Some(account)).await.unwrap());
    assert!(s.get_job("dl-done", None).await.is_none());
    assert_eq!(s.result_pages("dl-done", 0, 10).await.unwrap(), (vec![], 0));
    assert!(
        !s.delete_job("dl-done", None).await.unwrap(),
        "already gone"
    );
}

pub(super) async fn job_results_pages_and_summary(s: Arc<dyn JobStore>) {
    s.insert_job(&job("jr-1")).await.unwrap();
    for seq in 1..=3u64 {
        s.store_result_page(
            "jr-1",
            seq,
            &format!("https://e.com/{seq}"),
            true,
            &json!({"n": seq}),
        )
        .await
        .unwrap();
    }
    // duplicate seq is ignored
    s.store_result_page("jr-1", 2, "https://e.com/2", true, &json!({"n": 99}))
        .await
        .unwrap();
    let (rows, total) = s.result_pages("jr-1", 1, 10).await.unwrap();
    assert_eq!(total, 3);
    assert_eq!(rows, vec![(2, json!({"n": 2})), (3, json!({"n": 3}))]);
    assert_eq!(s.load_result_summary("jr-1").await.unwrap(), None);
    s.store_result_summary("jr-1", &json!({"v": 1}))
        .await
        .unwrap();
    s.store_result_summary("jr-1", &json!({"v": 2}))
        .await
        .unwrap();
    assert_eq!(
        s.load_result_summary("jr-1").await.unwrap(),
        Some(json!({"v": 2}))
    );
}

/// Expands to one `#[tokio::test]` per check for a backend.
/// `$make` is an async expression returning `Option<Arc<dyn JobStore>>`
/// (`None` = backend not available → test prints "skipped" and passes).
macro_rules! conformance_tests {
    ($backend:ident, $make:expr) => {
        mod $backend {
            macro_rules! case {
                ($check:ident) => {
                    #[tokio::test]
                    async fn $check() {
                        let Some(store) = $make.await else {
                            eprintln!("skipped: {} backend unavailable", stringify!($backend));
                            return;
                        };
                        super::$check(store).await;
                    }
                };
            }
            case!(insert_get_roundtrip);
            case!(counters_and_terminal_update);
            case!(stale_counters_never_regress_terminal);
            case!(terminal_snapshot_counters_leave_the_row_active);
            case!(accounting_and_active_recovery);
            case!(list_newest_first_and_account_filter);
            case!(job_results_pages_and_summary);
            case!(list_filters_by_status);
            case!(delete_only_finished_owned_jobs);
        }
    };
}

conformance_tests!(postgres, super::super::postgres::test_store());

conformance_tests!(sqlite, async {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", dir.path().join("t.db").display());
    let store = super::super::sqlite::SqliteJobStore::open(&url)
        .await
        .unwrap();
    std::mem::forget(dir); // keep the file for the test's lifetime
    Some(std::sync::Arc::new(store) as std::sync::Arc<dyn super::super::JobStore>)
});
