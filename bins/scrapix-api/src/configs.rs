//! Cron scheduler for saved crawl configs.
//!
//! The configs CRUD (create/list/get/update/delete/trigger) moved to the
//! Rails app (`saas/app/controllers/configs_controller.rb`, SCR-85). The
//! engine keeps the scheduler: it owns crawl execution, so it polls the
//! shared `crawl_configs` table for due cron entries and starts the jobs.

use std::sync::Arc;

use sqlx::{PgPool, Row};
use tracing::{error, info, warn};

use scrapix_core::CrawlConfig;

use crate::{do_create_crawl, AccountContext, AppState};

/// Compute the next run time from a cron expression.
pub(crate) fn compute_next_run(cron_expr: &str) -> Result<chrono::DateTime<chrono::Utc>, String> {
    use croner::Cron;
    use std::str::FromStr;

    let cron = Cron::from_str(cron_expr).map_err(|e| format!("Invalid cron expression: {e}"))?;

    cron.find_next_occurrence(&chrono::Utc::now(), false)
        .map_err(|e| format!("Failed to compute next run: {e}"))
}

pub(crate) fn spawn_cron_scheduler(
    state: Arc<AppState>,
    pool: PgPool,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if let Err(e) = run_cron_tick(&state, &pool).await {
                        warn!(error = %e, "Cron scheduler tick failed");
                    }
                }
                _ = shutdown_rx.changed() => {
                    info!("Cron scheduler shutting down");
                    break;
                }
            }
        }
    })
}

async fn run_cron_tick(state: &Arc<AppState>, pool: &PgPool) -> Result<(), sqlx::Error> {
    // Fetch due configs with row-level locking
    let rows = sqlx::query(
        "SELECT * FROM crawl_configs \
         WHERE cron_enabled = true AND cron_expression IS NOT NULL AND next_run_at <= now() \
         FOR UPDATE SKIP LOCKED \
         LIMIT 50",
    )
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        return Ok(());
    }

    info!(count = rows.len(), "Cron scheduler: processing due configs");

    for row in &rows {
        let config_id: uuid::Uuid = row.get("id");
        let config_name: String = row.get("name");
        let cron_expr: String = row.get("cron_expression");
        let config_json: serde_json::Value = row.get("config");
        let account_id: uuid::Uuid = row.get("account_id");

        // Deserialize crawl config
        let crawl_config: CrawlConfig = match serde_json::from_value(config_json) {
            Ok(c) => c,
            Err(e) => {
                error!(
                    config_id = %config_id,
                    name = %config_name,
                    error = %e,
                    "Failed to deserialize stored config, disabling cron"
                );
                let _ = sqlx::query("UPDATE crawl_configs SET cron_enabled = false WHERE id = $1")
                    .bind(config_id)
                    .execute(pool)
                    .await;
                continue;
            }
        };

        let tier: String = sqlx::query_scalar("SELECT tier FROM accounts WHERE id = $1")
            .bind(account_id)
            .fetch_optional(pool)
            .await
            .unwrap_or(None)
            .unwrap_or_else(|| "free".to_string());

        let account_ctx = AccountContext {
            account_id: account_id.to_string(),
            api_key_id: None,
            tier,
            user_role: None,
        };

        // Trigger crawl
        match do_create_crawl(state, crawl_config, Some(&account_ctx)).await {
            Ok(response) => {
                // Compute next run
                let next_run = compute_next_run(&cron_expr).ok();

                let _ = sqlx::query(
                    "UPDATE crawl_configs SET last_run_at = now(), last_job_id = $1, next_run_at = $2 WHERE id = $3",
                )
                .bind(&response.job_id)
                .bind(next_run)
                .bind(config_id)
                .execute(pool)
                .await;

                info!(
                    config_id = %config_id,
                    name = %config_name,
                    job_id = %response.job_id,
                    "Cron: crawl triggered"
                );
            }
            Err(e) => {
                warn!(
                    config_id = %config_id,
                    name = %config_name,
                    error = ?e,
                    "Cron: failed to trigger crawl, advancing next_run_at"
                );

                // Advance next_run_at to avoid retry storm
                let next_run = compute_next_run(&cron_expr).ok();
                let _ = sqlx::query("UPDATE crawl_configs SET next_run_at = $1 WHERE id = $2")
                    .bind(next_run)
                    .bind(config_id)
                    .execute(pool)
                    .await;
            }
        }
    }

    Ok(())
}
