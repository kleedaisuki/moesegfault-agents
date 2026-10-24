"""SDK harness checks against canonical wire fixtures, not server acceptance evidence."""

import json
from collections.abc import Iterator
from typing import Any

import httpx
import pytest
from openai import APIStatusError, OpenAI
from pydantic import ValidationError

from response_checks import checked_response, checked_stream


def response_fixture(
    *,
    response_id: str = "resp-fixture-1",
    text: str = "Hello, 雪.",
    status: str = "completed",
) -> dict[str, Any]:
    return {
        "id": response_id, "object": "response", "created_at": 1_792_860_000,
        "status": status, "error": None, "incomplete_details": None,
        "model": "agent-fixture", "instructions": None, "metadata": {},
        "parallel_tool_calls": True, "tool_choice": "auto", "tools": [],
        "output": [{
            "id": "msg-fixture", "type": "message", "status": "completed", "role": "assistant",
            "content": [{"type": "output_text", "text": text, "annotations": [], "logprobs": []}],
        }] if status == "completed" else [],
        "usage": {
            "input_tokens": 40, "input_tokens_details": {"cached_tokens": 10},
            "output_tokens": 5, "output_tokens_details": {"reasoning_tokens": 0}, "total_tokens": 45,
        } if status == "completed" else None,
    }


def event_fixture() -> list[dict[str, Any]]:
    final = response_fixture()
    pending = {**final, "status": "in_progress", "output": [], "usage": None}
    message = final["output"][0]
    part = message["content"][0]
    events: list[dict[str, Any]] = [
        {"type": "response.created", "response": pending},
        {"type": "response.in_progress", "response": pending},
        {"type": "response.output_item.added", "output_index": 0,
         "item": {**message, "status": "in_progress", "content": []}},
        {"type": "response.content_part.added", "output_index": 0, "content_index": 0,
         "item_id": message["id"], "part": {**part, "text": ""}},
        {"type": "response.output_text.delta", "output_index": 0, "content_index": 0,
         "item_id": message["id"], "delta": "Hello, ", "logprobs": []},
        {"type": "response.output_text.delta", "output_index": 0, "content_index": 0,
         "item_id": message["id"], "delta": "雪.", "logprobs": []},
        {"type": "response.output_text.done", "output_index": 0, "content_index": 0,
         "item_id": message["id"], "text": part["text"], "logprobs": []},
        {"type": "response.content_part.done", "output_index": 0, "content_index": 0,
         "item_id": message["id"], "part": part},
        {"type": "response.output_item.done", "output_index": 0, "item": message},
        {"type": "response.completed", "response": final},
    ]
    return [{**event, "sequence_number": index} for index, event in enumerate(events)]


def event_wire(events: list[dict[str, Any]]) -> bytes:
    return (": keepalive\n\n" + "".join(
        f"event: {event['type']}\ndata: {json.dumps(event, ensure_ascii=False)}\n\n"
        for event in events
    )).encode()


@pytest.fixture
def sdk_fixture() -> Iterator[tuple[OpenAI, list[httpx.Request]]]:
    requests: list[httpx.Request] = []

    def respond(request: httpx.Request) -> httpx.Response:
        requests.append(request)
        if request.method == "POST":
            body = json.loads(request.content)
            if body.get("input") == "fail":
                return httpx.Response(503, json={"error": {"type": "server_error", "code": "unavailable", "message": "Unavailable"}})
            if body.get("stream"):
                events = event_fixture()
                if body.get("input") == "early-eof":
                    events = events[:-1]
                return httpx.Response(200, headers={"content-type": "text/event-stream"}, content=event_wire(events))
            return httpx.Response(200, headers={"x-agents-session": "session-fixture"}, json=response_fixture())
        if request.url.params.get("stream") == "true":
            after = int(request.url.params.get("starting_after", "-1"))
            events = [event for event in event_fixture() if event["sequence_number"] > after]
            return httpx.Response(200, headers={"content-type": "text/event-stream"}, content=event_wire(events))
        return httpx.Response(200, json=response_fixture())

    with OpenAI(
        api_key="fixture-only", base_url="https://fixture.invalid/v1", max_retries=0,
        http_client=httpx.Client(transport=httpx.MockTransport(respond)),
    ) as client:
        yield client, requests


def test_sdk_responses_output_text_and_raw_session_header(
    sdk_fixture: tuple[OpenAI, list[httpx.Request]],
) -> None:
    client, requests = sdk_fixture
    raw = client.responses.with_raw_response.create(model="agent-fixture", input="hello")
    assert raw.headers["x-agents-session"] == "session-fixture"
    response = checked_response(raw.parse())
    assert response.output_text == "Hello, 雪."
    assert requests[0].url.path == "/v1/responses"
    assert json.loads(requests[0].content) == {"model": "agent-fixture", "input": "hello"}


def test_sdk_typed_stream_ignores_heartbeats_and_validates_final_output(
    sdk_fixture: tuple[OpenAI, list[httpx.Request]],
) -> None:
    client, requests = sdk_fixture
    with client.responses.create(model="agent-fixture", input="hello", stream=True) as stream:
        final, events = checked_stream(stream)
    assert final.output_text == "Hello, 雪."
    assert len(events) == 10
    assert len(requests) == 1


def test_sdk_retrieve_stream_uses_standard_cursor_query(
    sdk_fixture: tuple[OpenAI, list[httpx.Request]],
) -> None:
    client, requests = sdk_fixture
    with client.responses.retrieve("resp-fixture-1", stream=True, starting_after=5) as stream:
        final, events = checked_stream(stream, require_created=False)
    assert final.output_text == "Hello, 雪."
    assert [event.sequence_number for event in events] == [6, 7, 8, 9]
    assert requests[0].url.path == "/v1/responses/resp-fixture-1"
    assert dict(requests[0].url.params) == {"stream": "true", "starting_after": "5"}


def test_sdk_early_eof_is_not_a_success_or_reason_to_replay(
    sdk_fixture: tuple[OpenAI, list[httpx.Request]],
) -> None:
    client, requests = sdk_fixture
    with client.responses.create(model="agent-fixture", input="early-eof", stream=True) as stream:
        with pytest.raises(AssertionError, match="terminal outcome"):
            checked_stream(stream)
    original = client.responses.retrieve("resp-fixture-1")
    assert original.status == "completed"
    assert [request.method for request in requests] == ["POST", "GET"]


def test_sdk_transient_error_is_not_retried_for_stateful_execution(
    sdk_fixture: tuple[OpenAI, list[httpx.Request]],
) -> None:
    client, requests = sdk_fixture
    with pytest.raises(APIStatusError) as caught:
        client.responses.create(model="agent-fixture", input="fail")
    assert caught.value.status_code == 503
    assert len(requests) == 1


def test_unknown_reasoning_preserves_exact_counts_without_invalid_standard_schema() -> None:
    value = response_fixture()
    reported = value["usage"]
    reported["output_tokens_details"]["reasoning_tokens"] = None
    value["usage"] = None
    value["agents_usage"] = reported
    response = checked_response(value)
    assert response.usage is None
    assert response.model_extra is not None
    assert response.model_extra["agents_usage"] == {
        "input_tokens": 40, "input_tokens_details": {"cached_tokens": 10},
        "output_tokens": 5, "output_tokens_details": {"reasoning_tokens": None}, "total_tokens": 45,
    }


@pytest.mark.parametrize("status", ["failed", "incomplete", "cancelled"])
def test_terminal_statuses_validate_the_real_sdk_schema(status: str) -> None:
    value = response_fixture(status=status)
    if status == "failed":
        value["error"] = {"code": "server_error", "message": "Execution outcome is unavailable."}
        value["agents_error_code"] = "provider_outcome_unknown"
    elif status == "incomplete":
        value["incomplete_details"] = {"reason": "max_output_tokens"}

    def respond(request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, json=value)

    with OpenAI(api_key="fixture-only", base_url="https://fixture.invalid/v1", max_retries=0,
                http_client=httpx.Client(transport=httpx.MockTransport(respond))) as client:
        response = checked_response(client.responses.retrieve(value["id"]))
    assert response.status == status


@pytest.mark.parametrize("failure", ["custom-error-code", "custom-incomplete-reason"])
def test_internal_codes_cannot_replace_standard_response_literals(failure: str) -> None:
    value = response_fixture(status="failed" if failure == "custom-error-code" else "incomplete")
    if failure == "custom-error-code":
        value["error"] = {"code": "provider_outcome_unknown", "message": "Internal diagnostic code."}
    else:
        value["incomplete_details"] = {"reason": "response_event_limit"}
    with pytest.raises(ValidationError):
        checked_response(value)


@pytest.mark.parametrize("mutation", ["missing-terminal", "duplicate-sequence", "wrong-final-text"])
def test_stream_checker_rejects_invalid_protocol(mutation: str) -> None:
    events = event_fixture()
    if mutation == "missing-terminal":
        events.pop()
    elif mutation == "duplicate-sequence":
        events[5]["sequence_number"] = events[4]["sequence_number"]
    else:
        events[-1]["response"] = response_fixture(text="Different result")

    def respond(request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, headers={"content-type": "text/event-stream"}, content=event_wire(events))

    with OpenAI(api_key="fixture-only", base_url="https://fixture.invalid/v1", max_retries=0,
                http_client=httpx.Client(transport=httpx.MockTransport(respond))) as client:
        with client.responses.create(model="agent-fixture", input="hello", stream=True) as stream:
            with pytest.raises(AssertionError):
                checked_stream(stream)
