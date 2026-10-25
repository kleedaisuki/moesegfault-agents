"""Content-free receipts for failed SDK observers; never retry accepted work."""
from __future__ import annotations

import json
import re
import time

import httpx


def diagnostic_client(timeout: float) -> httpx.Client:
    return httpx.Client(timeout=timeout, event_hooks={
        "request": [_request_started], "response": [_response_received],
    })


def _request_started(request: httpx.Request) -> None:
    request.extensions["magents_observer_started"] = time.monotonic()


def _response_received(response: httpx.Response) -> None:
    if response.status_code < 500:
        return
    request = response.request
    started = request.extensions.get("magents_observer_started")
    receipt: dict[str, str | int] = {"status": response.status_code}
    if isinstance(started, float):
        receipt["duration_ms"] = max(0, round((time.monotonic() - started) * 1000))
    for header in ("x-request-id", "x-agents-session", "x-agents-run", "cf-ray"):
        value = response.headers.get(header)
        if value is not None and re.fullmatch(r"[A-Za-z0-9_-]{1,128}", value):
            receipt[header] = value
    traceparent = request.headers.get("traceparent")
    if traceparent is not None and re.fullmatch(r"00-[0-9a-f]{32}-[0-9a-f]{16}-0[01]", traceparent):
        receipt["trace_id"] = traceparent.split("-")[1]
    print("SDK observer failure: " + json.dumps(receipt, sort_keys=True), flush=True)
