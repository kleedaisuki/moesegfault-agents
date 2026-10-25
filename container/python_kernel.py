"""Credential-free Jupyter client supervised by the Rust workspace daemon."""

from __future__ import annotations

import argparse
import base64
import json
import logging
import os
from pathlib import Path
import queue
import sys
import subprocess
import threading
import tempfile
from typing import Any


MAX_ARTIFACT_BYTES: int = 16 * 1024 * 1024
MAX_OUTPUTS: int = 128
MIME_SUFFIX: dict[str, str] = {
    "image/png": ".png",
    "image/jpeg": ".jpg",
    "image/gif": ".gif",
    "image/webp": ".webp",
    "application/pdf": ".pdf",
}


class Outputs:
    def __init__(self, directory: Path, budget: int) -> None:
        self.directory = directory
        self.remaining = budget
        self.values: list[dict[str, Any]] = []
        self.truncated = False
        self.artifact_error: str | None = None
        self.render_directory: Path | None = None

    def logs(self, text: str) -> None:
        raw = text.encode("utf-8", errors="replace")
        retained = raw[: self.remaining].decode("utf-8", errors="ignore")
        self.remaining -= len(retained.encode("utf-8"))
        self.truncated |= len(raw) > len(retained.encode("utf-8"))
        if not retained:
            return
        if self.values and self.values[-1]["type"] == "logs":
            self.values[-1]["logs"] += retained
        elif len(self.values) < MAX_OUTPUTS:
            self.values.append({"type": "logs", "logs": retained})
        else:
            self.truncated = True

    def image(self, mime: str, value: Any) -> None:
        if len(self.values) >= MAX_OUTPUTS:
            self.truncated = True
            return
        encoded_limit = ((MAX_ARTIFACT_BYTES + 2) // 3) * 4
        transport_limit = encoded_limit * 2 + 2
        if isinstance(value, list) and all(isinstance(part, str) for part in value):
            if sum(len(part) for part in value) > transport_limit:
                self.artifact_error = "display_artifact_exceeds_limit"
                return
            value = "".join(value)
        if not isinstance(value, str):
            self.artifact_error = "invalid_display_artifact"
            return
        if len(value) > transport_limit:
            self.artifact_error = "display_artifact_exceeds_limit"
            return
        # IPython 8.x Image uses b2a_base64 with a terminating newline.
        # Accept MIME whitespace, not arbitrary non-alphabet characters.
        value = value.translate(str.maketrans("", "", " \t\r\n"))
        if len(value) > encoded_limit:
            self.artifact_error = "display_artifact_exceeds_limit"
            return
        try:
            raw = base64.b64decode(value, validate=True)
        except (ValueError, TypeError):
            self.artifact_error = "invalid_display_artifact"
            return
        if len(raw) > MAX_ARTIFACT_BYTES:
            self.artifact_error = "display_artifact_exceeds_limit"
            return
        if self.render_directory is None:
            self.render_directory = Path(tempfile.mkdtemp(prefix=".magents-output-", dir=self.directory))
        path = self.render_directory / f"display-{len(self.values)}{MIME_SUFFIX[mime]}"
        with path.open("xb") as output:
            output.write(raw)
        self.values.append({
            "type": "image" if mime.startswith("image/") else "file",
            "path": path.relative_to(self.directory).as_posix(),
            "mime_type": mime,
        })

    def message(self, message: dict[str, Any]) -> None:
        content = message.get("content", {})
        if not isinstance(content, dict):
            return
        kind = message.get("msg_type")
        if kind == "stream":
            text = content.get("text")
            if isinstance(text, str):
                self.logs(text)
            return
        if kind == "error":
            trace = content.get("traceback", [])
            if isinstance(trace, list):
                self.logs("\n".join(line for line in trace if isinstance(line, str)) + "\n")
            return
        if kind not in {"display_data", "execute_result", "update_display_data"}:
            return
        data = content.get("data", {})
        if not isinstance(data, dict):
            return
        for mime in MIME_SUFFIX:
            if mime in data:
                self.image(mime, data[mime])
                return
        text = data.get("text/plain")
        if isinstance(text, str):
            self.logs(text + "\n")


def send(value: dict[str, Any]) -> None:
    sys.stdout.write(json.dumps(value, ensure_ascii=True, allow_nan=False, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def execute(client: Any, request: dict[str, Any], directory: Path) -> dict[str, Any]:
    execution_id = request["id"]
    outputs = Outputs(directory, int(request["max_output_bytes"]))
    try:
        reply: dict[str, Any] = client.execute_interactive(
            request["code"],
            allow_stdin=False,
            store_history=True,
            stop_on_error=True,
            timeout=float(request["timeout_ms"]) / 1000,
            output_hook=outputs.message,
        )
        status = reply.get("content", {}).get("status")
        if status not in {"ok", "error"}:
            return {"id": execution_id, "error": "invalid_kernel_reply", "outputs": outputs.values}
        return {
            "id": execution_id,
            "status": status,
            "outputs": outputs.values,
            "truncated": outputs.truncated,
            "artifact_error": outputs.artifact_error,
        }
    except (TimeoutError, queue.Empty):
        return {"id": execution_id, "error": "kernel_timeout", "outputs": outputs.values}
    except Exception:
        # Kernel details and user code are not operational diagnostics.
        return {"id": execution_id, "error": "kernel_connection_lost", "outputs": outputs.values}


def startup_kind(error: Exception) -> str:
    if isinstance(error, PermissionError):
        return "permission_denied"
    if isinstance(error, ImportError):
        return "dependency_unavailable"
    if isinstance(error, OSError):
        return "os_error"
    if isinstance(error, RuntimeError):
        return "runtime_error"
    return "initialization_error"


class StartupCapture:
    """Drain continuously; only a bounded diagnostic tail is retained."""

    def __init__(self, pipe: Any) -> None:
        self.bytes: bytearray = bytearray()
        self.collecting: bool = True
        self.lock: threading.Lock = threading.Lock()
        self.thread: threading.Thread = threading.Thread(target=self.drain, args=(pipe,), daemon=True)
        self.thread.start()

    def drain(self, pipe: Any) -> None:
        try:
            while chunk := pipe.read(4096):
                with self.lock:
                    if self.collecting:
                        self.bytes.extend(chunk)
                        if len(self.bytes) > 8192:
                            del self.bytes[:-8192]
        except (OSError, ValueError):
            pass

    def snapshot(self) -> bytes:
        self.thread.join(timeout=0.2)
        with self.lock:
            return bytes(self.bytes)

    def discard(self) -> None:
        with self.lock:
            self.collecting = False
            self.bytes.clear()


def startup_site(raw: bytes) -> str:
    latest: tuple[int, str] = (-1, "unknown")
    for marker, name in (
        (b"src/tcp.cpp:", "libzmq_tcp"),
        (b"src/ip.cpp:", "libzmq_ip"),
        (b"src/signaler.cpp:", "libzmq_signaler"),
        (b"src/ctx.cpp:", "libzmq_context"),
        (b"src/ipc_listener.cpp:", "libzmq_ipc"),
        (b"ipykernel/iostream.py", "kernel_iostream"),
        (b"ipykernel/debugger.py", "kernel_debugger"),
        (b"ipykernel/kernelapp.py", "kernel_application"),
    ):
        position: int = raw.rfind(marker)
        if position > latest[0]:
            latest = (position, name)
    return latest[1]


def startup_detail(error: Exception, process: Any, capture: StartupCapture | None) -> dict[str, Any]:
    value: dict[str, Any] = {}
    if isinstance(error, RuntimeError):
        message: str = str(error)
        if message.startswith("Kernel died before replying to kernel_info"):
            value["outcome"] = "kernel_died"
        elif message.startswith("Kernel didn't respond"):
            value["outcome"] = "readiness_timeout"
    if process is not None:
        status: Any = process.poll()
        if isinstance(status, int) and -128 <= status <= 255:
            value["exit_status"] = status
    if capture is not None:
        raw: bytes = capture.snapshot()
        value["site"] = startup_site(raw)
        if b"Operation not permitted" in raw or b"Permission denied" in raw:
            value["errno_class"] = "permission_denied"
        elif b"Address family not supported" in raw:
            value["errno_class"] = "address_family_unsupported"
    return value


def start(args: argparse.Namespace) -> tuple[Any, Any]:
    stage: str = "import"
    process: Any = None
    capture: StartupCapture | None = None
    try:
        from jupyter_client import KernelManager

        stage = "construct"
        manager: Any = KernelManager(
            connection_file=str(args.runtime / "connection.json"),
            transport="ipc",
            ip=str(args.runtime / "kernel"),
            cache_ports=False,
        )
        manager.kernel_spec.argv = [sys.executable, "-I", "-m", "ipykernel_launcher", "-f", "{connection_file}"]
        stage = "launch"
        manager.start_kernel(
            cwd=str(args.directory),
            env=dict(os.environ),
            stdin=open(os.devnull, "rb"),
            stdout=open(os.devnull, "wb"),
            stderr=subprocess.PIPE,
        )
        process = getattr(getattr(manager, "provisioner", None), "process", None)
        if process is not None and process.stderr is not None:
            capture = StartupCapture(process.stderr)
        stage = "channels"
        client: Any = manager.blocking_client()
        client.start_channels()
        stage = "ready"
        client.wait_for_ready(timeout=25)
        if capture is not None:
            capture.discard()
        return manager, client
    except Exception as error:
        # Startup happens before user execution. Only bounded machine labels,
        # never dependency exception text, paths or environment, cross the pipe.
        send({"type": "startup_error", "stage": stage, "kind": startup_kind(error), **startup_detail(error, process, capture)})
        raise SystemExit(1) from None


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--directory", required=True, type=Path)
    parser.add_argument("--runtime", required=True, type=Path)
    parser.add_argument("--container-id", required=True)
    args = parser.parse_args()
    logging.disable(logging.CRITICAL)
    args.runtime.mkdir(mode=0o700, parents=True, exist_ok=True)
    manager, client = start(args)
    try:
        send({"type": "ready", "container_id": args.container_id})
        for line in sys.stdin:
            request: Any = json.loads(line)
            if not isinstance(request, dict):
                return 2
            result = execute(client, request, args.directory)
            send(result)
            if result.get("error"):
                return 1
        return 0
    finally:
        client.stop_channels()
        manager.shutdown_kernel(now=True)


if __name__ == "__main__":
    raise SystemExit(main())
