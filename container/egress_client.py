"""Bounded Unix HTTP client. Domain credentials exist only in the root gateway."""

from __future__ import annotations

import json
import os
import sys
from typing import Any
from urllib.parse import quote

import requests_unixsocket

MAX_REQUEST_BYTES: int = 24 * 1024 * 1024
MAX_REPLY_BYTES: int = 24 * 1024 * 1024


def request(value: dict[str, Any]) -> dict[str, Any]:
    socket: str | None = os.environ.get("MAGENTS_EGRESS_SOCKET")
    if socket is None:
        raise ValueError("network access is disabled")
    endpoint: str = "http+unix://" + quote(socket, safe="") + "/fetch"
    with requests_unixsocket.Session() as session:
        with session.post(endpoint, json=value, timeout=(5, 65), stream=True) as response:
            response.raise_for_status()
            raw: bytearray = bytearray()
            for chunk in response.iter_content(chunk_size=64 * 1024):
                if len(raw) + len(chunk) > MAX_REPLY_BYTES:
                    raise ValueError("egress reply exceeds limit")
                raw.extend(chunk)
    result: Any = json.loads(raw)
    if not isinstance(result, dict):
        raise ValueError("invalid egress reply")
    return result


def main() -> int:
    raw: bytes = sys.stdin.buffer.read(MAX_REQUEST_BYTES + 1)
    if len(raw) > MAX_REQUEST_BYTES:
        return 2
    try:
        value: Any = json.loads(raw)
        if not isinstance(value, dict):
            return 2
        result: dict[str, Any] = request(value)
    except Exception:
        # Upstream exceptions can include URLs or request data. They are not
        # operational diagnostics, nor should they escape through the wrapper.
        sys.stdout.write('{"error":"egress_request_rejected"}\n')
        return 1
    sys.stdout.write(json.dumps(result, ensure_ascii=True, separators=(",", ":")) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
