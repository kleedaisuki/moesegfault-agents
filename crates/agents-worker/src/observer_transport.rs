//! Private terminal transport between a Session DO and the public HTTP worker.

use eventsource_stream::Eventsource;
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
    let reply = Reply { status, body };
    let Ok(data) = serde_json::to_string(&reply) else {
        return false;
    };
    if data.len() > MAX_BYTES {
        return false;
    }
    sender
        .unbounded_send(Ok(format!("event: {EVENT}\ndata: {data}\n\n").into_bytes()))
        .is_ok()
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
        Some("chat" | "responses") => match response.stream() {
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
            if kind.as_deref() == Some("chat") {
                "Run continues durably; poll the session status"
            } else {
                "Run continues durably; retrieve the original response"
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
    let mut bytes = 0usize;
    let mut events = source
        .map(move |chunk| {
            let chunk = chunk.map_err(|_| ())?;
            bytes = bytes.saturating_add(chunk.len());
            if bytes > MAX_WIRE_BYTES {
                return Err(());
            }
            Ok(chunk)
        })
        .eventsource();
    let mut reply = None;
    while let Some(event) = events.next().await {
        let event = event.map_err(|_| ())?;
        if event.data == "[DONE]" {
            return reply.ok_or(());
        }
        if event.event != EVENT || reply.is_some() {
            return Err(());
        }
        if event.data.len() > MAX_BYTES {
            return Err(());
        }
        let value: Reply = serde_json::from_str(&event.data).map_err(|_| ())?;
        if !(value.status == 200 || (400..600).contains(&value.status)) || !value.body.is_object() {
            return Err(());
        }
        reply = Some(value);
    }
    reply.ok_or(())
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
}
