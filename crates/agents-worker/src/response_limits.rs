//! Bounds apply to canonical append output, not transport hydration or private state.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::ops::Deref;

pub(crate) const OUTPUT_MAX_BYTES: usize = 4 * 1024 * 1024;
const INTEGER_DIGITS: usize = 20;
const HASH_PREFIXES: &[&str] = &["hosted", "discovery", "mcp", "mcpr", "mcpl"];

#[derive(Clone, Copy, Debug)]
pub(crate) struct OutputLimits {
    pub max_bytes: usize,
    pub max_items: usize,
    pub min_id_bytes: usize,
    pub max_id_bytes: usize,
}

impl OutputLimits {
    pub(crate) fn for_response(response: &str) -> Result<Self, &'static str> {
        if !response_name(response) {
            return Err("invalid_canonical_response_id");
        }
        let min_id_bytes = (response.len() + "fc__0".len()).min(
            HASH_PREFIXES
                .iter()
                .map(|prefix| prefix.len() + 1 + 64)
                .min()
                .unwrap(),
        );
        let max_id_bytes = (response.len() + "item___".len() + 2 * INTEGER_DIGITS).max(
            HASH_PREFIXES
                .iter()
                .map(|prefix| prefix.len() + 1 + 64)
                .max()
                .unwrap(),
        );
        // Every canonical item has an ASCII owned ID and a string type. Even an
        // empty type is a conservative lower bound for every typed output family.
        let minimum = serde_json::to_vec(&json!({"id":"x".repeat(min_id_bytes),"type":""}))
            .map_err(|_| "invalid_canonical_output")?
            .len();
        Ok(Self {
            max_bytes: OUTPUT_MAX_BYTES,
            // n item encodings need n-1 commas and the two array delimiters.
            max_items: (OUTPUT_MAX_BYTES - 1) / (minimum + 1),
            min_id_bytes,
            max_id_bytes,
        })
    }

    pub(crate) fn item_bytes(&self, response: &str, item: &Value) -> Result<usize, &'static str> {
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .ok_or("invalid_canonical_output_id")?;
        if id.len() < self.min_id_bytes || id.len() > self.max_id_bytes || !owned_id(response, id) {
            return Err("invalid_canonical_output_id");
        }
        if item.get("type").and_then(Value::as_str).is_none() {
            return Err("invalid_canonical_output");
        }
        serde_json::to_vec(item)
            .map(|bytes| bytes.len())
            .map_err(|_| "invalid_canonical_output")
    }

    pub(crate) fn index(&self, index: usize) -> Result<(), &'static str> {
        if index >= self.max_items {
            return Err("response_output_limit");
        }
        Ok(())
    }
}

fn decimal(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= INTEGER_DIGITS
        && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn response_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

/// Recognizes the reserved server namespace, not ownership or lookup authority.
/// Raw Conversation writes may not prefill a future generated output identity.
pub(crate) fn service_owned_id(id: &str) -> bool {
    if hash_id(id) {
        return true;
    }
    let response = if let Some(body) = id.strip_prefix("fc_").or_else(|| id.strip_prefix("msg_")) {
        body.rsplit_once('_')
            .filter(|(_, slot)| decimal(slot))
            .map(|(response, _)| response)
    } else if let Some(body) = id.strip_prefix("item_") {
        body.rsplit_once('_')
            .filter(|(_, ordinal)| decimal(ordinal))
            .and_then(|(body, _)| body.rsplit_once('_'))
            .filter(|(_, round)| decimal(round))
            .map(|(response, _)| response)
    } else {
        None
    };
    response.is_some_and(|response| response.starts_with("resp_") && response_name(response))
}

fn owned_id(response: &str, id: &str) -> bool {
    for prefix in ["fc", "msg"] {
        if let Some(suffix) = id.strip_prefix(&format!("{prefix}_{response}_")) {
            return decimal(suffix);
        }
    }
    if let Some(suffix) = id.strip_prefix(&format!("item_{response}_")) {
        return suffix
            .split_once('_')
            .is_some_and(|(round, ordinal)| decimal(round) && decimal(ordinal));
    }
    hash_id(id)
}

fn hash_id(id: &str) -> bool {
    HASH_PREFIXES.iter().any(|prefix| {
        id.strip_prefix(prefix)
            .and_then(|suffix| suffix.strip_prefix('_'))
            .is_some_and(|hash| {
                hash.len() == 64
                    && hash
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
    })
}

/// An ephemeral cache. It is reconstructed from canonical bytes after cold load;
/// changing the vector length (including DELETE) invalidates it automatically.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OutputState {
    pub count: usize,
    pub bytes: usize,
}

impl OutputState {
    pub(crate) fn read(
        response: &str,
        output: &[Value],
        limits: &OutputLimits,
    ) -> Result<Self, &'static str> {
        let mut state = Self { count: 0, bytes: 2 };
        for item in output {
            state = state.append(limits.item_bytes(response, item)?, limits)?;
        }
        Ok(state)
    }

    pub(crate) fn append(self, bytes: usize, limits: &OutputLimits) -> Result<Self, &'static str> {
        limits.index(self.count)?;
        let bytes = self
            .bytes
            .checked_add(bytes)
            .and_then(|bytes| bytes.checked_add(usize::from(self.count != 0)))
            .filter(|bytes| *bytes <= limits.max_bytes)
            .ok_or("response_output_limit")?;
        Ok(Self {
            count: self.count + 1,
            bytes,
        })
    }

    pub(crate) fn replace(
        self,
        previous: usize,
        next: usize,
        limits: &OutputLimits,
    ) -> Result<Self, &'static str> {
        let bytes = self
            .bytes
            .checked_sub(previous)
            .and_then(|bytes| bytes.checked_add(next))
            .filter(|bytes| *bytes <= limits.max_bytes)
            .ok_or("response_output_limit")?;
        Ok(Self { bytes, ..self })
    }
}

/// Reads and serialized data keep their established array contract. There is no
/// DerefMut: every production append or replacement needs a checked change.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct CanonicalOutput {
    items: Vec<Value>,
    #[serde(skip)]
    cached: Option<CachedOutput>,
}

#[derive(Clone)]
struct CachedOutput {
    response: String,
    state: OutputState,
}

impl Deref for CanonicalOutput {
    type Target = [Value];
    fn deref(&self) -> &Self::Target {
        &self.items
    }
}

impl CanonicalOutput {
    fn state(
        &mut self,
        response: &str,
        limits: &OutputLimits,
    ) -> Result<OutputState, &'static str> {
        if let Some(cached) = &self.cached {
            if cached.response == response && cached.state.count == self.items.len() {
                return Ok(cached.state);
            }
        }
        let state = OutputState::read(response, &self.items, limits)?;
        self.cached = Some(CachedOutput {
            response: response.into(),
            state,
        });
        Ok(state)
    }

    pub(crate) fn plan(&mut self, response: &str) -> Result<OutputPlan, &'static str> {
        let limits = OutputLimits::for_response(response)?;
        let next = self.state(response, &limits)?;
        Ok(OutputPlan {
            response: response.into(),
            limits,
            next,
            changes: vec![],
        })
    }

    pub(crate) fn prepare(
        &mut self,
        response: &str,
        index: Option<usize>,
        item: Value,
    ) -> Result<OutputChange, &'static str> {
        let limits = OutputLimits::for_response(response)?;
        let before = self.state(response, &limits)?;
        let bytes = limits.item_bytes(response, &item)?;
        let after = match index {
            Some(index) => {
                let previous = self
                    .items
                    .get(index)
                    .ok_or("canonical_output_index_invalid")?;
                before.replace(limits.item_bytes(response, previous)?, bytes, &limits)?
            }
            None => before.append(bytes, &limits)?,
        };
        Ok(OutputChange {
            response: response.into(),
            before,
            after,
            index: index.unwrap_or(before.count),
            item,
        })
    }

    pub(crate) fn apply(&mut self, change: OutputChange) -> Result<(), &'static str> {
        if !self.cached.as_ref().is_some_and(|cached| {
            cached.response == change.response && cached.state == change.before
        }) || self.items.len() != change.before.count
        {
            return Err("canonical_output_changed");
        }
        if change.index == self.items.len() {
            self.items.push(change.item);
        } else {
            self.items[change.index] = change.item;
        }
        self.cached.as_mut().unwrap().state = change.after;
        Ok(())
    }

    pub(crate) fn clear(&mut self) {
        self.items.clear();
        self.cached = None;
    }
}

pub(crate) struct OutputChange {
    response: String,
    before: OutputState,
    after: OutputState,
    index: usize,
    item: Value,
}

impl OutputChange {
    pub(crate) fn item(&self) -> &Value {
        &self.item
    }
    pub(crate) fn index(&self) -> usize {
        self.index
    }
}

pub(crate) struct OutputPlan {
    response: String,
    limits: OutputLimits,
    next: OutputState,
    changes: Vec<OutputChange>,
}

impl OutputPlan {
    pub(crate) fn next_index(&self) -> usize {
        self.next.count
    }

    pub(crate) fn push(&mut self, item: Value) -> Result<(), &'static str> {
        let bytes = self.limits.item_bytes(&self.response, &item)?;
        let after = self.next.append(bytes, &self.limits)?;
        self.changes.push(OutputChange {
            response: self.response.clone(),
            before: self.next,
            after,
            index: self.next.count,
            item,
        });
        self.next = after;
        Ok(())
    }

    pub(crate) fn finish(self) -> Vec<OutputChange> {
        self.changes
    }
}

#[cfg(test)]
impl From<Vec<Value>> for CanonicalOutput {
    fn from(items: Vec<Value>) -> Self {
        Self {
            items,
            cached: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn all_production_id_families_obey_encoded_bounds() {
        let response = "resp_01234567890123456789012345678901";
        let limits = OutputLimits::for_response(response).unwrap();
        let mut ids = vec![
            format!("fc_{response}_0"),
            format!("msg_{response}_{}", u64::MAX),
            format!("item_{response}_{}_{}", u64::MAX, u64::MAX),
        ];
        ids.extend(
            HASH_PREFIXES
                .iter()
                .map(|prefix| format!("{prefix}_{}", "f".repeat(64))),
        );
        for id in ids {
            let item = json!({"id":id,"type":"message"});
            assert!(limits.item_bytes(response, &item).is_ok());
        }
        assert!(
            limits
                .item_bytes(response, &json!({"id":"provider-owned","type":"message"}))
                .is_err()
        );
    }

    #[test]
    fn reserved_namespace_matches_exact_slot_and_hash_grammars() {
        for id in [
            "fc_resp_abc_0",
            "msg_resp_abc_def_18446744073709551615",
            "item_resp_abc_0_1",
            "item_resp_abc__0_1",
            "fc_resp_abc_0_1",
        ] {
            assert!(service_owned_id(id), "{id}");
        }
        for prefix in HASH_PREFIXES {
            assert!(service_owned_id(&format!("{prefix}_{}", "0".repeat(64))));
            assert!(!service_owned_id(&format!("{prefix}_{}", "0".repeat(63))));
            assert!(!service_owned_id(&format!("{prefix}_{}", "0".repeat(65))));
            assert!(!service_owned_id(&format!("{prefix}_{}", "A".repeat(64))));
        }
        for id in [
            "item_initial",
            "fc_external_0",
            "msg_external_0",
            "item_external_0_0",
            "fc_resp_abc_x",
            "msg_resp_abc_-1",
            "item_resp_abc_0",
            "item_resp_abc_x_0",
            "item_resp_abc_0_x",
            "fc_resp_a/b_0",
            "msg_resp_雪_0",
            "fc_resp_abc_123456789012345678901",
            "hosted_opaque",
            "mcpx_opaque",
        ] {
            assert!(!service_owned_id(id), "{id}");
        }
        let longest = format!("resp_{}", "x".repeat(123));
        assert!(service_owned_id(&format!("fc_{longest}_0")));
        assert!(!service_owned_id(&format!("fc_{longest}x_0")));
        assert!(OutputLimits::for_response("external").is_ok());
    }

    proptest! {
        #[test]
        fn every_response_scoped_generated_id_is_reserved_even_with_embedded_underscores(
            suffix in "[a-zA-Z0-9_-]{0,123}", round in any::<u64>(), ordinal in any::<u64>(),
        ) {
            let response = format!("resp_{suffix}");
            let limits = OutputLimits::for_response(&response).unwrap();
            for id in [format!("fc_{response}_{ordinal}"), format!("msg_{response}_{round}"),
                format!("item_{response}_{round}_{ordinal}")]
            {
                prop_assert!(service_owned_id(&id));
                prop_assert!(limits.item_bytes(&response, &json!({"id":id,"type":"message"})).is_ok());
            }
        }

        #[test]
        fn generated_identifiers_make_the_item_lower_bound_executable(
            response in "[a-zA-Z0-9_-]{1,128}", round in any::<u64>(), ordinal in any::<u64>(),
        ) {
            let limits = OutputLimits::for_response(&response).unwrap();
            let minimum = serde_json::to_vec(&json!({"id":"x".repeat(limits.min_id_bytes),"type":""})).unwrap().len();
            for id in [format!("fc_{response}_{ordinal}"), format!("msg_{response}_{round}"),
                format!("item_{response}_{round}_{ordinal}")]
            {
                let item = json!({"id":id,"type":"message"});
                let bytes = limits.item_bytes(&response, &item).unwrap();
                prop_assert!(bytes >= minimum);
            }
        }

        #[test]
        fn incremental_append_replace_matches_exact_array_encoding(
            texts in proptest::collection::vec("[^\x00]{0,80}", 0..40),
            replacement in "[^\x00]{0,80}",
        ) {
            let response = "resp_property";
            let limits = OutputLimits::for_response(response).unwrap();
            let mut output = vec![];
            let mut state = OutputState { count: 0, bytes: 2 };
            for (index, text) in texts.iter().enumerate() {
                let item = json!({"id":format!("msg_{response}_{index}"),"type":"message","text":text});
                state = state.append(limits.item_bytes(response, &item).unwrap(), &limits).unwrap();
                output.push(item);
                prop_assert_eq!(state.bytes, serde_json::to_vec(&output).unwrap().len());
            }
            if !output.is_empty() {
                let previous = limits.item_bytes(response, &output[0]).unwrap();
                output[0]["text"] = json!(replacement);
                state = state.replace(previous, limits.item_bytes(response, &output[0]).unwrap(), &limits).unwrap();
                prop_assert_eq!(state.count, output.len());
                prop_assert_eq!(state.bytes, serde_json::to_vec(&output).unwrap().len());
            }
        }
    }

    #[test]
    fn byte_limit_and_item_limit_are_one_mathematical_contract() {
        let limits = OutputLimits::for_response("resp_proof").unwrap();
        let minimum = serde_json::to_vec(&json!({"id":"x".repeat(limits.min_id_bytes),"type":""}))
            .unwrap()
            .len();
        assert!((minimum + 1) * limits.max_items + 1 <= limits.max_bytes);
        assert!((minimum + 1) * (limits.max_items + 1) + 1 > limits.max_bytes);
        assert_eq!(limits.index(limits.max_items), Err("response_output_limit"));
        let state = OutputState {
            count: 1,
            bytes: limits.max_bytes,
        };
        assert!(state.append(1, &limits).is_err());
        assert!(state.replace(1, 2, &limits).is_err());
    }

    #[test]
    fn read_only_output_keeps_array_wire_shape_and_reconstructs_transient_cache() {
        let response = "resp_cache";
        let item = json!({"id":format!("hosted_{}", "a".repeat(64)),"type":"code_interpreter_call","status":"in_progress"});
        let mut output = CanonicalOutput::default();
        let change = output.prepare(response, None, item.clone()).unwrap();
        output.apply(change).unwrap();
        let wire = serde_json::to_value(&output).unwrap();
        assert_eq!(wire, json!([item]));
        assert_eq!(
            output.cached.as_ref().unwrap().state.bytes,
            serde_json::to_vec(&output).unwrap().len()
        );
        let mut cold: CanonicalOutput = serde_json::from_value(wire).unwrap();
        assert!(cold.cached.is_none());
        let next = json!({"id":format!("msg_{response}_0"),"type":"message","content":[]});
        let stale = cold.prepare(response, None, next).unwrap();
        assert!(cold.cached.is_some());
        cold.clear();
        assert!(cold.cached.is_none());
        assert_eq!(cold.apply(stale), Err("canonical_output_changed"));
        assert!(cold.is_empty());
    }
}
