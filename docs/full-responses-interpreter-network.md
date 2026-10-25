# Code Interpreter network isolation

## Implemented disabled boundary

The Python bridge and its managed Jupyter kernel use the upstream `ipc` transport
over filesystem Unix sockets, not TCP loopback. Before the bridge execs, the root
supervisor drops identity to UID 1001, sets `no_new_privs`, marks every descriptor
above stderr close-on-exec, and installs an irreversible seccomp filter generated
by Rust-VMM's mature `seccompiler` library. Filter compilation happens before fork.
Installation failure is a code-container startup error, never an unfiltered retry.

The filter allows only `AF_UNIX` socket/socketpair creation. IPv4, IPv6, packet,
netlink, vsock and other families fail with `EPERM`. It blocks `io_uring_setup`
because asynchronous socket operations do not pass through `socket(2)`, and blocks
`pidfd_getfd` so an unfiltered socket cannot be borrowed from another process.
The library rejects a different audit architecture. On x86-64 the same restrictions
also cover the x32 syscall-number aliases; IA32 `socketcall` cannot bypass the
audit-architecture guard. Fork/exec inherit the filter and cannot remove it.

UID 1000 Bash retains its existing networking. The supervisor is unfiltered and
retains the authority needed for its bounded control and artifact work. This is
not a claim that seccomp is a complete filesystem or malicious-kernel sandbox.
Unix sockets remain necessary for Jupyter. No existing root service exports an
unrestricted Internet relay or descriptor-passing service to UID 1001; any new
Unix service reachable by that UID must preserve that invariant.

The real-container tests exercise working Jupyter/Unix IPC, denial of all four
ordinary network families, inherited subprocess denial, io_uring/x32 denial and
unchanged Bash socket creation. They make no external network requests. GHA must
execute these tests against the actual image before deployment acceptance.

## Allowlist gateway

Ordinary `HTTP_PROXY` environment variables are not enforcement: malicious code
can ignore them. Direct AF_INET access therefore remains denied for allowlist
containers. A root-owned Unix HTTP gateway is configured from the private daemon
`CodeRequest.network_policy`. The first cell pins that policy to the logical
context; later requests may omit it to inherit but cannot change it. File imports
may precede the first cell. The gateway ends with its context; retirement cancels
active dispatches. Disabled contexts have no gateway.

The gateway accepts only bounded HTTPS requests to exact configured public
domains. It resolves and pins public addresses, keeps TLS SNI/certificate checking
bound to the approved URL hostname, disables environment proxies, refuses CONNECT,
and does not follow redirects. Public IPv4 and IPv6 addresses are checked before
dispatch; private/loopback/link-local/documentation/tunnel destinations cannot be
used to rebind DNS into the supervisor network. User Host, Authorization,
Proxy-* and forwarding headers are refused. A user cannot override a configured
domain-secret header under different casing. Domain headers are injected only
upstream after origin validation; the kernel receives no secret environment or
proxy response header. The authorized remote origin remains a trust boundary: an
origin can return its own application data, so it must be trusted with its injected
credential just as with any credential-bearing HTTP request.

The Unix socket is root-owned, group 1001, mode 0660, below the root-owned context
directory. Every connection is checked with `SO_PEERCRED` for UID 1001. Responses
contain only status, optional content type and a bounded base64 body. Requests and
responses are limited to 16 MiB raw; four requests may be in flight per context.
The listener retains permits on at most eight accepted Unix connections, before
HTTP header/body parsing, so many idle or oversized JSON clients cannot create an
unbounded number of root connection tasks or body allocations.
DNS, connect and total HTTP timeouts are bounded. Errors return a fixed machine
message, never request URLs, credential headers or reqwest exception details.

The image includes `requests-unixsocket` and the fully typed wrapper
`/usr/local/lib/magents/egress_client.py`. User code may invoke it with JSON stdin:

```python
import json, subprocess

reply = subprocess.run(
    ["/usr/bin/python3", "-I", "/usr/local/lib/magents/egress_client.py"],
    input=json.dumps({"url": "https://example.com/", "method": "GET"}),
    text=True, capture_output=True, check=True,
)
result = json.loads(reply.stdout)
```

Optional `headers` are a string map and `body` is base64. The wrapper sends only
to the nonsecret `MAGENTS_EGRESS_SOCKET` Unix path; it has no direct network
fallback and learns no domain secret. This is an explicit Unix HTTP capability,
not transparent unrestricted TCP networking or a global Python monkeypatch.
Ordinary direct socket access remains denied. Offline wrapper tests pass; the
actual image, kernel filter and positive public-TLS gateway journey still require
GHA/staging execution before the feature is accepted.

Cloudflare's current outbound interception can safely apply destination policies
and HTTPS header injection, but its documented scope is a whole Container, not a
Linux UID. Switching the existing shared Container's global `enableInternet` or
`allowedHosts` would also change established Bash behavior. It is not a drop-in
replacement for the interpreter-only isolation boundary.

## Primary references

- [OpenAI hosted-shell networking](https://developers.openai.com/api/docs/guides/tools-shell):
  hosted containers have no outbound access by default; allowlists and domain
  secrets are explicit capabilities shared with Code Interpreter.
- [Jupyter IPC transport](https://github.com/jupyter/jupyter_client/blob/main/jupyter_client/connect.py).
- [Rust-VMM seccompiler](https://docs.rs/seccompiler/0.5.0/seccompiler/).
- [Linux seccomp filters](https://docs.kernel.org/userspace-api/seccomp_filter.html).
- [Linux no-new-privileges](https://www.kernel.org/doc/html/latest/userspace-api/no_new_privs.html).
- [Cloudflare outbound traffic](https://developers.cloudflare.com/containers/configuration/outbound-traffic/).
