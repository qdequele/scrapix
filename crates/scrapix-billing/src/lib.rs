//! Scrapix Billing
//!
//! Credit pricing; the balance pre-check lives in the engine (via the Lab).
//! Ledger writes and payments live in the Rails control plane.

pub mod credits;
pub mod error;
pub mod pricing;

// Re-export key types at the crate root for convenience.
pub use credits::{
    crawl_credits, crawl_credits_per_page, ocr_credits, parse_credits, scrape_credits, MAP_CREDITS,
    OCR_PAGE_CREDITS, SEARCH_CREDITS,
};
pub use error::BillingError;
pub use pricing::calculate_price_cents;
