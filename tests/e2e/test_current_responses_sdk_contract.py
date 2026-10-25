"""Real current SDK client probes against isolated transports, not server evidence."""
from __future__ import annotations

import copy
import json
from threading import Thread
from typing import Any

import httpx2
from openai import OpenAI
import pytest
from websockets.sync.server import ServerConnection, serve

from current_responses_sdk import validate_wire


def response_fixture() -> dict[str, Any]:
    return {"id": "resp_fixture", "created_at": 1, "model": "agent_fixture", "object": "response", "status": "completed",
            "output": [{"id": "msg_fixture", "type": "message", "status": "completed", "role": "assistant", "phase": "final_answer",
                        "content": [{"type": "output_text", "text": "SDK fixture", "annotations": [], "logprobs": []}]}],
            "parallel_tool_calls": True, "tool_choice": "auto", "tools": [], "usage": usage_fixture()}


def usage_fixture() -> dict[str, Any]:
    return {"input_tokens": 2, "input_tokens_details": {"cached_tokens": 0, "cache_write_tokens": 0}, "output_tokens": 1,
            "output_tokens_details": {"reasoning_tokens": 0}, "total_tokens": 3}


def bundle_fixture() -> dict[str, Any]:
    response: dict[str, Any] = response_fixture()
    terminal: dict[str, Any] = {"type": "response.completed", "sequence_number": 1, "response": response}
    return {"response": response, "events": [terminal], "websocket_events": [{**terminal, "stream_id": "main"}],
            "compact": {"id": "cmp_fixture", "created_at": 1, "object": "response.compaction",
                        "output": [{"id": "cmp_item", "type": "compaction", "encrypted_content": "fixture-opaque"}], "usage": usage_fixture()},
            "count": {"object": "response.input_tokens", "input_tokens": 2},
            "conversation": {"id": "conv_fixture", "created_at": 1, "object": "conversation", "metadata": {}},
            "conversation_items": {"object": "list", "data": [{"id": "msg_user", "type": "message", "role": "user", "status": "completed",
                                                                   "content": [{"type": "input_text", "text": "fixture"}]}],
                                   "first_id": "msg_user", "last_id": "msg_user", "has_more": False}}


def test_current_sdk_all_surface_strict_wire_bundle() -> None:
    result: dict[str, Any] = validate_wire(bundle_fixture())
    assert result["passed"] and not result["live_execution_proven"]
    assert result["sdk"] == "3.37.0"


@pytest.mark.parametrize("missing", ["response", "events", "websocket_events", "compact", "count", "conversation", "conversation_items"])
def test_current_sdk_missing_surface_fails(missing: str) -> None:
    bundle: dict[str, Any] = bundle_fixture()
    del bundle[missing]
    with pytest.raises((KeyError, ValueError)):
        validate_wire(bundle)


def test_current_sdk_bad_wire_and_fake_terminal_fail() -> None:
    bundle: dict[str, Any] = bundle_fixture()
    bundle["count"]["object"] = "response"
    with pytest.raises(ValueError):
        validate_wire(bundle)
    bundle = bundle_fixture()
    bundle["events"][0]["response"] = {**response_fixture(), "id": "wrong"}
    with pytest.raises(ValueError, match="terminal_snapshot_differs"):
        validate_wire(bundle)
    bundle = bundle_fixture()
    bundle["events"] *= 2
    with pytest.raises(ValueError, match="response_sequence_not_strict"):
        validate_wire(bundle)


def test_current_sdk_create_retrieve_count_compact_and_conversations_http() -> None:
    observed: list[tuple[str, str]] = []
    bundle: dict[str, Any] = bundle_fixture()

    def handle(request: httpx2.Request) -> httpx2.Response:
        route: str = request.url.path
        observed.append((request.method, route))
        assert request.headers["authorization"] == "Bearer fixture-only"
        if route.endswith("/input_tokens"):
            assert json.loads(request.content)["previous_response_id"] == "resp_fixture"
            return httpx2.Response(200, json=bundle["count"])
        if route.endswith("/compact"):
            return httpx2.Response(200, json=bundle["compact"])
        if route == "/v1/responses":
            payload: dict[str, Any] = json.loads(request.content)
            assert payload["model"] == "agent_fixture"
            assert payload["text"]["verbosity"] == "low"
            assert payload["instructions"] == "request-only-fixture"
            return httpx2.Response(200, json=bundle["response"])
        if route.startswith("/v1/responses/"):
            if request.method == "DELETE":
                return httpx2.Response(200, json={"id": "resp_fixture", "object": "response.deleted", "deleted": True})
            return httpx2.Response(200, json=bundle["response"])
        if route.endswith("/items"):
            return httpx2.Response(200, json=bundle["conversation_items"])
        if request.method == "DELETE":
            return httpx2.Response(200, json={"id": "conv_fixture", "object": "conversation.deleted", "deleted": True})
        return httpx2.Response(200, json=bundle["conversation"])

    with OpenAI(api_key="fixture-only", base_url="https://fixture.invalid/v1", max_retries=0,
                _strict_response_validation=True, http_client=httpx2.Client(transport=httpx2.MockTransport(handle))) as client:
        response: Any = client.responses.create(model="agent_fixture", input="fixture", instructions="request-only-fixture", text={"verbosity": "low"})
        assert response.output_text == "SDK fixture"
        assert client.responses.retrieve(response.id).id == response.id
        assert client.responses.input_tokens.count(model="agent_fixture", previous_response_id=response.id).input_tokens == 2
        assert client.responses.compact(model="agent_fixture", input="fixture").object == "response.compaction"
        conversation: Any = client.conversations.create(metadata={"purpose": "fixture"})
        assert conversation.id == "conv_fixture"
        assert client.conversations.items.create(conversation.id, items=[{"role": "user", "content": "fixture"}]).data[0].id == "msg_user"
        assert [item.id for item in client.conversations.items.list(conversation.id)] == ["msg_user"]
        assert client.conversations.delete(conversation.id).deleted
        assert client.responses.delete(response.id) is None
    assert len(observed) == 9


def test_current_sdk_actual_local_websocket_named_lanes() -> None:
    observed: list[dict[str, Any]] = []
    failures: list[BaseException] = []

    def handler(connection: ServerConnection) -> None:
        try:
            assert connection.request is not None
            assert connection.request.path == "/v1/responses"
            assert connection.request.headers["Authorization"] == "Bearer fixture-only"
            for index in range(2):
                incoming: dict[str, Any] = json.loads(connection.recv(timeout=3))
                observed.append(incoming)
            for index, incoming in enumerate(reversed(observed)):
                response: dict[str, Any] = copy.deepcopy(response_fixture())
                response["id"] = "resp_" + incoming["stream_id"]
                connection.send(json.dumps({"type": "response.completed", "sequence_number": index,
                                            "stream_id": incoming["stream_id"], "response": response}))
        except BaseException as error:
            failures.append(error)

    with serve(handler, "127.0.0.1", 0) as server:
        port: int = server.socket.getsockname()[1]
        thread: Thread = Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            with OpenAI(api_key="fixture-only", base_url=f"http://127.0.0.1:{port}/v1", max_retries=0) as client:
                with client.responses.connect(max_retries=0) as connection:
                    connection.send({"type": "response.create", "model": "agent_fixture", "stream_id": "main", "input": "one"})
                    connection.send({"type": "response.create", "model": "agent_fixture", "stream_id": "fork", "previous_response_id": "resp_parent", "input": "two"})
                    events: list[Any] = [connection.recv(), connection.recv()]
                    assert [event.stream_id for event in events] == ["fork", "main"]
                    assert all(event.type == "response.completed" for event in events)
        finally:
            server.shutdown()
            thread.join(timeout=3)
    assert not failures
    assert observed[1]["previous_response_id"] == "resp_parent"
    assert all("stream" not in event and "background" not in event for event in observed)


def test_current_sdk_non_generative_stage_workflow(monkeypatch: pytest.MonkeyPatch) -> None:
    import current_responses_sdk as runner
    import paid_route_policy

    original_client: Any = httpx2.Client
    sdk_client: Any = OpenAI
    bundle: dict[str, Any] = bundle_fixture()
    model: dict[str, Any] = {"provider": "deepseek", "model": "deepseek-flash", "context_window": 10000, "max_output_tokens": 1000}
    session: dict[str, Any] = {"id": "session_fixture", "agent_id": "agent_fixture", "pinned": {"model": model}}
    requests: list[tuple[str, str]] = []
    state: dict[str, Any] = {"items": False, "metadata": {"purpose": "current-sdk-no-inference"}}
    source: str = "a" * 40

    def handle(request: httpx2.Request) -> httpx2.Response:
        route: str = request.url.path
        requests.append((request.method, route))
        assert not (request.method == "POST" and route in {"/v1/responses", "/v1/responses/compact"})
        if route == "/healthz":
            return httpx2.Response(200, json={"status": "ok", "build_id": source})
        if route.startswith("/v1/sessions/"):
            return httpx2.Response(200, json=session)
        if route == "/v1/models":
            return httpx2.Response(200, json={"available_platform_models": [model]})
        if route.endswith("/input_tokens"):
            return httpx2.Response(200, json=bundle["count"])
        if route.endswith("/input_items"):
            return httpx2.Response(200, json=bundle["conversation_items"])
        if route.startswith("/v1/responses/"):
            if request.url.params.get("stream") == "true":
                wire: str = "".join("data: " + json.dumps(event) + "\n\n" for event in bundle["events"])
                return httpx2.Response(200, content=wire, headers={"Content-Type": "text/event-stream"})
            return httpx2.Response(200, json=bundle["response"], headers={"x-agents-session": "session_fixture"})
        if route.endswith("/items/msg_user"):
            if request.method == "DELETE":
                state["items"] = False
                return httpx2.Response(200, json=bundle["conversation"])
            return httpx2.Response(200, json=bundle["conversation_items"]["data"][0])
        if route.endswith("/items"):
            if request.method == "POST":
                state["items"] = True
            page: dict[str, Any] = copy.deepcopy(bundle["conversation_items"])
            if not state["items"]:
                page["data"] = []
                page["first_id"] = page["last_id"] = ""
            return httpx2.Response(200, json=page)
        if request.method == "DELETE":
            return httpx2.Response(200, json={"id": "conv_fixture", "object": "conversation.deleted", "deleted": True})
        if route.endswith("/conv_fixture") and request.method == "POST":
            state["metadata"] = json.loads(request.content)["metadata"]
        return httpx2.Response(200, json={**bundle["conversation"], "metadata": state["metadata"]})

    def raw_factory(**kwargs: Any) -> httpx2.Client:
        return original_client(**kwargs, transport=httpx2.MockTransport(handle))

    def sdk_factory(**kwargs: Any) -> OpenAI:
        return sdk_client(**kwargs, http_client=original_client(transport=httpx2.MockTransport(handle)))

    monkeypatch.setenv("MAGENTS_TEST_API_KEY", "fixture-only")
    monkeypatch.setenv("MAGENTS_TEST_EXPECTED_BUILD", source)
    monkeypatch.setenv("MAGENTS_TEST_DIRECT_MODEL", "deepseek-flash")
    def origin_fixture(build: str) -> str:
        assert build == source
        return "https://api.deepseek.com"

    monkeypatch.setattr(paid_route_policy, "source_provider_origin", origin_fixture)
    monkeypatch.setattr(runner, "Client", raw_factory)
    monkeypatch.setattr(runner, "OpenAI", sdk_factory)
    result: dict[str, Any] = runner.inspect_staging(source, "session_fixture", "resp_fixture", "agent_fixture")
    assert result["passed"] and result["new_model_runs"] == 0 and not result["full_acceptance"]
    assert ("POST", "/v1/responses/input_tokens") in requests
    assert ("DELETE", "/v1/conversations/conv_fixture") in requests


def test_current_sdk_unknown_cache_details_are_not_fabricated() -> None:
    bundle: dict[str, Any] = bundle_fixture()
    del bundle["response"]["usage"]["input_tokens_details"]["cache_write_tokens"]
    with pytest.raises(ValueError, match="cache_write_tokens"):
        validate_wire(bundle)
    bundle["response"]["usage"] = None
    assert validate_wire(bundle)["passed"]
    del bundle["compact"]["usage"]["input_tokens_details"]["cache_write_tokens"]
    with pytest.raises(ValueError, match="cache_write_tokens"):
        validate_wire(bundle)
