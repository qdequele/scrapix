//! Kafka/Redpanda message producer

use std::time::Duration;

use rdkafka::{
    config::ClientConfig,
    message::{Header, OwnedHeaders},
    producer::{FutureProducer, FutureRecord, Producer},
    util::Timeout,
};
use serde::Serialize;
use tracing::{debug, error, info, instrument};

use scrapix_core::{Result, ScrapixError};

/// Environment variable setting the linger of every Kafka producer (ms).
/// Raise it (e.g. `5`) to trade send latency for fewer produce requests.
pub const LINGER_MS_ENV: &str = "KAFKA_LINGER_MS";

/// Default producer linger (ms). Measured with `examples/linger_bench.rs`
/// against a local single-node Redpanda (`acks=all`, idempotent, lz4,
/// 1.5 KB messages, 1-partition topic), linger 0 / 5 / 20 ms:
/// one awaited send p50 0.13 / 6.6 / 22.4 ms; 100 sends awaited together
/// 0.9 / 7.3 / 23 ms in 4 / 3 / 3 produce requests; 20,000 messages with
/// 256 in flight: same throughput and 436 / 435 / 435 requests (batches
/// fill by size, and librdkafka batches what queues up while a request is
/// in flight); 64 slots each awaiting its sends: 150k / 120k / 85k msg/s.
/// The linger only bought fewer requests at moderate rates (2,000 msg/s:
/// 1,170 / 183 / 48 requests/s), at 0.4 / 3.1 / 10.9 ms per send.
pub const DEFAULT_LINGER_MS: u64 = 0;

/// Highest accepted `KAFKA_LINGER_MS`.
pub const MAX_LINGER_MS: u64 = 1000;

/// Parse a `KAFKA_LINGER_MS` value: an integer in `0..=MAX_LINGER_MS`.
/// Unset or blank means [`DEFAULT_LINGER_MS`]; anything else is an error,
/// so a typo stops the service at startup instead of being ignored.
pub fn parse_linger_ms(raw: Option<&str>) -> Result<u64> {
    let Some(raw) = raw.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(DEFAULT_LINGER_MS);
    };
    match raw.parse::<u64>() {
        Ok(ms) if ms <= MAX_LINGER_MS => Ok(ms),
        _ => Err(ScrapixError::Queue(format!(
            "Invalid {LINGER_MS_ENV}={raw:?}: expected an integer between 0 and {MAX_LINGER_MS} (milliseconds)"
        ))),
    }
}

/// The producer linger from `KAFKA_LINGER_MS` (see [`parse_linger_ms`]).
pub fn linger_ms_from_env() -> Result<u64> {
    match std::env::var(LINGER_MS_ENV) {
        Ok(raw) => parse_linger_ms(Some(&raw)),
        Err(std::env::VarError::NotPresent) => parse_linger_ms(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(ScrapixError::Queue(format!(
            "Invalid {LINGER_MS_ENV}: not valid UTF-8"
        ))),
    }
}

/// Producer configuration
#[derive(Debug, Clone)]
pub struct ProducerConfig {
    /// Kafka/Redpanda broker addresses
    pub brokers: String,
    /// Client ID
    pub client_id: String,
    /// Message timeout
    pub message_timeout: Duration,
    /// Request timeout
    pub request_timeout: Duration,
    /// Enable idempotent producer
    pub idempotent: bool,
    /// Compression type (none, gzip, snappy, lz4, zstd)
    pub compression: String,
    /// Batch size
    pub batch_size: usize,
    /// Linger time (ms): how long a message waits for others before its
    /// batch is sent. `send` awaits the delivery report, so every awaited
    /// publish stage costs at least one linger. [`ProducerBuilder`] and
    /// [`KafkaProducer::with_brokers`] take it from `KAFKA_LINGER_MS`.
    pub linger_ms: u64,
    /// Acks required (0, 1, all)
    pub acks: String,
    /// Maximum message size in bytes (must match topic max.message.bytes)
    pub max_message_bytes: usize,
}

impl Default for ProducerConfig {
    fn default() -> Self {
        Self {
            brokers: "localhost:9092".to_string(),
            client_id: "scrapix-producer".to_string(),
            message_timeout: Duration::from_secs(30),
            request_timeout: Duration::from_secs(5),
            idempotent: true,
            compression: "lz4".to_string(),
            batch_size: 65536,
            linger_ms: DEFAULT_LINGER_MS,
            acks: "all".to_string(),
            max_message_bytes: 10 * 1024 * 1024, // 10MB
        }
    }
}

/// Kafka/Redpanda message producer
pub struct KafkaProducer {
    producer: FutureProducer,
    config: ProducerConfig,
}

impl KafkaProducer {
    /// Create a new Kafka producer
    pub fn new(config: ProducerConfig) -> Result<Self> {
        let mut client_config = ClientConfig::new();

        client_config
            .set("bootstrap.servers", &config.brokers)
            .set("client.id", &config.client_id)
            .set(
                "message.timeout.ms",
                config.message_timeout.as_millis().to_string(),
            )
            .set(
                "request.timeout.ms",
                config.request_timeout.as_millis().to_string(),
            )
            .set("compression.type", &config.compression)
            .set("batch.size", config.batch_size.to_string())
            .set("linger.ms", config.linger_ms.to_string())
            .set("acks", &config.acks)
            .set("message.max.bytes", config.max_message_bytes.to_string());

        if config.idempotent {
            client_config.set("enable.idempotence", "true");
        }

        let producer: FutureProducer = client_config
            .create()
            .map_err(|e| ScrapixError::Queue(format!("Failed to create producer: {}", e)))?;

        info!(
            client_id = %config.client_id,
            linger_ms = config.linger_ms,
            "Kafka producer created"
        );

        Ok(Self { producer, config })
    }

    /// Create a producer with default configuration (linger from
    /// `KAFKA_LINGER_MS`)
    pub fn with_brokers(brokers: impl Into<String>) -> Result<Self> {
        Self::new(ProducerConfig {
            brokers: brokers.into(),
            linger_ms: linger_ms_from_env()?,
            ..Default::default()
        })
    }

    /// Send a message to a topic
    #[instrument(skip(self, payload), fields(topic = %topic))]
    pub async fn send<T: Serialize>(
        &self,
        topic: &str,
        key: Option<&str>,
        payload: &T,
    ) -> Result<(i32, i64)> {
        let payload_bytes = serde_json::to_vec(payload)
            .map_err(|e| ScrapixError::Queue(format!("Serialization failed: {}", e)))?;

        self.send_raw(topic, key, &payload_bytes, None).await
    }

    /// Send a message with custom headers
    pub async fn send_with_headers<T: Serialize>(
        &self,
        topic: &str,
        key: Option<&str>,
        payload: &T,
        headers: Vec<(&str, &str)>,
    ) -> Result<(i32, i64)> {
        let payload_bytes = serde_json::to_vec(payload)
            .map_err(|e| ScrapixError::Queue(format!("Serialization failed: {}", e)))?;

        self.send_raw(topic, key, &payload_bytes, Some(headers))
            .await
    }

    /// Send raw bytes to a topic
    pub async fn send_raw(
        &self,
        topic: &str,
        key: Option<&str>,
        payload: &[u8],
        headers: Option<Vec<(&str, &str)>>,
    ) -> Result<(i32, i64)> {
        let mut record = FutureRecord::to(topic).payload(payload);

        if let Some(k) = key {
            record = record.key(k);
        }

        let owned_headers = if let Some(hdrs) = headers {
            let mut h = OwnedHeaders::new();
            for (name, value) in hdrs {
                h = h.insert(Header {
                    key: name,
                    value: Some(value),
                });
            }
            Some(h)
        } else {
            None
        };

        if let Some(h) = owned_headers {
            record = record.headers(h);
        }

        let timeout = Timeout::After(self.config.message_timeout);

        match self.producer.send(record, timeout).await {
            Ok(delivery) => {
                let partition = delivery.partition;
                let offset = delivery.offset;
                debug!(topic, partition, offset, "Message sent");
                Ok((partition, offset))
            }
            Err((err, _)) => {
                error!(topic, error = %err, "Failed to send message");
                Err(ScrapixError::Queue(format!(
                    "Failed to send message: {}",
                    err
                )))
            }
        }
    }

    /// Send multiple messages to a topic (batched)
    pub async fn send_batch<T: Serialize>(
        &self,
        topic: &str,
        messages: Vec<(Option<String>, T)>,
    ) -> Result<Vec<std::result::Result<(i32, i64), String>>> {
        let mut results = Vec::with_capacity(messages.len());

        for (key, payload) in messages {
            let result = match self.send(topic, key.as_deref(), &payload).await {
                Ok(r) => Ok(r),
                Err(e) => Err(e.to_string()),
            };
            results.push(result);
        }

        Ok(results)
    }

    /// Flush all pending messages
    pub fn flush(&self, timeout: Duration) {
        let _ = self.producer.flush(Timeout::After(timeout));
    }

    /// Check if the producer is connected and can reach the broker
    pub fn is_healthy(&self) -> bool {
        // Try to get cluster metadata with a short timeout
        // This verifies we can communicate with the broker
        match self
            .producer
            .client()
            .fetch_metadata(None, Timeout::After(Duration::from_secs(5)))
        {
            Ok(metadata) => {
                // Check that we have at least one broker
                !metadata.brokers().is_empty()
            }
            Err(_) => false,
        }
    }

    /// Get broker information for diagnostics
    pub fn broker_count(&self) -> Option<usize> {
        self.producer
            .client()
            .fetch_metadata(None, Timeout::After(Duration::from_secs(5)))
            .ok()
            .map(|m| m.brokers().len())
    }
}

/// Builder for KafkaProducer
pub struct ProducerBuilder {
    config: ProducerConfig,
    /// `linger_ms()` was called: `KAFKA_LINGER_MS` is not read.
    linger_set: bool,
}

impl ProducerBuilder {
    pub fn new(brokers: impl Into<String>) -> Self {
        Self {
            config: ProducerConfig {
                brokers: brokers.into(),
                ..Default::default()
            },
            linger_set: false,
        }
    }

    pub fn client_id(mut self, id: impl Into<String>) -> Self {
        self.config.client_id = id.into();
        self
    }

    pub fn message_timeout(mut self, timeout: Duration) -> Self {
        self.config.message_timeout = timeout;
        self
    }

    pub fn compression(mut self, compression: impl Into<String>) -> Self {
        self.config.compression = compression.into();
        self
    }

    pub fn batch_size(mut self, size: usize) -> Self {
        self.config.batch_size = size;
        self
    }

    /// Set the linger explicitly, overriding `KAFKA_LINGER_MS`.
    pub fn linger_ms(mut self, ms: u64) -> Self {
        self.config.linger_ms = ms;
        self.linger_set = true;
        self
    }

    pub fn acks(mut self, acks: impl Into<String>) -> Self {
        self.config.acks = acks.into();
        self
    }

    pub fn idempotent(mut self, enabled: bool) -> Self {
        self.config.idempotent = enabled;
        self
    }

    /// Build the producer. Unless `linger_ms()` was called, the linger
    /// comes from `KAFKA_LINGER_MS`; an invalid value is an error.
    pub fn build(mut self) -> Result<KafkaProducer> {
        if !self.linger_set {
            self.config.linger_ms = linger_ms_from_env()?;
        }
        KafkaProducer::new(self.config)
    }
}

// Implement the MessageProducer trait for KafkaProducer
#[async_trait::async_trait]
impl crate::traits::MessageProducer for KafkaProducer {
    async fn send<T: Serialize + Send + Sync>(
        &self,
        topic: &str,
        key: Option<&str>,
        payload: &T,
    ) -> Result<(i32, i64)> {
        KafkaProducer::send(self, topic, key, payload).await
    }

    async fn send_raw(&self, topic: &str, key: Option<&str>, payload: &[u8]) -> Result<(i32, i64)> {
        KafkaProducer::send_raw(self, topic, key, payload, None).await
    }

    fn flush(&self, timeout: Duration) {
        KafkaProducer::flush(self, timeout)
    }

    fn is_healthy(&self) -> bool {
        KafkaProducer::is_healthy(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_producer_config_default() {
        let config = ProducerConfig::default();
        assert_eq!(config.brokers, "localhost:9092");
        assert_eq!(config.compression, "lz4");
        assert!(config.idempotent);
    }

    #[test]
    fn test_builder() {
        let builder = ProducerBuilder::new("broker1:9092,broker2:9092")
            .client_id("test-producer")
            .compression("gzip")
            .batch_size(32768);

        assert_eq!(builder.config.brokers, "broker1:9092,broker2:9092");
        assert_eq!(builder.config.client_id, "test-producer");
        assert_eq!(builder.config.compression, "gzip");
        assert_eq!(builder.config.batch_size, 32768);
        assert!(!builder.linger_set);
        assert!(builder.linger_ms(0).linger_set);
    }

    #[test]
    fn linger_ms_defaults_when_unset_or_blank() {
        assert_eq!(parse_linger_ms(None).unwrap(), DEFAULT_LINGER_MS);
        assert_eq!(parse_linger_ms(Some("")).unwrap(), DEFAULT_LINGER_MS);
        assert_eq!(parse_linger_ms(Some("  ")).unwrap(), DEFAULT_LINGER_MS);
        assert_eq!(ProducerConfig::default().linger_ms, DEFAULT_LINGER_MS);
    }

    #[test]
    fn linger_ms_accepts_the_range() {
        assert_eq!(parse_linger_ms(Some("0")).unwrap(), 0);
        assert_eq!(parse_linger_ms(Some(" 20 ")).unwrap(), 20);
        assert_eq!(parse_linger_ms(Some("1000")).unwrap(), 1000);
    }

    #[test]
    fn linger_ms_rejects_invalid_values() {
        for bad in ["1001", "-1", "5ms", "2.5", "abc"] {
            let err = parse_linger_ms(Some(bad)).unwrap_err().to_string();
            assert!(err.contains("KAFKA_LINGER_MS"), "{bad}: {err}");
        }
    }
}
