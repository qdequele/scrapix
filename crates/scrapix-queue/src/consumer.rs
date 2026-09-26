//! Kafka/Redpanda message consumer

use std::time::Duration;

use rdkafka::{
    config::ClientConfig,
    consumer::{CommitMode, Consumer, StreamConsumer},
    message::{BorrowedMessage, Message as KafkaMessage},
    TopicPartitionList,
};
use serde::de::DeserializeOwned;
use tokio_stream::StreamExt;
use tracing::{error, warn};

use scrapix_core::{Result, ScrapixError};

/// Consumer configuration
#[derive(Debug, Clone)]
pub struct ConsumerConfig {
    /// Kafka/Redpanda broker addresses
    pub brokers: String,
    /// Consumer group ID
    pub group_id: String,
    /// Client ID
    pub client_id: String,
    /// Auto commit interval
    pub auto_commit_interval: Duration,
    /// Session timeout
    pub session_timeout: Duration,
    /// Enable auto commit
    pub enable_auto_commit: bool,
    /// Auto offset reset (earliest, latest)
    pub auto_offset_reset: String,
    /// Max poll interval
    pub max_poll_interval: Duration,
    /// Fetch min bytes
    pub fetch_min_bytes: i32,
    /// Fetch max wait ms
    pub fetch_max_wait_ms: i32,
}

impl Default for ConsumerConfig {
    fn default() -> Self {
        Self {
            brokers: "localhost:9092".to_string(),
            group_id: "scrapix-consumer".to_string(),
            client_id: "scrapix-consumer".to_string(),
            auto_commit_interval: Duration::from_secs(5),
            session_timeout: Duration::from_secs(30),
            enable_auto_commit: false, // Manual commit for reliability
            auto_offset_reset: "earliest".to_string(),
            max_poll_interval: Duration::from_secs(300),
            fetch_min_bytes: 1,
            fetch_max_wait_ms: 500,
        }
    }
}

/// Kafka/Redpanda message consumer
pub struct KafkaConsumer {
    consumer: StreamConsumer,
    #[allow(dead_code)]
    config: ConsumerConfig,
}

impl KafkaConsumer {
    /// Create a new Kafka consumer
    pub fn new(config: ConsumerConfig) -> Result<Self> {
        let mut client_config = ClientConfig::new();

        client_config
            .set("bootstrap.servers", &config.brokers)
            .set("group.id", &config.group_id)
            .set("client.id", &config.client_id)
            .set("enable.auto.commit", config.enable_auto_commit.to_string())
            .set(
                "auto.commit.interval.ms",
                config.auto_commit_interval.as_millis().to_string(),
            )
            .set(
                "session.timeout.ms",
                config.session_timeout.as_millis().to_string(),
            )
            .set("auto.offset.reset", &config.auto_offset_reset)
            .set(
                "max.poll.interval.ms",
                config.max_poll_interval.as_millis().to_string(),
            )
            .set("fetch.min.bytes", config.fetch_min_bytes.to_string())
            .set("fetch.wait.max.ms", config.fetch_max_wait_ms.to_string());

        let consumer: StreamConsumer = client_config
            .create()
            .map_err(|e| ScrapixError::Queue(format!("Failed to create consumer: {}", e)))?;

        Ok(Self { consumer, config })
    }

    /// Create a consumer with default configuration
    pub fn with_brokers(brokers: impl Into<String>, group_id: impl Into<String>) -> Result<Self> {
        Self::new(ConsumerConfig {
            brokers: brokers.into(),
            group_id: group_id.into(),
            ..Default::default()
        })
    }

    /// Subscribe to topics
    pub fn subscribe(&self, topics: &[&str]) -> Result<()> {
        self.consumer
            .subscribe(topics)
            .map_err(|e| ScrapixError::Queue(format!("Failed to subscribe: {}", e)))
    }

    /// Unsubscribe from all topics
    pub fn unsubscribe(&self) {
        self.consumer.unsubscribe();
    }

    /// Process messages with a handler function
    ///
    /// Note: This processes messages sequentially. For concurrent processing, use process_concurrent.
    ///
    /// Even though this is sequential (at most one message in flight at a time per
    /// partition), a naive "commit the message that was just handled" is still
    /// wrong: if offset N's handler fails (left uncommitted) and N+1's handler then
    /// succeeds, committing N+1 directly would commit "next offset to read" = N+2,
    /// which is *past* N and would never redeliver it. We route through the same
    /// [`OffsetTracker`](crate::OffsetTracker) used by `process_with_ack` so the
    /// commit point can never advance past an offset that hasn't completed.
    pub async fn process<T, F, Fut>(&self, mut handler: F) -> Result<()>
    where
        T: DeserializeOwned,
        F: FnMut(T, MessageMetadata) -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        let mut stream = self.consumer.stream();
        let mut tracker = crate::OffsetTracker::default();

        while let Some(result) = stream.next().await {
            match result {
                Ok(msg) => {
                    let metadata = MessageMetadata::from_message(&msg);
                    tracker.begin(&metadata.topic, metadata.partition, metadata.offset);

                    match self.deserialize_message::<T>(&msg) {
                        Ok(payload) => match handler(payload, metadata.clone()).await {
                            Ok(()) => {
                                tracker.complete(
                                    &metadata.topic,
                                    metadata.partition,
                                    metadata.offset,
                                );
                            }
                            Err(e) => {
                                error!(
                                    topic = %metadata.topic,
                                    partition = metadata.partition,
                                    offset = metadata.offset,
                                    error = %e,
                                    "Handler error, leaving message uncommitted for redelivery"
                                );
                                // Do NOT complete: this offset (and anything after it)
                                // must stay uncommitted until it succeeds.
                            }
                        },
                        Err(e) => {
                            error!(
                                topic = %metadata.topic,
                                partition = metadata.partition,
                                offset = metadata.offset,
                                error = %e,
                                "Deserialization error, skipping poison message"
                            );
                            // Poison message: complete it immediately so it doesn't
                            // block the commit point behind it forever.
                            tracker.complete(&metadata.topic, metadata.partition, metadata.offset);
                        }
                    }

                    self.commit_offsets(tracker.take_commits(), CommitMode::Async);
                    warn_stuck_partitions(&mut tracker);
                }
                Err(e) => {
                    error!(error = %e, "Kafka error");
                }
            }
        }

        Ok(())
    }

    /// Process messages concurrently, committing offsets only once the handler acks.
    ///
    /// Spawns up to `concurrency` handler tasks and keeps polling (maintaining
    /// heartbeats) while they run. Offsets are tracked per-partition by an
    /// [`OffsetTracker`](crate::OffsetTracker) so that only a contiguous prefix of
    /// acked offsets is ever committed — an un-acked or still in-flight message
    /// (or one whose `Ack` is dropped without being called) blocks the commit
    /// point behind it, so it is redelivered after a restart or rebalance.
    pub async fn process_with_ack<T, F, Fut>(
        &self,
        handler: F,
        concurrency: usize,
        shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<()>
    where
        T: DeserializeOwned + Send + 'static,
        F: Fn(T, MessageMetadata, crate::Ack) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        use std::sync::atomic::Ordering;
        use tokio::sync::{mpsc, Semaphore};
        use tracing::info;

        info!(
            concurrency = concurrency,
            "Starting ack-based concurrent message processing"
        );

        let handler = std::sync::Arc::new(handler);
        let semaphore = std::sync::Arc::new(Semaphore::new(concurrency.max(1)));
        let (done_tx, mut done_rx) = mpsc::unbounded_channel::<(String, i32, i64)>();
        let mut tracker = crate::OffsetTracker::default();
        let mut stream = self.consumer.stream();
        let mut commit_tick = tokio::time::interval(Duration::from_secs(1));
        let mut assigned = self.assigned_partitions();

        loop {
            if shutdown.load(Ordering::Relaxed) {
                info!("Shutdown requested, exiting consumer loop");
                break;
            }

            tokio::select! {
                _ = commit_tick.tick() => {
                    while let Ok((t, p, o)) = done_rx.try_recv() {
                        tracker.complete(&t, p, o);
                    }
                    // Drop tracker state for partitions we no longer own (rebalance).
                    let now = self.assigned_partitions();
                    for gone in assigned.difference(&now) {
                        tracker.revoke(&gone.0, gone.1);
                    }
                    assigned = now;
                    self.commit_offsets(tracker.take_commits(), CommitMode::Async);
                    warn_stuck_partitions(&mut tracker);
                }
                Some((t, p, o)) = done_rx.recv() => tracker.complete(&t, p, o),
                next = stream.next() => match next {
                    Some(Ok(msg)) => {
                        let metadata = MessageMetadata::from_message(&msg);
                        tracker.begin(&metadata.topic, metadata.partition, metadata.offset);
                        let ack = {
                            let (tx, t, p, o) = (
                                done_tx.clone(),
                                metadata.topic.clone(),
                                metadata.partition,
                                metadata.offset,
                            );
                            crate::Ack::from_fn(move || {
                                let _ = tx.send((t, p, o));
                            })
                        };

                        match self.deserialize_message::<T>(&msg) {
                            Ok(payload) => {
                                let permit = match semaphore.clone().acquire_owned().await {
                                    Ok(p) => p,
                                    Err(_) => break,
                                };
                                let handler = handler.clone();
                                tokio::spawn(async move {
                                    handler(payload, metadata, ack).await;
                                    drop(permit);
                                });
                            }
                            Err(e) => {
                                error!(
                                    topic = %metadata.topic,
                                    partition = metadata.partition,
                                    offset = metadata.offset,
                                    error = %e,
                                    "Deserialization error, skipping"
                                );
                                // Poison message: ack immediately so we don't block the
                                // commit point on something we can never process.
                                ack.ack();
                            }
                        }
                    }
                    Some(Err(e)) => error!(error = %e, "Kafka error"),
                    None => break,
                },
            }
        }

        // Drain: wait for in-flight handlers (bounded), then commit whatever finished.
        let _ = tokio::time::timeout(
            Duration::from_secs(30),
            semaphore.acquire_many(concurrency.max(1) as u32),
        )
        .await;
        while let Ok((t, p, o)) = done_rx.try_recv() {
            tracker.complete(&t, p, o);
        }
        // Same rebalance handling as the tick branch: don't try to commit offsets
        // for partitions we no longer own.
        let final_assigned = self.assigned_partitions();
        for gone in assigned.difference(&final_assigned) {
            tracker.revoke(&gone.0, gone.1);
        }
        self.commit_offsets(tracker.take_commits(), CommitMode::Sync);

        Ok(())
    }

    /// Process messages concurrently by spawning tasks for each message.
    /// This ensures the consumer keeps polling (maintaining heartbeats) while handlers run.
    ///
    /// Built on [`process_with_ack`](Self::process_with_ack): the offset is committed
    /// only when the handler returns `Ok`. On `Err`, the message is left uncommitted
    /// for redelivery — the handler is expected to have already handled retry/DLQ
    /// logic itself before returning.
    pub async fn process_concurrent<T, F, Fut>(
        &self,
        handler: F,
        concurrency: usize,
        shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<()>
    where
        T: DeserializeOwned + Send + 'static,
        F: Fn(T, MessageMetadata) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<()>> + Send + 'static,
    {
        let handler = std::sync::Arc::new(handler);
        self.process_with_ack::<T, _, _>(
            move |payload, metadata, ack| {
                let handler = handler.clone();
                async move {
                    match handler(payload, metadata.clone()).await {
                        Ok(()) => ack.ack(),
                        Err(e) => {
                            error!(
                                topic = %metadata.topic,
                                partition = metadata.partition,
                                offset = metadata.offset,
                                error = %e,
                                "Handler error, leaving message uncommitted for redelivery"
                            );
                            // Drop `ack` without calling it: offset stays uncommitted.
                        }
                    }
                }
            },
            concurrency,
            shutdown,
        )
        .await
    }

    /// Partitions currently assigned to this consumer, as `(topic, partition)` pairs.
    fn assigned_partitions(&self) -> std::collections::HashSet<(String, i32)> {
        self.consumer
            .assignment()
            .map(|tpl| {
                tpl.elements()
                    .iter()
                    .map(|e| (e.topic().to_string(), e.partition()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Commit a batch of `(topic, partition, next_offset)` tuples produced by an
    /// [`OffsetTracker`](crate::OffsetTracker).
    fn commit_offsets(&self, commits: Vec<(String, i32, i64)>, mode: CommitMode) {
        if commits.is_empty() {
            return;
        }
        let mut tpl = TopicPartitionList::new();
        for (topic, partition, next) in &commits {
            if let Err(e) =
                tpl.add_partition_offset(topic, *partition, rdkafka::Offset::Offset(*next))
            {
                warn!(error = %e, "Failed to build commit list");
            }
        }
        if let Err(e) = self.consumer.commit(&tpl, mode) {
            // `tracker.take_commits()` already advanced `last_committed` for these
            // offsets, so they will NOT be retried on the next tick unless the
            // partition makes further progress (a later `take_commits()` call
            // reports a higher offset). A commit failure here is effectively
            // dropped until then.
            warn!(
                error = %e,
                ?commits,
                "Offset commit failed; these offsets are dropped and won't be retried \
                 until the partition advances further"
            );
        }
    }

    /// Receive a batch of messages
    pub async fn receive_batch<T: DeserializeOwned>(
        &self,
        max_messages: usize,
        timeout: Duration,
    ) -> Result<Vec<(T, MessageMetadata)>> {
        let mut messages = Vec::with_capacity(max_messages);
        let deadline = tokio::time::Instant::now() + timeout;
        let mut stream = self.consumer.stream();

        while messages.len() < max_messages && tokio::time::Instant::now() < deadline {
            match tokio::time::timeout_at(deadline, stream.next()).await {
                Ok(Some(Ok(msg))) => {
                    let metadata = MessageMetadata::from_message(&msg);
                    if let Ok(payload) = self.deserialize_message::<T>(&msg) {
                        messages.push((payload, metadata));
                    }
                }
                Ok(Some(Err(e))) => {
                    warn!(error = %e, "Kafka error during batch receive");
                }
                Ok(None) => break,
                Err(_) => break, // Timeout
            }
        }

        Ok(messages)
    }

    /// Commit a message's offset
    fn commit_message(&self, msg: &BorrowedMessage<'_>) -> Result<()> {
        self.consumer
            .commit_message(msg, CommitMode::Async)
            .map_err(|e| ScrapixError::Queue(format!("Commit failed: {}", e)))
    }

    /// Commit all consumed offsets
    pub fn commit(&self) -> Result<()> {
        self.consumer
            .commit_consumer_state(CommitMode::Sync)
            .map_err(|e| ScrapixError::Queue(format!("Commit failed: {}", e)))
    }

    /// Deserialize a message payload
    fn deserialize_message<T: DeserializeOwned>(&self, msg: &BorrowedMessage<'_>) -> Result<T> {
        let payload = msg
            .payload()
            .ok_or_else(|| ScrapixError::Queue("Empty message payload".to_string()))?;

        serde_json::from_slice(payload)
            .map_err(|e| ScrapixError::Queue(format!("Deserialization failed: {}", e)))
    }

    /// Get current positions for assigned partitions
    pub fn positions(&self) -> Result<Vec<(String, i32, i64)>> {
        let _assignment = self
            .consumer
            .assignment()
            .map_err(|e| ScrapixError::Queue(format!("Failed to get assignment: {}", e)))?;

        let positions = self
            .consumer
            .position()
            .map_err(|e| ScrapixError::Queue(format!("Failed to get positions: {}", e)))?;

        let mut result = Vec::new();
        for elem in positions.elements() {
            if let Some(offset) = elem.offset().to_raw() {
                result.push((elem.topic().to_string(), elem.partition(), offset));
            }
        }

        Ok(result)
    }

    /// Seek to a specific offset
    pub fn seek(&self, topic: &str, partition: i32, offset: i64) -> Result<()> {
        let mut tpl = TopicPartitionList::new();
        tpl.add_partition_offset(topic, partition, rdkafka::Offset::Offset(offset))
            .map_err(|e| ScrapixError::Queue(format!("Failed to set offset: {}", e)))?;

        self.consumer
            .seek_partitions(tpl, Duration::from_secs(5))
            .map_err(|e| ScrapixError::Queue(format!("Seek failed: {}", e)))?;

        Ok(())
    }

    /// Pause consumption on specific partitions
    pub fn pause(&self, topic: &str, partitions: &[i32]) -> Result<()> {
        let mut tpl = TopicPartitionList::new();
        for &p in partitions {
            tpl.add_partition(topic, p);
        }

        self.consumer
            .pause(&tpl)
            .map_err(|e| ScrapixError::Queue(format!("Pause failed: {}", e)))
    }

    /// Resume consumption on specific partitions
    pub fn resume(&self, topic: &str, partitions: &[i32]) -> Result<()> {
        let mut tpl = TopicPartitionList::new();
        for &p in partitions {
            tpl.add_partition(topic, p);
        }

        self.consumer
            .resume(&tpl)
            .map_err(|e| ScrapixError::Queue(format!("Resume failed: {}", e)))
    }

    /// Get the broker addresses
    pub fn brokers(&self) -> &str {
        &self.config.brokers
    }

    /// Get the consumer group ID
    pub fn group_id(&self) -> &str {
        &self.config.group_id
    }

    /// Poll for a single message with timeout
    pub async fn poll_one<T: DeserializeOwned>(&self, timeout: Duration) -> Result<Option<T>> {
        let mut stream = self.consumer.stream();

        match tokio::time::timeout(timeout, stream.next()).await {
            Ok(Some(Ok(msg))) => {
                let result = self.deserialize_message::<T>(&msg)?;
                // Commit the message
                let _ = self.commit_message(&msg);
                Ok(Some(result))
            }
            Ok(Some(Err(e))) => Err(ScrapixError::Queue(format!("Kafka error: {}", e))),
            Ok(None) => Ok(None),
            Err(_) => Ok(None), // Timeout
        }
    }
}

/// How long a partition may go without committable progress before we consider it
/// stuck (a handler failing or hung on the same offset) and log a warning.
const STUCK_PARTITION_THRESHOLD: Duration = Duration::from_secs(60);

/// Minimum gap between repeated stuck-partition warnings for the same partition.
const STUCK_PARTITION_WARN_COOLDOWN: Duration = Duration::from_secs(60);

/// Log a warning (at most once per [`STUCK_PARTITION_WARN_COOLDOWN`] per partition)
/// for every partition the tracker considers stuck. Shared between `process` and
/// `process_with_ack` so both surface the same signal for an operator: this is a
/// mitigation (visibility), not a fix — the message itself is still left
/// uncommitted for redelivery, same as any other un-acked offset.
fn warn_stuck_partitions(tracker: &mut crate::OffsetTracker) {
    for (topic, partition, offset) in tracker.stuck_partitions(
        std::time::Instant::now(),
        STUCK_PARTITION_THRESHOLD,
        STUCK_PARTITION_WARN_COOLDOWN,
    ) {
        warn!(
            topic = %topic,
            partition = partition,
            offset = offset,
            "Consumer offset commit stuck: no progress for over 60s — handler \
             repeatedly failing or hung on this offset?"
        );
    }
}

/// Metadata about a consumed message
#[derive(Debug, Clone)]
pub struct MessageMetadata {
    /// Topic name
    pub topic: String,
    /// Partition number
    pub partition: i32,
    /// Message offset
    pub offset: i64,
    /// Message key (if present)
    pub key: Option<String>,
    /// Message timestamp (milliseconds)
    pub timestamp: Option<i64>,
}

impl MessageMetadata {
    fn from_message(msg: &BorrowedMessage<'_>) -> Self {
        Self {
            topic: msg.topic().to_string(),
            partition: msg.partition(),
            offset: msg.offset(),
            key: msg.key().map(|k| String::from_utf8_lossy(k).to_string()),
            timestamp: msg.timestamp().to_millis(),
        }
    }
}

// Implement the MessageConsumer trait for KafkaConsumer
#[async_trait::async_trait]
impl crate::traits::MessageConsumer for KafkaConsumer {
    fn subscribe(&self, topics: &[&str]) -> Result<()> {
        KafkaConsumer::subscribe(self, topics)
    }

    async fn process<T, F, Fut>(&self, handler: F) -> Result<()>
    where
        T: DeserializeOwned + Send + 'static,
        F: FnMut(T, MessageMetadata) -> Fut + Send,
        Fut: std::future::Future<Output = Result<()>> + Send,
    {
        KafkaConsumer::process(self, handler).await
    }

    async fn process_concurrent<T, F, Fut>(
        &self,
        handler: F,
        concurrency: usize,
        shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<()>
    where
        T: DeserializeOwned + Send + 'static,
        F: Fn(T, MessageMetadata) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<()>> + Send + 'static,
    {
        KafkaConsumer::process_concurrent(self, handler, concurrency, shutdown).await
    }

    async fn process_with_ack<T, F, Fut>(
        &self,
        handler: F,
        concurrency: usize,
        shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<()>
    where
        T: DeserializeOwned + Send + 'static,
        F: Fn(T, MessageMetadata, crate::Ack) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        KafkaConsumer::process_with_ack(self, handler, concurrency, shutdown).await
    }

    async fn poll_one<T: DeserializeOwned + Send>(&self, timeout: Duration) -> Result<Option<T>> {
        KafkaConsumer::poll_one(self, timeout).await
    }
}

/// Builder for KafkaConsumer
pub struct ConsumerBuilder {
    config: ConsumerConfig,
}

impl ConsumerBuilder {
    pub fn new(brokers: impl Into<String>, group_id: impl Into<String>) -> Self {
        Self {
            config: ConsumerConfig {
                brokers: brokers.into(),
                group_id: group_id.into(),
                ..Default::default()
            },
        }
    }

    pub fn client_id(mut self, id: impl Into<String>) -> Self {
        self.config.client_id = id.into();
        self
    }

    pub fn auto_commit(mut self, enabled: bool) -> Self {
        self.config.enable_auto_commit = enabled;
        self
    }

    pub fn auto_offset_reset(mut self, reset: impl Into<String>) -> Self {
        self.config.auto_offset_reset = reset.into();
        self
    }

    pub fn session_timeout(mut self, timeout: Duration) -> Self {
        self.config.session_timeout = timeout;
        self
    }

    pub fn build(self) -> Result<KafkaConsumer> {
        KafkaConsumer::new(self.config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_consumer_config_default() {
        let config = ConsumerConfig::default();
        assert_eq!(config.brokers, "localhost:9092");
        assert!(!config.enable_auto_commit);
        assert_eq!(config.auto_offset_reset, "earliest");
    }

    #[test]
    fn test_builder() {
        let builder = ConsumerBuilder::new("broker:9092", "test-group")
            .client_id("test-client")
            .auto_commit(true)
            .auto_offset_reset("latest");

        assert_eq!(builder.config.brokers, "broker:9092");
        assert_eq!(builder.config.group_id, "test-group");
        assert!(builder.config.enable_auto_commit);
        assert_eq!(builder.config.auto_offset_reset, "latest");
    }
}
