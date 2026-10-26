# Self-hosted Responses Code Interpreter

This is the daemon/hosted-tool integration contract, not a compiled or deployed
acceptance receipt. Workers and Durable Objects remain Rust; Python is the code
execution payload and the mature Jupyter client inside the existing container.

## Ownership and lifecycle

- One managed Jupyter kernel supplies a persistent Python namespace for a logical
  `cntr_<uuid>` context. Repeated cells reuse it rather than launching unrelated
  `python -c` processes. NumPy, pandas and Matplotlib are installed in the image.
- The existing physical session container and operation gate are reused. Bash,
  imports, workspace checkpoint/restore, code execution and artifact capture cannot
  overlap. Losing the HTTP observer does not cancel accepted execution.
- The root daemon supervises UID 1001 for the interpreter; Bash remains UID 1000.
  Both clear supplementary groups and enable `no_new_privs`. The interpreter gets
  a deliberately rebuilt environment, never daemon authorization, provider keys,
  account tokens or R2 credentials.
- Between cells **all** interpreter-UID processes, including escaped process
  groups and background writers, are stopped through pidfds. Artifact hashing and
  copying happen only after this barrier. The kernel is resumed for the next cell.
  User code cannot signal root/Bash or restart itself while the entire UID is stopped.
- A timeout, cancellation of a running cell or broken kernel protocol kills that UID and expires the
  logical context. A stale explicit ID returns 404; it never silently starts a fresh
  namespace under the old identity. A subsequent auto context has a new ID.
  After the all-UID kill barrier and child reaping, root retires the expired
  context's inputs/runtime/data directory. Immutable unpublished artifact spool
  entries survive retirement. A failed barrier or cleanup retains an expired,
  unusable slot and blocks replacement until retirement succeeds; reset count
  does not impose a lifetime limit on a healthy physical container.

The logical code filesystem is separate from the Bash workspace generation. Its
in-memory Python state and scratch files are volatile, like a hosted code container;
they are not falsely included in filesystem workspace commits. Published artifacts
are account-owned Space files. Intentional container release must wait for known
artifact publication to finish; the harness owns this pending-publication fence.

## Files and artifacts

Imported Space inputs are immutable read-only copies at
`/mnt/inputs/<file_id>/content`. They can be up to 64 MiB each, with a 512 MiB total
and 128-file context limit. Their root-owned namespace prevents code from modifying
an owned Space reference or causing unchanged input datasets to be exported again.
Code can copy an input into `/mnt/data` when it needs a writable working version.

Generated regular files under `/mnt/data` are copied into a root-only spool after
the UID barrier. Each copy has a known length and SHA-256. Artifact identity binds
the logical context, raw relative path and content digest, so unchanged files across
cells reuse publication identity; changed bytes produce a new artifact. The spool
path never appears in HTTP results. Downloads cannot be changed by a later cell.

Limits are 16 MiB per exported artifact, 64 MiB and 128 artifacts per capture. An
over-limit or unsupported entry returns an explicit `artifact_error`; no file is
silently truncated. The live code context remains available for correction.
Paginated metadata lists every supported entry within the code-tree limits, marks
non-exportable entries, and provides an opaque raw-path cursor. A selected regular
file can be exported separately. Links/absolute/parent paths never read outside
the directory capability. After Space has durably accepted a copy, the control
plane acknowledges its spool object; this reclaims intermediate storage, not the
published user file.

Jupyter `stream`, `execute_result`, `display_data`, and Python exception messages
are collected through `BlockingKernelClient.execute_interactive`, with stdin
disabled. Raster images/PDF displays become files, not unbounded base64 JSON.
Logs have one combined UTF-8 byte budget and are explicitly marked truncated when
necessary. The Rust supervisor enforces the limit again across untrusted replies.
HTML is not evaluated or returned as executable display content.

## Private daemon HTTP interface

All endpoints use the existing session-scoped supervisor bearer token.

| Method/path | Result |
| --- | --- |
| `POST /v1/python` | `{id, container_id?, code, timeout_ms?, max_output_bytes?}`; one accepted cell, detached from HTTP lifetime |
| `POST /v1/python/contexts` | Prepare and pause the actual kernel under the admitted memory/network policy, returning its real `container_id` without executing a cell |
| `GET /v1/python/{id}` | Exact known result, 202 while active, or 404 |
| `DELETE /v1/python/{id}` | Signals only this active code execution; resets its logical kernel |
| `PUT /v1/python/files/{id}` | Known-length/SHA-256 input import; optional `X-Code-Container` selects an existing context |
| `GET /v1/python/files` | Metadata page (`container_id`, `after`, `limit` query options) |
| `GET /v1/python/source/{cursor}` | Capability-safe immutable export of one listed file |
| `GET /v1/python/artifacts/{id}` | Known-length immutable bytes plus `X-Content-Sha256` |
| `DELETE /v1/python/artifacts/{id}` | Idempotent publication acknowledgement |

Preparing a context lets the Worker durably publish the actual native item,
`code.delta`/`code.done` and interpreting phase before executing user code. The
context is paused on return; retries select the same live auto namespace. This
does not run a dummy cell or create a fabricated public container identity.

Cancellation also records an authenticated pre-dispatch receipt. A DELETE that
arrives before its POST prevents that cell from running; GET and delayed POST
return the same empty cancelled result, without inventing a created context or
destroying an existing healthy namespace. Cancellation and execution share the
supervisor's bounded 128-identity window. The DO's durable dispatch journal remains
the authority against older replays. Observing an existing dispatch never boots
a replacement VM first.

Code requests permit 1–65,536 source bytes, 1–300,000 ms execution timeout and
1–4,194,304 retained log bytes. The same gate covers startup and capture; caller
timeouts do not permit dispatch replay. The daemon keeps the last 128 accepted IDs
and known results in memory, as with Bash. These are not durable exactly-once claims:
the DO must fence dispatch, GET the same execution after a lost reply, and treat an
unrecoverable accepted outcome as unknown rather than submitting the code again.

Results retain `id`, opaque `container_id`, `outputs`, `artifacts`, nullable
`artifact_error`/`error`, `python_error`, `timed_out`, `cancelled`, `truncated` and
`duration_ms`. Python exceptions are ordinary cell results so the model can inspect
logs and correct code. Infrastructure/unknown-outcome errors are fixed machine codes,
not an invitation to retry effects. Hosted-tool adapters translate these into native
Responses `code_interpreter_call` items/events and publish artifact references.

## Verification and sources

`test_python_kernel_bridge.py` contains inexpensive output/protocol contracts and
does not require Jupyter locally. `test_workspace_interpreter.py` is the real image
gate: persistent variables across Bash/checkpoint, UID/key isolation, rich image and
binary roundtrip, immutable artifacts/ack, input integrity/read-only behavior,
result identity, error/output bounds, cancellation/stale ID and timeout reset.
Rust pure tests cover UID selection, immutable capability copies, path rejection,
metadata identity and root-enforced output limits. Container execution and privileged
process tests run in GitHub Actions, never against arbitrary host processes.

Primary references:

- [OpenAI Code Interpreter](https://developers.openai.com/api/docs/guides/tools-code-interpreter)
- [Jupyter messaging](https://jupyter-client.readthedocs.io/en/stable/messaging.html)
- [Jupyter BlockingKernelClient](https://jupyter-client.readthedocs.io/en/stable/api/blocking.html)
- [Linux pidfd_send_signal](https://man7.org/linux/man-pages/man2/pidfd_send_signal.2.html)
- [cap-std directory capabilities](https://docs.rs/cap-std/4.0.3/cap_std/fs/struct.Dir.html)

### IPython MIME compatibility

The Debian bookworm IPython 8.x `Image._data_and_metadata` uses
[`b2a_base64(self.data)`](https://github.com/ipython/ipython/blob/8.5.0/IPython/core/display.py#L942-L945),
which includes a trailing newline; newer releases disable that newline. The bridge
normalizes only ASCII MIME whitespace within a separate bounded transport budget,
then applies strict base64 validation and the unchanged decoded 16 MiB limit.
The cheap bridge regression reproduces the upstream encoder, MIME line wrapping,
and notebook-style string lists. Normal Matplotlib container acceptance remains
an actual file/image rendering test, not a synthetic replacement.

## Worker transport and publication fence

`workspace_interpreter.rs` is a child module of the existing Rust `Workspace`.
It uses its physical profile, compute lease/clock restoration, authenticated traced
container transport and transport deadlines; it does not change a session's Bash
pin or run a second agent harness. A requested `1g`/`4g` tier must match the actual
persisted runtime profile before restoration or compute admission. The private
policy includes typed domain secrets only on the authenticated root-daemon route;
no secret environment is exposed to Python.

Each stable caller execution ID maps to a daemon-safe SHA-256 ID. The Worker GETs
that ID before dispatch. A durable dispatch record stores the physical boot and
request digest, not source or secrets. Once dispatch is recorded, missing status,
transport loss, reboot or changed arguments never authorize another POST. Poll and
cancel do not start replacement VMs. Known daemon preadmission rejection releases
the empty publication fence; uncertain admission remains fenced for reconciliation.

`workspace:python-publication-pending` is a serialized set of hashed execution IDs.
The Session owner serializes all operations and clears an entry only after the
canonical Space artifacts and daemon spool acknowledgements commit (including a
known terminal empty-artifact result). When the final entry clears, the key is
**deleted**, not replaced with an empty set. Physical destroy refuses this key;
Bash checkpoints remain allowed. Selected source exports use the same fence with
a caller-provided stable publication execution ID.

Input imports re-fetch an owner-scoped processed Space row and use its authoritative
`content_key()`, supporting both direct files and generic immutable blob-backed
files. The caller retains the ordinary attachment pin/strong reference for the
operation. R2 GETs use the existing reservation and operation metering, stream at
most 64 MiB to root-managed readonly imports, and verify daemon length/hash/path
receipts. Generated artifacts are immutable opaque IDs; Worker downloads verify
reported length and SHA-256 with a streaming 16 MiB cap. Filenames never form
transport URLs or R2 keys. Session publishes using the existing idempotent Space
publisher and original logical container/artifact identity, then acknowledges the
root spool. Neither side receives the other's provider or R2 credentials.

### Cancellation and known-result drainage

The endpoint persists the Response cancellation flag, then invokes
`SessionInner::cancel_current_hosted_code` outside the long-running step gate.
That hook derives the existing unresolved tool execution, reads its operation and
only records/sends cancellation; it never restores a VM, imports or submits code.
The root step must redispatch this unresolved Code operation before terminalizing
the cancelled run, keeping its resource reservation open for artifact publication.

An unsubmitted operation returns a durable cancelled result without imports or a
new cell. An already submitted operation with no known outcome sends cancellation
and polls exactly that execution; missing status is not permission to replay. Once
a terminal outcome is known, cancellation does not bypass its Space publications,
reference commits, spool acknowledgements or publication-fence cleanup. Response
deletion suppresses canonical output publication only after this cleanup; it does
not trap the operation permanently in `Pending`.

The Workspace stores `workspace:python-cancel:<hashed-id>` before best-effort daemon
DELETE, including while bootstrap is in flight. Submission checks it after startup
and immediately before POST. A durable `cancelled_before_dispatch` receipt records
local proof that a reserved dispatch never POSTed; legacy/unknown dispatch records
never acquire that classification merely because cancellation was requested.
The supervisor must additionally retain preadmission cancellation tombstones for
unknown stable IDs: remote DELETE can beat the final HTTP POST after the Worker's
last local check. A local flag alone cannot make remote admission atomic.
