//! All-in-one orchestrator for running all services in a single process.
//!
//! Creates an in-process channel bus (or Kafka if `--kafka-brokers` is set),
//! then spawns API, Frontier, Crawler, and Content as concurrent tokio tasks.

use std::sync::Arc;

use clap::Parser;
use tracing::{error, info};

use scrapix_queue::{topic_names, AnyConsumer, AnyProducer, ChannelBus};

/// Arguments for running all services in a single process.
#[derive(Parser, Debug)]
#[command(name = "all")]
#[command(about = "Run all Scrapix services in a single process")]
pub struct AllArgs {
    // === API ===
    /// API server host
    #[arg(long, env = "HOST", default_value = "0.0.0.0")]
    pub host: String,

    /// API server port
    #[arg(long, env = "PORT", default_value = "8080")]
    pub port: u16,

    // === Meilisearch ===
    /// Meilisearch server URL (required unless every crawl passes `meilisearch.url`)
    #[arg(long, env = "MEILISEARCH_URL")]
    pub meilisearch_url: Option<String>,

    /// Meilisearch API key
    #[arg(long, env = "MEILISEARCH_API_KEY")]
    pub meilisearch_key: Option<String>,

    // === Workers ===
    /// Crawler concurrency (concurrent fetchers)
    #[arg(long, env = "CRAWLER_CONCURRENCY", default_value = "50")]
    pub crawler_concurrency: usize,

    /// Content worker concurrency
    #[arg(long, env = "CONTENT_CONCURRENCY", default_value = "10")]
    pub content_concurrency: usize,

    // === Optional Kafka ===
    /// Kafka/Redpanda brokers. If set, uses Kafka instead of in-process channels.
    #[arg(long, env = "KAFKA_BROKERS")]
    pub kafka_brokers: Option<String>,

    // === Database (optional) ===
    /// PostgreSQL database URL (for auth/cron features)
    #[arg(long, env = "DATABASE_URL")]
    pub database_url: Option<String>,

    /// JWT secret for auth (required when DATABASE_URL is set)
    #[arg(long, env = "JWT_SECRET")]
    pub jwt_secret: Option<String>,

    // === Browser rendering ===
    /// Enable browser rendering for JavaScript-heavy pages
    #[arg(long, env = "BROWSER_RENDER")]
    pub browser_render: bool,

    /// URL patterns that require browser rendering (regex, comma-separated)
    #[arg(long, env = "BROWSER_RENDER_PATTERNS")]
    pub browser_render_patterns: Option<String>,

    /// Chrome/Chromium executable path
    #[arg(long, env = "CHROME_PATH")]
    pub chrome_path: Option<String>,

    /// Browser rendering timeout in seconds
    #[arg(long, env = "BROWSER_TIMEOUT", default_value = "30")]
    pub browser_timeout: u64,

    /// Max concurrent browser instances
    #[arg(long, env = "BROWSER_CONCURRENCY", default_value = "5")]
    pub browser_concurrency: usize,

    /// Enable verbose logging
    #[arg(short, long)]
    pub verbose: bool,
}

pub async fn run_all(args: AllArgs) -> anyhow::Result<()> {
    if let Some(ref brokers) = args.kafka_brokers {
        info!(brokers = %brokers, "Running all services with Kafka message bus");
        run_all_kafka(&args, brokers).await
    } else {
        info!("Running all services with in-process channel bus");
        run_all_channels(&args).await
    }
}

async fn run_all_channels(args: &AllArgs) -> anyhow::Result<()> {
    let bus = ChannelBus::new();

    // Create producers (all services share the same bus)
    let api_producer = AnyProducer::channel(bus.producer());
    let frontier_producer = Arc::new(AnyProducer::channel(bus.producer()));
    let crawler_producer = AnyProducer::channel(bus.producer());
    let content_producer = Arc::new(AnyProducer::channel(bus.producer()));

    // Create consumers for each service
    // API event consumer
    let api_event_consumer = AnyConsumer::channel(bus.consumer());
    api_event_consumer.subscribe(&[topic_names::EVENTS])?;

    // Frontier main consumer (URL_FRONTIER)
    let frontier_consumer = Arc::new(AnyConsumer::channel(bus.consumer()));
    frontier_consumer.subscribe(&[topic_names::URL_FRONTIER])?;

    // Frontier fetch-feedback consumer (FETCH_FEEDBACK, politeness slots)
    let frontier_feedback = Arc::new(AnyConsumer::channel(bus.consumer()));
    frontier_feedback.subscribe(&[topic_names::FETCH_FEEDBACK])?;

    // Crawler consumer (URL_PROCESSING)
    let crawler_consumer = AnyConsumer::channel(bus.consumer());
    crawler_consumer.subscribe(&[topic_names::URL_PROCESSING])?;

    // Content consumer (PAGES_RAW)
    let content_consumer = Arc::new(AnyConsumer::channel(bus.consumer()));
    content_consumer.subscribe(&[topic_names::PAGES_RAW])?;

    // Job control (JOB_STATUS): one named group per service, so the
    // frontier and both workers each receive every cancel/pause/resume.
    let control_consumer = |group: &str| -> anyhow::Result<AnyConsumer> {
        let c = AnyConsumer::channel(bus.consumer_in_group(group));
        c.subscribe(&[topic_names::JOB_STATUS])?;
        Ok(c)
    };
    let frontier_control = Arc::new(control_consumer("frontier-control")?);
    let crawler_control = control_consumer("crawler-control")?;
    let content_control = Arc::new(control_consumer("content-control")?);

    // Build service-specific args
    let api_args = scrapix_api::Args {
        host: args.host.clone(),
        port: args.port,
        brokers: String::new(), // unused with channel bus
        database_url: args.database_url.clone(),
        jwt_secret: args.jwt_secret.clone(),
        mode: std::env::var("SCRAPIX_MODE").unwrap_or_else(|_| "standalone".into()),
        admin_key: std::env::var("SCRAPIX_ADMIN_KEY").ok(),
        auth: std::env::var("SCRAPIX_AUTH").ok(),
        meilisearch_url: args.meilisearch_url.clone(),
        meilisearch_api_key: args.meilisearch_key.clone(),
        stripe_secret_key: std::env::var("STRIPE_SECRET_KEY").ok(),
        max_jobs: 1000,
        job_stall_timeout_secs: env_or("JOB_STALL_TIMEOUT_SECS", 1800),
        completion_grace_ms: env_or("JOB_COMPLETION_GRACE_MS", 3000),
        resume_heal_after_secs: env_or("RESUME_HEAL_AFTER_SECS", 60),
        max_pending_acks: env_or("MAX_PENDING_ACKS", 50_000) as usize,
        allow_private_ips: std::env::var("ALLOW_PRIVATE_IPS")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false),
        webhook_max_concurrent_deliveries: env_or(
            "WEBHOOK_MAX_CONCURRENT_DELIVERIES",
            scrapix_api::webhooks::DEFAULT_MAX_CONCURRENT_DELIVERIES as u64,
        ) as usize,
        verbose: args.verbose,
    };

    let frontier_args = scrapix_frontier_service::Args {
        brokers: String::new(),
        group_id: "scrapix-frontier".to_string(),
        // Durable Redis frontier when REDIS_URL is set, in-memory otherwise.
        redis_url: std::env::var("REDIS_URL").ok(),
        frontier_key_prefix: "scrapix:frontier".to_string(),
        job_retention_hours: 168,
        bloom_capacity: 10_000_000,
        bloom_fp_rate: 0.01,
        domain_delay_ms: 250,
        concurrent_per_domain: 4,
        request_timeout_secs: 30,
        robots_delay_multiplier: 1.0,
        dispatch_batch_size: 2000,
        dispatch_interval_ms: 20,
        dispatch_shutdown_grace_ms: env_or("DISPATCH_SHUTDOWN_GRACE_MS", 10_000),
        max_pending_per_job: 1_000_000,
        instance_id: Some("all-in-one".to_string()),
        verbose: args.verbose,
        enable_linkgraph: false,
        linkgraph_damping: 0.85,
        linkgraph_max_boost: 50,
        linkgraph_max_pages: 10_000_000,
        linkgraph_compute_interval: 300,
        enable_recrawl: false,
        recrawl_min_age: 3600,
        recrawl_max_age: 604800,
        recrawl_max_urls: 10_000_000,
    };

    let crawler_args = scrapix_worker_crawler::Args {
        brokers: String::new(),
        group_id: "scrapix-crawlers".to_string(),
        concurrency: args.crawler_concurrency,
        user_agent: "Scrapix/1.0 (compatible; +https://github.com/quentindequelen/scrapix)"
            .to_string(),
        timeout: 30,
        max_retries: 3,
        follow_external: false,
        max_depth: 100,
        max_body_size_mb: 10,
        respect_robots: true,
        worker_id: Some("all-in-one-crawler".to_string()),
        dns_cache: true,
        dns_cache_ttl: 300,
        link_graph: false,
        link_graph_interval: 1000,
        publish_links: false,
        incremental_crawl: true,
        redis_url: std::env::var("REDIS_URL").ok(),
        browser_render: args.browser_render,
        browser_render_patterns: args.browser_render_patterns.clone(),
        chrome_path: args.chrome_path.clone(),
        browser_timeout: args.browser_timeout,
        browser_concurrency: args.browser_concurrency,
        browser_headless: true,
        verbose: args.verbose,
        sitemap_discovery: true,
        max_sitemap_urls: 10000,
        // Same opt-out env var as the standalone worker (off by default).
        allow_private_ips: std::env::var("ALLOW_PRIVATE_IPS")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false),
    };

    let content_args = build_content_args(args, String::new());

    info!("Spawning all services as concurrent tasks...");

    // Spawn all 4 services
    let api_handle = tokio::spawn(async move {
        if let Err(e) = scrapix_api::run_with_bus(api_args, api_producer, api_event_consumer).await
        {
            error!(error = %e, "API server failed");
        }
    });

    let frontier_store = scrapix_frontier_service::build_store(&frontier_args).await?;
    let frontier_handle = tokio::spawn(async move {
        if let Err(e) = scrapix_frontier_service::run_with_bus(
            frontier_args,
            frontier_producer,
            frontier_consumer,
            None, // links consumer
            None, // history consumer
            Some(frontier_feedback),
            Some(frontier_control),
            frontier_store,
        )
        .await
        {
            error!(error = %e, "Frontier service failed");
        }
    });

    let crawler_handle = tokio::spawn(async move {
        if let Err(e) = scrapix_worker_crawler::run_with_bus(
            crawler_args,
            crawler_producer,
            crawler_consumer,
            Some(crawler_control),
        )
        .await
        {
            error!(error = %e, "Crawler worker failed");
        }
    });

    let content_handle = tokio::spawn(async move {
        if let Err(e) = scrapix_worker_content::run_with_bus(
            content_args,
            content_consumer,
            content_producer,
            Some(content_control),
        )
        .await
        {
            error!(error = %e, "Content worker failed");
        }
    });

    info!(
        host = %args.host,
        port = args.port,
        crawler_concurrency = args.crawler_concurrency,
        content_concurrency = args.content_concurrency,
        "All services started. Press Ctrl+C to stop."
    );

    // Wait for ctrl+c
    tokio::signal::ctrl_c().await?;
    info!("Received shutdown signal, stopping all services...");

    // Abort all tasks (each service handles its own graceful shutdown internally)
    api_handle.abort();
    frontier_handle.abort();
    crawler_handle.abort();
    content_handle.abort();

    // Give services 5 seconds to clean up
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    info!("All services stopped.");
    Ok(())
}

fn build_content_args(args: &AllArgs, brokers: String) -> scrapix_worker_content::Args {
    scrapix_worker_content::Args {
        brokers,
        group_id: "scrapix-content".to_string(),
        concurrency: args.content_concurrency,
        meilisearch_url: args.meilisearch_url.clone().unwrap_or_default(),
        meilisearch_key: args.meilisearch_key.clone(),
        default_index: "scrapix".to_string(),
        extract_content: true,
        convert_markdown: true,
        detect_language: true,
        extract_schema: true,
        min_content_length: 100,
        publish_to_kafka: false,
        publish_history: false,
        skip_meilisearch: false,
        batch_size: 2000,
        worker_id: Some("all-in-one-content".to_string()),
        verbose: args.verbose,
        enable_summary: false,
        summary_model: "gpt-5-nano".to_string(),
        enable_extraction: false,
        extraction_prompt: None,
        extraction_model: "gpt-5-nano".to_string(),
        ai_max_tokens: 1000,
        ai_concurrency: 5,
        enable_block_split: false,
        block_split_min_level: 2,
        block_split_max_level: 4,
        block_split_min_length: 50,
        enable_dedup: false,
        dedup_use_simhash: true,
        dedup_simhash_threshold: 3,
        dedup_minhash_threshold: 0.85,
        dedup_max_fingerprints: 10_000_000,
    }
}

async fn run_all_kafka(args: &AllArgs, brokers: &str) -> anyhow::Result<()> {
    // When Kafka is specified, just build Kafka-backed producers/consumers
    use scrapix_queue::{ConsumerBuilder, ProducerBuilder};

    let api_producer: AnyProducer = ProducerBuilder::new(brokers)
        .client_id("scrapix-all-api")
        .compression("lz4")
        .build()?
        .into();

    let api_event_consumer: AnyConsumer = {
        let c = ConsumerBuilder::new(brokers, "scrapix-all-api-events")
            .client_id("scrapix-all-api-events")
            .auto_offset_reset("latest")
            .build()?;
        c.subscribe(&[topic_names::EVENTS])?;
        c.into()
    };

    let frontier_producer: Arc<AnyProducer> = Arc::new(
        ProducerBuilder::new(brokers)
            .client_id("scrapix-all-frontier")
            .compression("lz4")
            .build()?
            .into(),
    );

    let frontier_consumer: Arc<AnyConsumer> = Arc::new({
        let c = ConsumerBuilder::new(brokers, "scrapix-all-frontier")
            .client_id("scrapix-all-frontier")
            .auto_offset_reset("earliest")
            .build()?;
        c.subscribe(&[topic_names::URL_FRONTIER])?;
        AnyConsumer::from(c)
    });

    // One instance here: its feedback group is shared only with Redis
    // politeness (see `feedback_group_id`).
    let feedback_group = scrapix_frontier_service::feedback_group_id(
        "scrapix-all-frontier",
        "all-in-one",
        std::env::var("REDIS_URL").is_ok_and(|u| !u.is_empty()),
    );
    let frontier_feedback: Arc<AnyConsumer> = Arc::new({
        let c = ConsumerBuilder::new(brokers, &feedback_group)
            .client_id("scrapix-all-frontier-feedback")
            .auto_offset_reset("latest")
            .build()?;
        c.subscribe(&[topic_names::FETCH_FEEDBACK])?;
        AnyConsumer::from(c)
    });

    let crawler_producer: AnyProducer = ProducerBuilder::new(brokers)
        .client_id("scrapix-all-crawler")
        .compression("lz4")
        .build()?
        .into();

    let crawler_consumer: AnyConsumer = {
        let c = ConsumerBuilder::new(brokers, "scrapix-all-crawlers")
            .client_id("scrapix-all-crawler")
            .auto_offset_reset("earliest")
            .build()?;
        c.subscribe(&[topic_names::URL_PROCESSING])?;
        c.into()
    };

    let content_producer: Arc<AnyProducer> = Arc::new(
        ProducerBuilder::new(brokers)
            .client_id("scrapix-all-content")
            .compression("lz4")
            .build()?
            .into(),
    );

    let content_consumer: Arc<AnyConsumer> = Arc::new({
        let c = ConsumerBuilder::new(brokers, "scrapix-all-content")
            .client_id("scrapix-all-content")
            .auto_offset_reset("earliest")
            .build()?;
        c.subscribe(&[topic_names::PAGES_RAW])?;
        AnyConsumer::from(c)
    });

    // Job control (JOB_STATUS): one group per service (each sees every
    // control), `latest` so a new group does not replay the history.
    let control_consumer = |service: &str| -> anyhow::Result<AnyConsumer> {
        let group = scrapix_queue::control_group_id(service, "all-in-one");
        let c = ConsumerBuilder::new(brokers, &group)
            .client_id(format!("{service}-control"))
            .auto_offset_reset("latest")
            .build()?;
        c.subscribe(&[topic_names::JOB_STATUS])?;
        Ok(AnyConsumer::from(c))
    };
    let frontier_control = Arc::new(control_consumer("scrapix-all-frontier")?);
    let crawler_control = control_consumer("scrapix-all-crawlers")?;
    let content_control = Arc::new(control_consumer("scrapix-all-content")?);

    // Build the same args as channel mode
    let api_args = scrapix_api::Args {
        host: args.host.clone(),
        port: args.port,
        brokers: brokers.to_string(),
        database_url: args.database_url.clone(),
        jwt_secret: args.jwt_secret.clone(),
        mode: std::env::var("SCRAPIX_MODE").unwrap_or_else(|_| "standalone".into()),
        admin_key: std::env::var("SCRAPIX_ADMIN_KEY").ok(),
        auth: std::env::var("SCRAPIX_AUTH").ok(),
        meilisearch_url: args.meilisearch_url.clone(),
        meilisearch_api_key: args.meilisearch_key.clone(),
        stripe_secret_key: std::env::var("STRIPE_SECRET_KEY").ok(),
        max_jobs: 1000,
        job_stall_timeout_secs: env_or("JOB_STALL_TIMEOUT_SECS", 1800),
        completion_grace_ms: env_or("JOB_COMPLETION_GRACE_MS", 3000),
        resume_heal_after_secs: env_or("RESUME_HEAL_AFTER_SECS", 60),
        max_pending_acks: env_or("MAX_PENDING_ACKS", 50_000) as usize,
        allow_private_ips: std::env::var("ALLOW_PRIVATE_IPS")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false),
        webhook_max_concurrent_deliveries: env_or(
            "WEBHOOK_MAX_CONCURRENT_DELIVERIES",
            scrapix_api::webhooks::DEFAULT_MAX_CONCURRENT_DELIVERIES as u64,
        ) as usize,
        verbose: args.verbose,
    };

    let frontier_args = scrapix_frontier_service::Args {
        brokers: brokers.to_string(),
        group_id: "scrapix-all-frontier".to_string(),
        // Durable Redis frontier when REDIS_URL is set, in-memory otherwise.
        redis_url: std::env::var("REDIS_URL").ok(),
        frontier_key_prefix: "scrapix:frontier".to_string(),
        job_retention_hours: 168,
        bloom_capacity: 10_000_000,
        bloom_fp_rate: 0.01,
        domain_delay_ms: 250,
        concurrent_per_domain: 4,
        request_timeout_secs: 30,
        robots_delay_multiplier: 1.0,
        dispatch_batch_size: 2000,
        dispatch_interval_ms: 20,
        dispatch_shutdown_grace_ms: env_or("DISPATCH_SHUTDOWN_GRACE_MS", 10_000),
        max_pending_per_job: 1_000_000,
        instance_id: Some("all-in-one".to_string()),
        verbose: args.verbose,
        enable_linkgraph: false,
        linkgraph_damping: 0.85,
        linkgraph_max_boost: 50,
        linkgraph_max_pages: 10_000_000,
        linkgraph_compute_interval: 300,
        enable_recrawl: false,
        recrawl_min_age: 3600,
        recrawl_max_age: 604800,
        recrawl_max_urls: 10_000_000,
    };

    let crawler_args = scrapix_worker_crawler::Args {
        brokers: brokers.to_string(),
        group_id: "scrapix-all-crawlers".to_string(),
        concurrency: args.crawler_concurrency,
        user_agent: "Scrapix/1.0 (compatible; +https://github.com/quentindequelen/scrapix)"
            .to_string(),
        timeout: 30,
        max_retries: 3,
        follow_external: false,
        max_depth: 100,
        max_body_size_mb: 10,
        respect_robots: true,
        worker_id: Some("all-in-one-crawler".to_string()),
        dns_cache: true,
        dns_cache_ttl: 300,
        link_graph: false,
        link_graph_interval: 1000,
        publish_links: false,
        incremental_crawl: true,
        redis_url: std::env::var("REDIS_URL").ok(),
        browser_render: args.browser_render,
        browser_render_patterns: args.browser_render_patterns.clone(),
        chrome_path: args.chrome_path.clone(),
        browser_timeout: args.browser_timeout,
        browser_concurrency: args.browser_concurrency,
        browser_headless: true,
        verbose: args.verbose,
        sitemap_discovery: true,
        max_sitemap_urls: 10000,
        // Same opt-out env var as the standalone worker (off by default).
        allow_private_ips: std::env::var("ALLOW_PRIVATE_IPS")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false),
    };

    let content_args = build_content_args(args, brokers.to_string());

    info!("Spawning all services with Kafka bus...");

    let api_handle = tokio::spawn(async move {
        if let Err(e) = scrapix_api::run_with_bus(api_args, api_producer, api_event_consumer).await
        {
            error!(error = %e, "API server failed");
        }
    });

    let frontier_store = scrapix_frontier_service::build_store(&frontier_args).await?;
    let frontier_handle = tokio::spawn(async move {
        if let Err(e) = scrapix_frontier_service::run_with_bus(
            frontier_args,
            frontier_producer,
            frontier_consumer,
            None,
            None,
            Some(frontier_feedback),
            Some(frontier_control),
            frontier_store,
        )
        .await
        {
            error!(error = %e, "Frontier service failed");
        }
    });

    let crawler_handle = tokio::spawn(async move {
        if let Err(e) = scrapix_worker_crawler::run_with_bus(
            crawler_args,
            crawler_producer,
            crawler_consumer,
            Some(crawler_control),
        )
        .await
        {
            error!(error = %e, "Crawler worker failed");
        }
    });

    let content_handle = tokio::spawn(async move {
        if let Err(e) = scrapix_worker_content::run_with_bus(
            content_args,
            content_consumer,
            content_producer,
            Some(content_control),
        )
        .await
        {
            error!(error = %e, "Content worker failed");
        }
    });

    info!(
        host = %args.host,
        port = args.port,
        brokers = %brokers,
        "All services started with Kafka. Press Ctrl+C to stop."
    );

    tokio::signal::ctrl_c().await?;
    info!("Received shutdown signal, stopping all services...");

    api_handle.abort();
    frontier_handle.abort();
    crawler_handle.abort();
    content_handle.abort();

    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    info!("All services stopped.");
    Ok(())
}

/// Parse an optional numeric env var, falling back to `default`.
fn env_or(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
