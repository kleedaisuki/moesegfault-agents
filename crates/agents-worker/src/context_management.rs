//! Pure policy for the existing compaction manager; no alternate summarizer.

use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InputSize {
    Exact(u64),
    WorkingEstimate(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Policy {
    threshold: u64,
    explicit: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PolicyError {
    Invalid,
    ExactCountUnavailable,
}

impl Policy {
    /// `default_threshold` comes from the pinned percentage and selected model's
    /// real context window. Public API thresholds are absolute token counts.
    pub(crate) fn resolve(
        value: Option<&Value>,
        default_threshold: u64,
    ) -> Result<Self, PolicyError> {
        if default_threshold == 0 {
            return Err(PolicyError::Invalid);
        }
        let implicit = Self {
            threshold: default_threshold,
            explicit: false,
        };
        let Some(value) = value.filter(|value| !value.is_null()) else {
            return Ok(implicit);
        };
        let entries = value
            .as_array()
            .filter(|entries| entries.len() <= 16)
            .ok_or(PolicyError::Invalid)?;
        if entries.is_empty() {
            return Ok(implicit);
        }
        let mut threshold = u64::MAX;
        for entry in entries {
            let entry = entry.as_object().ok_or(PolicyError::Invalid)?;
            if entry
                .keys()
                .any(|key| !matches!(key.as_str(), "type" | "compact_threshold"))
                || entry.get("type").and_then(Value::as_str) != Some("compaction")
            {
                return Err(PolicyError::Invalid);
            }
            let candidate = match entry.get("compact_threshold") {
                Some(value) if !value.is_null() => value
                    .as_u64()
                    .filter(|value| *value > 0)
                    .ok_or(PolicyError::Invalid)?,
                _ => default_threshold,
            };
            // Entries are independent triggers; one compaction satisfies all
            // currently crossed entries without repeated paid summarization.
            threshold = threshold.min(candidate);
        }
        Ok(Self {
            threshold,
            explicit: true,
        })
    }

    pub(crate) fn should_compact(self, size: InputSize) -> Result<bool, PolicyError> {
        let tokens = match size {
            InputSize::Exact(tokens) => tokens,
            InputSize::WorkingEstimate(_) if self.explicit => {
                return Err(PolicyError::ExactCountUnavailable);
            }
            InputSize::WorkingEstimate(tokens) => tokens,
        };
        Ok(tokens >= self.threshold)
    }

    pub(crate) fn emits_item(self) -> bool {
        self.explicit
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    #[test]
    fn pinned_product_policy_stays_implicit_without_a_protocol_request() {
        for value in [None, Some(Value::Null), Some(json!([]))] {
            let policy = Policy::resolve(value.as_ref(), 80).unwrap();
            assert!(!policy.emits_item());
            assert!(
                !policy
                    .should_compact(InputSize::WorkingEstimate(79))
                    .unwrap()
            );
            assert!(
                policy
                    .should_compact(InputSize::WorkingEstimate(80))
                    .unwrap()
            );
        }
    }

    #[test]
    fn an_explicit_protocol_threshold_never_uses_a_character_estimate() {
        let policy = Policy::resolve(
            Some(&json!([{"type":"compaction","compact_threshold":42}])),
            80,
        )
        .unwrap();
        assert!(policy.emits_item());
        assert_eq!(
            policy.should_compact(InputSize::WorkingEstimate(100)),
            Err(PolicyError::ExactCountUnavailable)
        );
        assert!(!policy.should_compact(InputSize::Exact(41)).unwrap());
        assert!(policy.should_compact(InputSize::Exact(42)).unwrap());
    }

    proptest! {
        #[test]
        fn independent_threshold_entries_are_order_invariant(a in 1u64..u64::MAX, b in 1u64..u64::MAX) {
            let forward = json!([{"type":"compaction","compact_threshold":a},{"type":"compaction","compact_threshold":b}]);
            let backward = json!([{"type":"compaction","compact_threshold":b},{"type":"compaction","compact_threshold":a}]);
            prop_assert_eq!(Policy::resolve(Some(&forward), 80), Policy::resolve(Some(&backward), 80));
        }
    }
}
