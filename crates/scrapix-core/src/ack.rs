//! Completion token for at-least-once message consumption.
//!
//! A handler is handed an `Ack` alongside the message it must process. Calling
//! `ack()` marks the message done (its offset becomes eligible for commit).
//! Dropping the `Ack` without acking leaves the offset uncommitted, so the
//! message is redelivered after a restart or partition rebalance.

/// Completion token handed to a handler. Calling `ack()` marks the message done;
/// dropping it without acking leaves the offset uncommitted (redelivered after a
/// restart or rebalance).
pub struct Ack {
    done: Option<Box<dyn FnOnce() + Send>>,
}

impl Ack {
    /// An `Ack` that does nothing when acked (or dropped). Used by backends that
    /// have no notion of offset commits (e.g. in-process channels).
    pub fn noop() -> Self {
        Self { done: None }
    }

    /// Build an ack that runs `f` exactly once when acked.
    pub fn from_fn(f: impl FnOnce() + Send + 'static) -> Self {
        Self {
            done: Some(Box::new(f)),
        }
    }

    /// Mark the message as done, running the completion callback (if any).
    pub fn ack(mut self) {
        if let Some(f) = self.done.take() {
            f();
        }
    }
}

impl Ack {
    /// Split this ack into `n` child acks: the original is acked once every
    /// child has been acked. If any child is dropped without acking, the
    /// original is never acked (the message is redelivered). `n == 0` acks
    /// the original immediately and returns no children.
    pub fn split(self, n: usize) -> Vec<Ack> {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};

        if n == 0 {
            self.ack();
            return Vec::new();
        }
        if n == 1 {
            return vec![self];
        }
        let remaining = Arc::new(AtomicUsize::new(n));
        let parent = Arc::new(Mutex::new(Some(self)));
        (0..n)
            .map(|_| {
                let (remaining, parent) = (remaining.clone(), parent.clone());
                Ack::from_fn(move || {
                    if remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
                        let taken = parent.lock().ok().and_then(|mut p| p.take());
                        if let Some(ack) = taken {
                            ack.ack();
                        }
                    }
                })
            })
            .collect()
    }
}

impl std::fmt::Debug for Ack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.done.is_some() {
            "Ack(pending)"
        } else {
            "Ack(done)"
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn counting() -> (Ack, Arc<AtomicUsize>) {
        let c = Arc::new(AtomicUsize::new(0));
        let c2 = c.clone();
        (
            Ack::from_fn(move || {
                c2.fetch_add(1, Ordering::SeqCst);
            }),
            c,
        )
    }

    #[test]
    fn split_acks_parent_after_all_children() {
        let (ack, count) = counting();
        let mut kids = ack.split(3);
        kids.pop().unwrap().ack();
        kids.pop().unwrap().ack();
        assert_eq!(count.load(Ordering::SeqCst), 0);
        kids.pop().unwrap().ack();
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn split_dropped_child_never_acks_parent() {
        let (ack, count) = counting();
        let mut kids = ack.split(2);
        kids.pop().unwrap().ack();
        drop(kids);
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn split_zero_acks_immediately() {
        let (ack, count) = counting();
        assert!(ack.split(0).is_empty());
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}
