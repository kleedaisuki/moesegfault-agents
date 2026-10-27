use super::*;
use proptest::prelude::*;
use std::{hint::black_box, path::Path, time::Instant};

fn previous_state_bytes(data: &PersistedSession) -> Vec<u8> {
    let mut copied = data.clone();
    copied.journal_count = copied.journal.len();
    serde_json::to_vec(&copied).unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]
    #[test]
    fn stored_state_is_byte_identical_without_cloning_source_history(
        entries in prop::collection::vec(".*", 0..16),
        stale_count in any::<usize>(),
    ) {
        let mut data = super::tests::state();
        data.journal = entries.iter().map(|text|Message::text("user",text)).collect();
        data.journal_count = stale_count;
        data.pending.push(Message::text("user", "queued steering"));
        data.messages.push(Message::text("user", "compacted inference view"));
        data.projection_pending = true;
        data.projection_generation = "opaque-generation".into();
        let expected = previous_state_bytes(&data);
        let source = serde_json::to_vec(&data.journal).unwrap();
        let actual = encode_state(&mut data).unwrap();
        prop_assert_eq!(actual, expected);
        prop_assert_eq!(data.journal_count, entries.len());
        prop_assert_eq!(serde_json::to_vec(&data.journal).unwrap(), source);
    }
}

fn workload(output_bytes: usize, recent_rounds: usize) -> PersistedSession {
    let mut data = super::tests::state();
    data.journal
        .push(Message::text("user", "Run focused checks."));
    for round in 0..128 {
        let id = format!("call_{round:04}");
        let mut assistant = Message::text("assistant", "Inspect the next check.");
        assistant.tool_calls.push(provider::ToolCall {
            id: id.clone(),
            kind: "function".into(),
            function: provider::FunctionCall {
                name: "bash".into(),
                arguments: json!({"command":"cargo test","timeout_ms":30000}).to_string(),
            },
        });
        data.journal.push(assistant);
        data.journal.push(Message::tool_result(
            &id,
            json!({"stdout":"x".repeat(output_bytes),"stderr":"","exit_code":0}).to_string(),
        ));
    }
    data.journal
        .push(Message::text("assistant", "Checks complete."));
    data.messages
        .push(Message::text("user", "A compacted summary. ".repeat(64)));
    let tail = recent_rounds * 2;
    data.messages
        .extend_from_slice(&data.journal[data.journal.len() - tail..]);
    data
}

fn timing<T>(mut operation: impl FnMut() -> T) -> Value {
    for _ in 0..3 {
        black_box(operation());
    }
    let mut samples = (0..25)
        .map(|_| {
            let started = Instant::now();
            black_box(operation());
            started.elapsed().as_secs_f64() * 1000.0
        })
        .collect::<Vec<_>>();
    samples.sort_by(f64::total_cmp);
    json!({"samples":samples.len(),"p50_ms":samples[12],"p95_ms":samples[23]})
}

fn value_text_bytes(value: &Value) -> usize {
    match value {
        Value::String(text) => text.len(),
        Value::Array(values) => values.iter().map(value_text_bytes).sum(),
        Value::Object(values) => values.values().map(value_text_bytes).sum(),
        _ => 0,
    }
}

#[test]
fn runtime_persistence_workload_avoids_recopying_a_compacted_source_journal() {
    let mut samples = vec![];
    for (output_bytes, recent_rounds) in [(4096, 0), (32768, 0), (32768, 16)] {
        let mut data = workload(output_bytes, recent_rounds);
        let copied_source_text = value_text_bytes(&serde_json::to_value(&data.journal).unwrap());
        let previous = previous_state_bytes(&data);
        let current = encode_state(&mut data).unwrap();
        assert_eq!(previous, current);
        assert_eq!(data.journal_count, 258);
        let before_deadline = alarm_deadline(&data.clone(), 1000);
        let after_deadline = alarm_deadline(&data, 1000);
        assert_eq!(before_deadline, after_deadline);
        samples.push(json!({
            "tool_rounds":128,"tool_output_bytes":output_bytes,"recent_inference_rounds":recent_rounds,
            "source_messages":data.journal.len(),"active_messages":data.messages.len(),
            "stored_state_bytes":current.len(),
            "source_utf8_payload_copy_bytes_removed_per_operation":copied_source_text,
            "source_utf8_payload_copy_bytes_removed_per_save_plus_alarm":copied_source_text*2,
            "previous_state_encoding":timing(||previous_state_bytes(&data)),
            "current_state_encoding":timing(||encode_state(&mut data).unwrap()),
            "previous_alarm_snapshot":timing(||alarm_deadline(&data.clone(),1000)),
            "current_alarm_borrow":timing(||alarm_deadline(&data,1000)),
        }));
    }
    let report = json!({
        "schema_version":1,"fixture":"actual PersistedSession and provider Message types; synthetic 128-tool-round transcript",
        "build":if cfg!(debug_assertions){"debug"}else{"release"},
        "os":std::env::consts::OS,"arch":std::env::consts::ARCH,
        "source":std::env::var("GITHUB_SHA").ok().filter(|value|value.len()==40&&value.bytes().all(|byte|byte.is_ascii_hexdigit())).unwrap_or_else(||"local".into()),
        "cases":samples,
        "notes":[
            "Native CPU probes exclude Cloudflare KV I/O, Worker Wasm, network, model time and allocator/RSS measurements.",
            "Removed source-string bytes are a lower bound of cloned UTF-8 values only; Vec/map allocations, dynamic object keys and other cloned state are additional.",
            "Both encoders must return byte-identical persisted state. Source journal remains separately persisted and unchanged.",
            "Alarm deadline calculation uses the same pure production function; only the unnecessary owned snapshot is removed.",
            "The shared persistence mutex and generation publication order are unchanged. No latency SLO or whole-workflow speedup is inferred."
        ],
    });
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let directory = root.join(".cache/perf/v3");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join("runtime-persistence.json"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
    println!("{}", serde_json::to_string(&report).unwrap());
}
