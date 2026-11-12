//! Producer linger benchmark: how `linger.ms` trades the latency of an
//! awaited send against batching, with the real `KafkaProducer`
//! (`acks=all`, idempotent, lz4). It backs `DEFAULT_LINGER_MS`.
//!
//! Redpanda's container image always runs with `--unsafe-bypass-fsync`,
//! so broker acks here do not include a disk flush.
//!
//! ```bash
//! docker run -d --rm --name scrapix-bench-redpanda -p 19093:19093 -p 19644:9644 \
//!   docker.redpanda.com/redpandadata/redpanda:latest redpanda start \
//!   --mode dev-container --smp 1 \
//!   --kafka-addr internal://0.0.0.0:9092,external://0.0.0.0:19093 \
//!   --advertise-kafka-addr internal://127.0.0.1:9092,external://localhost:19093
//!
//! SCRAPIX_BENCH_KAFKA_BROKERS=localhost:19093 \
//! SCRAPIX_BENCH_REDPANDA_METRICS=localhost:19644 \
//!   cargo run --release -p scrapix-queue --example linger_bench
//! ```
//!
//! Without `SCRAPIX_BENCH_KAFKA_BROKERS` it prints a note and exits.
//! `SCRAPIX_BENCH_REDPANDA_METRICS` (the broker's admin address) is
//! optional: with it the bench reads the broker's produce-request counter
//! around each phase. Phases: (a) one awaited send at a time, (b) 100 sends
//! awaited together, 50 rounds, (c) 20,000 messages with 256 in flight,
//! (d) 64 closed-loop slots of 150 awaited sends each, (e) 2,000 msg/s
//! open loop for 3 s. `SCRAPIX_BENCH_LINGERS` (default `0,1,5,10,20`) and
//! `SCRAPIX_BENCH_TOPIC` override the lingers and the topic.

use std::sync::Arc;
use std::time::{Duration, Instant};

use scrapix_queue::{KafkaProducer, ProducerBuilder};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

const SINGLE_SENDS: usize = 200;
const BURST_SIZE: usize = 100;
const BURST_ROUNDS: usize = 50;
const SUSTAINED_MESSAGES: usize = 20_000;
const SUSTAINED_IN_FLIGHT: usize = 256;
const LOOP_WORKERS: usize = 64;
const LOOP_SENDS_PER_WORKER: usize = 150;
const PACED_RATE_PER_SEC: usize = 2_000;
const PACED_SECONDS: usize = 3;

/// A ~1.5 KB JSON message, shaped like a discovered-URL message.
fn payload(i: usize) -> Vec<u8> {
    let body = serde_json::json!({
        "job_id": "0190f2a4-bench-7000-8000-000000000000",
        "index_uid": "bench",
        "account_id": "acct_bench",
        "url": format!("https://docs.example.com/guides/section-{}/page-{i}.html", i % 97),
        "parent_url": "https://docs.example.com/guides/index.html",
        "depth": 3,
        "priority": 0.5,
        "anchor_text": "A link text of an ordinary length, as found on a documentation page",
        "headers": {"accept-language": "en-US,en;q=0.9", "x-request": i},
        // Not compressible to nothing: varies per message.
        "padding": (0..36).map(|k| format!("{:08x}{:08x}{:08x}", i * 31 + k, k * 7919 + i, i ^ (k << 9))).collect::<Vec<_>>(),
    });
    serde_json::to_vec(&body).expect("json")
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx]
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// The broker's count of produce requests, from Redpanda's public metrics.
async fn produce_requests(admin: Option<&str>) -> Option<f64> {
    let admin = admin?;
    let mut stream = tokio::net::TcpStream::connect(admin).await.ok()?;
    let request =
        format!("GET /public_metrics HTTP/1.1\r\nHost: {admin}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.ok()?;
    let mut body = String::new();
    stream.read_to_string(&mut body).await.ok()?;
    body.lines()
        .find(|l| {
            l.starts_with("redpanda_kafka_request_latency_seconds_count")
                && l.contains("redpanda_request=\"produce\"")
        })
        .and_then(|l| l.rsplit(' ').next())
        .and_then(|v| v.parse().ok())
}

fn requests_between(before: Option<f64>, after: Option<f64>) -> String {
    match (before, after) {
        (Some(b), Some(a)) => format!("{:.0}", a - b),
        _ => "n/a".to_string(),
    }
}

async fn send_many(
    producer: &Arc<KafkaProducer>,
    topic: &str,
    count: usize,
    in_flight: usize,
) -> Duration {
    let permits = Arc::new(Semaphore::new(in_flight));
    let mut tasks = JoinSet::new();
    let started = Instant::now();
    for i in 0..count {
        let permit = permits.clone().acquire_owned().await.expect("semaphore");
        let producer = producer.clone();
        let topic = topic.to_string();
        tasks.spawn(async move {
            let key = format!("host-{}.example.com", i % 16);
            producer
                .send_raw(&topic, Some(&key), &payload(i), None)
                .await
                .expect("send");
            drop(permit);
        });
    }
    while let Some(done) = tasks.join_next().await {
        done.expect("task");
    }
    started.elapsed()
}

/// Closed loop: each worker awaits one send before the next, like a
/// crawler slot awaiting a publish stage. Throughput is workers / latency.
async fn closed_loop(producer: &Arc<KafkaProducer>, topic: &str) -> Duration {
    let mut tasks = JoinSet::new();
    let started = Instant::now();
    for w in 0..LOOP_WORKERS {
        let producer = producer.clone();
        let topic = topic.to_string();
        tasks.spawn(async move {
            let key = format!("host-{}.example.com", w % 16);
            for i in 0..LOOP_SENDS_PER_WORKER {
                producer
                    .send_raw(&topic, Some(&key), &payload(w * 1000 + i), None)
                    .await
                    .expect("send");
            }
        });
    }
    while let Some(done) = tasks.join_next().await {
        done.expect("task");
    }
    started.elapsed()
}

/// Open loop at a moderate, fixed rate (2 sends every millisecond): the
/// shape where a short linger batches least. Returns the mean send latency.
async fn paced(producer: &Arc<KafkaProducer>, topic: &str) -> Duration {
    let mut tasks = JoinSet::new();
    let mut tick = tokio::time::interval(Duration::from_millis(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
    let per_tick = PACED_RATE_PER_SEC / 1000;
    for t in 0..PACED_SECONDS * 1000 {
        tick.tick().await;
        for k in 0..per_tick {
            let producer = producer.clone();
            let topic = topic.to_string();
            let i = t * per_tick + k;
            tasks.spawn(async move {
                let key = format!("host-{}.example.com", i % 16);
                let started = Instant::now();
                producer
                    .send_raw(&topic, Some(&key), &payload(i), None)
                    .await
                    .expect("send");
                started.elapsed()
            });
        }
    }
    let mut total = Duration::ZERO;
    let mut count = 0u32;
    while let Some(done) = tasks.join_next().await {
        total += done.expect("task");
        count += 1;
    }
    total / count
}

#[tokio::main]
async fn main() {
    let Ok(brokers) = std::env::var("SCRAPIX_BENCH_KAFKA_BROKERS") else {
        eprintln!("SCRAPIX_BENCH_KAFKA_BROKERS is not set: nothing to do (see the file header)");
        return;
    };
    let admin = std::env::var("SCRAPIX_BENCH_REDPANDA_METRICS").ok();
    let admin = admin.as_deref();
    let topic =
        std::env::var("SCRAPIX_BENCH_TOPIC").unwrap_or_else(|_| "scrapix.bench.linger".to_string());
    let lingers: Vec<u64> = std::env::var("SCRAPIX_BENCH_LINGERS")
        .unwrap_or_else(|_| "0,1,5,10,20".to_string())
        .split(',')
        .map(|v| v.trim().parse().expect("SCRAPIX_BENCH_LINGERS: integers"))
        .collect();

    println!(
        "linger_ms | single p50 ms | single p99 ms | burst100 mean ms | burst100 p99 ms | burst msg/s | burst reqs/round | sustained msg/s | sustained reqs | msgs/req | loop64 msg/s | loop64 msgs/req | paced2k mean ms | paced2k req/s"
    );
    for linger in lingers {
        let producer = Arc::new(
            ProducerBuilder::new(&brokers)
                .client_id(format!("scrapix-bench-linger-{linger}"))
                .compression("lz4")
                .linger_ms(linger)
                .build()
                .expect("producer"),
        );
        // Warm up: connection, metadata, idempotent producer id.
        for i in 0..20 {
            producer
                .send_raw(&topic, Some("warmup"), &payload(i), None)
                .await
                .expect("warmup send");
        }

        // (a) one awaited send at a time.
        let mut single = Vec::with_capacity(SINGLE_SENDS);
        for i in 0..SINGLE_SENDS {
            let started = Instant::now();
            producer
                .send_raw(&topic, Some("host-0.example.com"), &payload(i), None)
                .await
                .expect("send");
            single.push(started.elapsed());
        }
        single.sort();

        // (b) a page with 100 links: 100 sends awaited together.
        let before = produce_requests(admin).await;
        let mut bursts = Vec::with_capacity(BURST_ROUNDS);
        for _ in 0..BURST_ROUNDS {
            bursts.push(send_many(&producer, &topic, BURST_SIZE, BURST_SIZE).await);
        }
        let after = produce_requests(admin).await;
        let burst_total: Duration = bursts.iter().sum();
        bursts.sort();
        let burst_reqs = match (before, after) {
            (Some(b), Some(a)) => format!("{:.1}", (a - b) / BURST_ROUNDS as f64),
            _ => "n/a".to_string(),
        };

        // (c) sustained load.
        let before = produce_requests(admin).await;
        let sustained = send_many(&producer, &topic, SUSTAINED_MESSAGES, SUSTAINED_IN_FLIGHT).await;
        let after = produce_requests(admin).await;
        let per_req = match (before, after) {
            (Some(b), Some(a)) if a > b => format!("{:.1}", SUSTAINED_MESSAGES as f64 / (a - b)),
            _ => "n/a".to_string(),
        };

        // (d) 64 slots, each awaiting its sends one at a time.
        let loop_before = produce_requests(admin).await;
        let looped = closed_loop(&producer, &topic).await;
        let loop_after = produce_requests(admin).await;
        let loop_total = LOOP_WORKERS * LOOP_SENDS_PER_WORKER;
        let loop_per_req = match (loop_before, loop_after) {
            (Some(b), Some(a)) if a > b => format!("{:.1}", loop_total as f64 / (a - b)),
            _ => "n/a".to_string(),
        };

        // (e) 2,000 msg/s, open loop.
        let paced_before = produce_requests(admin).await;
        let paced_latency = paced(&producer, &topic).await;
        let paced_after = produce_requests(admin).await;
        let paced_reqs = match (paced_before, paced_after) {
            (Some(b), Some(a)) => format!("{:.0}", (a - b) / PACED_SECONDS as f64),
            _ => "n/a".to_string(),
        };

        println!(
            "{linger:>9} | {:>13.2} | {:>13.2} | {:>16.2} | {:>15.2} | {:>11.0} | {:>16} | {:>15.0} | {:>14} | {:>8} | {:>12.0} | {:>15} | {:>15.2} | {:>13}",
            ms(percentile(&single, 0.50)),
            ms(percentile(&single, 0.99)),
            ms(burst_total) / BURST_ROUNDS as f64,
            ms(percentile(&bursts, 0.99)),
            (BURST_SIZE * BURST_ROUNDS) as f64 / burst_total.as_secs_f64(),
            burst_reqs,
            SUSTAINED_MESSAGES as f64 / sustained.as_secs_f64(),
            requests_between(before, after),
            per_req,
            loop_total as f64 / looped.as_secs_f64(),
            loop_per_req,
            ms(paced_latency),
            paced_reqs,
        );
        producer.flush(Duration::from_secs(5));
    }
}
