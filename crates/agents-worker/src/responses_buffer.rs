//! Transient text framing, independent of the durable response/event state.

pub(crate) const BUFFER_BYTES: usize = 1024;
pub(crate) const FLUSH_MS: u64 = 50;

#[derive(Clone, Debug, Default)]
pub(crate) struct DeltaBuffer {
    pending: String,
    deadline: Option<u64>,
    started: bool,
}

impl DeltaBuffer {
    /// The first visible fragment is never delayed. Later fragments share a
    /// deadline that callers must drive with a timer, even when upstream is quiet.
    pub(crate) fn push(&mut self, delta: &str, now_ms: u64) -> Option<String> {
        if delta.is_empty() {
            return None;
        }
        if !self.started {
            self.started = true;
            return Some(delta.into());
        }
        self.pending.push_str(delta);
        let deadline = *self.deadline.get_or_insert(now_ms.saturating_add(FLUSH_MS));
        if self.pending.len() >= BUFFER_BYTES || now_ms >= deadline {
            return self.flush();
        }
        None
    }

    pub(crate) fn deadline_ms(&self) -> Option<u64> {
        self.deadline
    }

    /// Timer and non-text barriers drain pending text without starting a new item.
    pub(crate) fn flush(&mut self) -> Option<String> {
        self.deadline = None;
        if self.pending.is_empty() {
            return None;
        }
        Some(std::mem::take(&mut self.pending))
    }

    /// A text-item boundary also restores immediate delivery for the next item.
    pub(crate) fn finish(&mut self) -> Option<String> {
        let pending = self.flush();
        self.started = false;
        pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::responses_wire::{Events, event_frame};
    use proptest::prelude::*;
    use serde_json::{Value, json};
    use std::path::Path;

    #[test]
    fn first_nonempty_text_is_immediate_and_quiet_pending_text_has_a_fixed_deadline() {
        let mut buffer = DeltaBuffer::default();
        assert_eq!(buffer.push("", 0), None);
        assert_eq!(buffer.deadline_ms(), None);
        assert_eq!(buffer.push("first 🌸", 10), Some("first 🌸".into()));
        assert_eq!(buffer.deadline_ms(), None);
        assert_eq!(buffer.push("second", 11), None);
        assert_eq!(buffer.deadline_ms(), Some(61));
        assert_eq!(buffer.push(" third", 30), None);
        assert_eq!(buffer.deadline_ms(), Some(61));
        assert_eq!(buffer.flush(), Some("second third".into()));
        assert_eq!(buffer.deadline_ms(), None);
        assert_eq!(buffer.flush(), None);
    }

    #[test]
    fn size_threshold_counts_utf8_bytes_and_a_large_fragment_does_not_stay_buffered() {
        let mut buffer = DeltaBuffer::default();
        assert_eq!(buffer.push("first", 0), Some("first".into()));
        let pending = "中".repeat(341);
        assert_eq!(pending.len(), BUFFER_BYTES - 1);
        assert_eq!(buffer.push(&pending, 1), None);
        assert_eq!(buffer.push("🌸", 2), Some(format!("{pending}🌸")));
        assert!(buffer.pending.is_empty());
        assert_eq!(buffer.deadline_ms(), None);
        let large = "x".repeat(1024 * 1024);
        assert_eq!(buffer.push(&large, 3), Some(large));
        assert!(buffer.pending.is_empty());
        assert_eq!(buffer.pending.capacity(), 0);
    }

    #[test]
    fn text_boundaries_flush_exact_text_and_restart_immediate_delivery() {
        let mut buffer = DeltaBuffer::default();
        assert_eq!(buffer.push("first", 0), Some("first".into()));
        assert_eq!(buffer.push("tail", 1), None);
        assert_eq!(buffer.finish(), Some("tail".into()));
        assert_eq!(buffer.deadline_ms(), None);
        assert_eq!(buffer.push("next item", 2), Some("next item".into()));
        assert_eq!(buffer.finish(), None);
        assert_eq!(buffer.push("third item", 3), Some("third item".into()));
    }

    #[test]
    fn elapsed_push_and_saturating_clock_do_not_extend_pending_text_deadlines() {
        let mut buffer = DeltaBuffer::default();
        buffer.push("first", 0);
        buffer.push("waiting", 1);
        assert_eq!(buffer.push(" now", 51), Some("waiting now".into()));
        assert_eq!(buffer.push("last", u64::MAX), Some("last".into()));
        assert_eq!(buffer.deadline_ms(), None);
    }

    struct Arrival {
        at_ms: u64,
        text: String,
    }

    fn coalesced(arrivals: &[Arrival], completed_at: u64) -> Vec<(u64, String)> {
        let mut buffer = DeltaBuffer::default();
        let mut frames = Vec::new();
        for arrival in arrivals {
            if let Some(deadline) = buffer.deadline_ms().filter(|due| *due <= arrival.at_ms) {
                if let Some(text) = buffer.flush() {
                    frames.push((deadline, text));
                }
            }
            if let Some(text) = buffer.push(&arrival.text, arrival.at_ms) {
                frames.push((arrival.at_ms, text));
            }
        }
        if let Some(deadline) = buffer.deadline_ms().filter(|due| *due <= completed_at) {
            if let Some(text) = buffer.flush() {
                frames.push((deadline, text));
            }
        }
        if let Some(text) = buffer.finish() {
            frames.push((completed_at, text));
        }
        frames
    }

    fn frame_bytes(frames: &[(u64, String)], padding: bool) -> usize {
        let mut events = Events::default();
        frames
            .iter()
            .map(|(_, text)| {
                let mut event = events.text_delta("msg_benchmark_0", 0, 0, text);
                if padding {
                    // Normalize random padding length to compare framing policies,
                    // not a favorable random sample from the obfuscation source.
                    event["obfuscation"] = json!("a".repeat(24));
                }
                event_frame(&event).len()
            })
            .sum()
    }

    fn report(name: &str, arrivals: Vec<Arrival>, completed_at: u64) -> Value {
        let baseline = arrivals
            .iter()
            .map(|arrival| (arrival.at_ms, arrival.text.clone()))
            .collect::<Vec<_>>();
        let optimized = coalesced(&arrivals, completed_at);
        assert_eq!(
            baseline
                .iter()
                .map(|(_, text)| text.as_str())
                .collect::<String>(),
            optimized
                .iter()
                .map(|(_, text)| text.as_str())
                .collect::<String>()
        );
        assert_eq!(optimized[0].0, arrivals[0].at_ms);
        let mut delays = Vec::new();
        let mut input = arrivals.iter();
        for (at, text) in &optimized {
            let mut remaining = text.len();
            while remaining > 0 {
                let original = input.next().unwrap();
                remaining = remaining.checked_sub(original.text.len()).unwrap();
                delays.push(at.checked_sub(original.at_ms).unwrap());
            }
        }
        assert!(input.next().is_none());
        assert!(delays.iter().all(|delay| *delay <= FLUSH_MS));
        delays.sort_unstable();
        json!({
            "name":name,"fragments":arrivals.len(),"baseline_frames":baseline.len(),
            "coalesced_frames":optimized.len(),
            "text_bytes":baseline.iter().map(|(_,text)|text.len()).sum::<usize>(),
            "first_fragment_added_ms":optimized[0].0-arrivals[0].at_ms,
            "max_fragment_added_ms":delays.last(),
            "p95_fragment_added_ms":delays[(delays.len()*95).div_ceil(100)-1],
            "padding_disabled":{
                "baseline_body_bytes":frame_bytes(&baseline,false),
                "coalesced_body_bytes":frame_bytes(&optimized,false)
            },
            "normalized_default_padding":{
                "baseline_body_bytes":frame_bytes(&baseline,true),
                "coalesced_body_bytes":frame_bytes(&optimized,true)
            },
            "exact_text_preserved":true
        })
    }

    #[test]
    fn production_buffer_and_wire_encoder_measure_frames_bytes_and_quiet_gap_policy() {
        let reports = vec![
            report(
                "burst",
                (0..2048)
                    .map(|index| Arrival {
                        at_ms: index * 2,
                        text: "text".into(),
                    })
                    .collect(),
                4096,
            ),
            report(
                "steady",
                (0..512)
                    .map(|index| Arrival {
                        at_ms: index * 20,
                        text: "fragment".into(),
                    })
                    .collect(),
                10240,
            ),
            report(
                "utf8",
                (0..512)
                    .map(|index| Arrival {
                        at_ms: index * 10,
                        text: ["中文", "🌸", "e\u{301}", "x"][index as usize % 4].into(),
                    })
                    .collect(),
                5120,
            ),
            report(
                "quiet_gap",
                vec![
                    Arrival {
                        at_ms: 0,
                        text: "first".into(),
                    },
                    Arrival {
                        at_ms: 1,
                        text: "second".into(),
                    },
                    Arrival {
                        at_ms: 3000,
                        text: "third".into(),
                    },
                ],
                10000,
            ),
        ];
        assert!(reports[0]["coalesced_frames"].as_u64().unwrap() < 100);
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let output = root.join(".cache/perf/v3");
        std::fs::create_dir_all(&output).unwrap();
        let receipt = json!({"schema":1,"reports":reports,"notes":[
            "Synthetic arrival schedules execute the production DeltaBuffer and Responses Events/event_frame encoder; no provider/Cloudflare calls.",
            "Deadline advancement is simulated here; actual timer scheduling and SDK handling require compiled-Worker integration tests.",
            "Default obfuscation is normalized to24 characters to isolate framing overhead; raw actual-default measurements belong to the Worker receipt.",
            "Body counts include delta SSE frames only, not invariant lifecycle frames, compression, transport headers or storage writes."
        ]});
        std::fs::write(
            output.join("responses-framing.json"),
            serde_json::to_vec_pretty(&receipt).unwrap(),
        )
        .unwrap();
        println!("{}", serde_json::to_string_pretty(&receipt).unwrap());
    }

    proptest! {
        #[test]
        fn arbitrary_unicode_fragments_preserve_text_and_bound_pending_bytes(
            fragments in prop::collection::vec(".{0,256}",0..100),
            interval in 0u64..200,
        ) {
            let mut buffer=DeltaBuffer::default();
            let mut actual=String::new();
            for (index,fragment) in fragments.iter().enumerate() {
                let now=index as u64*interval;
                if buffer.deadline_ms().is_some_and(|deadline| deadline<=now) {
                    if let Some(text)=buffer.flush() { actual.push_str(&text); }
                }
                if let Some(text)=buffer.push(fragment,now) {actual.push_str(&text);}
                prop_assert!(buffer.pending.len()<BUFFER_BYTES);
                prop_assert_eq!(buffer.deadline_ms().is_some(),!buffer.pending.is_empty());
            }
            if let Some(text)=buffer.finish() {actual.push_str(&text);}
            prop_assert_eq!(actual,fragments.concat());
            prop_assert!(buffer.deadline_ms().is_none());
        }
    }
}
