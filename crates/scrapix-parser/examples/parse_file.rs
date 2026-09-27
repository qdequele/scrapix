//! Parse a document file and print its Markdown and OCR assessment.
//!
//! ```bash
//! cargo run -p scrapix-parser --example parse_file -- path/to/report.docx
//! ```

fn main() {
    let path = std::env::args().nth(1).expect("usage: parse_file <path>");
    let bytes = std::fs::read(&path).expect("read file");
    match scrapix_parser::parse_document(&bytes, None, &Default::default()) {
        Ok(parsed) => {
            println!(
                "kind={} parser={} pages={:?} pdf_type={:?} needs_ocr={:?} title={:?} language={:?}\n",
                parsed.kind.as_str(),
                parsed.parser,
                parsed.page_count,
                parsed.pdf_type,
                parsed.pages_needing_ocr,
                parsed.title,
                parsed.language
            );
            println!("{}", parsed.markdown);
        }
        Err(e) => eprintln!("error: {e}"),
    }
}
