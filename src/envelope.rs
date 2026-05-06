//! Agent-native response envelope (Principle 4).
//!
//! Every retrieval tool wraps its results in [`Envelope`] so agents see a
//! single, stable shape: payload + truncation signal + total matches +
//! budget spent.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Envelope<T> {
    pub results: Vec<T>,
    pub truncated: bool,
    /// `None` when the tool cannot cheaply count matches.
    pub total_available: Option<u64>,
    pub budget_spent_tokens: u64,
}

impl<T> Envelope<T> {
    pub fn new(
        results: Vec<T>,
        truncated: bool,
        total_available: Option<u64>,
        budget: u64,
    ) -> Self {
        Self {
            results,
            truncated,
            total_available,
            budget_spent_tokens: budget,
        }
    }
}

/// Approximate token estimator: `ceil(bytes / 4)`.
///
/// Documented as approximate in `docs/starting-shape.md`. Tightens when we
/// put a real tokenizer on the hot path.
pub fn estimate_tokens<T: Serialize>(payload: &T) -> u64 {
    match serde_json::to_vec(payload) {
        Ok(bytes) => (bytes.len() as u64).div_ceil(4),
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn serializes_with_exact_keys() {
        let e: Envelope<serde_json::Value> = Envelope::new(vec![json!("x")], false, Some(1), 42);
        let v = serde_json::to_value(&e).unwrap();
        assert!(v.get("results").is_some());
        assert!(v.get("truncated").is_some());
        assert!(v.get("total_available").is_some());
        assert!(v.get("budget_spent_tokens").is_some());
        assert_eq!(v["budget_spent_tokens"], 42);
    }

    #[test]
    fn total_available_null_round_trips() {
        let e: Envelope<i32> = Envelope::new(vec![], false, None, 0);
        let v = serde_json::to_value(&e).unwrap();
        assert!(v["total_available"].is_null());
    }

    #[test]
    fn estimate_tokens_rounds_up_ceil() {
        // JSON-serialized "abcd" is 6 bytes (quotes included) → ceil(6/4)=2.
        assert_eq!(estimate_tokens(&"abcd"), 2);
        // empty object serializes to "{}" (2 bytes) → ceil(2/4)=1.
        let empty: std::collections::BTreeMap<String, String> = Default::default();
        assert_eq!(estimate_tokens(&empty), 1);
    }

    #[test]
    fn estimate_tokens_is_monotonic() {
        let small = estimate_tokens(&json!({"x": 1}));
        let large = estimate_tokens(&json!({"x": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}));
        assert!(large >= small);
    }
}
