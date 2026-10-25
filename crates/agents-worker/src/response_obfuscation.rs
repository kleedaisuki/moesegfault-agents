//! Private observation entropy; never model context or public response metadata.

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;

#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct Seed([u8; 32]);

impl Seed {
    pub(super) fn generate() -> Self {
        let mut bytes = [0; 32];
        bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        Self(bytes)
    }
}

impl std::fmt::Debug for Seed {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ObfuscationSeed { .. }")
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Obfuscation {
    seed: Seed,
    response_id: String,
}

impl Obfuscation {
    pub(super) fn new(seed: Seed, response_id: &str) -> Self {
        Self {
            seed,
            response_id: response_id.into(),
        }
    }

    pub(super) fn project(&self, event: &mut Value, enabled: bool) {
        let delta = event["type"]
            .as_str()
            .is_some_and(|kind| kind.ends_with(".delta"));
        if !enabled || !delta {
            if let Some(object) = event.as_object_mut() {
                object.remove("obfuscation");
            }
            return;
        }
        // Older journals already contain the exact bytes sent to observers.
        if event.get("obfuscation").is_some_and(Value::is_string) {
            return;
        }
        let Some(sequence) = event["sequence_number"].as_u64() else {
            return;
        };
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.seed.0).expect("fixed HMAC key length");
        mac.update(b"agents.responses.obfuscation.v1\0");
        mac.update(&(self.response_id.len() as u64).to_be_bytes());
        mac.update(self.response_id.as_bytes());
        mac.update(&sequence.to_be_bytes());
        let digest = mac.finalize().into_bytes();
        let length = 16 + usize::from(digest[0] % 17);
        let padding: String = digest
            .iter()
            .flat_map(|byte| {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                [
                    char::from(HEX[usize::from(byte >> 4)]),
                    char::from(HEX[usize::from(byte & 15)]),
                ]
            })
            .take(length)
            .collect();
        event["obfuscation"] = json!(padding);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_is_stable_but_response_seed_and_sequence_are_independent() {
        let context = Obfuscation::new(Seed([7; 32]), "resp_one");
        let event =
            json!({"type":"response.output_text.delta","sequence_number":4,"delta":"hello"});
        let mut first = event.clone();
        let mut replay = event.clone();
        context.project(&mut first, true);
        context.project(&mut replay, true);
        assert_eq!(first, replay);
        assert!(
            first["obfuscation"]
                .as_str()
                .is_some_and(|padding| (16..=32).contains(&padding.len()))
        );
        for other in [
            Obfuscation::new(Seed([8; 32]), "resp_one"),
            Obfuscation::new(Seed([7; 32]), "resp_two"),
        ] {
            let mut output = event.clone();
            other.project(&mut output, true);
            assert_ne!(output["obfuscation"], first["obfuscation"]);
        }
        let mut next = event.clone();
        next["sequence_number"] = json!(5);
        context.project(&mut next, true);
        assert_ne!(next["obfuscation"], first["obfuscation"]);
        assert_eq!(first["sequence_number"], 4);
        assert_eq!(event.get("obfuscation"), None);
    }

    #[test]
    fn legacy_padding_is_retained_and_disabled_observers_are_independent() {
        let context = Obfuscation::new(Seed([7; 32]), "resp_one");
        let mut event = json!({"type":"response.output_text.delta","sequence_number":4,"delta":"hello","obfuscation":"legacy-random-padding"});
        let original = event.clone();
        context.project(&mut event, true);
        assert_eq!(event, original);
        context.project(&mut event, false);
        assert!(event.get("obfuscation").is_none());
        assert_eq!(original["obfuscation"], "legacy-random-padding");
    }

    #[test]
    fn seed_debug_is_redacted_and_entropy_is_not_public_event_data() {
        let seed = Seed::generate();
        let serialized = serde_json::to_string(&seed).unwrap();
        let restored: Seed = serde_json::from_str(&serialized).unwrap();
        let context = Obfuscation::new(restored, "resp_one");
        assert_eq!(format!("{:?}", seed), "ObfuscationSeed { .. }");
        assert!(!format!("{context:?}").contains(&serialized));
        let mut event =
            json!({"type":"response.output_text.delta","sequence_number":0,"delta":"hello"});
        context.project(&mut event, true);
        assert!(!event.to_string().contains(&serialized));
        assert!(event.get("seed").is_none());
    }
}
