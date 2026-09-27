# Scrapix

High-performance, distributed web crawler and search indexer built in Rust.

## Vision

Scrapix aims to be an internet-scale web crawler capable of:

1. **Global Internet Indexing** - Crawl billions of pages
2. **Targeted Site Crawling** - Index specific websites/documentation
3. **Real-time Information Retrieval** - On-demand crawling

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                         API Layer                                │
│                     (scrapix-api)                                │
└─────────────────────────────────────────────────────────────────┘
                              │
        ┌─────────────────────┼─────────────────────┐
        ▼                     ▼                     ▼
┌───────────────┐   ┌───────────────┐   ┌───────────────┐
│   Frontier    │   │   Crawler     │   │   Content     │
│   Service     │   │   Workers     │   │   Workers     │
└───────────────┘   └───────────────┘   └───────────────┘
        │                     │                     │
        └─────────────────────┼─────────────────────┘
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                        Data Layer                               │
│  Redpanda │ RocksDB │ Meilisearch │ DragonflyDB │ S3            │
└─────────────────────────────────────────────────────────────────┘
```

## Tech Stack

| Component | Technology |
|-----------|------------|
| Language | Rust |
| Message Queue | Redpanda (Kafka-compatible) |
| Search | Meilisearch |
| Local State | RocksDB |
| Cache | DragonflyDB (Redis-compatible) |
| Object Storage | S3/MinIO/RustFS |
| Documents | pdf-inspector (PDF), anydoc (Word/Excel/PowerPoint/OpenDocument/RTF/EPUB/CSV) |
| OCR | PDFium rasterization + vision LLM or Tesseract (opt-in) |

## Quick Start

### Prerequisites

- Rust 1.75+
- Docker & Docker Compose

### 1. Start Infrastructure

```bash
# Start all infrastructure (Redpanda, Meilisearch, DragonflyDB)
docker compose -f docker-compose.yml -f docker-compose.dev.yml up -d
```

### 2. Build the Project

```bash
cargo build --release
```

### 3. Run Services

In separate terminals:

```bash
# Terminal 1: API Server
KAFKA_BROKERS=localhost:19092 \
MEILISEARCH_URL=http://localhost:7700 \
MEILISEARCH_API_KEY=masterKey \
cargo run --release --bin scrapix-api

# Terminal 2: Frontier Service
KAFKA_BROKERS=localhost:19092 \
cargo run --release --bin scrapix-frontier-service

# Terminal 3: Crawler Worker
KAFKA_BROKERS=localhost:19092 \
cargo run --release --bin scrapix-worker-crawler

# Terminal 4: Content Worker
KAFKA_BROKERS=localhost:19092 \
MEILISEARCH_URL=http://localhost:7700 \
MEILISEARCH_API_KEY=masterKey \
cargo run --release --bin scrapix-worker-content
```

### 4. Start a Crawl

```bash
# Using the CLI
cargo run --bin scrapix -- crawl -f examples/simple-crawl.json

# Or using curl
curl -X POST http://localhost:8080/crawl \
  -H "Content-Type: application/json" \
  -d @examples/simple-crawl.json
```

## Full Stack with Docker

Run everything in Docker:

```bash
# Build and start all services
docker compose up -d --build

# View logs
docker compose logs -f

# Stop all services
docker compose down
```

Services will be available at:
- API: http://localhost:8080
- Meilisearch: http://localhost:7700
- Redpanda Console: http://localhost:8090

## API Reference

### Start Crawl Job (Async)

```bash
POST /crawl
Content-Type: application/json

{
  "start_urls": ["https://example.com"],
  "index_uid": "my-index",
  "max_depth": 5,
  "features": {
    "markdown": { "enabled": true }
  }
}

# Response
{ "job_id": "550e8400-e29b-41d4-a716-446655440000" }
```

### Start Crawl Job (Sync)

```bash
POST /crawl/sync
Content-Type: application/json

# Same body as above, waits for completion
```

### Get Job Status

```bash
GET /job/{job_id}/status

# Response
{
  "job_id": "...",
  "status": "running",
  "pages_crawled": 150,
  "pages_indexed": 145
}
```

### Get Job Results

```bash
GET /job/{job_id}/results?limit=50&cursor=<next>

# Paginated documents from any job (crawl, batch scrape, extract),
# each shaped like a /scrape response
```

### Batch Scrape

```bash
POST /batch/scrape
Content-Type: application/json

{ "urls": ["https://a.com/x", "https://b.com/y"], "formats": ["markdown"] }

# Response: { "job_id": "..." } — read pages via /job/{job_id}/results
```

### Extract

```bash
POST /extract
Content-Type: application/json

{ "urls": ["https://example.com/blog/*"], "prompt": "Titles and authors of the posts" }

# Poll GET /extract/{job_id} for { "status", "data", "sources" }
```

### Stream Job Events (SSE)

```bash
GET /job/{job_id}/events
Accept: text/event-stream

# Returns server-sent events with crawl progress
```

### WebSocket Real-time Events

Connect to `/ws` for multi-job subscriptions or `/ws/job/{job_id}` for a single job.

**Client Messages:**
```json
{"type": "subscribe", "job_id": "..."}      // Subscribe to job
{"type": "unsubscribe", "job_id": "..."}    // Unsubscribe
{"type": "get_status", "job_id": "..."}     // Request status
{"type": "ping"}                            // Keepalive
```

**Server Messages:**
```json
{"type": "event", "job_id": "...", "event": {...}}   // Job event
{"type": "status", "job_id": "...", "status": {...}} // Status response
{"type": "subscribed", "job_id": "..."}              // Confirmed
{"type": "pong", "timestamp": 1234567890}            // Keepalive response
```

**Example (JavaScript):**
```javascript
const ws = new WebSocket('ws://localhost:8080/ws/job/' + jobId);
ws.onmessage = (e) => console.log(JSON.parse(e.data));
```

### Cancel Job

```bash
DELETE /job/{job_id}
```

Stops the frontier and every worker for that job and bills the pages crawled
so far. Only a pending, running or paused job can be cancelled; a terminal
job returns `409`.

### Pause / Resume Job

```bash
POST /job/{job_id}/pause   # Running -> Paused; 409 otherwise
POST /job/{job_id}/resume  # Paused -> Running; 409 otherwise
```

A paused job stops dispatching new URLs (pages already in flight finish, and
their discovered links keep being queued); it is never auto-completed or
failed as stalled while paused. Resuming restarts the stall-timeout clock.

### List Jobs

```bash
GET /jobs?limit=10&offset=0
```

### Health Check

```bash
GET /health
```

## SDKs

Official clients generated from [`contracts/openapi.json`](contracts/openapi.json), with retries and a job watcher (`crawl_and_wait` / `crawlAndWait`):

- Python: [`sdks/python`](sdks/python) (`pip install scrapix`)
- TypeScript: [`sdks/typescript`](sdks/typescript) (`npm install scrapix`)

After changing the spec, run `just sdk-generate` (CI fails on stale generated code via `just sdk-check`). See [`sdks/README.md`](sdks/README.md).

## CLI Usage

```bash
# Start a crawl job
scrapix crawl -f config.json
scrapix crawl --start-url https://example.com --index-uid my-index

# Start a sync crawl (wait for completion)
scrapix crawl -f config.json --sync

# Check job status
scrapix status <job_id>
scrapix status <job_id> --watch  # Poll continuously

# Stream job events
scrapix events <job_id>

# List recent jobs
scrapix jobs --limit 20

# Cancel a job
scrapix cancel <job_id>
```

## Configuration

See [examples/](examples/) for configuration examples:

- `simple-crawl.json` - Basic HTTP crawl
- `documentation-site.json` - Documentation with custom selectors
- `ecommerce-products.json` - Product catalog with schema extraction
- `ai-enrichment.json` - AI-powered content enrichment
- `with-proxy.json` - Crawling with proxy rotation

### Configuration Reference

```json
{
  "start_urls": ["https://example.com"],
  "index_uid": "my-index",
  "crawler_type": "http",
  "max_depth": 10,
  "max_pages": 1000,
  "url_patterns": {
    "include": ["https://example.com/**"],
    "exclude": ["**/private/**"],
    "index_only": ["**/docs/**"]
  },
  "sitemap": {
    "enabled": true,
    "urls": ["https://example.com/sitemap.xml"]
  },
  "concurrency": {
    "max_concurrent_requests": 20,
    "browser_pool_size": 5
  },
  "rate_limit": {
    "requests_per_second": 10,
    "per_domain_delay_ms": 100,
    "respect_robots_txt": true
  },
  "proxy": {
    "urls": ["http://proxy:8080"],
    "rotation": "round_robin"
  },
  "features": {
    "metadata": { "enabled": true },
    "markdown": { "enabled": true },
    "block_split": { "enabled": true },
    "schema": {
      "enabled": true,
      "only_types": ["Product", "Article"]
    },
    "custom_selectors": {
      "enabled": true,
      "selectors": {
        "title": "h1",
        "price": ".product-price"
      }
    },
    "ai_extraction": {
      "enabled": true,
      "prompt": "Extract key information...",
      "model": "gpt-4"
    },
    "ai_summary": { "enabled": true },
    "pdf": { "enabled": true, "extract_links": true },
    "documents": { "enabled": true },
    "ocr": { "mode": "auto" },
    "embeddings": {
      "enabled": true,
      "model": "text-embedding-3-small"
    }
  },
  "meilisearch": {
    "url": "http://localhost:7700",
    "api_key": "masterKey",
    "batch_size": 100
  },
  "webhooks": [{
    "url": "https://hooks.example.com/crawl",
    "events": ["crawl_completed"],
    "enabled": true
  }]
}
```

## Kubernetes Deployment

### Local (Docker Desktop)

```bash
# Apply local overlay
kubectl apply -k deploy/kubernetes/overlays/local

# Access services
kubectl port-forward -n scrapix svc/scrapix-api 8080:8080
kubectl port-forward -n scrapix svc/meilisearch 7700:7700
```

### Production

```bash
# Apply production overlay
kubectl apply -k deploy/kubernetes/overlays/prod
```

## Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `HOST` | API server host | `0.0.0.0` |
| `PORT` | API server port | `8080` |
| `KAFKA_BROKERS` | Kafka/Redpanda brokers | `localhost:9092` |
| `KAFKA_GROUP_ID` | Consumer group ID | Service-specific |
| `MEILISEARCH_URL` | Meilisearch URL | `http://localhost:7700` |
| `MEILISEARCH_API_KEY` | Meilisearch API key | - |
| `REDIS_URL` | Redis/DragonflyDB URL | `redis://localhost:6379` |
| `RUST_LOG` | Log level | `info` |
| `CONCURRENCY` | Crawler concurrency | `10` |
| `USER_AGENT` | HTTP User-Agent | Scrapix default |
| `REQUEST_TIMEOUT` | Request timeout (seconds) | `30` |
| `MAX_RETRIES` | Max retry attempts | `3` |
| `MAX_DEPTH` | Max crawl depth | `100` |
| `RESPECT_ROBOTS` | Respect robots.txt | `true` |
| `OPENAI_API_KEY` | OpenAI API key (for AI features) | - |
| `OCR_BACKEND` | OCR backend: `auto`, `vision`, `tesseract`, `off` | `auto` |
| `PDFIUM_LIB_PATH` | PDFium library for OCR page rasterization | system loader |
| `DOCUMENT_MAX_SIZE_MB` | Max document size for `/scrape` and `/parse` | `50` |

## Near-Duplicate Detection

Scrapix includes advanced near-duplicate detection using locality-sensitive hashing:

### SimHash (64-bit fingerprints)

Fast content fingerprinting using weighted token hashing:

```rust
use scrapix_frontier::SimHash;

let simhash = SimHash::new();
let hash1 = simhash.hash(content1);
let hash2 = simhash.hash(content2);

// Hamming distance < 10 indicates near-duplicate
let distance = SimHash::hamming_distance(hash1, hash2);
```

### MinHash (Jaccard similarity)

Accurate similarity estimation using multiple hash functions:

```rust
use scrapix_frontier::MinHash;

let minhash = MinHash::new(128); // 128 hash functions
let sig1 = minhash.signature(content1);
let sig2 = minhash.signature(content2);

// Returns similarity estimate 0.0-1.0
let similarity = MinHash::jaccard_similarity(&sig1, &sig2);
```

### NearDuplicateDetector

Combines both methods with LSH buckets for efficient detection:

```rust
use scrapix_frontier::{NearDuplicateDetector, NearDuplicateConfig};

let detector = NearDuplicateDetector::new(NearDuplicateConfig {
    use_simhash: true,
    simhash_threshold: 10,  // Max Hamming distance
    use_minhash: true,
    minhash_threshold: 0.8, // Min Jaccard similarity
    ..Default::default()
});

// Returns Some(canonical_url) if near-duplicate found
if let Some(original) = detector.check_and_add(url, content) {
    println!("Duplicate of: {}", original);
}
```

## Monitoring Stack

Scrapix includes a complete monitoring stack with Prometheus and Grafana.

### Quick Start

```bash
# Start monitoring services
cd deploy/monitoring
docker compose up -d

# Access dashboards
# Grafana: http://localhost:3000 (admin/admin)
# Prometheus: http://localhost:9090
# Alertmanager: http://localhost:9093
```

### Prometheus Metrics

`crates/scrapix-core/src/metrics.rs` registers the metrics every service
exposes at `GET /metrics` — on the API's HTTP port (8080) and on each
worker's `WAKE_PORT` (8081 by default):

| Metric | Type | Description |
|--------|------|-------------|
| `scrapix_crawler_fetches_total{outcome}` | Counter | Fetches by outcome (`crawled`/`not_modified`/`retry`/`failed`) |
| `scrapix_crawler_fetch_duration_seconds` | Histogram | Wall time of one fetch attempt |
| `scrapix_crawler_bytes_total` | Counter | Bytes downloaded |
| `scrapix_frontier_admissions_total{result}` | Counter | Admission attempts (`admitted`/`duplicate`/`error`) |
| `scrapix_frontier_queued{job}` | Gauge | Queued URL count, top 50 jobs by size |
| `scrapix_frontier_dispatched_total` | Counter | URLs dispatched to the crawl topic |
| `scrapix_content_documents_total{outcome}` | Counter | Pages handled (`success`/`failure`/`skipped`/`duplicate`) |
| `scrapix_content_flush_duration_seconds` | Histogram | Time to flush one storage backend |
| `scrapix_api_jobs{status}` | Gauge | In-memory job count by status |
| `scrapix_consumer_uncommitted{topic}` | Gauge | In-flight (unacked) messages per Kafka topic |

See [Monitoring](docs/operations/monitoring.mdx) for scrape config.

### Grafana Dashboards

Pre-configured dashboard in `deploy/monitoring/grafana/dashboards/scrapix-overview.json`:

- **Scrapix Overview** - crawl/download/error rate, queue size, fetch latency
  percentiles, crawler fetches and content documents by outcome, jobs by
  status, consumer in-flight message count

### Alerting Rules

Configured alerts in `deploy/monitoring/prometheus/alerts.yml` (see
[Monitoring](docs/operations/monitoring.mdx) for the full list and the
metrics behind each one):

- **HighCrawlerErrorRate** - fetch failure rate > 10% for 5 minutes
- **LowCrawlerThroughput** - fetch rate < 1/s for 15 minutes
- **QueueBacklogHigh** - total queued URLs > 100k for 15 minutes
- **FrontierNotDispatching** - admitting URLs but dispatching none for 10 minutes
- **CrawlerWorkerDown** / **ContentWorkerDown** - worker not responding to scrapes
- **RunningJobsPilingUp** - more than 50 jobs stuck `Running` for 30+ minutes

## Analytics Storage (ClickHouse)

For large-scale analytics, Scrapix supports ClickHouse:

```rust
use scrapix_storage::{ClickHouseClient, ClickHouseConfig};

let client = ClickHouseClient::new(ClickHouseConfig {
    url: "http://localhost:8123".to_string(),
    database: "scrapix".to_string(),
    ..Default::default()
}).await?;

// Query domain statistics
let stats = client.get_domain_stats("example.com", Some(30)).await?;

// Query hourly aggregates
let hourly = client.get_hourly_stats(24).await?;
```

### Event Tables

- `crawl_events` - Every page fetch with status, latency, size
- `content_events` - Processed content with word counts, language

## Project Structure

```
scrapix/
├── crates/                    # Library crates
│   ├── scrapix-core/          # Core types and traits
│   ├── scrapix-frontier/      # URL frontier with dedup
│   ├── scrapix-crawler/       # HTTP/browser fetching
│   ├── scrapix-parser/        # HTML parsing
│   ├── scrapix-extractor/     # Feature extraction
│   ├── scrapix-ai/            # AI enrichment
│   ├── scrapix-storage/       # Storage backends
│   ├── scrapix-queue/         # Message queue
│   └── scrapix-telemetry/     # Observability
│
├── bins/                      # Binary crates
│   ├── scrapix-api/           # REST API server
│   ├── scrapix-worker-crawler/# Crawler worker
│   ├── scrapix-worker-content/# Content processor
│   ├── scrapix-frontier-service/# Frontier service
│   └── scrapix-cli/           # CLI tool
│
├── deploy/                    # Deployment configs
│   ├── kubernetes/            # K8s manifests
│   │   ├── base/              # Base resources
│   │   └── overlays/          # Environment overrides
│   │       ├── local/         # Local development
│   │       └── prod/          # Production
│   └── monitoring/            # Prometheus/Grafana stack
│       ├── docker-compose.yml
│       ├── prometheus/        # Prometheus config + alerts
│       └── grafana/           # Dashboards + datasources
│
├── tests/                     # Integration tests
├── examples/                  # Example configurations
├── ARCHITECTURE.md            # Detailed architecture docs
└── docker-compose.yml         # Docker Compose stack
```

## Documentation

- [Architecture](ARCHITECTURE.md) - System design and tech decisions

## Contributing

1. Fork the repository
2. Create a feature branch (`git checkout -b feature/amazing-feature`)
3. Commit your changes (`git commit -m 'Add amazing feature'`)
4. Push to the branch (`git push origin feature/amazing-feature`)
5. Open a Pull Request

## License

MIT
