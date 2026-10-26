"""The owned executor advertises actual allocation without starting a namespace."""
from __future__ import annotations

import json
from http.client import HTTPMessage
from typing import Any

from conftest import DockerDaemon
from test_workspace_interpreter_memory import sized_daemon


def test_executor_capabilities_and_atomic_prepare_boot_match_actual_instance(
    sized_daemon: tuple[DockerDaemon, str | None],
) -> None:
    daemon, memory = sized_daemon
    assert memory == "4g", "Select only this new contract's4g parametrization"
    assert daemon.request("GET", "/v1/python/capabilities", authenticated=False)[0] == 401
    capabilities: dict[str, Any] = daemon.json("GET", "/v1/python/capabilities")
    assert capabilities["protocol"] == "workspace-daemon"
    assert capabilities["context_scope"] == "instance"
    assert capabilities["memory_limits"] == [memory]
    assert capabilities["atomic_boot_header"] is True
    assert daemon.request("GET", "/v1/python/contexts")[0] == 404
    status: int
    headers: HTTPMessage
    body: bytes
    status, headers, body = daemon.request("POST", "/v1/python/contexts", {"memory_limit": memory})
    assert status == 200
    prepared: dict[str, Any] = json.loads(body)
    assert prepared["container_id"].startswith("cntr_")
    assert headers["X-Code-Boot-Id"] == daemon.json("GET", "/health")["boot_id"]
