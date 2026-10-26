"""Python's authorized durable-tree writes use the same commit barrier as Bash."""
from __future__ import annotations

import hashlib
from http.client import HTTPMessage
from typing import Any
import uuid

from conftest import DockerDaemon


def test_code_scratch_is_clean_but_authorized_durable_writes_invalidate_checkpoint_ack(
    daemon: DockerDaemon,
) -> None:
    first: dict[str, Any] = daemon.json("POST", "/v1/python", {
        "id": "scratch_" + uuid.uuid4().hex,
        "code": "from pathlib import Path\nPath('scratch.txt').write_text('temporary code artifact')",
    })
    assert first["error"] is None
    assert daemon.json("GET", "/v1/status")["dirty_bytes"] == 0
    made: dict[str, Any] = daemon.json("POST", "/v1/exec", {
        "id": "share_" + uuid.uuid4().hex,
        "command": "chmod 777 .; printf original > shared.txt; chmod 666 shared.txt; pwd",
    })
    assert made["exit_code"] == 0
    directory: str = made["stdout"].strip()
    assert directory.startswith("/var/lib/magents/")
    status: int
    headers: HTTPMessage
    before: bytes
    status, headers, before = daemon.request("GET", "/v1/snapshot")
    assert status == 200
    old_sha: str = headers["X-Snapshot-Sha256"]
    assert hashlib.sha256(before).hexdigest() == old_sha
    assert daemon.request("POST", "/v1/checkpoint-ack", {"sha256": old_sha})[0] == 204
    assert daemon.json("GET", "/v1/status")["dirty_bytes"] == 0
    changed: dict[str, Any] = daemon.json("POST", "/v1/python", {
        "id": "durable_" + uuid.uuid4().hex,
        "container_id": first["container_id"],
        "code": f"Path({directory!r}).joinpath('shared.txt').write_text('modified')",
    })
    assert changed["error"] is None and changed["python_error"] is False
    assert daemon.json("GET", "/v1/status")["dirty_bytes"] > 0
    assert daemon.request("POST", "/v1/checkpoint-ack", {"sha256": old_sha})[0] == 204
    assert daemon.json("GET", "/v1/status")["dirty_bytes"] > 0
    status, headers, after = daemon.request("GET", "/v1/snapshot")
    assert status == 200
    new_sha: str = headers["X-Snapshot-Sha256"]
    assert hashlib.sha256(after).hexdigest() == new_sha and new_sha != old_sha
    assert daemon.request("POST", "/v1/restore", after, headers={"Content-Type": "application/zstd", "X-Snapshot-Sha256": new_sha})[0] == 200
    verified: dict[str, Any] = daemon.json("POST", "/v1/exec", {
        "id": "verify_" + uuid.uuid4().hex, "command": "cat shared.txt",
    })
    assert verified["exit_code"] == 0 and verified["stdout"] == "modified"
