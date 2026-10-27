use std::io::{self, Write};

use serde::{
    Serialize, Serializer,
    ser::{SerializeMap, SerializeSeq},
};
use serde_json::Value;

use crate::provider::Message;

// Working-context planning only: image tokenization depends on the selected model,
// resolution and detail. Financial usage always comes from the provider receipt.
const IMAGE_WORKING_TOKENS: u64 = 8192;

#[derive(Default)]
struct Counter(u64);

impl Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len() as u64);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct BudgetMessage<'a>(&'a Message);
struct BudgetContent<'a>(&'a Value);

fn image(part: &Value) -> bool {
    matches!(
        part.get("type").and_then(Value::as_str),
        Some("image_url" | "input_image")
    )
}

impl Serialize for BudgetContent<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let Some(parts) = self.0.as_array() else {
            return self.0.serialize(serializer);
        };
        let mut seq = serializer.serialize_seq(Some(parts.len()))?;
        for part in parts {
            if image(part) {
                seq.serialize_element("[native image]")?;
            } else {
                seq.serialize_element(part)?;
            }
        }
        seq.end()
    }
}

impl Serialize for BudgetMessage<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let message = self.0;
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("role", &message.role)?;
        map.serialize_entry("content", &BudgetContent(&message.content))?;
        if !message.tool_calls.is_empty() {
            map.serialize_entry("tool_calls", &message.tool_calls)?;
        }
        if let Some(id) = &message.tool_call_id {
            map.serialize_entry("tool_call_id", id)?;
        }
        for (key, value) in &message.extra {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

/// A bounded working estimate, not an upstream tokenizer or a billable quantity.
/// Native image bytes are transport data, not millions of textual prompt tokens.
pub(super) fn working_tokens(messages: &[Message]) -> Result<u64, serde_json::Error> {
    let mut count = Counter::default();
    let mut images = 0u64;
    for message in messages {
        images += message
            .content
            .as_array()
            .map_or(0, |parts| parts.iter().filter(|part| image(part)).count())
            as u64;
    }
    serde_json::to_writer(
        &mut count,
        &messages.iter().map(BudgetMessage).collect::<Vec<_>>(),
    )?;
    Ok(count
        .0
        .saturating_add(images.saturating_mul(IMAGE_WORKING_TOKENS)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_and_private_continuations_keep_the_existing_conservative_budget() {
        let mut assistant = Message::text("assistant", "A response with quotes: \"hello\"");
        assistant
            .extra
            .insert("reasoning_content".into(), json!("private continuation"));
        let messages = vec![
            Message::text("system", "fixed"),
            Message::text("user", "你好"),
            assistant,
        ];
        assert_eq!(
            working_tokens(&messages).unwrap(),
            serde_json::to_vec(&messages).unwrap().len() as u64
        );
    }

    #[test]
    fn image_transport_size_does_not_become_a_text_token_count() {
        let mut message = Message::text("user", "");
        let make = |bytes: usize| json!([{"type":"text","text":"Describe this image"},{"type":"image_url","image_url":{"url":format!("data:image/png;base64,{}","A".repeat(bytes))}}]);
        message.content = make(4);
        let small = working_tokens(&[message.clone()]).unwrap();
        message.content = make(1024 * 1024);
        assert_eq!(working_tokens(&[message.clone()]).unwrap(), small);
        let twice = working_tokens(&[message.clone(), message]).unwrap();
        assert!(twice >= IMAGE_WORKING_TOKENS * 2);
        assert!(twice < 20_000);
    }
}
