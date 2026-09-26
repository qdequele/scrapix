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

impl std::fmt::Debug for Ack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.done.is_some() {
            "Ack(pending)"
        } else {
            "Ack(done)"
        })
    }
}
