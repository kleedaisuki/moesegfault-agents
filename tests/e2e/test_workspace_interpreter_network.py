"""Real Linux kernel enforcement; no internet request or paid service is used."""

from __future__ import annotations

import uuid
import json
import os
from typing import Any

from conftest import DockerDaemon
import pytest


def execute(daemon: DockerDaemon, code: str) -> str:
    request: dict[str, Any] = {"id": "net_" + uuid.uuid4().hex, "code": code}
    result: dict[str, Any] = daemon.json("POST", "/v1/python", request, timeout=90)
    assert result["error"] is None
    assert result["python_error"] is False
    return "".join(item["logs"] for item in result["outputs"] if item["type"] == "logs")


def test_python_has_unix_ipc_but_no_ipv4_ipv6_packet_or_netlink(daemon: DockerDaemon) -> None:
    source = """
import errno, socket
a,b=socket.socketpair()
a.send(b'ipc-ok')
print(b.recv(6).decode())
a.close(); b.close()
for family in (socket.AF_INET,socket.AF_INET6,socket.AF_PACKET,socket.AF_NETLINK):
    try:
        socket.socket(family,socket.SOCK_DGRAM)
        print('UNSAFE',family)
    except OSError as error:
        print('blocked',family,error.errno)
"""
    output = execute(daemon, source)
    assert "ipc-ok" in output
    assert "UNSAFE" not in output
    assert output.count("blocked") == 4


def test_network_filter_survives_subprocess_and_does_not_change_bash(daemon: DockerDaemon) -> None:
    source = """
import subprocess,sys
program="import socket; socket.socket(socket.AF_INET,socket.SOCK_STREAM)"
result=subprocess.run([sys.executable,'-c',program],capture_output=True,text=True)
print('subprocess-blocked', result.returncode != 0 and 'PermissionError' in result.stderr)
"""
    assert "subprocess-blocked True" in execute(daemon, source)
    request: dict[str, Any] = {
        "id": "bash_net_" + uuid.uuid4().hex,
        "command": "python3 -c 'import socket; s=socket.socket(socket.AF_INET,socket.SOCK_STREAM); print(\"bash-socket-ok\"); s.close()'",
    }
    result: dict[str, Any] = daemon.json("POST", "/v1/exec", request)
    assert "bash-socket-ok" in result["stdout"]


def test_io_uring_and_x32_cannot_create_unfiltered_sockets(daemon: DockerDaemon) -> None:
    source = """
import ctypes,errno,platform
libc=ctypes.CDLL(None,use_errno=True)
parameters=ctypes.create_string_buffer(256)
value=libc.syscall(425,1,ctypes.byref(parameters))
print('uring-blocked',value == -1 and ctypes.get_errno() == errno.EPERM)
if platform.machine() == 'x86_64':
    value=libc.syscall(0x40000000+41,2,1,0)
    print('x32-blocked',value == -1 and ctypes.get_errno() == errno.EPERM)
"""
    output = execute(daemon, source)
    assert "uring-blocked True" in output
    assert "x32-blocked False" not in output


def reset(daemon: DockerDaemon) -> None:
    result: dict[str, Any] = daemon.json("POST", "/v1/python", {
        "id": "net_reset_" + uuid.uuid4().hex, "code": "import time; time.sleep(30)", "timeout_ms": 20,
    }, timeout=90)
    assert result["timed_out"] is True and result["error"] == "code_timeout"


def gateway_cell(daemon: DockerDaemon, requests: list[dict[str, Any]], policy: dict[str, Any]) -> dict[str, Any]:
    source: str = """import json,os,subprocess
for value in %s:
 result=subprocess.run(['/usr/bin/python3','-I','/usr/local/lib/magents/egress_client.py'],input=json.dumps(value),text=True,capture_output=True)
 print(json.dumps({'code':result.returncode,'value':json.loads(result.stdout)}))
print('secret-env',any('private-sentinel' in value for value in os.environ.values()))
""" % repr(requests)
    return daemon.json("POST", "/v1/python", {"id": "net_gateway_"+uuid.uuid4().hex,"code":source,"network_policy":policy}, timeout=180)


def test_allowlist_gateway_rejects_untrusted_origins_and_header_confusion(daemon: DockerDaemon) -> None:
    reset(daemon)
    policy: dict[str, Any] = {"type":"allowlist","allowed_domains":["example.com"],"domain_secrets":[{"domain":"example.com","name":"Authorization","value":"Bearer private-sentinel"}]}
    cases: list[dict[str, Any]] = [
        {"url":"https://127.0.0.1/"}, {"url":"https://evil.example.com/"},
        {"url":"http://example.com/"}, {"url":"https://example.com/","method":"CONNECT"},
        {"url":"https://example.com/","headers":{"Host":"localhost"}},
        {"url":"https://example.com/","headers":{"Authorization":"override"}},
    ]
    try:
        result: dict[str, Any] = gateway_cell(daemon,cases,policy)
        assert result["error"] is None and result["python_error"] is False
        output: str = "".join(item["logs"] for item in result["outputs"] if item["type"]=="logs")
        assert output.count('"code": 1')==len(cases)
        assert "secret-env False" in output and "private-sentinel" not in output
        conflict: dict[str, Any] = daemon.json("POST","/v1/python",{"id":"net_conflict_"+uuid.uuid4().hex,"container_id":result["container_id"],"code":"print('must-not-run')","network_policy":{"type":"disabled"}})
        assert conflict["error"]=="code_network_policy_conflict"
    finally:
        reset(daemon)


@pytest.mark.skipif(os.getenv("RUN_EGRESS_NETWORK_PROBE")!="1",reason="explicit real public-TLS probe")
def test_allowlist_gateway_real_public_tls_has_no_direct_socket_escape(daemon: DockerDaemon) -> None:
    reset(daemon)
    try:
        result: dict[str, Any] = gateway_cell(daemon,[{"url":"https://example.com/","method":"GET"}],{"type":"allowlist","allowed_domains":["example.com"]})
        assert result["error"] is None and result["python_error"] is False
        output: str = "".join(item["logs"] for item in result["outputs"] if item["type"]=="logs")
        value: dict[str, Any] = json.loads(output.splitlines()[0])
        assert value["code"]==0 and value["value"]["status"]==200
        assert value["value"]["body"]
        assert "UNSAFE" not in execute(daemon,"import socket\ntry:\n socket.socket(socket.AF_INET,socket.SOCK_STREAM)\n print('UNSAFE')\nexcept PermissionError:\n print('kernel-denied')")
    finally:
        reset(daemon)
