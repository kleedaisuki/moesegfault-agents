"""Real Responses SDK user journeys; no execution request is automatically replayed."""

import json
import os
import time
import uuid
from collections.abc import Iterator
from typing import Any

import pytest
from openai import APIStatusError, OpenAI
from openai.types.responses import Response

from conftest import Http, cleanup_deployed_agent, correlation_headers
from response_checks import checked_response, checked_stream
from paid_route_policy import direct_deepseek_model


@pytest.fixture
def responses_sdk(deployed: Http) -> Iterator[OpenAI]:
    if os.getenv("MAGENTS_TEST_INFERENCE") != "1":
        pytest.skip("real Responses calls require MAGENTS_TEST_INFERENCE=1")
    with OpenAI(api_key=deployed.token, base_url=deployed.origin + "/v1", timeout=240,
                max_retries=0, default_headers=correlation_headers()) as client:
        yield client


@pytest.fixture
def response_agent(
    deployed: Http,
    responses_sdk: OpenAI,
) -> Iterator[tuple[dict[str, Any], list[str]]]:
    agent = deployed.json("POST", "/v1/agents", {
        "name": "responses-acceptance-" + uuid.uuid4().hex[:12],
        "model": direct_deepseek_model(deployed),
        "instructions": "Answer briefly. Use the time tool when asked for the current time. Never guess tool results.",
        "tools": ["time"], "workspace": False, "compaction_percent": 73,
    }, expected=201)
    sessions: list[str] = []
    try:
        session = deployed.json("POST", f"/v1/agents/{agent['id']}/sessions", {}, expected=201)
        sessions.append(session["id"])
        pin = deployed.json("GET", "/v1/sessions/" + session["id"])["pinned"]
        assert pin["model"]["provider"] == "deepseek", "paid acceptance must use direct DeepSeek"
        assert pin["model"] == agent["model"]
        assert not pin["model"].get("credential_id"), "platform acceptance must not use an unverified BYOK origin"
        assert pin == session["pinned"]
        agent["_acceptance_session"] = session["id"]
        yield agent, sessions
    finally:
        failures = cleanup_deployed_agent(deployed, agent["id"], sessions)
        if failures:
            pytest.fail("Response fixture cleanup incomplete: " + json.dumps(failures, sort_keys=True), pytrace=False)


def initial_session_headers(agent: dict[str, Any], **headers: str) -> dict[str, str]:
    session_id = agent.get("_acceptance_session")
    assert isinstance(session_id, str) and session_id, "a verified pin-only session is required before paid execution"
    return {"X-Agents-Session": session_id, **headers}


def track_session(raw: Any, sessions: list[str]) -> str:
    session_id = raw.headers.get("x-agents-session")
    assert session_id, "the accepted execution must expose its recovery session identity"
    if session_id not in sessions:
        sessions.append(session_id)
    return session_id


def wait_terminal(client: OpenAI, response_id: str, *, timeout: float = 240) -> Response:
    deadline = time.monotonic() + timeout
    while True:
        response = checked_response(client.responses.retrieve(response_id))
        if response.status not in ("queued", "in_progress"):
            return response
        remaining = deadline - time.monotonic()
        assert remaining > 0, "accepted response did not reach a terminal state before its observation deadline"
        time.sleep(min(1, remaining))


def test_responses_sdk_lifecycle_stream_recovery_and_historical_fork(
    deployed: Http,
    responses_sdk: OpenAI,
    response_agent: tuple[dict[str, Any], list[str]],
) -> None:
    agent, sessions = response_agent
    request_id = "responses-" + uuid.uuid4().hex
    custom = "Keep the final answer shorter than three sentences."
    initial_input = "Call the time tool and briefly report the current UTC time."
    raw = responses_sdk.responses.with_raw_response.create(
        model=agent["id"], input=initial_input, instructions=custom,
        metadata={"journey": request_id}, extra_headers=initial_session_headers(agent, **{"Idempotency-Key": request_id}),
    )
    session_id = track_session(raw, sessions)
    first = checked_response(raw.parse())
    assert first.status == "completed" and first.output_text
    assert first.model == agent["id"] and first.instructions == custom
    assert first.metadata == {"journey": request_id}
    first_snapshot = first.model_dump()
    state = deployed.json("GET", "/v1/sessions/" + session_id)
    assert any(call["function"]["name"] == "time" for message in state["messages"] for call in message.get("tool_calls", []))
    revision = state["input_revision"]

    replay = checked_response(responses_sdk.responses.create(
        model=agent["id"], input=initial_input, instructions=custom,
        metadata={"journey": request_id}, extra_headers=initial_session_headers(agent, **{"Idempotency-Key": request_id}),
    ))
    assert replay.model_dump() == first_snapshot
    assert deployed.json("GET", "/v1/sessions/" + session_id)["input_revision"] == revision
    with pytest.raises(APIStatusError) as conflict:
        responses_sdk.responses.create(model=agent["id"], input="Different request",
                                       extra_headers=initial_session_headers(agent, **{"Idempotency-Key": request_id}))
    assert conflict.value.status_code == 409

    input_items = responses_sdk.responses.input_items.list(first.id, limit=1, order="asc")
    assert input_items.data, "the response must retain its own input-item boundary"
    assert any(initial_input in str(item.model_dump()) for item in input_items)

    second_input = "Acknowledge the previous result briefly. Branch exclusion marker: " + uuid.uuid4().hex
    with responses_sdk.responses.create(
        model=agent["id"], previous_response_id=first.id,
        input=second_input, stream=True,
    ) as stream:
        second, events = checked_stream(stream)
    assert second.id != first.id and second.previous_response_id == first.id
    assert second.instructions is None, "top-level request instructions must not implicitly inherit"
    assert checked_response(responses_sdk.responses.retrieve(first.id)).model_dump() == first_snapshot

    cursor = events[len(events) // 2].sequence_number
    with responses_sdk.responses.retrieve(second.id, stream=True, starting_after=cursor) as recovered:
        repeated, suffix = checked_stream(recovered, require_created=False)
    assert repeated.model_dump() == second.model_dump()
    assert all(event.sequence_number > cursor for event in suffix)
    assert [event.model_dump() for event in suffix] == [
        event.model_dump() for event in events if event.sequence_number > cursor
    ], "recovery must replay the same events, not infer a new response"
    source_before = deployed.json("GET", "/v1/sessions/" + session_id)
    fork_input = "Acknowledge only this independent branch. " + uuid.uuid4().hex
    fork_key = "fork-" + uuid.uuid4().hex
    fork_raw = responses_sdk.responses.with_raw_response.create(
        model=agent["id"], previous_response_id=first.id, input=fork_input,
        extra_headers={"Idempotency-Key": fork_key},
    )
    fork_session = track_session(fork_raw, sessions)
    fork = checked_response(fork_raw.parse())
    assert fork.status == "completed" and fork.output_text
    assert fork.id not in (first.id, second.id) and fork.previous_response_id == first.id
    assert fork.instructions is None
    assert fork_session != session_id, "historical forks require independent mutable sessions"
    branch = deployed.json("GET", "/v1/sessions/" + fork_session)
    assert branch["pinned"] == state["pinned"]
    assert branch["messages"][:len(state["messages"])] == state["messages"]
    assert second_input not in json.dumps(branch["messages"]), "a historical branch must not inherit later source input"
    assert fork_input in json.dumps(branch["messages"])
    fork_snapshot = fork.model_dump()
    repeated_fork = checked_response(responses_sdk.responses.create(
        model=agent["id"], previous_response_id=first.id, input=fork_input,
        extra_headers={"Idempotency-Key": fork_key},
    ))
    assert repeated_fork.model_dump() == fork_snapshot
    assert deployed.json("GET", "/v1/sessions/" + fork_session)["input_revision"] == branch["input_revision"]
    source_after = deployed.json("GET", "/v1/sessions/" + session_id)
    for field in ("input_revision", "last_input_at_ms", "pinned", "messages"):
        assert source_after[field] == source_before[field], "forking must not rewind or mutate the source"
    assert checked_response(responses_sdk.responses.retrieve(first.id)).model_dump() == first_snapshot
    assert checked_response(responses_sdk.responses.retrieve(second.id)).model_dump() == second.model_dump()


def test_responses_local_function_roundtrip_rejects_unsolicited_results(
    deployed: Http,
    responses_sdk: OpenAI,
    response_agent: tuple[dict[str, Any], list[str]],
) -> None:
    agent, sessions = response_agent
    argument = "arg-" + uuid.uuid4().hex
    result = "local-result-" + uuid.uuid4().hex
    tool: dict[str, Any] = {
        "type": "function", "name": "local_echo", "description": "Read a value from the caller's local computer.",
        "parameters": {"type": "object", "properties": {"value": {"type": "string"}},
                       "required": ["value"], "additionalProperties": False}, "strict": True,
    }
    raw = responses_sdk.responses.with_raw_response.create(
        model=agent["id"], input=f"Call local_echo with value {argument}. Do not invent the function result.",
        tools=[tool], tool_choice={"type": "function", "name": "local_echo"}, parallel_tool_calls=False,
        extra_headers=initial_session_headers(agent),
    )
    session_id = track_session(raw, sessions)
    handoff = checked_response(raw.parse())
    assert handoff.status == "completed"
    calls = [item for item in handoff.output if item.type == "function_call"]
    assert len(calls) == 1 and calls[0].name == "local_echo"
    call = calls[0]
    assert json.loads(call.arguments) == {"value": argument}
    waiting = deployed.json("GET", "/v1/sessions/" + session_id)
    assert waiting["status"] == "idle" and waiting.get("run_id") is None
    revision = waiting["input_revision"]
    with pytest.raises(APIStatusError) as unknown:
        responses_sdk.responses.create(model=agent["id"], previous_response_id=handoff.id,
                                       input=[{"type": "function_call_output", "call_id": "not-the-requested-call", "output": result}])
    assert unknown.value.status_code == 400
    assert deployed.json("GET", "/v1/sessions/" + session_id)["input_revision"] == revision

    # A head continuation should readmit an archived session without asking the
    # ordinary SDK caller to know a second, platform-specific resume protocol.
    deployed.json("DELETE", "/v1/sessions/" + session_id)
    assert deployed.json("GET", "/v1/sessions/" + session_id)["status"] == "closed"
    completed = checked_response(responses_sdk.responses.create(
        model=agent["id"], previous_response_id=handoff.id, tools=[tool],
        input=[{"type": "function_call_output", "call_id": call.call_id, "output": result}],
        instructions="Report the exact local function result briefly. Do not call more tools.", tool_choice="none",
    ))
    assert completed.status == "completed" and result in completed.output_text
    after = deployed.json("GET", "/v1/sessions/" + session_id)
    assert after["pinned"] == waiting["pinned"]
    assert sum(message.get("tool_call_id") == call.call_id for message in after["messages"]) == 1
    assert checked_response(responses_sdk.responses.retrieve(handoff.id)).model_dump() == handoff.model_dump()


def test_responses_background_and_owner_isolation(
    deployed: Http,
    responses_sdk: OpenAI,
    response_agent: tuple[dict[str, Any], list[str]],
) -> None:
    other_token = os.getenv("MAGENTS_TEST_OTHER_API_KEY")
    if not other_token:
        pytest.skip("response ownership requires MAGENTS_TEST_OTHER_API_KEY")
    other = Http(deployed.origin, other_token)
    other.json("GET", "/v1/agents")
    agent, sessions = response_agent
    raw = responses_sdk.responses.with_raw_response.create(
        model=agent["id"], input="Reply with a brief acknowledgement.", background=True,
        extra_headers=initial_session_headers(agent),
    )
    track_session(raw, sessions)
    accepted = checked_response(raw.parse())
    assert accepted.background is True and accepted.status in {"queued", "in_progress", "completed"}
    final = wait_terminal(responses_sdk, accepted.id)
    assert final.status == "completed" and final.output_text
    for method, suffix, data in [
        ("GET", "", None), ("GET", "/input_items", None), ("GET", "?stream=true", None),
        ("POST", "/cancel", {}), ("DELETE", "", None),
    ]:
        assert other.request(method, "/v1/responses/" + accepted.id + suffix, data)[0] == 404
    assert checked_response(responses_sdk.responses.retrieve(accepted.id)).model_dump() == final.model_dump()
    deleted = responses_sdk.responses.with_raw_response.delete(accepted.id)
    assert deleted.status_code == 200
    assert deleted.http_response.json() == {
        "id": accepted.id, "object": "response.deleted", "deleted": True,
    }
    with pytest.raises(APIStatusError) as missing:
        responses_sdk.responses.retrieve(accepted.id)
    assert missing.value.status_code == 404


def test_responses_consumes_owned_uploaded_file(
    responses_sdk: OpenAI,
    response_agent: tuple[dict[str, Any], list[str]],
) -> None:
    agent, sessions = response_agent
    marker = "attachment-" + uuid.uuid4().hex
    uploaded = responses_sdk.files.create(
        file=("evidence.txt", f"The verification marker is {marker}.\n".encode(), "text/plain"), purpose="user_data",
    )
    try:
        raw = responses_sdk.responses.with_raw_response.create(model=agent["id"], input=[{
            "role": "user", "content": [
                {"type": "input_text", "text": "Read the attached file and report its exact verification marker."},
                {"type": "input_file", "file_id": uploaded.id},
            ],
        }], extra_headers=initial_session_headers(agent))
        track_session(raw, sessions)
        response = checked_response(raw.parse())
        assert response.status == "completed" and marker in response.output_text
        responses_sdk.files.delete(uploaded.id)
        uploaded = None
        assert checked_response(responses_sdk.responses.retrieve(response.id)).model_dump() == response.model_dump()
    finally:
        if uploaded is not None:
            responses_sdk.files.delete(uploaded.id)
