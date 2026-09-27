//! OCR backends: page image → Markdown/text.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use scrapix_ai::{AiClient, ImageInput};
use tokio::io::AsyncWriteExt;
use tracing::{debug, warn};

use crate::error::OcrError;

/// Recognizes the text of one page image.
#[async_trait]
pub trait OcrBackend: Send + Sync {
    /// Stable identifier (`vision:<provider>/<model>`, `tesseract:<lang>`),
    /// reported in responses and part of the OCR cache key (a different
    /// backend or model never reuses another's output).
    fn name(&self) -> &str;

    /// Recognize `image` (`media_type` `image/png` for rendered PDF pages)
    /// and return its content as Markdown (or plain text).
    async fn recognize(&self, image: &[u8], media_type: &str) -> Result<String, OcrError>;
}

const VISION_SYSTEM_PROMPT: &str = "You are an OCR engine. Transcribe the document page in the \
image into GitHub-Flavored Markdown. Reproduce all visible text faithfully and in reading order \
(respect columns). Use Markdown headings for titles, lists for lists and GFM pipe tables for \
tables. Do not summarize, translate, correct or comment. Do not describe images or add text that \
is not on the page. Output only the Markdown, without code fences. If the page has no text, \
output nothing.";

const VISION_USER_PROMPT: &str = "Transcribe this page.";

/// Vision-LLM OCR through `scrapix-ai`: layout- and table-aware
/// transcription with any configured provider. Calls go through
/// [`AiClient::vision_chat`], so they are rate-limited, retried, and — when
/// made inside `AI_USAGE_CONTEXT` with feature `ocr` — reported as
/// `AiUsageEvent`s into ClickHouse like every other AI call.
pub struct VisionLlmOcr {
    client: Arc<AiClient>,
    model: String,
    max_tokens: u32,
    name: String,
}

impl VisionLlmOcr {
    pub fn new(client: Arc<AiClient>, model: impl Into<String>) -> Self {
        let model = model.into();
        let name = format!("vision:{}/{}", client.provider_name(), model);
        Self {
            client,
            model,
            max_tokens: 4096,
            name,
        }
    }

    /// With the provider's default vision model.
    pub fn with_default_model(client: Arc<AiClient>) -> Self {
        let model = AiClient::default_vision_model(client.provider_name());
        Self::new(client, model)
    }
}

#[async_trait]
impl OcrBackend for VisionLlmOcr {
    fn name(&self) -> &str {
        &self.name
    }

    async fn recognize(&self, image: &[u8], media_type: &str) -> Result<String, OcrError> {
        let image = ImageInput {
            media_type: media_type.to_string(),
            data: image.to_vec(),
        };
        let response = self
            .client
            .vision_chat(
                VISION_SYSTEM_PROMPT,
                VISION_USER_PROMPT,
                &image,
                &self.model,
                Some(self.max_tokens),
            )
            .await
            .map_err(|e| OcrError::Backend(e.to_string()))?;
        Ok(strip_code_fence(&response.content))
    }
}

/// Models sometimes wrap the transcription in a ```markdown fence anyway.
fn strip_code_fence(text: &str) -> String {
    let trimmed = text.trim();
    if let Some(rest) = trimmed.strip_prefix("```") {
        let rest = rest.split_once('\n').map(|(_, body)| body).unwrap_or("");
        if let Some(body) = rest.trim_end().strip_suffix("```") {
            return body.trim().to_string();
        }
    }
    trimmed.to_string()
}

/// Local Tesseract through its CLI (`tesseract stdin stdout`). No API key
/// and no per-page cost, so self-hosted deployments keep OCR working with
/// zero configuration; weaker than a vision model on tables and layout.
#[derive(Debug, Clone)]
pub struct TesseractOcr {
    binary: PathBuf,
    lang: String,
    timeout: Duration,
    name: String,
}

impl TesseractOcr {
    /// `binary` defaults to `tesseract` on `PATH`; `lang` to `eng`
    /// (Tesseract syntax, e.g. `eng+fra`).
    pub fn new(binary: Option<PathBuf>, lang: Option<String>) -> Self {
        let lang = lang.unwrap_or_else(|| "eng".to_string());
        Self {
            binary: binary.unwrap_or_else(|| PathBuf::from("tesseract")),
            name: format!("tesseract:{lang}"),
            lang,
            timeout: Duration::from_secs(120),
        }
    }

    /// Whether the binary runs (`tesseract --version`).
    pub async fn probe(&self) -> Result<String, OcrError> {
        let output = tokio::process::Command::new(&self.binary)
            .arg("--version")
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|e| {
                OcrError::Unavailable(format!(
                    "cannot run {}: {e} (install tesseract-ocr)",
                    self.binary.display()
                ))
            })?;
        // Older versions print the banner on stderr.
        let banner = if output.stdout.is_empty() {
            output.stderr
        } else {
            output.stdout
        };
        Ok(String::from_utf8_lossy(&banner)
            .lines()
            .next()
            .unwrap_or("tesseract")
            .trim()
            .to_string())
    }
}

#[async_trait]
impl OcrBackend for TesseractOcr {
    fn name(&self) -> &str {
        &self.name
    }

    async fn recognize(&self, image: &[u8], _media_type: &str) -> Result<String, OcrError> {
        let mut child = tokio::process::Command::new(&self.binary)
            .args(["stdin", "stdout", "-l", &self.lang, "--psm", "3"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                // Details (binary path) go to the log, not to API callers.
                warn!(binary = %self.binary.display(), error = %e, "cannot run tesseract");
                OcrError::Unavailable("OCR (Tesseract) is not installed on this server".into())
            })?;

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| OcrError::Backend("tesseract stdin unavailable".into()))?;
        let data = image.to_vec();
        let writer = tokio::spawn(async move {
            let result = stdin.write_all(&data).await;
            drop(stdin);
            result
        });

        let output = tokio::time::timeout(self.timeout, child.wait_with_output())
            .await
            .map_err(|_| OcrError::Backend("tesseract timed out".into()))?
            .map_err(|e| OcrError::Backend(format!("tesseract failed: {e}")))?;
        let _ = writer.await;

        if !output.status.success() {
            warn!(
                status = %output.status,
                stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                "tesseract failed on a page"
            );
            return Err(OcrError::Backend(format!(
                "tesseract could not read the page ({})",
                output.status
            )));
        }
        let text = String::from_utf8_lossy(&output.stdout);
        debug!(chars = text.len(), "tesseract recognized page");
        Ok(normalize_tesseract(&text))
    }
}

/// Collapse Tesseract's line-per-line output into paragraphs separated by
/// blank lines; drop the trailing form feed.
fn normalize_tesseract(text: &str) -> String {
    let mut paragraphs: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        let line = line.trim().trim_matches('\u{c}');
        if line.is_empty() {
            if !current.is_empty() {
                paragraphs.push(std::mem::take(&mut current));
            }
            continue;
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(line);
    }
    if !current.is_empty() {
        paragraphs.push(current);
    }
    paragraphs.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_markdown_fences() {
        assert_eq!(
            strip_code_fence("```markdown\n# Hi\n\ntext\n```"),
            "# Hi\n\ntext"
        );
        assert_eq!(strip_code_fence("# Hi"), "# Hi");
    }

    #[test]
    fn normalizes_tesseract_lines() {
        assert_eq!(
            normalize_tesseract("SCANNED INVOICE\n\nInvoice number\n4821\n\n\u{c}"),
            "SCANNED INVOICE\n\nInvoice number 4821"
        );
    }

    #[tokio::test]
    async fn missing_tesseract_is_unavailable() {
        let t = TesseractOcr::new(Some("/nonexistent/tesseract".into()), None);
        assert!(matches!(t.probe().await, Err(OcrError::Unavailable(_))));
        assert!(matches!(
            t.recognize(b"png", "image/png").await,
            Err(OcrError::Unavailable(_))
        ));
    }
}
