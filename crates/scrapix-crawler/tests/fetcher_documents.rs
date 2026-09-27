//! Binary document acceptance, base64 transport and size caps.

use std::sync::Arc;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use scrapix_core::{CrawlUrl, DocumentsConfig, FeaturesConfig, PdfConfig};
use scrapix_crawler::{FetchOptions, HttpFetcherBuilder, RobotsCache, RobotsConfig};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const DOCX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";

fn fetcher() -> scrapix_crawler::HttpFetcher {
    let robots = Arc::new(
        RobotsCache::new(RobotsConfig {
            respect_robots: false,
            ..Default::default()
        })
        .unwrap(),
    );
    HttpFetcherBuilder::new()
        .allow_private_ips(true)
        .max_retries(0)
        .build(robots)
        .unwrap()
}

fn url(server: &MockServer, p: &str) -> String {
    format!("{}{}", server.uri().replace("127.0.0.1", "localhost"), p)
}

async fn serve(server: &MockServer, p: &str, content_type: &str, body: Vec<u8>) {
    Mock::given(method("GET"))
        .and(path(p))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, content_type))
        .mount(server)
        .await;
}

fn documents_features(max_size_mb: Option<u64>) -> FeaturesConfig {
    FeaturesConfig {
        documents: Some(DocumentsConfig {
            enabled: true,
            max_size_mb,
        }),
        ..Default::default()
    }
}

#[tokio::test]
async fn office_documents_are_rejected_unless_enabled() {
    let server = MockServer::start().await;
    serve(&server, "/r.docx", DOCX, b"PK\x03\x04docx".to_vec()).await;
    let err = fetcher()
        .fetch(&CrawlUrl::seed(url(&server, "/r.docx")))
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("Unsupported content type"),
        "{err}"
    );
}

#[tokio::test]
async fn enabled_office_documents_travel_base64() {
    let server = MockServer::start().await;
    let body = b"PK\x03\x04\xff\xfe binary".to_vec();
    serve(&server, "/r.docx", DOCX, body.clone()).await;
    let page = fetcher()
        .fetch_with_options(
            &CrawlUrl::seed(url(&server, "/r.docx")),
            FetchOptions::for_features(&documents_features(None)),
        )
        .await
        .unwrap();
    assert_eq!(BASE64.decode(page.html).unwrap(), body);
    assert!(scrapix_core::content_types::is_binary_document(
        page.content_type.as_deref().unwrap()
    ));
}

#[tokio::test]
async fn generic_downloads_are_accepted_when_a_document_type_is_enabled() {
    let server = MockServer::start().await;
    let body = b"%PDF-1.4 bytes".to_vec();
    serve(
        &server,
        "/download",
        "application/octet-stream",
        body.clone(),
    )
    .await;

    // Only PDFs enabled: still accepted, the content worker sniffs the bytes.
    let pdf_only = FeaturesConfig {
        pdf: Some(PdfConfig {
            enabled: true,
            ..Default::default()
        }),
        ..Default::default()
    };
    let page = fetcher()
        .fetch_with_options(
            &CrawlUrl::seed(url(&server, "/download")),
            FetchOptions::for_features(&pdf_only),
        )
        .await
        .unwrap();
    assert_eq!(BASE64.decode(page.html).unwrap(), body);

    // Nothing enabled: rejected as before.
    assert!(fetcher()
        .fetch(&CrawlUrl::seed(url(&server, "/download")))
        .await
        .is_err());
}

#[tokio::test]
async fn document_size_cap_is_enforced_before_buffering() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/big.xlsx",
        "application/vnd.ms-excel",
        vec![0u8; 2 * 1024 * 1024],
    )
    .await;
    let err = fetcher()
        .fetch_with_options(
            &CrawlUrl::seed(url(&server, "/big.xlsx")),
            FetchOptions::for_features(&documents_features(Some(1))),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("too large"), "{err}");
}

#[tokio::test]
async fn html_is_unchanged_with_documents_enabled() {
    let server = MockServer::start().await;
    serve(&server, "/p", "text/html", b"<p>ok</p>".to_vec()).await;
    let page = fetcher()
        .fetch_with_options(
            &CrawlUrl::seed(url(&server, "/p")),
            FetchOptions::with_all_documents(None),
        )
        .await
        .unwrap();
    assert_eq!(page.html, "<p>ok</p>");
}
