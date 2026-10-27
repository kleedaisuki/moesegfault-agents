# Observability and diagnostic data

## Two trace planes, one investigation

Cloudflare Workers tracing owns platform span IDs and instruments fetch, Durable Object,
D1 and R2 operations. Enable `observability.traces.enabled` and structured Workers Logs
in Wrangler. Rust calls the platform custom-span API through the small `wasm-bindgen`
bridge in `crates/agents-worker/src/telemetry.rs`; this is not a second Worker or harness.
The current worker-rs 0.8.7 `Context` does not expose `tracing`.

`NativeSpan` ends its platform span on drop, including error returns. It uses
`startSpan`, so it measures an operation under the current native invocation without
pretending that later fetch operations are its children. Native automatic fetch spans
retain platform-determined ancestry. Cloudflare currently exposes neither
`spanContext()` nor manual parent wiring: application-generated IDs must **not** be
written as native trace/span IDs.

`TraceContext` separately supplies W3C application correlation across request,
session and trusted workspace hops. Invalid contexts start a fresh application trace;
valid contexts preserve the trace ID and sampled bit, generating a new operation ID.
Each outgoing internal operation uses `child()` and `inject()`; never reuse one span ID
for independent operations. The parser checks lowercase hex, nonzero IDs, delimiters,
version-00 exact length, forbidden version ff, and a 512-byte upper bound. It accepts
forward-compatible versions using the version-00 fields, ignoring extension semantics.
Unknown sampling flag bits are not propagated. No `tracestate` or baggage is forwarded.
ProviderConfig explicitly opts into validated `traceparent` propagation to the configured
provider; the session harness supplies a fresh child for each call. This shares only the
opaque application correlation IDs, not owner/session IDs, credentials or baggage. Other
arbitrary user URLs must not receive tracing context. Provider redirects are forbidden.

Record `app.trace_id` on native custom spans and `trace_id` in sanitized events. These
are correlation keys, not a claim that platform and application trace graphs are the
same graph. A platform trace and its attributed console events provide the join.

## Content-free events and provenance

Use `telemetry::emit`, never raw user/provider errors or arbitrary JSON in console logs.
The emitter drops unknown keys, arbitrary nested structures, negative counters and
free-form content. The one-level `observation` object is recursively restricted to
the same typed metadata schema. Signed MCP protocol error codes are not counters:
retain their real negative values. Bounded Content-Type parameters and numeric
cost observations have explicit validators rather than header-like blanket string
rejection. Event names have a fixed allowlist, with unknown names becoming
`operation`; the bounded `tool.image_generation.*` family is also recognized.

Useful event sequence:

```text
request.completed -> run.started -> provider.completed -> tool.completed
                                  -> session.checkpoint -> run.completed
                                                        -> session.archived
```

Failure paths use stable `error_code` values, not exception messages. The sanitizer
also accepts bounded counters such as `attempt`, `turn`, and `sequence`; acceptance
does not mean that every producer emits those fields. Do not infer complete retry
or event-sequence coverage from the sanitizer allowlist. Unknown usage must not
become a zero-cost observation.

The implemented provenance chain uses the following sources:

| Boundary / event | Fields actually emitted | Source |
| --- | --- | --- |
| Deployment render | `BUILD_ID`, `DEPLOYMENT_ASSEMBLY_SHA`, `DAEMON_IMAGE_SOURCE` | Validated Worker artifact source, assembly source and independent exact daemon artifact source; identical reused components must not be relabelled with the assembly SHA |
| `GET /healthz` | `status`, `build_id` | Worker `BUILD_ID`; absent/invalid local configuration returns `unknown`, never arbitrary environment text |
| `run.started` | `agent_id`, `session_id`, `run_id`, `trace_id`, `build_id`, `config_revision`, `prefix_hash`, `registry_version` | Owned session record, active run, application trace, deployment variable, and immutable pinned context |
| `provider.completed` | `session_id`, `run_id`, `trace_id`, `round`, input/output/cache tokens, `usage_reported` | Persisted provider completion and its owning run |
| `tool.completed` / `tool.failed` | `session_id`, `run_id`, `trace_id`, `round`, `tool_call_id`, `tool_name`; failure also has a stable error code/outcome | Active completion/tool dispatch; run plus round disambiguates provider IDs reused by later completions |
| `session.checkpoint` | `session_id`, optional `run_id`, `snapshot_id`, `workspace_revision`, `snapshot_bytes` | Observed change in committed snapshot metadata after a tool or archive checkpoint; archive maintenance has no active run |
| `session.archived` | `session_id`, `agent_id`, `build_id` | Completed session archival and current Worker deployment |
| `observer.recovery` | `session_id`, `run_id`, `trace_id`, `attempt`, `duration_ms`, `success`, fixed `outcome`/`error_code` | Rare Chat canonical-tail reconstruction or explicit observation failure; not proof of network delivery |

`prefix_hash` is the existing SHA-256 of canonical serialized `PinnedContext`: it
covers pinned model/budgets, instructions, resolved tools/skills, agent revision,
and compaction/workspace settings. It identifies the complete pinned configuration,
not provider-rendered token/KV bytes and not conversation history. Hashing occurs at
run start, not on every provider token. There is no separate emitted `config_hash`;
that name is only allowlisted, and must not be presented as implemented coverage.
`registry_version` comes from the pinned tool versions, not the currently deployed
registry: it is their common version, `mixed` when versions differ, or `none` for an
empty tool set. Individual pinned skill content/versions are covered by the hash.

An operator can start with `run_id`, find its `run.started`, identify the Worker Git
commit and pinned context digest/revision, then join provider/tool events and any
snapshot metadata by run/session IDs. An archived workspace is joined through its
session ID and snapshot digest/generation. The health endpoint identifies the
current deployment; a run-start event identifies the deployment that actually
admitted that run, which can differ after a rollout. No additional provenance table
or history copy is introduced. Events remain sampled operational observations,
can repeat after retries, and are not authoritative financial or execution records.
IDs and hashes are not authorization: retrieval always checks the authenticated owner.
Never emit prompts, instructions, skill contents, file paths, bash commands, tool
output, full provider responses, BYOK credentials, cookies or authorization headers.
Avoid identity subjects in operational logs; ownership belongs in authorized records.

## Storage and retention boundaries

| Class | Storage | Lifetime and access |
| --- | --- | --- |
| User conversations, pinned configuration, workspace snapshots | Session DO, D1 metadata, workspace R2 | User-data policy; owner-authorized export/delete; not telemetry cleanup |
| Recovery/checkpoint/intermediate state | Explicit recovery namespace | Remove superseded/unreferenced generations after successful commit; preserve latest durable recovery state |
| Original paid provider HTTP bodies | Encrypted chunked `WORKSPACES` private receipt namespace; actor-owned descriptor | Execution recovery, not diagnostic TTL data. Preserve unpublished paid bytes on parse/decode/archive failures; explicit funded cleanup only after durable publication or authorized abandonment |
| Operational logs and platform traces | Cloudflare Observability | Sampling and provider retention configured by deployment; no content payloads |
| Consented client diagnostics | Separate `DIAGNOSTICS` R2 binding, `diagnostics/` prefix | Seven-day lifecycle deletion; support/operator access only, separate from workspace credentials |
| Billing/audit ledger | D1 billing records | Financial retention policy; never delete as temporary telemetry |

Do not place the diagnostics bucket under a public R2 URL. Apply an R2 lifecycle rule
to delete the `diagnostics/` prefix after seven days; `expires_at_ms` is metadata, not
an automatic deletion mechanism. Deployment must enforce the rule. Keep observability
retention and sampling independent from user data retention; disabling telemetry must
not erase conversations, and cleaning old snapshots must not delete the current one.

## Client diagnostics contract

`POST /v1/diagnostics` is authenticated and opt-in **per submission**. The HTTP handler
must reject a body over 16 KiB before JSON decoding (check Content-Length and enforce
a stream byte cap; Content-Length alone is not sufficient). Rate-limit it using the
authenticated account, not a client-supplied owner or request ID. Missing diagnostics
storage is an explicit unavailable response, never a fallback into workspace storage.
`receive_diagnostic` enforces five validated submissions per rolling hour with an
atomic D1 insert-and-count predicate. `diagnostic_reports` contains only an empty JSON
placeholder plus owner/time/ID references, never the client payload. These references
expire after seven days and require scheduled D1 cleanup. Failed R2 uploads retain
their rate reservation, so repeated backend failures cannot bypass the limit.

```json
{
  "consent": true,
  "client": "magents",
  "version": "0.1.0",
  "platform": "linux-x86_64",
  "error_code": "connection_failed",
  "request_id": "req_example"
}
```

Only `magents` and `adash` are accepted clients. Version/platform/error-code fields
are bounded machine tokens. Unknown fields are rejected, including logs, stack traces,
environment variables and attachments. A successful submission receives a server-generated
diagnostic ID. The stored record includes authenticated ownership, receipt/expiry time,
schema version and the exact validated consent decision. This is deliberately a narrow
support signal, not a general remote log upload endpoint. The CLI must show the exact
payload and obtain affirmative consent before making the request.

## Verification and operational checks

Module tests cover invalid/non-ASCII trace headers, zero IDs, future versions, child
correlation, unknown-field redaction, negative counters, diagnostic consent, validated
build IDs, and content-free run/checkpoint metadata. Deployment-render tests assert
that staging and production receive the exact image SHA as `BUILD_ID`, preserve
other variables, and reject malformed/missing identities. A real
deployment smoke test should start a session, execute a workspace command, checkpoint,
archive and restore. Use its application trace ID to find the native Worker/DO/storage
operations. Check that a recognizable secret in prompt/command/output does not appear
in console event payloads or diagnostics. Verify lifecycle deletion with a short-lived
test prefix before enabling production retention. Never test by uploading real keys.
For a deployment, verify `/healthz.build_id` matches its checked-out Git SHA, then
inspect a real `run.started` and its joined provider/tool/snapshot references. Unit
tests and a WASM check do not establish that Cloudflare has ingested those events.

### Live staging acceptance — 2026-10-24

A bounded test against `agents-staging.moesegfault.dev` first verified the exact
health build `a918c6730cf95fcb66e3ec360b4d68f64eb091f8`. It used the secondary test
account, one new agent/session, and exactly one user turn with the registered
`deepseek/deepseek-v4.1-flash-1017` model. The model called `time` once and completed
after two upstream completions. Both owned resources were normally closed/deleted
with HTTP 200; no primary-account work was touched.

Cloudflare live `wrangler tail --env staging --format json` observations established
the following content-free chain, not merely local emitter/unit-test behavior:

| Observed event | Count | Verified correlation / provenance |
| --- | ---: | --- |
| `request.completed` | 7 | This test's application `trace_id`, status and duration |
| `run.started` | 1 | Same app trace; owned agent/session/run; exact deployment build; revision 1; 64-hex `prefix_hash`; pinned registry `2026-10-23.1` |
| `provider.completed` | 2 | Same trace/run/session; rounds 0 and 1; `usage_reported=true`; bounded token counters |
| `tool.completed` | 1 | Same trace/run/session; `tool_name=time`; tool-call ID and round 1 |
| `run.completed` | 1 | Same run/session; `success=true` |
| `session.archived` | 1 | Same session/agent; exact deployment build |

The app trace was `98e0c976a63044bfa7710cb1d2a695a8`; the run was
`run_bf0683b3d8d441c5b5008712d6b53d44`. Provider/tool events join to the pinned
context digest and build through `run.started`; the archive joins through the owned
session, not a nonexistent archive `trace_id` field. Request and completion logs can
arrive in different tail batches, so the table is a causal join, not an assertion of
transport ordering. The tool's emitted round is 1 following provider round 0; equal
provider/tool round numbers must not be assumed to identify the producing completion.

The collector disabled Wrangler disk logging with `WRANGLER_WRITE_LOGS=false` and
metrics with `WRANGLER_SEND_METRICS=false`, parsed tail objects only in memory,
discarded invocation requests/headers/query/body/raw exceptions, selected only this
test's app trace or owned session, and persisted only explicit allowlisted event
metadata. A unique private marker in instructions and user input did not appear in
the selected raw structured console-event payloads; no unexpected metadata fields
were observed. No credential, message, model answer or tool result was persisted.
Sanitized local evidence is `.cache/telemetry/a918c67-events.json` and
`.cache/telemetry/a918c67-acceptance.json`; these caches are not durable user records.

Scope: this establishes real Cloudflare ingestion and application provenance joins
for the request → run → provider/tool → archive journey. It does not establish
native Cloudflare trace-UI parentage or equate the application trace ID with a
platform span/trace ID. This time-only run did not allocate a workspace or change a
snapshot, so no `session.checkpoint` event was expected or claimed. R2 lifecycle
expiration and diagnostic-upload retention were not exercised by this bounded test.

### Passive workspace-checkpoint acceptance — 2026-10-24

On build `164d0aec3ea9404ef9b94f58847d775caacaa46e`, a separate content-safe global
tail observed two existing idle/long-task acceptance sessions, C and D. The collector
read only their session/agent identifiers from the test owner's fixture metadata,
read no API keys, created no resources or model turns, and made **zero session HTTP
or CLI calls**. It therefore did not renew their activity or interfere with the
owner's no-polling idle windows. Subscription ran from 04:57:56 to 05:08:47 UTC and
stopped once both sessions' checkpoint/archive events had arrived.

| Session | Checkpoint UTC | Snapshot SHA-256 | Generation / archive bytes | Archive UTC |
| --- | --- | --- | --- | --- |
| C — `22bb5edf-84e0-49b2-b5a8-5a63a263f93f` | 05:05:21.262 | `f6cde33a44641dcfb15612a67dae32529a41dd08a23a969894907c0134eacd76` | 1 / 364 | 05:05:22.707 |
| D — `d442190c-f32d-4b29-abb3-e54ba61d30c2` | 05:08:40.551 | `e4dae40fce3ca654bef56a6d99fd216f7b9c6b4d644ac6bef9290cb21c58340f` | 1 / 366 | 05:08:41.826 |

For both sessions, live `session.checkpoint` contained `session_id`, `snapshot_id`,
`workspace_revision` and `snapshot_bytes`. The subsequent `session.archived` matched
the owner's session/agent identifiers and exact build above. These are maintenance
checkpoints: they correctly omitted active `run_id`, `trace_id` and `build_id`; build
and agent provenance is joined through the matching session archive, not invented
as extra checkpoint fields. D's observed provider/bash events and successful
`run.completed` shared `run_8e13a6898c6b46179fbbaad2e05e6b8e` and its application
trace; the run-start event preceded this subscription and was not claimed captured.

This supplements the earlier time-only test with actual workspace snapshot-reference
ingestion. The watcher also saw the owner's later C restore/close traffic; its first
checkpoint/archive pair above is kept distinct from that later archival. Events can
arrive out of timestamp order across Cloudflare tail batches. No unexpected event
fields were observed. Request/headers/body/raw exceptions were discarded in memory,
Wrangler disk logging remained disabled, and only allowlisted C/D event fields were
persisted in `.cache/telemetry/164d0ae-checkpoint-events.json`; the checked joins and
zero-request observation scope are in `164d0ae-checkpoint-watch.json` beside it.

This verifies archive-time snapshot metadata and content-free provenance joins, not
workspace archive contents, ten-minute timing semantics independently of the owner's
test, native Cloudflare trace-UI ancestry, or R2/diagnostic retention expiration.

## Preserve useful evidence through the operator boundary

An error label alone is insufficient diagnosis. Image reception distinguishes
send, headers, bounded body read, encrypted retention, JSON parse, image decode,
validation and publication. Keep the upstream status/request/generation IDs,
received bytes, hash, Content-Type, duration, dimensions, expected/actual format,
parser position and known provider cost independently of the final local result.
Paid responses are retained before decoding in the owner/operation-bound encrypted
runtime-recovery plane, not dumped into global logs or mixed with user Space files.
A local publication failure cannot turn known HTTP 200 completion into a free or
failed supplier operation. Missing headers remain explicitly unknown/null.

MCP emits `mcp.transport` and `mcp.discovery_failed` with method, protocol,
server label, discovery stage, status, signed protocol error code, typed cause,
parser position, Fetch-visible response bytes and request/ray IDs. Fetch-visible
bytes are not an invoice measurement of compressed network traffic. Real server
catalog descriptions may contain ordinary newlines; descriptions and credentials
stay out of logs, but their validation stage and structural failure code do not.
`resource.d1.batch` attributes actual business rows and funding/observation/RPC
time without pretending accounting round trips are business SQL.

Collectors must preserve these producer records rather than silently reducing them
back to a generic error. Establish exact owned invocation/trace/session correlation
first, then reuse the **deployed** `telemetry::sanitize_fields_at` vocabulary for
bounded structural fields, including its one-level observation object. Keep the
actual `tool.image_generation.*` and MCP event names; an obsolete second allowlist
would erase the very evidence added by the producer. Preserve Content-Type with
charset, negative JSON-RPC codes and bounded locations such as `chunk=23`.
Do not collect arbitrary headers, prompts, descriptions, bodies or credential text.
Distinguish operation/execution/frame/request/span identities when reducing events
from the same millisecond; session/run alone is not an event identity.

The staging operator's offline crafted-record probe covered nested image phase,
HTTP 200, reported cost, parser line, charset, chunk location and negative MCP code
retention while rejecting an unowned trace and credential/prompt fields. It opened
zero network clients. Real subsequent DeepWiki traces retained modern rejection,
legacy initialize/notification/list and the one approved tool's HTTP 200/316 ms
completion, rather than losing all details behind `mcp_discovery_failed`.
The reusable policy is the producer schema plus exact ownership, not those one-off
collector files or a claim that old discarded response bodies can be reconstructed.

## Sources and engineering rationale

- [Cloudflare custom spans](https://developers.cloudflare.com/workers/observability/traces/custom-spans/):
  native async ancestry and current ID/manual-parent limitations; checked 2026-10-23.
- [worker-rs Context source](https://docs.rs/worker/latest/src/worker/context.rs.html):
  SDK binding capabilities; local downloaded 0.8.7 source also inspected.
- [W3C Trace Context](https://www.w3.org/TR/trace-context/): validation and propagation.
- [Workers Logs](https://developers.cloudflare.com/workers/observability/logs/workers-logs/):
  platform log ingestion and operational retention.
- [Dapper](https://research.google/pubs/dapper-a-large-scale-distributed-systems-tracing-infrastructure/):
  production research supports low-overhead sampling and shared-boundary instrumentation.
- [Uncertainty in Aggregate Estimates from Sampled Distributed Traces](https://research.google/pubs/uncertainty-in-aggregate-estimates-from-sampled-distributed-traces/):
  sampled trace aggregates have uncertainty. Billing must use the durable usage ledger,
  never extrapolation from sampled logs or traces.

The practical design principle is explicit correlation, not heuristic reconstruction:
carry a bounded context on trusted hops, use native tracing for platform operations,
and persist content only in its authorized data plane. This keeps tracing overhead
small and prevents a convenience logging path from becoming a second user-data store.

## v0.2 observer and CI correlation

`observer.completed` reports whether the run-bound channel accepted its terminal
frames, not acknowledgement by a remote client. `session.projection_pending`
distinguishes completed authoritative data from a lagging D1 status projection.
These fixed event names and existing sanitized fields contain no exception text,
conversation content or credential material.

Staging acceptance can opt in with `MAGENTS_TEST_TRACE_SEED=run_id:run_attempt`.
The test client derives a stable trace ID, uses fresh span IDs and flags `00`,
and changes only transport headers. Native sampling remains independent. Public
CI-derived trace IDs are correlation hints, never authentication or ownership.

One bounded live window for deployment 40100946077, attempt 1, correlated both
main compaction runs on exact `265e0ea`, including observer/run success and the
post-compaction input dropping to 385 tokens. It ended after Actions success and
a twelve-second drain. No model probe was added and raw tail output was not saved.
The received subset had no native exception, but coverage began after SDK startup;
that observation does not prove that every unmatched invocation was error-free.
