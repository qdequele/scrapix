//! Job control on the worker side: a bounded set of jobs that were
//! cancelled or finished, fed by a [`JobControl`] consumer on
//! [`names::JOB_STATUS`](crate::topic_names::JOB_STATUS).
//!
//! Workers (crawler, content) consume `JOB_STATUS` in a per-worker group so
//! each one sees every control message, and ack the messages of jobs in the
//! set without doing any work (spec R5: cancel stops every worker). Pause
//! is not recorded: a paused job's in-flight messages are processed
//! normally, the frontier just stops dispatching new ones.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use parking_lot::Mutex;
use tracing::{debug, error, info};

use crate::topics::{JobAction, JobControl};
use crate::traits::AnyConsumer;

/// Default capacity of a worker's [`CancelledJobs`] set.
pub const CANCELLED_JOBS_CAP: usize = 10_000;

/// Consumer group a worker uses for `JOB_STATUS`: one group per worker, so
/// every worker receives every control message.
pub fn control_group_id(group_id: &str, worker_id: &str) -> String {
    format!("{group_id}-control-{worker_id}")
}

/// Bounded set of cancelled/finished job ids. At capacity the oldest
/// inserted id is evicted (insertion order, not an LRU).
pub struct CancelledJobs {
    inner: Mutex<Inner>,
    cap: usize,
}

struct Inner {
    set: HashSet<String>,
    order: VecDeque<String>,
}

impl CancelledJobs {
    pub fn new(cap: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                set: HashSet::new(),
                order: VecDeque::new(),
            }),
            cap: cap.max(1),
        }
    }

    pub fn insert(&self, job_id: &str) {
        let mut inner = self.inner.lock();
        if inner.set.contains(job_id) {
            return;
        }
        while inner.order.len() >= self.cap {
            match inner.order.pop_front() {
                Some(oldest) => {
                    inner.set.remove(&oldest);
                }
                None => break,
            }
        }
        inner.set.insert(job_id.to_string());
        inner.order.push_back(job_id.to_string());
    }

    pub fn contains(&self, job_id: &str) -> bool {
        self.inner.lock().set.contains(job_id)
    }

    pub fn len(&self) -> usize {
        self.inner.lock().order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Apply one control message: Cancel and Finish mark the job dead.
    pub fn apply(&self, control: &JobControl) {
        match control.action {
            JobAction::Cancel | JobAction::Finish => {
                debug!(job_id = %control.job_id, action = ?control.action, "Job stopped: skipping its messages");
                self.insert(&control.job_id);
            }
            JobAction::Pause | JobAction::Resume => {}
        }
    }
}

impl Default for CancelledJobs {
    fn default() -> Self {
        Self::new(CANCELLED_JOBS_CAP)
    }
}

/// Consume `JobControl` messages from `consumer` (already subscribed to
/// `JOB_STATUS`) into `set` until `shutdown`. Every message is acked:
/// control is best effort (a missed Cancel only means a worker does work
/// the frontier already stopped feeding).
pub fn spawn_listener(
    consumer: Arc<AnyConsumer>,
    set: Arc<CancelledJobs>,
    shutdown: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        info!("Job control listener started");
        let result = consumer
            .process_with_ack::<JobControl, _, _>(
                move |control, _metadata, ack| {
                    let set = set.clone();
                    async move {
                        set.apply(&control);
                        ack.ack();
                    }
                },
                1,
                shutdown,
            )
            .await;
        if let Err(e) = result {
            error!(error = %e, "Job control listener stopped");
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{topic_names, AnyConsumer, AnyProducer, ChannelBus, JobAction, JobControl};
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn evicts_oldest_job_at_capacity() {
        let set = CancelledJobs::new(2);
        set.insert("a");
        set.insert("b");
        set.insert("a"); // already present: no reordering, no growth
        set.insert("c");
        assert!(!set.contains("a"));
        assert!(set.contains("b"));
        assert!(set.contains("c"));
        assert_eq!(set.len(), 2);
    }

    #[tokio::test]
    async fn listener_records_cancel_and_finish_but_not_pause() {
        let bus = ChannelBus::new();
        let consumer = AnyConsumer::channel(bus.consumer_in_group("w1"));
        consumer.subscribe(&[topic_names::JOB_STATUS]).unwrap();
        let set = Arc::new(CancelledJobs::new(10));
        let shutdown = Arc::new(AtomicBool::new(false));
        let handle = spawn_listener(Arc::new(consumer), set.clone(), shutdown.clone());

        let p = AnyProducer::channel(bus.producer());
        for (job, action) in [
            ("c", JobAction::Cancel),
            ("f", JobAction::Finish),
            ("p", JobAction::Pause),
            ("r", JobAction::Resume),
        ] {
            p.send(
                topic_names::JOB_STATUS,
                Some(job),
                &JobControl::new(job, action),
            )
            .await
            .unwrap();
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !(set.contains("c") && set.contains("f")) && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
        handle.await.unwrap();
        assert!(set.contains("c") && set.contains("f"));
        assert!(!set.contains("p") && !set.contains("r"));
    }
}
