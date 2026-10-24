"""Real deployment journeys. No fixture creates an authentication bypass."""
import concurrent.futures
import json
import os
import time
import uuid
from typing import Any
from collections.abc import Iterator
from urllib.parse import urlsplit

import pytest
from openai import OpenAI

from conftest import Http, cleanup_deployed_agent, correlation_headers
from stream_usage import reported_stream_usage


@pytest.fixture
def managed_agent(deployed):
    agent = deployed.json("POST", "/v1/agents", {
        "name": "acceptance-" + uuid.uuid4().hex[:12],
        "instructions": "Use the time tool whenever the user asks for the current time. Do not guess.",
        "tools": ["time"], "compaction_percent": 73,
    }, expected=201)
    sessions = []
    try:
        yield agent, sessions
    finally:
        failures = cleanup_deployed_agent(deployed, agent["id"], sessions)
        if failures:
            pytest.fail("Fixture cleanup incomplete: " + json.dumps(failures, sort_keys=True), pytrace=False)


@pytest.fixture
def inference(deployed: Http) -> Iterator[OpenAI]:
    if os.getenv("MAGENTS_TEST_INFERENCE") != "1":
        pytest.skip("real billed model calls require MAGENTS_TEST_INFERENCE=1 and funded provider configuration")
    with OpenAI(api_key=deployed.token, base_url=deployed.origin + "/v1", timeout=180,
                max_retries=0, default_headers=correlation_headers() or None) as client:
        yield client


def test_authentication_has_no_anonymous_bypass(deployed):
    assert deployed.request("GET", "/v1/agents", authenticated=False)[0] == 401
    assert deployed.request("GET", "/v1/agents", headers={"Authorization": "Bearer invalid-test-credential"})[0] == 401


@pytest.mark.parametrize("percent", [0, 100, -1, 256, 0.73, "73%"])
def test_compaction_requires_a_percentage(deployed, percent):
    assert deployed.request("POST", "/v1/agents", {"name": "invalid-percentage", "compaction_percent": percent})[0] == 400


def test_configuration_is_pinned_per_session(deployed, managed_agent):
    agent, sessions = managed_agent
    session = deployed.json("POST", f"/v1/agents/{agent['id']}/sessions", {}, expected=201)
    sessions.append(session["id"])
    pinned = session["pinned"]
    assert pinned["instructions"] == agent["instructions"]
    assert pinned["compaction_percent"] == 73
    assert any(tool["name"] == "time" and tool["execution"] == "harness" for tool in pinned["tools"])
    deployed.json("PUT", "/v1/agents/" + agent["id"], {
        "name": agent["name"], "model": agent["model"],
        "instructions": "Replacement instructions for future sessions only.",
        "tools": ["time"], "compaction_percent": 61,
    })
    assert deployed.json("GET", "/v1/sessions/" + session["id"])["pinned"] == pinned
    future = deployed.json("POST", f"/v1/agents/{agent['id']}/sessions", {}, expected=201)
    sessions.append(future["id"])
    assert future["pinned"]["instructions"] != pinned["instructions"]
    assert future["pinned"]["compaction_percent"] == 61


def test_openai_sdk_time_tool_and_streamed_continuation(deployed, inference, managed_agent):
    agent, sessions = managed_agent
    raw = inference.chat.completions.with_raw_response.create(
        model=agent["id"], messages=[{"role": "user", "content": "Call the time tool now and tell me its UTC time."}],
    )
    session_id = raw.headers.get("x-agents-session")
    assert session_id, "session identity must be recoverable using the standard SDK raw response"
    sessions.append(session_id)
    answer = raw.parse()
    assert answer.choices[0].message.content
    status = deployed.json("GET", "/v1/sessions/" + session_id)
    assert any(call["function"]["name"] == "time" for message in status["messages"] for call in message.get("tool_calls", []))
    assert any(message["role"] == "tool" for message in status["messages"])
    revision = status["input_revision"]
    with inference.chat.completions.with_streaming_response.create(
        model=agent["id"], messages=[{"role": "user", "content": "Now reply with a short acknowledgement."}],
        stream=True, extra_headers={"x-agents-session": session_id},
    ) as response:
        assert response.headers["x-agents-session"] == session_id
        chunks = list(response.parse())
    assert any(chunk.choices and chunk.choices[0].delta.content for chunk in chunks)
    assert any(chunk.choices and chunk.choices[0].finish_reason for chunk in chunks)
    after = deployed.json("GET", "/v1/sessions/" + session_id)
    assert after["input_revision"] > revision
    assert after["pinned"] == status["pinned"]


def test_second_account_cannot_access_agents_or_sessions(deployed, managed_agent):
    token = os.getenv("MAGENTS_TEST_OTHER_API_KEY")
    if not token:
        pytest.skip("cross-account isolation needs MAGENTS_TEST_OTHER_API_KEY belonging to a different account")
    other = Http(deployed.origin, token)
    # Verify credential validity before treating any 404 as evidence of isolation.
    other.json("GET", "/v1/agents")
    agent, sessions = managed_agent
    session = deployed.json("POST", f"/v1/agents/{agent['id']}/sessions", {}, expected=201)
    sessions.append(session["id"])
    assert other.request("GET", "/v1/agents/" + agent["id"])[0] == 404
    assert other.request("GET", "/v1/sessions/" + session["id"])[0] == 404
    assert other.request("POST", f"/v1/sessions/{session['id']}/steer", {"content": "intrusion"})[0] == 404


def test_busy_session_accepts_steering(deployed, inference, managed_agent):
    if os.getenv("MAGENTS_TEST_WORKSPACE") != "1":
        pytest.skip("busy steering requires MAGENTS_TEST_WORKSPACE=1 and deployed container capability")
    agent, sessions = managed_agent
    deployed.json("PUT", "/v1/agents/" + agent["id"], {
        "name": agent["name"], "model": agent["model"], "workspace": True,
        "instructions": "Use bash exactly as requested. After each command, check and follow appended user messages.",
        "tools": ["time", "bash"], "compaction_percent": 73,
    })
    session = deployed.json("POST", f"/v1/agents/{agent['id']}/sessions", {}, expected=201)
    session_id = session["id"]
    sessions.append(session_id)
    marker = "steering-" + uuid.uuid4().hex
    with concurrent.futures.ThreadPoolExecutor() as pool:
        future = pool.submit(inference.chat.completions.create, model=agent["id"],
                             messages=[{"role": "user", "content": "Run bash command `sleep 12` before replying."}],
                             extra_headers={"x-agents-session": session_id})
        for _ in range(200):
            state = deployed.json("GET", "/v1/sessions/" + session_id)
            if state["status"] == "running":
                break
            if future.done():
                pytest.fail("run completed before busy steering could be exercised")
            time.sleep(0.1)
        else:
            pytest.fail("session never became running")
        accepted = deployed.json("POST", f"/v1/sessions/{session_id}/steer", {"content": f"Include {marker} in your final answer."})
        assert accepted["queued"] is True
        assert accepted["input_revision"] > state["input_revision"]
        assert deployed.request("DELETE", "/v1/sessions/" + session_id)[0] == 409
        answer = future.result(timeout=180)
    assert marker in answer.choices[0].message.content
    final = deployed.json("GET", "/v1/sessions/" + session_id)
    assert final["status"] == "idle"
    assert any(call["function"]["name"] == "bash" for message in final["messages"] for call in message.get("tool_calls", []))


def test_workspace_archive_release_resume_restore(deployed, inference, managed_agent):
    if os.getenv("MAGENTS_TEST_WORKSPACE") != "1":
        pytest.skip("workspace restoration requires MAGENTS_TEST_WORKSPACE=1 and deployed R2/container capability")
    agent, sessions = managed_agent
    deployed.json("PUT", "/v1/agents/" + agent["id"], {
        "name": agent["name"], "model": agent["model"], "workspace": True,
        "instructions": "Run the exact bash command requested. Report stdout faithfully. Never guess file contents.",
        "tools": ["time", "bash"], "compaction_percent": 73,
    })
    session = deployed.json("POST", f"/v1/agents/{agent['id']}/sessions", {}, expected=201)
    session_id = session["id"]
    sessions.append(session_id)
    marker = uuid.uuid4().hex
    inference.chat.completions.create(
        model=agent["id"], messages=[{"role": "user", "content": f"Run bash: mkdir -p acceptance; printf {marker} > acceptance/state"}],
        extra_headers={"x-agents-session": session_id},
    )
    deployed.json("DELETE", "/v1/sessions/" + session_id)
    archived = deployed.json("GET", "/v1/sessions/" + session_id)
    assert archived["status"] == "closed"
    snapshot = archived["workspace_snapshot"]
    assert snapshot and snapshot["generation"] >= 1 and snapshot["archive_bytes"] > 0
    assert len(snapshot["sha256"]) == 64
    deployed.json("POST", f"/v1/sessions/{session_id}/resume", {})
    resumed = deployed.json("GET", "/v1/sessions/" + session_id)
    assert resumed["status"] == "idle"
    assert resumed["pinned"] == archived["pinned"]
    before = len(resumed["messages"])
    inference.chat.completions.create(
        model=agent["id"], messages=[{"role": "user", "content": "Use bash to run `cat acceptance/state` and report its stdout."}],
        extra_headers={"x-agents-session": session_id},
    )
    restored = deployed.json("GET", "/v1/sessions/" + session_id)
    # Check a new actual tool result, not the model recalling the old write prompt.
    results = [message for message in restored["messages"][before:] if message["role"] == "tool"]
    assert any(marker in str(message["content"]) for message in results)


@pytest.fixture
def compaction_ready(deployed):
    if os.getenv("MAGENTS_TEST_COMPACTION") != "1":
        pytest.skip("bounded real-provider compaction requires MAGENTS_TEST_COMPACTION=1 with a dedicated funded staging key")
    assert urlsplit(deployed.origin).hostname == "agents-staging.moesegfault.dev", "billable compaction acceptance is staging-only"


def assert_complete_tool_groups(messages):
    pending = set()
    seen = set()
    for message in messages:
        if message["role"] == "tool":
            call_id = message["tool_call_id"]
            assert call_id in pending, "tool result must match an unresolved preceding call"
            pending.remove(call_id)
            continue
        assert not pending, "a tool-call group must complete before the next conversational message"
        calls = message.get("tool_calls", [])
        if calls:
            assert message["role"] == "assistant"
            for call in calls:
                assert call["id"] and call["id"] not in seen, "tool call IDs must be unique"
                seen.add(call["id"])
                pending.add(call["id"])
    assert not pending, "completed source journal must not leave orphaned tool calls"


def test_real_provider_compaction_preserves_pin_and_source_journal(
    deployed: Http,
    inference: OpenAI,
    compaction_ready: None,
    managed_agent: tuple[dict[str, Any], list[str]],
) -> None:
    agent, sessions = managed_agent
    # Use the normal registered Flash model and a percentage, never client-side
    # context-window overrides or a tokenizer approximation as the test oracle.
    assert "flash" in agent["model"]["model"].lower()
    assert agent["model"]["context_window"] in (1_000_000, 1_048_576)
    deployed.json("PUT", "/v1/agents/" + agent["id"], {
        "name": agent["name"], "model": agent["model"], "tools": ["time"],
        "instructions": "Reply very briefly. For a source payload, acknowledge with ACK and do not repeat it. "
                        "For a current-time request, call the time tool exactly once and report the result briefly.",
        "compaction_percent": 1,
    })
    session = deployed.json("POST", f"/v1/agents/{agent['id']}/sessions", {}, expected=201)
    session_id = session["id"]
    sessions.append(session_id)
    pinned = session["pinned"]
    marker = uuid.uuid4().hex
    payload = (f"Source payload starts {marker}. Preserve this as conversation data; reply ACK only.\n"
               + " alpha beta gamma delta" * 3500
               + f"\nSource payload ends {marker}.")
    # Long prefill/compaction follows the documented streaming observer contract;
    # disconnecting a nonstream observer does not cancel its durable execution.
    with inference.chat.completions.create(
        model=agent["id"], messages=[{"role": "user", "content": payload}],
        extra_headers={"x-agents-session": session_id}, stream=True,
    ) as initial:
        initial_usage = reported_stream_usage(initial)
    # Provider-reported input counts are authoritative. The fixed corpus bounds
    # this journey to roughly 12–20k tokens rather than a million-token probe.
    assert 10_500 <= initial_usage.prompt_tokens <= 20_000, "real corpus must cross the 1% threshold without an oversized billable probe"
    before = deployed.json("GET", "/v1/sessions/" + session_id)
    assert before["compact_count"] == 0
    assert before["pinned"] == pinned
    assert any(message["role"] == "user" and message["content"] == payload for message in before["messages"])
    with inference.chat.completions.create(
        model=agent["id"], messages=[{"role": "user", "content": "Call the time tool now and briefly report the current UTC time."}],
        extra_headers={"x-agents-session": session_id}, stream=True,
    ) as continued:
        reported_stream_usage(continued)
    after = deployed.json("GET", "/v1/sessions/" + session_id)
    assert after["compact_count"] >= 1
    assert after["pinned"] == pinned
    assert after["messages"][:len(before["messages"])] == before["messages"]
    assert any(message["role"] == "user" and message["content"] == payload for message in after["messages"])
    assert any(call["function"]["name"] == "time" for message in after["messages"] for call in message.get("tool_calls", []))
    assert_complete_tool_groups(after["messages"])


def test_free_configuration_quota_is_atomic(deployed):
    if os.getenv("MAGENTS_TEST_QUOTA") != "1":
        pytest.skip("quota saturation is opt-in: MAGENTS_TEST_QUOTA=1 with a dedicated empty Free account")
    assert deployed.json("GET", "/v1/agents")["data"] == [], "quota test requires an empty dedicated Free account"
    created = []
    try:
        with concurrent.futures.ThreadPoolExecutor(max_workers=3) as pool:
            responses = list(pool.map(lambda _: deployed.request("POST", "/v1/agents", {"name": "quota-" + uuid.uuid4().hex}), range(3)))
        import json
        created = [json.loads(body)["id"] for code, _, body in responses if code == 201]
        assert sorted(code for code, _, _ in responses) == [201, 201, 429]
        assert len(deployed.json("GET", "/v1/agents")["data"]) == 2
    finally:
        for agent_id in created:
            deployed.json("DELETE", "/v1/agents/" + agent_id)
