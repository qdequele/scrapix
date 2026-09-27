//! PDF parsing via [pdf-inspector]: classification + layout-aware Markdown.
//!
//! Entry point for the opt-in PDF feature (`features.pdf.enabled`) and for
//! PDFs uploaded to `POST /parse`. pdf-inspector is pure Rust (built on
//! `lopdf`, no ML models) and gives us, beyond plain text:
//!
//! - Markdown with headings, lists, **tables** (rectangle- and
//!   heuristic-based, with multi-page continuation) and multi-column
//!   reading order, RTL and CID/ToUnicode font decoding;
//! - a document classification (text-based / scanned / image-based /
//!   mixed) with **per-page OCR recommendations** and reason codes. A
//!   scanned document is therefore reported as needing OCR instead of
//!   silently yielding empty text.
//!
//! PDFs go to pdf-inspector directly rather than through anydoc: anydoc
//! refuses any document with a scanned page (`ConvertError::NeedsOcr`),
//! while the crawl path and the OCR pipeline (SCR-86) need the text-based
//! pages plus the list of pages to OCR. anydoc uses pdf-inspector for its
//! own PDF path, so this is the same engine either way.
//!
//! [pdf-inspector]: https://github.com/firecrawl/pdf-inspector

use std::collections::HashSet;

use pdf_inspector::{DetectionConfig, PdfError, PdfOptions, PdfType, ScanStrategy};
use scrapix_core::{Result, ScrapixError};
use tracing::{debug, warn};
use url::Url;

use crate::document::{DocumentKind, DocumentParser, ParseOptions, ParsedDocument};
use crate::language::detect_language;

/// The pdf-inspector backend of the document dispatch.
#[derive(Debug, Default, Clone, Copy)]
pub struct PdfInspectorParser;

impl DocumentParser for PdfInspectorParser {
    fn name(&self) -> &'static str {
        "pdf-inspector"
    }

    fn supports(&self, kind: DocumentKind) -> bool {
        kind == DocumentKind::Pdf
    }

    fn parse(
        &self,
        bytes: &[u8],
        kind: DocumentKind,
        opts: &ParseOptions,
    ) -> Result<ParsedDocument> {
        debug_assert_eq!(kind, DocumentKind::Pdf);
        parse_pdf(bytes, opts)
    }
}

/// Parse raw PDF bytes into Markdown, metadata and an OCR assessment.
///
/// Returns `ScrapixError::Parse` for empty, unparseable or encrypted PDFs.
/// A scanned PDF is *not* an error: it comes back with empty (or partial)
/// Markdown and `pages_needing_ocr` listing the pages to OCR.
pub fn parse_pdf(bytes: &[u8], opts: &ParseOptions) -> Result<ParsedDocument> {
    if bytes.is_empty() {
        return Err(ScrapixError::Parse("Empty PDF body".to_string()));
    }

    let mut options = PdfOptions::new().detection(DetectionConfig {
        // Every page is classified, so the OCR recommendation is per page
        // rather than extrapolated from a sample.
        strategy: ScanStrategy::Full,
        ..DetectionConfig::default()
    });
    if let Some(max) = opts.max_pages.filter(|m| *m > 0) {
        options = options.pages(1..=max);
    }

    let result = pdf_inspector::process_pdf_mem_with_options(bytes, options).map_err(map_error)?;
    let page_count = result.page_count;
    let pages_processed = opts
        .max_pages
        .filter(|m| *m > 0)
        .map_or(page_count, |m| m.min(page_count));

    // Detection samples content streams and over-reports short or
    // image-heavy text pages; extraction knows which of them actually
    // yielded no text (the same refinement anydoc applies).
    let mut pages_needing_ocr: Vec<u32> = result
        .pages_needing_ocr
        .iter()
        .copied()
        .filter(|p| *p <= pages_processed)
        .collect();
    if !pages_needing_ocr.is_empty() {
        let flagged: Vec<u32> = pages_needing_ocr.iter().map(|p| p - 1).collect();
        match pdf_inspector::extract_pages_markdown_mem(bytes, Some(&flagged)) {
            Ok(extraction) => {
                let confirmed: HashSet<u32> = extraction
                    .pages
                    .iter()
                    .filter(|p| p.needs_ocr)
                    .map(|p| p.page + 1)
                    .chain(extraction.pages_needing_ocr.iter().copied())
                    .collect();
                pages_needing_ocr.retain(|p| confirmed.contains(p));
            }
            Err(e) => debug!(error = %e, "PDF per-page refinement failed; keeping detector pages"),
        }
    }
    let ocr_reasons = result
        .ocr_reasons_by_page
        .iter()
        .filter(|r| pages_needing_ocr.contains(&r.page))
        .map(|r| (r.page, r.reasons.clone()))
        .collect();

    let markdown = result
        .markdown
        .map(|m| m.trim().to_string())
        .unwrap_or_default();

    if markdown.is_empty() {
        warn!(
            bytes = bytes.len(),
            pages = page_count,
            pdf_type = ?result.pdf_type,
            "PDF yielded no extractable text; flagged as needing OCR"
        );
    } else if result.has_encoding_issues {
        warn!(
            "PDF has broken font encodings; extracted text may be garbled (ocr=force re-reads it)"
        );
    }

    let text_for_language = crate::markdown_to_text(&markdown);
    let language = if text_for_language.trim().is_empty() {
        None
    } else {
        detect_language(&text_for_language)
    };

    Ok(ParsedDocument {
        kind: DocumentKind::Pdf,
        parser: "pdf-inspector",
        markdown,
        title: non_empty(result.title),
        author: non_empty(result.author),
        subject: non_empty(result.subject),
        keywords: non_empty(result.keywords),
        language,
        page_count: Some(page_count),
        pages_processed: Some(pages_processed),
        pdf_type: Some(pdf_type_str(result.pdf_type)),
        pages_needing_ocr,
        ocr_reasons,
        has_encoding_issues: result.has_encoding_issues,
        has_tables: !result.layout.pages_with_tables.is_empty(),
    })
}

/// Per-page Markdown (0-indexed position = page − 1), for merging OCR'd
/// pages back into a document in page order. Pages beyond `max_pages` are
/// not extracted.
pub fn page_markdown(bytes: &[u8], max_pages: Option<u32>) -> Result<Vec<String>> {
    let pages: Option<Vec<u32>> = max_pages.filter(|m| *m > 0).map(|m| (0..m).collect());
    let extraction = match pdf_inspector::extract_pages_markdown_mem(bytes, pages.as_deref()) {
        Ok(extraction) => extraction,
        // A `max_pages` beyond the document length: retry with every page.
        Err(_) if pages.is_some() => {
            pdf_inspector::extract_pages_markdown_mem(bytes, None).map_err(map_error)?
        }
        Err(e) => return Err(map_error(e)),
    };
    let mut out: Vec<(u32, String)> = extraction
        .pages
        .into_iter()
        .map(|p| (p.page, p.markdown.trim().to_string()))
        .collect();
    out.sort_by_key(|(page, _)| *page);
    Ok(out.into_iter().map(|(_, md)| md).collect())
}

/// Absolute `http(s)` URLs from a PDF's link annotations and URLs its text
/// layer spells out, deduplicated in document order, resolved against
/// `base_url`, fragments dropped. Powers `features.pdf.extract_links`.
pub fn extract_links(bytes: &[u8], base_url: &str) -> Result<Vec<String>> {
    static SPELLED: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let spelled = SPELLED.get_or_init(|| {
        regex::Regex::new(r#"https?://[^\s<>"'()\[\]{}]+"#).expect("valid URL regex")
    });

    let items = pdf_inspector::extract_text_with_positions_mem(bytes).map_err(map_error)?;
    let base = Url::parse(base_url).ok();
    let mut seen = HashSet::new();
    let mut links = Vec::new();
    let mut push = |target: &str| {
        let target = target.trim().trim_end_matches(['.', ',', ';', ':']);
        let resolved = match &base {
            Some(base) => base.join(target).ok(),
            None => Url::parse(target).ok(),
        };
        let Some(mut url) = resolved else { return };
        if !matches!(url.scheme(), "http" | "https") {
            return;
        }
        url.set_fragment(None);
        let url = url.to_string();
        if seen.insert(url.clone()) {
            links.push(url);
        }
    };
    for item in &items {
        match &item.item_type {
            pdf_inspector::types::ItemType::Link(target) => push(target),
            pdf_inspector::types::ItemType::Text => {
                for m in spelled.find_iter(&item.text) {
                    push(m.as_str());
                }
            }
            _ => {}
        }
    }
    Ok(links)
}

/// Derive a title from a document URL's basename (`/files/q3-report.pdf` →
/// `q3 report`). Returns `None` if no meaningful name can be extracted.
pub fn title_from_url(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    let last = parsed.path_segments()?.rfind(|s| !s.is_empty())?;
    let last = percent_decode(last);
    let title = match last.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && (1..=5).contains(&ext.len()) => stem,
        _ => last.as_str(),
    };
    let title = title.replace(['-', '_'], " ");
    let title = title.trim();
    if title.is_empty() {
        None
    } else {
        Some(title.to_string())
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn pdf_type_str(t: PdfType) -> &'static str {
    match t {
        PdfType::TextBased => "text_based",
        PdfType::Scanned => "scanned",
        PdfType::ImageBased => "image_based",
        PdfType::Mixed => "mixed",
    }
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn map_error(e: PdfError) -> ScrapixError {
    match e {
        PdfError::Encrypted => ScrapixError::Parse("PDF is encrypted".to_string()),
        PdfError::NotAPdf(detail) => ScrapixError::Parse(format!("Not a PDF: {detail}")),
        PdfError::InvalidStructure => ScrapixError::Parse("Invalid PDF structure".to_string()),
        PdfError::Parse(detail) => ScrapixError::Parse(format!("PDF parsing error: {detail}")),
        PdfError::Io(e) => ScrapixError::Parse(format!("PDF read error: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_title_from_url_basic() {
        assert_eq!(
            title_from_url("https://example.com/spec-v2.pdf"),
            Some("spec v2".to_string())
        );
    }

    #[test]
    fn test_title_from_url_other_extensions_and_encoding() {
        assert_eq!(
            title_from_url("https://example.com/files/Q3%20report.docx"),
            Some("Q3 report".to_string())
        );
    }

    #[test]
    fn test_title_from_url_no_extension() {
        assert_eq!(
            title_from_url("https://example.com/files/report"),
            Some("report".to_string())
        );
    }

    #[test]
    fn test_title_from_url_empty_path() {
        assert_eq!(title_from_url("https://example.com/"), None);
    }

    #[test]
    fn test_title_from_url_invalid() {
        assert_eq!(title_from_url("not a url"), None);
    }

    #[test]
    fn test_parse_pdf_empty_input() {
        assert!(parse_pdf(&[], &ParseOptions::default()).is_err());
    }

    #[test]
    fn test_parse_pdf_invalid_input() {
        assert!(parse_pdf(b"not a pdf", &ParseOptions::default()).is_err());
    }
}
