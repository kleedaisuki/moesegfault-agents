"""SDK harness checks against canonical wire fixtures, not server acceptance evidence."""

import hashlib
import json
import uuid
from collections.abc import Iterator
from typing import Any
from unittest.mock import MagicMock

import httpx
import pytest
from openai import APIStatusError, OpenAI
from openai.types.responses import Response, ResponseStreamEvent
from pydantic import TypeAdapter, ValidationError

from conftest import Http
from response_checks import checked_response, checked_stream
import test_responses as response_journeys
import test_space as space_journeys


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


def test_sdk_input_item_iteration_fetches_subsequent_cursor_pages() -> None:
    requests: list[httpx.Request] = []

    def respond(request: httpx.Request) -> httpx.Response:
        requests.append(request)
        assert request.method == "GET"
        assert request.url.path == "/v1/responses/resp-fixture-1/input_items"
        assert request.url.params["limit"] == "1" and request.url.params["order"] == "asc"
        after = request.url.params.get("after")
        assert after in (None, "input-first")
        item_id = "input-first" if after is None else "input-last"
        return httpx.Response(200, json={
            "object": "list", "data": [{
                "id": item_id, "type": "message", "role": "user", "status": "completed",
                "content": [{"type": "input_text", "text": item_id}],
            }], "first_id": item_id, "last_id": item_id, "has_more": after is None,
        })

    with OpenAI(api_key="fixture-only", base_url="https://fixture.invalid/v1", max_retries=0,
                http_client=httpx.Client(transport=httpx.MockTransport(respond))) as client:
        page = client.responses.input_items.list("resp-fixture-1", limit=1, order="asc")
        assert [item.id for item in page] == ["input-first", "input-last"]
    assert len(requests) == 2


def test_sdk_response_delete_preserves_raw_receipt_despite_void_parsed_return() -> None:
    requests: list[httpx.Request] = []
    receipt = {"id": "resp-fixture-1", "object": "response.deleted", "deleted": True}

    def respond(request: httpx.Request) -> httpx.Response:
        requests.append(request)
        assert request.url.path == "/v1/responses/resp-fixture-1"
        if request.method == "DELETE":
            return httpx.Response(200, json=receipt)
        assert request.method == "GET"
        return httpx.Response(404, json={"error": {"code": "not_found", "message": "Response not found"}})

    with OpenAI(api_key="fixture-only", base_url="https://fixture.invalid/v1", max_retries=0,
                http_client=httpx.Client(transport=httpx.MockTransport(respond))) as client:
        raw = client.responses.with_raw_response.delete("resp-fixture-1")
        assert raw.status_code == 200 and raw.http_response.json() == receipt
        assert raw.parse() is None
        with pytest.raises(APIStatusError) as missing:
            client.responses.retrieve("resp-fixture-1")
        assert missing.value.status_code == 404
    assert [request.method for request in requests] == ["DELETE", "GET"]


def test_full_responses_lifecycle_journey_uses_pinned_sdk_contract() -> None:
    requests: list[httpx.Request] = []
    responses: dict[str, dict[str, Any]] = {}
    accepted_body: dict[str, Any] | None = None
    revision = 0
    events: list[dict[str, Any]] = []
    branch_body: dict[str, Any] | None = None
    source_messages: list[dict[str, Any]] = [{"tool_calls": [{"function": {"name": "time"}}]}]
    pin = {"model": {"provider": "deepseek", "model": "deepseek-flash"}}

    def respond(request: httpx.Request) -> httpx.Response:
        nonlocal accepted_body, revision, events, branch_body
        requests.append(request)
        if request.method == "POST":
            body = json.loads(request.content)
            if request.headers.get("Idempotency-Key", "").startswith("fork-"):
                assert body["previous_response_id"] == "resp-fixture-1"
                if branch_body is None:
                    branch_body = body
                    responses["resp-fork"] = {
                        **response_fixture(response_id="resp-fork"),
                        "previous_response_id": "resp-fixture-1",
                    }
                assert body == branch_body
                return httpx.Response(200, headers={"x-agents-session": "session-fork"}, json=responses["resp-fork"])
            if request.headers.get("Idempotency-Key") and accepted_body is not None:
                if body == accepted_body:
                    return httpx.Response(200, json=responses["resp-fixture-1"])
                return httpx.Response(409, json={"error": {"code": "conflict", "message": "Key conflict"}})
            revision += 1
            if revision == 1:
                accepted_body = body
                value = response_fixture()
                value["instructions"] = body["instructions"]
                value["metadata"] = body["metadata"]
                responses[value["id"]] = value
                return httpx.Response(200, headers={"x-agents-session": "session-fixture"}, json=value)
            assert revision == 2 and body["stream"] is True
            source_messages.append({"role": "user", "content": body["input"]})
            events = event_fixture()
            for event in events:
                if "response" in event:
                    event["response"]["id"] = "resp-fixture-2"
                    event["response"]["previous_response_id"] = "resp-fixture-1"
            responses["resp-fixture-2"] = events[-1]["response"]
            return httpx.Response(200, headers={"content-type": "text/event-stream"}, content=event_wire(events))
        assert request.method == "GET"
        if request.url.path.endswith("/input_items"):
            assert accepted_body is not None
            return httpx.Response(200, json={
                "object": "list", "data": [{
                    "id": "input-first", "type": "message", "role": "user", "status": "completed",
                    "content": [{"type": "input_text", "text": accepted_body["input"]}],
                }], "first_id": "input-first", "last_id": "input-first", "has_more": False,
            })
        if request.url.params.get("stream") == "true":
            after = int(request.url.params["starting_after"])
            suffix = [event for event in events if event["sequence_number"] > after]
            return httpx.Response(200, headers={"content-type": "text/event-stream"}, content=event_wire(suffix))
        return httpx.Response(200, json=responses[request.url.path.rsplit("/", 1)[-1]])

    def session_state(method: str, path: str) -> dict[str, Any]:
        assert method == "GET"
        if path == "/v1/sessions/session-fork":
            assert branch_body is not None
            return {"input_revision": 1, "pinned": pin, "messages": [
                source_messages[0], {"role": "user", "content": branch_body["input"]},
            ]}
        assert path == "/v1/sessions/session-fixture"
        return {"input_revision": revision, "last_input_at_ms": revision * 1000,
                "pinned": pin, "messages": list(source_messages)}

    deployed = MagicMock(spec=Http)
    deployed.json.side_effect = session_state
    sessions: list[str] = []
    with OpenAI(api_key="fixture-only", base_url="https://fixture.invalid/v1", max_retries=0,
                http_client=httpx.Client(transport=httpx.MockTransport(respond))) as client:
        response_journeys.test_responses_sdk_lifecycle_stream_recovery_and_historical_fork(
            deployed, client, ({"id": "agent-fixture", "_acceptance_session": "session-fixture"}, sessions),
        )
    assert sessions == ["session-fixture", "session-fork"] and revision == 2
    assert len(requests) == 11
    initial = requests[0]
    assert initial.headers["X-Agents-Session"] == "session-fixture"


def test_full_background_delete_journey_uses_raw_sdk_receipt(monkeypatch: pytest.MonkeyPatch) -> None:
    requests: list[httpx.Request] = []
    deleted = False
    final = {**response_fixture(), "background": True}
    receipt = {"id": final["id"], "object": "response.deleted", "deleted": True}

    def respond(request: httpx.Request) -> httpx.Response:
        nonlocal deleted
        requests.append(request)
        if request.method == "POST":
            assert json.loads(request.content)["background"] is True
            return httpx.Response(200, headers={"x-agents-session": "session-fixture"}, json={
                **response_fixture(status="queued"), "background": True,
            })
        if request.method == "DELETE":
            deleted = True
            return httpx.Response(200, json=receipt)
        assert request.method == "GET"
        if deleted:
            return httpx.Response(404, json={"error": {"code": "not_found", "message": "Response not found"}})
        return httpx.Response(200, json=final)

    other = MagicMock(spec=Http)
    other.request.return_value = (404, {}, b"")

    def other_account(origin: str, token: str) -> Http:
        assert origin == "https://fixture.invalid" and token == "other-fixture-only"
        return other

    monkeypatch.setenv("MAGENTS_TEST_OTHER_API_KEY", "other-fixture-only")
    monkeypatch.setattr(response_journeys, "Http", other_account)
    deployed = MagicMock(spec=Http)
    deployed.origin = "https://fixture.invalid"
    sessions: list[str] = []
    with OpenAI(api_key="fixture-only", base_url=deployed.origin + "/v1", max_retries=0,
                http_client=httpx.Client(transport=httpx.MockTransport(respond))) as client:
        response_journeys.test_responses_background_and_owner_isolation(
            deployed, client, ({"id": "agent-fixture", "_acceptance_session": "session-fixture"}, sessions),
        )
    assert sessions == ["session-fixture"] and other.request.call_count == 5
    assert [request.method for request in requests] == ["POST", "GET", "GET", "DELETE", "GET"]


def test_zero_byte_journey_uses_its_declared_http_fixture(monkeypatch: pytest.MonkeyPatch) -> None:
    def identifier() -> uuid.UUID:
        return uuid.UUID("01234567-89ab-cdef-0123-456789abcdef")

    monkeypatch.setattr(space_journeys.uuid, "uuid4", identifier)
    filename = "空文件-" + identifier().hex + ".txt"
    file_id = "file-empty-fixture"
    metadata = {
        "id": file_id, "object": "file", "bytes": 0, "created_at": 1_792_860_000,
        "filename": filename, "purpose": "user_data", "status": "processed",
        "sha256": hashlib.sha256(b"").hexdigest(),
    }
    requests: list[httpx.Request] = []

    def respond(request: httpx.Request) -> httpx.Response:
        requests.append(request)
        if request.method == "POST":
            assert request.url.path == "/v1/files"
            assert filename.encode() in request.content
            return httpx.Response(200, json=metadata)
        assert request.method == "GET"
        if request.url.path == f"/v1/files/{file_id}/content":
            return httpx.Response(200, content=b"")
        assert request.url.path == f"/v1/files/{file_id}"
        return httpx.Response(200, json=metadata)

    deployed = MagicMock(spec=Http)
    deployed.request.return_value = (416, {"Content-Range": "bytes */0"}, b"")
    uploads: list[str] = []
    with OpenAI(api_key="fixture-only", base_url="https://fixture.invalid/v1", max_retries=0,
                http_client=httpx.Client(transport=httpx.MockTransport(respond))) as client:
        space_journeys.test_sdk_zero_byte_file_and_unicode_filename(deployed, client, uploads)
    assert uploads == [file_id] and len(requests) == 3
    deployed.request.assert_called_once_with(
        "GET", f"/v1/files/{file_id}/content", headers={"Range": "bytes=0-0"},
    )


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


@pytest.mark.parametrize("mode", ["response", "stream"])
def test_structured_output_schema_alias_survives_sdk_revalidation(mode: str) -> None:
    text = {"format": {"type": "json_schema", "name": "policy_result", "strict": True,
                       "schema": {"type": "object", "properties": {"ok": {"type": "boolean"}}}}}
    value = response_fixture()
    value["text"] = text
    if mode == "response":
        response = checked_response(Response.model_validate(value))
    else:
        raw_events = event_fixture()
        for event in raw_events:
            if "response" in event:
                event["response"]["text"] = text
        adapter: TypeAdapter[ResponseStreamEvent] = TypeAdapter(ResponseStreamEvent)
        response, _ = checked_stream(adapter.validate_python(event) for event in raw_events)
    assert response.model_dump(by_alias=True)["text"]["format"]["schema"] == text["format"]["schema"]


def test_structured_output_schema_is_still_required_on_the_original_wire() -> None:
    value = response_fixture()
    value["text"] = {"format": {"type": "json_schema", "name": "missing_schema", "strict": True}}
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
