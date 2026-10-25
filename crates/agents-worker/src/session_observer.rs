use futures::channel::mpsc;
use serde_json::{Value, json};

use crate::{observer_transport, provider::Usage};

pub(super) type CompletionResult = std::result::Result<Value, String>;
type StreamSender = mpsc::UnboundedSender<worker::Result<Vec<u8>>>;

enum Transport {
    Stream(StreamSender),
    Json(observer_transport::Sender),
}

/// A live observer belongs to one run, independently of mutable session state.
/// Take this value before yielding during finalization; a later attachment must
/// never replace the transport captured by an earlier run.
pub(super) struct Observer {
    run_id: String,
    transport: Transport,
}

impl Observer {
    pub(super) fn stream(run_id: impl Into<String>, sender: StreamSender) -> Self {
        Self {
            run_id: run_id.into(),
            transport: Transport::Stream(sender),
        }
    }

    pub(super) fn json(run_id: impl Into<String>, sender: observer_transport::Sender) -> Self {
        Self {
            run_id: run_id.into(),
            transport: Transport::Json(sender),
        }
    }

    pub(super) fn matches(&self, run_id: &str) -> bool {
        self.run_id == run_id
    }

    pub(super) fn stream_sender(&self, run_id: &str) -> Option<StreamSender> {
        if !self.matches(run_id) {
            return None;
        }
        match &self.transport {
            Transport::Stream(sender) => Some(sender.clone()),
            Transport::Json(_) => None,
        }
    }

    /// Reports channel acceptance, not network delivery. A disconnected observer
    /// does not cancel execution or justify replaying provider/tool side effects.
    pub(super) fn complete(self, outcome: CompletionResult, model: &str, usage: &Usage) -> bool {
        match self.transport {
            Transport::Json(sender) => {
                let (status, body) = match outcome {
                    Ok(value) => (200, value),
                    Err(error) => {
                        let (status, code, message) = crate::routes::classify_error(&error);
                        (status, crate::util::error_body(code, message))
                    }
                };
                observer_transport::publish(sender, status, body)
            }
            Transport::Stream(sender) => {
                let value = terminal_frame(&self.run_id, &outcome, model, usage);
                let terminal = sender
                    .unbounded_send(Ok(format!("data: {value}\n\n").into_bytes()))
                    .is_ok();
                let done = sender
                    .unbounded_send(Ok(b"data: [DONE]\n\n".to_vec()))
                    .is_ok();
                terminal && done
            }
        }
    }
}

pub(super) fn terminal_frame(
    run_id: &str,
    outcome: &CompletionResult,
    model: &str,
    usage: &Usage,
) -> Value {
    match outcome {
        Ok(value) => {
            // Reuse the durable completion's timestamp, so terminal replay does
            // not depend on a platform clock or change completion identity.
            let created = value.get("created").and_then(Value::as_u64).unwrap_or(0);
            json!({"id":run_id,"object":"chat.completion.chunk","created":created,"model":model,
                "choices":[{"index":0,"delta":{},"finish_reason":"stop"}],
                "usage":super::openai_usage(usage)})
        }
        Err(code) => {
            json!({"error":{"code":code,"message":"Agent run failed; inspect session diagnostics"}})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{FutureExt, StreamExt, executor::block_on};

    fn completion() -> CompletionResult {
        Ok(json!({"created":123,"choices":[{"message":{"role":"assistant","content":"done"}}]}))
    }

    #[test]
    fn captured_old_observer_does_not_steal_new_attachment() {
        let (old_sender, old_receiver) = mpsc::unbounded();
        let mut slot = Some(Observer::json("old", old_sender));
        let old = slot.take().unwrap();
        let (new_sender, mut new_receiver) = mpsc::unbounded();
        slot = Some(Observer::json("new", new_sender));
        assert!(old.complete(completion(), "agent", &Usage::default()));
        let old_reply = block_on(observer_transport::decode(old_receiver)).unwrap();
        assert_eq!(old_reply.status, 200);
        assert_eq!(old_reply.body, completion().unwrap());
        assert!(new_receiver.next().now_or_never().is_none());
        assert!(slot.as_ref().unwrap().matches("new"));
        assert!(
            slot.take()
                .unwrap()
                .complete(Err("new_error".into()), "agent", &Usage::default())
        );
        let new_reply = block_on(observer_transport::decode(new_receiver)).unwrap();
        assert_eq!(new_reply.status, 500);
        assert_eq!(new_reply.body["error"]["code"], "internal_error");
    }

    #[test]
    fn wrong_run_cannot_stream_and_json_has_no_stream_sender() {
        let (sender, receiver) = mpsc::unbounded();
        let observer = Observer::stream("run", sender);
        assert!(observer.stream_sender("other").is_none());
        assert!(observer.stream_sender("run").is_some());
        let (sender, _receiver) = mpsc::unbounded();
        let json = Observer::json("run", sender);
        assert!(json.stream_sender("run").is_none());
        drop(observer);
        assert!(block_on(receiver.collect::<Vec<_>>()).is_empty());
    }

    #[test]
    fn stream_success_emits_usage_stop_then_done_and_closes() {
        let (sender, receiver) = mpsc::unbounded();
        let usage = Usage {
            prompt_tokens: 7,
            completion_tokens: 3,
            total_tokens: 10,
            cached_tokens: 5,
            ..Usage::default()
        };
        assert!(Observer::stream("run", sender).complete(completion(), "agent", &usage));
        let frames = block_on(receiver.collect::<Vec<_>>())
            .into_iter()
            .collect::<worker::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1], b"data: [DONE]\n\n");
        let wire = std::str::from_utf8(&frames[0]).unwrap();
        let value: Value =
            serde_json::from_str(wire.trim().strip_prefix("data: ").unwrap()).unwrap();
        assert_eq!(value["id"], "run");
        assert_eq!(value["model"], "agent");
        assert_eq!(value["created"], 123);
        assert_eq!(value["choices"][0]["finish_reason"], "stop");
        assert_eq!(value["usage"]["prompt_tokens_details"]["cached_tokens"], 5);
    }

    #[test]
    fn stream_error_emits_error_then_done_without_success_stop() {
        let (sender, receiver) = mpsc::unbounded();
        assert!(Observer::stream("run", sender).complete(
            Err("run_failed".into()),
            "agent",
            &Usage::default()
        ));
        let frames = block_on(receiver.collect::<Vec<_>>());
        assert_eq!(frames.len(), 2);
        let wire = std::str::from_utf8(frames[0].as_ref().unwrap()).unwrap();
        assert!(wire.contains("run_failed"));
        assert!(!wire.contains("finish_reason"));
        assert_eq!(frames[1].as_ref().unwrap(), b"data: [DONE]\n\n");
    }

    #[test]
    fn cancelled_observer_reports_no_channel_acceptance() {
        let (sender, receiver) = mpsc::unbounded();
        drop(receiver);
        assert!(!Observer::json("run", sender).complete(completion(), "agent", &Usage::default()));
        let (sender, receiver) = mpsc::unbounded();
        drop(receiver);
        assert!(!Observer::stream("run", sender).complete(
            completion(),
            "agent",
            &Usage::default()
        ));
    }
}
