//! Owner-authorized, point-in-time exports. Downloads never execute tools,
//! checkpoint a live tree, or advance the session's user-input clock.

use agents_core::{SessionRecord, SessionStatus};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use worker::{Response, Result};

use crate::{provider::Message, util};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Json,
    Jsonl,
    Markdown,
}

impl Format {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "json" => Some(Self::Json),
            "jsonl" => Some(Self::Jsonl),
            "markdown" => Some(Self::Markdown),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Jsonl => "jsonl",
            Self::Markdown => "markdown",
        }
    }

    fn content_type(self) -> &'static str {
        match self {
            Self::Json => "application/json; charset=utf-8",
            Self::Jsonl => "application/x-ndjson; charset=utf-8",
            Self::Markdown => "text/markdown; charset=utf-8",
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Jsonl => "jsonl",
            Self::Markdown => "md",
        }
    }
}

pub struct Transcript {
    pub session: SessionRecord,
    pub messages: Vec<Message>,
    pub pending_messages: Vec<Message>,
    pub input_tokens: u64,
    pub compact_count: u64,
    pub exported_at_ms: u64,
}

type Chunks = Box<dyn Iterator<Item = Result<Vec<u8>>>>;

impl Transcript {
    fn metadata(&self) -> Value {
        json!({
            "schema_version":1,
            "exported_at_ms":self.exported_at_ms,
            "session":self.session,
            "context":{"input_tokens":self.input_tokens,"compact_count":self.compact_count},
            "message_count":self.messages.len(),
            "pending_message_count":self.pending_messages.len(),
        })
    }

    pub fn response(self, format: Format) -> Result<Response> {
        let id = filename_id(&self.session.id);
        let count = self.messages.len();
        let pending_count = self.pending_messages.len();
        let chunks = self.chunks(format)?;
        let mut response = Response::from_stream(futures::stream::iter(chunks))?;
        let headers = response.headers_mut();
        headers.set("Content-Type", format.content_type())?;
        headers.set(
            "Content-Disposition",
            &format!(
                "attachment; filename=\"conversation-{id}.{}\"",
                format.extension()
            ),
        )?;
        headers.set("X-Export-Message-Count", &count.to_string())?;
        headers.set("X-Export-Pending-Count", &pending_count.to_string())?;
        headers.set("X-Content-Type-Options", "nosniff")?;
        Ok(response)
    }

    fn chunks(self, format: Format) -> Result<Chunks> {
        let mut metadata = self.metadata();
        match format {
            Format::Json => {
                let mut header = serde_json::to_vec(&metadata)?;
                header.pop();
                header.extend_from_slice(b",\"messages\":[");
                let messages = json_messages(self.messages);
                let pending = json_messages(self.pending_messages);
                Ok(Box::new(
                    std::iter::once(Ok(header))
                        .chain(messages)
                        .chain(std::iter::once(Ok(b"],\"pending_messages\":[".to_vec())))
                        .chain(pending)
                        .chain(std::iter::once(Ok(b"]}\n".to_vec()))),
                ))
            }
            Format::Jsonl => {
                metadata["type"] = json!("session");
                let header = json_line(&metadata)?;
                Ok(Box::new(
                    std::iter::once(Ok(header))
                        .chain(jsonl_messages(self.messages, "message"))
                        .chain(jsonl_messages(self.pending_messages, "pending_message")),
                ))
            }
            Format::Markdown => {
                let mut header = b"# Conversation\n\n".to_vec();
                header.extend_from_slice(&fenced_json(&metadata)?);
                Ok(Box::new(
                    std::iter::once(Ok(header))
                        .chain(markdown_messages(self.messages, "Message"))
                        .chain(markdown_messages(self.pending_messages, "Pending message")),
                ))
            }
        }
    }
}

fn json_messages(messages: Vec<Message>) -> impl Iterator<Item = Result<Vec<u8>>> {
    messages.into_iter().enumerate().map(|(index, message)| {
        let mut bytes = Vec::new();
        if index != 0 {
            bytes.push(b',');
        }
        serde_json::to_writer(&mut bytes, &message)?;
        Ok(bytes)
    })
}

fn jsonl_messages(
    messages: Vec<Message>,
    kind: &'static str,
) -> impl Iterator<Item = Result<Vec<u8>>> {
    messages
        .into_iter()
        .enumerate()
        .map(move |(sequence, message)| {
            json_line(&json!({"type":kind,"sequence":sequence,"message":message}))
        })
}

fn json_line(value: &Value) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn markdown_messages(
    messages: Vec<Message>,
    kind: &'static str,
) -> impl Iterator<Item = Result<Vec<u8>>> {
    messages
        .into_iter()
        .enumerate()
        .map(move |(sequence, message)| {
            let mut bytes = format!("\n## {kind} {sequence}\n\n").into_bytes();
            bytes.extend_from_slice(&fenced_json(&serde_json::to_value(message)?)?);
            Ok(bytes)
        })
}

fn fenced_json(value: &Value) -> Result<Vec<u8>> {
    let text = serde_json::to_string_pretty(value)?;
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest.saturating_add(1).max(3));
    Ok(format!("{fence}json\n{text}\n{fence}\n").into_bytes())
}

pub fn filename_id(id: &str) -> String {
    if !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return id.to_owned();
    }
    format!("{:x}", Sha256::digest(id.as_bytes()))
}

pub fn workspace_ready(
    record: &SessionRecord,
    archive_pending: bool,
    checkpoint_error: bool,
) -> std::result::Result<(), (u16, &'static str, &'static str)> {
    if !record.pinned.workspace {
        return Err((404, "workspace_not_found", "This session has no workspace"));
    }
    if record.status != SessionStatus::Closed || archive_pending || checkpoint_error {
        return Err((
            409,
            "workspace_not_ready",
            "Close the session and finish checkpointing before downloading its workspace",
        ));
    }
    if record.workspace_snapshot.is_none() {
        return Err((
            404,
            "workspace_not_found",
            "This session has no committed workspace files",
        ));
    }
    Ok(())
}

pub fn invalid_format() -> Result<Response> {
    util::json_error(
        400,
        "invalid_export_format",
        "Use format=json, jsonl, or markdown",
    )
}

pub fn requested_format(url: &url::Url) -> Option<Format> {
    let pairs: Vec<_> = url.query_pairs().collect();
    match pairs.as_slice() {
        [] => Some(Format::Json),
        [(key, value)] if key == "format" => Format::parse(value),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agents_core::{CreateAgent, PinnedContext};
    use proptest::prelude::*;

    fn transcript() -> Transcript {
        let agent = serde_json::from_value::<CreateAgent>(json!({"name":"fixture"}))
            .unwrap()
            .into_config("agent".into(), "owner".into(), 100);
        let mut first = Message::text("assistant", "Before compaction ``` and ````");
        first
            .extra
            .insert("reasoning_content".into(), json!("source reasoning"));
        Transcript {
            session: SessionRecord {
                id: "session".into(),
                owner_id: "owner".into(),
                agent_id: "agent".into(),
                status: SessionStatus::Closed,
                created_at_ms: 100,
                last_input_at_ms: 200,
                input_revision: 1,
                pinned: PinnedContext::resolve(&agent, vec![], vec![]),
                configuration: None,
                context_layout: None,
                workspace_snapshot: None,
                runtime_profile: agents_core::RuntimeProfile::Basic1g,
            },
            messages: vec![first, Message::tool_result("tool-1", "actual output")],
            pending_messages: vec![Message::text("user", "steering")],
            input_tokens: 42,
            compact_count: 3,
            exported_at_ms: 300,
        }
    }

    fn output(transcript: Transcript, format: Format) -> String {
        String::from_utf8(
            transcript
                .chunks(format)
                .unwrap()
                .map(Result::unwrap)
                .flatten()
                .collect(),
        )
        .unwrap()
    }

    #[test]
    fn json_contains_source_continuation_and_pending_fields() {
        let value: Value = serde_json::from_str(&output(transcript(), Format::Json)).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["session"]["last_input_at_ms"], 200);
        assert_eq!(value["context"]["compact_count"], 3);
        assert_eq!(
            value["messages"][0]["reasoning_content"],
            "source reasoning"
        );
        assert_eq!(value["messages"][1]["tool_call_id"], "tool-1");
        assert_eq!(value["pending_messages"][0]["content"], "steering");
    }

    #[test]
    fn jsonl_sequences_are_complete_and_distinguish_pending_input() {
        let text = output(transcript(), Format::Jsonl);
        let lines: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0]["type"], "session");
        assert_eq!(lines[1]["sequence"], 0);
        assert_eq!(lines[2]["sequence"], 1);
        assert_eq!(lines[3]["type"], "pending_message");
        assert_eq!(lines[3]["sequence"], 0);
    }

    #[test]
    fn markdown_uses_a_fence_that_user_content_cannot_close() {
        let text = output(transcript(), Format::Markdown);
        assert!(text.contains("`````json\n"));
        assert!(text.contains("reasoning_content"));
        assert!(text.contains("## Pending message 0"));
    }

    #[test]
    fn filenames_cannot_inject_headers_or_paths() {
        assert_eq!(filename_id("session_123-abc"), "session_123-abc");
        for input in ["../path", "\"\r\nX-Header: value", "", "用户"] {
            assert_eq!(filename_id(input).len(), 64);
            assert!(
                filename_id(input)
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
            );
        }
    }

    #[test]
    fn ready_requires_a_closed_workspace_with_a_committed_checkpoint() {
        let mut record = transcript().session;
        assert_eq!(workspace_ready(&record, false, false).unwrap_err().0, 404);
        record.pinned.workspace = true;
        record.status = SessionStatus::Idle;
        assert_eq!(workspace_ready(&record, false, false).unwrap_err().0, 409);
        record.status = SessionStatus::Closed;
        assert_eq!(workspace_ready(&record, true, false).unwrap_err().0, 409);
        assert_eq!(workspace_ready(&record, false, true).unwrap_err().0, 409);
        assert_eq!(workspace_ready(&record, false, false).unwrap_err().0, 404);
    }

    #[test]
    fn format_query_rejects_unknown_or_duplicate_fields() {
        assert_eq!(
            requested_format(&url::Url::parse("https://agents.test/export").unwrap()),
            Some(Format::Json)
        );
        for query in [
            "format=csv",
            "format=json&format=jsonl",
            "format=json&path=private",
            "other=value",
        ] {
            assert!(
                requested_format(
                    &url::Url::parse(&format!("https://agents.test/export?{query}")).unwrap()
                )
                .is_none()
            );
        }
    }

    proptest! {
        #[test]
        fn every_source_message_roundtrips_in_order_in_both_machine_formats(
            content in proptest::collection::vec("(?s).{0,128}", 0..32)
        ) {
            let mut json_transcript = transcript();
            json_transcript.messages = content.iter().map(|text| Message::text("user", text)).collect();
            let mut jsonl_transcript = transcript();
            jsonl_transcript.messages = json_transcript.messages.clone();
            let value: Value = serde_json::from_str(&output(json_transcript, Format::Json)).unwrap();
            let lines = output(jsonl_transcript, Format::Jsonl);
            let reconstructed: Vec<Value> = lines.lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .filter(|entry| entry["type"] == "message")
                .map(|entry| entry["message"].clone())
                .collect();
            prop_assert_eq!(value["messages"].as_array().unwrap(), &reconstructed);
            let exported: Vec<_> = reconstructed.iter().map(|message| message["content"].as_str().unwrap().to_owned()).collect();
            prop_assert_eq!(exported, content);
            prop_assert_eq!(value["session"]["input_revision"].as_u64(), Some(1));
            prop_assert_eq!(value["session"]["last_input_at_ms"].as_u64(), Some(200));
        }
    }
}
