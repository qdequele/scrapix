//! # Scrapix OCR
//!
//! Turns scanned / image-only PDF pages (and uploaded images) into text, so
//! they are indexed with real content instead of as blank documents.
//!
//! Document parsing (`scrapix-parser`) already reports, per page, whether a
//! page has a usable text layer. This crate acts on that signal:
//!
//! 1. **Rasterize** the pages to OCR ([`PageRasterizer`]; PDFium through
//!    pdf-inspector's renderer, loaded at runtime).
//! 2. **Recognize** each page image with an [`OcrBackend`]: a vision LLM
//!    through `scrapix-ai` (layout- and table-aware Markdown, usage tracked
//!    like any other AI call) or a local Tesseract (no API key, no per-page
//!    cost; the zero-config fallback for self-hosted deployments).
//! 3. **Assemble** the OCR'd pages back into the document in page order,
//!    next to the natively extracted pages.
//!
//! Cost controls live here too: a per-document page cap, a per-account
//! daily page budget ([`OcrBudget`]) and a cache keyed by page-image hash
//! ([`OcrCache`]) so re-crawling a document never recognizes the same page
//! twice. Credit pricing is `scrapix-billing`'s job
//! (`OCR_PAGE_CREDITS`); [`OcrReport::billable_pages`] is what to charge.

pub mod backend;
pub mod budget;
pub mod cache;
pub mod engine;
pub mod error;
pub mod raster;

pub use backend::{OcrBackend, TesseractOcr, VisionLlmOcr};
pub use budget::{MemoryOcrBudget, OcrBudget, RedisOcrBudget};
pub use cache::{MemoryOcrCache, OcrCache, RedisOcrCache};
pub use engine::{OcrEngine, OcrReport, OcrRequest, OcrSettings};
pub use error::OcrError;
pub use raster::{PageRasterizer, PdfiumRasterizer};
