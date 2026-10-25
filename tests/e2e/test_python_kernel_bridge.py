"""Credential-free bridge output contracts; real Jupyter runs in the container gate."""

from __future__ import annotations

import base64
import binascii
import argparse
from collections.abc import Callable
import importlib.util
import io
from pathlib import Path
import queue
from types import ModuleType, SimpleNamespace
import sys
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


def test_framework_extension_never_initializes_the_optional_tcp_fork_pipe(
    bridge: ModuleType, monkeypatch: pytest.MonkeyPatch,
) -> None:
    calls: list[str] = []
    socket: Any = SimpleNamespace(linger=0)
    background: object = object()

    class Thread:
        def __init__(self, output: Any, *, pipe: bool) -> None:
            assert output is socket and pipe is False
            self.background_socket: object = background
            calls.append("ipc-thread")

        def start(self) -> None:
            calls.append("started")

    class App:
        @classmethod
        def launch_instance(cls, *, argv: list[str]) -> None:
            assert argv == ["-f", "connection.json"]
            app: Any = cls()
            app.context = SimpleNamespace(socket=lambda kind: socket)
            app.iopub_port = 3
            app._bind_socket = lambda output, port: port
            app.configure_tornado_logger = lambda: calls.append("logger")
            app.init_iopub()
            assert app.iopub_socket is background and socket.linger == 1000

    monkeypatch.setitem(sys.modules,"ipykernel.kernelapp",SimpleNamespace(IPKernelApp=App))
    monkeypatch.setitem(sys.modules,"ipykernel.iostream",SimpleNamespace(IOPubThread=Thread))
    monkeypatch.setitem(sys.modules,"zmq",SimpleNamespace(PUB=1))
    monkeypatch.setattr(sys,"argv",["python_kernel.py","--kernel","-f","connection.json"])
    bridge.kernel_main()
    assert calls == ["logger","ipc-thread","started"]


def test_startup_stderr_is_bounded_and_never_returned_raw(bridge: ModuleType) -> None:
    capture: Any = bridge.StartupCapture(io.BytesIO(b"private-path-sentinel" * 1000 + b"Operation not permitted (src/tcp.cpp:123)"))
    capture.thread.join(timeout=1)
    assert not capture.thread.is_alive()
    assert len(capture.snapshot()) == 8192
    value: dict[str, Any] = bridge.startup_detail(
        RuntimeError("Kernel died before replying to kernel_info: private-message-sentinel"),
        SimpleNamespace(poll=lambda: -6), capture,
    )
    assert value == {"outcome":"kernel_died","exit_status":-6,"site":"libzmq_tcp","errno_class":"permission_denied"}
    assert "private" not in str(value)
    assert bridge.startup_site(b"private-path-or-secret") == "unknown"
    assert bridge.startup_site(b"ipykernel/iostream.py warning followed by src/tcp.cpp:123") == "libzmq_tcp"
    capture.discard()
    capture.drain(io.BytesIO(b"later-user-stderr-must-not-be-retained"))
    assert capture.snapshot() == b"" and capture.collecting is False


@pytest.mark.parametrize("failed_stage", ["construct", "launch", "channels", "ready"])
def test_startup_failure_exposes_only_fixed_stage_not_private_exception(
    bridge: ModuleType, tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str], failed_stage: str,
) -> None:
    class Manager:
        def __init__(self, **options: Any) -> None:
            assert options["transport"] == "ipc" and options["cache_ports"] is False
            self.kernel_spec: Any = SimpleNamespace(argv=[])
            if failed_stage == "construct":
                raise PermissionError("private-path-or-secret-sentinel")

        def start_kernel(self, **options: Any) -> None:
            if failed_stage == "launch":
                raise PermissionError("private-path-or-secret-sentinel")

        def blocking_client(self) -> Manager:
            return self

        def start_channels(self) -> None:
            if failed_stage == "channels":
                raise PermissionError("private-path-or-secret-sentinel")

        def wait_for_ready(self, timeout: int) -> None:
            if failed_stage == "ready":
                raise PermissionError("private-path-or-secret-sentinel")

    monkeypatch.setitem(sys.modules,"jupyter_client",SimpleNamespace(KernelManager=Manager))
    with pytest.raises(SystemExit):
        bridge.start(argparse.Namespace(runtime=tmp_path,directory=tmp_path))
    output: str = capsys.readouterr().out
    assert "private-path-or-secret-sentinel" not in output
    assert '"stage":"' + failed_stage + '"' in output
    assert '"kind":"permission_denied"' in output


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


@pytest.mark.parametrize("transport", ["ipython8", "wrapped", "lines"])
def test_ipython_mime_base64_whitespace_preserves_exact_artifact(
    bridge: ModuleType, tmp_path: Path, transport: str,
) -> None:
    raw = b"\x89PNG\r\n\x1a\n" + bytes(range(256))
    encoded: str | list[str]
    if transport == "ipython8":
        encoded = binascii.b2a_base64(raw).decode("ascii")
        assert encoded.endswith("\n")
    else:
        wrapped = base64.encodebytes(raw).decode("ascii").replace("\n", "\r\n")
        encoded = wrapped.splitlines(keepends=True) if transport == "lines" else wrapped
    output = bridge.Outputs(tmp_path, 100)
    output.message({"msg_type": "display_data", "content": {"data": {"image/png": encoded}}})
    assert output.artifact_error is None and len(output.values) == 1
    assert (tmp_path / output.values[0]["path"]).read_bytes() == raw


def test_mime_whitespace_does_not_relax_invalid_base64_or_transport_bound(
    bridge: ModuleType, tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(bridge, "MAX_ARTIFACT_BYTES", 16)
    for malformed in ("eA==!\n", "eA==\v", "eA==é", "eA==\u2003"):
        output = bridge.Outputs(tmp_path, 100)
        output.image("image/png", malformed)
        assert output.artifact_error == "invalid_display_artifact" and output.values == []
    output = bridge.Outputs(tmp_path, 100)
    output.image("image/png", " " * 100)
    assert output.artifact_error == "display_artifact_exceeds_limit" and output.values == []
