//! Binary documents on the engine API (SCR-81, SCR-86).
//!
//! - `POST /scrape` of a URL that serves a PDF or an office document
//!   (Word, PowerPoint, Excel, OpenDocument, RTF, EPUB, CSV) parses it
//!   instead of rejecting the content type.
//! - `POST /parse` parses an uploaded file (multipart `file`), plus images
//!   when OCR is requested.
//!
//! Both return the `/scrape` response shape (`markdown`, `content`,
//! `metadata`, `language`, `links`, `ai`, ...) plus `document` (format,
//! pages, OCR assessment) and `ocr` (what OCR did). Both go through the
//! parser crate's format dispatch — the same one the crawl pipeline's
//! content worker uses — and the same OCR engine.
//!
//! OCR is opt-in per request (`parsers.ocr`: `off` | `auto` | `force`).
//! Before recognizing anything, the credit pre-flight is re-run with the
//! planned OCR pages at `OCR_PAGE_CREDITS` each; after, the document is
//! billed under its operation (`scrape` / `parse`) and the freshly
//! recognized pages as a separate `ocr` ledger entry, so OCR spend is
//! attributable. Cache hits are not billed.

use std::sync::{Arc, OnceLock};
use std::time::Instant;

use axum::{
    extract::{Multipart, State},
    Extension, Json,
};
use serde::{Deserialize, Serialize};
use tracing::info;

use scrapix_ai::AiUsageContext;
use scrapix_core::OcrMode;
use scrapix_ocr::{OcrReport, OcrRequest};
use scrapix_parser::{document::markdown_links, DocumentKind, ParseOptions, ParsedDocument};
use scrapix_storage::clickhouse::RequestEvent as ClickHouseRequestEvent;

use crate::auth::{AuthenticatedAccount, AuthenticatedUser};
use crate::lab_events::LabEvent;
use crate::{
    billing, check_write_permission, extract_account_context, extract_domain, run_ai_enrichment,
    AccountContext, AiOptions, AiRun, ApiError, AppState, ScrapeFormat, ScrapeMetadata,
    ScrapeResponse,
};

/// Default size cap for documents fetched by `/scrape` and uploaded to
/// `/parse` (`DOCUMENT_MAX_SIZE_MB`).
const DEFAULT_DOCUMENT_MAX_MB: u64 = 50;

/// The document size cap in bytes (`DOCUMENT_MAX_SIZE_MB`, default 50).
pub(crate) fn max_document_bytes() -> u64 {
    static MAX: OnceLock<u64> = OnceLock::new();
    *MAX.get_or_init(|| {
        std::env::var("DOCUMENT_MAX_SIZE_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|mb| *mb > 0)
            .unwrap_or(DEFAULT_DOCUMENT_MAX_MB)
            .saturating_mul(1024 * 1024)
    })
}

/// Document parsing options for `/scrape` and `/parse`.
#[derive(Debug, Clone, Default, Deserialize, utoipa::ToSchema)]
pub(crate) struct ParserOptions {
    /// OCR for scanned / image-only pages: `off` (default — scanned pages
    /// are flagged in `document.pages_needing_ocr`, not recognized), `auto`
    /// (OCR only the pages that need it) or `force` (OCR every page, for
    /// PDFs whose broken font encodings extract as garbage). OCR pages are
    /// billed at a higher per-page rate.
    #[serde(default)]
    pub ocr: OcrMode,
    /// Per-document OCR page cap. Can only lower the server cap
    /// (`OCR_MAX_PAGES_PER_DOCUMENT`, default 50).
    #[serde(default)]
    pub ocr_max_pages: Option<u32>,
    /// Parse at most this many PDF pages (the first N).
    #[serde(default)]
    pub max_pages: Option<u32>,
}

/// What the document parser found.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct DocumentInfo {
    /// Detected format: `pdf`, `docx`, `xlsx`, `pptx`, `doc`, `ppt`, `odt`,
    /// `ods`, `odp`, `rtf`, `epub`, `csv`, `image`.
    pub format: String,
    /// Canonical media type of the format.
    pub content_type: String,
    /// Parser backend (`pdf-inspector`, `anydoc`, `image`).
    pub parser: String,
    /// Document size in bytes.
    pub bytes: u64,
    /// Pages in the document (PDF, image).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_count: Option<u32>,
    /// Pages actually parsed (≤ `page_count` with `parsers.max_pages`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pages_processed: Option<u32>,
    /// PDF classification: `text_based`, `scanned`, `image_based`, `mixed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pdf_type: Option<String>,
    /// Whether some pages still have no text (not OCR'd).
    pub needs_ocr: bool,
    /// 1-indexed pages that still need OCR.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pages_needing_ocr: Vec<u32>,
    /// Tables were detected and rendered as Markdown tables.
    pub has_tables: bool,
}

impl DocumentInfo {
    fn new(parsed: &ParsedDocument, bytes: usize) -> Self {
        Self {
            format: parsed.kind.as_str().to_string(),
            content_type: parsed.kind.mime_type().to_string(),
            parser: parsed.parser.to_string(),
            bytes: bytes as u64,
            page_count: parsed.page_count,
            pages_processed: parsed.pages_processed,
            pdf_type: parsed.pdf_type.map(str::to_string),
            needs_ocr: parsed.needs_ocr(),
            pages_needing_ocr: parsed.pages_needing_ocr.clone(),
            has_tables: parsed.has_tables,
        }
    }
}

/// What OCR did (present when `parsers.ocr` is not `off`).
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct OcrInfo {
    pub mode: OcrMode,
    /// Backend that recognized pages (`vision:<provider>/<model>`,
    /// `tesseract:<lang>`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// Pages whose text now comes from OCR.
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

impl From<&OcrReport> for OcrInfo {
    fn from(r: &OcrReport) -> Self {
        Self {
            mode: r.mode,
            backend: r.backend.clone(),
            pages_processed: r.pages_processed,
            pages_cached: r.pages_cached,
            pages_skipped: r.pages_skipped,
            pages_capped: r.pages_capped,
            pages_failed: r.pages_failed,
            pages: r.pages.clone(),
            warning: r.warning.clone(),
        }
    }
}

/// One document to turn into a `/scrape`-shaped response.
pub(crate) struct DocumentJob<'a> {
    /// `scrape` or `parse` (analytics + ledger operation).
    pub operation: &'static str,
    /// Response `url`: the final URL (`/scrape`) or `upload://<filename>`.
    pub label: String,
    /// Base for resolving relative links (`/scrape` only).
    pub base_url: Option<String>,
    /// Title fallback (URL basename or uploaded filename).
    pub fallback_title: Option<String>,
    pub bytes: Vec<u8>,
    pub content_type: Option<String>,
    /// Requested formats (empty = markdown + content + metadata).
    pub formats: Vec<ScrapeFormat>,
    pub include_links: bool,
    pub parsers: ParserOptions,
    pub ai: Option<&'a AiOptions>,
    pub status_code: u16,
    pub js_rendered: bool,
    /// Credits for the document itself (before OCR).
    pub base_cost: i64,
}

/// Usage events for one parsed document: the document itself, plus its OCR
/// pages (when any were billable) as a second event in the same write.
async fn record_document_usage(
    state: &AppState,
    ctx: &AccountContext,
    operation: &str,
    label: &str,
    base_cost: i64,
    ocr_billable: u32,
) {
    let mut events = vec![LabEvent::usage(
        &ctx.account_id,
        ctx.api_key_id.as_deref(),
        operation,
        base_cost,
        serde_json::json!({}),
        format!("{label} ({base_cost} credits)"),
        None,
    )];
    if ocr_billable > 0 {
        let ocr_cost = scrapix_billing::ocr_credits(ocr_billable as u64);
        events.push(LabEvent::usage(
            &ctx.account_id,
            ctx.api_key_id.as_deref(),
            "ocr",
            ocr_cost,
            serde_json::json!({ "pages_ocr": ocr_billable }),
            format!("{label} ({ocr_billable} OCR pages, {ocr_cost} credits)"),
            None,
        ));
    }
    state.record_events(&events).await;
}

/// Parse (+ OCR, + AI) a document, bill it, and build the response.
pub(crate) async fn document_response(
    state: &Arc<AppState>,
    account_ctx: &Option<AccountContext>,
    job: DocumentJob<'_>,
    start_time: Instant,
) -> Result<ScrapeResponse, ApiError> {
    let bytes_len = job.bytes.len();
    let kind =
        scrapix_parser::detect_kind(job.content_type.as_deref(), &job.bytes).ok_or_else(|| {
            ApiError::new(
                format!(
                    "Unsupported document format (content type {}). Supported: PDF, DOC/DOCX, \
                 PPT/PPTX, XLS/XLSX/XLSB, ODT/ODS/ODP, RTF, EPUB, CSV, and images with OCR",
                    job.content_type.as_deref().unwrap_or("unknown")
                ),
                "unsupported_document",
            )
        })?;
    let ocr_mode = job.parsers.ocr;
    if kind == DocumentKind::Image && ocr_mode.is_off() {
        return Err(ApiError::new(
            "Images have no text layer: set parsers.ocr to \"auto\" to recognize them",
            "ocr_required",
        ));
    }

    // Parsing is CPU-bound: keep it off the async runtime.
    let opts = ParseOptions {
        max_pages: job.parsers.max_pages,
    };
    let parse_bytes = job.bytes.clone();
    let mut parsed = tokio::task::spawn_blocking(move || {
        scrapix_parser::document::default_dispatch().parse(&parse_bytes, kind, &opts)
    })
    .await
    .map_err(|e| ApiError::new(format!("Document parse task failed: {e}"), "internal_error"))?
    .map_err(|e| ApiError::new(format!("Failed to parse document: {e}"), "parse_error"))?;

    let mut warnings: Vec<String> = Vec::new();
    let mut ocr_report: Option<OcrReport> = None;
    if !ocr_mode.is_off() {
        match state.ocr.as_ref() {
            Some(engine) => {
                // Pre-flight with the planned OCR pages (cache hits and the
                // daily budget can only lower the final cost).
                let planned = engine.plan(&parsed, ocr_mode, job.parsers.ocr_max_pages);
                if let (Some(pool), Some(ctx)) = (&state.saas_pool, account_ctx) {
                    let estimate =
                        job.base_cost + scrapix_billing::ocr_credits(planned.len() as u64);
                    billing::check_credits(pool, &ctx.account_id, estimate).await?;
                }
                let request = OcrRequest {
                    mode: ocr_mode,
                    max_pages: job.parsers.ocr_max_pages,
                    account_id: account_ctx.as_ref().map(|c| c.account_id.clone()),
                    usage_context: Some(AiUsageContext {
                        job_id: String::new(),
                        account_id: account_ctx.as_ref().map(|c| c.account_id.clone()),
                        feature: "ocr".to_string(),
                        url: job.label.clone(),
                    }),
                };
                let report = engine.apply(&job.bytes, &mut parsed, &request).await;
                if let Some(ref w) = report.warning {
                    warnings.push(w.clone());
                }
                ocr_report = Some(report);
            }
            None => warnings.push(
                "OCR is disabled on this server (OCR_BACKEND=off); scanned pages are flagged in \
                 document.pages_needing_ocr but not recognized"
                    .to_string(),
            ),
        }
    }
    if kind == DocumentKind::Image && parsed.markdown.is_empty() && ocr_report.is_none() {
        return Err(ApiError::new(
            "OCR is disabled on this server; images cannot be parsed",
            "ocr_unavailable",
        ));
    }
    if parsed.needs_ocr() && ocr_mode.is_off() {
        warnings.push(format!(
            "{} page(s) have no text layer (scanned); set parsers.ocr to \"auto\" to recognize them",
            parsed.pages_needing_ocr.len()
        ));
    }

    // Build the /scrape-shaped fields.
    let formats = if job.formats.is_empty() {
        vec![
            ScrapeFormat::Markdown,
            ScrapeFormat::Content,
            ScrapeFormat::Metadata,
        ]
    } else {
        job.formats.clone()
    };
    let text = parsed.text();
    let markdown = formats
        .contains(&ScrapeFormat::Markdown)
        .then(|| parsed.markdown.clone());
    let content = formats
        .contains(&ScrapeFormat::Content)
        .then(|| text.clone());
    let metadata = formats
        .contains(&ScrapeFormat::Metadata)
        .then(|| ScrapeMetadata {
            title: parsed.title.clone().or(job.fallback_title.clone()),
            description: parsed.subject.clone(),
            author: parsed.author.clone(),
            keywords: parsed
                .keywords
                .as_deref()
                .map(|k| {
                    k.split([',', ';'])
                        .map(|w| w.trim().to_string())
                        .filter(|w| !w.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
            canonical_url: None,
            published_date: None,
            open_graph: Default::default(),
            twitter: Default::default(),
        });
    let links = (formats.contains(&ScrapeFormat::Links) || job.include_links).then(|| {
        markdown_links(
            &parsed.markdown,
            job.base_url.as_deref().unwrap_or("upload://document"),
        )
    });
    for (format, name) in [
        (ScrapeFormat::Html, "html"),
        (ScrapeFormat::RawHtml, "rawhtml"),
        (ScrapeFormat::Schema, "schema"),
        (ScrapeFormat::Blocks, "blocks"),
        (ScrapeFormat::Screenshot, "screenshot"),
    ] {
        if formats.contains(&format) {
            warnings.push(format!("format \"{name}\" is not available for documents"));
        }
    }

    let AiRun {
        result: ai_result,
        warning: ai_warning,
        prompt_tokens,
        completion_tokens,
        model,
    } = run_ai_enrichment(
        state,
        job.ai,
        if parsed.markdown.is_empty() {
            &text
        } else {
            &parsed.markdown
        },
    )
    .await;
    if let Some(w) = ai_warning {
        warnings.push(w);
    }

    let duration_ms = start_time.elapsed().as_millis() as u64;
    let ocr_billable = ocr_report.as_ref().map_or(0, |r| r.billable_pages());

    // Analytics: one request row, OCR pages in their own column.
    if let Some(ref batcher) = state.analytics.request_batcher {
        let event = ClickHouseRequestEvent {
            account_id: account_ctx
                .as_ref()
                .map(|c| c.account_id.clone())
                .unwrap_or_default(),
            api_key_id: account_ctx
                .as_ref()
                .and_then(|c| c.api_key_id.clone())
                .unwrap_or_default(),
            operation: job.operation.to_string(),
            url: job.label.clone(),
            domain: job
                .base_url
                .as_deref()
                .and_then(extract_domain)
                .unwrap_or_default(),
            status_code: job.status_code,
            duration_ms: duration_ms as u32,
            content_length: bytes_len as u64,
            js_rendered: job.js_rendered,
            ai_summary: job.ai.is_some_and(|a| a.summary),
            ai_extraction: job.ai.is_some_and(|a| a.extract.is_some()),
            ai_prompt_tokens: prompt_tokens,
            ai_completion_tokens: completion_tokens,
            ai_model: model,
            pages_fetched: 1,
            ocr_pages: ocr_billable,
            timestamp: time::OffsetDateTime::now_utc(),
            ..Default::default()
        };
        let batcher = batcher.clone();
        tokio::spawn(async move {
            let _ = batcher.add(event).await;
        });
    }

    // Usage: the document under its operation, OCR pages separately.
    if let Some(ctx) = account_ctx {
        record_document_usage(
            state,
            ctx,
            job.operation,
            &job.label,
            job.base_cost,
            ocr_billable,
        )
        .await;
    }

    info!(
        operation = job.operation,
        url = %job.label,
        format = kind.as_str(),
        bytes = bytes_len,
        pages = ?parsed.page_count,
        ocr_pages = ocr_billable,
        duration_ms,
        "Document parsed"
    );

    Ok(ScrapeResponse {
        success: true,
        url: job.label,
        markdown,
        html: None,
        raw_html: None,
        content,
        metadata,
        links,
        language: parsed.language.clone(),
        schema: None,
        blocks: None,
        extract: None,
        ai: ai_result,
        screenshot: None,
        actions: None,
        warning: (!warnings.is_empty()).then(|| warnings.join("; ")),
        document: Some(DocumentInfo::new(&parsed, bytes_len)),
        ocr: ocr_report.as_ref().map(OcrInfo::from),
        status_code: job.status_code,
        scrape_duration_ms: duration_ms,
    })
}

/// Options part of a `/parse` upload (the `options` multipart field, JSON).
#[derive(Debug, Default, Deserialize, utoipa::ToSchema)]
pub(crate) struct ParseRequestOptions {
    /// Formats to return (default: markdown, content, metadata). `html`,
    /// `rawhtml`, `schema`, `blocks` and `screenshot` do not apply to
    /// documents.
    #[serde(default)]
    pub formats: Vec<ScrapeFormat>,
    /// Include links found in the document.
    #[serde(default)]
    pub include_links: bool,
    /// Document parsing / OCR options.
    #[serde(default)]
    pub parsers: ParserOptions,
    /// AI enrichment options.
    #[serde(default)]
    pub ai: Option<AiOptions>,
}

/// Multipart body of `POST /parse`.
#[derive(utoipa::ToSchema)]
#[allow(dead_code)]
pub(crate) struct ParseUpload {
    /// The document: PDF, DOC/DOCX, PPT/PPTX, XLS/XLSX/XLSB, ODT/ODS/ODP,
    /// RTF, EPUB or CSV (or a PNG/JPEG/GIF/WebP/TIFF image with
    /// `parsers.ocr`). Max 50 MB by default (`DOCUMENT_MAX_SIZE_MB`).
    #[schema(value_type = String, format = Binary)]
    file: Vec<u8>,
    /// JSON options: `{"formats": ["markdown"], "parsers": {"ocr": "auto"}}`.
    #[schema(value_type = Option<String>)]
    options: Option<String>,
    /// Shorthand for `options.formats`: a JSON array or comma-separated list.
    #[schema(value_type = Option<String>)]
    formats: Option<String>,
}

/// Parse an uploaded document
///
/// Converts an uploaded file to Markdown with the same parsers the crawler
/// uses, and returns the `/scrape` response shape. Billed per document
/// (like `/scrape`), plus OCR pages when `parsers.ocr` recognizes scanned
/// pages.
#[utoipa::path(
    post,
    path = "/parse",
    tag = "parse",
    request_body(content = ParseUpload, content_type = "multipart/form-data"),
    responses(
        (status = 200, body = ScrapeResponse),
        (status = 400, body = ApiError),
        (status = 413, description = "File larger than DOCUMENT_MAX_SIZE_MB")
    ),
    security(("api_key" = []))
)]
pub(crate) async fn parse_upload(
    State(state): State<Arc<AppState>>,
    account_ext: Option<Extension<AuthenticatedAccount>>,
    user_ext: Option<Extension<AuthenticatedUser>>,
    mut multipart: Multipart,
) -> Result<Json<ScrapeResponse>, ApiError> {
    let account_ctx =
        extract_account_context(state.saas_pool.as_ref(), &account_ext, &user_ext).await;
    check_write_permission(&account_ctx)?;
    let start_time = Instant::now();
    let max_bytes = max_document_bytes();

    let mut file: Option<(Vec<u8>, Option<String>, Option<String>)> = None;
    let mut options = ParseRequestOptions::default();
    let mut formats_override: Option<Vec<ScrapeFormat>> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| multipart_error(e.to_string()))?
    {
        match field.name().unwrap_or_default() {
            "file" => {
                let filename = field.file_name().map(str::to_string);
                let content_type = field.content_type().map(str::to_string);
                let mut field = field;
                let mut data = Vec::new();
                while let Some(chunk) = field
                    .chunk()
                    .await
                    .map_err(|e| multipart_error(e.to_string()))?
                {
                    if (data.len() + chunk.len()) as u64 > max_bytes {
                        return Err(ApiError::new(
                            format!("File too large (max {} MB)", max_bytes / (1024 * 1024)),
                            "file_too_large",
                        ));
                    }
                    data.extend_from_slice(&chunk);
                }
                file = Some((data, filename, content_type));
            }
            "options" => {
                let text = field
                    .text()
                    .await
                    .map_err(|e| multipart_error(e.to_string()))?;
                if !text.trim().is_empty() {
                    options = serde_json::from_str(&text).map_err(|e| {
                        ApiError::new(format!("Invalid options JSON: {e}"), "validation_error")
                    })?;
                }
            }
            "formats" => {
                let text = field
                    .text()
                    .await
                    .map_err(|e| multipart_error(e.to_string()))?;
                formats_override = Some(parse_formats(&text)?);
            }
            _ => {}
        }
    }
    if let Some(formats) = formats_override {
        options.formats = formats;
    }

    let (bytes, filename, part_content_type) = file.ok_or_else(|| {
        ApiError::new(
            "Missing multipart field \"file\" (multipart/form-data: file=@document.pdf)",
            "validation_error",
        )
    })?;
    if bytes.is_empty() {
        return Err(ApiError::new("Uploaded file is empty", "validation_error"));
    }
    // Browsers and curl often send `application/octet-stream`; format
    // detection falls back to the bytes. The filename is never trusted.
    let content_type = part_content_type.filter(|ct| !ct.trim().is_empty());

    let has_ai_summary = options.ai.as_ref().is_some_and(|ai| ai.summary);
    let has_ai_extraction = options.ai.as_ref().is_some_and(|ai| ai.extract.is_some());
    let base_cost = billing::scrape_credits(&options.formats, has_ai_summary, has_ai_extraction);
    if let (Some(pool), Some(ctx)) = (&state.saas_pool, &account_ctx) {
        billing::check_credits(pool, &ctx.account_id, base_cost).await?;
    }

    let label = format!("upload://{}", filename.as_deref().unwrap_or("document"));
    let fallback_title = filename.as_deref().and_then(|name| {
        let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
        let title = stem.replace(['-', '_'], " ").trim().to_string();
        (!title.is_empty()).then_some(title)
    });

    let response = document_response(
        &state,
        &account_ctx,
        DocumentJob {
            operation: "parse",
            label,
            base_url: None,
            fallback_title,
            bytes,
            content_type,
            formats: options.formats.clone(),
            include_links: options.include_links,
            parsers: options.parsers.clone(),
            ai: options.ai.as_ref(),
            status_code: 200,
            js_rendered: false,
            base_cost,
        },
        start_time,
    )
    .await?;
    Ok(Json(response))
}

fn multipart_error(detail: String) -> ApiError {
    ApiError::new(
        format!("Invalid multipart body: {detail}"),
        "validation_error",
    )
}

/// `["markdown","links"]` or `markdown,links`.
fn parse_formats(text: &str) -> Result<Vec<ScrapeFormat>, ApiError> {
    let text = text.trim();
    let invalid = |e: String| ApiError::new(format!("Invalid formats: {e}"), "validation_error");
    if text.starts_with('[') {
        return serde_json::from_str(text).map_err(|e| invalid(e.to_string()));
    }
    text.split(',')
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .map(|f| {
            serde_json::from_value(serde_json::Value::String(f.to_ascii_lowercase()))
                .map_err(|e| invalid(format!("{f}: {e}")))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_accept_json_and_csv() {
        assert_eq!(
            parse_formats(r#"["markdown","links"]"#).unwrap(),
            vec![ScrapeFormat::Markdown, ScrapeFormat::Links]
        );
        assert_eq!(
            parse_formats("Markdown, content").unwrap(),
            vec![ScrapeFormat::Markdown, ScrapeFormat::Content]
        );
        assert!(parse_formats("nope").is_err());
    }

    #[test]
    fn parser_options_default_to_no_ocr() {
        let o: ParserOptions = serde_json::from_str("{}").unwrap();
        assert_eq!(o.ocr, OcrMode::Off);
        let o: ParserOptions = serde_json::from_str(r#"{"ocr":"auto","ocr_max_pages":3}"#).unwrap();
        assert_eq!(o.ocr, OcrMode::Auto);
        assert_eq!(o.ocr_max_pages, Some(3));
    }

    // -----------------------------------------------------------------
    // Router-level tests: POST /parse and /scrape of a document URL
    // -----------------------------------------------------------------

    use crate::{webhooks, AppConfig};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::post;
    use axum::Router;
    use scrapix_crawler::{HttpFetcherBuilder, RobotsCache, RobotsConfig};
    use scrapix_queue::{AnyProducer, ChannelBus};
    use std::time::Duration;
    use tower::ServiceExt;

    fn fixture(name: &str) -> Vec<u8> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../crates/scrapix-parser/tests/fixtures")
            .join(name);
        std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    }

    fn state(ocr: Option<Arc<scrapix_ocr::OcrEngine>>) -> Arc<AppState> {
        let bus = ChannelBus::new();
        let robots = Arc::new(
            RobotsCache::new(RobotsConfig {
                respect_robots: false,
                ..Default::default()
            })
            .unwrap(),
        );
        let fetcher = Arc::new(
            HttpFetcherBuilder::new()
                .allow_private_ips(true)
                .max_retries(0)
                .build(robots)
                .unwrap(),
        );
        let mut state = AppState::new(
            AnyProducer::channel(bus.producer()),
            AppConfig {
                max_jobs: 10,
                job_stall_timeout: Duration::from_secs(1800),
                completion_grace: Duration::from_secs(3),
                resume_heal_after: Duration::from_secs(60),
                max_pending_acks: 1000,
            },
            None,
            None,
            None,
            None,
            fetcher,
            None,
            None,
            None,
            None,
            None,
            webhooks::WebhookDispatcher::new(
                scrapix_crawler::safe_client_builder(None, true)
                    .build()
                    .unwrap(),
                webhooks::DEFAULT_MAX_CONCURRENT_DELIVERIES,
            ),
        );
        state.ocr = ocr;
        Arc::new(state)
    }

    const BOUNDARY: &str = "scrapix-test-boundary";

    /// `(field name, filename, content type, data)`.
    type Part<'a> = (&'a str, Option<&'a str>, Option<&'a str>, &'a [u8]);

    fn multipart(parts: &[Part<'_>]) -> Vec<u8> {
        let mut body = Vec::new();
        for (name, filename, content_type, data) in parts {
            body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
            let disposition = match filename {
                Some(f) => format!("form-data; name=\"{name}\"; filename=\"{f}\""),
                None => format!("form-data; name=\"{name}\""),
            };
            body.extend_from_slice(format!("Content-Disposition: {disposition}\r\n").as_bytes());
            if let Some(ct) = content_type {
                body.extend_from_slice(format!("Content-Type: {ct}\r\n").as_bytes());
            }
            body.extend_from_slice(b"\r\n");
            body.extend_from_slice(data);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
        body
    }

    async fn post_parse(
        state: Arc<AppState>,
        parts: &[Part<'_>],
    ) -> (StatusCode, serde_json::Value) {
        let app = Router::new()
            .route("/parse", post(parse_upload))
            .layer(axum::extract::DefaultBodyLimit::max(60 * 1024 * 1024))
            .with_state(state);
        let response = app
            .oneshot(
                Request::post("/parse")
                    .header(
                        "content-type",
                        format!("multipart/form-data; boundary={BOUNDARY}"),
                    )
                    .body(Body::from(multipart(parts)))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn parse_converts_office_formats_to_markdown() {
        for (file, needle) in [
            ("report.docx", "| North | 120 | 135 |"),
            ("report.xlsx", "| West | 143 | 150 |"),
            ("report.pptx", "Quarterly Report"),
            ("report.epub", "Revenue grew"),
        ] {
            let data = fixture(file);
            let (status, body) = post_parse(
                state(None),
                &[("file", Some(file), Some("application/octet-stream"), &data)],
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{file}: {body}");
            assert_eq!(body["success"], true);
            assert_eq!(body["url"], format!("upload://{file}"));
            let md = body["markdown"].as_str().unwrap();
            assert!(md.contains(needle), "{file}: {md}");
            assert!(body["content"].as_str().is_some_and(|c| !c.is_empty()));
            assert_eq!(body["document"]["parser"], "anydoc");
            assert_eq!(body["document"]["needs_ocr"], false);
        }
    }

    #[tokio::test]
    async fn parse_pdf_keeps_tables_and_metadata() {
        let data = fixture("text-table.pdf");
        let options = br#"{"formats":["markdown","metadata","links"]}"#;
        let (status, body) = post_parse(
            state(None),
            &[
                ("file", Some("r.pdf"), Some("application/pdf"), &data),
                ("options", None, None, options),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body["markdown"]
            .as_str()
            .unwrap()
            .contains("|North|120|135|"));
        assert!(body.get("content").is_none(), "only requested formats");
        assert_eq!(body["metadata"]["title"], "Quarterly Report");
        assert_eq!(body["links"][0], "https://example.com/methodology");
        assert_eq!(body["document"]["format"], "pdf");
        assert_eq!(body["document"]["pdf_type"], "text_based");
        assert_eq!(body["document"]["has_tables"], true);
        assert_eq!(body["language"], "en");
    }

    #[tokio::test]
    async fn parse_flags_scanned_pdf_when_ocr_is_off() {
        let data = fixture("scanned.pdf");
        let (status, body) =
            post_parse(state(None), &[("file", Some("scan.pdf"), None, &data)]).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["document"]["needs_ocr"], true);
        assert_eq!(body["document"]["pages_needing_ocr"][0], 1);
        assert_eq!(body["markdown"], "");
        assert!(body["warning"].as_str().unwrap().contains("parsers.ocr"));
        assert!(body.get("ocr").is_none());
    }

    struct FakeRaster;
    impl scrapix_ocr::PageRasterizer for FakeRaster {
        fn name(&self) -> &str {
            "fake"
        }
        fn render_png(
            &self,
            _: &[u8],
            pages: &[u32],
            _: f32,
        ) -> Result<Vec<Vec<u8>>, scrapix_ocr::OcrError> {
            Ok(pages
                .iter()
                .map(|p| format!("png-{p}").into_bytes())
                .collect())
        }
    }

    struct FakeOcr;
    #[async_trait::async_trait]
    impl scrapix_ocr::OcrBackend for FakeOcr {
        fn name(&self) -> &str {
            "fake-ocr"
        }
        async fn recognize(&self, _: &[u8], _: &str) -> Result<String, scrapix_ocr::OcrError> {
            Ok("# SCANNED INVOICE\n\nTotal due 1250 EUR".to_string())
        }
    }

    fn fake_ocr() -> Arc<scrapix_ocr::OcrEngine> {
        Arc::new(scrapix_ocr::OcrEngine::new(
            Arc::new(FakeRaster),
            Arc::new(FakeOcr),
            Arc::new(scrapix_ocr::MemoryOcrCache::new(10)),
            Arc::new(scrapix_ocr::MemoryOcrBudget::new(0)),
            scrapix_ocr::OcrSettings::default(),
        ))
    }

    #[tokio::test]
    async fn parse_ocr_auto_recognizes_scanned_pages() {
        let data = fixture("mixed.pdf");
        let (status, body) = post_parse(
            state(Some(fake_ocr())),
            &[
                ("file", Some("mixed.pdf"), None, &data),
                ("options", None, None, br#"{"parsers":{"ocr":"auto"}}"#),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let md = body["markdown"].as_str().unwrap();
        assert!(
            md.contains("Quarterly Report") && md.contains("Total due 1250 EUR"),
            "{md}"
        );
        assert_eq!(body["ocr"]["mode"], "auto");
        assert_eq!(body["ocr"]["pages_processed"], 1);
        assert_eq!(body["ocr"]["pages_skipped"], 1);
        assert_eq!(body["ocr"]["pages"][0], 2);
        assert_eq!(body["ocr"]["backend"], "fake-ocr");
        assert_eq!(body["document"]["needs_ocr"], false);
    }

    #[tokio::test]
    async fn parse_images_need_ocr() {
        let data = fixture("scan.jpg");
        let (status, body) = post_parse(
            state(Some(fake_ocr())),
            &[("file", Some("scan.jpg"), Some("image/jpeg"), &data)],
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["code"], "ocr_required");

        let (status, body) = post_parse(
            state(Some(fake_ocr())),
            &[
                ("file", Some("scan.jpg"), Some("image/jpeg"), &data),
                ("formats", None, None, b"markdown"),
                ("options", None, None, br#"{"parsers":{"ocr":"auto"}}"#),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body["markdown"]
            .as_str()
            .unwrap()
            .contains("SCANNED INVOICE"));
        assert_eq!(body["document"]["format"], "image");
    }

    #[tokio::test]
    async fn parse_rejects_bad_uploads() {
        let (status, body) = post_parse(state(None), &[("options", None, None, b"{}")]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

        let (status, body) = post_parse(
            state(None),
            &[("file", Some("notes.bin"), None, b"just some bytes")],
        )
        .await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{body}");
        assert_eq!(body["code"], "unsupported_document");
    }

    #[tokio::test]
    async fn scrape_of_a_document_url_parses_it() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/files/report.docx"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                fixture("report.docx"),
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            ))
            .mount(&server)
            .await;
        let url = format!(
            "{}/files/report.docx",
            server.uri().replace("127.0.0.1", "localhost")
        );
        let request: crate::ScrapeRequest =
            serde_json::from_value(serde_json::json!({ "url": url })).unwrap();
        let Json(response) = crate::scrape_url(State(state(None)), None, None, Json(request))
            .await
            .unwrap_or_else(|e| panic!("{}", e.error));
        let body = serde_json::to_value(&response).unwrap();
        assert_eq!(body["success"], true, "{body}");
        assert!(body["markdown"]
            .as_str()
            .unwrap()
            .contains("# Quarterly Report"));
        assert_eq!(body["document"]["format"], "docx");
        assert_eq!(body["metadata"]["title"], "Quarterly Report");
    }

    // -----------------------------------------------------------------
    // Usage events
    // -----------------------------------------------------------------

    fn usage_ctx() -> AccountContext {
        AccountContext {
            account_id: "7f1c2a8e-0000-4000-8000-000000000001".into(),
            api_key_id: Some("k".into()),
            tier: "free".into(),
            user_role: None,
        }
    }

    #[tokio::test]
    async fn document_usage_is_one_event_without_ocr() {
        let bus = ChannelBus::new();
        let (state, outbox) = crate::results::test_support::test_state_with_lab(&bus);
        record_document_usage(&state, &usage_ctx(), "parse", "upload://a.pdf", 4, 0).await;
        let events = outbox.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "usage.recorded");
        assert_eq!(events[0].api_key_id.as_deref(), Some("k"));
        assert_eq!(
            events[0].data,
            serde_json::json!({
                "operation": "parse",
                "credits": 4,
                "units": {},
                "description": "upload://a.pdf (4 credits)",
            })
        );
    }

    #[tokio::test]
    async fn document_usage_adds_an_ocr_event_for_billable_pages() {
        let bus = ChannelBus::new();
        let (state, outbox) = crate::results::test_support::test_state_with_lab(&bus);
        record_document_usage(&state, &usage_ctx(), "scrape", "https://e.com/s.pdf", 3, 2).await;
        let events = outbox.events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data["operation"], "scrape");
        assert_eq!(events[0].data["credits"], 3);
        let ocr_cost = scrapix_billing::ocr_credits(2);
        assert_eq!(
            events[1].data,
            serde_json::json!({
                "operation": "ocr",
                "credits": ocr_cost,
                "units": {"pages_ocr": 2},
                "description": format!("https://e.com/s.pdf (2 OCR pages, {ocr_cost} credits)"),
            })
        );
        assert_ne!(events[0].id, events[1].id);
    }

    #[tokio::test]
    async fn document_response_records_usage_for_the_caller() {
        let bus = ChannelBus::new();
        let (state, outbox) = crate::results::test_support::test_state_with_lab(&bus);
        let job = DocumentJob {
            operation: "parse",
            label: "upload://t.csv".into(),
            base_url: None,
            fallback_title: None,
            bytes: b"a,b\n1,2\n".to_vec(),
            content_type: Some("text/csv".into()),
            formats: vec![],
            include_links: false,
            parsers: serde_json::from_str("{}").unwrap(),
            ai: None,
            status_code: 200,
            js_rendered: false,
            base_cost: 1,
        };
        document_response(&state, &Some(usage_ctx()), job, Instant::now())
            .await
            .unwrap_or_else(|e| panic!("{}", e.error));
        let events = outbox.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data["operation"], "parse");
        assert_eq!(events[0].data["credits"], 1);
    }
}
