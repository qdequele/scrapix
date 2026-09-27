//! Page rasterization: PDF pages → PNG images for OCR.
//!
//! pdf-inspector is `lopdf`-based and does not render, so pages that need
//! OCR are rendered with **PDFium** (the `firecrawl-pdfium` binding that
//! pdf-inspector's own OCR pipeline uses). Why PDFium over poppler or
//! MuPDF:
//!
//! - best render quality/speed of the candidates and BSD-licensed (MuPDF
//!   is AGPL); no process spawn per page like a `pdftoppm` shell-out;
//! - the binding loads `libpdfium` **at runtime** (`PDFIUM_LIB_PATH`, the
//!   executable's directory, then the system loader), so builds never need
//!   it, and a deployment without it keeps working with PDF OCR reported
//!   as unavailable. The images ship the pinned `bblanchon/pdfium-binaries`
//!   build (see the Dockerfiles).
//!
//! Before rendering, the document goes through pdf-inspector's
//! `widen_degenerate_form_bboxes_mem`, which repairs form XObjects whose
//! zero-area `/BBox` would otherwise render a page blank — the same repair
//! pdf-inspector applies to its own extraction.

use std::sync::OnceLock;

use firecrawl_pdfium::{Pdfium, PixelFormat, RenderConfig};
use tracing::warn;

use crate::error::OcrError;

/// Renders PDF pages to PNG.
pub trait PageRasterizer: Send + Sync {
    /// Stable name for logs and reports.
    fn name(&self) -> &str;

    /// Render the 1-indexed `pages` of `pdf` to PNG images, in the order
    /// given. Blocking (CPU-bound): call it from `spawn_blocking`.
    fn render_png(&self, pdf: &[u8], pages: &[u32], dpi: f32) -> Result<Vec<Vec<u8>>, OcrError>;
}

/// PDFium-backed rasterizer. The library is loaded on first use and the
/// outcome cached for the process lifetime.
#[derive(Default)]
pub struct PdfiumRasterizer {
    pdfium: OnceLock<Result<Pdfium, String>>,
}

impl PdfiumRasterizer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Load PDFium now (instead of on first render) and report whether it
    /// is usable, so a service can log its OCR capability at startup.
    pub fn probe(&self) -> Result<(), OcrError> {
        self.pdfium().map(|_| ())
    }

    fn pdfium(&self) -> Result<&Pdfium, OcrError> {
        self.pdfium
            .get_or_init(|| {
                Pdfium::load().map_err(|e| {
                    // The load error lists the server's search paths: log it
                    // once, and report only a short message to API callers.
                    warn!(
                        error = %e,
                        "cannot load PDFium; PDF OCR is unavailable (install libpdfium or set PDFIUM_LIB_PATH)"
                    );
                    "PDF page rendering (PDFium) is not installed on this server".to_string()
                })
            })
            .as_ref()
            .map_err(|e| OcrError::Unavailable(e.clone()))
    }
}

impl PageRasterizer for PdfiumRasterizer {
    fn name(&self) -> &str {
        "pdfium"
    }

    fn render_png(&self, pdf: &[u8], pages: &[u32], dpi: f32) -> Result<Vec<Vec<u8>>, OcrError> {
        if pages.is_empty() {
            return Ok(Vec::new());
        }
        if pages.contains(&0) {
            return Err(OcrError::Render("page numbers are 1-indexed".into()));
        }
        let pdfium = self.pdfium()?;

        let repaired = pdf_inspector::widen_degenerate_form_bboxes_mem(pdf)
            .ok()
            .flatten();
        let bytes = repaired.unwrap_or_else(|| pdf.to_vec());
        let document = pdfium
            .load_document(bytes, None)
            .map_err(|e| OcrError::Render(e.to_string()))?;
        if document.form_type() != firecrawl_pdfium::FormType::None {
            // Visible form-field appearances are part of what a scan shows.
            let _ = document.enable_form_rendering();
        }

        let config = RenderConfig::new()
            .dpi(dpi)
            .pixel_format(PixelFormat::Rgba8)
            // A pathological page size must not allocate gigabytes.
            .max_output_bytes(128 * 1024 * 1024);
        let page_count = document.page_count();
        pages
            .iter()
            .map(|&number| {
                if number as usize > page_count {
                    return Err(OcrError::Render(format!(
                        "page {number} is out of bounds for a {page_count}-page document"
                    )));
                }
                let page = document
                    .page(number as usize - 1)
                    .map_err(|e| OcrError::Render(e.to_string()))?;
                let rendered = page
                    .render(&config)
                    .map_err(|e| OcrError::Render(format!("page {number}: {e}")))?;
                encode_rgba_as_png(
                    rendered.width(),
                    rendered.height(),
                    rendered.stride(),
                    rendered.pixels(),
                )
            })
            .collect()
    }
}

/// Encode an RGBA buffer (rendered over an opaque white background) as an
/// RGB PNG. Deterministic for identical pixels, which keeps the
/// page-image cache key stable.
pub fn encode_rgba_as_png(
    width: u32,
    height: u32,
    stride: usize,
    pixels: &[u8],
) -> Result<Vec<u8>, OcrError> {
    let row = width as usize * 4;
    if stride < row || pixels.len() < stride * (height as usize).saturating_sub(1) + row {
        return Err(OcrError::Render(
            "pixel buffer smaller than its dimensions".into(),
        ));
    }
    let mut rgb = Vec::with_capacity(width as usize * height as usize * 3);
    for y in 0..height as usize {
        let line = &pixels[y * stride..y * stride + row];
        for px in line.as_chunks::<4>().0 {
            rgb.extend_from_slice(&px[..3]);
        }
    }

    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fast);
        let mut writer = encoder
            .write_header()
            .map_err(|e| OcrError::Render(format!("PNG header: {e}")))?;
        writer
            .write_image_data(&rgb)
            .map_err(|e| OcrError::Render(format!("PNG encode: {e}")))?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_padded_rows_deterministically() {
        // 2x2 RGBA with 4 bytes of row padding.
        let pixels = [
            255, 0, 0, 255, 0, 255, 0, 255, 9, 9, 9, 9, //
            0, 0, 255, 255, 255, 255, 255, 255, 9, 9, 9, 9,
        ];
        let png = encode_rgba_as_png(2, 2, 12, &pixels).unwrap();
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert_eq!(png, encode_rgba_as_png(2, 2, 12, &pixels).unwrap());
    }

    #[test]
    fn rejects_short_buffers() {
        assert!(encode_rgba_as_png(4, 4, 16, &[0; 10]).is_err());
    }
}
