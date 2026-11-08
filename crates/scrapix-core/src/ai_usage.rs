//! What a job's LLM calls used and cost, reported on the job (`usage.ai`).

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Totals of a set of LLM calls (one page, one request, or a whole job).
///
/// `cost_micro_usd` is the provider cost the hosted AI gateway (Lumen)
/// settled for those calls (its `x-lumen-cost-micro-usd`), which is what it
/// bills the team. Calls whose cost the server did not report (a provider's
/// own API in standalone) add their tokens but no cost.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct AiUsage {
    /// Successful LLM calls.
    pub calls: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Provider cost reported by the gateway, in micro-USD.
    pub cost_micro_usd: u64,
}

impl AiUsage {
    /// No call was made.
    pub fn is_empty(&self) -> bool {
        self.calls == 0
    }

    /// Add `other`'s calls to these totals (saturating).
    pub fn add(&mut self, other: &AiUsage) {
        self.calls = self.calls.saturating_add(other.calls);
        self.prompt_tokens = self.prompt_tokens.saturating_add(other.prompt_tokens);
        self.completion_tokens = self
            .completion_tokens
            .saturating_add(other.completion_tokens);
        self.cost_micro_usd = self.cost_micro_usd.saturating_add(other.cost_micro_usd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_sums_every_field_and_saturates() {
        let mut total = AiUsage::default();
        assert!(total.is_empty());
        total.add(&AiUsage {
            calls: 1,
            prompt_tokens: 10,
            completion_tokens: 5,
            cost_micro_usd: 300,
        });
        total.add(&AiUsage {
            calls: 2,
            prompt_tokens: 1,
            completion_tokens: 1,
            cost_micro_usd: u64::MAX,
        });
        assert_eq!(total.calls, 3);
        assert_eq!(total.prompt_tokens, 11);
        assert_eq!(total.completion_tokens, 6);
        assert_eq!(total.cost_micro_usd, u64::MAX);
        assert!(!total.is_empty());
    }

    #[test]
    fn a_missing_field_deserializes_as_zero() {
        let usage: AiUsage = serde_json::from_str(r#"{"calls":1}"#).unwrap();
        assert_eq!(usage.calls, 1);
        assert_eq!(usage.cost_micro_usd, 0);
    }
}
