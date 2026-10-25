# HTTP API v1

Base URL: `https://agents.moesegfault.dev`. Management endpoints and the
OpenAI-compatible subset share owner-scoped authentication. JSON request bodies
are limited to 1 MiB. Error responses use
`{"error":{"message":"…","type":"…","code":"…"}}` and meaningful HTTP status
codes. API responses are private and `Cache-Control: no-store`.

This document distinguishes the implemented subset from the complete upstream
OpenAI API: the platform runs its own tools and maintains session history. It is
not a transparent proxy accepting arbitrary OpenAI request fields.

## Authentication

* SDK: `Authorization: Bearer <platform-api-key>`.
* CLI: moeSegFault native-client OIDC authorization code + PKCE, tokens stored in
  the operating-system credential store, not a plaintext config file.
* Browser: BFF-managed secure HttpOnly cookie; mutations require `X-CSRF-Token`.
  Only the configured dashboard origin receives credentialed CORS permission.

The platform key is not an upstream OpenRouter/DeepSeek key. Create/revoke platform
keys through the account API or dashboard. A BYOK credential is stored separately
and never returned as plaintext after creation.

## OpenAI SDK journey

```python
import os
from openai import OpenAI

client = OpenAI(
    base_url="https://agents.moesegfault.dev/v1",
    api_key=os.environ["MAGENTS_API_KEY"],
    max_retries=0,
)
agent_id = "<id from agents create or dashboard>"
raw = client.chat.completions.with_raw_response.create(
    model=agent_id,
    messages=[{"role": "user", "content": "What time is it?"}],
)
session_id = raw.headers["x-agents-session"]
print(raw.parse().choices[0].message.content)

# Append only NEW user input. The service already owns previous history.
reply = client.chat.completions.create(
    model=agent_id,
    messages=[{"role": "user", "content": "Now summarize that in UTC."}],
    extra_headers={"X-Agents-Session": session_id},
)
print(reply.choices[0].message.content)
```

`GET /v1/models` returns the caller's agent IDs as OpenAI model records. Its
`available_models` extension lists server-registered upstream model configurations
for management clients, including models usable through BYOK. The additive
`available_platform_models` array has the same `ModelConfig` shape but includes only
registered non-BYOK routes with a valid configured platform price. Management
clients should use it for platform-funded provider/model choices. BYOK uses a
stored protocol credential and an arbitrary endpoint-supported model ID; it is
not restricted to `available_models` or a vendor-specific platform catalog. Missing or
malformed pricing exposes an empty platform list, not apparently supported models;
invalid individual prices are excluded. Older servers may omit the additive field.
This is configured pricing support, not a promise of upstream health, available
credentials, or funded execution admission. In `POST /v1/chat/completions`, `model` always identifies an
**agent**, not an upstream model.

### Chat Completions

`POST /v1/chat/completions`

```json
{
  "model": "<agent-id>",
  "messages": [{"role": "user", "content": "Inspect the project"}],
  "stream": false
}
```

| Input | Semantics |
|---|---|
| `model` | Existing agent ID owned by caller |
| `messages` | 1–128 new, nonempty, text-only user messages |
| `stream` | Standard Chat Completions chunk stream when true |
| `X-Agents-Session` | Continue this owned session; omitted creates a session |

The response includes `X-Agents-Session` for both JSON and streaming calls. Normal
OpenAI completion/chunk object shapes are used; SSE ends with `data: [DONE]`.
Quiet streaming responses send the [standard SSE comment](https://html.spec.whatwg.org/multipage/server-sent-events.html#authoring-notes) `: keepalive\n\n`
after 15 seconds without a data chunk. SSE parsers, including the OpenAI SDK,
ignore comments; these are not user messages or model tokens. Heartbeats stop
when the event channel closes or `[DONE]` is sent.

For long tasks, create a session with `POST /v1/agents/{id}/sessions` first and
pass its ID in `X-Agents-Session` with `stream: true`. A non-streaming observer can
lose its connection through client or intermediary timeouts even when a durable
run is still executing. Increasing the SDK timeout does not control every proxy.
A disconnected observer does **not** cancel the run: inspect
`GET /v1/sessions/{id}` before deciding what to do next, and do not automatically
replay the completion request or its user messages. Non-streaming responses do
not send whitespace heartbeats, preserving normal JSON and HTTP error statuses.
Harness tool calls and workspace execution are managed on the server. System,
developer, assistant, tool-result, and arbitrary multimodal message injection is
not supported by this subset. Tools, instructions, model budgets, and compaction
policy are pinned from the agent configuration, not overridden per request.
Unsupported generation options are rejected rather than silently changing the
configured behavior. Do not replay the complete conversation on a continuing
session; doing so would append duplicate user input.

A session has one active run. Start a different session for parallel work; use
the steering endpoint to add information during a run. Subscription limits apply
across all of an owner's sessions, including API keys and browser/CLI callers.

## Responses

Responses is an additive API, not a relaxation of the Chat Completions schema
above. `model` still names an owned agent. The public wire protocol is independent
of whether that agent uses an OpenAI-compatible or Anthropic upstream. Use the
same platform bearer authentication and SDK base URL ending in `/v1`.

| Method | Path | Meaning |
|---|---|---|
| POST | `/v1/responses` | Create a synchronous, streaming, or background response |
| GET | `/v1/responses/{id}` | Retrieve that run's response, not the latest session result |
| GET | `/v1/responses/{id}?stream=true&starting_after={sequence}` | Observe/replay typed events after the saved sequence |
| GET | `/v1/responses/{id}/events?after={sequence}` | Equivalent event-observation extension |
| GET | `/v1/responses/{id}/input_items` | Paginated input items for this response |
| POST | `/v1/responses/{id}/cancel` | Request cancellation of a background response |
| DELETE | `/v1/responses/{id}` | Delete the terminal response representation |

The SDK uses ordinary Responses objects: `response.id`, `response.status`, ordered
`response.output`, and `response.output_text`. API extensions expose
`X-Agents-Response` and `X-Agents-Session`. Persist the response ID immediately;
retain these headers when handling an ambiguous observer error.

Usage never fabricates unknown reasoning-token counts. When the provider reports
authoritative reasoning usage, standard `usage` includes input/output/total tokens,
cached input details, and reasoning output details. If it reports totals but no
separate reasoning count, standard `usage` is `null` and additive `agents_usage`
preserves those exact totals with `output_tokens_details:{reasoning_tokens:null}`.
Consumers may read `agents_usage` for totals but must not turn unknown reasoning
into zero. With no reported provider usage, neither a standard usage object nor
an extension is invented.

```python
response = client.responses.create(
    model=agent_id,
    input="Explain the next useful step.",
    instructions="Keep this response concise.",
)
if response.status != "completed":
    raise RuntimeError(f"Response ended as {response.status}")
print(response.output_text)

next_response = client.responses.create(
    model=agent_id,
    previous_response_id=response.id,
    input="Turn that into a three-item checklist.",
)
```

These snippets reuse the `client` and `agent_id` from the SDK setup above, including
`max_retries=0`. They perform model work when run; documentation and offline client
development do not themselves authorize live execution.

### Inputs, customization, and provider options

`input` accepts text or at most 128 typed items. Text messages can use `user`,
`assistant`, `system`, or `developer` roles, with string content or text parts.
Assistant history supports `output_text` parts; user context supports `input_text`.
Fresh chains can supply explicit text history and correlated local function
call/output items. Continuing chains append only new input and the outstanding
function results, never reimport returned assistant history. Unknown fields and
unsupported item types produce parameter errors rather than silent no-ops.

Keep the immutable server behavioral contract, resolved server tools/skills, and
pinned agent instructions separate from user customization. Top-level
`instructions` is response-scoped: it survives repeated compaction verbatim during
that response, but omitting it on the next response does not inherit the override.
Typed `system`/`developer` input items, by contrast, are retained historical
instructions and persist in later linear continuation and compaction. Both user
layers apply behind the stable prefix and cannot grant permissions. The agent pin
still persists; agent updates do not rewrite it. Resend a desired top-level
customization deliberately, without replaying historical input items.

| Parameter | Implemented contract |
|---|---|
| `previous_response_id` | Continue the current head of the same owned agent/session |
| `instructions` | Response-scoped text; combined custom instructions at most 64 KiB |
| `tools` | At most 128 local function definitions, distinct from pinned server tools |
| `tool_choice` | `auto`, `none`, `required`, or `{type:"function",name:"..."}` naming an available function |
| `parallel_tool_calls` | Boolean tool-parallelism policy |
| `max_output_tokens` | Positive, no larger than the pinned agent output budget |
| `temperature` | 0–2 for OpenAI-compatible protocol; 0–1 for Anthropic |
| `top_p` | 0–1 |
| `text.format` | `text`, `json_object`, or named `json_schema`; Anthropic rejects `json_object` |
| `reasoning` | OpenAI-compatible `effort` option; Anthropic rejects reasoning overrides |
| `metadata` | At most 16 string entries; keys at most 64 characters, values at most 512 |
| `stream`, `background`, `store` | Observation, asynchronous execution, and response-representation retention |
| `truncation` | `disabled` (default) fails if configured compaction cannot fit; `auto` may drop oldest complete user/tool groups, never the stable prefix or latest input |
| `stream_options.include_obfuscation` | Delta padding enabled by default; false disables it; requires `stream:true` |

JSON schema controls use the provider's native format configuration, not an
instruction pretending to enforce a schema. Endpoint/model capabilities still
matter; upstream wire compatibility does not guarantee every model supports each
option. Hosted OpenAI tools, image/audio generation, nonempty `include`, and
`text.verbosity` are not supported. Files have the explicit attachment path below;
file storage alone is not native multimodal inference support.
Schema validation is bounded: schemas at most 64 KiB and depth 32, instances at
most 1 MiB and depth 64. Only acyclic local `#` references are supported;
recursive/reference cycles and nested `$id` resources are rejected, and
network/file references are never fetched. The request-wide function/output schema
budget is 256 KiB, 4,096 expanded applications, and 32 patterns; reference depth is
bounded to 64. Known formats are enforced and unknown formats fail admission.
Regex patterns use linear-time Rust syntax with bounded compiled automata;
unsupported lookarounds/backreferences are rejected rather than silently ignored.
Structured JSON/schema outputs and strict function arguments are also checked
before successful completion. Streamed partial text is not proof the eventual
output passed validation; inspect the terminal response state.
For user image input, explicitly use `input_image` with an owned image `file_id`
or bounded PNG/JPEG/GIF/WebP base64 `image_url`, and optional `detail` of `auto`,
`low`, or `high`. At most eight images, 5 MiB decoded per image and 4 MiB aggregate
inline attachment content are accepted; the ordinary JSON body limit still
applies to inline data. Remote image/file URLs are not fetched. Native image
parts require a vision-capable upstream model; choosing `input_file` instead
does not silently request native vision. Audio input is not supported.

An `input_file` part can contain either an owned `file_id`, or `filename` plus
`file_data` as base64 or a `data:<mime>;base64,...` URL, never both. Inline files are
published into Space with a response/item-specific identity before use; repeated
admission of that same operation cannot substitute different bytes or duplicate
its completed upload. This is real storage with quota/resource accounting, not an
unmetered transient field. Inline files share the 4 MiB decoded attachment budget
and the tighter 1 MiB JSON request bound. Prefer the Files upload path for reusable
or larger content. Remote `file_url` inputs are rejected, not fetched.

### Linear continuation and local functions

A response identifies one durable input/output boundary. Retrieval remains tied to
that boundary after later turns, steering, or compaction. Only the current response
head is eligible for continuation. Stale parents return 409
`response_fork_not_supported`; busy-session continuations conflict rather than
launching a second loop. There is no arbitrary historical conversation or workspace
fork. Omit previous history; use another fresh chain for independent work.
For continuation, `model` can be omitted and is resolved from the owned parent;
an explicitly supplied model must match. An optional `X-Agents-Session` binding
must select the same session as the parent. Without either binding, creation
starts a new session. Archived Responses continuations are readmitted under
current owner limits without changing the pin or cloning the workspace; this does
not change Chat's explicit-resume contract.

Local functions are declared in the Responses format, not Chat tool schemas:

```json
{
  "model": "<agent-id>",
  "input": "Read the application's build status.",
  "tools": [{
    "type": "function",
    "name": "read_build_status",
    "description": "Read the local application's build status.",
    "parameters": {"type":"object","properties":{},"additionalProperties":false},
    "strict": true
  }],
  "tool_choice": {"type":"function","name":"read_build_status"},
  "parallel_tool_calls": false
}
```

The platform runs only its registered server tools. A local function appears in
`response.output` as a `function_call` item with `call_id`, `name`, and complete
JSON `arguments`. A `completed` response with such items means inference handed
off to the caller, not that the local function executed. The application validates
the function name and arguments, performs its authorized operation, then submits
the outstanding call set exactly once:

```python
continued = client.responses.create(
    model=agent_id,
    previous_response_id=handoff.id,
    input=[{
        "type": "function_call_output",
        "call_id": call.call_id,
        "output": '{"status":"passed"}',
    }],
)
```

Here `handoff` is the completed handoff response and `call` is its validated
function-call item. For multiple calls, send one output for every outstanding
`call_id`, not the item's `id`. Missing, unknown, duplicate, or already-consumed
results are rejected without accepting a partial set. Request-local tools must be
resupplied when needed by the next response. Local functions cannot shadow server
tools. Waiting for local output releases active inference capacity; the session
retains the outstanding calls durably. Keep application-side receipts for local
side effects: an HTTP retry policy is not exactly-once local execution.

Function outputs can also be arrays of `input_text`, `input_image`, and
`input_file` parts, not only strings. Image/file references use the same owned
Space IDs, inline limits, workspace requirements and immutable attachment
protection as ordinary inputs. The original typed items remain retrievable;
provider-specific image placement does not rewrite the source journal.

```python
from pathlib import Path

# After validating and authorizing a local screenshot function call:
screenshot = client.files.create(file=Path("screenshot.png"), purpose="user_data")
continued = client.responses.create(
    model=agent_id,
    previous_response_id=handoff.id,
    input=[{
        "type": "function_call_output",
        "call_id": call.call_id,
        "output": [{"type": "input_image", "file_id": screenshot.id}],
    }],
)
```

Supply every outstanding call's output together. Images are untrusted tool data,
not new instructions or permission to execute another local operation. Native
image understanding depends on the configured upstream model's capabilities.

### Background, streaming, and ambiguous failure

```python
pending = client.responses.create(
    model=agent_id,
    input="Inspect the project and prepare a short report.",
    background=True,
)
print(pending.id)  # Persist before returning control to the frontend.
current = client.responses.retrieve(pending.id)
```

`background:true` returns accepted queued/in-progress state and keeps running
independently of the observer. Poll the same ID until a terminal status; polling
does not execute another turn or reset user inactivity. Use bounded polling and
retain the ID when the application stops waiting. Background execution requires
`store:true`. `store:false` with foreground execution suppresses later terminal
response retrieval and chaining, not the durable recovery journal or retained
financial records. Deleted responses likewise cannot serve as continuation
parents. Cancellation requests are supported for background responses; their
acknowledgement does not prove an in-flight external command has stopped or its
effects were reversed. The additive `agents_cancel_requested:true` flag records
that request while canonical status may still be `in_progress`. Delete only terminal representations; deletion neither
closes the session nor deletes its source journal, files, workspace, or accounting.

Once cancellation is committed, retrieval reports `status:"cancelled"` and
`error:null`. A streaming observer receives the standard SSE error event with
`type:"error"`, `code:"response_cancelled"`, a safe message, and its durable sequence,
then closes. The SDK may raise `APIError` while consuming it; catch that as an
observation failure and retrieve the same ID. Do not invent a `response.cancelled`
event or mislabel canonical cancelled state as failed.

```python
completed = False
last_sequence: int | None = None
accepted = client.responses.with_raw_response.create(
    model=agent_id, input="Summarize the project.", stream=True,
)
response_id = accepted.headers["x-agents-response"]
with accepted.parse() as events:
    for event in events:
        if event.type == "response.created":
            assert event.response.id == response_id
        elif event.type == "response.output_text.delta":
            print(event.delta, end="", flush=True)
        elif event.type == "response.completed":
            completed = True
        elif event.type in {"response.failed", "response.incomplete", "error"}:
            raise RuntimeError("Retrieve the original response; do not replay input")
        last_sequence = event.sequence_number
        if completed:
            break
if not completed:
    raise RuntimeError("Missing successful terminal event; inspect the original response")
```

Responses SSE uses typed lifecycle, item/content, text, and function-argument
events with monotonic `sequence_number`. Ignore comment heartbeats; they are not
output or success. Retain the last processed sequence when reconnecting to the
same response; replayed events are observation, not a new model execution. Do not
execute functions from partial JSON argument deltas. A closed stream or `[DONE]`
is not a successful terminal event.

Leave SDK automatic retries disabled. A failed create observer, timeout, EOF, or
5xx can follow accepted input and external side effects; retrieve the original
response/session instead of resending prompts. Responses creation supports an
owner-scoped `Idempotency-Key` of 1–256 visible ASCII characters: the same key,
body, and session binding select the same response; a changed request conflicts.
Do not change `stream` or `background` and reuse the key to recover an observer.
This is request admission identity, not permission to
replay uncertain provider work or local effects. The older Chat endpoint's reserved
idempotency header remains unchanged.

### Resume an accepted stream

Keep the accepted ID before reading the first event: a connection can close before
`response.created` arrives. Save the last sequence only after processing that event
succeeds. After transport loss, observe the same response; do not call create again.

| Saved state | Read-only recovery |
| --- | --- |
| ID and processed sequence | Retrieve with `stream=True, starting_after=last_sequence` |
| ID, but no processed event | Retrieve with `stream=True`, omitting the cursor |
| Missing identity, deleted/unstored response, or expired cursor | Stop and reconcile; do not invent events or repeat input |

```python
replay = (
    client.responses.retrieve(response_id, stream=True)
    if last_sequence is None
    else client.responses.retrieve(response_id, stream=True, starting_after=last_sequence)
)
completed = False
with replay as events:
    for event in events:
        if last_sequence is not None and event.sequence_number <= last_sequence:
            raise RuntimeError("Duplicate or backward replay event")
        if event.type == "response.output_text.delta":
            print(event.delta, end="", flush=True)
        elif event.type == "response.completed":
            assert event.response.id == response_id
            completed = True
        elif event.type in {"response.failed", "response.incomplete", "error"}:
            raise RuntimeError("Inspect the original response outcome")
        last_sequence = event.sequence_number
        if completed:
            break
if not completed:
    raise EOFError("Observation remains incomplete; keep the same ID and checkpoint")
```

Sequence gaps are valid durable allocation fences, not missing-event evidence;
duplicate or backward sequences are invalid. An application needing complete-stream
verification retains the authentic prefix and appends the authentic replay suffix,
then checks exactly one terminal, exact delta text, and equality with canonical GET.
An ordinary snapshot can be rendered separately but cannot replace missing events.
Bound reconnect attempts and SDK timeouts; do not retry schema/identity failures,
caller cancellation, or semantic failure/incomplete outcomes. The original
`max_retries=0` setting still applies. Checkpointed observation does not make
arbitrary local tool effects exactly once.

## Files, account Space, and workspace versions

One authenticated account has one lazily created Space. SDK file IDs, response IDs,
and workspace-version IDs are owner-scoped identities, not access capabilities.
These application APIs accept the platform bearer key; cookie callers retain
existing origin/CSRF protection. This does not grant API keys permission to manage
human-only provider credentials, account skills, or platform-key issuance.

| Method | Path | Meaning |
|---|---|---|
| GET / PATCH | `/v1/space` | Read singleton/usage or rename with `{name}` |
| POST / GET | `/v1/files` | SDK multipart upload / paginated file metadata and lifecycle |
| GET / DELETE | `/v1/files/{id}` | File metadata / explicit mutable-entry deletion |
| GET | `/v1/files/{id}/content` | Owned raw bytes |
| Same operations | `/v1/space/files[...]` | Aliases for the same stored files |
| PATCH | `/v1/space/files/{id}` | Rename display metadata with `{filename}` |
| GET | `/v1/space/workspace-versions` | Account-wide immutable-version page |
| GET | `/v1/space/workspace-versions/{id}` | Immutable metadata |
| GET | `/v1/space/workspace-versions/{id}/content` | Immutable tar.zst archive stream |
| POST | `/v1/sessions/{id}/workspace/commit` | Persist the current workspace as a version |
| POST | `/v1/sessions/{id}/workspace/restore` | Preserve current work, then restore an owned version |

```python
uploaded = client.files.create(
    file=("notes.txt", b"Build passed; review the release checklist.\n", "text/plain"),
    purpose="user_data",
)
answer = client.responses.create(
    model=agent_id,
    input=[{
        "role": "user",
        "content": [
            {"type": "input_file", "file_id": uploaded.id},
            {"type": "input_text", "text": "Summarize these notes."},
        ],
    }],
)
if answer.status != "completed":
    raise RuntimeError(f"Attachment response ended as {answer.status}")
print(answer.output_text)

with client.files.with_streaming_response.content(uploaded.id) as download:
    with open("downloaded-notes.txt", "xb") as output:
        for chunk in download.iter_bytes():
            output.write(chunk)
```

`client.files.retrieve(id)` returns familiar file metadata (`id`, `object:"file"`,
`bytes`, `created_at`, `filename`, `purpose`, `status`) plus SHA-256 and update
metadata. `client.files.list(limit=20, order="desc", purpose="user_data")` uses
keyset pagination with `after`, `limit` 1–100, optional purpose, `has_more`,
`first_id`, and `last_id`. Purpose labels are storage metadata, not implementation
of hosted OpenAI products. `client.files.delete(id)` deletes that Space entry;
neither a rename nor deletion rewrites an already admitted attachment receipt.
Pending uploads remain discoverable as SDK status `uploaded`, failed uploads as
`error`, and committed files as `processed`; additive `diagnostics` exposes bounded
upload status and an opaque operation ID. Pending/failed content is not available
for download or model input. A disconnected upload observer is not proof the
transfer failed; inspect the original file/operation rather than blindly creating
another upload.

SDK multipart uploads require known Content-Length and allow one file up to
16 MiB with at most 64 KiB framing overhead. Raw uploads support up to 64 MiB at
`POST /v1/files?filename=<encoded-name>&purpose=user_data` with the file MIME type,
`Content-Length`, and lowercase hex SHA-256 in `X-Content-Sha256`. The singleton's
`usage` exposes actual owner quota/usage; storage and resource admission still
apply. An arbitrary upstream's file limit is not this service's limit.

File content downloads stream bytes with `Content-Length`, `X-Content-Sha256`, a
digest ETag, and safe attachment disposition. Validate integrity before publishing
downloaded data. Choose an explicit local destination and avoid clobbering files;
a display filename is not an R2 key or a trusted filesystem path. Other-owner and
unknown IDs both return 404. No public/presigned storage URLs are returned.

File storage can hold arbitrary bytes; input consumption is capability-specific.
UTF-8, NUL-free attachments up to 256 KiB can become untrusted textual content.
Larger/binary attachments require a workspace and are materialized atomically at
`uploads/{file_id}/content` after funded run admission. The model receives the
receipt/path and can inspect bytes with authorized workspace tools; this is not
native PDF/image understanding by a text-only model. Client filenames cannot
choose the materialization path. File changes do not trigger workspace sync.

Version listing spans all of the owner's sessions, newest first, with `after` and
`limit` 1–100. It returns `object:"list"`, `data`, `has_more`, `first_id`, `last_id`,
and `next_cursor`. A version contains `id`, `object:"workspace.version"`, name,
source session, `created_at_ms`, generation, SHA-256, format version, archive and
logical byte counts, and entry count. Versions cannot be renamed, overwritten, or
deleted by user APIs. The content route returns the immutable archive independently
of the source session's current workspace or closed status, with snapshot integrity
headers. Downloads neither start a workspace nor reset the input clock.

Registered workspace tools are `workspace_commit`, `workspace_versions`, and
`workspace_restore`. Commit records a named immutable version; restore first saves
the current workspace as a safety version, then installs an owned version, possibly
from another session. Failed safety preservation prevents replacement. Restore
changes the linear live workspace, never forks a historical conversation or mutates
the source version. The old closed-session archive endpoint remains unchanged.

Management commit/restore requires an idle, open, workspace-enabled session and a
stable body `operation_id` of 1–128 ASCII letters, digits, underscores or hyphens:

```http
POST /v1/sessions/<session-id>/workspace/commit
Authorization: Bearer <platform-api-key>
Content-Type: application/json

{"operation_id":"release_checkpoint_01","name":"Before migration"}
```

```http
POST /v1/sessions/<session-id>/workspace/restore
Authorization: Bearer <platform-api-key>
Content-Type: application/json

{"operation_id":"restore_release_01","version_id":"<owned-version-id>"}
```

Persist the operation identity before submission and keep it for recovery of that
same action; use a new identity for a new intended action. The Responses
`Idempotency-Key` header does not replace this required body field. Commit returns
the immutable version record; restore returns `{restored:<version>,safety_version_id}`;
both include the management `operation_id`. A closed
session must be explicitly resumed for these management mutations. Neither polling
nor downloading should implicitly run workspace commands to make an artifact.

See [Space](space.md) and [workspace versions](workspace-versions.md) for storage,
publication, recovery, and retention details.

## Agent configurations

| Method | Path | Result |
|---|---|---|
| GET | `/v1/agents` | `{"object":"list","data":[AgentConfig,…]}` |
| POST | `/v1/agents` | Create; 201 + `AgentConfig` |
| GET | `/v1/agents/{id}` | `AgentConfig` or 404 |
| PUT | `/v1/agents/{id}` | Replace editable configuration; increment revision |
| PATCH | `/v1/agents/{id}` | Merge supplied top-level editable fields; increment revision |
| DELETE | `/v1/agents/{id}` | Soft-delete configuration; existing session pins remain independent |

Creation/replacement body:

```json
{
  "name": "Research",
  "model": {
    "provider": "openrouter",
    "model": "deepseek/deepseek-v4.1-flash-1017"
  },
  "instructions": "Investigate carefully and present a concise result.",
  "tools": ["time", "bash"],
  "skills": [],
  "compaction_percent": 80,
  "workspace": true
}
```

`provider` is `openrouter`, `deepseek`, or `byok`. The older Rust-style spellings
`open_router` and `deep_seek` are accepted as input aliases.

Platform context/output limits are validated and replaced with server registry
metadata; sending larger numbers cannot increase the platform allowance. A BYOK
configuration includes a `credential_id` referencing the caller's encrypted stored
credential and accepts an endpoint-supported custom model ID:

```json
{
  "provider": "byok",
  "credential_id": "cred_example",
  "model": "my-model",
  "context_window": 131072,
  "max_output_tokens": 8192
}
```

The BYOK context is a caller-controlled working budget, not a claim that an
arbitrary endpoint actually supports that many tokens. When omitted it defaults
conservatively to 131,072, with 8,192 output tokens; configure a smaller budget
when the selected model requires it. Output must be positive and smaller than
context. Compaction remains a percentage of this pinned budget. Legacy vendor
credentials retain registered limits for previously registered model IDs; old
serialized session pins keep their exact limits. Protocol and endpoint identity
belong to the credential, not the model field.

Omit `model` for the default OpenRouter
`deepseek/deepseek-v4.1-flash-1017` route (1,048,576-token context and 8,192-token
default output budget). Direct DeepSeek and BYOK routes remain explicit choices;
they require the corresponding configured credential. `instructions` defaults to
empty; tools to `["time"]`; skills to `[]`; compaction to 80; workspace to false.
Names contain 1–100 bytes; instructions at most 64 KiB. `bash` requires a workspace.
Unknown tools and skills are rejected, as are unregistered platform-funded models.

Returned `AgentConfig` adds `id`, `owner_id`, `revision`, `created_at_ms`, and
`updated_at_ms`. Agent updates do not mutate existing sessions. `compaction_percent`
is an integer from 1 through 99, relative to the selected model context window.
The service can compact earlier to preserve output headroom. Model registry entries
and skill content are deployment-managed in the initial version.

## Sessions and steering

| Method | Path | Body / meaning |
|---|---|---|
| GET | `/v1/sessions` | List caller's sessions |
| GET | `/v1/agents/{id}/sessions` | List caller's sessions for this agent |
| POST | `/v1/agents/{id}/sessions` | Create an idle session with a pinned configuration |
| GET | `/v1/sessions/{id}` | Current session status |
| GET | `/v1/sessions/{id}/view` | Small conditional status view without journal or full pin |
| GET | `/v1/sessions/{id}/messages` | Bounded source-journal page with opaque cursors |
| POST | `/v1/sessions/{id}/resume` | Explicitly readmit an archived session under current plan limits; preserve pinned config and workspace |
| POST | `/v1/sessions/{id}/messages` | `{"messages":[{"role":"user","content":"…"}],"stream":false}`; append and run |
| POST | `/v1/sessions/{id}/steer` | `{"content":"Prioritize the failing test first"}`; durable input for active run |
| DELETE | `/v1/sessions/{id}` | Request safe close and workspace checkpoint |

Conversation exports and committed workspace archive downloads are documented in
[export contracts](exports.md). They preserve full source records rather than
exporting only the compacted model context.

Steering text is nonempty and at most 64 KiB. It is consumed at a safe model/tool
boundary, not by launching a concurrent loop. An in-flight external command need
not be immediately cancellable. Clients should poll status rather than assuming
that a transport disconnect cancels an accepted operation.

`SessionRecord` contains `id`, `owner_id`, `agent_id`, `status`, `created_at_ms`,
`last_input_at_ms`, `input_revision`, `pinned`, and optional `workspace_snapshot`.
Status values are `idle`, `running`, and `closed`. The pinned record contains the
agent revision, full resolved tool/skill definitions, model configuration,
instructions, compaction percentage, and workspace setting.

Status `messages` is the preserved append-only conversation journal, not the
compacted model prompt. Compaction replaces only the derived active context;
original user messages, assistant messages, tool calls, and tool results remain in
the journal and conversation archive. Pending steering remains separate until a
safe execution boundary consumes it. A summary is not a replacement for the
source conversation or an indication that those messages were deleted.

The additive `/view` endpoint exposes session metadata, `message_count`,
`latest_response_id`, and outstanding `pending_local_calls`, without transferring
the complete journal or pin. Send its ETag in `If-None-Match` for unchanged 304
responses. These remain private, `no-store` observations, not model/user activity.
The existing full-session endpoint retains its original representation.

`/messages` returns the latest 50 source records by default. Accept one opaque,
session-bound `before` or `after` cursor and optional `limit` 1–100. The list has
`data:[{sequence,message}]`, `start` (inclusive), `end` (exclusive), `high_water`,
`before_cursor`, `after_cursor`, `has_older`, and `has_newer`. Source sequences stay
stable across appends and compaction. Pages target 256 KiB but never split a source
message; one oversized record can occupy a page. See
[dataflow contracts](v0.3-dataflow.md) for baseline measurements and details.

The idle deadline is ten minutes after the last accepted user append, including
steering. Agent output and tools do not reset the timer. An active task continues
past the deadline; when it completes, a session already idle for ten minutes is
archived and its container released immediately. A new append during checkpoint
invalidates the pending idle close. Closed sessions cannot silently be resumed by
appending; explicitly resume them or create a new session. An explicit resume is
user activity and starts a new ten-minute idle period without changing the pin.

## Account endpoints

The exact account/key/credential wire shapes live with the authentication module.
Canonical management resources are:

| Method | Resource | Purpose |
|---|---|---|
| GET | `/v1/account` | Owner profile, effective plan/limits, credits, CSRF metadata |
| GET/POST | `/v1/api-keys` | List redacted platform keys / create one-time-displayed secret |
| DELETE | `/v1/api-keys/{id}` | Revoke a key |
| GET | `/v1/provider-keys` | List credential metadata; never returns API keys |
| POST | `/v1/provider-keys` | Create an independently named protocol credential; 201 |
| PUT | `/v1/provider-keys/{id}` | Rotate its key and optionally rename it; route identity is immutable |
| DELETE | `/v1/provider-keys/{id}` | Remove this owned credential |
| POST | `/v1/diagnostics` | Explicitly authorized sanitized diagnostic upload |

Credential creation accepts only:

```json
{
  "name": "My endpoint",
  "protocol": "openai",
  "base_url": "https://api.example.com/v1",
  "api_key": "upstream-secret"
}
```

`protocol` is exactly `openai` (OpenAI-compatible Chat Completions) or `anthropic`
(Anthropic Messages). `name` is optional. A user may store multiple endpoints or
keys for either protocol, up to 100 credentials. Responses contain `id`, `name`,
`protocol`, normalized `base_url`, `created_at`, `updated_at`, and the compatibility
field `provider`. The latter is not a third protocol choice.

An Anthropic SDK-style base URL may omit its final `/v1`: both
`https://api.anthropic.com` and `https://api.anthropic.com/v1` normalize to the same
route; a gateway prefix such as `https://api.deepseek.com/anthropic` becomes
`https://api.deepseek.com/anthropic/v1`. The adapter appends `/messages`.
OpenAI-compatible base URLs retain their explicit path and append
`/chat/completions`; include `/v1` if the endpoint requires it.

Existing credential IDs keep their protocol and normalized base URL permanently.
PUT accepts `{"api_key":"replacement"}` and an optional new name; a full body
with the unchanged route is also accepted. A changed protocol or route returns
409 `credential_route_immutable`: create a new credential and update the agent
instead. This preserves persisted tool/reasoning continuations and cache behavior
in sessions pinned to the old credential. API key rotation does not change its ID.

Legacy `PUT /v1/provider-keys/openrouter` and `/deepseek` with `{"api_key":"…"}`,
and their DELETE aliases, remain compatible with existing clients. Existing rows,
IDs, encryption contexts and pinned references survive the additive migration;
their listing now also includes protocol/base-URL metadata.

All credential management requires a human OIDC account, not a platform API key.
Endpoints must use public HTTPS hostnames without embedded credentials, query or
fragment; literal IPs, local/internal and reserved hostnames are rejected. Redirects
are not followed. Only the selected user's key is sent to that credential's route;
platform credentials are never inherited by a custom endpoint. Resource usage is
still billed while BYOK model charges are zero.

`/auth/config` exposes public native-client OIDC metadata. `/auth/login` and
`/auth/callback` implement the dashboard's browser flow; `POST /auth/logout` revokes
its local BFF session. Subscription checkout is delegated to moeSegFault Billing,
not implemented as a payment form in this API.

Usage is integer micro-USD and measured resources, not binary floating-point
credits. Cached-input/model-output/infrastructure charges are separate. BYOK model
usage is visible without charging the same upstream model request twice; container
and persistence resources remain metered. Account budget, usage visibility, and
subscription status must be wired to actual pricing/entitlement configuration
before enabling paid production traffic.

| Plan | Agent configurations | Concurrent sessions | Concurrent runs |
|---|---:|---:|---:|
| Free | 2 | 2 | 1 |
| Go | 10 | 5 | 2 |
| Plus | 30 | 15 | 5 |
| Pro | 200 | 100 | 20 |

Deployment-wide container capacity is separate from account entitlement limits.
A subscription does not bypass a full deployment's resource admission control.

## Client error handling

| Status | Typical meaning | Action |
|---|---|---|
| 400 | Invalid input, unsupported options, unknown registry item | Correct the request; do not retry unchanged |
| 401/403 | Missing credentials, CSRF failure, disallowed origin | Authenticate / verify caller context |
| 404 | Unknown or other-owner resource | Do not infer whether another owner has this ID |
| 409 | Concurrent config update, busy/closed session, incompatible transition | Refresh status; steer or create a new session |
| 413 | Request exceeds body limit | Reduce payload |
| 429 | Plan capacity or usage admission denied | Wait for capacity or upgrade/check allowance |
| 502/503 | Upstream/container/persistence unavailable | Inspect run status before retrying side effects |

Do not automatically retry ambiguous execution submissions without a supported
idempotency contract. `Idempotency-Key` is a reserved public header, not a blanket
exactly-once guarantee for arbitrary bash side effects. The release journey must
test actual SDK JSON and SSE responses, steering under tool execution, session
closure, cross-owner access, and checkpoint/restore against deployed bindings.
