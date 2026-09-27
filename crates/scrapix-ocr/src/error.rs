use thiserror::Error;

/// Why an OCR step failed.
#[derive(Debug, Error)]
pub enum OcrError {
    /// A required component is not installed or not configured (PDFium
    /// library, `tesseract` binary, AI provider key).
    #[error("OCR unavailable: {0}")]
    Unavailable(String),

    /// Rasterizing a page failed.
    #[error("page rendering failed: {0}")]
    Render(String),

    /// The OCR backend failed on a page.
    #[error("OCR backend error: {0}")]
    Backend(String),
}
