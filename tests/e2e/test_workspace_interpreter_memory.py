"""Actual cgroup allocation, not a metadata-only interpreter memory label."""

from __future__ import annotations

from collections.abc import Iterator
import json
import os
import subprocess
from typing import Any
import uuid

import pytest

from conftest import DockerDaemon


@pytest.fixture(params=["1g", "4g", None])
def sized_daemon(request: pytest.FixtureRequest) -> Iterator[tuple[DockerDaemon, str | None]]:
    image: str | None = os.getenv("MAGENTS_DAEMON_IMAGE")
    if not image:
        pytest.skip("memory journey requires a built Linux MAGENTS_DAEMON_IMAGE")
    memory: str | None = request.param
    token: str = uuid.uuid4().hex + uuid.uuid4().hex
    name: str = "magents-memory-" + uuid.uuid4().hex
    command: list[str] = ["docker", "run", "--detach", "--name", name,
                          "--publish", "127.0.0.1::8080", "--env", "MAGENTS_DAEMON_TOKEN"]
    if memory is not None:
        command.extend(["--memory", memory, "--memory-swap", memory])
    command.append(image)
    subprocess.run(command, env={**os.environ, "MAGENTS_DAEMON_TOKEN": token},
                   check=True, capture_output=True, timeout=30)
    daemon: DockerDaemon = DockerDaemon(name, token)
    try:
        daemon.wait_ready()
        yield daemon, memory
    finally:
        subprocess.run(["docker", "rm", "--force", name],
                       check=True, capture_output=True, timeout=20)


def test_physical_memory_guard_precedes_kernel_and_execution_id_acceptance(
    sized_daemon: tuple[DockerDaemon, str | None],
) -> None:
    daemon, memory = sized_daemon
    execution_id: str = "memory_" + uuid.uuid4().hex
    wrong: str = "4g" if memory == "1g" else "1g"
    payload: dict[str, Any] = {"id": execution_id, "code": "print('allocation verified')", "memory_limit": wrong}
    status, _, body = daemon.request("POST", "/v1/python", payload)
    assert status == 422
    assert json.loads(body)["error"] == "code_memory_profile_mismatch"
    assert daemon.request("GET", "/v1/python/" + execution_id)[0] == 404
    for unsupported in ("16g", "64g"):
        assert daemon.request("POST", "/v1/python", {**payload, "memory_limit": unsupported})[0] == 422
    if memory is None:
        # The old private supervisor API makes no allocation promise. Public
        # Responses always sends an explicitly admitted memory tier.
        payload.pop("memory_limit")
    else:
        payload["memory_limit"] = memory
    result: dict[str, Any] = daemon.json("POST", "/v1/python", payload, timeout=90)
    assert result["error"] is None
    assert any("allocation verified" in output.get("logs", "") for output in result["outputs"])
    assert daemon.request("POST", "/v1/python", payload)[0] == 409
