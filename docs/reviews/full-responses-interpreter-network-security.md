# Code Interpreter network security review

Reviewed the uncommitted network slice over `b1edf97746d3902373dbb05e224799f03d9a3a10`:
`interpreter_network.rs`, `interpreter_egress.rs`, the process and interpreter
hooks, Jupyter IPC transport, `egress_client.py`, Docker packaging and network
journeys. This is an independent source review, not a live Linux/TLS acceptance
receipt. Production changes were made by the implementation owner, not reviewer.

## Findings and remediation

### P1 — domain credentials crossed into the untrusted interpreter UID (resolved in source)

The first version of `Interpreter::cell` serialized the complete `CodeRequest`
into the bridge pipe. The newly added `network_policy.domain_secrets` therefore
went to the UID 1001 Python bridge, despite the intended root-only gateway
boundary. The client and arbitrary kernel code share that UID. A same-UID bridge
pipe or process inspection must not become a prerequisite for preserving secret
confidentiality; an environment-only sentinel test did not cover this IPC path.

The fix adds `kernel_request` with an explicit four-field serialization DTO:
`id`, `code`, `timeout_ms`, `max_output_bytes`. The actual cell dispatch invokes
that function, not serialization of the supervisor request. The regression
constructs a real request containing domain credentials, a memory selector and a
container selector, then checks the actual serialized bridge bytes have exactly
four fields and contain none of those private fields or the credential sentinel.
Independent re-review confirms the root policy does not enter this IPC payload.

### P2 — expired contexts retained root egress authority on cleanup failure (resolved in source)

Originally the root gateway was destroyed only when the retired context slot
was cleared, after the UID kill barrier, child reap and filesystem cleanup.
A failure at any of those steps retained an expired context with its gateway and
accepted requests still alive. Expiry must revoke network authority even while a
failed process/filesystem cleanup keeps the context as a safety fence.

The fix marks the context expired and immediately takes/drops its gateway before
any fallible cleanup. `Gateway::drop` cancels the dispatch token and aborts the
listener. An expired slot remains unusable if cleanup fails, without retaining
root HTTP authority. Already transmitted remote side effects cannot be undone;
the fix does not claim otherwise.

## Boundary analysis

- The pre-exec filter is compiled before fork, then installed only after UID/GID
  drop and `no_new_privs`. Installation fails closed. Descriptors above stderr
  are close-on-exec; stdin/stdout are private pipes and stderr is null.
- Only AF_UNIX socket/socketpair creation is allowed. IPv4/IPv6/packet/netlink/
  vsock cannot be opened through the ordinary socket ABI. Architecture checking
  blocks IA32 `socketcall`; explicit x32 aliases cover the filtered x86-64 syscall
  numbers. `io_uring_setup` and `pidfd_getfd` are denied, and no inherited ring or
  Internet descriptor is preserved across exec. Fork/exec inherit the filter.
- Jupyter uses filesystem Unix IPC. UID 1000 Bash is not filtered; the existing
  workspace operation gate and Bash cleanup remain important prerequisites.
  AF_UNIX does not by itself guarantee isolation: future reachable Unix services
  must not provide an unrestricted TCP relay or descriptor-passing capability.
- The root-owned gateway socket is group 1001/mode 0660 beneath a root-owned
  context directory. Every accepted connection is checked with `SO_PEERCRED`.
  The policy is immutable for the logical context; absence inherits the prior
  policy, while a conflicting explicit policy does not execute another cell.
- HTTPS is required on port 443 with an exact normalized allowed hostname, no
  userinfo/fragment, no IP literals, no CONNECT and no redirect following. DNS
  results are checked for public addresses and pinned into that request's TLS
  client; URL hostname/SNI/certificate verification remain enabled. Environment
  proxies are disabled. No concrete DNS-rebinding or Host/SNI bypass was found.
- User Host/Authorization/proxy/forwarding authority is rejected. Secret headers
  cannot be overridden using a different case and are injected only after exact
  origin validation. Raw upstream headers are not exposed; content type is
  reduced to a bounded MIME whitelist rather than returning arbitrary values.
  The authorized origin remains trusted with its own credential: it can return
  application data reflecting that credential. This is explicitly documented,
  not a claim that arbitrary origin response bodies can be perfectly redacted.
- Connection tasks are bounded before HTTP body parsing (eight); outgoing
  requests are bounded separately (four). Request/response bodies, DNS/connect/
  total request duration are bounded. Root error replies are fixed strings,
  not credential-bearing reqwest/URL diagnostics.

## Verification still required

No additional substantive bypass was demonstrated after source remediation.
The five real-image tests cover network-family denial, child inheritance,
unchanged Bash networking, io_uring/x32 denial and gateway policy rejection; the
positive public HTTPS probe is explicitly opt-in with
`RUN_EGRESS_NETWORK_PROBE=1`. The supplied thirteen offline bridge/client passes
are useful context, not proof that Linux seccomp, UID peer credentials or TLS
pinning execute correctly in the deployed image. Root GHA must execute the real
container journeys, including the secret-free IPC regression, before release.
No local privileged process, heavy Rust/Docker build, paid call or deployment was
performed during this review.

## Primary references

- [Linux seccomp filter](https://docs.kernel.org/userspace-api/seccomp_filter.html):
  syscall ABI filtering, inheritance and the distinction from a complete sandbox.
- [Linux no-new-privileges](https://www.kernel.org/doc/html/latest/userspace-api/no_new_privs.html):
  irreversible fork/exec inheritance; descriptor passing remains a separate risk.
- [Rust-VMM seccompiler 0.5.0](https://docs.rs/seccompiler/0.5.0/seccompiler/):
  match/default actions and architecture-aware filter compilation.
