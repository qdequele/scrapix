//! Content-type classification shared by the fetcher and the content worker.
//!
//! Binary documents (PDF, office formats, and generic `octet-stream`
//! bodies that may turn out to be one) travel through Kafka as base64 in
//! `RawPageMessage.html`. The fetcher encodes exactly the content types
//! [`is_binary_document`] accepts, and the content worker decodes exactly
//! those, so both sides must use this one predicate.
//!
//! These helpers only look at the `Content-Type` header. Deciding *which*
//! document format a body is (header first, magic bytes as fallback) is
//! `scrapix_parser::document::detect_kind`'s job; the URL extension is
//! never trusted for either.

/// The lowercase media type of a `Content-Type` header value, without
/// parameters (`application/pdf; charset=binary` → `application/pdf`).
pub fn media_type(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// `application/pdf` (and the legacy `application/x-pdf`).
pub fn is_pdf(content_type: &str) -> bool {
    matches!(
        media_type(content_type).as_str(),
        "application/pdf" | "application/x-pdf"
    )
}

/// Office and other document formats converted through anydoc: Word,
/// PowerPoint, Excel (OOXML, legacy binary, macro-enabled and template
/// variants), OpenDocument, RTF, EPUB and CSV.
pub fn is_office_document(content_type: &str) -> bool {
    let mt = media_type(content_type);
    let mt = mt.as_str();
    mt.starts_with("application/vnd.openxmlformats-officedocument.")
        || mt.starts_with("application/vnd.ms-word")
        || mt.starts_with("application/vnd.ms-excel")
        || mt.starts_with("application/vnd.ms-powerpoint")
        || mt.starts_with("application/vnd.oasis.opendocument.text")
        || mt.starts_with("application/vnd.oasis.opendocument.spreadsheet")
        || mt.starts_with("application/vnd.oasis.opendocument.presentation")
        || matches!(
            mt,
            "application/msword"
                | "application/rtf"
                | "application/x-rtf"
                | "text/rtf"
                | "application/epub+zip"
                | "text/csv"
                | "application/csv"
        )
}

/// Generic binary content types servers use for downloads of any kind. The
/// body may be a document; the parser decides from its magic bytes.
pub fn is_generic_binary(content_type: &str) -> bool {
    matches!(
        media_type(content_type).as_str(),
        "application/octet-stream"
            | "binary/octet-stream"
            | "application/download"
            | "application/x-download"
            | "application/force-download"
    )
}

/// Raster image types OCR can read (uploads only; never crawled).
pub fn is_image(content_type: &str) -> bool {
    matches!(
        media_type(content_type).as_str(),
        "image/png" | "image/jpeg" | "image/jpg" | "image/gif" | "image/webp" | "image/tiff"
    )
}

/// Whether a crawled body of this content type is carried base64-encoded
/// (see the module docs). Must match on both sides of the Kafka hop.
pub fn is_binary_document(content_type: &str) -> bool {
    is_pdf(content_type) || is_office_document(content_type) || is_generic_binary(content_type)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_type_strips_parameters_and_case() {
        assert_eq!(
            media_type("Application/PDF; charset=binary"),
            "application/pdf"
        );
    }

    #[test]
    fn classifies_office_types() {
        for ct in [
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            "application/vnd.ms-excel.sheet.macroEnabled.12",
            "application/msword",
            "application/vnd.oasis.opendocument.text",
            "application/epub+zip",
            "text/csv; charset=utf-8",
            "text/rtf",
        ] {
            assert!(is_office_document(ct), "{ct}");
            assert!(is_binary_document(ct), "{ct}");
        }
        assert!(!is_office_document("text/html"));
        assert!(!is_office_document("application/pdf"));
    }

    #[test]
    fn binary_documents_cover_pdf_and_downloads_but_not_html() {
        assert!(is_binary_document("application/pdf"));
        assert!(is_binary_document("application/octet-stream"));
        assert!(!is_binary_document("text/html; charset=utf-8"));
        assert!(!is_binary_document("text/markdown"));
        assert!(!is_binary_document("image/png"));
    }
}
