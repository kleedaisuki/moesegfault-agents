"""Observe an accepted Responses stream; reconnect only through SDK GET replay.

The attempt budget bounds reconnections, not a healthy stream's wall-clock life.
SDK read timeouts remain explicit. No prompt, tool output or create callback enters
this boundary, and authentic terminal events are never synthesized from a GET.
"""

from __future__ import annotations

import math
import re
import time
from dataclasses import dataclass
from typing import Any

import httpx
from openai import APIConnectionError, OpenAI, Stream
from openai.types.responses import Response, ResponseStreamEvent
from pydantic import TypeAdapter

from response_checks import checked_response, checked_stream


_schema: TypeAdapter[ResponseStreamEvent] = TypeAdapter(ResponseStreamEvent)
_terminal = frozenset({"response.completed", "response.failed", "response.incomplete"})
_transport_errors = (
    APIConnectionError, httpx.ReadError, httpx.ReadTimeout,
    httpx.ConnectError, httpx.ConnectTimeout, httpx.RemoteProtocolError,
)


@dataclass(frozen=True)
class ResponseObservation:
    response: Response
    events: list[ResponseStreamEvent]
    resumptions: int


def _validated_event(
    event: ResponseStreamEvent, response_id: str, last_sequence: int | None,
) -> ResponseStreamEvent:
    raw: dict[str, Any] = event.model_dump(by_alias=True)
    sequence = raw.get("sequence_number")
    assert type(sequence) is int and sequence >= 0, "event sequence must be a nonnegative integer"
    assert last_sequence is None or sequence > last_sequence, "replay cursor must advance without duplicates"
    value = _schema.validate_python(raw)
    if last_sequence is None:
        assert value.type == "response.created", "the complete observation must begin at creation"
    nested = raw.get("response")
    if isinstance(nested, dict):
        assert nested.get("id") == response_id, "event belongs to another response"
    if "response_id" in raw:
        assert raw["response_id"] == response_id, "event belongs to another response"
    assert value.type != "error", "response error/cancellation is not a transport retry"
    return value


def _consume(
    segment: Stream[ResponseStreamEvent], response_id: str, events: list[ResponseStreamEvent],
) -> bool:
    last_sequence = events[-1].sequence_number if events else None
    try:
        for candidate in segment:
            event = _validated_event(candidate, response_id, last_sequence)
            events.append(event)
            last_sequence = event.sequence_number
            if event.type in _terminal:
                return True
    except _transport_errors:
        return False
    finally:
        segment.close()
    return False


def observe_response(
    client: OpenAI,
    initial: Stream[ResponseStreamEvent],
    response_id: str,
    *,
    max_resumptions: int = 3,
    backoff_seconds: float = 0.25,
) -> ResponseObservation:
    """Preserve authentic events and resume their same-ID suffix after transport loss.

    A valid terminal is semantic completion: close its connection immediately,
    rather than waiting for a later TCP EOF. Failed/incomplete/cancelled outcomes,
    malformed events and caller cancellation propagate without reconnection.
    Healthy streams perform no GET. A resumed success must also match canonical
    GET retrieval exactly, while the unchanged strict checker validates its full
    authentic event history and text.
    """
    if not re.fullmatch(r"[A-Za-z0-9_-]{1,128}", response_id):
        initial.close()
        raise ValueError("An accepted response ID is required before observing")
    if type(max_resumptions) is not int or not 0 <= max_resumptions <= 8:
        initial.close()
        raise ValueError("Use a bounded resumption budget between zero and eight")
    if not math.isfinite(backoff_seconds) or backoff_seconds < 0:
        initial.close()
        raise ValueError("Backoff must be finite and nonnegative")

    reader = client.with_options(max_retries=0)
    events: list[ResponseStreamEvent] = []
    current: Stream[ResponseStreamEvent] | None = initial
    resumptions = 0
    while True:
        terminal_seen = current is not None and _consume(current, response_id, events)
        current = None
        if terminal_seen:
            response, observed = checked_stream(events)
            assert response.id == response_id
            if resumptions:
                canonical = checked_response(reader.responses.retrieve(response_id))
                assert canonical.model_dump(by_alias=True) == response.model_dump(by_alias=True), "replayed terminal differs from canonical retrieval"
            return ResponseObservation(response, observed, resumptions)
        if resumptions == max_resumptions:
            raise RuntimeError("Response observation exhausted its GET-only resumption budget")
        time.sleep(backoff_seconds * (2 ** resumptions))
        resumptions += 1
        last_sequence = events[-1].sequence_number if events else None
        cursor = {} if last_sequence is None else {"starting_after": last_sequence}
        try:
            current = reader.responses.retrieve(response_id, stream=True, **cursor)
        except _transport_errors:
            # Opening a replacement observation can fail too; its next attempt
            # consumes the same budget and never changes execution parameters.
            current = None
