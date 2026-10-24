"""Independent SDK-schema and lifecycle checks shared by fixture and live journeys."""

from collections.abc import Iterable
from typing import Any

from openai.types.responses import Response, ResponseStreamEvent
from pydantic import TypeAdapter


_event_schema: TypeAdapter[ResponseStreamEvent] = TypeAdapter(ResponseStreamEvent)


def checked_response(value: Response | dict[str, Any]) -> Response:
    raw = value.model_dump() if isinstance(value, Response) else value
    response = Response.model_validate(raw)
    assert response.id and response.object == "response"
    assert response.created_at > 0
    assert response.status in {"queued", "in_progress", "completed", "failed", "incomplete", "cancelled"}
    if response.status == "completed":
        assert response.error is None
        assert response.incomplete_details is None
    if response.status == "failed":
        assert response.error is not None, "failed responses need a standard typed error"
    if response.status == "incomplete":
        assert response.incomplete_details is not None
        assert response.incomplete_details.reason in {"max_output_tokens", "content_filter"}
    usage = response.usage
    if usage is not None:
        assert usage.input_tokens >= 0 and usage.output_tokens >= 0
        assert usage.total_tokens == usage.input_tokens + usage.output_tokens
        assert 0 <= usage.input_tokens_details.cached_tokens <= usage.input_tokens
        assert 0 <= usage.output_tokens_details.reasoning_tokens <= usage.output_tokens
    elif response.status == "completed" or raw.get("agents_usage") is not None:
        reported = raw.get("agents_usage")
        assert isinstance(reported, dict), "completed response must preserve its authoritative token counts"
        assert set(reported) == {"input_tokens", "input_tokens_details", "output_tokens", "output_tokens_details", "total_tokens"}
        assert type(reported["input_tokens"]) is int and reported["input_tokens"] >= 0
        assert type(reported["output_tokens"]) is int and reported["output_tokens"] >= 0
        assert reported["total_tokens"] == reported["input_tokens"] + reported["output_tokens"]
        cached = reported["input_tokens_details"]["cached_tokens"]
        assert type(cached) is int and 0 <= cached <= reported["input_tokens"]
        assert reported["output_tokens_details"] == {"reasoning_tokens": None}, "unknown reasoning is not a fabricated zero"
    return response


def checked_stream(
    events: Iterable[ResponseStreamEvent],
    *,
    expected_status: str = "completed",
    require_created: bool = True,
) -> tuple[Response, list[ResponseStreamEvent]]:
    observed = [_event_schema.validate_python(event.model_dump()) for event in events]
    assert observed, "response stream ended without lifecycle events"
    sequences = [event.sequence_number for event in observed]
    assert all(number >= 0 for number in sequences)
    assert sequences == sorted(set(sequences)), "stream events must be uniquely ordered"
    if require_created:
        assert observed[0].type == "response.created"
    terminal = [event for event in observed if event.type in {
        "response.completed", "response.failed", "response.incomplete",
    }]
    assert len(terminal) == 1, "response stream must publish exactly one terminal outcome"
    assert observed[-1] is terminal[0], "response stream continued after its terminal outcome"
    response = checked_response(terminal[0].response)
    assert response.status == expected_status
    for event in observed:
        if event.type in {"response.created", "response.in_progress"}:
            assert event.response.id == response.id
    if expected_status == "completed" and require_created:
        text = "".join(event.delta for event in observed if event.type == "response.output_text.delta")
        assert text == response.output_text, "streamed deltas and committed output must agree"
    return response, observed
