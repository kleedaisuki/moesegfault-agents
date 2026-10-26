"""Explicit code-resource release preserves immutable, unpublished artifacts."""
from __future__ import annotations

import hashlib
import concurrent.futures
import time
from typing import Any
import uuid

from conftest import DockerDaemon


def test_context_metadata_does_not_wait_for_a_running_cell(daemon: DockerDaemon) -> None:
    prepared: dict[str, Any] = daemon.json("POST", "/v1/python/contexts", {})
    cid: str = prepared["container_id"]
    execution_id: str = "metadata_" + uuid.uuid4().hex
    with concurrent.futures.ThreadPoolExecutor() as pool:
        active: concurrent.futures.Future[dict[str, Any]] = pool.submit(daemon.json, "POST", "/v1/python", {
            "id": execution_id, "container_id": cid, "code": "import time\ntime.sleep(3)",
        })
        for _ in range(100):
            if daemon.request("GET", "/v1/python/" + execution_id)[0] == 202:
                break
            time.sleep(0.01)
        else:
            raise AssertionError("Cell never entered its observable active state")
        assert not active.done()
        metadata: dict[str, Any] = daemon.json("GET", "/v1/python/contexts/" + cid, timeout=1)
        assert metadata == {"container_id": cid, "status": "running"}
        assert not active.done(), "Metadata must not be serialized behind user execution"
        assert active.result(timeout=15)["error"] is None


def test_explicit_context_release_does_not_retire_a_replacement_or_its_artifact_spool(
    daemon: DockerDaemon,
) -> None:
    prepared: dict[str, Any] = daemon.json("POST", "/v1/python/contexts", {"network_policy": {"type": "disabled"}})
    cid: str = prepared["container_id"]
    route: str = "/v1/python/contexts/" + cid
    assert daemon.json("GET", route) == {"container_id": cid, "status": "running"}
    result: dict[str, Any] = daemon.json("POST", "/v1/python", {
        "id": "context_" + uuid.uuid4().hex,
        "container_id": cid,
        "code": "from pathlib import Path\nretained_context_value = 42\nPath('retained.csv').write_bytes(b'value\\n42\\n')",
    })
    assert result["error"] is None
    artifact: dict[str, Any] = next(item for item in result["artifacts"] if item["filename"] == "retained.csv")
    body: bytes = daemon.request("GET", "/v1/python/artifacts/" + artifact["id"])[2]
    assert hashlib.sha256(body).hexdigest() == artifact["sha256"]
    unknown: str = "cntr_" + uuid.uuid4().hex
    assert daemon.request("DELETE", "/v1/python/contexts/" + unknown)[0] == 404
    assert daemon.json("GET", route)["container_id"] == cid
    assert daemon.request("DELETE", route)[0] == 204
    assert daemon.request("DELETE", route)[0] == 204
    assert daemon.request("GET", route)[0] == 404
    assert daemon.request("GET", "/v1/python/artifacts/" + artifact["id"])[2] == body
    replacement: dict[str, Any] = daemon.json("POST", "/v1/python/contexts", {"network_policy": {"type": "disabled"}})
    assert replacement["container_id"] != cid
    assert daemon.request("DELETE", route)[0] == 404
    assert daemon.json("GET", "/v1/python/contexts/" + replacement["container_id"])["container_id"] == replacement["container_id"]
    assert daemon.request("GET", "/v1/python/artifacts/" + artifact["id"])[2] == body
