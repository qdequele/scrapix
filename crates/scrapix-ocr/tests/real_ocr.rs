//! End-to-end OCR with the real components: PDFium rasterization and the
//! local Tesseract backend (no API key). Ignored by default because both
//! are system dependencies:
//!
//! ```bash
//! PDFIUM_LIB_PATH=/path/to/libpdfium.dylib \
//!   cargo test -p scrapix-ocr --test real_ocr -- --ignored
//! ```

use std::sync::Arc;

use scrapix_core::OcrMode;
use scrapix_ocr::{
    MemoryOcrBudget, MemoryOcrCache, OcrBackend, OcrEngine, OcrRequest, OcrSettings,
    PageRasterizer, PdfiumRasterizer, TesseractOcr,
};
use scrapix_parser::ParseOptions;

fn fixture(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../scrapix-parser/tests/fixtures")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn engine() -> OcrEngine {
    OcrEngine::new(
        Arc::new(PdfiumRasterizer::new()),
        Arc::new(TesseractOcr::new(None, None)),
        Arc::new(MemoryOcrCache::new(100)),
        Arc::new(MemoryOcrBudget::new(0)),
        OcrSettings::default(),
    )
}

#[test]
#[ignore = "needs libpdfium (PDFIUM_LIB_PATH)"]
fn pdfium_renders_a_scanned_page() {
    let pngs = PdfiumRasterizer::new()
        .render_png(&fixture("scanned.pdf"), &[1], 150.0)
        .expect("render");
    assert_eq!(pngs.len(), 1);
    assert!(pngs[0].starts_with(b"\x89PNG\r\n\x1a\n"));
    assert!(pngs[0].len() > 10_000, "a real page image");
}

#[tokio::test]
#[ignore = "needs tesseract"]
async fn tesseract_reads_an_image() {
    let text = TesseractOcr::new(None, None)
        .recognize(&fixture("scan.jpg"), "image/jpeg")
        .await
        .expect("tesseract");
    assert!(text.contains("INVOICE"), "{text}");
    assert!(text.contains("4821"), "{text}");
}

#[tokio::test]
#[ignore = "needs libpdfium and tesseract"]
async fn scanned_pdf_becomes_readable_markdown_with_auto() {
    let bytes = fixture("scanned.pdf");
    let mut parsed =
        scrapix_parser::parse_document(&bytes, None, &ParseOptions::default()).unwrap();
    assert!(parsed.needs_ocr());

    let report = engine()
        .apply(
            &bytes,
            &mut parsed,
            &OcrRequest {
                mode: OcrMode::Auto,
                ..Default::default()
            },
        )
        .await;
    assert_eq!(report.pages_processed, 1, "{report:?}");
    assert!(report.backend.unwrap().starts_with("tesseract"));
    assert!(!parsed.needs_ocr());
    assert!(
        parsed.markdown.contains("Invoice number 4821"),
        "{}",
        parsed.markdown
    );
    assert!(
        parsed.markdown.contains("Total due 1250 EUR"),
        "{}",
        parsed.markdown
    );
    assert_eq!(parsed.language.as_deref(), Some("en"));
}

#[tokio::test]
#[ignore = "needs libpdfium and tesseract"]
async fn mixed_pdf_ocrs_only_its_scanned_page() {
    let bytes = fixture("mixed.pdf");
    let mut parsed =
        scrapix_parser::parse_document(&bytes, None, &ParseOptions::default()).unwrap();
    let report = engine()
        .apply(
            &bytes,
            &mut parsed,
            &OcrRequest {
                mode: OcrMode::Auto,
                ..Default::default()
            },
        )
        .await;
    assert_eq!(report.pages, vec![2], "{report:?}");
    assert_eq!(report.pages_skipped, 1, "the text page stayed native");
    let md = &parsed.markdown;
    assert!(md.contains("|North|120|135|"), "native table kept: {md}");
    assert!(md.contains("Invoice number 4821"), "OCR text merged: {md}");
    assert!(md.find("Quarterly Report") < md.find("Invoice number"));
}
