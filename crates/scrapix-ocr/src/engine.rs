//! The OCR pipeline: plan → rasterize → cache/budget → recognize → merge.

use std::sync::Arc;
use std::time::Duration;

use futures::stream::{self, StreamExt};
use scrapix_ai::{AiClient, AiUsageContext, AI_USAGE_CONTEXT};
use scrapix_core::OcrMode;
use scrapix_parser::{DocumentKind, ParsedDocument};
use serde::Serialize;
use tracing::{debug, info, warn};

use crate::backend::{OcrBackend, TesseractOcr, VisionLlmOcr};
use crate::budget::{MemoryOcrBudget, OcrBudget, RedisOcrBudget, ANONYMOUS_ACCOUNT};
use crate::cache::{cache_key, MemoryOcrCache, OcrCache, RedisOcrCache};
use crate::error::OcrError;
use crate::raster::{PageRasterizer, PdfiumRasterizer};

/// Server-wide OCR limits.
#[derive(Debug, Clone)]
pub struct OcrSettings {
    /// Hard per-document page cap (`OCR_MAX_PAGES_PER_DOCUMENT`, default
    /// 50). A request can lower it, never raise it.
    pub max_pages_per_document: u32,
    /// Per-account daily page budget (`OCR_DAILY_PAGE_BUDGET`, default
    /// 1000; `0` = unlimited).
    pub daily_page_budget: u64,
    /// Rasterization resolution (`OCR_RENDER_DPI`, default 150).
    pub dpi: f32,
    /// Pages recognized concurrently per document (`OCR_CONCURRENCY`,
    /// default 4).
    pub concurrency: usize,
}

impl Default for OcrSettings {
    fn default() -> Self {
        Self {
            max_pages_per_document: 50,
            daily_page_budget: 1000,
            dpi: 150.0,
            concurrency: 4,
        }
    }
}

impl OcrSettings {
    pub fn from_env() -> Self {
        let d = Self::default();
        Self {
            max_pages_per_document: env_parse("OCR_MAX_PAGES_PER_DOCUMENT")
                .unwrap_or(d.max_pages_per_document),
            daily_page_budget: env_parse("OCR_DAILY_PAGE_BUDGET").unwrap_or(d.daily_page_budget),
            dpi: env_parse::<f32>("OCR_RENDER_DPI")
                .unwrap_or(d.dpi)
                .clamp(72.0, 400.0),
            concurrency: env_parse::<usize>("OCR_CONCURRENCY")
                .unwrap_or(d.concurrency)
                .max(1),
        }
    }
}

fn env_parse<T: std::str::FromStr>(key: &str) -> Option<T> {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok())
}

/// One OCR request for one document.
#[derive(Debug, Clone, Default)]
pub struct OcrRequest {
    pub mode: OcrMode,
    /// Request/job page cap; only lowers `max_pages_per_document`.
    pub max_pages: Option<u32>,
    /// Account charged against the daily budget (`None` → shared
    /// anonymous bucket).
    pub account_id: Option<String>,
    /// Attribution for vision-LLM usage events (`feature` is forced to
    /// `ocr`).
    pub usage_context: Option<AiUsageContext>,
}

/// What OCR did to a document (returned in API responses as `ocr`).
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct OcrReport {
    pub mode: OcrMode,
    /// Backend that recognized pages (`vision:<provider>/<model>`,
    /// `tesseract:<lang>`); `None` when nothing was recognized.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// Pages whose text now comes from OCR (fresh + cached).
    pub pages_processed: u32,
    /// Of `pages_processed`, pages served from the OCR cache (not billed).
    pub pages_cached: u32,
    /// Pages left on the native text-extraction path.
    pub pages_skipped: u32,
    /// Pages that needed OCR but exceeded the page cap or daily budget.
    pub pages_capped: u32,
    /// Pages whose recognition failed.
    pub pages_failed: u32,
    /// 1-indexed pages that were OCR'd.
    pub pages: Vec<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

impl OcrReport {
    /// Pages to charge for: freshly recognized ones (cache hits are free).
    pub fn billable_pages(&self) -> u32 {
        self.pages_processed.saturating_sub(self.pages_cached)
    }

    fn off(total_pages: u32) -> Self {
        Self {
            mode: OcrMode::Off,
            pages_skipped: total_pages,
            ..Default::default()
        }
    }
}

/// Rasterizer + backend + cache + budget + limits.
pub struct OcrEngine {
    rasterizer: Arc<dyn PageRasterizer>,
    backend: Arc<dyn OcrBackend>,
    cache: Arc<dyn OcrCache>,
    budget: Arc<dyn OcrBudget>,
    settings: OcrSettings,
}

impl OcrEngine {
    pub fn new(
        rasterizer: Arc<dyn PageRasterizer>,
        backend: Arc<dyn OcrBackend>,
        cache: Arc<dyn OcrCache>,
        budget: Arc<dyn OcrBudget>,
        settings: OcrSettings,
    ) -> Self {
        Self {
            rasterizer,
            backend,
            cache,
            budget,
            settings,
        }
    }

    /// Build the engine from the environment:
    ///
    /// - `OCR_BACKEND` = `vision` (AI provider via `scrapix-ai`, needs
    ///   `ai_client`), `tesseract` (local CLI; `TESSERACT_PATH`,
    ///   `OCR_TESSERACT_LANG`), `off`, or `auto` (default: vision when an
    ///   AI client is configured, else tesseract).
    /// - `OCR_MODEL` overrides the provider's default vision model.
    /// - `OCR_REDIS_URL` (falling back to `REDIS_URL`) shares the cache
    ///   and daily budget across instances; otherwise both are in-process.
    ///   `OCR_CACHE_TTL_DAYS` (default 30), `OCR_CACHE_CAPACITY` (in-memory
    ///   entries, default 10000).
    ///
    /// Returns `None` when OCR is turned off. The rasterizer (PDFium) is
    /// loaded lazily, so a missing library only fails PDF OCR requests
    /// (with a warning in the report), not startup.
    pub async fn from_env(ai_client: Option<Arc<AiClient>>) -> Option<Self> {
        let choice = std::env::var("OCR_BACKEND")
            .unwrap_or_else(|_| "auto".to_string())
            .to_ascii_lowercase();
        let tesseract = || -> Arc<dyn OcrBackend> {
            Arc::new(TesseractOcr::new(
                std::env::var("TESSERACT_PATH").ok().map(Into::into),
                std::env::var("OCR_TESSERACT_LANG").ok(),
            ))
        };
        let vision = |client: Arc<AiClient>| -> Arc<dyn OcrBackend> {
            match std::env::var("OCR_MODEL") {
                Ok(model) if !model.trim().is_empty() => {
                    Arc::new(VisionLlmOcr::new(client, model.trim()))
                }
                _ => Arc::new(VisionLlmOcr::with_default_model(client)),
            }
        };
        let backend: Arc<dyn OcrBackend> = match (choice.as_str(), ai_client) {
            ("off" | "none" | "disabled", _) => return None,
            ("tesseract" | "local", _) => tesseract(),
            ("vision" | "llm", Some(client)) => vision(client),
            ("vision" | "llm", None) => {
                warn!("OCR_BACKEND=vision but no AI provider is configured; falling back to tesseract");
                tesseract()
            }
            (_, Some(client)) => vision(client),
            (_, None) => tesseract(),
        };

        let settings = OcrSettings::from_env();
        let redis_url = std::env::var("OCR_REDIS_URL")
            .or_else(|_| std::env::var("REDIS_URL"))
            .ok()
            .filter(|u| !u.trim().is_empty());
        let prefix = std::env::var("OCR_REDIS_PREFIX").unwrap_or_else(|_| "scrapix:ocr".into());
        let ttl = Duration::from_secs(
            env_parse::<u64>("OCR_CACHE_TTL_DAYS").unwrap_or(30).max(1) * 24 * 3600,
        );

        let mut cache: Arc<dyn OcrCache> = Arc::new(MemoryOcrCache::new(
            env_parse("OCR_CACHE_CAPACITY").unwrap_or(10_000),
        ));
        let mut budget: Arc<dyn OcrBudget> =
            Arc::new(MemoryOcrBudget::new(settings.daily_page_budget));
        if let Some(url) = redis_url {
            match RedisOcrCache::connect(&url, &prefix, ttl).await {
                Ok(c) => cache = Arc::new(c),
                Err(e) => warn!(error = %e, "OCR cache: Redis unavailable, using in-memory cache"),
            }
            match RedisOcrBudget::connect(&url, &prefix, settings.daily_page_budget).await {
                Ok(b) => budget = Arc::new(b),
                Err(e) => {
                    warn!(error = %e, "OCR budget: Redis unavailable, using in-process budget")
                }
            }
        }

        info!(
            backend = backend.name(),
            max_pages_per_document = settings.max_pages_per_document,
            daily_page_budget = settings.daily_page_budget,
            "OCR enabled (opt-in per request/job)"
        );
        Some(Self::new(
            Arc::new(PdfiumRasterizer::new()),
            backend,
            cache,
            budget,
            settings,
        ))
    }

    pub fn backend_name(&self) -> &str {
        self.backend.name()
    }

    pub fn settings(&self) -> &OcrSettings {
        &self.settings
    }

    /// The page cap for a request: the server cap, lowered by the request.
    pub fn page_cap(&self, requested: Option<u32>) -> u32 {
        let cap = self.settings.max_pages_per_document;
        requested.map_or(cap, |r| r.min(cap))
    }

    /// Pages `apply` would OCR (before cache hits and the daily budget), in
    /// order — for pre-flight cost estimates. Pure.
    pub fn plan(&self, parsed: &ParsedDocument, mode: OcrMode, max_pages: Option<u32>) -> Vec<u32> {
        let mut pages = candidates(parsed, mode);
        pages.truncate(self.page_cap(max_pages) as usize);
        pages
    }

    /// Run OCR on `parsed` (parsed from `bytes`) according to `request`,
    /// merging recognized pages into `parsed.markdown` in page order and
    /// removing them from `parsed.pages_needing_ocr`. Never fails: problems
    /// are reported in the returned [`OcrReport`] and the document keeps
    /// its native content.
    pub async fn apply(
        &self,
        bytes: &[u8],
        parsed: &mut ParsedDocument,
        request: &OcrRequest,
    ) -> OcrReport {
        let total_pages = parsed.pages_processed.or(parsed.page_count).unwrap_or(1);
        if request.mode.is_off() {
            return OcrReport::off(total_pages);
        }

        let all = candidates(parsed, request.mode);
        let planned = self.plan(parsed, request.mode, request.max_pages);
        let mut report = OcrReport {
            mode: request.mode,
            pages_capped: (all.len() - planned.len()) as u32,
            ..Default::default()
        };
        if planned.is_empty() {
            report.pages_skipped = total_pages;
            return report;
        }

        // 1. Rasterize (PDF) or use the uploaded image as-is.
        let images = match self.page_images(bytes, parsed, &planned).await {
            Ok(images) => images,
            Err(e) => {
                warn!(error = %e, "OCR skipped: pages could not be rasterized");
                report.pages_failed = planned.len() as u32;
                report.pages_skipped = total_pages - planned.len() as u32;
                // Every OCR warning names OCR, so callers can show it as is.
                report.warning = Some(match e {
                    OcrError::Unavailable(_) => e.to_string(),
                    _ => format!("OCR skipped: {e}"),
                });
                return report;
            }
        };

        // 2. Cache lookups; reserve budget for the misses.
        let backend_name = self.backend.name().to_string();
        let mut recognized: Vec<(u32, String)> = Vec::new();
        let mut misses: Vec<(u32, Vec<u8>, String, String)> = Vec::new();
        for (page, (image, media_type)) in planned.iter().copied().zip(images) {
            let key = cache_key(&backend_name, &image);
            match self.cache.get(&key).await {
                Some(text) => {
                    report.pages_cached += 1;
                    recognized.push((page, text));
                }
                None => misses.push((page, image, media_type, key)),
            }
        }
        let account = request
            .account_id
            .as_deref()
            .filter(|a| !a.is_empty())
            .unwrap_or(ANONYMOUS_ACCOUNT);
        let granted = if misses.is_empty() {
            0
        } else {
            self.budget.reserve(account, misses.len() as u32).await as usize
        };
        if granted < misses.len() {
            let over = misses.len() - granted;
            report.pages_capped += over as u32;
            report.warning = Some(format!(
                "daily OCR page budget reached: {over} page(s) not recognized"
            ));
            misses.truncate(granted);
        }

        // 3. Recognize the misses concurrently, within the usage context.
        let context = request.usage_context.clone().map(|mut c| {
            c.feature = "ocr".to_string();
            c
        });
        let backend = self.backend.clone();
        let results: Vec<(u32, String, Result<String, OcrError>)> = stream::iter(misses)
            .map(|(page, image, media_type, key)| {
                let backend = backend.clone();
                let context = context.clone();
                async move {
                    let fut = async { backend.recognize(&image, &media_type).await };
                    let result = match context {
                        Some(ctx) => AI_USAGE_CONTEXT.scope(ctx, fut).await,
                        None => fut.await,
                    };
                    (page, key, result)
                }
            })
            .buffer_unordered(self.settings.concurrency)
            .collect()
            .await;

        let mut failed = 0u32;
        let mut last_error = None;
        for (page, key, result) in results {
            match result {
                Ok(text) => {
                    self.cache.put(&key, &text).await;
                    recognized.push((page, text));
                }
                Err(e) => {
                    debug!(page, error = %e, "OCR failed on page");
                    failed += 1;
                    last_error = Some(e.to_string());
                }
            }
        }
        if failed > 0 {
            self.budget.release(account, failed).await;
            report.pages_failed = failed;
            report.warning = Some(format!(
                "OCR failed on {failed} page(s): {}",
                last_error.unwrap_or_default()
            ));
        }

        recognized.sort_by_key(|(page, _)| *page);
        report.pages_processed = recognized.len() as u32;
        report.pages = recognized.iter().map(|(p, _)| *p).collect();
        report.pages_skipped = total_pages.saturating_sub(report.pages_processed);
        if !recognized.is_empty() {
            report.backend = Some(backend_name);
            self.merge(bytes, parsed, &recognized).await;
        }
        info!(
            mode = request.mode.as_str(),
            processed = report.pages_processed,
            cached = report.pages_cached,
            capped = report.pages_capped,
            failed = report.pages_failed,
            "OCR finished"
        );
        report
    }

    async fn page_images(
        &self,
        bytes: &[u8],
        parsed: &ParsedDocument,
        pages: &[u32],
    ) -> Result<Vec<(Vec<u8>, String)>, OcrError> {
        if parsed.kind == DocumentKind::Image {
            let media_type = image_media_type(bytes).to_string();
            return Ok(vec![(bytes.to_vec(), media_type)]);
        }
        let rasterizer = self.rasterizer.clone();
        let pdf = bytes.to_vec();
        let pages = pages.to_vec();
        let dpi = self.settings.dpi;
        let pngs = tokio::task::spawn_blocking(move || rasterizer.render_png(&pdf, &pages, dpi))
            .await
            .map_err(|e| OcrError::Render(format!("render task failed: {e}")))??;
        Ok(pngs
            .into_iter()
            .map(|png| (png, "image/png".to_string()))
            .collect())
    }

    /// Put recognized pages back in page order next to the natively
    /// extracted ones.
    async fn merge(&self, bytes: &[u8], parsed: &mut ParsedDocument, recognized: &[(u32, String)]) {
        let ocr_pages: Vec<u32> = recognized.iter().map(|(p, _)| *p).collect();
        if parsed.kind == DocumentKind::Pdf {
            let pdf = bytes.to_vec();
            let max = parsed.pages_processed;
            let native =
                tokio::task::spawn_blocking(move || scrapix_parser::pdf::page_markdown(&pdf, max))
                    .await
                    .ok()
                    .and_then(Result::ok);
            parsed.markdown = match native {
                Some(mut pages) => {
                    let needed = recognized
                        .iter()
                        .map(|(p, _)| *p as usize)
                        .max()
                        .unwrap_or(0);
                    if pages.len() < needed {
                        pages.resize(needed, String::new());
                    }
                    for (page, text) in recognized {
                        pages[*page as usize - 1] = text.trim().to_string();
                    }
                    join_non_empty(pages.iter().map(String::as_str))
                }
                // Per-page re-extraction failed: keep the whole-document
                // Markdown and append the recognized pages in page order,
                // rather than losing the native text.
                None => join_non_empty(
                    std::iter::once(parsed.markdown.as_str())
                        .chain(recognized.iter().map(|(_, t)| t.as_str())),
                ),
            };
        } else {
            parsed.markdown = join_non_empty(recognized.iter().map(|(_, t)| t.as_str()));
        }
        parsed.pages_needing_ocr.retain(|p| !ocr_pages.contains(p));
        parsed.ocr_reasons.retain(|(p, _)| !ocr_pages.contains(p));
        let text = parsed.text();
        parsed.language = if text.trim().is_empty() {
            None
        } else {
            scrapix_parser::detect_language(&text)
        };
    }
}

/// Trimmed, non-empty parts separated by blank lines.
fn join_non_empty<'a>(parts: impl Iterator<Item = &'a str>) -> String {
    parts
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Pages eligible for OCR under `mode`, before any cap.
fn candidates(parsed: &ParsedDocument, mode: OcrMode) -> Vec<u32> {
    match mode {
        OcrMode::Off => Vec::new(),
        OcrMode::Auto => parsed.pages_needing_ocr.clone(),
        OcrMode::Force => match parsed.kind {
            DocumentKind::Pdf | DocumentKind::Image => {
                let total = parsed.pages_processed.or(parsed.page_count).unwrap_or(1);
                (1..=total).collect()
            }
            // Office formats have a real text layer; nothing to rasterize.
            _ => Vec::new(),
        },
    }
}

fn image_media_type(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"\x89PNG") {
        "image/png"
    } else if bytes.starts_with(b"\xFF\xD8\xFF") {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF8") {
        "image/gif"
    } else if bytes.len() >= 12 && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else {
        "image/tiff"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FakeRaster;
    impl PageRasterizer for FakeRaster {
        fn name(&self) -> &str {
            "fake"
        }
        fn render_png(&self, _: &[u8], pages: &[u32], _: f32) -> Result<Vec<Vec<u8>>, OcrError> {
            Ok(pages
                .iter()
                .map(|p| format!("page-{p}").into_bytes())
                .collect())
        }
    }

    struct BrokenRaster;
    impl PageRasterizer for BrokenRaster {
        fn name(&self) -> &str {
            "broken"
        }
        fn render_png(&self, _: &[u8], _: &[u32], _: f32) -> Result<Vec<Vec<u8>>, OcrError> {
            Err(OcrError::Unavailable("no pdfium".into()))
        }
    }

    #[derive(Default)]
    struct FakeBackend {
        calls: AtomicUsize,
    }
    #[async_trait]
    impl OcrBackend for FakeBackend {
        fn name(&self) -> &str {
            "fake-ocr"
        }
        async fn recognize(&self, image: &[u8], _: &str) -> Result<String, OcrError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(format!("OCR {}", String::from_utf8_lossy(image)))
        }
    }

    fn engine(backend: Arc<FakeBackend>, budget: u64, cap: u32) -> OcrEngine {
        OcrEngine::new(
            Arc::new(FakeRaster),
            backend,
            Arc::new(MemoryOcrCache::new(100)),
            Arc::new(MemoryOcrBudget::new(budget)),
            OcrSettings {
                max_pages_per_document: cap,
                daily_page_budget: budget,
                ..OcrSettings::default()
            },
        )
    }

    /// A 3-page "PDF" whose pages 2 and 3 need OCR. Not real PDF bytes, so
    /// the native per-page re-extraction fails and the OCR text is appended
    /// to the whole-document Markdown (real per-page merging is covered by
    /// the content-worker and API tests on the mixed-PDF fixture).
    fn scanned_doc() -> ParsedDocument {
        ParsedDocument {
            markdown: String::new(),
            page_count: Some(3),
            pages_processed: Some(3),
            pages_needing_ocr: vec![2, 3],
            ..ParsedDocument::empty(DocumentKind::Pdf)
        }
    }

    fn request(mode: OcrMode) -> OcrRequest {
        OcrRequest {
            mode,
            account_id: Some("acct".into()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn off_touches_nothing() {
        let backend = Arc::new(FakeBackend::default());
        let e = engine(backend.clone(), 0, 50);
        let mut doc = scanned_doc();
        let report = e.apply(b"x", &mut doc, &request(OcrMode::Off)).await;
        assert_eq!(report.pages_processed, 0);
        assert_eq!(report.pages_skipped, 3);
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
        assert!(doc.needs_ocr());
    }

    #[tokio::test]
    async fn auto_ocrs_only_flagged_pages() {
        let backend = Arc::new(FakeBackend::default());
        let e = engine(backend.clone(), 0, 50);
        let mut doc = scanned_doc();
        let report = e.apply(b"x", &mut doc, &request(OcrMode::Auto)).await;
        assert_eq!(report.pages, vec![2, 3]);
        assert_eq!(report.pages_processed, 2);
        assert_eq!(report.pages_skipped, 1);
        assert_eq!(report.billable_pages(), 2);
        assert_eq!(report.backend.as_deref(), Some("fake-ocr"));
        assert!(!doc.needs_ocr());
        assert!(doc.markdown.contains("OCR page-2") && doc.markdown.contains("OCR page-3"));
        assert!(doc.markdown.find("page-2") < doc.markdown.find("page-3"));
    }

    #[tokio::test]
    async fn failed_native_reextraction_keeps_the_native_markdown() {
        let e = engine(Arc::new(FakeBackend::default()), 0, 50);
        let mut doc = scanned_doc();
        doc.markdown = "# Native page one".into();
        e.apply(b"not a pdf", &mut doc, &request(OcrMode::Auto))
            .await;
        assert!(
            doc.markdown.starts_with("# Native page one"),
            "{}",
            doc.markdown
        );
        assert!(doc.markdown.ends_with("OCR page-3"), "{}", doc.markdown);
    }

    #[tokio::test]
    async fn force_ocrs_every_page_up_to_the_cap() {
        let backend = Arc::new(FakeBackend::default());
        let e = engine(backend.clone(), 0, 2);
        let mut doc = scanned_doc();
        let report = e.apply(b"x", &mut doc, &request(OcrMode::Force)).await;
        assert_eq!(report.pages, vec![1, 2]);
        assert_eq!(report.pages_capped, 1);
        // Page 3 still needs OCR: it was capped.
        assert_eq!(doc.pages_needing_ocr, vec![3]);
    }

    #[tokio::test]
    async fn request_cap_only_lowers_the_server_cap() {
        let e = engine(Arc::new(FakeBackend::default()), 0, 2);
        assert_eq!(e.page_cap(Some(10)), 2);
        assert_eq!(e.page_cap(Some(1)), 1);
        assert_eq!(e.plan(&scanned_doc(), OcrMode::Auto, Some(1)), vec![2]);
    }

    #[tokio::test]
    async fn cache_hits_are_not_recognized_or_billed_twice() {
        let backend = Arc::new(FakeBackend::default());
        let e = engine(backend.clone(), 0, 50);
        let first = e
            .apply(b"x", &mut scanned_doc(), &request(OcrMode::Auto))
            .await;
        assert_eq!(first.billable_pages(), 2);
        let second = e
            .apply(b"x", &mut scanned_doc(), &request(OcrMode::Auto))
            .await;
        assert_eq!(second.pages_processed, 2);
        assert_eq!(second.pages_cached, 2);
        assert_eq!(second.billable_pages(), 0);
        assert_eq!(
            backend.calls.load(Ordering::SeqCst),
            2,
            "no new recognition"
        );
    }

    #[tokio::test]
    async fn daily_budget_caps_recognition() {
        let backend = Arc::new(FakeBackend::default());
        let e = engine(backend.clone(), 1, 50);
        let mut doc = scanned_doc();
        let report = e.apply(b"x", &mut doc, &request(OcrMode::Auto)).await;
        assert_eq!(report.pages_processed, 1);
        assert_eq!(report.pages_capped, 1);
        assert!(report.warning.unwrap().contains("budget"));
    }

    #[tokio::test]
    async fn missing_rasterizer_keeps_native_content() {
        let e = OcrEngine::new(
            Arc::new(BrokenRaster),
            Arc::new(FakeBackend::default()),
            Arc::new(MemoryOcrCache::new(10)),
            Arc::new(MemoryOcrBudget::new(0)),
            OcrSettings::default(),
        );
        let mut doc = scanned_doc();
        doc.markdown = "native".into();
        let report = e.apply(b"x", &mut doc, &request(OcrMode::Auto)).await;
        assert_eq!(report.pages_failed, 2);
        assert!(report.warning.unwrap().contains("no pdfium"));
        assert_eq!(doc.markdown, "native");
        assert!(doc.needs_ocr());
    }

    #[tokio::test]
    async fn images_are_recognized_without_rasterizing() {
        let backend = Arc::new(FakeBackend::default());
        let e = engine(backend, 0, 50);
        let mut doc =
            scrapix_parser::parse_document(b"\x89PNG\r\n\x1a\nimg", None, &Default::default())
                .unwrap();
        let report = e
            .apply(b"\x89PNG\r\n\x1a\nimg", &mut doc, &request(OcrMode::Auto))
            .await;
        assert_eq!(report.pages, vec![1]);
        assert!(doc.markdown.starts_with("OCR "));
        assert!(!doc.needs_ocr());
    }
}
