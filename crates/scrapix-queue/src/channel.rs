//! In-process message bus using bounded async channels.
//!
//! This replaces Kafka when running all services in a single process (`scrapix all`).
//! Uses `async-channel` (mpmc, bounded) to simulate topics.
//!
//! Consumers from [`ChannelBus::consumer`] share one queue per topic (like
//! one Kafka consumer group). Consumers from [`ChannelBus::consumer_in_group`]
//! each get their own copy of every message sent after they subscribed
//! (like a Kafka group with `auto.offset.reset=latest`), which is how
//! broadcast topics such as job control reach every service. Delivery to
//! a topic with named groups never blocks the producer: a full group queue
//! drops the message (logged).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_channel::{Receiver, Sender};
use parking_lot::RwLock;
use serde::de::DeserializeOwned;
use serde::Serialize;
use tracing::{debug, error, warn};

use scrapix_core::{Result, ScrapixError};

use crate::traits::{MessageConsumer, MessageProducer};
use crate::MessageMetadata;

/// Default channel capacity per topic.
const DEFAULT_CAPACITY: usize = 50_000;

/// In-process message bus that holds topic channels.
///
/// Create one `ChannelBus`, then call `producer()` and `consumer()` to get
/// handles that implement `MessageProducer` / `MessageConsumer`.
pub struct ChannelBus {
    topics: Arc<RwLock<HashMap<String, TopicChannel>>>,
    capacity: usize,
}

struct TopicChannel {
    sender: Sender<Vec<u8>>,
    receiver: Receiver<Vec<u8>>,
    offset: AtomicI64,
    /// Named consumer groups: each receives its own copy of every message.
    groups: Vec<GroupChannel>,
}

struct GroupChannel {
    name: String,
    sender: Sender<Vec<u8>>,
    receiver: Receiver<Vec<u8>>,
}

impl TopicChannel {
    fn new(capacity: usize) -> Self {
        let (sender, receiver) = async_channel::bounded(capacity);
        Self {
            sender,
            receiver,
            offset: AtomicI64::new(0),
            groups: Vec::new(),
        }
    }

    /// The receiver of consumer group `name`, created on first use.
    fn group_receiver(&mut self, name: &str, capacity: usize) -> Receiver<Vec<u8>> {
        if let Some(g) = self.groups.iter().find(|g| g.name == name) {
            return g.receiver.clone();
        }
        let (sender, receiver) = async_channel::bounded(capacity);
        self.groups.push(GroupChannel {
            name: name.to_string(),
            sender,
            receiver: receiver.clone(),
        });
        receiver
    }
}

/// Where one produced message goes.
struct Route {
    default: Sender<Vec<u8>>,
    groups: Vec<(String, Sender<Vec<u8>>)>,
    offset: i64,
}

impl Route {
    async fn deliver(self, topic: &str, bytes: Vec<u8>) -> Result<()> {
        if self.groups.is_empty() {
            return self
                .default
                .send(bytes)
                .await
                .map_err(|e| ScrapixError::Queue(format!("Channel send failed: {}", e)));
        }
        for (name, sender) in &self.groups {
            if let Err(e) = sender.try_send(bytes.clone()) {
                warn!(topic, group = %name, error = %e, "Channel group queue full or closed, message dropped");
            }
        }
        // The shared queue of a broadcast topic may have no reader at all.
        let _ = self.default.try_send(bytes);
        Ok(())
    }
}

impl ChannelBus {
    /// Create a new channel bus with default capacity (50,000 messages per topic).
    pub fn new() -> Self {
        Self {
            topics: Arc::new(RwLock::new(HashMap::new())),
            capacity: DEFAULT_CAPACITY,
        }
    }

    /// Create a new channel bus with custom capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            topics: Arc::new(RwLock::new(HashMap::new())),
            capacity,
        }
    }

    /// Create a producer handle.
    pub fn producer(&self) -> ChannelProducer {
        ChannelProducer {
            bus: self.topics.clone(),
            capacity: self.capacity,
        }
    }

    /// Create a consumer handle.
    pub fn consumer(&self) -> ChannelConsumer {
        ChannelConsumer {
            bus: self.topics.clone(),
            capacity: self.capacity,
            subscriptions: Arc::new(RwLock::new(Vec::new())),
            group: None,
            #[cfg(feature = "test-hooks")]
            hooks: Arc::new(hooks::Hooks::default()),
        }
    }

    /// Create a consumer in the named group `group`: it receives its own
    /// copy of every message sent to its topics after `subscribe`.
    pub fn consumer_in_group(&self, group: impl Into<String>) -> ChannelConsumer {
        ChannelConsumer {
            group: Some(group.into()),
            ..self.consumer()
        }
    }
}

impl Default for ChannelBus {
    fn default() -> Self {
        Self::new()
    }
}

/// In-process message producer.
pub struct ChannelProducer {
    bus: Arc<RwLock<HashMap<String, TopicChannel>>>,
    capacity: usize,
}

impl ChannelProducer {
    fn route(&self, topic: &str) -> Route {
        // Fast path
        {
            let topics = self.bus.read();
            if let Some(tc) = topics.get(topic) {
                return route_of(tc);
            }
        }

        // Slow path
        let mut topics = self.bus.write();
        let tc = topics.entry(topic.to_string()).or_insert_with(|| {
            debug!(topic = topic, "Created topic channel (from producer)");
            TopicChannel::new(self.capacity)
        });
        route_of(tc)
    }
}

fn route_of(tc: &TopicChannel) -> Route {
    Route {
        default: tc.sender.clone(),
        groups: tc
            .groups
            .iter()
            .map(|g| (g.name.clone(), g.sender.clone()))
            .collect(),
        offset: tc.offset.fetch_add(1, Ordering::Relaxed),
    }
}

#[async_trait::async_trait]
impl MessageProducer for ChannelProducer {
    async fn send<T: Serialize + Send + Sync>(
        &self,
        topic: &str,
        _key: Option<&str>,
        payload: &T,
    ) -> Result<(i32, i64)> {
        let bytes = serde_json::to_vec(payload)
            .map_err(|e| ScrapixError::Queue(format!("Serialization failed: {}", e)))?;

        let route = self.route(topic);
        let offset = route.offset;
        route.deliver(topic, bytes).await?;

        debug!(topic = topic, offset = offset, "Channel message sent");
        Ok((0, offset)) // partition=0 for channels
    }

    async fn send_raw(
        &self,
        topic: &str,
        _key: Option<&str>,
        payload: &[u8],
    ) -> Result<(i32, i64)> {
        let route = self.route(topic);
        let offset = route.offset;
        route.deliver(topic, payload.to_vec()).await?;
        Ok((0, offset))
    }

    fn flush(&self, _timeout: Duration) {
        // No-op for channels — messages are delivered immediately
    }

    fn is_healthy(&self) -> bool {
        true // Always healthy
    }
}

/// In-process message consumer.
pub struct ChannelConsumer {
    bus: Arc<RwLock<HashMap<String, TopicChannel>>>,
    capacity: usize,
    subscriptions: Arc<RwLock<Vec<String>>>,
    /// Named consumer group (`None`: the topic's shared queue).
    group: Option<String>,
    /// Un-acked delivery tracking (test builds only).
    #[cfg(feature = "test-hooks")]
    hooks: Arc<hooks::Hooks>,
}

impl ChannelConsumer {
    fn get_receiver(&self, topic: &str) -> Receiver<Vec<u8>> {
        // Fast path
        if self.group.is_none() {
            let topics = self.bus.read();
            if let Some(tc) = topics.get(topic) {
                return tc.receiver.clone();
            }
        }

        // Create topic (and group) if it doesn't exist yet
        let mut topics = self.bus.write();
        let tc = topics.entry(topic.to_string()).or_insert_with(|| {
            debug!(topic = topic, "Created topic channel (from consumer)");
            TopicChannel::new(self.capacity)
        });
        match self.group {
            Some(ref group) => tc.group_receiver(group, self.capacity),
            None => tc.receiver.clone(),
        }
    }
}

#[async_trait::async_trait]
impl MessageConsumer for ChannelConsumer {
    fn subscribe(&self, topics: &[&str]) -> Result<()> {
        let mut subs = self.subscriptions.write();
        for topic in topics {
            if !subs.contains(&topic.to_string()) {
                subs.push(topic.to_string());
            }
            if self.group.is_some() {
                // Register the group now: it receives what is sent from here on.
                self.get_receiver(topic);
            }
        }
        debug!(topics = ?topics, "Channel consumer subscribed");
        Ok(())
    }

    async fn process<T, F, Fut>(&self, mut handler: F) -> Result<()>
    where
        T: DeserializeOwned + Send + 'static,
        F: FnMut(T, MessageMetadata) -> Fut + Send,
        Fut: std::future::Future<Output = Result<()>> + Send,
    {
        let topics: Vec<String> = self.subscriptions.read().clone();
        if topics.is_empty() {
            return Err(ScrapixError::Queue("No topics subscribed".into()));
        }

        // For single-topic subscriptions (most common), use direct receive
        let receiver = self.get_receiver(&topics[0]);
        let mut offset = 0i64;

        while let Ok(bytes) = receiver.recv().await {
            let metadata = MessageMetadata {
                topic: topics[0].clone(),
                partition: 0,
                offset,
                key: None,
                timestamp: Some(chrono::Utc::now().timestamp_millis()),
            };
            offset += 1;

            match serde_json::from_slice::<T>(&bytes) {
                Ok(payload) => {
                    if let Err(e) = handler(payload, metadata.clone()).await {
                        error!(
                            topic = %metadata.topic,
                            offset = metadata.offset,
                            error = %e,
                            "Handler error"
                        );
                    }
                }
                Err(e) => {
                    error!(
                        topic = %metadata.topic,
                        offset = metadata.offset,
                        error = %e,
                        "Deserialization error"
                    );
                }
            }
        }

        Ok(())
    }

    async fn process_concurrent<T, F, Fut>(
        &self,
        handler: F,
        concurrency: usize,
        shutdown: Arc<AtomicBool>,
    ) -> Result<()>
    where
        T: DeserializeOwned + Send + 'static,
        F: Fn(T, MessageMetadata) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<()>> + Send + 'static,
    {
        let handler = Arc::new(handler);
        self.process_with_ack::<T, _, _>(
            move |payload, metadata, ack| {
                let handler = handler.clone();
                async move {
                    match handler(payload, metadata.clone()).await {
                        Ok(()) => ack.ack(),
                        Err(e) => {
                            error!(
                                topic = %metadata.topic,
                                offset = metadata.offset,
                                error = %e,
                                "Handler error, leaving message uncommitted for redelivery"
                            );
                            // Drop `ack` without calling it. The channel bus has no
                            // notion of offset commits, so this only changes logging
                            // — nothing is actually redelivered.
                        }
                    }
                }
            },
            concurrency,
            shutdown,
        )
        .await
    }

    async fn process_with_ack<T, F, Fut>(
        &self,
        handler: F,
        concurrency: usize,
        shutdown: Arc<AtomicBool>,
    ) -> Result<()>
    where
        T: DeserializeOwned + Send + 'static,
        F: Fn(T, MessageMetadata, scrapix_core::Ack) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let topics: Vec<String> = self.subscriptions.read().clone();
        if topics.is_empty() {
            return Err(ScrapixError::Queue("No topics subscribed".into()));
        }

        let receiver = self.get_receiver(&topics[0]);
        let topic_name = topics[0].clone();
        let handler = Arc::new(handler);
        let semaphore = Arc::new(tokio::sync::Semaphore::new(concurrency.max(1)));
        let mut offset = 0i64;

        loop {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            #[cfg(feature = "test-hooks")]
            if self.hooks.crashed() {
                break;
            }

            match tokio::time::timeout(Duration::from_millis(100), receiver.recv()).await {
                Ok(Ok(bytes)) => {
                    // Tracked from receipt: a crash while waiting for a
                    // handler permit must not lose the message either.
                    #[cfg(feature = "test-hooks")]
                    let token = self.hooks.track(&topic_name, &bytes);
                    let metadata = MessageMetadata {
                        topic: topic_name.clone(),
                        partition: 0,
                        offset,
                        key: None,
                        timestamp: Some(chrono::Utc::now().timestamp_millis()),
                    };
                    offset += 1;

                    match serde_json::from_slice::<T>(&bytes) {
                        Ok(payload) => {
                            let permit = match semaphore.clone().acquire_owned().await {
                                Ok(permit) => permit,
                                Err(_) => break,
                            };

                            let handler = handler.clone();
                            // The in-process channel bus has no durable offset to
                            // commit, so `Ack` here is a no-op — it exists only to
                            // keep the handler signature uniform with Kafka. Test
                            // builds record it so un-acked messages can be
                            // redelivered (`redeliver_unacked`).
                            #[cfg(not(feature = "test-hooks"))]
                            let ack = scrapix_core::Ack::noop();
                            #[cfg(feature = "test-hooks")]
                            let ack = {
                                if self.hooks.crashed() {
                                    // Left un-acked, as a crashed process would.
                                    break;
                                }
                                self.hooks.ack_for(token)
                            };
                            let _task = tokio::spawn(async move {
                                handler(payload, metadata, ack).await;
                                drop(permit);
                            });
                            #[cfg(feature = "test-hooks")]
                            self.hooks.register(_task.abort_handle());
                        }
                        Err(e) => {
                            // A poison message is never redelivered.
                            #[cfg(feature = "test-hooks")]
                            self.hooks.ack_for(token).ack();
                            error!(
                                topic = %metadata.topic,
                                offset = metadata.offset,
                                error = %e,
                                "Deserialization error"
                            );
                        }
                    }
                }
                Ok(Err(_)) => {
                    // Channel closed
                    break;
                }
                Err(_) => {
                    // Timeout — check shutdown and continue
                }
            }
        }

        // Wait for all in-flight tasks
        let _ = semaphore.acquire_many(concurrency.max(1) as u32).await;
        Ok(())
    }

    async fn poll_one<T: DeserializeOwned + Send>(&self, timeout: Duration) -> Result<Option<T>> {
        let topics: Vec<String> = self.subscriptions.read().clone();
        if topics.is_empty() {
            return Err(ScrapixError::Queue("No topics subscribed".into()));
        }

        let receiver = self.get_receiver(&topics[0]);

        match tokio::time::timeout(timeout, receiver.recv()).await {
            Ok(Ok(bytes)) => {
                let payload = serde_json::from_slice::<T>(&bytes)
                    .map_err(|e| ScrapixError::Queue(format!("Deserialization failed: {}", e)))?;
                Ok(Some(payload))
            }
            Ok(Err(_)) => Ok(None), // Channel closed
            Err(_) => Ok(None),     // Timeout
        }
    }
}

/// Test-only hooks simulating Kafka's un-acked redelivery on the channel
/// bus (feature `test-hooks`): every message a `process_with_ack` consumer
/// receives is recorded until its `Ack` fires; [`ChannelConsumer::crash`]
/// kills the consumer and its in-flight handlers (their acks never fire),
/// and [`ChannelConsumer::redeliver_unacked`] republishes what was never
/// acked, like a consumer-group rebalance after a crash.
#[cfg(feature = "test-hooks")]
mod hooks {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;

    use parking_lot::Mutex;

    /// Un-acked messages by delivery token: `(topic, payload)`.
    type Unacked = Arc<Mutex<HashMap<u64, (String, Vec<u8>)>>>;

    #[derive(Default)]
    pub(super) struct Hooks {
        next: AtomicU64,
        crashed: AtomicBool,
        pub(super) unacked: Unacked,
        tasks: Mutex<Vec<tokio::task::AbortHandle>>,
    }

    impl Hooks {
        pub(super) fn track(&self, topic: &str, bytes: &[u8]) -> u64 {
            let token = self.next.fetch_add(1, Ordering::Relaxed);
            self.unacked
                .lock()
                .insert(token, (topic.to_string(), bytes.to_vec()));
            token
        }

        pub(super) fn ack_for(&self, token: u64) -> scrapix_core::Ack {
            let unacked = self.unacked.clone();
            scrapix_core::Ack::from_fn(move || {
                unacked.lock().remove(&token);
            })
        }

        pub(super) fn register(&self, task: tokio::task::AbortHandle) {
            let mut tasks = self.tasks.lock();
            tasks.retain(|t| !t.is_finished());
            tasks.push(task);
        }

        pub(super) fn crashed(&self) -> bool {
            self.crashed.load(Ordering::SeqCst)
        }

        pub(super) fn crash(&self) {
            self.crashed.store(true, Ordering::SeqCst);
            for task in self.tasks.lock().drain(..) {
                task.abort();
            }
        }
    }
}

#[cfg(feature = "test-hooks")]
impl ChannelConsumer {
    /// Number of received messages whose `Ack` has not fired yet.
    pub fn unacked_count(&self) -> usize {
        self.hooks.unacked.lock().len()
    }

    /// Simulate a crash: stop receiving and abort every in-flight handler
    /// (their acks never fire). Messages stay recorded as un-acked.
    pub fn crash(&self) {
        self.hooks.crash();
    }

    /// Republish every un-acked message to its topic's shared queue (what a
    /// Kafka rebalance does after a consumer died), returning how many.
    /// Meant to be called after [`crash`](Self::crash).
    pub async fn redeliver_unacked(&self) -> usize {
        let pending: Vec<(String, Vec<u8>)> = {
            let mut unacked = self.hooks.unacked.lock();
            let mut entries: Vec<(u64, (String, Vec<u8>))> = unacked.drain().collect();
            entries.sort_by_key(|(token, _)| *token);
            entries.into_iter().map(|(_, m)| m).collect()
        };
        let producer = ChannelProducer {
            bus: self.bus.clone(),
            capacity: self.capacity,
        };
        let mut sent = 0;
        for (topic, bytes) in pending {
            if producer.route(&topic).deliver(&topic, bytes).await.is_ok() {
                sent += 1;
            }
        }
        sent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn poll(c: &ChannelConsumer) -> Option<String> {
        c.poll_one::<String>(Duration::from_millis(100))
            .await
            .unwrap()
    }

    #[cfg(feature = "test-hooks")]
    #[tokio::test]
    async fn crashed_consumer_redelivers_only_unacked_messages() {
        let bus = ChannelBus::new();
        let p = bus.producer();
        for m in ["ok", "stuck"] {
            p.send("t", None, &m.to_string()).await.unwrap();
        }
        let c = Arc::new(bus.consumer());
        c.subscribe(&["t"]).unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let acked_ok = Arc::new(AtomicBool::new(false));
        let stuck_started = Arc::new(AtomicBool::new(false));
        let run = {
            let c = c.clone();
            let (acked_ok, stuck_started) = (acked_ok.clone(), stuck_started.clone());
            tokio::spawn(async move {
                c.process_with_ack::<String, _, _>(
                    move |m, _, ack| {
                        let (acked_ok, stuck_started) = (acked_ok.clone(), stuck_started.clone());
                        async move {
                            if m == "ok" {
                                ack.ack();
                                acked_ok.store(true, Ordering::SeqCst);
                            } else {
                                stuck_started.store(true, Ordering::SeqCst);
                                std::future::pending::<()>().await;
                            }
                        }
                    },
                    4,
                    shutdown,
                )
                .await
            })
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !(acked_ok.load(Ordering::SeqCst) && stuck_started.load(Ordering::SeqCst)) {
            assert!(std::time::Instant::now() < deadline, "never settled");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(c.unacked_count(), 1);
        c.crash();
        tokio::time::timeout(Duration::from_secs(2), run)
            .await
            .expect("crashed consumer stops")
            .unwrap()
            .unwrap();
        assert_eq!(c.redeliver_unacked().await, 1);
        let next = bus.consumer();
        next.subscribe(&["t"]).unwrap();
        assert_eq!(poll(&next).await.as_deref(), Some("stuck"));
        assert_eq!(poll(&next).await, None);
    }

    #[tokio::test]
    async fn named_groups_each_receive_every_message() {
        let bus = ChannelBus::new();
        let a = bus.consumer_in_group("a");
        a.subscribe(&["t"]).unwrap();
        let b = bus.consumer_in_group("b");
        b.subscribe(&["t"]).unwrap();
        let p = bus.producer();
        p.send("t", None, &"m1".to_string()).await.unwrap();
        p.send("t", None, &"m2".to_string()).await.unwrap();
        for c in [&a, &b] {
            assert_eq!(poll(c).await.as_deref(), Some("m1"));
            assert_eq!(poll(c).await.as_deref(), Some("m2"));
            assert_eq!(poll(c).await, None);
        }
    }

    #[tokio::test]
    async fn default_consumers_still_share_one_queue() {
        let bus = ChannelBus::new();
        let p = bus.producer();
        p.send("t", None, &"m1".to_string()).await.unwrap();
        let a = bus.consumer();
        a.subscribe(&["t"]).unwrap();
        let b = bus.consumer();
        b.subscribe(&["t"]).unwrap();
        // Buffered before any consumer existed, delivered once.
        let got = [poll(&a).await, poll(&b).await];
        assert_eq!(got.iter().flatten().count(), 1, "{got:?}");
    }

    #[tokio::test]
    async fn group_topic_never_blocks_the_producer_when_nobody_reads() {
        let bus = ChannelBus::with_capacity(2);
        let g = bus.consumer_in_group("g");
        g.subscribe(&["t"]).unwrap();
        let p = bus.producer();
        for i in 0..10 {
            tokio::time::timeout(
                Duration::from_millis(200),
                p.send("t", None, &format!("m{i}")),
            )
            .await
            .expect("send must not block")
            .unwrap();
        }
        // The group kept the oldest messages it had room for.
        assert_eq!(poll(&g).await.as_deref(), Some("m0"));
    }
}
