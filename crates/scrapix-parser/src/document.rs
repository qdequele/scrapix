//! Binary document parsing: one format → parser dispatch.
//!
//! Every non-HTML document goes through here, whether it was crawled (the
//! content worker), fetched by `POST /scrape`, or uploaded to
//! `POST /parse`:
//!
//! 1. [`detect_kind`] decides the format from the `Content-Type` header
//!    first and the body's magic bytes as a fallback. The URL extension is
//!    never trusted.
//! 2. [`DocumentDispatch`] hands the bytes to the first registered
//!    [`DocumentParser`] that supports that format: [`PdfInspectorParser`]
//!    for PDFs, [`AnydocParser`] for office/other formats, [`ImageParser`]
//!    for raster images (which only OCR can read).
//! 3. [`build_document`] turns the [`ParsedDocument`] into an indexable
//!    [`Document`].
//!
//! The parser trait is deliberately small so a backend can be swapped (both
//! crates are young) without touching any call site.
//!
//! [`PdfInspectorParser`]: crate::pdf::PdfInspectorParser
//! [`AnydocParser`]: crate::office::AnydocParser

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use scrapix_core::{content_types, Document, FeaturesConfig, Result, ScrapixError};
use url::Url;

use crate::office::{kind_from_anydoc, AnydocParser};
use crate::pdf::PdfInspectorParser;

/// A binary document format Scrapix can parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DocumentKind {
    Pdf,
    /// Binary Word 97-2003.
    Doc,
    /// WordprocessingML (`.docx`, `.docm`).
    Docx,
    /// Binary PowerPoint 97-2003 (`.ppt`, `.pps`, `.pot`).
    Ppt,
    /// PresentationML (`.pptx`, `.pptm`, `.ppsx`, `.ppsm`).
    Pptx,
    /// Excel workbooks: `.xlsx`, `.xlsm`, `.xlsb`, legacy `.xls`.
    Excel,
    Odt,
    Ods,
    Odp,
    Rtf,
    Epub,
    Csv,
    /// A raster image (PNG, JPEG, GIF, WebP, TIFF): no text without OCR.
    Image,
}

impl DocumentKind {
    /// Short stable name (`pdf`, `docx`, `xlsx`, ...), used in document
    /// metadata and API responses.
    pub fn as_str(self) -> &'static str {
        match self {
            DocumentKind::Pdf => "pdf",
            DocumentKind::Doc => "doc",
            DocumentKind::Docx => "docx",
            DocumentKind::Ppt => "ppt",
            DocumentKind::Pptx => "pptx",
            DocumentKind::Excel => "xlsx",
            DocumentKind::Odt => "odt",
            DocumentKind::Ods => "ods",
            DocumentKind::Odp => "odp",
            DocumentKind::Rtf => "rtf",
            DocumentKind::Epub => "epub",
            DocumentKind::Csv => "csv",
            DocumentKind::Image => "image",
        }
    }

    /// Canonical media type, stamped as `metadata.content_type` so indexed
    /// documents can be filtered by format in Meilisearch.
    pub fn mime_type(self) -> &'static str {
        match self {
            DocumentKind::Pdf => "application/pdf",
            DocumentKind::Doc => "application/msword",
            DocumentKind::Docx => {
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            }
            DocumentKind::Ppt => "application/vnd.ms-powerpoint",
            DocumentKind::Pptx => {
                "application/vnd.openxmlformats-officedocument.presentationml.presentation"
            }
            DocumentKind::Excel => {
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
            }
            DocumentKind::Odt => "application/vnd.oasis.opendocument.text",
            DocumentKind::Ods => "application/vnd.oasis.opendocument.spreadsheet",
            DocumentKind::Odp => "application/vnd.oasis.opendocument.presentation",
            DocumentKind::Rtf => "application/rtf",
            DocumentKind::Epub => "application/epub+zip",
            DocumentKind::Csv => "text/csv",
            DocumentKind::Image => "image/*",
        }
    }

    /// The kind a `Content-Type` header names, if it names one precisely.
    /// Generic download types (`application/octet-stream`) return `None`.
    pub fn from_content_type(content_type: &str) -> Option<Self> {
        let mt = content_types::media_type(content_type);
        let mt = mt.as_str();
        if content_types::is_pdf(mt) {
            return Some(DocumentKind::Pdf);
        }
        if content_types::is_image(mt) {
            return Some(DocumentKind::Image);
        }
        Some(if mt.contains("wordprocessingml") {
            DocumentKind::Docx
        } else if mt.contains("presentationml") {
            DocumentKind::Pptx
        } else if mt.contains("spreadsheetml") || mt.starts_with("application/vnd.ms-excel") {
            DocumentKind::Excel
        } else if mt.starts_with("application/vnd.ms-powerpoint") {
            DocumentKind::Ppt
        } else if mt == "application/msword" || mt.starts_with("application/vnd.ms-word") {
            DocumentKind::Doc
        } else if mt.starts_with("application/vnd.oasis.opendocument.text") {
            DocumentKind::Odt
        } else if mt.starts_with("application/vnd.oasis.opendocument.spreadsheet") {
            DocumentKind::Ods
        } else if mt.starts_with("application/vnd.oasis.opendocument.presentation") {
            DocumentKind::Odp
        } else if matches!(mt, "application/rtf" | "application/x-rtf" | "text/rtf") {
            DocumentKind::Rtf
        } else if mt == "application/epub+zip" {
            DocumentKind::Epub
        } else if matches!(mt, "text/csv" | "application/csv") {
            DocumentKind::Csv
        } else {
            return None;
        })
    }

    /// Whether a crawl job's features allow indexing this kind: PDFs need
    /// `features.pdf`, office/other formats need `features.documents`.
    /// Images are never crawled (only uploaded to `/parse`).
    pub fn allowed_by(self, features: &FeaturesConfig) -> bool {
        match self {
            DocumentKind::Pdf => features.is_pdf_enabled(),
            DocumentKind::Image => false,
            _ => features.is_documents_enabled(),
        }
    }
}

/// Identify a format from the body alone: anydoc's container detection
/// (PDF header, RTF group, OLE stream names, ZIP package identity) plus
/// raster image signatures. CSV has no signature and is never sniffed.
pub fn sniff_kind(bytes: &[u8]) -> Option<DocumentKind> {
    if is_image_signature(bytes) {
        return Some(DocumentKind::Image);
    }
    anydoc::Format::from_bytes(bytes).map(kind_from_anydoc)
}

fn is_image_signature(b: &[u8]) -> bool {
    b.starts_with(b"\x89PNG\r\n\x1a\n")
        || b.starts_with(b"\xFF\xD8\xFF")
        || b.starts_with(b"GIF87a")
        || b.starts_with(b"GIF89a")
        || (b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP")
        || b.starts_with(b"II*\0")
        || b.starts_with(b"MM\0*")
}

/// Decide a document's format: the `Content-Type` header first, the
/// body's magic bytes as a fallback. Never the URL extension.
///
/// When the header names a format but the bytes carry a *different*
/// recognized container signature (a `.docx` served as
/// `application/msword`, a PDF served as `application/vnd.ms-excel`), the
/// bytes win: the declared parser could not read them anyway.
pub fn detect_kind(content_type: Option<&str>, bytes: &[u8]) -> Option<DocumentKind> {
    let declared = content_type.and_then(DocumentKind::from_content_type);
    let sniffed = sniff_kind(bytes);
    match (declared, sniffed) {
        (Some(declared), Some(sniffed)) if declared != sniffed => Some(sniffed),
        (Some(declared), _) => Some(declared),
        (None, sniffed) => sniffed,
    }
}

/// Parser-independent options.
#[derive(Debug, Clone, Default)]
pub struct ParseOptions {
    /// Parse at most this many pages (PDF only; the first N).
    pub max_pages: Option<u32>,
}

/// What a parser produced for one document.
#[derive(Debug, Clone)]
pub struct ParsedDocument {
    pub kind: DocumentKind,
    /// Name of the backend that produced it (`pdf-inspector`, `anydoc`).
    pub parser: &'static str,
    /// GitHub-Flavored Markdown of the whole document (empty for a
    /// document whose pages all need OCR).
    pub markdown: String,
    /// Title from the document's own metadata (PDF Info dictionary) or its
    /// first heading.
    pub title: Option<String>,
    pub author: Option<String>,
    pub subject: Option<String>,
    pub keywords: Option<String>,
    /// Detected language (ISO 639-1) of the extracted text.
    pub language: Option<String>,
    /// Pages in the document (PDF and images), when known.
    pub page_count: Option<u32>,
    /// Pages actually parsed (≤ `page_count` when `max_pages` capped it).
    pub pages_processed: Option<u32>,
    /// PDF classification: `text_based`, `scanned`, `image_based`, `mixed`.
    pub pdf_type: Option<&'static str>,
    /// 1-indexed pages with no usable text layer: OCR would recover them.
    pub pages_needing_ocr: Vec<u32>,
    /// Machine-readable reasons per page in `pages_needing_ocr` (`scanned`,
    /// `no_text`, `vector_text`, `invisible_text_layer`,
    /// `suspected_garbled_text`, `image`).
    pub ocr_reasons: Vec<(u32, Vec<String>)>,
    /// Broken font encodings detected: text may be garbled even on pages
    /// that are not flagged (OCR `force` re-reads them).
    pub has_encoding_issues: bool,
    /// Tables were detected (and rendered as Markdown tables).
    pub has_tables: bool,
}

impl ParsedDocument {
    /// An empty result of the given kind, for backends to fill in.
    pub fn empty(kind: DocumentKind) -> Self {
        Self {
            kind,
            parser: "",
            markdown: String::new(),
            title: None,
            author: None,
            subject: None,
            keywords: None,
            language: None,
            page_count: None,
            pages_processed: None,
            pdf_type: None,
            pages_needing_ocr: Vec::new(),
            ocr_reasons: Vec::new(),
            has_encoding_issues: false,
            has_tables: false,
        }
    }

    /// Whether some pages carry no usable text without OCR.
    pub fn needs_ocr(&self) -> bool {
        !self.pages_needing_ocr.is_empty()
    }

    /// Plain text derived from the Markdown (for `content` and language
    /// detection).
    pub fn text(&self) -> String {
        crate::markdown_to_text(&self.markdown)
    }
}

/// A document parsing backend.
pub trait DocumentParser: Send + Sync {
    /// Stable backend name, reported on the parsed output.
    fn name(&self) -> &'static str;
    /// Whether this backend handles `kind`.
    fn supports(&self, kind: DocumentKind) -> bool;
    /// Parse `bytes`, already identified as `kind`.
    fn parse(
        &self,
        bytes: &[u8],
        kind: DocumentKind,
        opts: &ParseOptions,
    ) -> Result<ParsedDocument>;
}

/// Raster images have no text layer: parsing reports one page needing OCR.
#[derive(Debug, Default, Clone, Copy)]
pub struct ImageParser;

impl DocumentParser for ImageParser {
    fn name(&self) -> &'static str {
        "image"
    }

    fn supports(&self, kind: DocumentKind) -> bool {
        kind == DocumentKind::Image
    }

    fn parse(
        &self,
        bytes: &[u8],
        kind: DocumentKind,
        _opts: &ParseOptions,
    ) -> Result<ParsedDocument> {
        if bytes.is_empty() {
            return Err(ScrapixError::Parse("Empty image body".to_string()));
        }
        Ok(ParsedDocument {
            parser: "image",
            page_count: Some(1),
            pages_processed: Some(1),
            pages_needing_ocr: vec![1],
            ocr_reasons: vec![(1, vec!["image".to_string()])],
            ..ParsedDocument::empty(kind)
        })
    }
}

/// Format → parser dispatch over an ordered list of backends.
#[derive(Clone)]
pub struct DocumentDispatch {
    parsers: Vec<Arc<dyn DocumentParser>>,
}

impl Default for DocumentDispatch {
    /// pdf-inspector for PDFs, anydoc for office/other formats, and the
    /// image placeholder.
    fn default() -> Self {
        Self {
            parsers: vec![
                Arc::new(PdfInspectorParser),
                Arc::new(AnydocParser),
                Arc::new(ImageParser),
            ],
        }
    }
}

impl DocumentDispatch {
    /// An empty dispatch; register backends with [`Self::with_parser`].
    pub fn empty() -> Self {
        Self {
            parsers: Vec::new(),
        }
    }

    /// Register a backend ahead of the existing ones (it wins for every
    /// kind it supports).
    pub fn with_parser(mut self, parser: Arc<dyn DocumentParser>) -> Self {
        self.parsers.insert(0, parser);
        self
    }

    /// Parse `bytes` as `kind` with the first backend that supports it.
    pub fn parse(
        &self,
        bytes: &[u8],
        kind: DocumentKind,
        opts: &ParseOptions,
    ) -> Result<ParsedDocument> {
        let parser = self
            .parsers
            .iter()
            .find(|p| p.supports(kind))
            .ok_or_else(|| {
                ScrapixError::Parse(format!("No parser for {} documents", kind.as_str()))
            })?;
        parser.parse(bytes, kind, opts)
    }

    /// Detect the format ([`detect_kind`]) and parse.
    pub fn detect_and_parse(
        &self,
        bytes: &[u8],
        content_type: Option<&str>,
        opts: &ParseOptions,
    ) -> Result<ParsedDocument> {
        let kind = detect_kind(content_type, bytes).ok_or_else(|| {
            ScrapixError::Parse(format!(
                "Unrecognized document format (content type {})",
                content_type.unwrap_or("none")
            ))
        })?;
        self.parse(bytes, kind, opts)
    }
}

/// The process-wide default dispatch.
pub fn default_dispatch() -> &'static DocumentDispatch {
    static DISPATCH: OnceLock<DocumentDispatch> = OnceLock::new();
    DISPATCH.get_or_init(DocumentDispatch::default)
}

/// Detect the format and parse with the default backends.
pub fn parse_document(
    bytes: &[u8],
    content_type: Option<&str>,
    opts: &ParseOptions,
) -> Result<ParsedDocument> {
    default_dispatch().detect_and_parse(bytes, content_type, opts)
}

/// Build a ready-to-index `Document` from a parsed document.
///
/// - `markdown` holds the parser's Markdown, `content` its plain text.
/// - `metadata.content_type` is the format's canonical media type (so
///   `metadata.content_type = "application/pdf"` filters keep working),
///   `metadata.document_format` its short name (`pdf`, `docx`, ...).
/// - `metadata.needs_ocr` / `metadata.pages_needing_ocr` flag scanned
///   pages that were not OCR'd, instead of silently indexing blank text.
/// - URL tags follow the same convention as HTML/markdown pages.
///
/// `fallback_title` (typically the URL basename) is used when the document
/// carries no title of its own.
pub fn build_document(
    url: &str,
    bytes_len: usize,
    parsed: &ParsedDocument,
    fallback_title: Option<String>,
) -> Result<Document> {
    let parsed_url = Url::parse(url)?;
    let domain = parsed_url
        .host_str()
        .ok_or_else(|| ScrapixError::Parse("URL has no host".to_string()))?;

    let mut doc = Document::new(url, domain);
    doc.title = parsed.title.clone().or(fallback_title);

    if !parsed.markdown.is_empty() {
        let text = parsed.text();
        if !text.is_empty() {
            doc.content = Some(text);
        }
        doc.markdown = Some(parsed.markdown.clone());
    }
    doc.language = parsed.language.clone();

    let mut tags = Vec::new();
    let mut current = String::new();
    for segment in parsed_url.path().split('/').filter(|s| !s.is_empty()) {
        if !current.is_empty() {
            current.push('/');
        }
        current.push_str(segment);
        tags.push(format!("/{}", current));
    }
    doc.urls_tags = Some(tags);

    doc.metadata = Some(document_metadata(parsed, bytes_len));
    Ok(doc)
}

/// The metadata map stamped on an indexed document (see [`build_document`]).
pub fn document_metadata(parsed: &ParsedDocument, bytes_len: usize) -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert(
        "content_type".to_string(),
        parsed.kind.mime_type().to_string(),
    );
    m.insert(
        "document_format".to_string(),
        parsed.kind.as_str().to_string(),
    );
    m.insert("document_bytes".to_string(), bytes_len.to_string());
    if parsed.kind == DocumentKind::Pdf {
        // Kept for indexes built before the document dispatch existed.
        m.insert("pdf_bytes".to_string(), bytes_len.to_string());
    }
    if let Some(pages) = parsed.page_count {
        m.insert("page_count".to_string(), pages.to_string());
    }
    if let Some(pdf_type) = parsed.pdf_type {
        m.insert("pdf_type".to_string(), pdf_type.to_string());
    }
    if let Some(ref author) = parsed.author {
        m.insert("author".to_string(), author.clone());
    }
    m.insert("needs_ocr".to_string(), parsed.needs_ocr().to_string());
    if parsed.needs_ocr() {
        m.insert(
            "pages_needing_ocr".to_string(),
            parsed
                .pages_needing_ocr
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(","),
        );
    }
    m
}

/// `http(s)` links from Markdown `[text](url)` / `<url>` constructs,
/// resolved against `base_url`, deduplicated in order.
pub fn markdown_links(markdown: &str, base_url: &str) -> Vec<String> {
    static LINK: OnceLock<regex::Regex> = OnceLock::new();
    let re = LINK.get_or_init(|| {
        regex::Regex::new(r#"\]\(\s*<?([^)\s>]+)>?(?:\s+"[^"]*")?\s*\)|<(https?://[^>\s]+)>"#)
            .expect("valid link regex")
    });
    let base = Url::parse(base_url).ok();
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for cap in re.captures_iter(markdown) {
        let Some(target) = cap.get(1).or_else(|| cap.get(2)) else {
            continue;
        };
        let resolved = match &base {
            Some(base) => base.join(target.as_str()).ok(),
            None => Url::parse(target.as_str()).ok(),
        };
        let Some(mut url) = resolved else { continue };
        if !matches!(url.scheme(), "http" | "https") {
            continue;
        }
        url.set_fragment(None);
        let url = url.to_string();
        if seen.insert(url.clone()) {
            out.push(url);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOCX_CT: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";

    #[test]
    fn content_type_names_the_kind() {
        assert_eq!(
            DocumentKind::from_content_type(DOCX_CT),
            Some(DocumentKind::Docx)
        );
        assert_eq!(
            DocumentKind::from_content_type("application/pdf; charset=binary"),
            Some(DocumentKind::Pdf)
        );
        assert_eq!(
            DocumentKind::from_content_type("text/csv"),
            Some(DocumentKind::Csv)
        );
        assert_eq!(
            DocumentKind::from_content_type("application/octet-stream"),
            None
        );
        assert_eq!(DocumentKind::from_content_type("text/html"), None);
    }

    #[test]
    fn magic_bytes_are_the_fallback() {
        let pdf = b"%PDF-1.7\n...";
        assert_eq!(
            detect_kind(Some("application/octet-stream"), pdf),
            Some(DocumentKind::Pdf)
        );
        assert_eq!(detect_kind(None, pdf), Some(DocumentKind::Pdf));
        assert_eq!(
            detect_kind(None, b"\x89PNG\r\n\x1a\n...."),
            Some(DocumentKind::Image)
        );
        assert_eq!(detect_kind(None, b"hello"), None);
    }

    #[test]
    fn bytes_win_over_a_conflicting_header() {
        assert_eq!(
            detect_kind(Some("application/vnd.ms-excel"), b"%PDF-1.4 body"),
            Some(DocumentKind::Pdf)
        );
        // No signature to contradict the header: trust it.
        assert_eq!(
            detect_kind(Some("text/csv"), b"a,b\n1,2\n"),
            Some(DocumentKind::Csv)
        );
    }

    #[test]
    fn features_gate_kinds() {
        let mut features = FeaturesConfig::default();
        assert!(!DocumentKind::Pdf.allowed_by(&features));
        assert!(!DocumentKind::Docx.allowed_by(&features));
        features.documents = Some(scrapix_core::DocumentsConfig {
            enabled: true,
            max_size_mb: None,
        });
        assert!(DocumentKind::Docx.allowed_by(&features));
        assert!(!DocumentKind::Pdf.allowed_by(&features));
        assert!(!DocumentKind::Image.allowed_by(&features));
    }

    #[test]
    fn image_parses_as_one_page_needing_ocr() {
        let parsed = parse_document(b"\xFF\xD8\xFF\xE0jpeg", None, &ParseOptions::default())
            .expect("images parse");
        assert_eq!(parsed.kind, DocumentKind::Image);
        assert!(parsed.needs_ocr());
        assert_eq!(parsed.pages_needing_ocr, vec![1]);
        assert!(parsed.markdown.is_empty());
    }

    #[test]
    fn a_registered_backend_wins() {
        struct Fake;
        impl DocumentParser for Fake {
            fn name(&self) -> &'static str {
                "fake"
            }
            fn supports(&self, kind: DocumentKind) -> bool {
                kind == DocumentKind::Csv
            }
            fn parse(
                &self,
                _: &[u8],
                kind: DocumentKind,
                _: &ParseOptions,
            ) -> Result<ParsedDocument> {
                Ok(ParsedDocument {
                    parser: "fake",
                    markdown: "fake".into(),
                    ..ParsedDocument::empty(kind)
                })
            }
        }
        let dispatch = DocumentDispatch::default().with_parser(Arc::new(Fake));
        let parsed = dispatch
            .detect_and_parse(b"a,b", Some("text/csv"), &ParseOptions::default())
            .unwrap();
        assert_eq!(parsed.parser, "fake");
    }

    #[test]
    fn build_document_stamps_format_and_ocr_flags() {
        let parsed = ParsedDocument {
            parser: "pdf-inspector",
            markdown: "# Spec\n\n| a | b |\n|---|---|\n| 1 | 2 |".into(),
            title: Some("Spec".into()),
            language: Some("en".into()),
            page_count: Some(3),
            pdf_type: Some("mixed"),
            pages_needing_ocr: vec![2],
            ..ParsedDocument::empty(DocumentKind::Pdf)
        };
        let doc = build_document("https://example.com/docs/spec.pdf", 1024, &parsed, None).unwrap();
        assert_eq!(doc.title.as_deref(), Some("Spec"));
        assert!(doc.markdown.as_deref().unwrap().contains("| a | b |"));
        assert!(doc.content.as_deref().unwrap().contains("Spec"));
        let meta = doc.metadata.unwrap();
        assert_eq!(meta["content_type"], "application/pdf");
        assert_eq!(meta["document_format"], "pdf");
        assert_eq!(meta["pdf_bytes"], "1024");
        assert_eq!(meta["needs_ocr"], "true");
        assert_eq!(meta["pages_needing_ocr"], "2");
        assert!(doc.urls_tags.unwrap().contains(&"/docs".to_string()));
    }

    #[test]
    fn build_document_uses_fallback_title_and_leaves_scans_blank() {
        let parsed = ParsedDocument {
            pages_needing_ocr: vec![1],
            ..ParsedDocument::empty(DocumentKind::Pdf)
        };
        let doc = build_document(
            "https://example.com/a.pdf",
            10,
            &parsed,
            Some("Fallback".to_string()),
        )
        .unwrap();
        assert_eq!(doc.title.as_deref(), Some("Fallback"));
        assert!(doc.content.is_none());
        assert_eq!(doc.metadata.unwrap()["needs_ocr"], "true");
    }

    #[test]
    fn markdown_links_resolve_and_dedupe() {
        let md = "See [spec](https://a.test/spec#s1) and [rel](../b) and <https://a.test/spec> \
                  and [mail](mailto:x@y.z).";
        assert_eq!(
            markdown_links(md, "https://a.test/docs/x.pdf"),
            vec![
                "https://a.test/spec".to_string(),
                "https://a.test/b".to_string()
            ]
        );
    }
}
