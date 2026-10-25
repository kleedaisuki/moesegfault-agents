# Full Responses contract inventory

This is a bounded contract inventory for the expanded user objective, inspected
on 2026-10-25 against source `6552a93`. It is not an implementation or release
acceptance claim. The former linear-only/"relatively complete" scope in
`responses-spaces-v0.3.md` is historical, not an exclusion for the new objective.

## Contract authority and version boundary

Use the current public OpenAI Responses reference for full OpenAI SDK behavior.
Use OpenResponses as an interoperable core specification, not an interchangeable
snapshot of every current OpenAI extension. In particular:

- [OpenResponses 2026-04-24](https://www.openresponses.org/specification) specifies
  one in-flight response per WebSocket and no multiplexing.
- The current [OpenAI WebSocket guide](https://developers.openai.com/api/docs/guides/websocket-mode)
  supports named `stream_id` lanes, same-lane FIFO and cross-lane concurrency and
  forks. Its limits are 16 active responses, 32 distinct named lanes and a
  60-minute connection. These are materially different contracts.
- The guide calls for Python `openai[realtime]>=3.8.0` and Node `openai>=7.10.0`.
  Keep the existing Python 2.6.1 compatibility tests, but add a current-SDK gate;
  the old package cannot be the completeness oracle for newer types and methods.

Stable Responses/Conversations are distinct from the separately documented beta
multi-agent Responses API, Realtime, Live, Agents, administration, fine-tuning,
standalone audio/image endpoints, and account-specific access programs. A tool
variant in Responses still requires a real capability adapter; acknowledging its
schema without executing/preserving its semantics is not implementation.

The REST create reference's HTML exceeds the web reader's size bound. Its official
Markdown was fetched directly (509,830 bytes) into
`.cache/full-responses-spec/create.md`. Other scoped official pages are cached in
that directory. These caches are source material, not production inputs.

## Resource and transport surface

| Public surface | Baseline | Full contract / acceptance needed |
| --- | --- | --- |
| `POST /v1/responses` | Implemented bounded text/media/functions and generation options | Complete typed item/control coverage; historical parent branches; conversation integration; preserve strict rejection before admission for genuinely invalid requests |
| `GET /v1/responses/{id}` | Immutable stored snapshots and GET SSE cursor replay | Current `include` projections and current response fields; stored/no-store/retention semantics; authentic replay without inference |
| `DELETE /v1/responses/{id}` | Tombstone, no session destruction | Keep deletion receipt/SDK void semantics and independent conversation/file lifetime |
| `POST /v1/responses/{id}/cancel` | Safe cancellation and canonical result | Retain background restrictions/current status/error contract and known/unknown usage boundaries |
| `GET /v1/responses/{id}/input_items` | Ordered bounded pagination | Complete typed items and include projections, not only message/function variants |
| `POST /v1/responses/input_tokens` | Missing | Render the same resolved previous history/conversation, current instructions, tools and media; return `object: response.input_tokens` and authoritative count without generation/admission |
| `POST /v1/responses/compact` | Missing standalone endpoint | Return a compacted input window, **not** a normal response ID/continuation; usage and opaque compaction item must be genuine |
| WebSocket `/v1/responses` | Missing | Native response events, lanes, state/cache/retention, errors, steering, forks, disconnect and connection limits; no second harness |
| Conversations CRUD and items CRUD/list | Missing | Owner-scoped durable conversation identity, immutable response boundaries and defined item mutation concurrency |

Sources: [Responses reference](https://developers.openai.com/api/reference/python/resources/responses),
[input count](https://developers.openai.com/api/reference/python/resources/responses/subresources/input_tokens/methods/count),
[compact](https://developers.openai.com/api/reference/resources/responses/methods/compact),
[Conversations](https://developers.openai.com/api/reference/python/resources/conversations).

### Conversation details that affect state design

`conversation` accepts an ID or `{id}`; its items are prepended to the response
input and both input and output items are added after response completion.
It is not another spelling of `previous_response_id`; do not combine the two
history authorities ambiguously. Creation can seed at most 20 items at once.
Conversation update is `POST /conversations/{id}` with metadata. Items support
create/list and per-ID retrieve/delete under their conversation.
**Deleting a conversation does not delete its items.** Do not cascade away user
history or previously stored responses merely to simplify the schema.

Sources: [create](https://developers.openai.com/api/reference/resources/conversations/methods/create),
[update](https://developers.openai.com/api/reference/resources/conversations/methods/update),
[delete](https://developers.openai.com/api/reference/resources/conversations/methods/delete),
[items](https://developers.openai.com/api/reference/python/resources/conversations/subresources/items).

## Create controls: current public REST inventory

The current reference has 32 top-level fields. This table groups fields rather
than replacing their official schemas with another hand-maintained schema.

| Group | Public fields | Baseline / required delta |
| --- | --- | --- |
| Identity/history | `model`, `input`, `previous_response_id`, `conversation` | Agent-ID model domain is established. Conversation missing; previous history only current head; item references and richer items rejected |
| Instructions/lifecycle | `instructions`, `background`, `store`, `stream`, `stream_options`, `metadata` | Implemented baseline. Audit explicit `null` handling: current bool/map/vector Rust fields reject some documented nullable values. `store` defaults true; streaming/background defaults false |
| Generation | `max_output_tokens`, `temperature`, `top_p`, `text`, `reasoning`, `truncation` | Basic controls exist. `text.verbosity` rejected; reasoning limited to effort; output logprobs/refusals/reasoning/annotations not represented fully. Truncation defaults disabled |
| Tools | `tools`, `tool_choice`, `parallel_tool_calls`, `max_tool_calls` | Function-only tools and simple choice exist; max calls absent. Add custom/namespaces/allowed tool subsets and corresponding item lifecycles. Parallel default is true |
| Projection | `include`, `top_logprobs` | Every nonempty include currently rejected; top logprobs absent. Implement actual data projections, not placeholder arrays |
| Context/cache | `context_management`, `prompt_cache_key`, `prompt_cache_options`, `prompt_cache_retention` | All absent. Public context-management uses token threshold; retain the platform's user-facing percentage configuration separately and resolve it for the pinned model |
| Reusable prompts | `prompt` | Missing prompt ID/version/variable resolution. Reuse a managed versioned registry rather than accepting an unverified ID and ignoring it |
| Routing/safety | `service_tier`, `safety_identifier`, `user`, `moderation`, `access_programs` | Absent. Some are model/provider/account-specific; preserve and enforce supported semantics, report effective values, and fail explicitly for unavailable capabilities |

[Create reference](https://developers.openai.com/api/reference/resources/responses/methods/create).
The Python input-count reference additionally lists `personality`; this is a
source-surface discrepancy worth checking against the current official SDK when
implementing that endpoint, not a reason to invent a create field.

Specific changes hidden by the former allowlists:

- Function names are currently capped at 64. Reconcile the limits of plain,
  namespace and custom tool variants separately rather than assuming one shared
  cap. Latest nested function schemas also describe caller context, asynchronous
  output, deferred loading and output schemas. Their omitted-strictness behavior
  must be checked per variant: some current nested schemas describe attempting
  compatible strict validation with a non-strict fallback, not `None == false`.
- Tool-choice allowed subsets preserve a stable `tools` schema while narrowing
  actual invocation. Enforce the subset in the harness even if a provider ignores
  its hint. Keep client/server names disambiguated without silently changing a
  client's public name or granting permission to shadow a built-in.
- Reasoning includes context, effort (`none` through `max`, model-dependent),
  mode and summary (plus deprecated generate-summary compatibility), not merely
  one effort string. Encrypted reasoning and compaction are opaque state items,
  not text to fabricate or decrypt across providers.
- Cache options include implicit/explicit mode, comparison response, prewarm and
  TTL. Deprecated retention is separate. Explicit content breakpoints and cache
  diagnostics must not alter pinned instructions or leak another owner's prefix.
- Current usage details include `cache_write_tokens` as well as cached tokens.
  Do not invent a zero when a provider does not report the quantity.

## Item and tool unions

Baseline normalization accepts messages, function calls and function results;
text, owned/inline images/files are resolved within those shapes. Public output
is principally text message/function call. The complete item inventory additionally
contains reasoning, compaction and compaction trigger, item references, custom
calls/results, tool search/results, additional tools/configuration updates,
computer calls/results, web/file search, image generation, code interpreter,
local shell and shell, apply-patch, MCP tool lists/approval/calls/results.

The declared tool union contains function, custom and namespace tools; tool search;
MCP; local shell/shell/apply-patch; web search/current+preview; file search;
computer/current+preview; code interpreter; image generation; and programmatic
tool calling. Tool ownership and execution mode are distinct from protocol type.
The root's pending hosting-strategy decision determines provider-backed versus
platform-hosted implementations, not permission to silently drop those item types.

The current typed message-content union inspected here contains text, image and
file input; generic prose mentioning audio must not be used to infer a standalone
audio API requirement. Concrete missing content features include assistant
`phase` (`commentary`/`final_answer`), `input_image.detail: original`, file detail,
content cache breakpoints, output refusals, citations/annotations and logprobs.
The reference accepts fully qualified image URLs and `file_url`; the baseline
rejects arbitrary remote attachments. Implement a bounded, SSRF-safe fetch policy
for those supported forms rather than silently pretending the URL was processed.

Cross-provider handling needs a capability boundary. For example, an opaque
upstream encrypted reasoning item cannot be "supported" by interpreting its bytes
as instructions for an unrelated model. Preserve provenance and fail closed on
incompatible routing. Likewise, image/audio generation requires actual model
support; do not return a plausible empty receipt.

## WebSocket state contract

The current OpenAI transport uses `response.create` without HTTP `stream`,
`stream_options` or `background`; events share the HTTP event model. Distinguish
lane ordering from conversation lineage. Omitted `stream_id` is the default lane;
empty or invalid identifiers are errors. Store-false continuation can use the
connection-local prior response; an unavailable ID returns
`previous_response_not_found`. Same-lane errors evict that parent cache, while a
failed cross-lane fork preserves the source lane's shared parent.

The standalone compact output starts a fresh chain (omit/null parent), whereas
normal server-managed compaction continues the existing parent chain. Enforce
connection/lane/active-response budgets and bounded buffering without using a
volatile socket as execution authority.

### Steering is not ordinary append

`response.steer` accepts only its type, parent response ID and new user input.
Acceptance queues input and produces `response.steer.accepted`; the server waits
for safe output/tool boundaries and creates a successor automatically. An
interrupted response can end incomplete with reason `steered`. Client-owned tool
or approval pauses use `response.steer.pending` and required-input stubs, followed
by an explicit client create with the same parent/lane. Do not send a second model
run merely because acceptance was acknowledged. Current OpenAI model support is
GPT-6-family-specific; the platform's existing safe-boundary steering machinery
can supply its own provider capability policy without claiming upstream support.

Sources: [WebSocket mode](https://developers.openai.com/api/docs/guides/websocket-mode),
[steering](https://developers.openai.com/api/docs/guides/steering),
[WebSocket events](https://developers.openai.com/api/reference/resources/responses/websocket-events).

## Recommended cohesive implementation slices

1. **History and item authority:** immutable response parent graph, explicit
   branch materialization/workspace policy, Conversations/items and ownership.
   Settle this before WebSocket lanes diverge from HTTP semantics.
2. **Typed protocol/provider boundary:** current item/tool unions, controls,
   effective capability routing, faithful output/usage/events and null/default
   handling. Reuse maintained schemas and official SDK contracts.
3. **Auxiliary context APIs:** standalone compact round-trip and exact token
   counting over the same resolved context. Counting must not use a completion
   request as a probe or label the current heuristic as authoritative.
4. **WebSocket transport:** lanes, connection-local no-store cache, create/steer,
   errors and reconnection over the same canonical engine and parent graph.
5. **Acceptance:** retain Python 2.6/Node compatibility and add current official
   SDK coverage; execute cross-lane/historical branches, conversation mutation,
   compaction round-trip, token-count consistency, mixed tool lifecycles,
   cold/no-store recovery, cancellation and exact monetary conservation.

This ordering preserves the existing tested base and lets storage/performance
work optimize shared immutable structures rather than another set of linear-only
special cases. None of the missing slices becomes complete merely because its
request object is accepted or a provider silently ignores it.
