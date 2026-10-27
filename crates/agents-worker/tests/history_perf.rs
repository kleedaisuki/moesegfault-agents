//! Opt-in native history-cost probe. No provider, Cloudflare or filesystem workspace calls.
#![allow(dead_code)]

use agents_worker::{FunctionCall, Message, ToolCall};
use serde::Serialize;
use serde_json::{Value, json};
use std::{hint::black_box, path::Path, time::Instant};

#[derive(Clone)]
struct History {
    messages: Vec<Message>,
    journal: Vec<Message>,
}

#[derive(Serialize)]
struct View<'a> {
    messages: &'a [Message],
}

fn trace(rounds: usize, output_bytes: usize) -> Vec<Message> {
    let mut journal = vec![Message::text(
        "user",
        "Inspect the repository, run targeted checks, and fix the implementation.",
    )];
    for round in 0..rounds {
        let id = format!("call_{round:04}");
        let mut assistant = Message::text(
            "assistant",
            "Run the next focused check and inspect its result.",
        );
        assistant.extra.insert(
            "reasoning_content".into(),
            json!("Check the observed behavior before changing the implementation. ".repeat(8)),
        );
        assistant.tool_calls.push(ToolCall {
            id: id.clone(), kind: "function".into(),
            function: FunctionCall {
                name: "bash".into(),
                arguments: json!({"command":format!("cargo test --package module_{round} -- --nocapture"),"timeout_ms":30000}).to_string(),
            },
        });
        journal.push(assistant);
        let mut output = String::with_capacity(output_bytes + 100);
        let mut line = 0_u64;
        while output.len() < output_bytes {
            let digest = ((round as u64 + 1) * 0x9e3779b9).wrapping_mul(line + 17);
            output.push_str(&format!(
                "test module_{round:03}::case_{line:04} ... ok; observed={digest:016x}\n"
            ));
            line += 1;
        }
        output.truncate(output_bytes);
        journal.push(Message::tool_result(&id, json!({"id":format!("exec_{round}"),"stdout":output,"stderr":"","exit_code":0,"timed_out":false,"truncated":false}).to_string()));
    }
    journal.push(Message::text(
        "assistant",
        "The targeted changes and checks are complete.",
    ));
    journal
}

fn timing<T>(mut work: impl FnMut() -> T) -> Value {
    for _ in 0..3 {
        black_box(work());
    }
    let mut times = (0..25)
        .map(|_| {
            let start = Instant::now();
            black_box(work());
            start.elapsed().as_secs_f64() * 1000.0
        })
        .collect::<Vec<_>>();
    times.sort_by(f64::total_cmp);
    json!({"samples":times.len(),"p50_ms":times[12],"p95_ms":times[23]})
}

fn current_status(history: &History) -> Vec<u8> {
    let snapshot = history.clone();
    let mut value = json!({});
    value["messages"] = serde_json::to_value(&snapshot.journal).unwrap();
    serde_json::to_vec(&value).unwrap()
}

#[test]
#[ignore = "explicit performance probe; run with --ignored --nocapture"]
fn measure_history_cost() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let output = root.join(".cache/perf/history");
    std::fs::create_dir_all(&output).unwrap();
    let mut reports = vec![];
    for bytes in [4096, 32768] {
        let journal = trace(128, bytes);
        let mut compacted = vec![
            Message::text("system", "Pinned session instructions."),
            Message::text(
                "assistant",
                "Summary of previous checked changes. ".repeat(32),
            ),
        ];
        compacted.extend_from_slice(&journal[journal.len() - 33..]);
        let history = History {
            messages: compacted,
            journal,
        };
        let current = current_status(&history);
        let borrowed = serde_json::to_vec(&View {
            messages: &history.journal,
        })
        .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&current).unwrap(),
            serde_json::from_slice::<Value>(&borrowed).unwrap()
        );
        let page = serde_json::to_vec(&View {
            messages: &history.journal[history.journal.len() - 2..],
        })
        .unwrap();
        let file = format!("trace-128-{bytes}.json");
        std::fs::write(output.join(&file), &current).unwrap();
        let chunks = history
            .journal
            .iter()
            .map(|message| {
                serde_json::to_vec(message)
                    .unwrap()
                    .len()
                    .div_ceil(256 * 1024)
            })
            .sum::<usize>();
        reports.push(json!({
            "rounds":128,"tool_stdout_bytes":bytes,"journal_messages":history.journal.len(),
            "active_prompt_messages":history.messages.len(),"journal_json_bytes":current.len(),
            "two_message_delta_bytes":page.len(),"payload_file":file,
            "cold_load_serial_storage_gets":history.journal.len() + chunks,
            "snapshot_clone_history_only":timing(|| history.clone()),
            "status_snapshot_plus_value_plus_serialize":timing(|| current_status(&history)),
            "proposed_borrowed_status_serialize":timing(|| serde_json::to_vec(&View { messages:&history.journal }).unwrap()),
            "proposed_two_message_delta_serialize":timing(|| serde_json::to_vec(&View { messages:&history.journal[history.journal.len()-2..] }).unwrap()),
        }));
    }
    let report = json!({"build":if cfg!(debug_assertions) {"debug"} else {"release"},"arch":std::env::consts::ARCH,"os":std::env::consts::OS,"samples":reports,"notes":["Uses actual provider Message/ToolCall types; synthetic deterministic test-log corpus, not observed customer traffic.","Native Rust history-only CPU timings exclude Workers/wasm, D1, DO storage latency, network and browser rendering.","Current status sends one source transcript, not both source and compacted prompt; optimization alternatives are prototypes, not deployed changes."]});
    std::fs::write(
        output.join("native.json"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
}
