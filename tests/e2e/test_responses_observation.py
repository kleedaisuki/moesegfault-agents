"""Pinned SDK transport/replay contracts; no real model or deployment calls."""

from __future__ import annotations

import copy
from collections.abc import Iterator
from typing import Any

import httpx
import pytest
from openai import APIError, APIStatusError, OpenAI
from openai.types.responses import ResponseStreamEvent
from pydantic import TypeAdapter

from response_checks import checked_stream
from responses_observation import observe_response
from test_responses_contract import event_fixture, event_wire, response_fixture


_schema: TypeAdapter[ResponseStreamEvent] = TypeAdapter(ResponseStreamEvent)


class CutStream(httpx.SyncByteStream):
    def __init__(self, data: bytes, error: BaseException | None = None) -> None:
        self.data = data
        self.error = error
        self.closed = False

    def __iter__(self) -> Iterator[bytes]:
        yield self.data
        if self.error is not None:
            raise self.error

    def close(self) -> None:
        self.closed = True


class Fixture:
    def __init__(self, prefix: list[dict[str, Any]], suffix: list[dict[str, Any]] | None = None,
                 *, initial_error: BaseException | None = None, get_status: int = 200,
                 canonical: dict[str, Any] | None = None) -> None:
        self.requests: list[httpx.Request] = []
        self.prefix = prefix
        self.suffix = event_fixture() if suffix is None else suffix
        self.initial = CutStream(event_wire(prefix), initial_error)
        self.get_status = get_status
        self.canonical = response_fixture() if canonical is None else canonical

    def respond(self, request: httpx.Request) -> httpx.Response:
        self.requests.append(request)
        if request.method == "POST":
            assert request.url.path == "/v1/responses"
            return httpx.Response(200, headers={"content-type": "text/event-stream", "x-agents-response": "resp-fixture-1"}, stream=self.initial)
        assert request.method == "GET", "Observation is never allowed to issue an execution POST"
        assert request.url.path == "/v1/responses/resp-fixture-1"
        if self.get_status != 200:
            return httpx.Response(self.get_status, json={"error": {"code": "observation_unavailable", "message": "Unavailable"}})
        if request.url.params.get("stream") == "true":
            after = request.url.params.get("starting_after")
            events = self.suffix if after is None else [event for event in self.suffix if event["sequence_number"] > int(after)]
            return httpx.Response(200, headers={"content-type": "text/event-stream"}, content=event_wire(events))
        return httpx.Response(200, json=self.canonical)

    def client(self) -> OpenAI:
        return OpenAI(api_key="fixture-only", base_url="https://fixture.invalid/v1", max_retries=0,
                      http_client=httpx.Client(transport=httpx.MockTransport(self.respond)))


@pytest.mark.parametrize("cut", [0, 1, 5, 6, 9])
@pytest.mark.parametrize("transport_error", [False, True])
def test_get_replay_preserves_the_complete_authentic_unicode_history(cut: int, transport_error: bool) -> None:
    events = event_fixture()
    fixture = Fixture(events[:cut], initial_error=httpx.ReadError("synthetic connection loss") if transport_error else None)
    with fixture.client() as client:
        raw = client.responses.with_raw_response.create(model="agent-fixture", input="Exactly one execution", stream=True)
        result = observe_response(client, raw.parse(), raw.headers["x-agents-response"], backoff_seconds=0)
    assert result.response.output_text == "Hello, 雪."
    assert result.resumptions == 1
    _, baseline = checked_stream(_schema.validate_python(event) for event in events)
    assert [event.model_dump(by_alias=True) for event in result.events] == [event.model_dump(by_alias=True) for event in baseline]
    assert [request.method for request in fixture.requests] == ["POST", "GET", "GET"]
    replay = fixture.requests[1]
    assert replay.url.params.get("starting_after") == (str(cut - 1) if cut else None)
    assert fixture.initial.closed


def test_healthy_terminal_does_not_require_clean_eof_or_any_get() -> None:
    fixture = Fixture(event_fixture(), initial_error=httpx.ReadError("after semantic completion"))
    with fixture.client() as client:
        raw = client.responses.with_raw_response.create(model="agent-fixture", input="One execution", stream=True)
        result = observe_response(client, raw.parse(), raw.headers["x-agents-response"], backoff_seconds=0)
    assert result.resumptions == 0 and result.response.status == "completed"
    assert [request.method for request in fixture.requests] == ["POST"]
    assert fixture.initial.closed


@pytest.mark.parametrize("status", [403, 404, 410, 429])
def test_unavailable_or_overloaded_replay_is_not_retried(status: int) -> None:
    fixture = Fixture(event_fixture()[:1], get_status=status)
    with fixture.client() as client:
        stream = client.responses.create(model="agent-fixture", input="One execution", stream=True)
        with pytest.raises(APIStatusError) as error:
            observe_response(client, stream, "resp-fixture-1", backoff_seconds=0)
    assert error.value.status_code == status
    assert [request.method for request in fixture.requests] == ["POST", "GET"]


@pytest.mark.parametrize("kind", ["duplicate", "backward", "different-id", "schema"])
def test_bad_replay_cannot_be_hidden_as_transport_recovery(kind: str) -> None:
    prefix = event_fixture()[:6]
    suffix = copy.deepcopy(event_fixture()[6:])
    if kind == "different-id":
        suffix[-1]["response"]["id"] = "resp-other"
    elif kind == "schema":
        del suffix[0]["sequence_number"]
    else:
        suffix[0]["sequence_number"] = 5 if kind == "duplicate" else 4
    fixture = Fixture(prefix, suffix)
    # Deliberately violate the server cursor contract instead of filtering away
    # the malformed sequence in this adversarial fixture.
    original = fixture.respond

    def respond(request: httpx.Request) -> httpx.Response:
        if request.method == "GET" and request.url.params.get("stream") == "true":
            fixture.requests.append(request)
            return httpx.Response(200, headers={"content-type": "text/event-stream"}, content=event_wire(suffix))
        return original(request)

    with OpenAI(api_key="fixture-only", base_url="https://fixture.invalid/v1", max_retries=0,
                http_client=httpx.Client(transport=httpx.MockTransport(respond))) as client:
        stream = client.responses.create(model="agent-fixture", input="One execution", stream=True)
        # On Python 3.14 the pinned SDK can raise its discriminator-cache
        # AttributeError before yielding malformed fields. Python 3.13 may reach
        # strict validation instead. Neither path may become a GET retry.
        expected = (AssertionError, ValueError, AttributeError) if kind == "schema" else AssertionError
        with pytest.raises(expected):
            observe_response(client, stream, "resp-fixture-1", backoff_seconds=0)
    assert [request.method for request in fixture.requests] == ["POST", "GET"]


@pytest.mark.parametrize("status", ["failed", "incomplete"])
def test_domain_terminal_is_not_retried_or_converted_to_completed(status: str) -> None:
    events = event_fixture()[:2]
    value = response_fixture(status=status)
    if status == "failed":
        value["error"] = {"code": "server_error", "message": "Unknown external result"}
        value["agents_error_code"] = "provider_outcome_unknown"
    else:
        value["incomplete_details"] = {"reason": "max_output_tokens"}
    events.append({"type": f"response.{status}", "sequence_number": 2, "response": value})
    fixture = Fixture(events)
    with fixture.client() as client:
        stream = client.responses.create(model="agent-fixture", input="One execution", stream=True)
        with pytest.raises(AssertionError):
            observe_response(client, stream, "resp-fixture-1", backoff_seconds=0)
    assert [request.method for request in fixture.requests] == ["POST"]


def test_cancel_error_is_not_a_transport_retry() -> None:
    events = event_fixture()[:1] + [{"type": "error", "sequence_number": 1, "code": "response_cancelled",
                                    "message": "Cancelled", "param": None}]
    fixture = Fixture(events)
    with fixture.client() as client:
        stream = client.responses.create(model="agent-fixture", input="One execution", stream=True)
        with pytest.raises((APIError, AssertionError)):
            observe_response(client, stream, "resp-fixture-1", backoff_seconds=0)
    assert [request.method for request in fixture.requests] == ["POST"]


def test_empty_replays_exhaust_only_the_explicit_get_budget() -> None:
    fixture = Fixture([], [])
    with fixture.client() as client:
        stream = client.responses.create(model="agent-fixture", input="One execution", stream=True)
        with pytest.raises(RuntimeError, match="budget"):
            observe_response(client, stream, "resp-fixture-1", max_resumptions=3, backoff_seconds=0)
    assert [request.method for request in fixture.requests] == ["POST", "GET", "GET", "GET"]


def test_caller_interrupt_is_not_caught_or_resumed() -> None:
    fixture = Fixture(event_fixture()[:1], initial_error=KeyboardInterrupt())
    with fixture.client() as client:
        stream = client.responses.create(model="agent-fixture", input="One execution", stream=True)
        with pytest.raises(KeyboardInterrupt):
            observe_response(client, stream, "resp-fixture-1", backoff_seconds=0)
    assert [request.method for request in fixture.requests] == ["POST"]


def test_canonical_snapshot_cannot_replace_or_disagree_with_the_terminal_event() -> None:
    canonical = response_fixture(text="Not the streamed answer")
    fixture = Fixture(event_fixture()[:6], canonical=canonical)
    with fixture.client() as client:
        stream = client.responses.create(model="agent-fixture", input="One execution", stream=True)
        with pytest.raises(AssertionError, match="canonical"):
            observe_response(client, stream, "resp-fixture-1", backoff_seconds=0)
    assert [request.method for request in fixture.requests] == ["POST", "GET", "GET"]


def test_failed_get_connection_consumes_the_same_budget_and_checkpoint() -> None:
    fixture = Fixture(event_fixture()[:6])
    original = fixture.respond
    openings = 0

    def respond(request: httpx.Request) -> httpx.Response:
        nonlocal openings
        if request.method == "GET" and request.url.params.get("stream") == "true":
            openings += 1
            if openings == 1:
                fixture.requests.append(request)
                raise httpx.ConnectError("Synthetic failed read-only connection", request=request)
        return original(request)

    with OpenAI(api_key="fixture-only", base_url="https://fixture.invalid/v1", max_retries=0,
                http_client=httpx.Client(transport=httpx.MockTransport(respond))) as client:
        stream = client.responses.create(model="agent-fixture", input="One execution", stream=True)
        result = observe_response(client, stream, "resp-fixture-1", backoff_seconds=0)
    assert result.resumptions == 2
    assert [request.method for request in fixture.requests] == ["POST", "GET", "GET", "GET"]
    assert [request.url.params.get("starting_after") for request in fixture.requests[1:3]] == ["5", "5"]


def test_zero_budget_does_not_open_a_replacement_observation() -> None:
    fixture = Fixture(event_fixture()[:1])
    with fixture.client() as client:
        stream = client.responses.create(model="agent-fixture", input="One execution", stream=True)
        with pytest.raises(RuntimeError, match="budget"):
            observe_response(client, stream, "resp-fixture-1", max_resumptions=0, backoff_seconds=0)
    assert fixture.initial.closed
    assert [request.method for request in fixture.requests] == ["POST"]


@pytest.mark.parametrize("status", ["failed", "incomplete"])
def test_failed_replayed_terminal_stops_without_another_resume(status: str) -> None:
    events = event_fixture()[:2]
    value = response_fixture(status=status)
    if status == "failed":
        value["error"] = {"code": "server_error", "message": "Unknown external result"}
        value["agents_error_code"] = "provider_outcome_unknown"
    else:
        value["incomplete_details"] = {"reason": "max_output_tokens"}
    events.append({"type": f"response.{status}", "sequence_number": 2, "response": value})
    fixture = Fixture(events[:1], events)
    with fixture.client() as client:
        stream = client.responses.create(model="agent-fixture", input="One execution", stream=True)
        with pytest.raises(AssertionError):
            observe_response(client, stream, "resp-fixture-1", backoff_seconds=0)
    assert [request.method for request in fixture.requests] == ["POST", "GET"]


def test_first_wrong_response_identity_is_not_retried() -> None:
    events = copy.deepcopy(event_fixture()[:1])
    events[0]["response"]["id"] = "resp-other"
    fixture = Fixture(events)
    with fixture.client() as client:
        stream = client.responses.create(model="agent-fixture", input="One execution", stream=True)
        with pytest.raises(AssertionError, match="another response"):
            observe_response(client, stream, "resp-fixture-1", backoff_seconds=0)
    assert [request.method for request in fixture.requests] == ["POST"]


def test_durable_sequence_fence_gap_does_not_require_contiguous_numbers() -> None:
    events = copy.deepcopy(event_fixture())
    for index, event in enumerate(events[6:]):
        event["sequence_number"] = 65536 + index
    fixture = Fixture(events[:6], events)
    with fixture.client() as client:
        stream = client.responses.create(model="agent-fixture", input="One execution", stream=True)
        result = observe_response(client, stream, "resp-fixture-1", backoff_seconds=0)
    assert result.response.output_text == "Hello, 雪."
    assert [event.sequence_number for event in result.events] == [event["sequence_number"] for event in events]
    assert result.resumptions == 1
    assert fixture.requests[1].url.params["starting_after"] == "5"


def test_missing_response_identity_cannot_trigger_any_replay() -> None:
    fixture = Fixture([])
    with fixture.client() as client:
        stream = client.responses.create(model="agent-fixture", input="One execution", stream=True)
        with pytest.raises(ValueError, match="accepted response ID"):
            observe_response(client, stream, "", backoff_seconds=0)
    assert fixture.initial.closed
    assert [request.method for request in fixture.requests] == ["POST"]
