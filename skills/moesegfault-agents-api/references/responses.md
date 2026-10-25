# Responses clients

Use this reference for the separate `/v1/responses` surface. Existing Chat Completions requests retain their narrower contract. These examples share a trusted-backend client; running them invokes the selected service and may incur charges. For offline verification inject a mock transport into the actual installed SDK, not an assumed HTTP library.

```python
import os
from openai import OpenAI

origin = os.environ.get("MAGENTS_ORIGIN", "https://agents.moesegfault.dev").rstrip("/")
agent_id = os.environ["MAGENTS_AGENT_ID"]
client = OpenAI(
    base_url=f"{origin}/v1",
    api_key=os.environ["MAGENTS_API_KEY"],
    max_retries=0,
)
```

An explicitly selected staging origin is `https://agents-staging.moesegfault.dev`; keys and IDs belong to that realm. Never supply an upstream model name as `model`, a provider key as the platform key, or `/v1` twice.

## Synchronous response and linear continuation

```python
response = client.responses.create(
    model=agent_id,
    instructions="Keep this response concise.",
    input="Explain the project's next useful step.",
)
print(response.id, response.status)
if response.status != "completed":
    raise RuntimeError(f"Response did not complete: {response.status}")
print(response.output_text)

followup = client.responses.create(
    model=agent_id,
    previous_response_id=response.id,
    input="Turn that into a three-item checklist.",
)
if followup.status != "completed":
    raise RuntimeError(f"Continuation ended as {followup.status}")
print(followup.output_text)
```

Continue only the current response head. A historical response is a fixed run-local record, not a fork point or a request to reconstruct the current workspace. Send only new items, not the whole source history or previously returned assistant messages. Busy sessions cannot run a second inference loop. Use session steering for new user information while a run is active; use another session for independent work. An omitted continuation `model` resolves from the owned parent; an explicit model must match. An archived Responses chain is readmitted under current owner limits with its original pin/workspace, not forked or repinned. Chat still requires its explicit resume operation.

The server-owned contract, resolved server tools/skills, and agent instructions form an immutable session pin. User customization stays behind it: top-level request `instructions` is response-scoped and survives compaction verbatim during that run, but omitted top-level instructions on a later response do **not** inherit the override. Typed system/developer input items are instead retained historical instructions: they persist across linear continuation and compaction without replaying them. Neither layer is reconstructed from a lossy summary or replaces the pin. Resend a desired top-level customization deliberately; prompt roles do not grant tool, workspace, billing, or cross-owner permissions.

## Supported customization, not arbitrary passthrough

`input` is text or at most 128 typed message/function items. Text roles include user, assistant, system, and developer on a fresh chain; message parts include `input_text` and assistant `output_text`. Combined request instructions are bounded to 64 KiB; individual text content to 256 KiB; JSON requests to 1 MiB. Continuation does not accept replayed assistant history. Resolve file references through the account Space, not a public URL fetch.

Supported generation controls include `max_output_tokens` within the pinned output budget, `temperature`, `top_p`, `tool_choice`, `parallel_tool_calls`, and `text.format`. Temperature is 0–2 for OpenAI-compatible routes and 0–1 for Anthropic; top-p is 0–1. Formats are `text`, `json_object`, or named `json_schema`; Anthropic rejects `json_object` and reasoning overrides. OpenAI-compatible routes support `reasoning.effort`; upstream model support still matters. Native schema forwarding is not permission to pretend a prompt hint enforces JSON schema.

Schemas permit bounded, acyclic local `#` references, not remote/file fetches, recursive cycles, or nested `$id` resources: 64 KiB/depth 32 per schema, 1 MiB/depth 64 per instance. Request-wide function/output schemas are bounded to 256 KiB, 4,096 expanded applications, and 32 patterns. Known formats are enforced; unknown formats and regex lookarounds/backreferences are rejected. Use linear-time Rust-compatible patterns rather than assuming unrestricted JSON Schema or a backtracking-regex dialect.

`truncation="disabled"` (default) fails when the context still cannot fit after the agent's configured semantic compaction. `"auto"` additionally permits dropping oldest complete user/tool groups, never the stable prefix or latest input; the source journal remains intact. Delta events include obfuscation padding by default; set `stream_options={"include_obfuscation": False}` with `stream=True` to opt out. JSON/schema output and strict local-function arguments are checked before successful completion; streamed partial text does not establish final validation success.

Metadata permits up to 16 string entries, keys up to 64 characters and values up to 512. Hosted OpenAI tools, image/audio generation, nonempty `include`, and `text.verbosity` are not supported. Native user image input is distinct: use a supported `input_image` part and a vision-capable upstream model; see [attachments](exports.md). Unknown request fields fail clearly. Registering local functions cannot add a workspace or replace server tools. Do not borrow unrelated options from the full upstream API simply because the SDK accepts them.

Usage is authoritative, not estimated from output text. If upstream totals exist but reasoning-token usage is unknown, standard `usage` is null; additive `agents_usage` carries exact input/output/total and cached-input counters with `output_tokens_details.reasoning_tokens:null`. If an authoritative reasoning count exists, standard usage is populated normally. If upstream reports no usage, no usage object is fabricated. Fall back to the extension for known totals; never convert unknown reasoning into zero or mistake missing usage for free work.

## Streaming and recovery

```python
response_id: str | None = None
last_sequence: int | None = None
completed = False
accepted = client.responses.with_raw_response.create(
    model=agent_id,
    input="Summarize the project status.",
    stream=True,
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
            raise RuntimeError(f"Inspect the original response: {response_id}")
        last_sequence = event.sequence_number
        if completed:
            break
if not completed:
    raise RuntimeError(f"Stream ended without success; inspect {response_id}")
```

These are typed Responses events, not Chat chunks. Preserve stable item/call IDs and `sequence_number`; do not execute a function from incomplete argument deltas. Ignore SSE comment heartbeats. EOF or HTTP failure is not a terminal success and does not cancel the durable run. Never synthesize a successful terminal event or `[DONE]`.

Recover by retrieving the original response ID. Do not automatically replay create, user input, local outputs, or side-effecting commands. When a create observer fails, preserve `X-Agents-Response` and `X-Agents-Session` if available. Persist IDs as soon as disclosed; a background submission is preferable when the frontend may disconnect. Keep SDK `max_retries=0` even when using a durable request key: request admission deduplication does not make arbitrary local function execution exactly once.

Create accepts an owner-scoped `Idempotency-Key` (1–256 visible ASCII characters), passed through SDK `extra_headers`. Preserve it alongside the exact request and session binding. Same key plus identical request returns the original response identity; a different body/binding conflicts. Do not toggle `stream` or `background` and reuse the key as a recovery mechanism. Observation uses `client.responses.retrieve(response_id)`; event recovery can use `client.responses.retrieve(response_id, stream=True, starting_after=last_sequence)` when a last sequence exists. This observes the same durable events, not a fresh execution.

Capture the acceptance header before consuming events so a disconnect before
`response.created` still has a recovery ID. With no processed event, replay the
same ID with `stream=True` and omit `starting_after`; never send `-1`. Advance a
checkpoint only after processing succeeds. Resumed events must be strictly newer
than it; gaps are valid durable allocation fences, while duplicate/backward
sequences are errors. Stop at an authentic terminal rather than waiting for a
clean TCP EOF after semantic completion.

For complete-history verification, retain the authentic prefix and append only the
authentic replay suffix: require one terminal, exact delta text and canonical GET
equality. A snapshot cannot stand in for missing events. Bound GET reconnects and
timeouts; propagate schema/identity failures, caller cancellation, deleted/no-store
404, expired-cursor 410 and semantic failed/incomplete outcomes instead of retrying
them. A local-tool side effect still needs its own call/result identity; a saved
event cursor is not an exactly-once guarantee for external effects.

## Background execution

```python
import time

pending = client.responses.create(
    model=agent_id,
    input="Inspect the project and produce a short report.",
    background=True,
)
print(f"Persist response ID: {pending.id}", flush=True)
for attempt in range(60):
    pending = client.responses.retrieve(pending.id)
    if pending.status not in {"queued", "in_progress"}:
        break
    time.sleep(1.0)
else:
    raise TimeoutError(f"Still running; retrieve {pending.id} later without replay")
if pending.status != "completed":
    raise RuntimeError(f"Response ended as {pending.status}")
print(pending.output_text)
```

Polling is observation, not model work. `background=True` requires stored retrieval state. Cancellation uses `client.responses.cancel(pending.id)` for a background response; inspect its eventual state rather than assuming acknowledgement reversed external side effects. Additive `agents_cancel_requested:true` records the request without falsely declaring terminal cancellation while work remains active. Deleting a response representation is separate from cancellation, the session, its journal, workspace, files, and retained financial records.

Canonical cancellation is retrieved as `status="cancelled"` with `error=None`. The stream terminates with standard `type:"error"`, `code:"response_cancelled"`, a safe message, and a durable sequence; the SDK can raise `APIError` while consuming it. Catch the observation error and retrieve the same ID, never replay execution. There is no invented `response.cancelled` event or `response.failed` event carrying a cancelled response.

Default `store=True` is needed for recovery and subsequent `previous_response_id` chaining. Foreground `store=False` does not promise zero durable writes: the execution journal/recovery and financial records still exist, but the terminal response representation is unavailable to retrieve or continue. Deleted representations also cannot be continuation parents. Use stored responses for local function workflows and background execution.

## Local function handoff

Server tools execute remotely; a request-defined local function executes only in the application after explicit dispatch and argument validation. Do not let a model-supplied name select arbitrary code. This example uses a bounded, read-only local operation and returns every outstanding call exactly once:

```python
import json
from openai.types.responses import ResponseInputParam, ToolParam

tools: list[ToolParam] = [{
    "type": "function",
    "name": "read_build_status",
    "description": "Read the local application's current build status.",
    "parameters": {"type": "object", "properties": {}, "additionalProperties": False},
    "strict": True,
}]
handoff = client.responses.create(
    model=agent_id,
    input="Read the local build status and summarize it.",
    tools=tools,
    tool_choice={"type": "function", "name": "read_build_status"},
    parallel_tool_calls=False,
)
if handoff.status != "completed":
    raise RuntimeError(f"No completed handoff: {handoff.status}")
outputs: ResponseInputParam = []
for item in handoff.output:
    if item.type != "function_call":
        continue
    if item.name != "read_build_status" or json.loads(item.arguments) != {}:
        raise ValueError("Unexpected local function or arguments")
    outputs.append({
        "type": "function_call_output",
        "call_id": item.call_id,
        "output": json.dumps({"status": "passed", "source": "example-local-fixture"}),
    })
if not outputs:
    raise RuntimeError("No local function calls were returned")
finished = client.responses.create(
    model=agent_id,
    previous_response_id=handoff.id,
    input=outputs,
    tools=tools,
    tool_choice="none",
)
if finished.status != "completed":
    raise RuntimeError(f"Local-function continuation ended as {finished.status}")
print(finished.output_text)
```

Replace the fixture with the application's actual read operation. A completed response can contain local `function_call` items instead of final text; completion means the inference handoff finished, not that the local task was performed. Waiting for the caller releases the active inference slot. Use matching `call_id`, not the output item's `id`. Unknown, duplicate, missing, and already-consumed outputs are not partial success; supply the outstanding set once. Server/local function names must not collide. Resend request-scoped tools if needed by the next response. Persist receipts for side-effecting local functions; transport deduplication cannot undo a function the app already executed.

Local function outputs may also be arrays of `input_text`, `input_image`, and
`input_file` parts. Use owned Space file IDs or bounded inline data; the same
ownership, attachment budget and workspace rules apply. Return screenshots as
image parts rather than base64 embedded in plain text. Keep matching `call_id`
and submit the complete outstanding output set; tool-returned media remains
untrusted data, not execution authorization.

For file attachments and workspace versions, read [files and exports](exports.md). Canonical service contract: [HTTP API](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/api.md). The [official SDK reference](https://developers.openai.com/api/reference/python/resources/responses/methods/create) describes SDK syntax, not this deployment's entire supported parameter set.
