# v0.3 Responses, Space and workspace versions

This is the implementation contract for v0.3.0, not an acceptance receipt.
The existing [session runtime](session-runtime.md), [workspace protocol](workspace.md)
and [data lifecycle](data-lifecycle.md) remain authoritative where unchanged.
Implementation gaps must be tracked and closed; this document does not imply that
an endpoint already serves production traffic.

## Scope and representative workflow

An application uploads a file to its account Space, submits it to an agent through
the normal OpenAI Responses SDK, receives text and/or local function calls, returns
local function results, and retrieves the completed response after disconnecting.
The agent can persist its workspace, find older account-wide versions, and restore
one without discarding uncommitted current files. The dashboard and CLI manage the
same Space and immutable versions. Chat Completions continues unchanged.

Arbitrary historical conversation forks and workspace forks are explicitly out of
scope. Native hosted OpenAI products are not implied by wire compatibility: reject
unsupported tools/modalities rather than pretending to execute them. Relatively
complete compatibility includes meaningful input items, local functions, generation
options, streaming, background execution, retrieval, cancellation and deletion, not
merely a `responses.create()` alias for the old user-text-only endpoint.

## Ownership and identity

| State | Authority | Identity and lifetime |
| --- | --- | --- |
| One Space per account | D1 | Unique authenticated account; lazily ensure idempotently |
| File metadata and publication state | D1 | Opaque file ID, owner, Space, immutable content key; visible only after committed upload |
| File bytes | Private R2 | Immutable object; rename changes metadata, not bytes |
| Workspace version metadata | D1 | Opaque version ID, owner Space, source session, archive digest and size |
| Workspace version archive | Private R2 | Committed immutable bytes; no user update/delete endpoint |
| Response routing | D1 | Owner + response ID -> session ID; routing only, not canonical result |
| Response request, output and terminal state | Session DO | One response ID per accepted execution boundary, durable before disclosure |
| Linear conversation head and pending local calls | Session DO | Atomically checked with accepting the next response |
| Active inference view | Session DO | Compactable, distinct from source journal and response snapshots |
| Input file attachment receipt | Session DO | Owner-validated content identity; durable before admitting execution |
| Usage, reservations and retained storage clocks | D1 ledger | Existing immutable financial provenance; never telemetry-derived |

Neither a response ID, file ID nor version ID grants access. Resolve the
authenticated owner before routing or returning bytes. Non-owner reads return the
same 404 as a missing ID. Never expose private R2 keys, container tokens or upstream
credentials in public objects or tool results.

A session remains one linear mutable workspace context. A response represents a
specific run and fixed input/output boundary, not "whatever the session now says".
Old response retrieval never changes after a later run, steering or compaction.

## Responses HTTP surface

| Method | Path | Contract |
| --- | --- | --- |
| POST | `/v1/responses` | Create a response; standard JSON or typed SSE |
| GET | `/v1/responses/{id}` | Retrieve the exact stored response, not the latest session result |
| DELETE | `/v1/responses/{id}` | Tombstone the stored response representation; do not delete its session, files, workspace, accounting or other responses |
| POST | `/v1/responses/{id}/cancel` | Request cancellation and return current response state; successful acknowledgement is not proof an external side effect was reversed |
| GET | `/v1/responses/{id}/input_items` | Ordered input-item page with `limit`, `order`, `after` and `before` |

`model` continues to name an owned agent configuration. Creation without
`previous_response_id` creates a fresh session unless an explicitly documented
`X-Agents-Session` binding selects its current eligible head. The response returns
that header for existing tools, but ordinary Responses continuation needs only
`previous_response_id`.

The DO checks owner, agent, current head, current run and input revision together
before durable acceptance. Concurrent children cannot both be admitted. A stale
parent returns 409 `response_fork_not_supported`; a still-running parent returns
409 `response_in_progress`. Neither error creates a session, charges a model call
or appends input. Do not let response routing-index lag turn an accepted operation
into a second run. D1 index repair must not replay provider/tool work.

### Inputs and parameters

Support `input` as text or a typed item array. Text message roles include `user`,
`assistant`, `system` and `developer`; message content accepts strings and
appropriate `input_text`/`output_text` parts. Unlike Chat Completions, Responses
must not inherit the old user-text-only parser. Fresh chains can import explicit
text context and declared local function call/output pairs. Existing chains append
only newly supplied items; clients must not resend their entire previous history.
Reject orphan function outputs, ambiguous item references and malformed parts.

Common request controls are `instructions`, `tools`, `tool_choice`,
`parallel_tool_calls`, `max_output_tokens`, `temperature`, `top_p`, `text.format`,
`metadata`, `background`, `store` and `stream`. Their values are persisted with the
response and either implemented or explicitly rejected before execution. Bounds
must respect the pinned model budget and provider capabilities. Structured JSON
output cannot be advertised as schema-enforced if only a prompt hint was added.
OpenAI-protocol routes map named/auto/none/required tool choice, parallel controls,
text/JSON-object/JSON-schema output and supported `reasoning.effort` natively.
Anthropic-protocol routes map native JSON-schema output, but explicitly reject
JSON-object output and request reasoning overrides; temperature is limited to
0..1 rather than OpenAI's 0..2. An arbitrary BYOK endpoint can impose narrower
model capabilities: expose its clear failure, never claim every endpoint supports
every option. `max_output_tokens` bounds generated output across the response's
rounds, not a fresh full allowance after every server tool call. Unsupported
`include`, audio/image generation, hosted tools and other fields produce clear
parameter errors, not silent no-ops.

Return standard Response object types and statuses, stable output item IDs and
ordered `output`, metadata, timestamps, errors/incomplete details and normalized
`usage` with input/output/total and token detail fields. SDK `output_text` must work
from genuine `message` / `output_text` items. Terminal snapshots are committed before
successful final transport delivery; transport closure does not cancel execution.
External provider uncertainty remains explicit, never an automatic paid replay.

### Instructions and cache boundaries

Keep three separate layers:

1. Server-owned behavioral contract and resolved server tools/skills, pinned to the
   session in stable sorted order.
2. The user's agent-configuration instructions, pinned with that configuration.
3. This response's custom `instructions` and supplied system/developer context.

The first two form the stable server-managed prefix. Request customizations are a
separate durable layer behind it, never a mutation of that prefix. They remain
verbatim available after every compaction during the response, including repeated
compactions and isolate replacement. Never ask the summarizer to reconstruct them.
The source journal is append-only and must not be replaced by a summary.

Official Responses does not inherit a previous response's top-level `instructions`.
Keep this behavior: omitted request instructions do not silently inherit the last
request's override; agent-configuration instructions still persist. The client can
resend a stable customization for later responses. Supplied roles are context, not
authorization: server-side tool permissions, ownership and billing are enforced in
code regardless of prompt text. Chat's existing pinned-only contract is unchanged.

### Local function tools

Accept Responses function definitions `{type:"function",name,description,
parameters,strict}`. Validate bounded JSON schema through a mature validator and
preserve canonical ordering. Keep external names disjoint from reserved registered
server names; a local function must never shadow `bash`, time or persistence tools.
Per-response tool definitions are durable and stable for that response. Do not
mutate the session's pinned server registry to install a local function.

Server tools execute in the harness. Local functions are returned to the caller as
standard `function_call` output items; the platform never executes arbitrary local
code or infers an executable from a function name. A response that hands off local
calls completes normally with those output items. It releases its active inference
slot and financial run reservation; it does not consume active-run concurrency for
the entire time a user is away. The session keeps a durable set of expected calls.

The next `responses.create(previous_response_id=..., input=[...])` supplies
`function_call_output` items with matching `call_id`. Accept exactly the outstanding
set once before continuing inference. Missing, duplicate, unknown or already
consumed calls are rejected without appending partial results. Multiple local calls
are supported; mixed server/local batches must not replay completed server tools.
`parallel_tool_calls:false` must be honored. Outputs are user-supplied data, bounded
in size and charged as actual subsequent inference input, not trusted instructions.

Cancellation is an active background-response operation, not a way to reverse an
already completed function handoff. A completed handoff stays completed; future
tool output must still match its pending call set. Input time advances on
accepted user/tool-result submissions, not polling or model tokens. The existing
ten-minute idle/archive policy remains. Responses continuation from the current
head explicitly requests renewed activity. If
its session archived while awaiting results, readmit it under current quotas and
resume the exact pinned prefix, workspace and pending calls automatically. The SDK
caller need not know a separate session ID. Never forget tool state or replay the
previous model call. Chat Completions keeps its existing explicit-resume contract.

### Background, storage, cancellation and idempotency

`background:true` returns the accepted queued/in-progress response promptly and
continues under durable alarms, independently of the HTTP observer. Polling the
response never creates model work. Cancelling sets a durable request checked at
safe provider/tool boundaries and uses daemon cancellation where possible. Do not
claim `cancelled` while work is still knowingly running; settle observed usage and
leave ambiguous financial outcomes for existing reconciliation.

`store:false` governs the retrievable Response representation, not a fictional
promise that durable execution never writes recovery state. Either implement this
distinction with explicit documented retention and unavailability of later
retrieve/chaining, or reject unsupported combinations before execution. Never echo
`store:false` while silently exposing a stored response indefinitely. Deletion and
storage policy do not erase legally retained financial provenance.

Use an owner-scoped `Idempotency-Key` (1..256 visible ASCII bytes) with a canonical request
hash, including any explicit session binding, and durable
receipt for accepted create operations. Same key + same request returns the same
response; same key + different request returns conflict. Expiry is explicit and
longer than accepted execution; never expire a still-active operation into a new
identity. Reserve the response/session identity before new-session admission so
concurrent retries cannot allocate multiple quota slots. An index reservation is
not proof of accepted inference: retrieval checks the DO. An initialization-only
failure may recover the same empty session identity under normal admission, but a
known/uncertain accepted run must be inspected rather than replayed. Do not introduce a transparent HTTP retry that
replays an uncertain upstream inference or local function side effect.

### Streaming

Use typed Responses SSE events, not Chat chunks with different field names.
Maintain monotonically increasing `sequence_number`, stable response/item IDs and
output/content indices. Emit created/in-progress, output-item/content-part added,
text or function-argument deltas, matching done events, then the correct response
terminal event (`completed`, `failed` or `incomplete`). A complete JSON function
argument must not be inferred from a truncated delta stream.

Heartbeat comments contain no payload and do not extend user inactivity. A closed
stream is not proof of success; clients recover via the same response ID. Persisted
event replay, when offered, must use the same event identities and sequence numbers
and a bounded cursor, never a new model execution. Backpressure must not permit an
unbounded observer queue to exhaust the Worker when a client is slow or gone.

## Space and file HTTP surface

| Method | Path | Contract |
| --- | --- | --- |
| GET / PATCH | `/v1/space` | Get or rename/update the account singleton; no tenant ID supplied by client |
| POST / GET | `/v1/files` | SDK multipart upload and paginated file listing |
| GET / DELETE | `/v1/files/{id}` | SDK file metadata and file deletion |
| GET | `/v1/files/{id}/content` | Owned raw file stream with length, digest/ETag and safe filename |
| Same operations | `/v1/space/files[...]` | Space management aliases; PATCH can rename metadata without rewriting bytes |
| GET | `/v1/space/workspace-versions` | Paginated immutable versions across all the owner's sessions |
| GET | `/v1/space/workspace-versions/{id}` | Version metadata; no mutable storage pointer exposed |
| GET | `/v1/space/workspace-versions/{id}/content` | Download immutable workspace archive |
| POST | `/v1/sessions/{id}/workspace/commit` | Publish a named immutable version of that session's workspace |
| POST | `/v1/sessions/{id}/workspace/restore` | Restore `{version_id}` into this session after preserving current work |

Files use familiar SDK `file` metadata (`id`, `object`, `bytes`, `created_at`,
`filename`, `purpose`) plus additive Space metadata. Content is private, never
automatically public. A filename is display metadata, not an R2 key or filesystem
path. Download disposition must prevent header injection. Restrict list operations
with indexed owner-scoped keyset pagination; no bucket scan or unbounded select.

Uploads stream through a bounded multipart parser into R2; reuse a mature parser
rather than implementing MIME boundary parsing. Bound total bytes, field count,
filename and framing overhead. Publish metadata only after complete upload and
integrity/size verification. A durable upload/deletion intent handles the lack of
cross-system D1/R2 transactions; failed publication cannot leave untracked permanent
storage. Quotas and existing billing apply to files, versions, operations and
retained bytes. Unknown length is not an excuse to buffer an arbitrary upload in a
128 MiB Worker or bypass admission.

### Using files in responses and workspaces

`input_file.file_id` resolves only an owned committed file. File storage accepts
arbitrary bytes; model consumption is capability-specific and must be honest:

* Bounded UTF-8 textual input can become an explicit attachment content item. Never
  silently truncate it or interpolate filename/content into privileged instructions.
* Supported image parts use native provider multimodal conversion, with explicit
  capability checks and bounded decoding. An unsupported provider returns a clear
  modality error before charging inference.
* Binary documents can be safely imported into a workspace and named in the input
  so Bash-based tooling can inspect them. The daemon writes to a capability-scoped
  attachment path atomically, never a client-controlled absolute/parent path. This
  is distinct from claiming a text-only model natively consumed a PDF or image.
* Inline `file_data` and image data URLs need bounded base64 decoding and explicit
  format validation. Remote `file_url` must not become arbitrary credential-bearing
  server fetch; reject unless a separate SSRF-safe, size-bounded fetch path exists.

Persist the admitted file content identity and import receipt. Deleting a Space
entry cannot mutate an already accepted response or cause a later model round to
read different bytes. Space file changes do not automatically overwrite a running
workspace. Import is an explicit operation or an attachment admission step, not a
per-token/per-tool synchronization loop.

## Workspace tools and retention

Expose functional names `workspace_commit`, `workspace_versions` and
`workspace_restore`, not R2 keys, tar internals or container bookkeeping. Workspace
agents can opt into them through the normal registry; disabled workspaces fail
clearly. Version listing is owner-wide, not restricted to the current agent or
session. Version objects cannot be overwritten, renamed or deleted by the user.

Commit serializes with execution, checkpoint, archival and restoration. It creates
an immutable archive in an isolated `space-versions` key namespace with verified
metadata first, then publishes its Space version receipt. Do not retain the mutable
checkpoint's archive by reference: a still-running old DO can legitimately remove
that archive when its manifest advances. A version publication/recovery intent
precedes the copy, and owner+digest reuse can avoid duplicate immutable bytes.
A successful version must reference durable verified bytes and participate in
storage metering. Aggregate Space admission includes uploaded files and distinct
version archive bytes, plus a bounded metadata count. Current workspace pointers
and immutable version references are independent. Existing session archives remain
readable.

Restore is owner-scoped and serialized with all workspace writes. Commit current
dirty work as a safety version before replacing it; if preservation fails, do not
restore. Validate the source archive using the existing bounded atomic daemon
restore. Publish the new current pointer and state only after successful generation
installation. Restore adds a new linear workspace state, not a fork or mutation of
the source version. Persist an operation receipt across crashes so recovery neither
reruns arbitrary Bash nor discards a safety version. New R2 manifests and checkpoint
GC must preserve immutable versions while retaining existing failure safety.

## Bandwidth and data movement, after baseline

Record a baseline before optimizing. Use byte counts and operation counts, not
unsubstantiated latency claims. Representative workloads are long chat history with
short new turns, repeated background status polls, many short local tool results,
large uploads/downloads and repeated unchanged workspace commits.

Prefer these changes in order:

1. Keep response retrieval run-local instead of returning entire session history.
   Add bounded input/output/event pages and management incremental reads; preserve
   existing full-history endpoints and their source-journal meaning.
2. Stream file/archive bytes without materializing them in Worker memory. Honor
   conditional/range reads where compatible with integrity and truthful metering;
   return correct 206/304/416 semantics rather than downloading whole objects first.
3. Reuse immutable archive bytes for identical committed content where safe, with
   explicit version references and no cross-owner existence oracle. Avoid one remote
   request per workspace file or premature global content-addressed deduplication.
4. Bound/coalesce streaming frames without hiding lifecycle/tool boundaries; measure
   frame overhead and first-token latency. No repeated entire accumulated text per
   delta, and no periodic transfer of a growing conversation just to show status.

Keep raw response/session/file payloads out of telemetry. Trace opaque IDs, phases,
sizes, queue depth, retries and timing with explicit consent for diagnostic payloads.
Source journals, rebuildable prompt caches, retained archives, financial provenance
and sampled telemetry remain separate data classes with separate cleanup authority.

## Implementation order and acceptance

Use additive migration `0011` for account-level resource metering, `0012` for
Space/files/version metadata and `0013` for response routing/idempotency. Existing
v0.2 JSON, DO storage and archive formats
remain readable; new fields have backward-compatible defaults. No migration resets
old account keys, journals, workspace pointers or usage. Protect immutable versions
from the old collector before publishing any version, including mixed-version
deployment windows. Dedicated version keys protect them from legacy session
collectors. Account-level resource records have nullable session IDs; the v0.2
global retained-storage sweeper cannot deserialize them. After account-level
resources are admitted, a whole-service rollback requires a build with the nullable
account-resource reader (or a forward patch), not an unmodified v0.2 scheduled
Worker. This is a service rollback floor, not a change to existing public clients,
account keys, configuration or legacy session reads. Never downgrade schema or
manufacture fake sessions to make an old sweeper parse new account resources.

Integrate typed core/request conversion, DO lifecycle and provider capability
plumbing first; connect HTTP routes/index, Space storage and workspace tools; then
CLI/dashboard and measured transport improvements. Keep independent test ownership.
Heavy Rust/Wasm/container verification belongs in selective cached Actions jobs.
Real SDK and browser staging acceptance proves user journeys; mocks prove only
their bounded contracts.

Acceptance must include ordinary SDK text and streaming, latest-head continuation,
stale-parent and concurrent-child rejection, repeated compact preserving both
instruction layers, local multi-function handoff/result continuation, cancellation
before/during tools, background disconnect/retrieve, store/delete semantics,
cross-owner isolation, SDK file upload/input/download/delete, safe binary import,
workspace commit/list/restore/download across sessions, immutable-version mutation
rejection, failed-upload and failed-restore recovery, legacy Chat compatibility and
old stored data. Add state-machine/property tests for lineage, pending call sets,
pagination, accounting identities and path traversal. Stage actual DeepSeek Flash
journeys without retrying accepted inference. Complete acceptance also records
baseline/optimized wire bytes and unchanged user-visible semantics.

## Evidence informing the design

* [OpenAI Responses migration](https://developers.openai.com/api/docs/guides/migrate-to-responses)
  distinguishes output items, response chains, request instructions and typed
  streaming; a field-renaming proxy is not adequate.
* [OpenAI function calling](https://developers.openai.com/api/docs/guides/function-calling)
  defines client-executed calls and correlated `function_call_output` continuation.
* [OpenAI background mode](https://developers.openai.com/api/docs/guides/background)
  motivates durable status/retrieval/cancellation independent of a live observer.
* [Cloudflare Streams](https://developers.cloudflare.com/workers/runtime-apis/streams/)
  and [R2 Workers API](https://developers.cloudflare.com/r2/api/workers/workers-api-reference/)
  support bounded streaming, conditional operations and range reads instead of
  whole-body buffering.
* [MemGPT](https://arxiv.org/abs/2310.08560) treats working context and external
  memory as different tiers. The useful transfer here is separation of the
  compactable inference view from durable source data and explicit state movement;
  it does not justify reconstructing pinned instructions from a lossy summary.
