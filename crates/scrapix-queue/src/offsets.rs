//! Per-partition offset tracking for at-least-once consumption.
//!
//! Kafka commits are "next offset to read" per partition, so an offset can only be
//! committed once it and every earlier offset of that partition is done. Handlers
//! finish out of order; this tracker computes the highest safe commit.

use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant};

#[derive(Debug, Default)]
struct PartitionState {
    in_flight: BTreeSet<i64>,
    /// Highest offset ever begun + 1.
    next_after_highest: i64,
    /// The first offset ever begun for this partition (since the last `revoke`).
    /// Used as the baseline below which nothing has ever been observed to complete.
    start: Option<i64>,
    /// Last value returned by `take_commits` for this partition.
    last_committed: Option<i64>,
    /// The last time this partition's committable offset advanced (or, before any
    /// progress, the time we started tracking it). Used to detect a partition
    /// that's stuck — in flight but not progressing, e.g. a handler wedged or
    /// repeatedly erroring on the same offset.
    last_progress_at: Option<Instant>,
    /// The last time `stuck_partitions` reported this partition, so we only warn
    /// at most once per cooldown window instead of on every check.
    last_warned_at: Option<Instant>,
}

impl PartitionState {
    /// The offset below which everything is already known to be safe (either the
    /// last commit we emitted, or — if we haven't committed anything yet — the
    /// first offset we ever began tracking).
    fn base(&self) -> i64 {
        self.last_committed
            .unwrap_or_else(|| self.start.unwrap_or(0))
    }

    fn committable(&self) -> Option<i64> {
        match self.in_flight.iter().next().copied() {
            // Nothing in flight: everything begun so far is done.
            None => Some(self.next_after_highest),
            // Something still in flight: only safe to commit if the front of the
            // queue has moved past the baseline, i.e. at least one offset below it
            // has completed since the baseline was established.
            Some(front) if front > self.base() => Some(front),
            Some(_) => None,
        }
    }
}

#[derive(Debug, Default)]
pub struct OffsetTracker {
    parts: HashMap<(String, i32), PartitionState>,
}

impl OffsetTracker {
    pub fn begin(&mut self, topic: &str, partition: i32, offset: i64) {
        let now = Instant::now();
        let st = self
            .parts
            .entry((topic.to_string(), partition))
            .or_default();
        if st.start.is_none() {
            st.start = Some(offset);
            st.last_progress_at = Some(now);
        }
        st.in_flight.insert(offset);
        st.next_after_highest = st.next_after_highest.max(offset + 1);
    }

    pub fn complete(&mut self, topic: &str, partition: i32, offset: i64) {
        if let Some(st) = self.parts.get_mut(&(topic.to_string(), partition)) {
            st.in_flight.remove(&offset);
        }
    }

    /// Forget a partition (called on revocation). In-flight completions that arrive
    /// later are ignored because their offsets are no longer in `in_flight`.
    pub fn revoke(&mut self, topic: &str, partition: i32) {
        self.parts.remove(&(topic.to_string(), partition));
    }

    /// Offsets that advanced since the last call: `(topic, partition, next_offset)`.
    pub fn take_commits(&mut self) -> Vec<(String, i32, i64)> {
        let now = Instant::now();
        let mut out = Vec::new();
        for ((topic, partition), st) in self.parts.iter_mut() {
            if let Some(c) = st.committable() {
                if st.last_committed.map_or(true, |prev| c > prev) {
                    st.last_committed = Some(c);
                    st.last_progress_at = Some(now);
                    out.push((topic.clone(), *partition, c));
                }
            }
        }
        out.sort();
        out
    }

    /// Partitions that have made no committable progress for at least `stuck_after`
    /// while still holding in-flight offsets (e.g. a handler that errors forever, or
    /// hangs, on the same message). Returns `(topic, partition, stuck_offset)` for
    /// each such partition, at most once per `warn_cooldown` window per partition —
    /// repeated calls within the cooldown return nothing for a partition already
    /// reported, so callers can poll this freely (e.g. once per tick) without
    /// spamming logs.
    pub fn stuck_partitions(
        &mut self,
        now: Instant,
        stuck_after: Duration,
        warn_cooldown: Duration,
    ) -> Vec<(String, i32, i64)> {
        let mut out = Vec::new();
        for ((topic, partition), st) in self.parts.iter_mut() {
            let Some(&front) = st.in_flight.iter().next() else {
                continue; // fully caught up: nothing can be "stuck"
            };
            let Some(progress_at) = st.last_progress_at else {
                continue;
            };
            if now.saturating_duration_since(progress_at) < stuck_after {
                continue;
            }
            if let Some(warned_at) = st.last_warned_at {
                if now.saturating_duration_since(warned_at) < warn_cooldown {
                    continue;
                }
            }
            st.last_warned_at = Some(now);
            out.push((topic.clone(), *partition, front));
        }
        out.sort();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commits_only_contiguous_prefix() {
        let mut t = OffsetTracker::default();
        for o in 10..15 {
            t.begin("t", 0, o);
        }
        t.complete("t", 0, 11);
        t.complete("t", 0, 12);
        assert!(t.take_commits().is_empty(), "10 still in flight");
        t.complete("t", 0, 10);
        assert_eq!(t.take_commits(), vec![("t".to_string(), 0, 13)]);
        t.complete("t", 0, 14);
        assert!(t.take_commits().is_empty(), "13 still in flight");
        t.complete("t", 0, 13);
        assert_eq!(t.take_commits(), vec![("t".to_string(), 0, 15)]);
    }

    #[test]
    fn take_commits_is_idempotent_until_progress() {
        let mut t = OffsetTracker::default();
        t.begin("t", 1, 0);
        t.complete("t", 1, 0);
        assert_eq!(t.take_commits().len(), 1);
        assert!(t.take_commits().is_empty());
    }

    #[test]
    fn partitions_are_independent() {
        let mut t = OffsetTracker::default();
        t.begin("t", 0, 5);
        t.begin("t", 1, 7);
        t.complete("t", 1, 7);
        assert_eq!(t.take_commits(), vec![("t".to_string(), 1, 8)]);
    }

    #[test]
    fn redelivery_after_rebalance_resets_partition() {
        // Offsets 5..=6 were in flight, partition got revoked, and after re-assignment
        // the broker redelivers from 6 (5 was already committed). Completions of the
        // old attempt must not push the committed offset past work that is in flight
        // again.
        let mut t = OffsetTracker::default();
        t.begin("t", 0, 5);
        t.begin("t", 0, 6);
        t.complete("t", 0, 5);
        assert_eq!(t.take_commits(), vec![("t".to_string(), 0, 6)]);
        t.revoke("t", 0);
        t.begin("t", 0, 6); // redelivered
        t.complete("t", 0, 6);
        assert_eq!(t.take_commits(), vec![("t".to_string(), 0, 7)]);
    }

    #[test]
    fn completion_for_unknown_offset_is_ignored() {
        let mut t = OffsetTracker::default();
        t.complete("t", 0, 99);
        assert!(t.take_commits().is_empty());
    }

    #[test]
    fn err_at_n_blocks_commit_even_after_n_plus_one_succeeds() {
        // N (offset 5) fails and is never completed; N+1 (offset 6) succeeds. The
        // commit point must stay behind the failed offset — a handler error at N
        // must not be papered over by a later success at N+1.
        let mut t = OffsetTracker::default();
        t.begin("t", 0, 5);
        t.begin("t", 0, 6);
        t.complete("t", 0, 6); // N+1 succeeds; N (5) never completes
        assert!(
            t.take_commits().is_empty(),
            "offset 5 failed/never acked — nothing past it may commit"
        );
    }

    #[test]
    fn stuck_partition_warns_after_threshold_then_respects_cooldown() {
        let mut t = OffsetTracker::default();
        t.begin("t", 0, 5); // never completed: handler wedged or erroring forever
        let started = Instant::now();

        // Well within the stuck threshold: not reported yet.
        assert!(t
            .stuck_partitions(started, Duration::from_secs(60), Duration::from_secs(60))
            .is_empty());

        let past_threshold = started + Duration::from_secs(61);
        assert_eq!(
            t.stuck_partitions(
                past_threshold,
                Duration::from_secs(60),
                Duration::from_secs(60)
            ),
            vec![("t".to_string(), 0, 5)]
        );

        // Still inside the cooldown window: no repeat warning.
        let within_cooldown = past_threshold + Duration::from_secs(10);
        assert!(t
            .stuck_partitions(
                within_cooldown,
                Duration::from_secs(60),
                Duration::from_secs(60)
            )
            .is_empty());

        // Cooldown has elapsed: warns again.
        let after_cooldown = past_threshold + Duration::from_secs(61);
        assert_eq!(
            t.stuck_partitions(
                after_cooldown,
                Duration::from_secs(60),
                Duration::from_secs(60)
            ),
            vec![("t".to_string(), 0, 5)]
        );
    }

    #[test]
    fn stuck_partitions_ignores_fully_caught_up_partition() {
        let mut t = OffsetTracker::default();
        t.begin("t", 0, 5);
        t.complete("t", 0, 5);
        let far_future = Instant::now() + Duration::from_secs(600);
        assert!(t
            .stuck_partitions(far_future, Duration::from_secs(60), Duration::from_secs(60))
            .is_empty());
    }
}
