//! Process-wide Prometheus metrics registry shared by every Scrapix binary.
//!
//! Every collector below is a named accessor function backed by a
//! `std::sync::OnceLock`: the first caller (whichever binary/task gets there
//! first) constructs and registers it against the single process-wide
//! [`registry`]; every later call — including from a different service
//! running in the same process (`scrapix all`) — returns the same instance.
//! This makes "don't double-register" automatic rather than something each
//! call site has to remember.
//!
//! Metric names are a contract with `qdq-server/monitoring` — do not rename
//! without updating the scrape config there.

use std::collections::HashSet;
use std::sync::OnceLock;

use prometheus::{Counter, CounterVec, GaugeVec, Histogram, HistogramOpts, Opts, Registry};

/// The single process-wide registry. All collectors in this module register
/// against it exactly once (via `OnceLock`).
pub fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(Registry::new)
}

/// Render every registered metric in Prometheus text format 0.0.4.
pub fn encode() -> String {
    use prometheus::Encoder;
    let metric_families = registry().gather();
    let encoder = prometheus::TextEncoder::new();
    let mut buf = Vec::new();
    if let Err(e) = encoder.encode(&metric_families, &mut buf) {
        tracing::warn!(error = %e, "failed to encode Prometheus metrics");
    }
    String::from_utf8(buf).unwrap_or_default()
}

/// The Prometheus text-format content type served by `/metrics` endpoints.
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4";

fn register_counter_vec(name: &'static str, help: &'static str, labels: &[&str]) -> CounterVec {
    let cv = CounterVec::new(Opts::new(name, help), labels)
        .expect("static metric definition must be valid");
    if let Err(e) = registry().register(Box::new(cv.clone())) {
        tracing::warn!(error = %e, metric = name, "failed to register counter vec");
    }
    cv
}

fn register_counter(name: &'static str, help: &'static str) -> Counter {
    let c = Counter::new(name, help).expect("static metric definition must be valid");
    if let Err(e) = registry().register(Box::new(c.clone())) {
        tracing::warn!(error = %e, metric = name, "failed to register counter");
    }
    c
}

fn register_gauge_vec(name: &'static str, help: &'static str, labels: &[&str]) -> GaugeVec {
    let gv = GaugeVec::new(Opts::new(name, help), labels)
        .expect("static metric definition must be valid");
    if let Err(e) = registry().register(Box::new(gv.clone())) {
        tracing::warn!(error = %e, metric = name, "failed to register gauge vec");
    }
    gv
}

fn register_histogram(name: &'static str, help: &'static str) -> Histogram {
    let h = Histogram::with_opts(HistogramOpts::new(name, help))
        .expect("static metric definition must be valid");
    if let Err(e) = registry().register(Box::new(h.clone())) {
        tracing::warn!(error = %e, metric = name, "failed to register histogram");
    }
    h
}

/// `scrapix_crawler_fetches_total{outcome}` — one fetch outcome per handled
/// message (`crawled`, `not_modified`, `retry`, `failed`).
pub fn crawler_fetches_total() -> &'static CounterVec {
    static METRIC: OnceLock<CounterVec> = OnceLock::new();
    METRIC.get_or_init(|| {
        register_counter_vec(
            "scrapix_crawler_fetches_total",
            "Total URL fetches handled by the crawler worker, by outcome",
            &["outcome"],
        )
    })
}

/// `scrapix_crawler_fetch_duration_seconds` — wall time of one fetch attempt.
pub fn crawler_fetch_duration_seconds() -> &'static Histogram {
    static METRIC: OnceLock<Histogram> = OnceLock::new();
    METRIC.get_or_init(|| {
        register_histogram(
            "scrapix_crawler_fetch_duration_seconds",
            "Duration of one crawler fetch attempt, in seconds",
        )
    })
}

/// `scrapix_crawler_bytes_total` — bytes downloaded by successful fetches.
pub fn crawler_bytes_total() -> &'static Counter {
    static METRIC: OnceLock<Counter> = OnceLock::new();
    METRIC.get_or_init(|| {
        register_counter(
            "scrapix_crawler_bytes_total",
            "Total bytes downloaded by the crawler worker",
        )
    })
}

/// `scrapix_frontier_admissions_total{result}` — `admitted`/`duplicate`/`error`.
pub fn frontier_admissions_total() -> &'static CounterVec {
    static METRIC: OnceLock<CounterVec> = OnceLock::new();
    METRIC.get_or_init(|| {
        register_counter_vec(
            "scrapix_frontier_admissions_total",
            "Total URL admission attempts, by result",
            &["result"],
        )
    })
}

/// `scrapix_frontier_queued{job}` — current queue depth, top 50 jobs only
/// (see [`update_top_n_gauge`]).
pub fn frontier_queued() -> &'static GaugeVec {
    static METRIC: OnceLock<GaugeVec> = OnceLock::new();
    METRIC.get_or_init(|| {
        register_gauge_vec(
            "scrapix_frontier_queued",
            "Queued URL count for the top jobs by queue size",
            &["job"],
        )
    })
}

/// `scrapix_frontier_dispatched_total` — URLs dispatched to the crawl topic.
pub fn frontier_dispatched_total() -> &'static Counter {
    static METRIC: OnceLock<Counter> = OnceLock::new();
    METRIC.get_or_init(|| {
        register_counter(
            "scrapix_frontier_dispatched_total",
            "Total URLs dispatched by the frontier service",
        )
    })
}

/// `scrapix_content_documents_total{outcome}` — `success`/`failure`/`skipped`/`duplicate`.
pub fn content_documents_total() -> &'static CounterVec {
    static METRIC: OnceLock<CounterVec> = OnceLock::new();
    METRIC.get_or_init(|| {
        register_counter_vec(
            "scrapix_content_documents_total",
            "Total pages handled by the content worker, by outcome",
            &["outcome"],
        )
    })
}

/// `scrapix_content_flush_duration_seconds` — time to flush one storage backend.
pub fn content_flush_duration_seconds() -> &'static Histogram {
    static METRIC: OnceLock<Histogram> = OnceLock::new();
    METRIC.get_or_init(|| {
        register_histogram(
            "scrapix_content_flush_duration_seconds",
            "Duration of one content worker storage flush, in seconds",
        )
    })
}

/// `scrapix_api_jobs{status}` — in-memory job count by status.
pub fn api_jobs() -> &'static GaugeVec {
    static METRIC: OnceLock<GaugeVec> = OnceLock::new();
    METRIC.get_or_init(|| {
        register_gauge_vec(
            "scrapix_api_jobs",
            "Number of jobs tracked by the engine API, by status",
            &["status"],
        )
    })
}

/// `scrapix_consumer_uncommitted{topic}` — in-flight (unacked) message count
/// per Kafka topic, sampled once per commit tick.
pub fn consumer_uncommitted() -> &'static GaugeVec {
    static METRIC: OnceLock<GaugeVec> = OnceLock::new();
    METRIC.get_or_init(|| {
        register_gauge_vec(
            "scrapix_consumer_uncommitted",
            "In-flight (unacked) message count per topic",
            &["topic"],
        )
    })
}

/// Set `gauge` to reflect only the top `limit` entries in `entries` by value
/// (descending), clearing any label set in `previous` that fell out of the
/// top set. `previous` is updated in place to the label set that now has a
/// value, so the next tick's stale labels get pruned in turn.
///
/// This keeps per-job label cardinality bounded: without pruning, a gauge
/// vec keyed by an unbounded label (job id) grows forever as jobs come and
/// go.
pub fn update_top_n_gauge(
    gauge: &GaugeVec,
    entries: &[(String, u64)],
    limit: usize,
    previous: &mut HashSet<String>,
) {
    let mut sorted: Vec<&(String, u64)> = entries.iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    sorted.truncate(limit);

    let mut current: HashSet<String> = HashSet::with_capacity(sorted.len());
    for (label, value) in sorted {
        gauge
            .with_label_values(&[label.as_str()])
            .set(*value as f64);
        current.insert(label.clone());
    }

    for stale in previous.iter() {
        if !current.contains(stale) {
            let _ = gauge.remove_label_values(&[stale.as_str()]);
        }
    }

    *previous = current;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_gauge() -> GaugeVec {
        GaugeVec::new(Opts::new("test_gauge", "test"), &["job"]).unwrap()
    }

    fn values(gauge: &GaugeVec, jobs: &[&str]) -> Vec<f64> {
        jobs.iter()
            .map(|j| gauge.with_label_values(&[j]).get())
            .collect()
    }

    #[test]
    fn keeps_only_top_n_by_value() {
        let gauge = test_gauge();
        let mut previous = HashSet::new();
        let entries: Vec<(String, u64)> = (0..10).map(|i| (format!("job-{i}"), i as u64)).collect();
        update_top_n_gauge(&gauge, &entries, 3, &mut previous);

        // Only the 3 highest-valued jobs (7, 8, 9) survive.
        assert_eq!(previous.len(), 3);
        assert!(previous.contains("job-9"));
        assert!(previous.contains("job-8"));
        assert!(previous.contains("job-7"));
        assert_eq!(
            values(&gauge, &["job-9", "job-8", "job-7"]),
            vec![9.0, 8.0, 7.0]
        );
    }

    #[test]
    fn clears_stale_labels_that_fall_out_of_top_n() {
        let gauge = test_gauge();
        let mut previous = HashSet::new();

        update_top_n_gauge(
            &gauge,
            &[("a".into(), 100), ("b".into(), 50)],
            2,
            &mut previous,
        );
        assert_eq!(gauge.with_label_values(&["a"]).get(), 100.0);
        assert_eq!(gauge.with_label_values(&["b"]).get(), 50.0);

        // "b" drops off the top set; "c" takes its place. The gauge must
        // stop reporting "b" entirely (not just set it to 0), so cardinality
        // doesn't grow unbounded over the life of the process.
        update_top_n_gauge(
            &gauge,
            &[("a".into(), 100), ("c".into(), 75)],
            2,
            &mut previous,
        );
        assert_eq!(previous, HashSet::from(["a".to_string(), "c".to_string()]));

        let families = registry_families_for_test(&gauge);
        assert!(
            !families.contains(&"b".to_string()),
            "stale label 'b' must be removed, not just zeroed: {families:?}"
        );
    }

    /// Collect the label values currently exposed by `gauge` (via its own
    /// `collect()`, not the process-wide registry, so the test is isolated).
    fn registry_families_for_test(gauge: &GaugeVec) -> Vec<String> {
        use prometheus::core::Collector;
        gauge
            .collect()
            .iter()
            .flat_map(|mf| mf.get_metric().iter().cloned())
            .flat_map(|m| {
                m.get_label()
                    .iter()
                    .map(|l| l.get_value().to_string())
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    #[test]
    fn respects_insertion_order_ties_deterministically() {
        let gauge = test_gauge();
        let mut previous = HashSet::new();
        update_top_n_gauge(
            &gauge,
            &[("z".into(), 5), ("a".into(), 5)],
            1,
            &mut previous,
        );
        // Tie broken by label for determinism (not by insertion order).
        assert_eq!(previous, HashSet::from(["a".to_string()]));
    }

    #[test]
    fn encode_produces_prometheus_text_with_registered_counter() {
        crawler_bytes_total().inc_by(1.0);
        let text = encode();
        assert!(text.contains("scrapix_crawler_bytes_total"));
    }
}
