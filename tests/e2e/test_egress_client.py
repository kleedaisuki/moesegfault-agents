"""Offline contracts for the mature Unix HTTP client wrapper."""

from __future__ import annotations

import importlib.util
import io
import json
from pathlib import Path
import sys
import types
from typing import Any, Iterator

import pytest


def load(monkeypatch: pytest.MonkeyPatch, session: Any = None) -> Any:
    monkeypatch.setitem(sys.modules, "requests_unixsocket", types.SimpleNamespace(Session=session))
    path: Path = Path(__file__).resolve().parents[2] / "container" / "egress_client.py"
    spec: Any = importlib.util.spec_from_file_location("egress_client_test", path)
    assert spec is not None and spec.loader is not None
    module: Any = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_disabled_client_has_no_network_fallback(monkeypatch: pytest.MonkeyPatch) -> None:
    client: Any = load(monkeypatch)
    monkeypatch.delenv("MAGENTS_EGRESS_SOCKET", raising=False)
    with pytest.raises(ValueError, match="disabled"):
        client.request({"url": "https://example.com"})


def test_client_uses_only_the_unix_gateway_and_bounds_response(monkeypatch: pytest.MonkeyPatch) -> None:
    captured: list[str] = []

    class Response:
        def __enter__(self) -> Response:
            return self

        def __exit__(self, *args: Any) -> None:
            pass

        def raise_for_status(self) -> None:
            pass

        def iter_content(self, chunk_size: int) -> Iterator[bytes]:
            yield b'{"status":200,"body":"b2s="}'

    class Session:
        def __enter__(self) -> Session:
            return self

        def __exit__(self, *args: Any) -> None:
            pass

        def post(self, endpoint: str, **options: Any) -> Response:
            captured.append(endpoint)
            assert options["json"]["url"] == "https://example.com"
            assert options["stream"] is True
            return Response()

    client: Any = load(monkeypatch, Session)
    monkeypatch.setenv("MAGENTS_EGRESS_SOCKET", "/root protected/egress.sock")
    assert client.request({"url": "https://example.com"})["status"] == 200
    assert captured == ["http+unix://%2Froot%20protected%2Fegress.sock/fetch"]
    client.MAX_REPLY_BYTES = 3
    with pytest.raises(ValueError, match="limit"):
        client.request({"url": "https://example.com"})


def test_client_diagnostics_do_not_echo_private_exceptions(monkeypatch: pytest.MonkeyPatch) -> None:
    client: Any = load(monkeypatch)
    monkeypatch.delenv("MAGENTS_EGRESS_SOCKET", raising=False)
    stdin: io.TextIOWrapper = io.TextIOWrapper(io.BytesIO(json.dumps({"url": "https://secret.example"}).encode()))
    stdout: io.StringIO = io.StringIO()
    monkeypatch.setattr(sys, "stdin", stdin)
    monkeypatch.setattr(sys, "stdout", stdout)
    assert client.main() == 1
    assert stdout.getvalue() == '{"error":"egress_request_rejected"}\n'
