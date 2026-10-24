"""Harness checks against canonical wire fixtures, not deployed-server evidence."""
import json
from typing import Any

import httpx
import pytest
from openai import OpenAI
from openai.types.chat import ChatCompletionChunk

from stream_usage import reported_stream_usage


def test_sdk_nonstream_session_header_contract() -> None:
    observed: list[httpx.Request] = []

    def respond(request: httpx.Request) -> httpx.Response:
        observed.append(request)
        return httpx.Response(200, headers={"x-agents-session": "session-1"}, json={
            "id": "run-1", "object": "chat.completion", "created": 1, "model": "agent-1",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
        })

    with OpenAI(api_key="fixture-only", base_url="https://fixture.invalid/v1", http_client=httpx.Client(transport=httpx.MockTransport(respond))) as client:
        raw = client.chat.completions.with_raw_response.create(model="agent-1", messages=[{"role": "user", "content": "hello"}])
        assert raw.headers["x-agents-session"] == "session-1"
        assert raw.parse().choices[0].message.content == "ok"
    assert observed[0].url.path == "/v1/chat/completions"
    assert json.loads(observed[0].content)["model"] == "agent-1"


def test_sdk_stream_session_header_contract() -> None:
    def respond(request: httpx.Request) -> httpx.Response:
        assert request.headers["x-agents-session"] == "session-1"
        assert json.loads(request.content)["stream"] is True
        base = {"id": "run-2", "object": "chat.completion.chunk", "created": 1, "model": "agent-1"}
        events = [
            {**base, "choices": [{"index": 0, "delta": {"role": "assistant", "content": "ok"}, "finish_reason": None}]},
            {**base, "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
        ]
        body = "".join("data: " + json.dumps(event) + "\n\n" for event in events) + "data: [DONE]\n\n"
        return httpx.Response(200, headers={"Content-Type": "text/event-stream", "x-agents-session": "session-1"}, content=body)

    with OpenAI(api_key="fixture-only", base_url="https://fixture.invalid/v1", http_client=httpx.Client(transport=httpx.MockTransport(respond))) as client:
        with client.chat.completions.with_streaming_response.create(model="agent-1", messages=[{"role": "user", "content": "hello"}], stream=True, extra_headers={"x-agents-session": "session-1"}) as response:
            assert response.headers["x-agents-session"] == "session-1"
            chunks = list(response.parse())
    assert chunks[0].choices[0].delta.content == "ok"
    assert chunks[-1].choices[0].finish_reason == "stop"


def chunk(*, finish: str | None = None, usage: bool = False, choices: bool = True) -> ChatCompletionChunk:
    value: dict[str, Any] = {
        "id": "stream-usage", "object": "chat.completion.chunk", "created": 1,
        "model": "agent", "choices": [{"index": 0, "delta": {}, "finish_reason": finish}] if choices else [],
    }
    if usage:
        value["usage"] = {"prompt_tokens": 14399, "completion_tokens": 3, "total_tokens": 14402}
    return ChatCompletionChunk.model_validate(value)


@pytest.mark.parametrize("separate_usage", [False, True])
def test_stream_usage_requires_normal_finish_and_authoritative_counters(separate_usage: bool) -> None:
    terminal = [chunk(finish="stop", usage=not separate_usage)]
    if separate_usage:
        terminal.append(chunk(usage=True, choices=False))
    usage = reported_stream_usage([chunk(), *terminal])
    assert (usage.prompt_tokens, usage.completion_tokens, usage.total_tokens) == (14399, 3, 14402)


@pytest.mark.parametrize("chunks", [[], [chunk()], [chunk(finish="stop")], [chunk(usage=True)], [chunk(finish="length", usage=True)]])
def test_stream_usage_rejects_incomplete_or_abnormal_terminal_state(chunks: list[ChatCompletionChunk]) -> None:
    with pytest.raises(AssertionError):
        reported_stream_usage(chunks)
