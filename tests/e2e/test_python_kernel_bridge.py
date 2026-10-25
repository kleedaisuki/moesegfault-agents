"""Credential-free bridge output contracts; real Jupyter runs in the container gate."""

from __future__ import annotations

import base64
from collections.abc import Callable
import importlib.util
from pathlib import Path
import queue
from types import ModuleType
from typing import Any

import pytest


@pytest.fixture
def bridge() -> ModuleType:
    path = Path(__file__).resolve().parents[2] / "container" / "python_kernel.py"
    spec = importlib.util.spec_from_file_location("magents_python_kernel", path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_unicode_logs_obey_one_combined_byte_budget(bridge: ModuleType, tmp_path: Path) -> None:
    output = bridge.Outputs(tmp_path, 7)
    output.message({"msg_type": "stream", "content": {"name": "stdout", "text": "雪🙂more"}})
    output.message({"msg_type": "stream", "content": {"name": "stderr", "text": "hidden tail"}})
    assert output.values == [{"type": "logs", "logs": "雪🙂"}]
    assert output.truncated and output.remaining == 0


def test_images_become_files_not_unbounded_json(bridge: ModuleType, tmp_path: Path) -> None:
    raw = b"bounded image\x00\xff"
    output = bridge.Outputs(tmp_path, 20)
    output.message({"msg_type": "display_data", "content": {"data": {
        "image/png": base64.b64encode(raw).decode(), "text/html": "<script>not forwarded</script>",
    }}})
    assert len(output.values) == 1
    item = output.values[0]
    assert item["type"] == "image" and item["mime_type"] == "image/png"
    assert (tmp_path / item["path"]).read_bytes() == raw
    assert "data" not in item and "html" not in item


def test_invalid_or_oversized_displays_are_explicit(bridge: ModuleType, tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(bridge, "MAX_ARTIFACT_BYTES", 16)
    output = bridge.Outputs(tmp_path, 20)
    output.image("image/png", "not base64!")
    assert output.artifact_error == "invalid_display_artifact"
    output.image("image/png", base64.b64encode(b"x" * 17).decode())
    assert output.artifact_error == "display_artifact_exceeds_limit"
    assert output.values == [] and list(tmp_path.iterdir()) == []


class Client:
    def __init__(self, fail: bool = False) -> None:
        self.fail = fail

    def execute_interactive(
        self, code: str, *, allow_stdin: bool, store_history: bool,
        stop_on_error: bool, timeout: float,
        output_hook: Callable[[dict[str, Any]], None],
    ) -> dict[str, Any]:
        assert code == "known source" and not allow_stdin and store_history and stop_on_error
        assert timeout == 2
        output_hook({"msg_type": "stream", "content": {"text": "before\n"}})
        if self.fail:
            raise queue.Empty
        output_hook({"msg_type": "error", "content": {"traceback": ["NameError: user failure"]}})
        return {"content": {"status": "error"}}


def test_python_errors_are_normal_recoverable_cell_results(bridge: ModuleType, tmp_path: Path) -> None:
    result = bridge.execute(Client(), {"id": "cell_1", "code": "known source", "timeout_ms": 2000, "max_output_bytes": 100}, tmp_path)
    assert result["id"] == "cell_1" and result["status"] == "error"
    assert result.get("error") is None
    assert result["outputs"] == [{"type": "logs", "logs": "before\nNameError: user failure\n"}]


def test_timeout_retains_identity_and_known_output(bridge: ModuleType, tmp_path: Path) -> None:
    result = bridge.execute(Client(fail=True), {"id": "cell_1", "code": "known source", "timeout_ms": 2000, "max_output_bytes": 100}, tmp_path)
    assert result == {"id": "cell_1", "error": "kernel_timeout", "outputs": [{"type": "logs", "logs": "before\n"}]}


def test_plain_representations_never_execute_html(bridge: ModuleType, tmp_path: Path) -> None:
    output = bridge.Outputs(tmp_path, 100)
    output.message({"msg_type": "execute_result", "content": {"data": {"text/plain": "42", "text/html": "<img onerror=bad>"}}})
    assert output.values == [{"type": "logs", "logs": "42\n"}]
