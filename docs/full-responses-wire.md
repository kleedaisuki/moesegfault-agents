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

Namespaces share the existing bounded schema validator budget across their nested
functions and structured output. Native declarations retain caller, async,
deferred-loading and output-schema fields. Provider aliases and public tool names
are distinct; the reserved `__agents_` prefix cannot be supplied as a client tool.

MCP request credentials are never rendered by `Debug` or echoed in public response
tools: `public_value()` removes authorization and headers. They remain present in
the private request value for the authorized adapter. Admission must move such
secrets into the appropriate protected execution state rather than adding them to
ordinary conversation output or telemetry.

## Native content and unresolved integration

Rich items preserve phase, reasoning/compaction state, custom calls/results,
tool/search/MCP/computer/shell items and item references. Public HTTPS image/file
URLs receive a syntax/host-policy check but **are not fetched by this module**.
Owned file resolution, bounded remote freezing, item-reference ownership,
provider-bound opaque state and pending-result relationships are separate
admission responsibilities. A preserved object is not a claim that its tool has
executed or a remote attachment was downloaded.

The first checkpoint includes pure validation/round-trip/default/secret-redaction
tests and retains the original function/media/schema tests. Full completion still
requires the native context, hosted tool, output projection, current-SDK and
runtime-policy integration recorded in [full-responses.md](full-responses.md).

Contract source: [current create reference](https://developers.openai.com/api/reference/resources/responses/methods/create),
cached for inspection in `.cache/full-responses-spec/create.md`.
