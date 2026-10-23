"""Probe instrumentation checks; fixture numbers are not platform benchmarks."""
import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import threading
import time
import uuid

import httpx
from openai import OpenAI
import pytest

SPEC = importlib.util.spec_from_file_location("measure", Path(__file__).with_name("measure.py"))
measure = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(measure)


class DelayedStream(httpx.SyncByteStream):
    def __iter__(self):
        base = {"id": "fixture", "object": "chat.completion.chunk", "created": 1, "model": "agent"}
        events = [
            {**base, "choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": None}]},
            {**base, "choices": [{"index": 0, "delta": {"content": "ready"}, "finish_reason": None}]},
            {**base, "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
             "usage": {"prompt_tokens": 10, "completion_tokens": 1, "total_tokens": 11, "prompt_tokens_details": {"cached_tokens": 4}}},
        ]
        for event in events:
            time.sleep(.04)
            yield ("data: " + json.dumps(event) + "\n\n").encode()
        yield b"data: [DONE]\n\n"


def test_first_text_is_observed_before_stream_completion():
    def respond(request):
        assert request.headers["x-agents-session"] == "session"
        assert json.loads(request.content)["messages"] == [{"role": "user", "content": "Reply with exactly: ready"}]
        return httpx.Response(200, headers={"content-type": "text/event-stream", "x-agents-session": "session"}, stream=DelayedStream())

    with OpenAI(api_key="fixture-only", base_url="http://127.0.0.1/v1", http_client=httpx.Client(transport=httpx.MockTransport(respond))) as sdk:
        session, metrics = measure.stream_turn(sdk, "agent", "session")
    assert session == "session"
    assert metrics["headers_ms"] < metrics["first_event_ms"] < metrics["first_text_ms"] < metrics["total_ms"]
    assert metrics["characters"] == 5 and metrics["chunks"] == 3
    assert metrics["usage"]["cached_tokens"] == 4
    assert "ready" not in json.dumps(metrics)


def test_sdk_lazy_resource_initialization_is_separate_from_request_time():
    def respond(_request):
        return httpx.Response(200, headers={"content-type": "text/event-stream", "x-agents-session": "session"}, stream=DelayedStream())

    with OpenAI(api_key="fixture-only", base_url="http://127.0.0.1/v1", http_client=httpx.Client(transport=httpx.MockTransport(respond))) as sdk:
        class DelayedSdk:
            accesses = 0

            @property
            def chat(self):
                self.accesses += 1
                time.sleep(.04)
                return sdk.chat

        wrapped = DelayedSdk()
        _, metrics = measure.stream_turn(wrapped, "agent")
    assert wrapped.accesses == 1
    assert metrics["sdk_setup_ms"] >= 35
    assert metrics["headers_ms"] < metrics["sdk_setup_ms"]
    assert metrics["client_total_ms"] >= metrics["sdk_setup_ms"] + metrics["total_ms"] - 1


@pytest.mark.parametrize("origin", ["https://agents.moesegfault.dev", "http://agents-staging.moesegfault.dev", "https://agents-staging.moesegfault.dev.evil.test", "https://user@agents-staging.moesegfault.dev", "https://agents-staging.moesegfault.dev/path"])
def test_nonstaging_origins_are_refused(origin):
    with pytest.raises(argparse.ArgumentTypeError):
        measure.staging_origin(origin)


def test_summaries_use_nearest_rank_with_explicit_sample_count():
    assert measure.summary([9, 1, 3])["p50_ms"] == 3
    assert measure.summary([9, 1, 3])["p95_ms"] == 9
    assert measure.summary([9, 1, 3])["count"] == 3
    assert measure.summary([]) == {}


def test_executable_journey_records_only_metrics_and_cleans_up():
    observed = []

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *_):
            pass

        def handle_request(self):
            assert self.headers["Authorization"] == "Bearer fixture-only-key"
            body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
            observed.append((self.command, self.path))
            headers = {"Content-Type": "application/json"}
            if self.path == "/v1/chat/completions":
                assert json.loads(body)["stream"] is True
                content = b"".join(DelayedStream())
                headers.update({"Content-Type": "text/event-stream", "x-agents-session": "fixture-session"})
            elif self.command == "POST":
                content = b'{"id":"fixture-agent","model":{"provider":"openrouter","model":"fixture-model"}}'
            else:
                content = b'{"data":[]}'
            self.send_response(200)
            for name, value in headers.items():
                self.send_header(name, value)
            self.send_header("Content-Length", str(len(content)))
            self.end_headers()
            self.wfile.write(content)

        do_GET = handle_request
        do_POST = handle_request
        do_DELETE = handle_request

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    worker = threading.Thread(target=server.serve_forever, daemon=True)
    worker.start()
    relative = ".cache/perf/fixture-" + uuid.uuid4().hex + ".json"
    output = measure.ROOT / relative
    try:
        result = subprocess.run([
            sys.executable, str(Path(__file__).with_name("measure.py")),
            "--origin", f"http://127.0.0.1:{server.server_port}", "--samples", "1",
            "--inference", "--output", relative,
        ], env={**os.environ, "MAGENTS_TEST_API_KEY": "fixture-only-key"}, capture_output=True, timeout=30)
        assert result.returncode == 0, result.stderr.decode()
        report = json.loads(output.read_text(encoding="utf-8"))
        assert report["schema"] == 2
        assert len(report["management"]) == 4
        assert len(report["inference"]["samples"]) == 1
        assert report["cleanup_failures"] == 0
        assert observed.count(("POST", "/v1/chat/completions")) == 3
        assert ("DELETE", "/v1/sessions/fixture-session") in observed
        assert ("DELETE", "/v1/agents/fixture-agent") in observed
        serialized = json.dumps(report)
        for secret_or_content in ["fixture-only-key", "fixture-session", "fixture-agent", "ready"]:
            assert secret_or_content not in serialized
    finally:
        server.shutdown()
        server.server_close()
        worker.join(timeout=2)
        output.unlink(missing_ok=True)
