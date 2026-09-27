//! Error types for Scrapix

use thiserror::Error;

/// Main error type for Scrapix operations
#[derive(Error, Debug)]
pub enum ScrapixError {
    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Crawl error: {0}")]
    Crawl(String),

    #[error("Parse error: {0}")]
    Parse(String),

    #[error("Storage error: {0}")]
    Storage(String),

    #[error("Queue error: {0}")]
    Queue(String),

    #[error("Network error: {0}")]
    Network(String),

    #[error("HTTP error: status {status}, url: {url}")]
    Http { status: u16, url: String },

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("URL parse error: {0}")]
    UrlParse(#[from] url::ParseError),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Validation error: {0}")]
    Validation(String),

    #[error("Rate limited: retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: u64 },

    #[error("Robots.txt disallowed: {url}")]
    RobotsDisallowed { url: String },

    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Timeout: {0}")]
    Timeout(String),

    #[error("Connection error: {0}")]
    Connection(String),

    /// An SSRF-safety refusal: a raw-IP host/redirect target, or a hostname
    /// whose DNS results are all non-public, was rejected outright. Callers
    /// (notably `HttpFetcher::fetch_inner`) must treat this as terminal and
    /// never retry it — retrying would just re-run the same refused
    /// resolution/redirect and burn through the retry backoff for nothing.
    #[error("Refused: {0}")]
    Refused(String),

    #[error("AI error: {0}")]
    Ai(String),

    /// A browser page action (`/scrape` `actions`) failed: `index` is its
    /// position in the request, `action` its type (e.g. `click`).
    #[error("actions[{index}] ({action}) failed: {message}")]
    Action {
        index: usize,
        action: String,
        message: String,
    },

    #[error("Internal error: {0}")]
    Internal(#[from] anyhow::Error),
}

/// Result type alias for Scrapix operations
pub type Result<T> = std::result::Result<T, ScrapixError>;

/// Error codes for API responses
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    ConfigInvalid = 1001,
    JobNotFound = 1002,
    IndexNotFound = 1003,
    RateLimited = 1004,
    Unauthorized = 1005,
    InternalError = 5000,
}

impl ErrorCode {
    pub fn http_status(&self) -> u16 {
        match self {
            ErrorCode::ConfigInvalid => 400,
            ErrorCode::JobNotFound => 404,
            ErrorCode::IndexNotFound => 404,
            ErrorCode::RateLimited => 429,
            ErrorCode::Unauthorized => 401,
            ErrorCode::InternalError => 500,
        }
    }
}
