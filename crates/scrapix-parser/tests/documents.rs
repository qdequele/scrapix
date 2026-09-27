//! Document dispatch over real fixtures (see `fixtures/generate_fixtures.py`).

use scrapix_parser::document::{build_document, detect_kind, parse_document, DocumentKind};
use scrapix_parser::{pdf, ParseOptions};

fn fixture(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn parse(name: &str, content_type: Option<&str>) -> scrapix_parser::ParsedDocument {
    parse_document(&fixture(name), content_type, &ParseOptions::default())
        .unwrap_or_else(|e| panic!("{name}: {e}"))
}

#[test]
fn office_formats_produce_readable_markdown() {
    for (name, kind) in [
        ("report.docx", DocumentKind::Docx),
        ("report.xlsx", DocumentKind::Excel),
        ("report.pptx", DocumentKind::Pptx),
        ("report.epub", DocumentKind::Epub),
        ("report.odt", DocumentKind::Odt),
    ] {
        // Served as a generic download: detection falls back to the bytes.
        let parsed = parse(name, Some("application/octet-stream"));
        assert_eq!(parsed.kind, kind, "{name}");
        assert_eq!(parsed.parser, "anydoc", "{name}");
        assert!(!parsed.needs_ocr(), "{name}");
        let md = &parsed.markdown;
        if kind == DocumentKind::Excel {
            assert!(md.contains("North") && md.contains("143"), "{name}: {md}");
            assert!(
                md.contains('|'),
                "{name}: spreadsheet renders as a table: {md}"
            );
        } else {
            assert!(md.contains("Quarterly Report"), "{name}: {md}");
            assert!(md.contains("Revenue grew"), "{name}: {md}");
        }
    }
}

#[test]
fn docx_keeps_heading_and_table() {
    let parsed = parse("report.docx", None);
    assert!(
        parsed.markdown.contains("# Quarterly Report"),
        "{}",
        parsed.markdown
    );
    assert!(
        parsed.markdown.contains("| North"),
        "table preserved: {}",
        parsed.markdown
    );
    assert_eq!(parsed.title.as_deref(), Some("Quarterly Report"));
    assert_eq!(parsed.language.as_deref(), Some("en"));
    assert!(parsed.has_tables);
}

#[test]
fn text_pdf_yields_markdown_with_the_table() {
    let parsed = parse("text-table.pdf", Some("application/pdf"));
    assert_eq!(parsed.kind, DocumentKind::Pdf);
    assert_eq!(parsed.parser, "pdf-inspector");
    assert_eq!(parsed.pdf_type, Some("text_based"));
    assert!(!parsed.needs_ocr(), "{:?}", parsed.pages_needing_ocr);
    assert_eq!(parsed.page_count, Some(1));
    assert_eq!(parsed.title.as_deref(), Some("Quarterly Report"));
    let md = &parsed.markdown;
    assert!(md.contains("Quarterly Report"), "{md}");
    assert!(parsed.has_tables, "table detected: {md}");
    for cell in ["Region", "North", "143", "150"] {
        assert!(md.contains(cell), "{cell} missing: {md}");
    }
    assert!(
        md.lines()
            .any(|l| l.contains("North") && l.contains("120") && l.contains('|')),
        "table row rendered as a Markdown table row: {md}"
    );
}

#[test]
fn scanned_pdf_is_flagged_instead_of_empty() {
    let parsed = parse("scanned.pdf", Some("application/pdf"));
    assert!(parsed.needs_ocr());
    assert_eq!(parsed.pages_needing_ocr, vec![1]);
    assert!(matches!(parsed.pdf_type, Some("scanned" | "image_based")));
    assert!(parsed.markdown.trim().is_empty(), "{}", parsed.markdown);

    let doc = build_document("https://example.com/scan.pdf", 10, &parsed, None).unwrap();
    let meta = doc.metadata.unwrap();
    assert_eq!(meta["needs_ocr"], "true");
    assert_eq!(meta["pages_needing_ocr"], "1");
    // Title comes from the PDF Info dictionary.
    assert_eq!(doc.title.as_deref(), Some("Scanned Invoice"));
}

#[test]
fn mixed_pdf_flags_only_its_scanned_page() {
    let parsed = parse("mixed.pdf", None);
    assert_eq!(parsed.page_count, Some(2));
    assert_eq!(parsed.pages_needing_ocr, vec![2]);
    assert!(parsed.markdown.contains("Quarterly Report"));

    let pages = pdf::page_markdown(&fixture("mixed.pdf"), None).unwrap();
    assert_eq!(pages.len(), 2);
    assert!(pages[0].contains("Quarterly Report"));
    assert!(pages[1].trim().is_empty());
}

#[test]
fn max_pages_caps_pdf_parsing() {
    let parsed = parse_document(
        &fixture("mixed.pdf"),
        None,
        &ParseOptions { max_pages: Some(1) },
    )
    .unwrap();
    assert_eq!(parsed.page_count, Some(2));
    assert_eq!(parsed.pages_processed, Some(1));
    // Page 2 (the scan) was never looked at.
    assert!(parsed.pages_needing_ocr.is_empty());
}

#[test]
fn pdf_links_are_extracted() {
    let links =
        pdf::extract_links(&fixture("text-table.pdf"), "https://example.com/r.pdf").unwrap();
    assert_eq!(links, vec!["https://example.com/methodology".to_string()]);

    let md = parse("text-table.pdf", None).markdown;
    let from_markdown = scrapix_parser::document::markdown_links(&md, "https://example.com/r.pdf");
    assert!(from_markdown.contains(&"https://example.com/methodology".to_string()));
}

#[test]
fn image_fixture_is_detected_as_an_image() {
    assert_eq!(
        detect_kind(Some("image/jpeg"), &fixture("scan.jpg")),
        Some(DocumentKind::Image)
    );
}
