# Caller-executed native tool families

This slice extends the existing canonical Session pending-call lifecycle. It is
not a second executor and does not claim that a client computer or local shell
exists inside our Container.

| Declaration | Returned call | Required caller result |
| --- | --- | --- |
| `computer`, `computer_use_preview` | `computer_call` | `computer_call_output`, matching `call_id`, screenshot and required safety acknowledgements |
| `local_shell` | `local_shell_call` | `local_shell_call_output`, **`id` equal to the original `call_id`** |
| `shell` with `environment.type=local` | `shell_call` | `shell_call_output`, matching `call_id`, stdout/stderr and typed exit/timeout outcomes |
| `apply_patch` | `apply_patch_call` | `apply_patch_call_output`, matching `call_id`, completed/failed and optional text |

The local-shell identity rule follows the current official SDK and guide, not
the newer shell result schema. We never manufacture a `call_id` field for that
legacy result.

## Authority and state

`client_tool_items.rs` defines bounded action/result validation and public field
projection. `GeneralPendingCall` retains call identity plus small safety/caller
metadata; it does not duplicate shell actions or potentially large patch text.
That metadata is private persisted execution state, omitted from public output
and redacted in Debug. Existing function/custom snapshots omit the new defaulted
field, preserving old serialization.

Before continuing an owned current or historical Response, the existing Session
checks that every required result appears exactly once with the correct family
and identity. A function result cannot satisfy a computer, patch or shell call.
Computer safety acknowledgements match original check IDs; optional code/message
fields, if supplied, must match. Explicit nullable acknowledgements are equivalent
to omission, not permission to bypass pending checks. Caller-created results do
not grant execution rights or a new model/provider context.

Computer screenshot file references pass through the same owner-scoped,
size-budgeted attachment resolver used for generation and read-only auxiliary
preparation. The provider receives frozen inline bytes, never a Space file ID
belonging to another owner. Uploaded files, data URLs and public HTTPS images use
one renderer. Public HTTPS
sources are frozen through the existing anonymous image asset transport: no
credentials, redirects or private origins, bounded bytes and verified raster
headers/dimensions. Generated-image asset origin pins remain independent.

## Provider and execution boundaries

These declarations require a native Responses transport and a capable model.
They are rejected before model reservation on legacy transports or default
DeepSeek native routes that do not implement the requested built-in family.
Compatible explicit BYOK native routes retain the real tool declarations.
Hosted shell environments are not reinterpreted as local execution: they need
the workspace service adapter and its actual resource/network policy.

Public sanitization retains typed fields and removes unknown vendor extensions
both in incremental item events and terminal output. Known fields with invalid
types, such as object-valued timeouts, are rejected rather than echoed. Nullable
shell limits/environment remain nullable; no timeout or allocation is invented.

`allowed_callers=programmatic` requires a causal program-execution registry; an
untrusted model-supplied `caller_id` is not such a proof. Until that registry is
bound, the declaration fails before inference and a returned program caller is
not authorized. Preserving typed caller metadata is not programmatic execution.

Steering uses the same typed required-input stubs without synthesizing outputs,
approval decisions or computer safety acknowledgements. The official current
steering union does not define legacy local-shell stubs, so that execution mode
has an explicit steering capability error; ordinary local-shell continuation
remains supported.

## Incremental verification

Pure contracts cover local-shell identity, family mismatch, safety checks,
nullable acknowledgements, malformed limits/outcomes, nested field privacy and
early caller metadata. The actual Worker test is
`tests/worker/client-tool-families.test.mjs`: four SDK handoff/result/historical
fork cases with controlled native providers, negative pre-inference checks,
idempotent result replay, and a cross-owner screenshot rejection.

Select one changed family with `MAGENTS_CLIENT_TOOL_CASE=computer|local-shell|shell|patch`.
Set `MAGENTS_CURRENT_SCHEMA_PYTHON` to the project interpreter containing
`tests/e2e/requirements-current.lock`. Captured raw canonical Responses and SSE
events pass through the current SDK's bounded stdin `response-wire` validator;
SDK-added parsed/convenience fields are not used as server evidence.
Receipts identify the actual Wasm hash and executed families. Other previously
passing families need not run again. No external model call is made by this
fixture. Source readiness and pure-contract passes do not establish compiled
Worker acceptance; the latter is recorded only after those journeys execute.

Primary contracts:
[official shell guide](https://developers.openai.com/api/docs/guides/tools-shell),
[legacy local shell guide](https://developers.openai.com/api/docs/guides/tools-local-shell),
[apply patch guide](https://developers.openai.com/api/docs/guides/tools-apply-patch),
and [official SDK types](https://github.com/openai/openai-node/blob/main/src/resources/responses/responses.ts).

## First compiled execution

Source `86a7e7a7c4e258a8cc14c464c14a3f326436c4e1` built in Actions run
`40310608529`, Worker artifact `12645310389`, Wasm SHA-256
`b34a37c5732549373a29bdb0c5b9736b5a52cb0f95615196264311004ba8279e`.
Its new Rust, native and container checks passed. The first client journey
stopped on fixture accounting/upload issues, not a tool execution replay.

The same retained Wasm subsequently passed all four actual local workerd
handoff/continuation/fork cases with the current Python SDK 3.37.0. Local-shell,
shell and patch passed after joining ledger ownership through the reservation;
only the remaining computer case was rerun. The computer fixture now sends the
required raw-upload checksum and uses a PNG with a valid IDAT checksum. Redirect
denial is checked by zero requests to either redirect target, not by the new
Request object's default mode at the outbound-service boundary. No production
validation was relaxed and no external provider/model was called.

These are controlled native-provider and real Rust-runtime results, not a live
computer, real BYOK-provider acceptance or a passing aggregate Actions job.
Detailed outputs remain under this topic worktree's `.temp/client-86-*.log`.
