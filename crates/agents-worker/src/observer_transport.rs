//! Private terminal transport between a Session DO and the public HTTP worker.

use eventsource_stream::{EventStreamError, Eventsource};
use futures::{Stream, StreamExt, channel::mpsc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use worker::{Response, Result};

use crate::{session::sse, util};

const HEADER: &str = "x-agents-internal-observer";
const EVENT: &str = "agents.json";
// Responses output is capped at 4 MiB; Chat provider bodies at 16 MiB. Leave
// room for JSON framing/escaping without accepting an unbounded internal body.
const MAX_BYTES: usize = 32 * 1024 * 1024;
const MAX_WIRE_BYTES: usize = MAX_BYTES + 64 * 1024;

pub(crate) type Sender = mpsc::UnboundedSender<Result<Vec<u8>>>;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Reply {
    pub(crate) status: u16,
    pub(crate) body: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DecodeError {
    Detached,
    Invalid,
}

#[derive(Serialize)]
struct ReplyRef<'a> {
    status: u16,
    body: &'a Value,
}

pub(crate) fn channel(kind: &'static str) -> Result<(Sender, Response)> {
    let (sender, receiver) = mpsc::unbounded();
    let mut response = Response::from_stream(sse::keep_alive(receiver))?;
    response.headers_mut().set(HEADER, kind)?;
    response
        .headers_mut()
        .set("Content-Type", "text/event-stream")?;
    response
        .headers_mut()
        .set("Cache-Control", "no-cache, no-transform")?;
    Ok((sender, response))
}

pub(crate) fn publish(sender: Sender, status: u16, body: Value) -> bool {
    let Some(frame) = encode_terminal(status, &body) else {
        return false;
    };
    publish_frame(sender, &frame)
}

pub(crate) fn encode_terminal(status: u16, body: &Value) -> Option<Vec<u8>> {
    encode_with_limit(status, body, MAX_BYTES)
}

fn encode_with_limit(status: u16, body: &Value, limit: usize) -> Option<Vec<u8>> {
    if !(status == 200 || (400..600).contains(&status)) || !body.is_object() {
        return None;
    }
    let mut frame = format!("event: {EVENT}\ndata: ").into_bytes();
    let prefix = frame.len();
    serde_json::to_writer(&mut frame, &ReplyRef { status, body }).ok()?;
    if frame.len().saturating_sub(prefix) > limit {
        return None;
    }
    frame.extend_from_slice(b"\n\n");
    Some(frame)
}

pub(crate) fn publish_frame(sender: Sender, frame: &[u8]) -> bool {
    if frame.len() > MAX_WIRE_BYTES {
        return false;
    }
    sender.unbounded_send(Ok(frame.to_vec())).is_ok()
        && sender
            .unbounded_send(Ok(b"data: [DONE]\n\n".to_vec()))
            .is_ok()
}

/// Only called on trusted DO responses. Public JSON headers/body remain withheld
/// while the private stream supplies native I/O and comment-only keepalives.
pub(crate) async fn resolve(mut response: Response) -> Result<Response> {
    let kind = response.headers().get(HEADER)?;
    if kind.is_none() {
        return Ok(response);
    }
    let headers = response.headers().clone();
    let result = match kind.as_deref() {
        Some("chat" | "responses" | "compact") => match response.stream() {
            Ok(stream) => decode(stream).await,
            Err(_) => Err(()),
        },
        _ => Err(()),
    };
    let mut output = match result {
        Ok(reply) => Response::from_json(&reply.body)?.with_status(reply.status),
        Err(()) => util::json_error(
            503,
            "run_detached",
            match kind.as_deref() {
                Some("chat") => "Run continues durably; poll the session status",
                Some("compact") => {
                    "Repeat the SAME compact idempotency key; do not recreate the operation"
                }
                _ => "Run continues durably; retrieve the original response",
            },
        )?,
    };
    for (name, value) in &headers {
        if !matches!(
            name.as_str(),
            HEADER | "content-type" | "content-length" | "cache-control" | "transfer-encoding"
        ) {
            output.headers_mut().set(&name, &value)?;
        }
    }
    Ok(output)
}

pub(crate) async fn decode<S>(source: S) -> std::result::Result<Reply, ()>
where
    S: Stream<Item = Result<Vec<u8>>> + Unpin,
{
    decode_detailed(source).await.map_err(|_| ())
}

pub(crate) async fn decode_detailed<S>(source: S) -> std::result::Result<Reply, DecodeError>
where
    S: Stream<Item = Result<Vec<u8>>> + Unpin,
{
    decode_limited(source, MAX_WIRE_BYTES).await
}

async fn decode_limited<S>(source: S, limit: usize) -> std::result::Result<Reply, DecodeError>
where
    S: Stream<Item = Result<Vec<u8>>> + Unpin,
{
    let mut bytes = 0usize;
    let mut events = source
        .map(move |chunk| {
            let chunk = chunk.map_err(|_| DecodeError::Detached)?;
            bytes = bytes.saturating_add(chunk.len());
            if bytes > limit {
                return Err(DecodeError::Invalid);
            }
            Ok(chunk)
        })
        .eventsource();
    let mut reply = None;
    while let Some(event) = events.next().await {
        let event = match event {
            Ok(event) => event,
            Err(EventStreamError::Transport(DecodeError::Detached)) => {
                return reply.ok_or(DecodeError::Detached);
            }
            Err(_) => return Err(DecodeError::Invalid),
        };
        if event.data == "[DONE]" {
            return reply.ok_or(DecodeError::Invalid);
        }
        if event.event != EVENT || reply.is_some() {
            return Err(DecodeError::Invalid);
        }
        if event.data.len() > MAX_BYTES {
            return Err(DecodeError::Invalid);
        }
        let value: Reply = serde_json::from_str(&event.data).map_err(|_| DecodeError::Invalid)?;
        if !(value.status == 200 || (400..600).contains(&value.status)) || !value.body.is_object() {
            return Err(DecodeError::Invalid);
        }
        reply = Some(value);
    }
    reply.ok_or(DecodeError::Detached)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{executor::block_on, stream};
    use serde_json::json;

    fn read(bytes: &[u8]) -> std::result::Result<Reply, ()> {
        block_on(decode(stream::iter([Ok(bytes.to_vec())])))
    }

    #[test]
    fn terminal_status_and_body_survive_comments_and_chunk_boundaries() {
        let (sender, receiver) = mpsc::unbounded();
        sender
            .unbounded_send(Ok(b": keepalive\n\n".to_vec()))
            .unwrap();
        let body = json!({"error":{"code":"provider_error"}});
        assert!(publish(sender, 502, body.clone()));
        let reply = block_on(decode(receiver)).unwrap();
        assert_eq!(reply.status, 502);
        assert_eq!(reply.body, body);
        let reply = read(
            b"event: agents.json\ndata: {\"status\":200,\"body\":{\"status\":\"cancelled\"}}\n\n",
        )
        .unwrap();
        assert_eq!(reply.body["status"], "cancelled");
        let data = "event: agents.json\ndata: {\"status\":200,\"body\":{\"text\":\"雪\"}}\n\n";
        let chunks = data.as_bytes().chunks(3).map(|chunk| Ok(chunk.to_vec()));
        let reply = block_on(decode(stream::iter(chunks))).unwrap();
        assert_eq!(reply.body["text"], "雪");
        assert!(MAX_WIRE_BYTES > MAX_BYTES + 64);
    }

    #[test]
    fn incomplete_duplicate_and_invalid_envelopes_never_become_success() {
        for data in [
            ": keepalive\n\n",
            "data: [DONE]\n\n",
            "event: agents.json\ndata: {\"status\":200,\"body\":{}",
            "event: agents.json\ndata: {\"status\":201,\"body\":{}}\n\n",
            "event: agents.json\ndata: {\"status\":200,\"body\":null}\n\n",
            "event: message\ndata: {\"status\":200,\"body\":{}}\n\n",
            "event: agents.json\ndata: {\"status\":200,\"body\":{}}\n\nevent: agents.json\ndata: {\"status\":200,\"body\":{}}\n\n",
        ] {
            assert!(read(data.as_bytes()).is_err(), "{data}");
        }
    }

    #[test]
    fn disconnected_observer_never_acknowledges_delivery() {
        let (sender, receiver) = mpsc::unbounded();
        drop(receiver);
        assert!(!publish(sender, 200, json!({"status":"completed"})));
    }

    #[test]
    fn shared_encoding_uses_the_same_typed_status_and_payload_bound() {
        assert!(encode_terminal(201, &json!({})).is_none());
        assert!(encode_terminal(200, &Value::Null).is_none());
        let body = json!({"text":"雪"});
        let bytes = serde_json::to_vec(&ReplyRef {
            status: 200,
            body: &body,
        })
        .unwrap()
        .len();
        assert!(encode_with_limit(200, &body, bytes - 1).is_none());
        let frame = encode_with_limit(200, &body, bytes).unwrap();
        let reply = read(&frame).unwrap();
        assert_eq!(reply.status, 200);
        assert_eq!(reply.body, body);
        assert!(frame.len() < bytes + (MAX_WIRE_BYTES - MAX_BYTES));
    }

    #[test]
    fn detailed_decode_distinguishes_transport_loss_from_protocol_failure() {
        assert_eq!(
            block_on(decode_detailed(stream::empty())).unwrap_err(),
            DecodeError::Detached
        );
        let source = stream::iter([Err(worker::Error::RustError("connection_closed".into()))]);
        assert_eq!(
            block_on(decode_detailed(source)).unwrap_err(),
            DecodeError::Detached
        );
        let bad = b"event: agents.json\ndata: {broken}\n\n".to_vec();
        assert_eq!(
            block_on(decode_detailed(stream::iter([Ok(bad)]))).unwrap_err(),
            DecodeError::Invalid
        );
        assert_eq!(
            block_on(decode_detailed(stream::iter([Ok(vec![0xff])]))).unwrap_err(),
            DecodeError::Invalid
        );
        let frame = encode_terminal(200, &json!({"status":"completed"})).unwrap();
        assert_eq!(
            block_on(decode_limited(
                stream::iter([Ok(frame.clone())]),
                frame.len() - 1
            ))
            .unwrap_err(),
            DecodeError::Invalid
        );
        assert_eq!(
            block_on(decode_detailed(stream::iter([
                Ok(frame.clone()),
                Ok(frame)
            ])))
            .unwrap_err(),
            DecodeError::Invalid
        );
    }

    #[test]
    fn an_authentic_terminal_survives_loss_before_the_optional_done_frame() {
        let body = json!({"run_id":"run-original","status":"completed"});
        let frame = encode_terminal(200, &body).unwrap();
        let source = stream::iter([
            Ok(frame),
            Err(worker::Error::RustError("connection_closed".into())),
        ]);
        let reply = block_on(decode_detailed(source)).unwrap();
        assert_eq!(reply.status, 200);
        assert_eq!(reply.body, body);
    }
}
