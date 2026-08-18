//! Live-database ledger tests. Skipped unless BILLING_TEST_DATABASE_URL is
//! set (they mutate real rows); run against the dev database:
//!   BILLING_TEST_DATABASE_URL=postgres://scrapix:scrapix@localhost:5433/scrapix \
//!     cargo test -p scrapix-billing --test ledger_live

use sqlx::postgres::PgPoolOptions;

#[tokio::test]
async fn unchecked_deduction_allows_negative_balance() {
    let Ok(url) = std::env::var("BILLING_TEST_DATABASE_URL") else {
        eprintln!("skipped: BILLING_TEST_DATABASE_URL not set");
        return;
    };
    let pool = PgPoolOptions::new().connect(&url).await.unwrap();

    // Fresh account with a 2-credit balance.
    let account_id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO accounts (name, credits_balance) VALUES ('ledger-test', 2) RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let id = account_id.to_string();

    // Checked deduction refuses and reports the REAL balance.
    let err = scrapix_billing::ledger::deduct_credits(&pool, &id, 5, "crawl", "over budget")
        .await
        .unwrap_err();
    match err {
        scrapix_billing::BillingError::InsufficientCredits {
            available,
            required,
        } => {
            assert_eq!(available, 2);
            assert_eq!(required, 5);
        }
        other => panic!("unexpected error: {other:?}"),
    }

    // Post-hoc deduction always lands; balance goes negative.
    let new_balance =
        scrapix_billing::ledger::deduct_credits_unchecked(&pool, &id, 5, "crawl", "over budget")
            .await
            .unwrap();
    assert_eq!(new_balance, -3);

    let (balance, tx_count): (i64, i64) = sqlx::query_as(
        "SELECT a.credits_balance, (SELECT count(*) FROM transactions t WHERE t.account_id = a.id) \
         FROM accounts a WHERE a.id = $1",
    )
    .bind(account_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(balance, -3);
    assert_eq!(tx_count, 1);

    sqlx::query("DELETE FROM accounts WHERE id = $1")
        .bind(account_id)
        .execute(&pool)
        .await
        .unwrap();
}
