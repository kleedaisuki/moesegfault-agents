# Responses wire and control boundary

This increment expands the public request vocabulary without making the wire
parser an execution engine. `ResponseRequest` now declares the current 32 create
fields and accepts documented nullable lifecycle/collection defaults. It validates
history-authority exclusivity, control enums/ranges and structured options before
admission. Unknown fields remain errors.

## Consumers and invariants

- `conversation_id()` resolves either accepted conversation form and rejects a
  simultaneous `previous_response_id`.
- `native_input_items()` returns the original, validated item values, **not** the
  attachment placeholders used to build the legacy preflight projection.
- `NormalizedInput.native_items` / `native_only` distinguish preserved rich input
  from the legacy text/function projection. A legacy codec must reject genuinely
  native-only context before mutating a session, not silently discard it.
- `native_tool_items()` returns declared public tool values. Hosted service
  resolution must replace server-executed declarations with its private bindings
  before building a provider payload; it must not grant native provider execution
  under the wrong service credential.
- `native_controls()` carries only provider generation/projection controls.
  Conversation/parent ownership, lifecycle/storage, protected instructions,
  prompt resolution and context management remain harness responsibilities.
- `requires_native_provider()` identifies controls without a legacy mapping;
  declared tool ownership is resolved separately, since hosted tools can be
  implemented through harness-owned function bridges.

## Tools

The existing function accessors remain source-compatible for the first integration
checkpoint. A non-function `ResponseTool` retains its variant options losslessly;
serialization does not invent `name`, `parameters` or `strict` fields for hosted
tools. Function/custom/namespace and all currently documented built-in type names
are distinguished and checked for allowed top-level keys. Per-service nested
option validation and capability authorization belong to the corresponding tool
adapter and must happen before paid work.

`function_definitions()` is a plain-function subset for admission/run builders
which separately bind every typed tool. It does not flatten namespaces or imply
that skipped hosted declarations are executable. The legacy `tool_definitions()`
entry point still rejects mixed typed declarations instead of silently dropping
them.

Namespaces share the existing bounded schema validator budget across their nested
functions and structured output. Native declarations retain caller, async,
deferred-loading and output-schema fields. Provider aliases and public tool names
are distinct; the reserved `__agents_` prefix cannot be supplied as a client tool.

MCP request credentials are never rendered by `Debug` or echoed in public response
tools: `public_value()` removes authorization and headers. They remain present in
the private request value for the authorized adapter. Admission must move such
secrets into the appropriate protected execution state rather than adding them to
ordinary conversation output or telemetry.

Code-interpreter auto-container `network_policy` is parsed as a typed disabled or
allowlist policy. Domain-secret declarations have redacted `Debug` output; raw
serialization is deliberately private execution input. Public response tool
descriptors omit the entire optional `domain_secrets` member, not merely its
values. Validation errors never interpolate a supplied secret or serde error.
Memory sizes and network policies accepted by this syntax layer are **not**
runtime guarantees: hosted binding admission must reject unsupported memory
budgets, network isolation and secret injection before starting execution or
financial work. In particular, accepting `disabled` syntax cannot silently mean
an unrestricted container. A future network proxy must receive protected secret
references rather than copying raw credentials into ordinary binding state.

## Native content and unresolved integration

Rich items preserve phase, reasoning/compaction state, custom calls/results,
tool/search/MCP/computer/shell items and item references. Public HTTPS image/file
URLs receive a syntax/host-policy check but **are not fetched by this module**.
The pure wire validator rejects IP literals and local/reserved host suffixes,
including trailing-dot spellings, without depending on Worker credential storage.
Fetching still requires resolved-address and redirect checks at the admission
boundary; a public-looking DNS name is not network authorization.
Owned file resolution, bounded remote freezing, item-reference ownership,
provider-bound opaque state and pending-result relationships are separate
admission responsibilities. A preserved object is not a claim that its tool has
executed or a remote attachment was downloaded.

The first checkpoint includes pure validation/round-trip/default/secret-redaction
tests and retains the original function/media/schema tests. Full completion still
requires the native context, hosted tool, output projection, current-SDK and
runtime-policy integration recorded in [full-responses.md](full-responses.md).

## Public include projection

`project_output_item` mutates only a caller-owned public copy. It never changes
the canonical persisted native item or item identity. `response_value` applies
this boundary when rendering output, and `project_output_event` provides the
equivalent item/content/logprob boundary for canonical-ID SSE fanout. Retrieval
with a different include selection must render from canonical storage and apply
the requested selection, not expand a previously redacted public response.

Without an explicit include, optional search results/sources, image URLs and
reasoning ciphertext are omitted; code outputs are null and output-text logprobs
are empty arrays to retain their required wire shapes. An include preserves data
already available at the authorized canonical boundary; it does not synthesize
provider capabilities or authorize foreign opaque continuation tokens. Required
compaction ciphertext is not the optional reasoning ciphertext projection and is
left intact. Projection is idempotent and preserves unrelated fields.

HTTP retrieval accepts SDK-style repeated `include[]` (and equivalent `include`)
and a strict boolean `include_obfuscation`, with stream/cursor validation shared
between the public edge and private DO. Each observer owns its projection; a
retrieval never edits the saved generation request, instructions or cache prefix.
An omitted retrieval include selects the ordinary minimal projection. Creation
observers continue to use their create-request include/stream policy.

Durable replay events retain authorized canonical semantic data before include
filtering, so a later observer can request an expanded view without rerunning the
model. Delivery projects a copy for live events, replay, terminal frames and
steering successors. Obfuscation is generated per delivery rather than stored as
canonical state; sequence numbers, item IDs and native private logs are untouched.
Legacy replay entries already redacted by an older release cannot be expanded
retroactively. Owner-sealed continuation resolution remains a separate privacy
boundary: include does not expose raw foreign-provider reasoning ciphertext.

Tool-choice syntax validation is deliberately independent of availability.
Admission validates availability against the combined pinned server registry,
caller declarations and resolved hosted bindings before generation. This keeps
automatically injected `time`/`bash` selectable without accepting unknown tools.

Contract source: [current create reference](https://developers.openai.com/api/reference/resources/responses/methods/create),
cached for inspection in `.cache/full-responses-spec/create.md`.
