//! Opt-in source-journal pages. The legacy full status representation is unchanged.

use std::io::{self, Write};

use serde::Serialize;
use sha2::{Digest, Sha256};
use url::Url;

use crate::provider::Message;

pub(crate) const DEFAULT_LIMIT: usize = 50;
pub(crate) const MAX_LIMIT: usize = 100;
const PAGE_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Direction {
    Before(usize),
    After(usize),
    Tail,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PageQuery {
    direction: Direction,
    limit: usize,
}

impl PageQuery {
    pub(crate) fn parse(url: &Url, session_id: &str) -> Result<Self, &'static str> {
        let mut query = Self {
            direction: Direction::Tail,
            limit: DEFAULT_LIMIT,
        };
        let mut limit_seen = false;
        for (name, value) in url.query_pairs() {
            match name.as_ref() {
                "before" | "after" if query.direction == Direction::Tail => {
                    let offset = parse_cursor(&value, session_id)?;
                    query.direction = if name == "before" {
                        Direction::Before(offset)
                    } else {
                        Direction::After(offset)
                    };
                }
                "limit" if !limit_seen => {
                    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                        return Err("limit must be an integer from 1 through 100");
                    }
                    query.limit = value.parse().map_err(|_| "invalid limit")?;
                    if !(1..=MAX_LIMIT).contains(&query.limit) {
                        return Err("limit must be an integer from 1 through 100");
                    }
                    limit_seen = true;
                }
                _ => return Err("use one before or after cursor and one optional limit"),
            }
        }
        Ok(query)
    }
}

#[derive(Serialize)]
pub(crate) struct Entry<'a> {
    pub sequence: usize,
    pub message: &'a Message,
}

#[derive(Serialize)]
pub(crate) struct Page<'a> {
    object: &'static str,
    pub data: Vec<Entry<'a>>,
    pub start: usize,
    pub end: usize,
    pub high_water: usize,
    pub before_cursor: String,
    pub after_cursor: String,
    pub has_older: bool,
    pub has_newer: bool,
}

pub(crate) fn page<'a>(
    session_id: &str,
    journal: &'a [Message],
    query: PageQuery,
) -> Result<Page<'a>, &'static str> {
    let count = journal.len();
    let (start, end) = match query.direction {
        Direction::After(start) if start <= count => {
            (start, forward_end(journal, start, query.limit)?)
        }
        Direction::Before(end) if end <= count => (backward_start(journal, end, query.limit)?, end),
        Direction::Tail => (backward_start(journal, count, query.limit)?, count),
        _ => return Err("cursor is beyond the source journal"),
    };
    Ok(Page {
        object: "list",
        data: journal[start..end]
            .iter()
            .enumerate()
            .map(|(index, message)| Entry {
                sequence: start + index,
                message,
            })
            .collect(),
        start,
        end,
        high_water: count,
        before_cursor: cursor(session_id, start),
        after_cursor: cursor(session_id, end),
        has_older: start > 0,
        has_newer: end < count,
    })
}

fn forward_end(journal: &[Message], start: usize, limit: usize) -> Result<usize, &'static str> {
    let mut bytes = 0;
    let mut end = start;
    for (index, message) in journal.iter().enumerate().skip(start).take(limit) {
        let size = entry_bytes(index, message)?;
        if end > start && bytes + size > PAGE_BYTES {
            break;
        }
        bytes += size;
        end += 1;
    }
    Ok(end)
}

fn backward_start(journal: &[Message], end: usize, limit: usize) -> Result<usize, &'static str> {
    let mut bytes = 0;
    let mut start = end;
    for (index, message) in journal[..end].iter().enumerate().rev().take(limit) {
        let size = entry_bytes(index, message)?;
        if start < end && bytes + size > PAGE_BYTES {
            break;
        }
        bytes += size;
        start -= 1;
    }
    Ok(start)
}

#[derive(Default)]
struct ByteCount(usize);

impl Write for ByteCount {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .ok_or_else(|| io::Error::other("serialized entry is too large"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn entry_bytes(sequence: usize, message: &Message) -> Result<usize, &'static str> {
    let mut bytes = ByteCount::default();
    serde_json::to_writer(&mut bytes, &Entry { sequence, message })
        .map_err(|_| "source entry cannot be serialized")?;
    Ok(bytes.0 + 1)
}

fn namespace(session_id: &str) -> String {
    format!("{:x}", Sha256::digest(session_id.as_bytes()))[..16].into()
}

fn cursor(session_id: &str, offset: usize) -> String {
    format!("j1_{}_{offset:x}", namespace(session_id))
}

fn parse_cursor(value: &str, session_id: &str) -> Result<usize, &'static str> {
    let prefix = format!("j1_{}_", namespace(session_id));
    let offset = value
        .strip_prefix(&prefix)
        .filter(|offset| !offset.is_empty() && offset.len() <= 16)
        .ok_or("invalid source cursor for this session")?;
    if !offset.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("invalid source cursor for this session");
    }
    let offset = u64::from_str_radix(offset, 16).map_err(|_| "invalid source cursor")?;
    usize::try_from(offset).map_err(|_| "source cursor is out of range")
}

/// Hash only the small public view, never the journal or its immutable pin.
pub(crate) fn etag<T: Serialize>(view: &T) -> Result<String, serde_json::Error> {
    Ok(format!(
        "\"{:x}\"",
        Sha256::digest(serde_json::to_vec(view)?)
    ))
}

/// GET validators use weak comparison, including lists and the wildcard.
pub(crate) fn not_modified(header: Option<&str>, current: &str) -> bool {
    header.is_some_and(|header| {
        header.split(',').any(|candidate| {
            let candidate = candidate.trim();
            candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == current
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::{Value, json};

    fn query(parameters: &str, session: &str) -> Result<PageQuery, &'static str> {
        PageQuery::parse(
            &Url::parse(&format!("https://example.invalid/messages?{parameters}")).unwrap(),
            session,
        )
    }

    fn messages(count: usize) -> Vec<Message> {
        (0..count)
            .map(|index| Message::text("user", format!("entry {index}")))
            .collect()
    }

    #[test]
    fn default_is_recent_ascending_and_unknown_query_is_rejected() {
        let source = messages(128);
        let result = page("s", &source, query("", "s").unwrap()).unwrap();
        assert_eq!(
            (result.start, result.end, result.high_water),
            (78, 128, 128)
        );
        assert!(result.has_older);
        assert!(!result.has_newer);
        assert_eq!(result.data[0].sequence, 78);
        for invalid in [
            "limit=0",
            "limit=101",
            "limit=-1",
            "limit=1&limit=2",
            "after=0",
            "before=garbage",
            "unknown=true",
        ] {
            assert!(query(invalid, "s").is_err(), "{invalid}");
        }
    }

    #[test]
    fn cursor_boundaries_cannot_cross_sessions_or_exceed_current_journal() {
        let source = messages(1);
        let token = cursor("s", 1);
        assert!(query(&format!("after={token}"), "other").is_err());
        assert!(query(&format!("after={token}&before={token}"), "s").is_err());
        let future = query(&format!("after={}", cursor("s", 2)), "s").unwrap();
        assert!(page("s", &source, future).is_err());
        let empty = page("s", &source, query(&format!("after={token}"), "s").unwrap()).unwrap();
        assert!(empty.data.is_empty());
        assert_eq!((empty.start, empty.end), (1, 1));
    }

    #[test]
    fn append_does_not_invalidate_a_previous_cursor_and_large_entries_make_progress() {
        let mut source = messages(2);
        let first = page("s", &source, query("", "s").unwrap()).unwrap();
        let token = first.after_cursor.clone();
        source.push(Message::text("tool", "x".repeat(PAGE_BYTES * 2)));
        source.push(Message::text("assistant", "done"));
        let next = page("s", &source, query(&format!("after={token}"), "s").unwrap()).unwrap();
        assert_eq!((next.start, next.end), (2, 3));
        assert!(next.has_newer);
        let final_page = page(
            "s",
            &source,
            query(&format!("after={}", next.after_cursor), "s").unwrap(),
        )
        .unwrap();
        assert_eq!((final_page.start, final_page.end), (3, 4));
        let tail = page("s", &source, query("", "s").unwrap()).unwrap();
        assert_eq!((tail.start, tail.end), (3, 4));
        let oversized = page(
            "s",
            &source,
            query(&format!("before={}", tail.before_cursor), "s").unwrap(),
        )
        .unwrap();
        assert_eq!((oversized.start, oversized.end), (2, 3));
    }

    #[test]
    fn page_keeps_original_tool_and_provider_fields() {
        let source: Vec<Message> = serde_json::from_value(json!([
            {"role":"assistant","content":"","reasoning_content":"private continuation","tool_calls":[
                {"id":"call-1","type":"function","function":{"name":"bash","arguments":"{}"}}
            ]},
            {"role":"tool","content":"result","tool_call_id":"call-1"}
        ]))
        .unwrap();
        let result = page("s", &source, query("", "s").unwrap()).unwrap();
        let result = serde_json::to_value(&result).unwrap();
        let reconstructed: Vec<Value> = result["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["message"].clone())
            .collect();
        assert_eq!(json!(reconstructed), serde_json::to_value(source).unwrap());
    }

    #[test]
    fn metadata_changes_invalidate_etag_and_http_weak_lists_match() {
        let current = etag(&json!({"status":"running","message_count":2})).unwrap();
        assert_ne!(
            current,
            etag(&json!({"status":"idle","message_count":2})).unwrap()
        );
        assert!(not_modified(Some(&current), &current));
        assert!(not_modified(
            Some(&format!("\"other\", W/{current}")),
            &current
        ));
        assert!(not_modified(Some("*"), &current));
        assert!(!not_modified(Some("\"other\""), &current));
        assert!(!not_modified(None, &current));
    }

    #[test]
    fn v3_dataflow_workload_preserves_history_with_bounded_repeat_transfer() {
        use std::path::Path;

        let mut reports = Vec::new();
        for output_bytes in [4096, 32768] {
            let mut source = vec![Message::text("user", "Run the focused repository checks.")];
            for round in 0..128 {
                let call_id = format!("call_{round:04}");
                let mut assistant = Message::text("assistant", "Run the next focused check.");
                assistant.extra.insert(
                    "reasoning_content".into(),
                    json!("Preserve the original continuation field."),
                );
                assistant.tool_calls.push(crate::provider::ToolCall {
                    id: call_id.clone(),
                    kind: "function".into(),
                    function: crate::provider::FunctionCall {
                        name: "bash".into(),
                        arguments:
                            json!({"command":format!("cargo test --package module_{round}")})
                                .to_string(),
                    },
                });
                source.push(assistant);
                let mut output = String::new();
                let mut line = 0_u64;
                while output.len() < output_bytes {
                    let observed = ((round as u64 + 1) * 0x9e3779b9).wrapping_mul(line + 17);
                    output.push_str(&format!(
                        "test module_{round:03}::case_{line:04} ... ok; observed={observed:016x}\n"
                    ));
                    line += 1;
                }
                output.truncate(output_bytes);
                source.push(Message::tool_result(&call_id, json!({
                    "stdout":output,"stderr":"","exit_code":0,"timed_out":false,"truncated":false
                }).to_string()));
            }
            source.push(Message::text("assistant", "Checks completed."));
            let legacy = serde_json::to_vec(&json!({"messages":&source})).unwrap();
            let recent = page("benchmark", &source, query("", "benchmark").unwrap()).unwrap();
            let recent_bytes = serde_json::to_vec(&recent).unwrap().len();
            let prior_end = recent.after_cursor.clone();
            let mut summary = json!({
                "id":"benchmark","agent_id":"agent-benchmark","status":"idle",
                "input_revision":1,"last_input_at_ms":0,"context_tokens":35000,
                "context_window":131072,"compact_count":1,"last_error":null,
                "pending_messages":[],"message_count":source.len(),"run_id":null,
                "checkpoint_pending":false,"checkpoint_error":null,"billing_pending":false,
                "projection_pending":false,"latest_response_id":"resp-benchmark","pending_local_calls":[]
            });
            let initial_summary = serde_json::to_vec(&summary).unwrap().len();
            let initial_etag = etag(&summary).unwrap();
            assert!(not_modified(Some(&initial_etag), &initial_etag));
            source.push(Message::text("user", "Summarize the result."));
            source.push(Message::text("assistant", "All focused checks passed."));
            let delta = page(
                "benchmark",
                &source,
                query(&format!("after={prior_end}"), "benchmark").unwrap(),
            )
            .unwrap();
            assert_eq!(delta.data.len(), 2);
            summary["message_count"] = json!(source.len());
            summary["input_revision"] = json!(2);
            assert_ne!(etag(&summary).unwrap(), initial_etag);
            let updated_summary = serde_json::to_vec(&summary).unwrap().len();
            let delta_bytes = serde_json::to_vec(&delta).unwrap().len();
            let final_legacy = serde_json::to_vec(&json!({"messages":&source}))
                .unwrap()
                .len();

            let mut reconstructed = Vec::new();
            let mut position = 0;
            loop {
                let request = query(
                    &format!("after={}", cursor("benchmark", position)),
                    "benchmark",
                )
                .unwrap();
                let result = page("benchmark", &source, request).unwrap();
                reconstructed.extend(
                    result
                        .data
                        .iter()
                        .map(|entry| serde_json::to_value(entry.message).unwrap()),
                );
                position = result.end;
                if !result.has_newer {
                    break;
                }
            }
            assert_eq!(json!(reconstructed), serde_json::to_value(&source).unwrap());
            let baseline_bytes = legacy.len() * 31 + final_legacy;
            let optimized_bytes = initial_summary + recent_bytes + updated_summary + delta_bytes;
            assert!(optimized_bytes < baseline_bytes / 100);
            reports.push(json!({
                "tool_stdout_bytes":output_bytes,"initial_messages":source.len()-2,
                "legacy_initial_json_bytes":legacy.len(),"recent_page_json_bytes":recent_bytes,
                "recent_page_messages":recent.end-recent.start,
                "initial_view_json_bytes":initial_summary,"updated_view_json_bytes":updated_summary,
                "append_page_json_bytes":delta_bytes,"append_page_messages":delta.data.len(),
                "unchanged_polls":30,"legacy_requests":32,"optimized_requests":34,
                "legacy_body_bytes":baseline_bytes,"optimized_body_bytes":optimized_bytes,
                "body_reduction_percent":100.0*(1.0-optimized_bytes as f64/baseline_bytes as f64),
                "complete_source_reconstruction":true
            }));
        }
        let report = json!({
            "schema":1,"reports":reports,"notes":[
                "Synthetic deterministic 128-tool-round workload using actual provider Message/ToolCall types and the production pager.",
                "One initial render, thirty unchanged polls, then one two-message append observation; 304 bodies are empty.",
                "Counts JSON response body bytes only: excludes HTTP headers, transport compression, database/Worker CPU and latency.",
                "Recent history is progressive disclosure; complete original history remains obtainable and reconstruction is asserted.",
                "Optimized requests increase by two for independent metadata/history resources; conditional reads still authenticate."
            ]
        });
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let output = root.join(".cache/perf/v3");
        std::fs::create_dir_all(&output).unwrap();
        std::fs::write(
            output.join("dataflow.json"),
            serde_json::to_vec_pretty(&report).unwrap(),
        )
        .unwrap();
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
    }

    proptest! {
        #[test]
        fn forward_and_backward_pages_reconstruct_the_original_once(
            count in 0usize..300,
            limit in 1usize..=MAX_LIMIT,
        ) {
            let source = messages(count);
            let mut start = 0;
            let mut forward = Vec::new();
            loop {
                let request = query(&format!("after={}&limit={limit}", cursor("s", start)), "s").unwrap();
                let result = page("s", &source, request).unwrap();
                forward.extend(result.data.iter().map(|entry| entry.sequence));
                start = result.end;
                if !result.has_newer { break; }
            }
            prop_assert_eq!(forward, (0..count).collect::<Vec<_>>());
            let mut end = count;
            let mut backward = Vec::new();
            loop {
                let request = query(&format!("before={}&limit={limit}", cursor("s", end)), "s").unwrap();
                let result = page("s", &source, request).unwrap();
                backward.splice(0..0, result.data.iter().map(|entry| entry.sequence));
                end = result.start;
                if !result.has_older { break; }
            }
            prop_assert_eq!(backward, (0..count).collect::<Vec<_>>());
        }
    }
}
