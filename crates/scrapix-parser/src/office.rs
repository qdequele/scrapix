//! Office and other document formats via [anydoc].
//!
//! One API converts Word (`.doc`/`.docx`/`.docm`), PowerPoint (`.ppt`,
//! `.pptx` and slideshow/template variants), Excel (`.xls`, `.xlsx`,
//! `.xlsm`, `.xlsb`), OpenDocument (`.odt`/`.ods`/`.odp`), RTF, EPUB and CSV
//! to GitHub-Flavored Markdown, preserving headings, inline formatting,
//! tables, footnotes and speaker notes. Pure Rust, no external services.
//!
//! [anydoc]: https://github.com/firecrawl/anydoc

use anydoc::{ConvertError, Format};
use scrapix_core::{Result, ScrapixError};

use crate::document::{DocumentKind, DocumentParser, ParseOptions, ParsedDocument};
use crate::language::detect_language;

/// The anydoc backend of the document dispatch.
#[derive(Debug, Default, Clone, Copy)]
pub struct AnydocParser;

impl DocumentParser for AnydocParser {
    fn name(&self) -> &'static str {
        "anydoc"
    }

    fn supports(&self, kind: DocumentKind) -> bool {
        anydoc_format(kind).is_some() && kind != DocumentKind::Pdf
    }

    fn parse(
        &self,
        bytes: &[u8],
        kind: DocumentKind,
        _opts: &ParseOptions,
    ) -> Result<ParsedDocument> {
        let format = anydoc_format(kind).ok_or_else(|| {
            ScrapixError::Parse(format!("anydoc cannot convert {}", kind.as_str()))
        })?;
        if bytes.is_empty() {
            return Err(ScrapixError::Parse(format!("Empty {} body", kind.as_str())));
        }
        let markdown = anydoc::to_markdown_bytes(bytes, format).map_err(map_error)?;
        let markdown = markdown.trim().to_string();

        let text = crate::markdown_to_text(&markdown);
        let language = if text.trim().is_empty() {
            None
        } else {
            detect_language(&text)
        };

        Ok(ParsedDocument {
            kind,
            parser: "anydoc",
            title: first_heading(&markdown),
            has_tables: has_gfm_table(&markdown),
            markdown,
            language,
            ..ParsedDocument::empty(kind)
        })
    }
}

/// The anydoc parser selected for a document kind.
pub(crate) fn anydoc_format(kind: DocumentKind) -> Option<Format> {
    Some(match kind {
        DocumentKind::Pdf => Format::Pdf,
        DocumentKind::Doc => Format::Doc,
        DocumentKind::Docx => Format::Docx,
        DocumentKind::Ppt => Format::Ppt,
        DocumentKind::Pptx => Format::Pptx,
        DocumentKind::Excel => Format::Excel,
        DocumentKind::Odt => Format::Odt,
        DocumentKind::Ods => Format::Ods,
        DocumentKind::Odp => Format::Odp,
        DocumentKind::Rtf => Format::Rtf,
        DocumentKind::Epub => Format::Epub,
        DocumentKind::Csv => Format::Csv,
        DocumentKind::Image => return None,
    })
}

/// Map anydoc's content detection onto our kinds (magic-byte fallback).
pub(crate) fn kind_from_anydoc(format: Format) -> DocumentKind {
    match format {
        Format::Pdf => DocumentKind::Pdf,
        Format::Doc => DocumentKind::Doc,
        Format::Docx => DocumentKind::Docx,
        Format::Ppt => DocumentKind::Ppt,
        Format::Pptx => DocumentKind::Pptx,
        Format::Excel => DocumentKind::Excel,
        Format::Odt => DocumentKind::Odt,
        Format::Ods => DocumentKind::Ods,
        Format::Odp => DocumentKind::Odp,
        Format::Rtf => DocumentKind::Rtf,
        Format::Epub => DocumentKind::Epub,
        Format::Csv => DocumentKind::Csv,
    }
}

/// Whether the Markdown contains a GFM table (a `| --- | --- |` delimiter
/// row); anydoc does not report tables separately.
fn has_gfm_table(markdown: &str) -> bool {
    markdown.lines().any(|line| {
        let t = line.trim();
        t.starts_with('|')
            && t.contains("---")
            && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
    })
}

/// The first ATX heading (any level) of the Markdown, as a title candidate.
fn first_heading(markdown: &str) -> Option<String> {
    markdown.lines().find_map(|line| {
        let t = line.trim_start();
        let hashes = t.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes) && t[hashes..].starts_with(' ') {
            // Drop an explicit `{#anchor}` suffix anydoc may emit.
            let text = t[hashes..].trim();
            let text = match text.rfind(" {#") {
                Some(i) if text.ends_with('}') => &text[..i],
                _ => text,
            };
            let text = text.trim_matches(|c| c == '*' || c == '_').trim();
            (!text.is_empty()).then(|| text.to_string())
        } else {
            None
        }
    })
}

fn map_error(e: ConvertError) -> ScrapixError {
    ScrapixError::Parse(format!("Document conversion failed ({}): {e}", e.code()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_converts_to_a_markdown_table() {
        let doc = AnydocParser
            .parse(
                b"name,qty\nwidget,3\ngadget,5\n",
                DocumentKind::Csv,
                &ParseOptions::default(),
            )
            .unwrap();
        assert!(doc.markdown.contains("widget"), "{}", doc.markdown);
        assert!(doc.markdown.contains('|'), "{}", doc.markdown);
        assert!(!doc.needs_ocr());
    }

    #[test]
    fn garbage_is_a_parse_error() {
        assert!(AnydocParser
            .parse(b"not a docx", DocumentKind::Docx, &ParseOptions::default())
            .is_err());
    }

    #[test]
    fn detects_gfm_tables() {
        assert!(has_gfm_table("| a | b |\n| --- | :-: |\n| 1 | 2 |"));
        assert!(!has_gfm_table("a | b\n\n---\n"));
    }

    #[test]
    fn first_heading_skips_body_and_anchors() {
        assert_eq!(
            first_heading("intro\n\n## **Quarterly report** {#q3}\n"),
            Some("Quarterly report".to_string())
        );
        assert_eq!(first_heading("#hashtag\nplain"), None);
    }
}
