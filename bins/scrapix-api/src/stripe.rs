//! Stripe integration for engine-side auto-topup.
//!
//! The customer-facing billing surface (setup intents, payment methods,
//! purchases, invoices, pricing, webhooks) moved to the Rails app
//! (`saas/app/controllers/stripe_billing_controller.rb`, SCR-85). The engine
//! keeps only auto-topup: usage debits happen here, so when a balance dips
//! below the threshold it charges the saved default payment method through
//! the same invoice flow and grants credits idempotently.

use axum::{http::StatusCode, Json};
use serde::Serialize;
use sqlx::Row;
use stripe::{
    Client as StripeClient, CreateInvoice, CreateInvoiceItem, Currency, CustomerId, Invoice,
    InvoicePendingInvoiceItemsBehavior, PaymentIntentStatus,
};
use tracing::{error, info};

// Re-export pricing from the billing crate.
pub use scrapix_billing::calculate_price_cents;

type ApiError = (StatusCode, Json<StripeErrorBody>);

#[derive(Debug, Serialize)]
pub(crate) struct StripeErrorBody {
    error: String,
    code: String,
}

fn err(status: StatusCode, msg: &str, code: &str) -> ApiError {
    (
        status,
        Json(StripeErrorBody {
            error: msg.to_string(),
            code: code.to_string(),
        }),
    )
}

/// Create a Stripe Invoice with a line item, finalize it, and pay it.
/// Returns the paid Invoice object (with `invoice_pdf`, `hosted_invoice_url`, etc.).
async fn create_and_pay_invoice(
    stripe: &StripeClient,
    customer_id: CustomerId,
    account_id: uuid::Uuid,
    payment_method_id: &str,
    credits: i64,
    amount_cents: i64,
    purchase_type: &str,
) -> Result<Invoice, ApiError> {
    // 1. Create an invoice item (pending, attached to customer)
    let item_description = format!("Scrapix: {} credits", credits);
    let mut item_params = CreateInvoiceItem::new(customer_id.clone());
    item_params.amount = Some(amount_cents);
    item_params.currency = Some(Currency::USD);
    item_params.description = Some(&item_description);

    stripe::InvoiceItem::create(stripe, item_params)
        .await
        .map_err(|e| {
            error!(error = %e, "Failed to create InvoiceItem");
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to create invoice item",
                "stripe_error",
            )
        })?;

    // 2. Create a draft invoice (picks up the pending invoice item)
    let description = format!("Scrapix: {} credits", credits);
    let mut invoice_params = CreateInvoice::new();
    invoice_params.customer = Some(customer_id);
    invoice_params.collection_method = Some(stripe::CollectionMethod::ChargeAutomatically);
    invoice_params.auto_advance = Some(false); // we'll finalize and pay manually
    invoice_params.default_payment_method = Some(payment_method_id);
    invoice_params.description = Some(&description);
    invoice_params.pending_invoice_items_behavior =
        Some(InvoicePendingInvoiceItemsBehavior::Include);
    invoice_params.metadata = Some(
        [
            ("scrapix_account_id".to_string(), account_id.to_string()),
            ("credits".to_string(), credits.to_string()),
            ("type".to_string(), purchase_type.to_string()),
        ]
        .into_iter()
        .collect(),
    );

    let invoice = Invoice::create(stripe, invoice_params).await.map_err(|e| {
        error!(error = %e, "Failed to create Invoice");
        err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to create invoice",
            "stripe_error",
        )
    })?;

    // 3. Finalize the invoice
    let finalize_params: std::collections::HashMap<&str, &str> =
        [("auto_advance", "false")].into_iter().collect();
    let invoice: Invoice = stripe
        .post_form(
            &format!("/invoices/{}/finalize", invoice.id),
            finalize_params,
        )
        .await
        .map_err(|e| {
            error!(error = %e, invoice_id = %invoice.id, "Failed to finalize invoice");
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to finalize invoice",
                "stripe_error",
            )
        })?;

    // 4. Pay the invoice — expands the payment_intent so we can check its status
    let pay_params: std::collections::HashMap<&str, &str> =
        [("expand[]", "payment_intent")].into_iter().collect();
    let invoice: Invoice = stripe
        .post_form(&format!("/invoices/{}/pay", invoice.id), pay_params)
        .await
        .map_err(|e| {
            error!(error = %e, invoice_id = %invoice.id, "Failed to pay invoice");
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Payment failed. Please try again or use a different card.",
                "stripe_error",
            )
        })?;

    info!(
        account_id = %account_id,
        invoice_id = %invoice.id,
        credits,
        amount_cents,
        "Invoice created and paid"
    );

    Ok(invoice)
}

/// Add credits to an account after a successful payment.
/// Delegates to `scrapix_billing::add_credits_for_payment`.
async fn add_credits_for_payment(
    pool: &sqlx::PgPool,
    account_id: uuid::Uuid,
    credits: i64,
    payment_intent_id: &str,
    description: &str,
) -> Result<(), ApiError> {
    scrapix_billing::add_credits_for_payment(
        pool,
        account_id,
        credits,
        payment_intent_id,
        description,
    )
    .await
    .map_err(|e| {
        error!(error = %e, "Failed to add credits for payment");
        err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string(), e.code())
    })
}

/// Charge the account's default payment method for an automatic top-up.
pub async fn charge_auto_topup(
    stripe: &StripeClient,
    pool: &sqlx::PgPool,
    account_id: uuid::Uuid,
    credits: i64,
) -> Result<String, String> {
    // Get customer ID and default payment method
    let row = sqlx::query(
        "SELECT stripe_customer_id, stripe_default_payment_method_id FROM accounts WHERE id = $1",
    )
    .bind(account_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| format!("DB error: {e}"))?
    .ok_or("Account not found")?;

    let customer_id: Option<String> = row.get("stripe_customer_id");
    let pm_id: Option<String> = row.get("stripe_default_payment_method_id");

    let customer_id = customer_id.ok_or("No Stripe customer")?;
    let pm_id = pm_id.ok_or("No default payment method for auto-topup")?;

    let cid: CustomerId = customer_id.parse().map_err(|_| "Invalid customer ID")?;

    let amount_cents = calculate_price_cents(credits);

    let invoice = create_and_pay_invoice(
        stripe,
        cid,
        account_id,
        &pm_id,
        credits,
        amount_cents,
        "auto_topup",
    )
    .await
    .map_err(|e| format!("Invoice error: {}", e.1.error))?;

    let pi_status = invoice
        .payment_intent
        .as_ref()
        .and_then(|pi| pi.as_object())
        .map(|pi| pi.status);

    if pi_status == Some(PaymentIntentStatus::Succeeded) {
        let pi_id = invoice
            .payment_intent
            .as_ref()
            .map(|pi| pi.id().to_string())
            .unwrap_or_default();

        add_credits_for_payment(pool, account_id, credits, &pi_id, "Auto top-up (Stripe)")
            .await
            .map_err(|e| format!("Failed to add credits: {}", e.0))?;

        Ok(pi_id)
    } else {
        Err(format!("Auto-topup payment status: {:?}", pi_status))
    }
}
