//! OpenAPI specification for the Scrapix crawl engine.
//!
//! Generates an OpenAPI 3.1 spec from annotated handlers and types, served at
//! `/openapi.json` with a Scalar UI at `/docs`.
//!
//! This spec covers the engine surface only. The SaaS control plane (auth,
//! account/team, configs/engines CRUD, billing, analytics pipes, OAuth,
//! MCP) is served by the Rails app; the frozen full-platform public spec is
//! `contracts/openapi.json`, and the engine-only snapshot pinned by
//! `tests/openapi_snapshot.rs` is `contracts/openapi.engine.json`.

use utoipa::OpenApi;

/// Scrapix API — OpenAPI specification
#[derive(OpenApi)]
#[openapi(
    info(
        title = "Scrapix API",
        version = "0.1.0",
        description = "High-performance web crawler and search indexer API. Scrape pages, map websites, run distributed crawls, and search indexed content.",
        contact(name = "Meilisearch", url = "https://scrapix.meilisearch.com"),
        license(name = "MIT")
    ),
    servers(
        (url = "https://scrapix.meilisearch.dev", description = "Production"),
        (url = "http://localhost:8080", description = "Local development")
    ),
    tags(
        (name = "health", description = "Health and diagnostics"),
        (name = "scrape", description = "Single-page scraping"),
        (name = "map", description = "Website URL discovery"),
        (name = "search", description = "Search indexed content"),
        (name = "crawl", description = "Distributed crawl jobs"),
        (name = "jobs", description = "Job management")
    ),
    paths(
        // Health & diagnostics
        crate::health,
        crate::health_services,
        crate::handle_stats,
        crate::handle_errors,
        crate::handle_domains,
        // Core endpoints
        crate::scrape_url,
        crate::map_url,
        crate::search_url,
        crate::create_crawl,
        crate::create_crawl_sync,
        crate::create_crawl_bulk,
        // Job management
        crate::list_jobs,
        crate::job_status,
        crate::cancel_job,
    ),
    components(schemas(
        // Core API types
        crate::HealthResponse,
        crate::ServiceHealthResponse,
        crate::ServiceStatus,
        crate::ScrapeRequest,
        crate::ScrapeResponse,
        crate::ScrapeFormat,
        crate::ScrapeMetadata,
        crate::AiOptions,
        crate::AiExtractOptions,
        crate::AiFieldDef,
        crate::AiResult,
        crate::MapRequest,
        crate::MapResponse,
        crate::MapLink,
        crate::SearchRequest,
        crate::CreateCrawlResponse,
        crate::BulkCrawlResponse,
        crate::BulkCrawlError,
        crate::JobStatusResponse,
        crate::ApiError,
        // Diagnostic types
        crate::SystemStatsResponse,
        crate::MeilisearchStats,
        crate::JobSummary,
        crate::DiagnosticsStats,
        crate::ErrorsResponse,
        crate::ErrorRecord,
        crate::DomainsResponse,
        crate::DomainInfo,
    )),
    security(
        ("api_key" = [])
    ),
    modifiers(&SecurityAddon)
)]
pub struct ScrapixApi;

/// Adds the API key security scheme to the OpenAPI spec.
struct SecurityAddon;

impl utoipa::Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        if let Some(components) = openapi.components.as_mut() {
            components.add_security_scheme(
                "api_key",
                utoipa::openapi::security::SecurityScheme::ApiKey(
                    utoipa::openapi::security::ApiKey::Header(
                        utoipa::openapi::security::ApiKeyValue::new("x-api-key"),
                    ),
                ),
            );
        }
    }
}
