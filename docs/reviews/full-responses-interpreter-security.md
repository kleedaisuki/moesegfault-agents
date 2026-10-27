# Persistent Code Interpreter security review

Reviewed foundation commit `e4bf137f2370894fb555431650a4ce6c1ca2a576` and the
surrounding daemon, Python bridge, Docker image and real-container tests.
This review is independent of the implementation author. It does not constitute
a successful Docker/GHA execution receipt.

## Finding: expired contexts exhaust the warm daemon after eight resets (P2)

`crates/workspace-daemon/src/interpreter.rs`, `Interpreter::create_context` counts
all `cntr_*` directories against `MAX_CONTEXTS = 8`. The error branch at the end
of `Interpreter::execute` kills the interpreter UID and clears the context slot,
but leaves that context's data, runtime and input directories on disk. Timeout,
cancellation and broken IPC therefore permanently consume slots. After eight
such resets in one physical container, even a fresh auto-context request returns
`code_container_unavailable`; no currently live context needs to exist. Old
imports and scratch files also retain physical storage without a live owner.

Reproduction in the real daemon: repeatedly submit an auto-context cell that
sleeps longer than its short timeout, wait for each known timeout result, then
submit `print(42)` after the eighth reset. The final request must succeed in a
new namespace but currently fails until the whole physical container restarts.

Remedy: after successful all-UID termination, destroy the expired logical
context's data/runtime/input directory, independently of the immutable root-only
artifact spool. Preserve pending unpublished artifact bytes. Apply a limit to
live contexts rather than accumulating historical directory names. Add a
real-container journey exceeding eight resets and verify that a previously
captured, unacknowledged artifact remains readable. The implementation owner is
addressing this finding; independent re-review is pending.

## Boundaries examined

- Daemon requests authenticate before handlers using a session bearer digest and
  constant-time comparison. The Python process receives a rebuilt environment,
  not daemon authorization or model/R2 credentials.
- Both Python bridge and kernel run as UID/GID 1001 after supplementary groups
  are cleared and `no_new_privs` is set; Bash remains UID 1000. Full root
  `setuid` changes real, effective and saved identity. No retained-root identity
  was found in this path.
- All interpreter-UID processes, not merely the Jupyter process group, are
  frozen through pidfds before capture and killed on reset. pidfds identify the
  process rather than a reusable numeric PID. Paths are inspected while the UID
  is stopped; symlinks and nonregular artifact entries are rejected, and cap-std
  directory handles constrain traversal.
- Immutable artifact copies are root-only spool files, and imported inputs are
  root-owned read-only files beneath a nonwritable namespace. Public artifact
  metadata exposes opaque IDs, not spool paths. Selected export rejects parent,
  absolute and symlink components.
- The root supervisor rechecks reply identity/count, log budgets and artifact
  lengths; JSON replies have a 32 MiB read bound. Timeout/cancel/IPC error does
  not silently execute under the same expired logical context identity.
- Actual physical memory SKU selection and the requested 1g/4g capability check
  belong to the pending Worker admission integration. This daemon slice does
  not establish 16g/64g allocation support; tests must not imply otherwise.

No additional demonstrated cross-UID credential leak or path escape was found
in this bounded source review. The privileged Docker journeys remain necessary
for Linux process-quiescence and actual image acceptance.

## Primary references

- [Linux setuid](https://man7.org/linux/man-pages/man2/setuid.2.html): complete
  root privilege drop and mandatory return-value checks.
- [Linux pidfd_send_signal](https://man7.org/linux/man-pages/man2/pidfd_send_signal.2.html):
  stable process identity and PID-reuse protection.
- [cap-std Dir](https://docs.rs/cap-std/4.0.3/cap_std/fs/struct.Dir.html): filesystem
  directory capabilities used by the implementation.

## Remediation re-review

The uncommitted follow-up adds `ContextState.expired` and `Interpreter::retire`.
It marks a namespace unusable before cleanup, requires successful all-UID kill,
reaps the bridge, deletes only the validated `cntr_<uuid>` scratch root, then
clears the slot. A failed barrier/cleanup retains the expired slot; automatic
requests must retry cleanup before starting another namespace. Explicit selectors,
listing and export do not expose an expired context. The lifetime directory
counter is removed because only one context can be active in this interpreter.

Source-level re-review finds this remedy addresses the reported exhaustion and
retains the independent immutable artifact spool. The new pure retirement test
covers twelve cleanup cycles, idempotent removal, invalid parent components and
pending spool preservation. A cleanup symlink-containment assertion was requested
from the owner. The requested real-container regression exceeds eight resets and
checks that an unacknowledged old artifact remains readable. GHA execution is
still required; no local privileged process or container test was run by reviewer.

The final source recheck confirms both cleanup symlink regressions are now present:
twelve contexts contain a symlink to the outside pending spool, whose sentinel
survives; a valid-looking context-root symlink to the spool is rejected. No
additional substantive findings remain from this bounded review. The finding is
source-resolved, pending the queued GHA/Docker regression evidence.
