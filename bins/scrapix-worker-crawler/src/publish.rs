//! Publishing helpers of the crawler worker: concurrent sends, the set of
//! links this worker already published per job, and message size checks.

use std::collections::hash_map::RandomState;
use std::collections::{HashMap, HashSet};
use std::hash::BuildHasher;
use std::time::{Duration, Instant};

use scrapix_core::ScrapixError;
use scrapix_queue::{AnyProducer, ProducerConfig};
use serde::Serialize;
use tokio::task::JoinSet;

/// How many sends of one page (or one sitemap) are in flight at once.
///
/// A send waits for its delivery report: about the producer's linger
/// (20 ms) plus the broker ack. Sent one after the other, 100 links took
/// about 2 s; in flight together they share the same batches.
pub(crate) const SEND_CONCURRENCY: usize = 256;

/// Room kept under the producer's message size limit for the record's key
/// and the protocol framing around it.
const MESSAGE_SIZE_MARGIN: usize = 4 * 1024;

/// Send `messages` (`(key, payload)`) to `topic`, at most `limit` in flight
/// at once, and wait for all of them. Returns one result per message, in
/// the order of `messages`.
pub(crate) async fn send_concurrently<T, I>(
    producer: &AnyProducer,
    topic: &'static str,
    messages: I,
    limit: usize,
) -> Vec<scrapix_core::Result<()>>
where
    T: Serialize + Send + Sync + 'static,
    I: IntoIterator<Item = (String, T)>,
{
    fn record(
        results: &mut [Option<scrapix_core::Result<()>>],
        joined: Result<(usize, scrapix_core::Result<()>), tokio::task::JoinError>,
    ) {
        // A send task that panicked or was aborted leaves its slot empty:
        // reported as failed below.
        if let Ok((index, result)) = joined {
            results[index] = Some(result);
        }
    }

    let limit = limit.max(1);
    let mut results: Vec<Option<scrapix_core::Result<()>>> = Vec::new();
    // Dropping the set (an abandoned handler) aborts the sends in flight.
    let mut sends = JoinSet::new();
    for (index, (key, message)) in messages.into_iter().enumerate() {
        results.push(None);
        while sends.len() >= limit {
            if let Some(joined) = sends.join_next().await {
                record(&mut results, joined);
            }
        }
        let producer = producer.clone();
        sends.spawn(async move {
            let result = producer.send(topic, Some(&key), &message).await.map(|_| ());
            (index, result)
        });
    }
    while let Some(joined) = sends.join_next().await {
        record(&mut results, joined);
    }
    results
        .into_iter()
        .map(|result| {
            result.unwrap_or_else(|| Err(ScrapixError::Queue("send task failed".to_string())))
        })
        .collect()
}

/// The largest serialized message the worker's Kafka producer accepts
/// (its `message.max.bytes`, less a margin for the key and framing).
pub(crate) fn kafka_max_message_bytes() -> usize {
    ProducerConfig::default()
        .max_message_bytes
        .saturating_sub(MESSAGE_SIZE_MARGIN)
}

/// Size of `value` once serialized to JSON, without building the bytes.
pub(crate) fn serialized_len<T: Serialize>(value: &T) -> Option<usize> {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, value).ok()?;
    Some(counter.0)
}

/// Whether a send failed because the message is larger than the producer
/// or the broker accepts: sending it again can never succeed.
///
/// The queue reports every send failure as `ScrapixError::Queue(text)`, so
/// this reads the text: librdkafka's `MessageSizeTooLarge` code, whose
/// description is "Broker: Message size too large" (the same code for the
/// client-side check and for the broker's refusal).
pub(crate) fn is_message_too_large(error: &ScrapixError) -> bool {
    match error {
        ScrapixError::Queue(text) => {
            text.contains("MessageSizeTooLarge") || text.contains("Message size too large")
        }
        _ => false,
    }
}

/// Most link hashes one generation of a job holds; a job holds at most two
/// generations.
const LINKS_PER_GENERATION: usize = 50_000;
/// Most jobs tracked at once; past it the least recently used job is
/// forgotten.
const MAX_TRACKED_JOBS: usize = 64;
/// How long a generation takes links before it is rotated out.
const GENERATION_MAX_AGE: Duration = Duration::from_secs(600);
/// A job without any page for this long is forgotten.
const JOB_IDLE_AFTER: Duration = Duration::from_secs(1800);

/// The links this worker already published to the frontier, per job, so a
/// link every page repeats (navigation, sidebar, footer) is sent once
/// instead of once per page. The frontier's dedup stays the authority: this
/// only skips messages it would refuse as duplicates.
///
/// Rules that keep it safe:
/// - A link is recorded ([`Self::record`]) only after it was sent **and**
///   the page's accounting event counting it was sent. A handler that fails
///   in between records nothing, so the redelivered page sends its links
///   again and reports them.
/// - Memory is bounded: URLs are kept as 64-bit hashes (keyed per process),
///   at most `2 x LINKS_PER_GENERATION` per job and `MAX_TRACKED_JOBS` jobs.
/// - Entries expire: a job's links live in two generations, rotated when
///   the current one is full or `GENERATION_MAX_AGE` old. A link the
///   frontier refused for a reason that can pass later (a full queue) is
///   therefore sent again by a later page, as it was before, only later.
/// - Jobs that ended (the worker's cancelled/finished set) or went idle are
///   dropped when a new job needs room.
///
/// It is per worker and in memory: another worker, or this one after a
/// restart, sends a link again and the frontier refuses the duplicate.
pub(crate) struct PublishedLinks {
    hasher: RandomState,
    jobs: parking_lot::Mutex<Jobs>,
    per_generation: usize,
    max_jobs: usize,
    max_age: Duration,
}

#[derive(Default)]
struct Jobs {
    jobs: HashMap<String, JobLinks>,
    next_seq: u64,
}

struct JobLinks {
    current: HashSet<u64>,
    previous: HashSet<u64>,
    rotated_at: Instant,
    last_used: Instant,
    /// Order of the last `record` among all jobs (least recently used
    /// eviction).
    used_seq: u64,
}

impl JobLinks {
    fn new(now: Instant) -> Self {
        Self {
            current: HashSet::new(),
            previous: HashSet::new(),
            rotated_at: now,
            last_used: now,
            used_seq: 0,
        }
    }

    fn rotate(&mut self, now: Instant) {
        self.previous = std::mem::take(&mut self.current);
        self.rotated_at = now;
    }

    fn rotate_if_old(&mut self, now: Instant, max_age: Duration) {
        if now.duration_since(self.rotated_at) >= max_age {
            self.rotate(now);
        }
    }
}

impl Default for PublishedLinks {
    fn default() -> Self {
        Self::new(LINKS_PER_GENERATION, MAX_TRACKED_JOBS, GENERATION_MAX_AGE)
    }
}

impl PublishedLinks {
    pub(crate) fn new(per_generation: usize, max_jobs: usize, max_age: Duration) -> Self {
        Self {
            hasher: RandomState::new(),
            jobs: parking_lot::Mutex::new(Jobs::default()),
            per_generation: per_generation.max(1),
            max_jobs: max_jobs.max(1),
            max_age,
        }
    }

    /// The key of `url` in this set.
    pub(crate) fn key(&self, url: &str) -> u64 {
        self.hasher.hash_one(url)
    }

    /// For each key, whether this worker already published it for `job_id`.
    pub(crate) fn published(&self, job_id: &str, keys: &[u64]) -> Vec<bool> {
        let now = Instant::now();
        let mut state = self.jobs.lock();
        let Some(job) = state.jobs.get_mut(job_id) else {
            return vec![false; keys.len()];
        };
        job.rotate_if_old(now, self.max_age);
        keys.iter()
            .map(|key| job.current.contains(key) || job.previous.contains(key))
            .collect()
    }

    /// Record `keys` as published for `job_id`. `ended` tells whether a job
    /// is over (cancelled or finished): such jobs are dropped when room is
    /// needed for a new one, and nothing is recorded for them.
    pub(crate) fn record(&self, job_id: &str, keys: &[u64], ended: impl Fn(&str) -> bool) {
        if keys.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut state = self.jobs.lock();
        let Jobs { jobs, next_seq } = &mut *state;
        if !jobs.contains_key(job_id) {
            if ended(job_id) {
                return;
            }
            if jobs.len() >= self.max_jobs {
                jobs.retain(|id, job| {
                    !ended(id) && now.duration_since(job.last_used) < JOB_IDLE_AFTER
                });
            }
            if jobs.len() >= self.max_jobs {
                let oldest = jobs
                    .iter()
                    .min_by_key(|(_, job)| job.used_seq)
                    .map(|(id, _)| id.clone());
                if let Some(oldest) = oldest {
                    jobs.remove(&oldest);
                }
            }
            jobs.insert(job_id.to_string(), JobLinks::new(now));
        }
        let Some(job) = jobs.get_mut(job_id) else {
            return;
        };
        job.last_used = now;
        *next_seq += 1;
        job.used_seq = *next_seq;
        job.rotate_if_old(now, self.max_age);
        for key in keys {
            if job.current.len() >= self.per_generation {
                job.rotate(now);
            }
            job.current.insert(*key);
        }
    }

    /// Forget everything recorded for `job_id`.
    pub(crate) fn forget(&self, job_id: &str) {
        self.jobs.lock().jobs.remove(job_id);
    }

    #[cfg(test)]
    fn tracked_jobs(&self) -> usize {
        self.jobs.lock().jobs.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scrapix_queue::{AnyConsumer, ChannelBus, ProducerBuilder};

    fn never(_: &str) -> bool {
        false
    }

    #[test]
    fn a_link_is_published_only_after_it_was_recorded() {
        let links = PublishedLinks::default();
        let keys = [links.key("https://a.test/x"), links.key("https://a.test/y")];
        assert_eq!(links.published("job", &keys), vec![false, false]);
        links.record("job", &keys[..1], never);
        assert_eq!(links.published("job", &keys), vec![true, false]);
        // Per job: another job has published nothing.
        assert_eq!(links.published("other", &keys), vec![false, false]);
        links.forget("job");
        assert_eq!(links.published("job", &keys), vec![false, false]);
    }

    #[test]
    fn a_job_keeps_at_most_two_generations() {
        let links = PublishedLinks::new(2, 8, Duration::from_secs(3600));
        let keys: Vec<u64> = (0..6)
            .map(|i| links.key(&format!("https://a.test/{i}")))
            .collect();
        links.record("job", &keys, never);
        // Generations of 2: [0,1] was dropped, [2,3] is the previous one,
        // [4,5] the current one.
        assert_eq!(
            links.published("job", &keys),
            vec![false, false, true, true, true, true]
        );
    }

    #[test]
    fn links_expire_with_their_generation() {
        let links = PublishedLinks::new(100, 8, Duration::ZERO);
        let key = [links.key("https://a.test/x")];
        links.record("job", &key, never);
        // A zero max age rotates on every use: the first lookup moves the
        // link to the previous generation, the second one drops it.
        assert_eq!(links.published("job", &key), vec![true]);
        assert_eq!(links.published("job", &key), vec![false]);
    }

    #[test]
    fn ended_and_least_recently_used_jobs_make_room() {
        let links = PublishedLinks::new(100, 2, Duration::from_secs(3600));
        let key = [links.key("https://a.test/x")];
        links.record("job-1", &key, never);
        links.record("job-2", &key, never);
        // A finished job is dropped first.
        links.record("job-3", &key, |id| id == "job-2");
        assert_eq!(links.tracked_jobs(), 2);
        assert_eq!(links.published("job-1", &key), vec![true]);
        assert_eq!(links.published("job-2", &key), vec![false]);
        // Without one, the least recently used job goes.
        links.record("job-1", &key, never);
        links.record("job-4", &key, never);
        assert_eq!(links.tracked_jobs(), 2);
        assert_eq!(links.published("job-3", &key), vec![false]);
        assert_eq!(links.published("job-1", &key), vec![true]);
        // Nothing is recorded for a job that already ended.
        links.record("job-5", &key, |id| id == "job-5");
        assert_eq!(links.published("job-5", &key), vec![false]);
    }

    #[tokio::test]
    async fn sends_every_message_and_reports_each_result() {
        let bus = ChannelBus::new();
        let reader = AnyConsumer::from(bus.consumer());
        reader.subscribe(&["t"]).unwrap();
        let producer = AnyProducer::from(bus.producer());
        let messages = (0..1000u32).map(|i| (format!("k{i}"), i));
        let results = send_concurrently(&producer, "t", messages, 16).await;
        assert_eq!(results.len(), 1000);
        assert!(results.iter().all(Result::is_ok));
        let mut got = Vec::new();
        while let Some(i) = reader
            .poll_one::<u32>(Duration::from_millis(50))
            .await
            .unwrap()
        {
            got.push(i);
        }
        got.sort_unstable();
        assert_eq!(got, (0..1000).collect::<Vec<_>>());
    }

    /// A Kafka producer whose broker cannot be reached: sends fail when the
    /// (short) message timeout expires.
    fn unreachable_producer() -> AnyProducer {
        AnyProducer::from(
            ProducerBuilder::new("127.0.0.1:1")
                .message_timeout(Duration::from_millis(300))
                .build()
                .unwrap(),
        )
    }

    #[tokio::test]
    async fn a_failed_send_is_reported_for_its_message() {
        let producer = unreachable_producer();
        let messages = (0..3u32).map(|i| (format!("k{i}"), i));
        let results = send_concurrently(&producer, "t", messages, 2).await;
        assert_eq!(results.len(), 3);
        assert!(results.iter().all(Result::is_err), "{results:?}");
        assert!(!is_message_too_large(results[0].as_ref().unwrap_err()));
    }

    /// The producer refuses a message over its size limit with an error
    /// `is_message_too_large` recognizes, and accepts (to its queue) one at
    /// the limit the worker checks against.
    #[tokio::test]
    async fn the_producer_refuses_an_oversized_message() {
        let producer = unreachable_producer();
        let limit = ProducerConfig::default().max_message_bytes;
        let over = "x".repeat(limit + 1);
        let error = producer.send("t", Some("k"), &over).await.unwrap_err();
        assert!(is_message_too_large(&error), "{error}");

        // `"…"`: two quotes around the text.
        let at_limit = "x".repeat(kafka_max_message_bytes() - 2);
        assert_eq!(serialized_len(&at_limit), Some(kafka_max_message_bytes()));
        let error = producer.send("t", Some("k"), &at_limit).await.unwrap_err();
        assert!(!is_message_too_large(&error), "{error}");
    }

    #[test]
    fn serialized_len_counts_json_escapes() {
        assert_eq!(serialized_len(&"ab"), Some(4));
        assert_eq!(serialized_len(&"a\"\n"), Some(7));
        assert_eq!(
            serialized_len(&serde_json::json!({"a": 1})),
            Some(r#"{"a":1}"#.len())
        );
    }
}
